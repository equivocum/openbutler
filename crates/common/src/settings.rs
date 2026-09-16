/* Shared settings core: ONE registry, ONE validator, ONE router for every
config CLI (`openbutler config`, `voice config`) so the two can never
disagree. Covers voice.json (+ nested wake.*), face.json (face.*),
board.json (board.*). `.env` sync + re-render live one layer up. */

use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum SettingKind {
    RangedFloat(f32, f32),
    Port,
    MinInt(u32),
    PositiveFloat,
    OneOf(&'static [&'static str]),
    WakeModel,
    Text,
}

pub struct Setting {
    pub key: &'static str,
    pub prompt: &'static str,
    pub kind: SettingKind,
}

pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "speed",
        prompt: "Speech speed (0.5-2.0, 1.0 is normal)",
        kind: SettingKind::RangedFloat(0.5, 2.0),
    },
    Setting {
        key: "voice",
        prompt: "Voice name (e.g. bm_lewis; checked against the engine when switching)",
        kind: SettingKind::Text,
    },
    Setting {
        key: "effort",
        prompt: "Effort (low/medium/high/max, empty = unset)",
        kind: SettingKind::OneOf(&["", "low", "medium", "high", "max"]),
    },
    Setting {
        key: "mic_mode",
        prompt: "Mic mode (ptt/open/wake)",
        kind: SettingKind::OneOf(&["ptt", "open", "wake"]),
    },
    Setting {
        key: "stt_model",
        prompt: "STT model (e.g. small.en, medium.en)",
        kind: SettingKind::Text,
    },
    Setting {
        key: "face.name",
        prompt: "Face agent name chip",
        kind: SettingKind::Text,
    },
    Setting {
        key: "face.face",
        prompt: "Face id (board/neural/radial/rain)",
        kind: SettingKind::Text,
    },
    Setting {
        key: "face.port",
        prompt: "Face port (127.0.0.1)",
        kind: SettingKind::Port,
    },
    Setting {
        key: "board.name",
        prompt: "Board agent name",
        kind: SettingKind::Text,
    },
    Setting {
        key: "board.port",
        prompt: "Board port (127.0.0.1)",
        kind: SettingKind::Port,
    },
    Setting {
        key: "wake.model",
        prompt: "Wake-word model id (hey_jarvis/alexa/hey_mycroft/hey_rhasspy/timer/weather; see models/wake.json)",
        kind: SettingKind::WakeModel,
    },
    Setting {
        key: "wake.threshold",
        prompt: "Wake-word threshold 0-1 (lower = jumpier)",
        kind: SettingKind::RangedFloat(0.0, 1.0),
    },
    Setting {
        key: "wake.patience",
        prompt: "Wake-word patience, frames in a row (>=1)",
        kind: SettingKind::MinInt(1),
    },
    Setting {
        key: "wake.attention_s",
        prompt: "Wake attention window, seconds (>0)",
        kind: SettingKind::PositiveFloat,
    },
];

pub fn find_setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

pub fn parse_setting(key: &str, raw: &str) -> Result<serde_json::Value, String> {
    let s = find_setting(key)
        .ok_or_else(|| format!("unknown setting {key:?} (see `config set` for the list)"))?;
    let raw = raw.trim();
    match s.kind {
        SettingKind::RangedFloat(lo, hi) => {
            let v: f64 = raw
                .parse()
                .map_err(|_| format!("{key} wants a number {lo}-{hi}"))?;
            if !(v >= lo as f64 && v <= hi as f64) {
                return Err(format!("{key} out of range ({lo}-{hi})"));
            }
            Ok(serde_json::json!(v))
        }
        SettingKind::Port => {
            let v: u32 = raw
                .parse()
                .map_err(|_| format!("{key} wants a port 1-65535"))?;
            if !(1..=65535).contains(&v) {
                return Err(format!("{key} wants a port 1-65535"));
            }
            Ok(serde_json::json!(v))
        }
        SettingKind::MinInt(lo) => {
            let v: u64 = raw
                .parse()
                .map_err(|_| format!("{key} wants a whole number >= {lo}"))?;
            if v < lo as u64 {
                return Err(format!("{key} wants a whole number >= {lo}"));
            }
            Ok(serde_json::json!(v))
        }
        SettingKind::PositiveFloat => {
            let v: f64 = raw
                .parse()
                .map_err(|_| format!("{key} wants a number > 0"))?;
            if v <= 0.0 {
                return Err(format!("{key} wants a number > 0"));
            }
            Ok(serde_json::json!(v))
        }
        SettingKind::OneOf(opts) => {
            if opts.contains(&raw) {
                Ok(serde_json::Value::String(raw.to_string()))
            } else {
                Err(format!("{key} wants one of: {}", opts.join("/")))
            }
        }
        SettingKind::Text => {
            if raw.is_empty() {
                Err(format!("{key} can't be empty"))
            } else {
                Ok(serde_json::Value::String(raw.to_string()))
            }
        }
        SettingKind::WakeModel => {
            let ok = !raw.is_empty()
                && raw
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if ok {
                Ok(serde_json::Value::String(raw.to_string()))
            } else {
                Err(format!(
                    "{key} wants a model id like hey_jarvis (lowercase, digits, underscores; see models/wake.json)"
                ))
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SettingFile {
    Voice,
    Face,
    Board,
}

pub fn setting_file(key: &str) -> SettingFile {
    if key.starts_with("face.") {
        SettingFile::Face
    } else if key.starts_with("board.") {
        SettingFile::Board
    } else {
        SettingFile::Voice
    }
}

fn setting_leaf(key: &str) -> &str {
    match setting_file(key) {
        SettingFile::Face => key.strip_prefix("face.").unwrap_or(key),
        SettingFile::Board => key.strip_prefix("board.").unwrap_or(key),
        SettingFile::Voice => key,
    }
}

fn voice_config_path(home: &Path) -> PathBuf {
    match std::env::var("VOICE_CONFIG").or_else(|_| std::env::var("BACKTALK_CONFIG")) {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => home.join("configs/voice.json"),
    }
}

pub fn setting_path(home: &Path, key: &str) -> PathBuf {
    match setting_file(key) {
        SettingFile::Face => home.join("configs/face.json"),
        SettingFile::Board => home.join("configs/board.json"),
        SettingFile::Voice => voice_config_path(home),
    }
}

/* Strict file mutation shared by every writer: a missing file starts empty,
but an unparsable or unwritable file is an Err — never silently rebuilt,
since that would wipe every other setting. */
pub fn modify_file(
    path: &Path,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<(), String> {
    let mut data = match std::fs::read_to_string(path) {
        Ok(t) => match serde_json::from_str::<serde_json::Value>(&t) {
            Ok(serde_json::Value::Object(o)) => o,
            _ => {
                return Err(format!(
                    "{} is not valid JSON — fix or delete it first, no changes made",
                    path.display()
                ));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            return Err(format!(
                "cannot read {} ({e}) — no changes made",
                path.display()
            ));
        }
    };
    mutate(&mut data);
    std::fs::write(
        path,
        serde_json::to_string_pretty(&data).unwrap_or_default() + "\n",
    )
    .map_err(|e| format!("write {}: {e} — no changes made", path.display()))
}

/* File-only write of one dotted key: voice honors one nesting level
(`wake.threshold`), face/board strip their prefix (`face.port` → `port`).
Voice sessions use write_setting below, which additionally updates the
in-memory map; the CLI calls this directly. */
pub fn write_dotted_key(home: &Path, dotted: &str, value: serde_json::Value) -> Result<(), String> {
    let path = setting_path(home, dotted);
    let (head, tail): (String, Option<String>) = match setting_file(dotted) {
        SettingFile::Voice => {
            let mut parts = dotted.splitn(2, '.');
            let head = parts.next().unwrap_or("");
            match parts.next() {
                None if !head.is_empty() => (head.to_string(), None),
                Some(tail) if !head.is_empty() && !tail.is_empty() && !tail.contains('.') => {
                    (head.to_string(), Some(tail.to_string()))
                }
                _ => return Err(format!("unsupported key {dotted:?}")),
            }
        }
        _ => {
            let leaf = setting_leaf(dotted);
            if leaf.is_empty() {
                return Err(format!("unsupported key {dotted:?}"));
            }
            (leaf.to_string(), None)
        }
    };
    modify_file(&path, |data| match &tail {
        None => {
            data.insert(head.clone(), value.clone());
        }
        Some(t) => {
            let sec = data
                .entry(head.clone())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(m) = sec {
                m.insert(t.clone(), value.clone());
            }
        }
    })
}

fn session_write(result: Result<(), String>) -> bool {
    match result {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[settings] {e} (session-only)");
            false
        }
    }
}

pub fn write_config_key(
    cfg: &mut serde_json::Map<String, serde_json::Value>,
    home: &Path,
    key: &str,
    value: serde_json::Value,
) -> bool {
    cfg.insert(key.to_string(), value.clone());
    session_write(write_dotted_key(home, key, value))
}

pub fn write_setting(
    home: &Path,
    cfg: &mut serde_json::Map<String, serde_json::Value>,
    dotted: &str,
    value: serde_json::Value,
) -> bool {
    if matches!(setting_file(dotted), SettingFile::Voice) {
        let mut parts = dotted.splitn(2, '.');
        let head = parts.next().unwrap_or("");
        match parts.next() {
            None => {
                cfg.insert(head.to_string(), value.clone());
            }
            Some(tail) if !tail.is_empty() && !tail.contains('.') => {
                if head.is_empty() {
                    return false;
                }
                if let Some(serde_json::Value::Object(sec)) = cfg.get_mut(head) {
                    sec.insert(tail.to_string(), value.clone());
                } else {
                    let mut sec = serde_json::Map::new();
                    sec.insert(tail.to_string(), value.clone());
                    cfg.insert(head.to_string(), serde_json::Value::Object(sec));
                }
            }
            _ => return false,
        }
    }
    session_write(write_dotted_key(home, dotted, value))
}

pub fn get_config_path<'a>(
    cfg: &'a serde_json::Map<String, serde_json::Value>,
    dotted: &str,
) -> Option<&'a serde_json::Value> {
    let mut parts = dotted.splitn(2, '.');
    let head = parts.next().unwrap_or("");
    match parts.next() {
        None => cfg.get(head),
        Some(tail) => cfg
            .get(head)
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(tail)),
    }
}

pub fn get_setting(
    home: &Path,
    voice_cfg: &serde_json::Map<String, serde_json::Value>,
    dotted: &str,
) -> Option<serde_json::Value> {
    match setting_file(dotted) {
        SettingFile::Voice => get_config_path(voice_cfg, dotted).cloned(),
        SettingFile::Face => {
            let file = crate::read_json_object(&home.join("configs/face.json"));
            let leaf = setting_leaf(dotted);
            let (env_key, default) = match leaf {
                "name" => ("AGENT_NAME", serde_json::json!("Assistant")),
                "face" => ("FACE_NAME", serde_json::json!("board")),
                "port" => ("FACE_PORT", serde_json::json!(8790)),
                _ => ("", serde_json::Value::Null),
            };
            overlay_env(file.get(leaf).cloned(), env_key, leaf == "port", default)
        }
        SettingFile::Board => {
            let file = crate::read_json_object(&home.join("configs/board.json"));
            let leaf = setting_leaf(dotted);
            let (env_key, default) = match leaf {
                "name" => ("AGENT_NAME", serde_json::json!("Assistant")),
                "port" => ("HANDS_PORT", serde_json::json!(8794)),
                _ => ("", serde_json::Value::Null),
            };
            overlay_env(file.get(leaf).cloned(), env_key, leaf == "port", default)
        }
    }
}

/* Display defaults for `config get` when neither file nor `.env` seed sets a
value. The runtime merges are authoritative — voice `config.rs`, face and
board server defaults — so keep this table in sync with those on change;
a light crate cannot import the heavy servers to derive them. */
pub fn default_value(key: &str) -> serde_json::Value {
    match key {
        "speed" => serde_json::json!(1.0),
        "voice" => serde_json::json!("bm_lewis"),
        "effort" => serde_json::json!(""),
        "mic_mode" => serde_json::json!("open"),
        "stt_model" => serde_json::json!("small.en"),
        "face.name" | "board.name" => serde_json::json!("Assistant"),
        "face.face" => serde_json::json!("board"),
        "face.port" => serde_json::json!(8790),
        "board.port" => serde_json::json!(8794),
        "wake.model" => serde_json::json!("hey_jarvis"),
        "wake.threshold" => serde_json::json!(0.5),
        "wake.patience" => serde_json::json!(2),
        "wake.attention_s" => serde_json::json!(8.0),
        _ => serde_json::Value::Null,
    }
}

pub fn overlay_env(
    file: Option<serde_json::Value>,
    env_key: &str,
    numeric: bool,
    default: serde_json::Value,
) -> Option<serde_json::Value> {
    if let Some(v) = file {
        return Some(v);
    }
    if !env_key.is_empty() {
        if let Ok(raw) = std::env::var(env_key) {
            if !raw.is_empty() {
                if numeric {
                    if let Ok(n) = raw.trim().parse::<u64>() {
                        return Some(serde_json::json!(n));
                    }
                } else {
                    return Some(serde_json::Value::String(raw));
                }
            }
        }
    }
    Some(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_servers() {
        assert_eq!(default_value("speed"), serde_json::json!(1.0));
        assert_eq!(default_value("stt_model"), serde_json::json!("small.en"));
        assert_eq!(default_value("face.port"), serde_json::json!(8790));
        assert_eq!(default_value("wake.patience"), serde_json::json!(2));
        assert_eq!(default_value("bogus"), serde_json::Value::Null);
    }

    #[test]
    fn dotted_write_routes_and_refuses() {
        let dir = std::env::temp_dir().join(format!("ob-wd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("configs")).unwrap();
        write_dotted_key(&dir, "speed", serde_json::json!(1.2)).unwrap();
        write_dotted_key(&dir, "wake.threshold", serde_json::json!(0.7)).unwrap();
        write_dotted_key(&dir, "face.port", serde_json::json!(8811)).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("configs/voice.json")).unwrap())
                .unwrap();
        assert_eq!(v["speed"], serde_json::json!(1.2));
        assert_eq!(v["wake"]["threshold"], serde_json::json!(0.7));
        let f: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("configs/face.json")).unwrap())
                .unwrap();
        assert_eq!(f["port"], serde_json::json!(8811));
        assert!(write_dotted_key(&dir, "a.b.c", serde_json::json!(1)).is_err());
        std::fs::write(dir.join("configs/voice.json"), "{oops").unwrap();
        assert!(write_dotted_key(&dir, "speed", serde_json::json!(1.0)).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("configs/voice.json")).unwrap(),
            "{oops"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setting_parse_validates() {
        assert_eq!(
            parse_setting("speed", "1.1").unwrap(),
            serde_json::json!(1.1)
        );
        assert!(parse_setting("speed", "3").is_err());
        assert!(parse_setting("speed", "fast").is_err());
        assert_eq!(
            parse_setting("wake.threshold", "0").unwrap(),
            serde_json::json!(0.0)
        );
        assert!(parse_setting("wake.threshold", "2").is_err());
        assert_eq!(
            parse_setting("wake.patience", "3").unwrap(),
            serde_json::json!(3)
        );
        assert!(parse_setting("wake.patience", "0").is_err());
        assert!(parse_setting("mic_mode", "wake").is_ok());
        assert!(parse_setting("mic_mode", "loud").is_err());
        assert!(parse_setting("effort", "").is_ok());
        assert!(parse_setting("nope", "1").is_err());
        assert!(find_setting("voice").is_some());
        assert!(SETTINGS.iter().any(|s| s.key == "wake.attention_s"));
    }

    #[test]
    fn wake_model_validates() {
        assert_eq!(
            parse_setting("wake.model", "alexa").unwrap(),
            serde_json::json!("alexa")
        );
        assert_eq!(
            parse_setting("wake.model", "hey_mycroft").unwrap(),
            serde_json::json!("hey_mycroft")
        );
        assert!(parse_setting("wake.model", "Hey Jarvis").is_err());
        assert!(parse_setting("wake.model", "").is_err());
        assert!(find_setting("wake.model").is_some());
    }

    #[test]
    fn nested_config_path_reads() {
        let mut cfg = serde_json::Map::new();
        let mut w = serde_json::Map::new();
        w.insert("threshold".into(), serde_json::json!(0.5));
        cfg.insert("wake".into(), serde_json::Value::Object(w));
        cfg.insert("speed".into(), serde_json::json!(1.0));
        assert_eq!(
            get_config_path(&cfg, "speed").unwrap(),
            &serde_json::json!(1.0)
        );
        assert_eq!(
            get_config_path(&cfg, "wake.threshold").unwrap(),
            &serde_json::json!(0.5)
        );
        assert!(get_config_path(&cfg, "wake.nope").is_none());
        assert!(get_config_path(&cfg, "nope").is_none());
    }

    #[test]
    fn port_and_routing() {
        assert_eq!(
            parse_setting("face.port", "8790").unwrap(),
            serde_json::json!(8790)
        );
        assert!(parse_setting("face.port", "0").is_err());
        assert!(parse_setting("face.port", "99999").is_err());
        assert!(parse_setting("board.port", "abc").is_err());
        assert_eq!(setting_file("face.port"), SettingFile::Face);
        assert_eq!(setting_file("face.face"), SettingFile::Face);
        assert_eq!(setting_file("board.port"), SettingFile::Board);
        assert_eq!(setting_file("speed"), SettingFile::Voice);
        assert_eq!(setting_file("wake.threshold"), SettingFile::Voice);
        assert!(find_setting("face.port").is_some());
        assert!(find_setting("board.port").is_some());
        // Pure overlay: file beats env beats default.
        assert_eq!(
            overlay_env(Some(serde_json::json!(1)), "X", false, serde_json::json!(2)),
            Some(serde_json::json!(1))
        );
        assert_eq!(
            overlay_env(
                None,
                "DEFINITELY_UNSET_OPENBUTLER_TEST_VAR",
                false,
                serde_json::json!(2)
            ),
            Some(serde_json::json!(2))
        );
    }
}
