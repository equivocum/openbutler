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

/// One registry entry: (id, classifier file, spoken phrase, pin).
pub struct WakeModel {
    pub id: String,
    pub file: String,
    pub phrase: String,
    pub sha256: String,
    pub bytes: u64,
}

/// Read the models/wake.json classifier registry (id -> file + phrase).
pub fn wake_registry(home: &Path) -> Vec<WakeModel> {
    let mut out = vec![];
    let Ok(t) = std::fs::read_to_string(home.join("models/wake.json")) else {
        return out;
    };
    let Ok(serde_json::Value::Object(o)) = serde_json::from_str(&t) else {
        return out;
    };
    if let Some(models) = o.get("models").and_then(|v| v.as_object()) {
        for (id, spec) in models {
            out.push(WakeModel {
                id: id.clone(),
                file: spec
                    .get("file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                phrase: spec
                    .get("phrase")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                sha256: spec
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                bytes: spec.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Match an agent name to a classifier: "Alexa" -> alexa, "Mycroft" ->
/// hey_mycroft ("hey <name>" equals the phrase). None when no stock
/// model says the name — that needs a custom-trained classifier.
pub fn derive_wake_model(home: &Path, agent_name: &str) -> Option<WakeModel> {
    let low = agent_name.trim().to_lowercase();
    if low.is_empty() {
        return None;
    }
    wake_registry(home)
        .into_iter()
        .find(|m| m.phrase == low || m.phrase == format!("hey {low}") || m.id == low)
}

/// Set configs/voice.json wake.model when the file doesn't pin one yet.
/// Never overrides existing tuning. Returns the effective (id, phrase).
pub fn ensure_wake_model(home: &Path, id: &str) -> (String, String) {
    let path = home.join("configs/voice.json");
    let mut data: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let cur = data
        .get("wake")
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if cur.is_empty() {
        let mut wake = data
            .get("wake")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        wake.insert("model".into(), serde_json::Value::String(id.into()));
        data.insert("wake".into(), serde_json::Value::Object(wake));
        if let Ok(t) = serde_json::to_string_pretty(&data) {
            let _ = std::fs::write(&path, t + "\n");
        }
        // Re-read for the return (phrase override may also live there).
        return effective_wake_model(home);
    }
    effective_wake_model(home)
}

/// Effective (id, phrase) after merges: file wins, else registry default.
pub fn effective_wake_model(home: &Path) -> (String, String) {
    let data: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(home.join("configs/voice.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    let id = data
        .get("wake")
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("hey_jarvis")
        .to_string();
    if let Some(p) = data
        .get("wake")
        .and_then(|v| v.get("phrase"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return (id, p.to_string());
    }
    if let Some(m) = wake_registry(home).into_iter().find(|m| m.id == id) {
        if !m.phrase.is_empty() {
            return (id, m.phrase);
        }
    }
    (id.clone(), id.replace('_', " "))
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
