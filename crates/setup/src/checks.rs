// Read-only auditors. `check` prints one line per item and exits 1
// when anything fails. No check changes the system.

use std::path::Path;

pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn ok(name: &'static str, detail: String) -> Check {
    Check {
        name,
        ok: true,
        detail,
    }
}

fn fail(name: &'static str, detail: String) -> Check {
    Check {
        name,
        ok: false,
        detail,
    }
}

fn have_on_path(prog: &str) -> bool {
    std::env::var_os("PATH").map_or(false, |p| {
        std::env::split_paths(&p)
            .map(|d| d.join(prog))
            .any(|f| f.is_file())
    })
}

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

fn sha256sum(path: &Path) -> Option<String> {
    sh("sha256sum", &[&path.to_string_lossy()])
        .and_then(|o| o.split_whitespace().next().map(|s| s.to_string()))
}

fn home_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn all(home: &Path) -> Vec<Check> {
    let mut v = vec![];
    v.push(check_env_file(home));
    v.push(check_render_clean(home));
    v.push(check_vault(home));
    v.push(check_kokoro());
    v.push(check_stt(home));
    v.push(check_wake(home));
    v.push(check_bins());
    v.push(check_input_access());
    v.push(check_ports());
    v.push(check_toolbox(home));
    v.push(check_opencode());
    v.push(check_binaries(home));
    v
}

fn check_env_file(home: &Path) -> Check {
    let p = home.join(".env");
    if !p.is_file() {
        return fail("env-file", "no .env (run `setup init`)".into());
    }
    let vars = openbutler_setup::envfile::load(home);
    for k in ["AGENT_NAME", "MEMORY_VAULT", "VOICE_CONTAINER"] {
        if vars.get(k).map(|s| s.is_empty()).unwrap_or(true) {
            return fail("env-file", format!(".env present but {k} is empty"));
        }
    }
    ok("env-file", format!("{} keys", vars.len()))
}

fn check_render_clean(home: &Path) -> Check {
    let vars = openbutler_setup::envfile::load(home);
    let drift = openbutler_setup::render::check_all(home, &vars);
    if drift.is_empty() {
        ok("render", "configs match .env".into())
    } else {
        fail(
            "render",
            format!(
                "drift: {} (run `setup init --yes` to re-render)",
                drift.join(", ")
            ),
        )
    }
}

fn check_vault(home: &Path) -> Check {
    let vars = openbutler_setup::envfile::load(home);
    let raw = vars.get("MEMORY_VAULT").cloned().unwrap_or_default();
    if raw.is_empty() {
        return fail("vault", "MEMORY_VAULT unset".into());
    }
    let p = std::path::PathBuf::from(openbutler_common::expand(&raw));
    if p.is_dir() {
        ok("vault", p.display().to_string())
    } else {
        fail("vault", format!("{} missing (mkdir -p it)", p.display()))
    }
}

fn kokoro_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("KOKORO_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    home_dir().join(".cache/jarvis/kokoro")
}

fn check_kokoro() -> Check {
    let d = kokoro_dir();
    let voices = d.join("voices-v1.0.bin").is_file();
    let onnx = std::fs::read_dir(&d)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|x| x == "onnx").unwrap_or(false))
        })
        .unwrap_or(false);
    if voices && onnx {
        ok("kokoro", d.display().to_string())
    } else {
        fail(
            "kokoro",
            format!(
                "need voices-v1.0.bin + .onnx under {} (see toolbox-setup.sh)",
                d.display()
            ),
        )
    }
}

fn check_stt(home: &Path) -> Check {
    let cfg = openbutler_common::read_json_object(&home.join("configs/voice.json"));
    let model = cfg
        .get("stt_model")
        .and_then(|v| v.as_str())
        .unwrap_or("small.en");
    let hub = std::env::var("HF_HUB_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cache/huggingface/hub"));
    let dir = hub.join(format!("models--Systran--faster-whisper-{model}"));
    let snaps = dir.join("snapshots");
    let has = std::fs::read_dir(&snaps)
        .map(|r| r.filter_map(|e| e.ok()).next().is_some())
        .unwrap_or(false);
    if has {
        ok("stt", format!("{model} cached"))
    } else {
        fail(
            "stt",
            format!("{model} not in HF cache (first `ears.warm()` downloads it)"),
        )
    }
}

fn check_wake(home: &Path) -> Check {
    let pins = openbutler_common::read_json_object(&home.join("models/wake.json"));
    let files = pins.get("files").and_then(|v| v.as_object());
    let dir = std::env::var("JARVIS_WAKE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cache/jarvis/wake"));
    let Some(files) = files else {
        return fail("wake", "models/wake.json unreadable".into());
    };
    let mut missing = vec![];
    let mut bad = vec![];
    for (name, spec) in files {
        let p = dir.join(name);
        if !p.is_file() {
            missing.push(name.clone());
            continue;
        }
        if let Some(want) = spec.get("sha256").and_then(|v| v.as_str()) {
            match sha256sum(&p) {
                Some(got) if got == want => {}
                Some(got) => bad.push(format!("{name} (got {got:.12}…)")),
                None => bad.push(format!("{name} (sha256sum unavailable)")),
            }
        }
    }
    if missing.is_empty() && bad.is_empty() {
        // Configured classifier (wake.model): pinned entries get a sha
        // check, custom ids just need the file in the cache dir.
        let (id, _) = openbutler_setup::render::effective_wake_model(home);
        let reg = openbutler_setup::render::wake_registry(home)
            .into_iter()
            .find(|m| m.id == id);
        let file = reg
            .as_ref()
            .map(|m| m.file.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{id}_v0.1.onnx"));
        let p = dir.join(&file);
        if !p.is_file() {
            return fail(
                "wake",
                format!("model {id} missing: {file} not in cache (run `setup init`)"),
            );
        }
        if let Some(m) = reg.filter(|m| !m.sha256.is_empty()) {
            match sha256sum(&p) {
                Some(got) if got == m.sha256 => {}
                Some(got) => {
                    return fail(
                        "wake",
                        format!("model {id} corrupt: {file} (got {got:.12}…)"),
                    );
                }
                None => {
                    return fail("wake", format!("model {id}: sha256sum unavailable"));
                }
            }
        }
        ok(
            "wake",
            format!("{} files pinned-ok, model {id}", files.len()),
        )
    } else {
        let mut d = vec![];
        if !missing.is_empty() {
            d.push(format!("missing: {}", missing.join(", ")));
        }
        if !bad.is_empty() {
            d.push(format!("bad: {}", bad.join(", ")));
        }
        fail("wake", d.join("; "))
    }
}

fn check_bins() -> Check {
    let mut missing: Vec<&str> = vec![];
    for b in ["espeak-ng", "ffmpeg"] {
        if !have_on_path(b) {
            missing.push(b);
        }
    }
    if missing.is_empty() {
        ok("bins", "espeak-ng + ffmpeg present".into())
    } else {
        fail(
            "bins",
            format!("missing: {} (see toolbox-setup.sh)", missing.join(", ")),
        )
    }
}

fn check_input_access() -> Check {
    let in_group = sh("id", &["-Gn"])
        .map(|g| g.split_whitespace().any(|x| x == "input"))
        .unwrap_or(false);
    let rule = [
        "/usr/lib/udev/rules.d/70-openbutler-input.rules",
        "/etc/udev/rules.d/70-openbutler-input.rules",
        // Legacy filename from the pre-OSS layout still counts.
        "/usr/lib/udev/rules.d/70-jarvis-input.rules",
        "/etc/udev/rules.d/70-jarvis-input.rules",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).is_file());
    let readable = std::fs::read_dir("/dev/input").is_ok();
    if readable || in_group || rule {
        ok(
            "input",
            format!("group={in_group} rule={rule} readable={readable}"),
        )
    } else {
        fail("input", "no /dev/input access — PTT deaf-degrades (hands-free unaffected); install udev/70-openbutler-input.rules or join `input`".into())
    }
}

fn check_ports() -> Check {
    let mut busy = vec![];
    for p in [8790u16, 8794, 8791] {
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_err() {
            busy.push(p.to_string());
        }
    }
    if busy.is_empty() {
        ok("ports", "8790/8794/8791 free".into())
    } else {
        fail(
            "ports",
            format!("busy: {} (a voice line may be running)", busy.join(",")),
        )
    }
}

fn check_toolbox(home: &Path) -> Check {
    let vars = openbutler_setup::envfile::load(home);
    let c = vars
        .get("VOICE_CONTAINER")
        .cloned()
        .unwrap_or_else(|| "openbutler".into());
    match sh("toolbox", &["list", "--containers"]) {
        Some(o) if o.contains(&c) => ok("toolbox", format!("container {c} present")),
        Some(_) => fail(
            "toolbox",
            format!("container {c} missing (toolbox create {c})"),
        ),
        None => fail("toolbox", "toolbox not on PATH".into()),
    }
}

fn check_opencode() -> Check {
    match sh("opencode", &["--version"]) {
        Some(v) => ok(
            "opencode",
            v.lines().next().unwrap_or("present").to_string(),
        ),
        None => fail("opencode", "not on PATH (brain needs it)".into()),
    }
}

fn check_binaries(home: &Path) -> Check {
    let td = home.join("target/debug");
    let mut missing: Vec<&str> = vec![];
    for b in [
        "openbutler",
        "openbutler-voice",
        "openbutler-face",
        "openbutler-board",
        "openbutler-tts",
        "openbutler-stt",
        "openbutler-aspike",
        "openbutler-setup",
    ] {
        if !td.join(b).is_file() {
            missing.push(b);
        }
    }
    if missing.is_empty() {
        ok(
            "rust-bins",
            "all 8 present (see `openbutler config` etc.)".into(),
        )
    } else {
        fail(
            "rust-bins",
            format!("missing: {} (cargo build in container)", missing.join(", ")),
        )
    }
}

/// Print table, return exit code.
pub fn run(home: &Path) -> i32 {
    let mut bad = 0;
    for c in all(home) {
        println!(
            "{:>10}  {}  {}",
            if c.ok { "ok" } else { "GAP" },
            c.name,
            c.detail
        );
        if !c.ok {
            bad += 1;
        }
    }
    if bad > 0 {
        println!("{bad} gap(s) — see `setup init` / `setup fix`.");
        1
    } else {
        println!("all green.");
        0
    }
}
