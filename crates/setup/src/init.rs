// Interactive first-use walk: identity -> .env -> render -> models.
// `--yes` takes current .env (or template defaults) with no prompts.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

fn prompt(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    label: &str,
    default: &str,
) -> String {
    print!("{label} [{default}]: ");
    let _ = std::io::stdout().flush();
    match lines.next() {
        Some(Ok(l)) => {
            let l = l.trim().to_string();
            if l.is_empty() {
                default.to_string()
            } else {
                l
            }
        }
        _ => default.to_string(), // EOF: take defaults, never hang
    }
}

fn confirm(lines: &mut std::io::Lines<std::io::StdinLock<'_>>, label: &str, def_yes: bool) -> bool {
    let hint = if def_yes { "Y/n" } else { "y/N" };
    let r = prompt(
        lines,
        &format!("{label} ({hint})"),
        if def_yes { "y" } else { "n" },
    );
    match r.to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => def_yes,
    }
}

fn run_render(home: &Path, vars: &std::collections::BTreeMap<String, String>) -> bool {
    match crate::render::render_all(home, vars) {
        Ok(written) => {
            println!("rendered {}.", written.join(", "));
            true
        }
        Err(e) => {
            println!("render failed: {e}");
            false
        }
    }
}

fn kokoro_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("KOKORO_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".cache/jarvis/kokoro")
}

/// Kokoro TTS weights: any .onnx model + voices-v1.0.bin, from the
/// thewh1teagle model-files-v1.0 release (~340MB total). Mirrors
/// fetch_wake: skip-if-present, confirm, curl.
fn fetch_kokoro(lines: &mut std::io::Lines<std::io::StdinLock<'_>>, yes: bool) -> bool {
    const BASE: &str =
        "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0";
    let dir = kokoro_dir();
    let _ = std::fs::create_dir_all(&dir);
    let has_onnx = std::fs::read_dir(&dir)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|x| x == "onnx").unwrap_or(false))
        })
        .unwrap_or(false);
    let mut need: Vec<&str> = vec![];
    if !has_onnx {
        need.push("kokoro-v1.0.onnx");
    }
    if !dir.join("voices-v1.0.bin").is_file() {
        need.push("voices-v1.0.bin");
    }
    if need.is_empty() {
        println!("kokoro models: present.");
        return true;
    }
    println!("kokoro models missing: {}", need.join(", "));
    let go = yes
        || confirm(
            lines,
            "download them now (~340MB, Apache-2.0 Kokoro-82M weights)",
            true,
        );
    if !go {
        println!("kokoro: skipped (the voice line cannot speak without them).");
        return true;
    }
    let mut ok = true;
    for name in need {
        let url = format!("{BASE}/{name}");
        let dest = dir.join(name);
        print!("fetching {name}... ");
        let _ = std::io::stdout().flush();
        match std::process::Command::new("curl")
            .args(["-sSL", &url, "-o", &dest.to_string_lossy()])
            .output()
        {
            Ok(o) if o.status.success() && dest.is_file() => println!("ok"),
            _ => {
                println!("FAILED ({url})");
                ok = false;
            }
        }
    }
    ok
}

fn fetch_wake(home: &Path, lines: &mut std::io::Lines<std::io::StdinLock<'_>>, yes: bool) -> bool {
    let pins = openbutler_common::read_json_object(&home.join("models/wake.json"));
    let Some(files) = pins.get("files").and_then(|v| v.as_object()) else {
        println!("wake: no pins in models/wake.json, skipping.");
        return true;
    };
    let base = pins
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dir = std::env::var("JARVIS_WAKE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".cache/jarvis/wake")
        });
    let _ = std::fs::create_dir_all(&dir);
    let mut need: Vec<(&String, &serde_json::Value)> = vec![];
    for (name, spec) in files {
        if !dir.join(name).is_file() {
            need.push((name, spec));
        }
    }
    if need.is_empty() {
        println!("wake models: present.");
        return true;
    }
    println!(
        "wake models missing: {}",
        need.iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let go = yes
        || confirm(
            lines,
            "download them now (~4MB, Apache-2.0, openWakeWord releases)",
            true,
        );
    if !go {
        println!("wake: skipped (wake-word mode will degrade loudly without them).");
        return true;
    }
    let mut ok = true;
    for (name, _) in need {
        let url = format!("{base}/{name}");
        let dest = dir.join(name);
        print!("fetching {name}... ");
        let _ = std::io::stdout().flush();
        match std::process::Command::new("curl")
            .args(["-sSL", &url, "-o", &dest.to_string_lossy()])
            .output()
        {
            Ok(o) if o.status.success() && dest.is_file() => println!("ok"),
            _ => {
                println!("FAILED ({url})");
                ok = false;
            }
        }
    }
    // Configured classifier (wake.model): fetch when the registry pins it
    // and the cache lacks it. Custom (unpinned) ids must be placed by hand.
    {
        let (id, _) = crate::render::effective_wake_model(home);
        if let Some(m) = crate::render::wake_registry(home)
            .into_iter()
            .find(|m| m.id == id)
        {
            if !m.file.is_empty() && !dir.join(&m.file).is_file() {
                let go = yes
                    || confirm(
                        lines,
                        &format!(
                            "wake model {id} missing — download {} (~{}MB)",
                            m.file,
                            m.bytes / 1_000_000
                        ),
                        true,
                    );
                if !go {
                    println!("wake model {id}: skipped (wake-word mode degrades without it).");
                    return true;
                }
                let url = format!("{base}/{}", m.file);
                let dest = dir.join(&m.file);
                print!("fetching {}... ", m.file);
                let _ = std::io::stdout().flush();
                match std::process::Command::new("curl")
                    .args(["-sSL", &url, "-o", &dest.to_string_lossy()])
                    .output()
                {
                    Ok(o) if o.status.success() && dest.is_file() => println!("ok"),
                    _ => {
                        println!("FAILED ({url})");
                        ok = false;
                    }
                }
            }
        }
    }
    ok
}

pub fn run(home: &Path, yes: bool) -> i32 {
    let cur = crate::envfile::load(home);
    let get = |k: &str, d: &str| cur.get(k).cloned().unwrap_or_else(|| d.to_string());
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    let mut vals: BTreeMap<String, String> = if yes {
        let mut v = cur.clone();
        for (k, d) in [
            ("AGENT_NAME", "Assistant"),
            ("VOICE_CONTAINER", "openbutler"),
            ("FACE_PORT", "8790"),
            ("HANDS_PORT", "8794"),
            ("FACE_NAME", "board"),
            ("VOICE_NAME", "bm_lewis"),
            ("STT_MODEL", "small.en"),
            ("UPSTREAM_ORG", "your-org"),
            ("COPYRIGHT_HOLDER", "Your Name"),
        ] {
            v.entry(k.into()).or_insert_with(|| d.into());
        }
        if !v.contains_key("MEMORY_VAULT") {
            v.insert(
                "MEMORY_VAULT".into(),
                format!(
                    "{}/openbutler-vault",
                    std::env::var("HOME").unwrap_or_else(|_| "~".into())
                ),
            );
        }
        v
    } else {
        println!(
            "First-use setup for this agent home ({}) — Enter keeps [default].",
            home.display()
        );
        let mut v = BTreeMap::new();
        v.insert(
            "AGENT_NAME".into(),
            prompt(
                &mut lines,
                "Agent name (greetings, quit phrases)",
                &get("AGENT_NAME", "Assistant"),
            ),
        );
        v.insert(
            "MEMORY_VAULT".into(),
            prompt(
                &mut lines,
                "Memory vault path (Obsidian folder)",
                &get(
                    "MEMORY_VAULT",
                    &format!(
                        "{}/openbutler-vault",
                        std::env::var("HOME").unwrap_or_else(|_| "~".into())
                    ),
                ),
            ),
        );
        v.insert(
            "VOICE_CONTAINER".into(),
            prompt(
                &mut lines,
                "Toolbox container for builds",
                &get("VOICE_CONTAINER", "openbutler"),
            ),
        );
        v.insert(
            "FACE_PORT".into(),
            prompt(
                &mut lines,
                "Face port (127.0.0.1)",
                &get("FACE_PORT", "8790"),
            ),
        );
        v.insert(
            "HANDS_PORT".into(),
            prompt(
                &mut lines,
                "Board port (127.0.0.1)",
                &get("HANDS_PORT", "8794"),
            ),
        );
        v.insert(
            "FACE_NAME".into(),
            prompt(
                &mut lines,
                "Face (board/neural/radial/rain)",
                &get("FACE_NAME", "board"),
            ),
        );
        v.insert(
            "VOICE_NAME".into(),
            prompt(
                &mut lines,
                "Voice (e.g. bm_lewis)",
                &get("VOICE_NAME", "bm_lewis"),
            ),
        );
        v.insert(
            "STT_MODEL".into(),
            prompt(
                &mut lines,
                "STT model (small.en/medium.en)",
                &get("STT_MODEL", "small.en"),
            ),
        );
        // Preserve white-label + any custom keys untouched.
        for k in ["UPSTREAM_ORG", "COPYRIGHT_HOLDER", "GREETING"] {
            if let Some(val) = cur.get(k) {
                v.insert(k.into(), val.clone());
            }
        }
        for (k, val) in cur.iter() {
            if !crate::envfile::KNOWN_KEYS.contains(&k.as_str()) {
                v.entry(k.clone()).or_insert(val.clone());
            }
        }
        v
    };

    // AGENT_HOME is computed, never prompted: render needs the real path.
    vals.insert("AGENT_HOME".into(), home.to_string_lossy().into_owned());
    // Vault must exist before render points JSONs at it.
    let vault =
        openbutler_common::expand(vals.get("MEMORY_VAULT").map(|s| s.as_str()).unwrap_or(""));
    if !vault.is_empty() && !std::path::Path::new(&vault).is_dir() {
        let mk = yes || confirm(&mut lines, &format!("create vault dir {vault}"), true);
        if mk {
            if let Err(e) = std::fs::create_dir_all(&vault) {
                eprintln!("cannot create {vault}: {e}");
                return 1;
            }
        }
    }
    if let Err(e) = crate::envfile::write(home, &vals) {
        eprintln!("{e}");
        return 1;
    }
    println!("wrote {}", home.join(".env").display());
    if !run_render(home, &vals) {
        return 1;
    }
    // Wake word follows the assistant's name when a stock classifier
    // says it; otherwise the default stands (custom names need a
    // trained classifier — see README).
    {
        let agent = vals.get("AGENT_NAME").map(|s| s.as_str()).unwrap_or("");
        if let Some(m) = crate::render::derive_wake_model(home, agent) {
            let (id, phrase) = crate::render::ensure_wake_model(home, &m.id);
            println!("wake word: \"{phrase}\" (model {id}).");
        } else {
            let (id, phrase) = crate::render::effective_wake_model(home);
            if agent.is_empty() {
                println!("wake word: \"{phrase}\" (model {id}).");
            } else {
                println!(
                    "wake word: \"{phrase}\" (model {id}) — no stock classifier says \"{agent}\"; switch anytime with `voice config set wake.model <id>`."
                );
            }
        }
    }
    if !fetch_wake(home, &mut lines, yes) {
        return 1;
    }
    if !fetch_kokoro(&mut lines, yes) {
        return 1;
    }
    println!("\nsetup complete. Audit anytime: `openbutler-setup check`.");
    println!("Build once (needs g++ — host lacks libstdc++ for heavy crates):");
    println!(
        "  toolbox run -c {} {}/toolbox-setup.sh",
        vals.get("VOICE_CONTAINER")
            .map(|s| s.as_str())
            .unwrap_or("openbutler"),
        home.display()
    );
    println!("  cargo build   (heavy crates: build inside that container)");
    println!("Launch (from a real terminal, not an AI session):");
    println!("  {}/start.sh", home.display());
    0
}
