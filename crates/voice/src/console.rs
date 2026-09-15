// Voice console — port of the CONSOLE_VERBS block in backtalk/main.py.
//
// Exact spoken phrases, spoken alone, control the session itself so the
// person never goes back to the keyboard. Matching is EXACT after
// normalization (never prefixes); quit phrases are the one exception —
// those are substring matches, as in Python.

/// verb -> accepted exact phrases.
pub const CONSOLE_VERBS: &[(&str, &[&str])] = &[
    (
        "clear",
        &[
            "clear the session",
            "clear the context",
            "clear context",
            "fresh slate",
            "slash clear",
        ],
    ),
    (
        "compact",
        &[
            "compact the session",
            "compact the context",
            "compact context",
            "slash compact",
        ],
    ),
    (
        "deep",
        &[
            "switch to the deep model",
            "use the deep model",
            "slash model deep",
        ],
    ),
    (
        "fast",
        &[
            "switch to the fast model",
            "use the fast model",
            "back to the fast model",
            "slash model fast",
        ],
    ),
    ("usage", &["usage report", "slash usage"]),
    (
        "micopen",
        &[
            "go hands free",
            "hands free mode",
            "hands free listening",
            "open mic",
            "open the mic",
        ],
    ),
    (
        "micptt",
        &[
            "push to talk",
            "push to talk mode",
            "back to push to talk",
            "back to the button",
        ],
    ),
    (
        "micwake",
        &[
            "wake word mode",
            "listen for the wake word",
            "only listen for hey jarvis",
            "slash mic wake",
        ],
    ),
    (
        "noask",
        &[
            "stop asking for permission",
            "stop asking permission",
            "stop asking me for permission",
            "turn off the permission prompt",
            "turn off the permission prompts",
            "turn off the permissions prompt",
            "turn off the permissions prompts",
            "turn off permissions",
            "turn off permission checks",
            "disable the permission checks",
            "disable permission checks",
            "auto approve",
            "auto approve mode",
        ],
    ),
    (
        "ask",
        &[
            "start asking again",
            "ask before acting",
            "ask for permission again",
        ],
    ),
];

pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Normalize speech for exact matching: lowercase, hyphens to spaces,
/// collapsed, edge punctuation stripped.
fn norm_verb(text: &str) -> String {
    text.to_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c| matches!(c, '.' | ',' | '!' | '?'))
        .to_string()
}

/// Lowercase, every non-letter to space, collapse. Used for the yes/no
/// permission vocabulary (exact matches only — prefix matching turns
/// "yesterday" into consent).
pub fn norm_speech(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|ch| if ('a'..='z').contains(&ch) { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Exact-yes vocabulary for the spoken permission gate. Unused on the
/// OpenCode engine (no mid-turn tool hook); kept as the ported contract
/// for the gate, whichever engine grows one.
#[allow(dead_code)]
pub const YES: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "yup",
    "sure",
    "approve",
    "approved",
    "go ahead",
    "do it",
    "yes please",
    "yes sir",
    "yes boss",
    "yes go ahead",
    "go for it",
    "green light",
    "okay",
    "ok",
    "y",
    "permission granted",
    "granted",
    "you have permission",
    "you may",
    "allowed",
    "allow it",
    "confirmed",
    "affirmative",
];

/// Fold a voice name for comparison: lowercase, drop spaces,
/// underscores, hyphens. "bf emma" ≡ "bf_emma" ≡ "BF-EMMA".
pub fn fold_voice(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect()
}

/// Levenshtein distance over chars (voice names are short).
pub fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Rank known voices against a heard name (already folded by the
/// caller — pass fold_voice(want)). Returns (name, distance), best
/// first. No threshold here: callers decide between confirm-ask
/// (close) and closest-3 (far), so a miss degrades to a helpful
/// question instead of a wrong switch.
pub fn rank_voices<'a>(voices: &'a [String], folded_want: &str) -> Vec<(&'a str, usize)> {
    let mut v: Vec<(&str, usize)> = voices
        .iter()
        .map(|n| (n.as_str(), lev(&fold_voice(n), folded_want)))
        .collect();
    v.sort_by_key(|(_, d)| *d);
    v
}

/// Close enough to ask "did you mean …?" — generous on purpose: a
/// confirm-ask costs one turn, a missed switch costs the soak.
pub fn voice_close_enough(folded_want: &str, dist: usize) -> bool {
    dist <= (folded_want.len() / 3).max(3)
}

/// Match one utterance against the console verbs. Returns the verb, or
/// `effort:<lvl>` for the effort phrases. `voice:<name>` for voice-switch
/// phrases ("switch voice to X" / "use voice X" / "slash voice X").
pub fn console_match(text: &str) -> Option<String> {
    let norm = norm_verb(text);
    for (verb, phrases) in CONSOLE_VERBS {
        if phrases.contains(&norm.as_str()) {
            return Some(verb.to_string());
        }
    }
    for lvl in EFFORTS {
        if norm == format!("set effort to {lvl}")
            || norm == format!("effort {lvl}")
            || norm == format!("slash effort {lvl}")
        {
            return Some(format!("effort:{lvl}"));
        }
    }
    for prefix in ["switch voice to ", "use voice ", "slash voice "] {
        if let Some(name) = norm.strip_prefix(prefix) {
            let name = name.trim().replace(' ', "");
            if !name.is_empty() {
                return Some(format!("voice:{name}"));
            }
        }
    }
    None
}

/// Substring quit-phrase test, mirroring `any(q in text.lower() ...)` —
/// "No! Don't hang up, skip it" must NOT quit (handled by callers that
/// check exactness first where needed), but any turn containing a quit
/// phrase hangs up. Both sides are letter-normalized first so STT
/// punctuation ("Goodbye, assistant.") still matches ("goodbye
/// assistant"). Python's copy keeps the raw-substring bug; it is not
/// being fixed — the Python engine is deprecated pending removal.
pub fn is_quit(text: &str, quit_phrases: &[String]) -> bool {
    let low = norm_speech(text);
    quit_phrases.iter().any(|q| {
        let nq = norm_speech(q);
        !nq.is_empty() && low.contains(nq.as_str())
    })
}

/// The agent rewrites the config; the person never hand-edits it.
/// A file that fails to PARSE is left untouched (rewriting from {}
/// would wipe every other setting); the in-memory map updates either
/// way so the session behaves. Returns True on a persisted write.
pub fn write_config_key(
    cfg: &mut serde_json::Map<String, serde_json::Value>,
    home: &std::path::Path,
    key: &str,
    value: serde_json::Value,
) -> bool {
    cfg.insert(key.to_string(), value.clone());
    let path = voice_config_path(home);
    modify_config_file(&path, |data| {
        data.insert(key.to_string(), value);
    })
}

/// Routed write for `config set`: `face.*` → face.json,
/// `board.*` → board.json, else voice.json (one nesting level,
/// same as before). Face/board keys are file-only — they are NOT
/// inserted into the voice in-memory map, which doesn't own them.
pub fn write_setting(
    home: &std::path::Path,
    cfg: &mut serde_json::Map<String, serde_json::Value>,
    dotted: &str,
    value: serde_json::Value,
) -> bool {
    let path = setting_path(home, dotted);
    match setting_file(dotted) {
        SettingFile::Voice => {
            let mut parts = dotted.splitn(2, '.');
            let head = parts.next().unwrap_or("");
            match parts.next() {
                None => write_config_key(cfg, home, head, value),
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
                    let h = head.to_string();
                    let t = tail.to_string();
                    modify_config_file(&path, |data| {
                        let sec = data
                            .entry(h.clone())
                            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                        if let serde_json::Value::Object(m) = sec {
                            m.insert(t.clone(), value.clone());
                        }
                    })
                }
                _ => false,
            }
        }
        _ => {
            let leaf = setting_leaf(dotted);
            if leaf.is_empty() {
                return false;
            }
            let l = leaf.to_string();
            modify_config_file(&path, |data| {
                data.insert(l.clone(), value.clone());
            })
        }
    }
}

/// Merged read for `config get`: voice keys come from the merged
/// voice map; face/board keys overlay file > env > server default
/// (mirroring the servers' own precedence, minus CLI flags).
pub fn get_setting(
    home: &std::path::Path,
    voice_cfg: &serde_json::Map<String, serde_json::Value>,
    dotted: &str,
) -> Option<serde_json::Value> {
    match setting_file(dotted) {
        SettingFile::Voice => get_config_path(voice_cfg, dotted).cloned(),
        SettingFile::Face => {
            let file = openbutler_common::read_json_object(&home.join("configs/face.json"));
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
            let file = openbutler_common::read_json_object(&home.join("configs/board.json"));
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

/// Pure overlay step: file value wins when present, else a parsed env
/// var, else the default. Exported for unit tests (env reads stay in
/// get_setting so tests never touch the process environment).
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

/// Which config file owns a setting: `face.*` → face.json,
/// `board.*` → board.json, everything else → voice.json.
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

/// Strip the `face.`/`board.` prefix (None for voice keys).
fn setting_leaf(key: &str) -> &str {
    match setting_file(key) {
        SettingFile::Face => key.strip_prefix("face.").unwrap_or(key),
        SettingFile::Board => key.strip_prefix("board.").unwrap_or(key),
        SettingFile::Voice => key,
    }
}

/// Resolve the target file. VOICE_CONFIG (legacy: BACKTALK_CONFIG)
/// redirects the voice file only (face/board always live beside the home).
pub fn setting_path(home: &std::path::Path, key: &str) -> std::path::PathBuf {
    match setting_file(key) {
        SettingFile::Face => home.join("configs/face.json"),
        SettingFile::Board => home.join("configs/board.json"),
        SettingFile::Voice => voice_config_path(home),
    }
}

/// Voice config location: explicit override wins, else configs/voice.json.
fn voice_config_path(home: &std::path::Path) -> std::path::PathBuf {
    match std::env::var("VOICE_CONFIG").or_else(|_| std::env::var("BACKTALK_CONFIG")) {
        Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => home.join("configs/voice.json"),
    }
}

fn modify_config_file(
    path: &std::path::Path,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> bool {
    let mut data: serde_json::Map<String, serde_json::Value> = match std::fs::read_to_string(path) {
        Ok(t) => match serde_json::from_str::<serde_json::Value>(&t) {
            Ok(serde_json::Value::Object(o)) => o,
            _ => {
                eprintln!("[console] config not writable/parsable, session-only");
                return false;
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            eprintln!("[console] config write failed, session-only: {e}");
            return false;
        }
    };
    mutate(&mut data);
    match std::fs::write(
        path,
        serde_json::to_string_pretty(&data).unwrap_or_default() + "\n",
    ) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[console] config write failed, session-only: {e}");
            false
        }
    }
}

/// Read a (possibly dotted) key from a merged config map.
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

// ---- Interactive `config set` tunables (pure; unit-tested). ----

/// How to parse + validate one settable key.
#[derive(Clone, Copy)]
pub enum SettingKind {
    /// f32 clamped to [lo, hi].
    RangedFloat(f32, f32),
    /// u16 port (1-65535; 0 would random-bind, never what you want).
    Port,
    /// u32 >= lo.
    MinInt(u32),
    /// f32 > 0.
    PositiveFloat,
    /// One of the listed strings (exact).
    OneOf(&'static [&'static str]),
    /// Any non-empty string (validated at use time, e.g. voice list).
    Text,
}

pub struct Setting {
    pub key: &'static str,
    pub prompt: &'static str,
    pub kind: SettingKind,
}

/// Every key `config set` accepts, in prompt order.
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

/// Parse + validate raw input for a key. Empty input means "keep" and
/// is handled by the caller (never passed here).
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
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        return format!("about {} million tokens", round1(n as f64 / 1_000_000.0));
    }
    if n >= 1000 {
        return format!(
            "about {} thousand tokens",
            (n as f64 / 1000.0).round() as u64
        );
    }
    format!("{n} tokens")
}

fn round1(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r == r.trunc() {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// A short CFO brief of the session, written for the ear. The OpenCode
/// engine exposes no context breakdown, so this is turns + spoken-out +
/// cost only (mirrors _spoken_usage with ctx_usage=None).
pub fn spoken_usage(turns: u64, out_tokens: u64, cost: f64) -> String {
    let mut parts = vec![
        format!(
            "{} turn{} this session",
            turns,
            if turns == 1 { "" } else { "s" }
        ),
        fmt_tokens(out_tokens) + " spoken out",
    ];
    let cents = (cost * 100.0).round() as i64;
    if cents >= 1 {
        parts.push(if cents < 100 {
            format!("roughly {cents} cents")
        } else {
            format!("roughly {} dollars", (cents as f64 / 100.0).round() as i64)
        });
    }
    parts.join(". ") + "."
}

/// Scrub terminal-copy artifacts: blockquote gutter glyphs and stray
/// whitespace (copying from a CLI chat render drags bars along).
pub fn clean_typed(line: &str) -> String {
    let mut s = line.trim().to_string();
    loop {
        let t = s.trim_start().to_string();
        if let Some(c) = t.chars().next() {
            if c == '▎' || c == '│' || c == '>' {
                s = t[c.len_utf8()..].trim_start().to_string();
                continue;
            }
        }
        s = t;
        break;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_match_exactly() {
        assert_eq!(console_match("clear the session"), Some("clear".into()));
        assert_eq!(console_match("Go hands free!"), Some("micopen".into()));
        assert_eq!(
            console_match("set effort to high"),
            Some("effort:high".into())
        );
        assert_eq!(
            console_match("slash effort xhigh"),
            Some("effort:xhigh".into())
        );
        assert_eq!(console_match("usage report"), Some("usage".into()));
        // Ordinary sentences never trigger.
        assert_eq!(console_match("please clear the session for me"), None);
        assert_eq!(console_match(""), None);
    }

    #[test]
    fn voice_verb_parses() {
        assert_eq!(
            console_match("switch voice to af_heart"),
            Some("voice:af_heart".into())
        );
        assert_eq!(
            console_match("use voice bm_george"),
            Some("voice:bm_george".into())
        );
        assert_eq!(console_match("switch voice to"), None);
    }

    #[test]
    fn speech_norm_is_exact_only() {
        assert!(YES.contains(&norm_speech("Yes, confirm").as_str()) == false);
        assert!(YES.contains(&norm_speech("yes").as_str()));
        assert!(!YES.contains(&norm_speech("yesterday").as_str()));
    }

    #[test]
    fn quit_is_substring() {
        let q = vec!["goodbye assistant".to_string(), "hang up".to_string()];
        assert!(is_quit("well, goodbye assistant, thanks", &q));
        assert!(is_quit("Goodbye, assistant.", &q));
        assert!(is_quit("GOODBYE ASSISTANT!", &q));
        assert!(!is_quit("hello there", &q));
    }

    #[test]
    fn usage_brief_reads_for_the_ear() {
        assert_eq!(
            spoken_usage(1, 40, 0.0),
            "1 turn this session. 40 tokens spoken out."
        );
        assert_eq!(
            spoken_usage(3, 2500, 0.12),
            "3 turns this session. about 3 thousand tokens spoken out. roughly 12 cents."
        );
    }

    #[test]
    fn typed_scrub_strips_gutters() {
        assert_eq!(clean_typed("▎ hello"), "hello");
        assert_eq!(clean_typed("> > deep"), "deep");
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
    fn voice_fold_and_rank() {
        assert_eq!(fold_voice("bf emma"), "bfemma");
        assert_eq!(fold_voice("bf_emma"), "bfemma");
        assert_eq!(fold_voice("BF-EMMA"), "bfemma");
        assert_eq!(lev("kitten", "sitting"), 3);
        assert_eq!(lev("", "abc"), 3);
        let vs = vec![
            "af_heart".to_string(),
            "bf_emma".to_string(),
            "bm_george".to_string(),
            "bm_lewis".to_string(),
        ];
        // Soak cases: perfect-STT and mangled hearings all rank bf_emma first.
        for heard in ["bf emma", "beanemma", "bfm"] {
            let r = rank_voices(&vs, &fold_voice(heard));
            assert_eq!(r[0].0, "bf_emma", "heard {heard:?}");
            assert!(
                voice_close_enough(&fold_voice(heard), r[0].1),
                "heard {heard:?} d={}",
                r[0].1
            );
        }
        // Perfect hit is distance zero.
        let r = rank_voices(&vs, &fold_voice("bm_lewis"));
        assert_eq!(r[0], ("bm_lewis", 0));
        // Nonsense is far from everything.
        let r = rank_voices(&vs, &fold_voice("xyzzy plugh"));
        assert!(!voice_close_enough(&fold_voice("xyzzy plugh"), r[0].1));
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
                "DEFINITELY_UNSET_JARVIS_TEST_VAR",
                false,
                serde_json::json!(2)
            ),
            Some(serde_json::json!(2))
        );
    }
}
