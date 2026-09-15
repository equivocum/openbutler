// Hold-to-talk — port of backtalk/ptt.py onto Linux evdev.
//
// HOLD the key -> mic opens. RELEASE -> mic closes and the utterance is
// processed. The button IS the voice-activity detector.
//
// THE KEY-REPEAT TRAP: some keyboards send auto-repeat as full DOWN/UP
// pairs rather than repeated DOWNs. A release is therefore never trusted
// on sight — it must stand unchallenged for RELEASE_GRACE (120ms) before
// it counts. A press cancels any pending release. (Field-caught on a
// Logitech MX Mechanical: one 2.6s hold produced 186 events.)
//
// When /dev/input is unreadable (missing `input` group / udev rule),
// construction fails with the udev-rule hint and the caller runs deaf
// (hands-free + typed turns unaffected) — mirroring ptt.py's import
// fallback and the aspike `ptt` exit-2 path.

use evdev::{Device, EventType, Key};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a release must stand unchallenged before it is believed.
pub const RELEASE_GRACE: Duration = Duration::from_millis(120);

/// 'home' / 'f13' / 'right_alt' / any single character -> evdev key.
pub fn resolve_key(name: &str) -> Key {
    let n = name.trim().to_lowercase();
    if n.len() == 1 {
        if let Some(c) = n.chars().next() {
            // evdev letter codes are positional (QWERTY order), not
            // alphabetical — explicit table, no arithmetic.
            let code: Option<u16> = match c {
                'a' => Some(30),
                'b' => Some(48),
                'c' => Some(46),
                'd' => Some(32),
                'e' => Some(18),
                'f' => Some(33),
                'g' => Some(34),
                'h' => Some(35),
                'i' => Some(23),
                'j' => Some(36),
                'k' => Some(37),
                'l' => Some(38),
                'm' => Some(50),
                'n' => Some(49),
                'o' => Some(24),
                'p' => Some(25),
                'q' => Some(16),
                'r' => Some(19),
                's' => Some(31),
                't' => Some(20),
                'u' => Some(22),
                'v' => Some(47),
                'w' => Some(17),
                'x' => Some(45),
                'y' => Some(21),
                'z' => Some(44),
                '1' => Some(2),
                '2' => Some(3),
                '3' => Some(4),
                '4' => Some(5),
                '5' => Some(6),
                '6' => Some(7),
                '7' => Some(8),
                '8' => Some(9),
                '9' => Some(10),
                '0' => Some(11),
                ' ' => Some(57),
                _ => None,
            };
            if let Some(v) = code {
                return Key::new(v);
            }
        }
    }
    // Friendly names -> evdev codes (pynput called right-option alt_r;
    // evdev calls it RIGHTALT; this map speaks human like the docs do).
    let aliases: &[(&str, Key)] = &[
        ("right_alt", Key::KEY_RIGHTALT),
        ("alt_r", Key::KEY_RIGHTALT),
        ("left_alt", Key::KEY_LEFTALT),
        ("alt_l", Key::KEY_LEFTALT),
        ("right_option", Key::KEY_RIGHTALT),
        ("left_option", Key::KEY_LEFTALT),
        ("right_ctrl", Key::KEY_RIGHTCTRL),
        ("ctrl_r", Key::KEY_RIGHTCTRL),
        ("left_ctrl", Key::KEY_LEFTCTRL),
        ("ctrl_l", Key::KEY_LEFTCTRL),
        ("right_cmd", Key::KEY_RIGHTMETA),
        ("cmd_r", Key::KEY_RIGHTMETA),
        ("left_cmd", Key::KEY_LEFTMETA),
        ("cmd_l", Key::KEY_LEFTMETA),
        ("right_shift", Key::KEY_RIGHTSHIFT),
        ("shift_r", Key::KEY_RIGHTSHIFT),
        ("left_shift", Key::KEY_LEFTSHIFT),
        ("shift_l", Key::KEY_LEFTSHIFT),
        ("home", Key::KEY_HOME),
        ("end", Key::KEY_END),
        ("pageup", Key::KEY_PAGEUP),
        ("page_up", Key::KEY_PAGEUP),
        ("pagedown", Key::KEY_PAGEDOWN),
        ("page_down", Key::KEY_PAGEDOWN),
        ("up", Key::KEY_UP),
        ("down", Key::KEY_DOWN),
        ("left", Key::KEY_LEFT),
        ("right", Key::KEY_RIGHT),
        ("esc", Key::KEY_ESC),
        ("escape", Key::KEY_ESC),
        ("tab", Key::KEY_TAB),
        ("enter", Key::KEY_ENTER),
        ("return", Key::KEY_ENTER),
        ("space", Key::KEY_SPACE),
        ("backspace", Key::KEY_BACKSPACE),
        ("delete", Key::KEY_DELETE),
        ("insert", Key::KEY_INSERT),
        ("caps_lock", Key::KEY_CAPSLOCK),
        ("capslock", Key::KEY_CAPSLOCK),
        ("menu", Key::KEY_MENU),
    ];
    for (want, code) in &[
        ("f1", 59),
        ("f2", 60),
        ("f3", 61),
        ("f4", 62),
        ("f5", 63),
        ("f6", 64),
        ("f7", 65),
        ("f8", 66),
        ("f9", 67),
        ("f10", 68),
        ("f11", 87),
        ("f12", 88),
        ("f13", 183),
        ("f14", 184),
        ("f15", 185),
        ("f16", 186),
        ("f17", 187),
        ("f18", 188),
        ("f19", 189),
        ("f20", 190),
        ("f21", 191),
        ("f22", 192),
        ("f23", 193),
        ("f24", 194),
    ] {
        if n == *want {
            return Key::new(*code);
        }
    }
    for (want, code) in aliases {
        if n == *want {
            return *code;
        }
    }
    eprintln!("[ptt] unknown key {name:?} — falling back to 'home'");
    Key::KEY_HOME
}

struct Inner {
    held: bool,
    release_at: Option<Instant>,
    pressed: bool, // wait_press event pending
}

pub struct PTTListener {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    // Device watch threads are detached on spawn (they run for the life
    // of the process); holding their handles would cost Sync for no gain.
}

impl PTTListener {
    /// Open every keyboard-like /dev/input/event* and watch for `key`.
    /// Fails gracefully (with the udev hint) when nothing is readable.
    pub fn open(key_name: &str) -> Result<Self, String> {
        let key = resolve_key(key_name);
        let inner = Arc::new((
            Mutex::new(Inner {
                held: false,
                release_at: None,
                pressed: false,
            }),
            Condvar::new(),
        ));
        let mut opened = 0;
        let mut tried = 0;
        let mut paths: Vec<PathBuf> = std::fs::read_dir("/dev/input")
            .map(|r| {
                r.filter_map(|e| e.ok().map(|x| x.path()))
                    .filter(|p| {
                        p.file_name()
                            .map(|f| f.to_string_lossy().starts_with("event"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        for path in paths {
            tried += 1;
            let dev = match Device::open(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Keyboards have real keys; power-button / lid pseudo-devices
            // expose only KEY_POWER-style singletons. KEY_A is the tell.
            let keys = dev
                .supported_keys()
                .map(|k| k.contains(Key::KEY_A))
                .unwrap_or(false);
            if !keys {
                continue;
            }
            opened += 1;
            let mine = inner.clone();
            let name = dev.name().unwrap_or_default().to_string();
            std::thread::Builder::new()
                .name(format!("ptt-{}", path.display()))
                .spawn(move || {
                    let _ = watch_device(path, name, key, mine);
                })
                .ok();
        }
        if opened == 0 {
            return Err(format!(
                 "PTT unavailable: no readable keyboard under /dev/input (tried {tried}). \
                  hint: evdev needs /dev/input read access — install udev/70-openbutler-input.rules \
                 (TAG+=uaccess), or run hands-free."
            ));
        }
        eprintln!("[ptt] watching {opened} input device(s) for key '{key_name}'");
        Ok(PTTListener { inner })
    }

    /// Block until the key goes DOWN (one event per physical press).
    pub fn wait_press(&self) {
        let (mu, cv) = &*self.inner;
        let mut g = mu.lock().unwrap();
        loop {
            settle_locked(&mut g);
            if g.pressed {
                g.pressed = false;
                return;
            }
            let (ng, _) = cv.wait_timeout(g, RELEASE_GRACE).unwrap();
            g = ng;
        }
    }

    pub fn is_held(&self) -> bool {
        let (mu, _) = &*self.inner;
        let mut g = mu.lock().unwrap();
        settle_locked(&mut g);
        g.held
    }
}

fn settle_locked(g: &mut Inner) {
    if g.held {
        if let Some(t) = g.release_at {
            if Instant::now() >= t {
                g.held = false;
                g.release_at = None;
            }
        }
    }
}

fn watch_device(
    path: PathBuf,
    name: String,
    key: Key,
    inner: Arc<(Mutex<Inner>, Condvar)>,
) -> Result<(), String> {
    let mut dev = Device::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("[ptt] listening on {} ({name})", path.display());
    loop {
        let ev = match dev.fetch_events() {
            Ok(it) => it.collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("[ptt] {} read failed ({e}); device dropped", path.display());
                return Err(e.to_string());
            }
        };
        for e in ev {
            if e.event_type() != EventType::KEY || Key::new(e.code()) != key {
                continue;
            }
            let (mu, cv) = &*inner;
            let mut g = mu.lock().unwrap();
            match e.value() {
                1 => {
                    // A press cancels any pending release: that release
                    // was auto-repeat, not a human letting go.
                    g.release_at = None;
                    if !g.held {
                        g.held = true;
                        g.pressed = true;
                        cv.notify_all();
                    }
                }
                0 => {
                    // PROVISIONAL. Believed only if no press follows.
                    g.release_at = Some(Instant::now() + RELEASE_GRACE);
                    cv.notify_all();
                }
                _ => {} // value 2: auto-repeat echo, ignored
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_resolve() {
        assert_eq!(resolve_key("home"), Key::KEY_HOME);
        assert_eq!(resolve_key("HOME"), Key::KEY_HOME);
        assert_eq!(resolve_key("right_alt"), Key::KEY_RIGHTALT);
        assert_eq!(resolve_key("f13"), Key::new(183));
        assert_eq!(resolve_key("a"), Key::KEY_A);
        assert_eq!(resolve_key("z"), Key::KEY_Z);
        assert_eq!(resolve_key("5"), Key::KEY_5);
        assert_eq!(resolve_key("bogus-key-name"), Key::KEY_HOME);
    }

    #[test]
    fn settle_grace_holds_releases() {
        let mut g = Inner {
            held: true,
            release_at: Some(Instant::now() - Duration::from_millis(1)),
            pressed: false,
        };
        settle_locked(&mut g);
        assert!(!g.held);
        let mut g2 = Inner {
            held: true,
            release_at: Some(Instant::now() + Duration::from_secs(10)),
            pressed: false,
        };
        settle_locked(&mut g2);
        assert!(g2.held);
    }
}
