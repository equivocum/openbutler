// Renders configs/{voice,face,board}.json from .env — native port of the
// old scripts/render-configs.sh, so setup needs no python at all.
//
// Semantics preserved: the live file wins as the base (else the
// *.example template, else {}); unknown keys are preserved; only the
// managed keys are written; voice/stt_model/greeting fill in only when
// missing so local tuning survives re-renders.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn env(vars: &BTreeMap<String, String>, k: &str) -> String {
    vars.get(k).cloned().unwrap_or_default()
}

fn read_base(home: &Path, rel: &str) -> serde_json::Map<String, serde_json::Value> {
    let live = home.join(rel);
    let example = home.join(format!("{rel}.example"));
    for p in [&live, &example] {
        if let Ok(t) = std::fs::read_to_string(p) {
            if let Ok(serde_json::Value::Object(o)) = serde_json::from_str(&t) {
                return o;
            }
        }
    }
    serde_json::Map::new()
}

fn setdefault_str(cfg: &mut serde_json::Map<String, serde_json::Value>, key: &str, val: &str) {
    let missing = cfg
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(true);
    if missing && !val.is_empty() {
        cfg.insert(key.into(), serde_json::Value::String(val.to_string()));
    }
}

fn render_voice(home: &Path, vars: &BTreeMap<String, String>) -> (PathBuf, String) {
    let rel = "configs/voice.json";
    let mut cfg = read_base(home, rel);
    let ah = env(vars, "AGENT_HOME");
    let an = env(vars, "AGENT_NAME");
    let mv = env(vars, "MEMORY_VAULT");
    if !ah.is_empty() {
        cfg.insert("agent_dir".into(), serde_json::Value::String(ah.clone()));
    }
    if !an.is_empty() {
        cfg.insert("name".into(), serde_json::Value::String(an));
    }
    if !mv.is_empty() {
        cfg.insert(
            "extra_dirs".into(),
            serde_json::Value::Array(vec![serde_json::Value::String(mv)]),
        );
    }
    if !ah.is_empty() {
        cfg.insert(
            "board_state_dir".into(),
            serde_json::Value::String(format!("{ah}/state")),
        );
    }
    // Fill-when-missing only — preserves local tuning.
    setdefault_str(&mut cfg, "voice", &env(vars, "VOICE_NAME"));
    setdefault_str(&mut cfg, "stt_model", &env(vars, "STT_MODEL"));
    setdefault_str(&mut cfg, "greeting", &env(vars, "GREETING"));
    (
        home.join(rel),
        serde_json::to_string_pretty(&cfg).unwrap_or_default() + "\n",
    )
}

fn parse_port(file: &serde_json::Map<String, serde_json::Value>, fallback: &str) -> u64 {
    if let Some(n) = file.get("port").and_then(|v| v.as_u64()) {
        if n >= 1 && n <= 65535 {
            return n;
        }
    }
    fallback
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|n| (1..=65535).contains(n))
        .unwrap_or(8790)
}

fn render_face(home: &Path, vars: &BTreeMap<String, String>) -> (PathBuf, String) {
    let rel = "configs/face.json";
    let mut cfg = read_base(home, rel);
    let an = env(vars, "AGENT_NAME");
    if !an.is_empty() {
        cfg.insert("name".into(), serde_json::Value::String(an));
    }
    let face = cfg
        .get("face")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&env(vars, "FACE_NAME"))
        .to_string();
    if !face.is_empty() {
        cfg.insert("face".into(), serde_json::Value::String(face));
    }
    let port = parse_port(&cfg, &env(vars, "FACE_PORT"));
    cfg.insert("port".into(), serde_json::json!(port));
    let ah = env(vars, "AGENT_HOME");
    if !ah.is_empty() {
        cfg.insert(
            "bus_dir".into(),
            serde_json::Value::String(format!("{ah}/bus")),
        );
    }
    (
        home.join(rel),
        serde_json::to_string_pretty(&cfg).unwrap_or_default() + "\n",
    )
}

fn render_board(home: &Path, vars: &BTreeMap<String, String>) -> (PathBuf, String) {
    let rel = "configs/board.json";
    let mut cfg = read_base(home, rel);
    let an = env(vars, "AGENT_NAME");
    if !an.is_empty() {
        cfg.insert("name".into(), serde_json::Value::String(an));
    }
    let port = parse_port(&cfg, &env(vars, "HANDS_PORT"));
    cfg.insert("port".into(), serde_json::json!(port));
    let mv = env(vars, "MEMORY_VAULT");
    let mut orbs: Vec<serde_json::Value> = cfg
        .get("orbs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !mv.is_empty() {
        let mut placed = false;
        for o in orbs.iter_mut() {
            if o.get("kind").and_then(|v| v.as_str()) == Some("notes") {
                o["path"] = serde_json::Value::String(mv.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            orbs.insert(
                0,
                serde_json::json!({"title": "Notes", "path": mv, "kind": "notes"}),
            );
        }
        cfg.insert("orbs".into(), serde_json::Value::Array(orbs));
    }
    (
        home.join(rel),
        serde_json::to_string_pretty(&cfg).unwrap_or_default() + "\n",
    )
}

/// Render all three configs. Returns the relative paths written.
pub fn render_all(home: &Path, vars: &BTreeMap<String, String>) -> Result<Vec<String>, String> {
    let mut written = vec![];
    for (path, text) in [
        render_voice(home, vars),
        render_face(home, vars),
        render_board(home, vars),
    ] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir configs: {e}"))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
        written.push(
            path.strip_prefix(home)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(written)
}

/// Dry-run: relative paths whose live content differs from a render.
pub fn check_all(home: &Path, vars: &BTreeMap<String, String>) -> Vec<String> {
    let mut drift = vec![];
    for (path, text) in [
        render_voice(home, vars),
        render_face(home, vars),
        render_board(home, vars),
    ] {
        let cur = std::fs::read_to_string(&path).unwrap_or_default();
        if cur != text {
            drift.push(
                path.strip_prefix(home)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    drift
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("AGENT_HOME".into(), "/home/u/butler".into()),
            ("AGENT_NAME".into(), "Butler".into()),
            ("MEMORY_VAULT".into(), "/home/u/vault".into()),
            ("FACE_PORT".into(), "8790".into()),
            ("HANDS_PORT".into(), "8794".into()),
            ("FACE_NAME".into(), "board".into()),
            ("VOICE_NAME".into(), "bm_lewis".into()),
            ("STT_MODEL".into(), "small.en".into()),
            ("GREETING".into(), "Hello, {name}.".into()),
        ])
    }

    #[test]
    fn voice_sets_managed_and_keeps_tuning() {
        let dir = std::env::temp_dir().join(format!("ob-render-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("configs"));
        // Pre-tuned voice must survive a re-render.
        std::fs::write(
            dir.join("configs/voice.json"),
            r#"{"voice": "af_heart", "speed": 1.1, "custom": true}"#,
        )
        .unwrap();
        let written = render_all(&dir, &vars()).unwrap();
        assert_eq!(written.len(), 3);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("configs/voice.json")).unwrap())
                .unwrap();
        assert_eq!(v["voice"], serde_json::json!("af_heart"));
        assert_eq!(v["speed"], serde_json::json!(1.1));
        assert_eq!(v["custom"], serde_json::json!(true));
        assert_eq!(v["agent_dir"], serde_json::json!("/home/u/butler"));
        assert_eq!(v["extra_dirs"], serde_json::json!(["/home/u/vault"]));
        assert_eq!(
            v["board_state_dir"],
            serde_json::json!("/home/u/butler/state")
        );
        assert_eq!(v["stt_model"], serde_json::json!("small.en"));
        // Face + board got bus/state homes.
        let f: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("configs/face.json")).unwrap())
                .unwrap();
        assert_eq!(f["bus_dir"], serde_json::json!("/home/u/butler/bus"));
        assert_eq!(f["port"], serde_json::json!(8790));
        let b: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("configs/board.json")).unwrap())
                .unwrap();
        assert_eq!(b["port"], serde_json::json!(8794));
        assert_eq!(b["orbs"][0]["path"], serde_json::json!("/home/u/vault"));
        // Clean right after a render.
        assert!(check_all(&dir, &vars()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drift_detected() {
        let dir = std::env::temp_dir().join(format!("ob-render2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("configs"));
        render_all(&dir, &vars()).unwrap();
        std::fs::write(dir.join("configs/face.json"), "{}").unwrap();
        let d = check_all(&dir, &vars());
        assert_eq!(d, vec!["configs/face.json".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
