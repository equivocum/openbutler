// Live voice loop — port of backtalk/main.py amain() + handle() +
// run_console() + speak_reply() for the OpenCode engine.
//
// ONE loop, two mic modes, switchable live. Typing is a first-class turn.
// The talk key is honored in BOTH modes: it interrupts, and holding it
// always gets you heard. Concurrency is threads + channels (std only):
// the turn (brain + speak emit) runs on one worker thread so the key,
// the open mic, and typing stay live mid-reply; a shared kill handle
// lets the loop interrupt a turn without borrowing Brain.

use crate::brain::Brain;
use crate::console;
use crate::ears::Ears;
use crate::mouth::{strip_directions, Mouth, MouthCfg};
use crate::signals::Signals;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn log(line: &str) {
    crate::vlog::log(line);
}

#[derive(Clone)]
pub struct RunOpts {
    pub open_mic: bool,
    pub barge_in: bool,
    pub wake: bool,
    pub model: Option<String>,
    pub voice: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum MicMode {
    Ptt,
    Open,
    Wake,
}

struct MicState {
    mode: MicMode,
    gen: u64,
    btn: bool,
}

enum Event {
    Typed(String),
    Press,
    Mic { gen: u64, text: String },
    MicError(String),
}

/// Loopback port used purely as a mutex. Nothing is ever served on it.
/// A bound socket is the mutex rather than a pid file, because the OS
/// releases it however this process dies; a pid file outlives a crash
/// and then lies about a process long gone.
const INSTANCE_PORT: u16 = 8791;

fn claim_single_instance() -> Option<std::net::TcpListener> {
    std::net::TcpListener::bind(("127.0.0.1", INSTANCE_PORT)).ok()
}

/// Phase 7 wake-word cycle: score 80ms mic ticks until "hey jarvis"
/// fires (patience consecutive frames >= threshold), then capture
/// utterances for the attention window without re-wake. Returns on
/// abort/mode-switch; the caller re-checks mode. Scoring pauses while
/// the mouth speaks (unless barge-in) — own replies must not self-fire.
#[allow(clippy::too_many_arguments)]
fn wake_cycle(
    ears: &Arc<Ears>,
    mouth: &Arc<Mouth>,
    mic_state: &Arc<Mutex<MicState>>,
    tx: &std::sync::mpsc::Sender<Event>,
    ctrlc: &Arc<AtomicBool>,
    barge_in: bool,
    thr: f32,
    patience: u32,
    attn_s: f64,
    gen: u64,
) {
    let aborted = || mic_state.lock().unwrap().gen != gen || ctrlc.load(Ordering::SeqCst);
    let wake_mode = || mic_state.lock().unwrap().mode == MicMode::Wake;
    let mut tap = match ears.wake_tap() {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Event::MicError(e));
            return;
        }
    };
    let mut serve = match crate::wake::WakeServe::new() {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Event::MicError(format!("wake scorer unavailable: {e}")));
            return;
        }
    };
    // Keep scoring + attention looping inside one wake_cycle so the
    // scorer (3 ONNX sessions) isn't reloaded after every answer —
    // that reload was the "takes time to wake after answer" delay.
    loop {
        // ---- scoring loop ----
        let mut consec = 0u32;
        let mut fails = 0u32;
        let detected = loop {
            if aborted() || !wake_mode() {
                serve.shutdown();
                return;
            }
            if !barge_in && mouth.speaking() {
                consec = 0;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let tick = match tap.read_tick() {
                Ok(t) => t,
                Err(e) => {
                    fails += 1;
                    log(&format!("[wake] mic tick failed ({e}) [{fails}]"));
                    if fails >= 5 {
                        log("[wake] mic failing — falling back to open mic for this session (say push to talk mode to change)");
                        mic_state.lock().unwrap().mode = MicMode::Open;
                        mic_state.lock().unwrap().gen += 1;
                        serve.shutdown();
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                    match ears.wake_tap() {
                        Ok(t) => tap = t,
                        Err(e2) => {
                            let _ = tx.send(Event::MicError(e2));
                            serve.shutdown();
                            return;
                        }
                    }
                    continue;
                }
            };
            match serve.tick(&tick) {
                Ok((score, _)) => {
                    fails = 0;
                    if score >= thr {
                        consec += 1;
                    } else {
                        consec = 0;
                    }
                    if consec >= patience.max(1) {
                        break true;
                    }
                }
                Err(e) => {
                    fails += 1;
                    log(&format!("[wake] scorer failed ({e}) [{fails}]"));
                    if fails >= 5 {
                        log("[wake] scorer unavailable — falling back to open mic for this session (say push to talk mode to change)");
                        mic_state.lock().unwrap().mode = MicMode::Open;
                        mic_state.lock().unwrap().gen += 1;
                        serve.shutdown();
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        };
        if !detected {
            serve.shutdown();
            return;
        }
        log("[wake] hey jarvis — listening");
        serve.reset();
        // Keep up to 0.6s of mic audio that arrived during the scoring gap
        // as pre-roll for the next utterance so "hey Jarvis what time"
        // doesn't lose "what". Tap is then closed — listen_once opens its
        // own cpal stream and two concurrent opens on flaky ALSA assert.
        let preroll = tap.take_preroll(9600);
        drop(tap);
        let mut last = Instant::now();
        let mut first_preroll = Some(preroll);
        while wake_mode() && !aborted() && last.elapsed().as_secs_f64() < attn_s {
            let remain = attn_s - last.elapsed().as_secs_f64();
            let gate = || mic_state.lock().unwrap().btn || (!barge_in && mouth.speaking());
            let abort2 = || mic_state.lock().unwrap().gen != gen || ctrlc.load(Ordering::SeqCst);
            let res = if let Some(p) = first_preroll.take() {
                ears.listen_once_with_preroll(p, &gate, &abort2, Some(remain.max(0.5)))
            } else {
                ears.listen_once(&gate, &abort2, Some(remain.max(0.5)))
            };
            match res {
                Ok(Some(text)) => {
                    if !text.trim().is_empty() {
                        last = Instant::now(); // each turn extends attention
                        if tx.send(Event::Mic { gen, text }).is_err() {
                            serve.shutdown();
                            return;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx.send(Event::MicError(e));
                    break;
                }
            }
        }
        log("[wake] attention over — sleeping until hey jarvis");
        // Loop back to scoring without reloading the scorer — this is the
        // fast path that fixes post-answer wake delay. Fresh tap for next
        // scoring round.
        match ears.wake_tap() {
            Ok(t) => tap = t,
            Err(e) => {
                let _ = tx.send(Event::MicError(e));
                serve.shutdown();
                return;
            }
        }
    }
}

pub fn run(
    home: &std::path::Path,
    mut cfg: serde_json::Map<String, serde_json::Value>,
    opts: RunOpts,
) -> i32 {
    crate::vlog::init(home.join("logs").join("voice.log"));

    let _lock = match claim_single_instance() {
        Some(l) => l,
        None => {
            println!("[butler] ANOTHER VOICE LINE IS ALREADY RUNNING on this machine, so this one is stopping.");
            println!("[butler] Two of them fight over the microphone and the talk key, which looks exactly like the talk key being broken. Use the window that is already open, or close it and start again.");
            return 1;
        }
    };

    // Session-only launch overrides (never persisted here).
    if opts.open_mic {
        cfg.insert("mic_mode".into(), serde_json::Value::String("open".into()));
    }
    if opts.wake {
        cfg.insert("mic_mode".into(), serde_json::Value::String("wake".into()));
    }
    if let Some(v) = opts.voice.as_ref().filter(|v| !v.is_empty()) {
        cfg.insert("voice".into(), serde_json::Value::String(v.clone()));
    }

    let get = |c: &serde_json::Map<String, serde_json::Value>, k: &str| {
        c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let quit_phrases: Vec<String> = cfg
        .get("quit_phrases")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let name = get(&cfg, "name");
    let agent_dir = get(&cfg, "agent_dir");
    let signals_dir = get(&cfg, "signals_dir");
    let ptt_key = get(&cfg, "ptt_key");
    let deep_model = get(&cfg, "deep_model");
    let fast_model = get(&cfg, "model");

    let signals = Arc::new(Signals::new(
        &signals_dir,
        &get(&cfg, "board_state_dir"),
        &get(&cfg, "thinking_sound"),
    ));

    // Session-only launch overrides (never persisted here).
    if opts.open_mic {
        cfg.insert("mic_mode".into(), serde_json::Value::String("open".into()));
    }
    if opts.wake {
        cfg.insert("mic_mode".into(), serde_json::Value::String("wake".into()));
    }
    if let Some(v) = opts.voice.as_ref().filter(|v| !v.is_empty()) {
        cfg.insert("voice".into(), serde_json::Value::String(v.clone()));
    }

    let mouth = Arc::new(Mouth::new(MouthCfg::from_map(&cfg), signals.clone()));
    let ears = Arc::new(Ears::new(&cfg));
    let brain = Arc::new(Mutex::new(Brain::new(
        agent_dir.clone(),
        crate::config::discipline(&cfg),
        signals_dir.clone(),
        cfg.get("resume_last_session")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        {
            let p = get(&cfg, "permission_mode");
            if p.is_empty() {
                "ask".into()
            } else {
                p
            }
        },
        opts.model.clone(),
        cfg.get("opencode_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        None,
    )));

    let mic_state = Arc::new(Mutex::new(MicState {
        mode: if opts.wake || get(&cfg, "mic_mode") == "wake" {
            MicMode::Wake
        } else if opts.open_mic || get(&cfg, "mic_mode") == "open" {
            MicMode::Open
        } else {
            MicMode::Ptt
        },
        gen: 0,
        btn: false,
    }));

    // Phase 7 wake-word tuning (Rust-only config; see config.rs).
    let (wake_thr, wake_pat, wake_attn) = {
        let w = cfg.get("wake");
        let f = |k: &str, d: f64| {
            w.and_then(|v| v.get(k))
                .and_then(|v| v.as_f64())
                .unwrap_or(d)
        };
        (
            f("threshold", 0.5) as f32,
            f("patience", 2.0) as u32,
            f("attention_s", 8.0),
        )
    };

    // PTT key: deaf when /dev/input is unreadable (hands-free + typed
    // turns unaffected), mirroring ptt.py's import fallback.
    let ptt_live = match crate::ptt::PTTListener::open(&ptt_key) {
        Ok(p) => Some(Arc::new(p)),
        Err(e) => {
            log(&format!(
                "[ptt] key backend unavailable ({e}); no talk key in this session"
            ));
            None
        }
    };

    let ctrlc = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, ctrlc.clone());
    // SIGTERM gets the same graceful exit (supervisors and `timeout`
    // send TERM, not INT — Python parity would orphan the serve child).
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, ctrlc.clone());

    let mode_str = if mic_state.lock().unwrap().mode == MicMode::Open {
        "hands-free listening (the talk key still works)".to_string()
    } else {
        format!("push-to-talk ({ptt_key})")
    };
    log(&format!("[butler] up — agent={name} dir={agent_dir} model={} mic={mode_str} (say 'goodbye {}' to hang up)",
        brain.lock().unwrap().model_label, name.to_lowercase()));
    mouth.say(&get(&cfg, "greeting"));

    // Warm the ears while the greeting plays; the STT load hides behind
    // the spoken line.
    {
        let ears = ears.clone();
        std::thread::Builder::new()
            .name("warm-ears".into())
            .spawn(move || {
                if let Err(e) = ears.warm() {
                    log(&format!("[ears] warm failed: {e}"));
                }
            })
            .ok();
    }

    // THE BRAIN CONNECT, guarded: needs opencode on PATH + a signed-in
    // session. When it fails the mouth still works, so SAY SO instead of
    // dying silently with the face stuck on idle.
    log("[butler] connecting the brain...");
    if let Err(e) = brain.lock().unwrap().start() {
        return brain_dead(&mouth, &format!("failed: {e}"));
    }
    // Hidden warmup ping (plumbing, not conversation) with a 180s cap.
    {
        let wbrain = brain.clone();
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("warmup".into())
            .spawn(move || {
                let mut sink = |_: &str, _: f64| {};
                let ok = wbrain
                    .lock()
                    .unwrap()
                    .run_turn("Warmup ping - reply with the single word: ready", &mut sink)
                    .is_ok();
                let _ = tx.send(ok);
            })
            .ok();
        match rx.recv_timeout(Duration::from_secs(180)) {
            Ok(true) => {}
            Ok(false) => return brain_dead(&mouth, "failed: warmup turn errored"),
            Err(_) => {
                brain.lock().unwrap().interrupt();
                return brain_dead(&mouth, "timed out");
            }
        }
    }
    log("[butler] brain warm");
    {
        let mut b = brain.lock().unwrap();
        b.turns = 0;
        b.out_tokens = 0;
        b.in_tokens = 0;
        b.cost = 0.0;
    }
    // A configured effort level applies at launch.
    let boot_effort = get(&cfg, "effort").trim().to_lowercase();
    if !boot_effort.is_empty() {
        if console::EFFORTS.contains(&boot_effort.as_str()) {
            let r = brain
                .lock()
                .unwrap()
                .command(&format!("/effort {boot_effort}"));
            log(&format!(
                "[butler] effort set to {boot_effort} (from config): {r}"
            ));
        } else {
            log(&format!(
                "[butler] ignoring unknown effort {boot_effort:?} in config"
            ));
        }
    }

    let (tx, rx) = std::sync::mpsc::channel::<Event>();

    // Typed lines are first-class turns: same pipeline as speech.
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("typed".into())
            .spawn(move || {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    let clean = console::clean_typed(&line);
                    if !clean.is_empty() {
                        if tx.send(Event::Typed(clean)).is_err() {
                            break;
                        }
                    }
                }
            })
            .ok();
    }
    // Talk-key presses.
    if let Some(ptt) = ptt_live.clone() {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("ptt".into())
            .spawn(move || loop {
                ptt.wait_press();
                if tx.send(Event::Press).is_err() {
                    break;
                }
            })
            .ok();
    }
    // Open-mic capture loop: one utterance at a time, recreated after
    // each capture; yields while the BUTTON records and, without
    // barge-in, while the mouth speaks. In Wake mode this thread scores
    // "hey jarvis" instead and only captures utterances inside the
    // attention window after a detection.
    {
        let tx = tx.clone();
        let ears = ears.clone();
        let mouth = mouth.clone();
        let mic_state = mic_state.clone();
        let ctrlc = ctrlc.clone();
        let barge_in = opts.barge_in;
        std::thread::Builder::new()
            .name("openmic".into())
            .spawn(move || {
                loop {
                    if ctrlc.load(Ordering::SeqCst) {
                        break;
                    }
                    let gen = mic_state.lock().unwrap().gen;
                    let mode = mic_state.lock().unwrap().mode;
                    if mode == MicMode::Ptt {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    if mode == MicMode::Wake {
                        wake_cycle(
                            &ears, &mouth, &mic_state, &tx, &ctrlc, barge_in, wake_thr, wake_pat,
                            wake_attn, gen,
                        );
                        continue;
                    }
                    let gate = || mic_state.lock().unwrap().btn || (!barge_in && mouth.speaking());
                    let abort =
                        || mic_state.lock().unwrap().gen != gen || ctrlc.load(Ordering::SeqCst);
                    match ears.listen_once(&gate, &abort, None) {
                        Ok(Some(text)) => {
                            if !text.trim().is_empty() {
                                if tx.send(Event::Mic { gen, text }).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(None) => {} // aborted (mode switch) — re-check mode
                        Err(e) => {
                            if tx.send(Event::MicError(e)).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .ok();
    }

    let mut app = App {
        cfg,
        home: home.to_path_buf(),
        brain,
        mouth,
        ears,
        signals,
        mic_state,
        quit_phrases,
        name,
        deep_model,
        fast_model,
        turn: None,
        proc: None,
        confirm: None,
        mic_fails: 0,
    };

    let code = loop {
        if ctrlc.load(Ordering::SeqCst) {
            break 0;
        }
        let ev = match rx.try_recv() {
            Ok(e) => Some(e),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => break 0,
        };
        let ev = ev.unwrap();
        match ev {
            Event::Typed(text) => {
                if !app.handle(text) {
                    break 0;
                }
            }
            Event::Mic { gen, text } => {
                if gen != app.mic_state.lock().unwrap().gen {
                    continue; // captured before a switch
                }
                if !app.handle(text) {
                    break 0;
                }
            }
            Event::MicError(e) => {
                if !app.on_mic_error(&e) {
                    break 0;
                }
            }
            Event::Press => {
                // The button = interrupt. During a live turn the turn
                // stays alive only as a corpse: kill + silence now, join
                // in handle() before anything touches the brain.
                if app.turn_alive() {
                    log("[turn] interrupted mid-reply — key pressed");
                    app.interrupt_turn();
                }
                app.mouth.shut_up();
                app.signals.static_stop();
                app.signals.set_state("listening");
                app.mouth.ducker.speech_start();
                println!("[ptt] recording (release to send)...");
                app.mic_state.lock().unwrap().btn = true;
                let text = match &ptt_live {
                    Some(p) => app.ears_record(p),
                    None => None,
                };
                app.mic_state.lock().unwrap().btn = false;
                app.mouth.ducker.speech_end(0.2);
                match text {
                    None => {
                        log("[ptt] (tap or empty — ignored)");
                        app.signals.set_state("idle");
                    }
                    Some(t) => {
                        if !app.handle(t) {
                            break 0;
                        }
                    }
                }
            }
        }
    };

    // Exit path: abort any live capture promptly, kill speech + synth
    // FIRST (an in-flight synth has no stop check — killing the serve
    // child fails it fast so the join below can't hang), then join the
    // turn and restore the room on Ctrl-C AND crash paths alike.
    app.mic_state.lock().unwrap().gen += 1;
    app.mouth.shutdown();
    app.interrupt_turn();
    app.join_turn();
    app.signals.static_stop();
    app.signals.set_state("idle");
    app.brain.lock().unwrap().stop();
    log("[butler] hung up");
    code
}

fn brain_dead(mouth: &Mouth, kind: &str) -> i32 {
    log(&format!("[butler] BRAIN CONNECT {kind}"));
    mouth.say("Bad news. The voice and the face are fine, but I couldn't reach my brain, the Claude Code session. Check this window for the error. The usual causes: Claude Code isn't signed in, the internet is down, or the plan is out of usage.");
    mouth.wait_done(Duration::from_secs(30));
    mouth.shutdown();
    1
}

/// Said in full once, then briefly, across every mic path (startup,
/// PTT presses, open-mic errors share the one flag, as in Python).
static MIC_WARNED: std::sync::OnceLock<std::sync::Mutex<bool>> = std::sync::OnceLock::new();

fn mic_warned() -> &'static std::sync::Mutex<bool> {
    MIC_WARNED.get_or_init(|| std::sync::Mutex::new(false))
}

struct App {
    cfg: serde_json::Map<String, serde_json::Value>,
    home: std::path::PathBuf,
    brain: Arc<Mutex<Brain>>,
    mouth: Arc<Mouth>,
    ears: Arc<Ears>,
    signals: Arc<Signals>,
    mic_state: Arc<Mutex<MicState>>,
    quit_phrases: Vec<String>,
    name: String,
    deep_model: String,
    fast_model: String,
    turn: Option<std::thread::JoinHandle<()>>,
    /// Kill handle for the live turn's opencode process, if any.
    proc: Option<Arc<Mutex<Option<std::process::Child>>>>,
    /// Pending auto-approve confirm + when it was posed.
    confirm: Option<(String, Instant)>,
    mic_fails: u32,
}

impl App {
    /// Hold-to-talk capture on the shared ears (the engine cache inside
    /// is loaded once and reused across presses).
    fn ears_record(&self, ptt: &crate::ptt::PTTListener) -> Option<String> {
        match self.ears.record_held(&|| ptt.is_held()) {
            Ok(t) => t.filter(|s| !s.trim().is_empty()),
            Err(e) => {
                // A device-level failure gets plain words instead of a
                // raw exception; anything else goes to the log.
                if !crate::ears::explain_audio_failure(&e, mic_warned()) {
                    log(&format!("[ears] record/transcribe failed: {e:?}"));
                    self.mouth
                        .say("My ears hit an error. Check this window for the details.");
                } else {
                    self.mouth
                        .say("I can't hear you. There's no working microphone I can use.");
                }
                None
            }
        }
    }

    fn turn_alive(&self) -> bool {
        self.turn
            .as_ref()
            .map(|t| !t.is_finished())
            .unwrap_or(false)
    }

    fn interrupt_turn(&self) {
        self.mouth.shut_up();
        if let Some(p) = self.proc.as_ref() {
            if let Some(c) = p.lock().unwrap().as_mut() {
                let _ = c.kill();
            }
        }
    }

    fn join_turn(&mut self) {
        if let Some(t) = self.turn.take() {
            let _ = t.join();
        }
        self.proc = None;
    }

    fn on_mic_error(&mut self, e: &str) -> bool {
        // Stale errors from a pre-switch capture are discarded, never
        // counted: without this a burst in flight during the switch
        // re-fires the failover and re-says the message.
        if self.mic_state.lock().unwrap().mode != MicMode::Open {
            return true;
        }
        if !crate::ears::explain_audio_failure(e, mic_warned()) {
            log(&format!(
                "[ears] open mic failed ({}): {e:?}",
                self.mic_fails + 1
            ));
        }
        self.mic_fails += 1;
        if self.mic_fails >= 3 {
            let mut st = self.mic_state.lock().unwrap();
            st.mode = MicMode::Ptt;
            st.gen += 1;
            drop(st);
            self.mic_fails = 0;
            self.mouth.say("The open microphone keeps failing, so I'm switching to push to talk. Hold the key to reach me, and check this window for the error.");
        }
        true
    }

    /// Process one utterance; False quits the loop.
    fn handle(&mut self, text: String) -> bool {
        if text.trim().is_empty() {
            self.signals.set_state("idle");
            return true;
        }
        log(&format!("[you]    {text}"));
        // A pending auto-approve confirm owns the next utterance for two
        // minutes; after that it expires and speech flows normally.
        if let Some((pend, at)) = self.confirm.take() {
            let expired = at.elapsed() > Duration::from_secs(120);
            let norm = console::norm_speech(&text);
            if !expired
                && ["confirm", "confirmed", "yes confirm", "yes confirmed"].contains(&norm.as_str())
            {
                return self.run_console(format!("{pend}:confirmed"));
            } else if !expired && !console::is_quit(&text, &self.quit_phrases) {
                self.mouth.say("Staying as we are.");
                return true;
            }
            // Expired, or quit wins: fall through.
        }
        if console::is_quit(&text, &self.quit_phrases) {
            self.interrupt_turn();
            self.join_turn();
            self.mouth.shut_up();
            let signoff = self
                .cfg
                .get("signoff")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            self.mouth.say(&signoff);
            self.mouth.wait_done(Duration::from_secs(15));
            return false;
        }
        if self.turn_alive() || self.mouth.speaking() {
            log("[turn] interrupted mid-reply by new input");
            self.interrupt_turn();
        }
        // Let the cancellation fully land (its kill included) BEFORE
        // anything else touches the brain.
        self.join_turn();
        if let Some(verb) = console::console_match(&text) {
            return self.run_console(verb);
        }
        self.signals.set_state("thinking");
        self.signals.static_start();
        self.brain.lock().unwrap().reset_turn();
        let agent = self.name.clone();
        self.spawn_turn(text, agent);
        true
    }

    fn spawn_turn(&mut self, text: String, agent: String) {
        let brain = self.brain.clone();
        let mouth = self.mouth.clone();
        let signals = self.signals.clone();
        let proc = self.brain.lock().unwrap().proc_handle();
        self.proc = Some(proc);
        mouth.clear_stop(); // stale interrupt flag must not mute this turn
        let stop = mouth.stop_flag();
        self.turn = std::thread::Builder::new()
            .name("turn".into())
            .spawn(move || speak_reply(brain, mouth, signals, stop, &text, &agent))
            .ok();
    }

    fn run_console(&mut self, verb: String) -> bool {
        // The current reply was already cancelled and joined by handle().
        let say_after: Option<String>;
        let mut resp = String::new();
        if verb == "clear" {
            resp = self.brain.lock().unwrap().command("/clear");
            say_after = Some("Cleared. Fresh slate.".into());
        } else if verb == "compact" {
            self.mouth.say("Compacting. One moment.");
            resp = self.brain.lock().unwrap().command("/compact");
            say_after = Some("Compacted. Same conversation, smaller footprint.".into());
        } else if verb == "deep" {
            self.mouth.say("Switching to the deep model. Heads up, replies get slower. Say back to the fast model when you're done.");
            resp = self
                .brain
                .lock()
                .unwrap()
                .command(&format!("/model {}", self.deep_model));
            say_after = Some("Deep model online, for this session only.".into());
        } else if verb == "fast" {
            resp = self
                .brain
                .lock()
                .unwrap()
                .command(&format!("/model {}", self.fast_model));
            say_after = Some("Back on the fast model.".into());
        } else if let Some(lvl) = verb.strip_prefix("effort:") {
            resp = self
                .brain
                .lock()
                .unwrap()
                .command(&format!("/effort {lvl}"));
            let saved = console::write_config_key(
                &mut self.cfg,
                &self.home,
                "effort",
                serde_json::Value::String(lvl.into()),
            );
            say_after = Some(if saved {
                format!("Effort set to {lvl}, and saved as your default.")
            } else {
                format!("Effort set to {lvl} for this session. The config file couldn't be written, so it won't stick past a restart.")
            });
        } else if verb == "usage" {
            let b = self.brain.lock().unwrap();
            self.mouth
                .say(&console::spoken_usage(b.turns, b.out_tokens, b.cost));
            say_after = None;
        } else if verb == "micopen" {
            let mut st = self.mic_state.lock().unwrap();
            if st.mode == MicMode::Open {
                self.mouth.say("Already in hands-free listening.");
            } else {
                st.mode = MicMode::Open;
                st.gen += 1;
                drop(st);
                console::write_config_key(
                    &mut self.cfg,
                    &self.home,
                    "mic_mode",
                    serde_json::Value::String("open".into()),
                );
                log("[console] mic_mode -> open (hands-free listening)");
                self.mouth.say("Hands-free listening on. I'm always listening now, so anything said in the room can reach me. The talk key still works, and holding it always gets you heard. Say push to talk mode to bring the button back.");
            }
            say_after = None;
        } else if verb == "micptt" {
            let mut st = self.mic_state.lock().unwrap();
            if st.mode == MicMode::Ptt {
                self.mouth.say("Already on push to talk.");
            } else {
                st.mode = MicMode::Ptt;
                st.gen += 1;
                drop(st);
                console::write_config_key(
                    &mut self.cfg,
                    &self.home,
                    "mic_mode",
                    serde_json::Value::String("ptt".into()),
                );
                log("[console] mic_mode -> ptt");
                let key = self
                    .cfg
                    .get("ptt_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("home")
                    .replace('_', " ");
                self.mouth.say(&format!(
                    "Push to talk. Hold the {key} key and talk; the mic stays closed otherwise."
                ));
            }
            say_after = None;
        } else if verb == "micwake" {
            let mut st = self.mic_state.lock().unwrap();
            if st.mode == MicMode::Wake {
                self.mouth.say("Already in wake word mode.");
            } else {
                st.mode = MicMode::Wake;
                st.gen += 1;
                drop(st);
                console::write_config_key(
                    &mut self.cfg,
                    &self.home,
                    "mic_mode",
                    serde_json::Value::String("wake".into()),
                );
                log("[console] mic_mode -> wake (hey jarvis)");
                self.mouth.say("Wake word mode. I only listen after the wake word. Say go hands free to go back to always listening.");
            }
            say_after = None;
        } else if verb == "noask" {
            self.confirm = Some(("noask".into(), Instant::now()));
            self.mouth.say("Auto-approve means I act without asking permission, and it becomes your saved default. Say confirm to switch.");
            say_after = None;
        } else if verb == "noask:confirmed" {
            let saved = console::write_config_key(
                &mut self.cfg,
                &self.home,
                "permission_mode",
                serde_json::Value::String("bypassPermissions".into()),
            );
            self.brain
                .lock()
                .unwrap()
                .set_permission_mode("bypassPermissions");
            log(&format!(
                "[console] permission_mode -> bypassPermissions{}",
                if saved { " (saved)" } else { " (session only)" }
            ));
            self.mouth.say(&format!("{} Say start asking again any time to flip it back.",
                if saved { "Auto-approve on, and saved as your default." }
                else { "Auto-approve on for this session. The config file couldn't be written, so it won't stick past a restart. " }));
            say_after = None;
        } else if verb == "ask" {
            let saved = console::write_config_key(
                &mut self.cfg,
                &self.home,
                "permission_mode",
                serde_json::Value::String("ask".into()),
            );
            self.brain.lock().unwrap().set_permission_mode("ask");
            log(&format!(
                "[console] permission_mode -> ask{}",
                if saved { " (saved)" } else { " (session only)" }
            ));
            self.mouth.say(&format!(
                "Done. I'll ask out loud before real actions{}",
                if saved {
                    ", and that's saved as your default."
                } else {
                    ". The config file couldn't be written, so tell me again after a restart."
                }
            ));
            say_after = None;
        } else if let Some(want) = verb.strip_prefix("voice:") {
            return self.run_voice_verb(want);
        } else {
            resp = String::new();
            say_after = None;
        }
        if let Some(line) = say_after {
            // The CLI answers slash commands with its own text; an error
            // outranks our line.
            let low = resp.to_lowercase();
            if !resp.is_empty() && (low.contains("error") || low.contains("invalid")) {
                self.mouth.say(&resp[..resp.len().min(160)]);
                log(&format!(
                    "[console] {verb} answered: {}",
                    &resp[..resp.len().min(120)]
                ));
            } else {
                self.mouth.say(&line);
            }
        }
        self.signals.set_state("idle");
        true
    }

    fn run_voice_verb(&mut self, want: &str) -> bool {
        // Confirmed fuzzy candidate: switch without re-asking.
        if let Some(name) = want.strip_suffix(":confirmed") {
            return self.switch_voice(name);
        }
        // Validate against the engine's own voice list (loads the model
        // on first use, like the first spoken reply does).
        let voices = self.mouth.voices();
        // Step 0: underscore-equivalence — even perfect STT ("bf emma")
        // never matches the canonical "bf_emma" without folding.
        if let Some(hit) = voices
            .iter()
            .find(|v| console::fold_voice(v) == console::fold_voice(want))
        {
            let hit = hit.clone();
            return self.switch_voice(&hit);
        }
        let ranked = console::rank_voices(&voices, &console::fold_voice(want));
        match ranked.first() {
            Some((best, d)) if console::voice_close_enough(&console::fold_voice(want), *d) => {
                // Fuzzy hit: NEVER switch silently (a wrong voice
                // persists into future sessions). Ask, reusing the
                // 120s confirm channel; anything else keeps the voice.
                let best = best.to_string();
                log(&format!(
                    "[console] voice fuzzy {want:?} -> candidate {best:?} (awaiting confirm)"
                ));
                self.confirm = Some((format!("voice:{best}"), Instant::now()));
                self.mouth.say(&format!(
                    "Did you mean {}? Say confirm.",
                    best.replace('_', " ")
                ));
            }
            _ => {
                log(&format!("[console] unknown voice {want:?}"));
                let mut msg = format!("I don't know a voice called {want}. Say switch voice to, then one of the built in names.");
                let close: Vec<String> = ranked
                    .iter()
                    .take(3)
                    .map(|(n, _)| n.replace('_', " "))
                    .collect();
                if !close.is_empty() {
                    msg.push_str(&format!(" Closest I have: {}.", close.join(", ")));
                }
                self.mouth.say(&msg);
            }
        }
        self.signals.set_state("idle");
        true
    }

    fn switch_voice(&mut self, want: &str) -> bool {
        if self.mouth.voice_known(want) {
            self.mouth.set_voice(want);
            let saved = console::write_config_key(
                &mut self.cfg,
                &self.home,
                "voice",
                serde_json::Value::String(want.into()),
            );
            log(&format!(
                "[console] voice -> {want}{}",
                if saved { " (saved)" } else { " (session only)" }
            ));
            self.mouth.say(&format!(
                "Voice switched to {want}{}.",
                if saved {
                    ", and saved as your default"
                } else {
                    " for this session"
                }
            ));
        } else {
            log(&format!(
                "[console] confirmed voice no longer known: {want:?}"
            ));
            self.mouth.say(&format!(
                "{want} isn't available after all. Nothing changed."
            ));
        }
        self.signals.set_state("idle");
        true
    }
}

/// Every sentence ships immediately — no batch wait. First sentence
/// is still logged as "to first" for latency, the rest stream as they
/// arrive. This collapses the 1-2s inter-paragraph starvation gap
/// (batch held S2 until S3) to a natural breath inserted by the mouth
/// worker when the queue actually empties. Fuller 2-sentence prosody is
/// opportunistic: if LLM streams fast, two sentences queue back-to-back
/// and play with only the breath gap, not a batch hold.
fn speak_reply(
    brain: Arc<Mutex<Brain>>,
    mouth: Arc<Mouth>,
    signals: Arc<Signals>,
    stop: Arc<AtomicBool>,
    text: &str,
    agent: &str,
) {
    let t0 = Instant::now();
    let mut first = true;
    let mut pending: Vec<String> = Vec::new();

    let mut emit = |raw: &str, mouth: &Arc<Mouth>| {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let (spoken, mut found) = strip_directions(raw);
        if !found.is_empty() {
            pending.append(&mut found);
        }
        if spoken.is_empty() {
            return;
        }
        if first {
            log(&format!(
                "[{agent}] ({:.1}s to first) {spoken}{}",
                t0.elapsed().as_secs_f64(),
                pending_str(&pending)
            ));
            let dirs = std::mem::take(&mut pending);
            mouth.say_chunk(&spoken, dirs);
            first = false;
        } else {
            log(&format!("[{agent}] {spoken}{}", pending_str(&pending)));
            let dirs = std::mem::take(&mut pending);
            mouth.say_chunk(&spoken, dirs);
        }
    };

    let res = {
        let mut b = brain.lock().unwrap();
        let mouth = mouth.clone();
        b.run_turn(text, &mut |s: &str, _dt: f64| emit(s, &mouth))
    };
    if res.is_err() {
        return;
    }
    if first {
        // Zero sentences yielded and nothing queued: nothing will ever
        // dequeue, so nothing resets the bus — park it here.
        signals.static_stop();
        signals.set_state("idle");
    }
}

fn pending_str(pending: &[String]) -> String {
    if pending.is_empty() {
        String::new()
    } else {
        format!("  <directions: {pending:?}>")
    }
}
