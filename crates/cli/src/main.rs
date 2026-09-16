// `openbutler config` — the one CLI for every setting: tuning keys write
// the live JSON, seeded keys sync `.env` too, identity keys re-render.
// Validation + routing come from openbutler-common.

use std::collections::BTreeMap;
use std::path::Path;

use openbutler_common::settings as S;

const NAME: &str = "openbutler";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq)]
enum Target {
    // Live JSON is the runtime truth; `.env` seed synced where set.
    Tuning,
    // `.env` only, no JSON counterpart.
    Env,
    // `.env` plus a re-render, so managed values propagate.
    EnvRender,
}

struct UniKey {
    key: &'static str,
    target: Target,
    env_seed: Option<&'static str>,
    blurb: &'static str,
}

const KEYS: &[UniKey] = &[
    UniKey {
        key: "name",
        target: Target::EnvRender,
        env_seed: Some("AGENT_NAME"),
        blurb: "assistant name (greetings, quit phrase, face/board titles)",
    },
    UniKey {
        key: "memory_vault",
        target: Target::EnvRender,
        env_seed: Some("MEMORY_VAULT"),
        blurb: "memory vault dir (Obsidian folder)",
    },
    UniKey {
        key: "voice_container",
        target: Target::Env,
        env_seed: Some("VOICE_CONTAINER"),
        blurb: "toolbox container for heavy builds",
    },
    UniKey {
        key: "speed",
        target: Target::Tuning,
        env_seed: None,
        blurb: "speech speed 0.5-2.0",
    },
    UniKey {
        key: "voice",
        target: Target::Tuning,
        env_seed: Some("VOICE_NAME"),
        blurb: "TTS voice (e.g. bm_lewis)",
    },
    UniKey {
        key: "effort",
        target: Target::Tuning,
        env_seed: None,
        blurb: "reasoning effort low/medium/high/max",
    },
    UniKey {
        key: "mic_mode",
        target: Target::Tuning,
        env_seed: None,
        blurb: "mic mode ptt/open/wake",
    },
    UniKey {
        key: "stt_model",
        target: Target::Tuning,
        env_seed: Some("STT_MODEL"),
        blurb: "STT model (e.g. small.en, medium.en)",
    },
    UniKey {
        key: "greeting",
        target: Target::Tuning,
        env_seed: Some("GREETING"),
        blurb: "spoken greeting",
    },
    UniKey {
        key: "face.name",
        target: Target::Tuning,
        env_seed: None,
        blurb: "face agent name chip",
    },
    UniKey {
        key: "face.face",
        target: Target::Tuning,
        env_seed: Some("FACE_NAME"),
        blurb: "face id (board/neural/radial/rain)",
    },
    UniKey {
        key: "face.port",
        target: Target::Tuning,
        env_seed: Some("FACE_PORT"),
        blurb: "face port on 127.0.0.1",
    },
    UniKey {
        key: "board.name",
        target: Target::Tuning,
        env_seed: None,
        blurb: "board agent name",
    },
    UniKey {
        key: "board.port",
        target: Target::Tuning,
        env_seed: Some("HANDS_PORT"),
        blurb: "board port on 127.0.0.1",
    },
    UniKey {
        key: "wake.model",
        target: Target::Tuning,
        env_seed: None,
        blurb: "wake-word model id (see models/wake.json)",
    },
    UniKey {
        key: "wake.threshold",
        target: Target::Tuning,
        env_seed: None,
        blurb: "wake threshold 0-1 (lower = jumpier)",
    },
    UniKey {
        key: "wake.patience",
        target: Target::Tuning,
        env_seed: None,
        blurb: "wake patience, frames in a row (>=1)",
    },
    UniKey {
        key: "wake.attention_s",
        target: Target::Tuning,
        env_seed: None,
        blurb: "wake attention window, seconds (>0)",
    },
    UniKey {
        key: "upstream_org",
        target: Target::Env,
        env_seed: Some("UPSTREAM_ORG"),
        blurb: "upstream attribution org",
    },
    UniKey {
        key: "copyright_holder",
        target: Target::Env,
        env_seed: Some("COPYRIGHT_HOLDER"),
        blurb: "copyright holder",
    },
];

fn canonical(raw: &str) -> Option<&'static UniKey> {
    if let Some(k) = KEYS.iter().find(|k| k.key == raw) {
        return Some(k);
    }
    let upper = raw.to_ascii_uppercase();
    KEYS.iter().find(|k| k.env_seed == Some(upper.as_str()))
}

fn file_value(home: &Path, key: &str) -> Option<serde_json::Value> {
    let o = openbutler_common::read_json_object(&S::setting_path(home, key));
    S::get_config_path(&o, key).cloned()
}

fn effective(home: &Path, vars: &BTreeMap<String, String>, k: &UniKey) -> serde_json::Value {
    let seed = k
        .env_seed
        .and_then(|e| vars.get(e))
        .filter(|s| !s.is_empty())
        .cloned();
    match k.target {
        Target::Env | Target::EnvRender => seed
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        Target::Tuning if k.key.starts_with("face.") || k.key.starts_with("board.") => {
            let voice_map = openbutler_common::read_json_object(&home.join("configs/voice.json"));
            S::get_setting(home, &voice_map, k.key).unwrap_or_else(|| S::default_value(k.key))
        }
        Target::Tuning => {
            if let Some(v) = file_value(home, k.key) {
                return v;
            }
            if let Some(s) = seed {
                return serde_json::Value::String(s);
            }
            S::default_value(k.key)
        }
    }
}

fn fmt_val(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "(unset)".into(),
        _ => v.to_string(),
    }
}

fn key_list() -> String {
    KEYS.iter().map(|k| k.key).collect::<Vec<_>>().join(", ")
}

fn run_list(home: &Path) -> i32 {
    let vars = openbutler_setup::envfile::load(home);
    for k in KEYS {
        println!(
            "{:<16} = {}  # {}",
            k.key,
            fmt_val(&effective(home, &vars, k)),
            k.blurb
        );
    }
    println!("Takes effect on next launch. See also: `openbutler-setup check`.");
    0
}

fn run_get(home: &Path, raw_key: &str) -> i32 {
    let Some(k) = canonical(raw_key) else {
        eprintln!("unknown setting {raw_key:?}\nkeys: {}", key_list());
        return 2;
    };
    let vars = openbutler_setup::envfile::load(home);
    println!("{} = {}", k.key, fmt_val(&effective(home, &vars, k)));
    0
}

fn run_set(home: &Path, raw_key: &str, raw_val: &str) -> i32 {
    let Some(k) = canonical(raw_key) else {
        eprintln!("unknown setting {raw_key:?}\nkeys: {}", key_list());
        return 2;
    };
    let val = match k.target {
        Target::Tuning => match S::parse_setting(k.key, raw_val) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        },
        Target::Env | Target::EnvRender => {
            let s = raw_val.trim();
            if s.is_empty() {
                eprintln!("{} can't be empty", k.key);
                return 2;
            }
            serde_json::Value::String(s.to_string())
        }
    };
    let mut wrote_json = false;
    if k.target == Target::Tuning {
        if let Err(e) = std::fs::create_dir_all(home.join("configs")) {
            eprintln!("mkdir configs: {e}");
            return 1;
        }
        match S::write_dotted_key(home, k.key, val.clone()) {
            Ok(()) => wrote_json = true,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    }
    let mut vars = openbutler_setup::envfile::load(home);
    let mut env_changed = false;
    if let Some(seed) = k.env_seed {
        let s = match &val {
            serde_json::Value::String(s) => s.clone(),
            _ => val.to_string(),
        };
        if vars.get(seed).map(|v| v.as_str()) != Some(s.as_str()) {
            vars.insert(seed.into(), s);
            env_changed = true;
        }
    }
    if env_changed {
        if let Err(e) = openbutler_setup::envfile::write(home, &vars) {
            eprintln!("{e}");
            return 1;
        }
    }
    let mut rendered = false;
    if k.target == Target::EnvRender {
        match openbutler_setup::render::render_all(home, &vars) {
            Ok(_) => rendered = true,
            Err(e) => {
                eprintln!("render failed: {e}");
                return 1;
            }
        }
    }
    let where_wrote = match (wrote_json, env_changed, rendered) {
        (true, true, _) => "live JSON + .env seed",
        (true, false, _) if k.env_seed.is_some() => "live JSON (.env seed already matched)",
        (true, false, _) => "live JSON",
        (false, true, true) => ".env + re-rendered JSONs",
        (false, true, false) => ".env",
        _ => "no change (already set)",
    };
    println!(
        "{} = {} (saved to {where_wrote}). Takes effect on next launch.",
        k.key,
        fmt_val(&val)
    );
    0
}

fn print_help() {
    println!("{NAME} {VERSION} — one CLI for every setting");
    println!("  --agent-home DIR   agent home (default: discovered)");
    println!("  config             list every setting with its effective value");
    println!("  config get <key>   print one setting (UPPER .env aliases accepted)");
    println!("  config set <k> <v> validate, write live JSON + .env seed, re-render where managed");
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut home_flag: Option<String> = None;
    let mut args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-V" | "--version" => {
                println!("{NAME} {VERSION}");
                return;
            }
            "--agent-home" if i + 1 < raw.len() => {
                home_flag = Some(raw[i + 1].clone());
                i += 2;
            }
            _ => {
                args.push(raw[i].clone());
                i += 1;
            }
        }
    }
    let home = openbutler_common::agent_home(home_flag.as_deref());
    if !home.join("Cargo.toml").is_file() || !home.join("crates").is_dir() {
        eprintln!(
            "{NAME}: not an OpenButler home (no Cargo.toml + crates/ under {})",
            home.display()
        );
        std::process::exit(2);
    }
    match args.first().map(|s| s.as_str()) {
        None => print_help(),
        Some("config") => match args.get(1).map(|s| s.as_str()) {
            None => std::process::exit(run_list(&home)),
            Some("get") => {
                let key = args.get(2).cloned().unwrap_or_default();
                if key.is_empty() {
                    eprintln!("usage: {NAME} config get <key>\nkeys: {}", key_list());
                    std::process::exit(2);
                }
                std::process::exit(run_get(&home, &key));
            }
            Some("set") => {
                let (key, val) = (
                    args.get(2).cloned().unwrap_or_default(),
                    args.get(3).cloned().unwrap_or_default(),
                );
                if key.is_empty() || val.is_empty() {
                    eprintln!(
                        "usage: {NAME} config set <key> <value>\nkeys: {}",
                        key_list()
                    );
                    std::process::exit(2);
                }
                std::process::exit(run_set(&home, &key, &val));
            }
            Some(other) => {
                eprintln!("{NAME}: unknown config verb '{other}' (see --help)");
                std::process::exit(2);
            }
        },
        Some(other) => {
            eprintln!("{NAME}: unknown subcommand '{other}' (see --help)");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ob-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("configs")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        let mut vars = openbutler_setup::envfile::load(&dir);
        for (k, d) in [
            ("AGENT_NAME", "Assistant"),
            ("MEMORY_VAULT", "/tmp/vault"),
            ("VOICE_CONTAINER", "openbutler"),
            ("FACE_PORT", "8790"),
            ("HANDS_PORT", "8794"),
            ("FACE_NAME", "board"),
            ("VOICE_NAME", "bm_lewis"),
            ("STT_MODEL", "small.en"),
        ] {
            vars.entry(k.into()).or_insert_with(|| d.into());
        }
        vars.insert("AGENT_HOME".into(), dir.to_string_lossy().into_owned());
        openbutler_setup::envfile::write(&dir, &vars).unwrap();
        openbutler_setup::render::render_all(&dir, &vars).unwrap();
        dir
    }

    #[test]
    fn stt_model_writes_both_sides() {
        let home = setup_home("both");
        assert_eq!(run_set(&home, "stt_model", "medium.en"), 0);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("configs/voice.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["stt_model"], serde_json::json!("medium.en"));
        let env = openbutler_setup::envfile::load(&home);
        assert_eq!(env.get("STT_MODEL").map(|s| s.as_str()), Some("medium.en"));
        assert!(openbutler_setup::render::check_all(&home, &env).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn name_propagates_via_render() {
        let home = setup_home("name");
        assert_eq!(run_set(&home, "name", "Butler"), 0);
        for f in [
            "configs/voice.json",
            "configs/face.json",
            "configs/board.json",
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(home.join(f)).unwrap()).unwrap();
            assert_eq!(v["name"], serde_json::json!("Butler"), "{f}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let home = setup_home("valid");
        assert_eq!(run_set(&home, "speed", "99"), 2);
        assert_eq!(run_set(&home, "face.port", "0"), 2);
        assert_eq!(run_set(&home, "mic_mode", "loud"), 2);
        assert_eq!(run_set(&home, "wake.threshold", "2"), 2);
        assert_eq!(run_set(&home, "bogus", "x"), 2);
        assert_eq!(run_get(&home, "bogus"), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn unparsable_json_refuses_without_env_change() {
        let home = setup_home("safe");
        std::fs::write(home.join("configs/voice.json"), "{oops").unwrap();
        assert_eq!(run_set(&home, "speed", "1.2"), 1);
        let env = openbutler_setup::envfile::load(&home);
        assert!(!env.contains_key("SPEED"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn aliases_and_fallbacks() {
        let home = setup_home("alias");
        assert_eq!(canonical("STT_MODEL").unwrap().key, "stt_model");
        assert_eq!(canonical("AGENT_NAME").unwrap().key, "name");
        assert_eq!(canonical("FACE_PORT").unwrap().key, "face.port");
        assert!(canonical("bogus").is_none());
        let vars = openbutler_setup::envfile::load(&home);
        let k = canonical("speed").unwrap();
        assert_eq!(effective(&home, &vars, k), serde_json::json!(1.0));
        let _ = std::fs::remove_dir_all(&home);
    }
}
