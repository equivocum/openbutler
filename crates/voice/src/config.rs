// Voice configuration: configs/voice.json (VOICE_CONFIG override) merged
// over DEFAULTS, then the same env overlay, expansion, and derived fields
// (quit_phrases, greeting).

use openbutler_common as C;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const DISCIPLINE_BASE: &str = "VOICE SESSION (your reply is spoken aloud through a TTS engine, not displayed): you are SPEAKING, in your own voice and personality — your AGENTS.md is who you are. The TTS engine PERFORMS your punctuation, so write like a performance, never like a memo: contractions always, punchy conversational sentences, and if a line could open a quarterly report, rewrite it like you're telling a friend. Keep replies to a few short sentences; go longer only when the question genuinely needs it. No markdown, no lists, no code blocks, no emoji, no URLs. Say numbers the way a human says them out loud — never raw figures or symbols. NEVER SPEAK A FILE PATH: say the file, not its address. 'the config' or 'ears dot rs', never a string of slashes and folder names read one by one — it is unbearable aloud and carries no meaning by ear. Same for URLs and long ids: name the thing, not the address. Skip any startup sequence; answer directly. VOICE CONSOLE FACTS, answer from these whenever the person asks you to change a voice-line setting: this session is controlled by exact spoken phrases, never by you. Permissions: 'stop asking for permission' (then 'confirm'), or 'start asking again'. Microphone: 'go hands free', 'wake word mode' (only listens after the wake word), or 'push to talk mode'. Also: 'clear the session', 'compact the session', 'switch to the deep model', 'back to the fast model', 'set effort to low' (or medium, high, max), and 'usage report'. You cannot flip these live yourself, so when asked, give the person the exact phrase to SAY. Editing voice.json only changes the default for the NEXT launch.";

const ELEVENLABS_MASTER: &str = "atempo=1.12,highpass=f=70,equalizer=f=3200:t=q:w=1.2:g=3.5,equalizer=f=140:t=q:w=1:g=1.5,acompressor=threshold=-18dB:ratio=2.5:attack=8:release=120:makeup=4dB,alimiter=limit=0.95";

fn elevenlabs_default() -> Value {
    let mut m = Map::new();
    m.insert("enabled".into(), Value::Bool(false));
    m.insert("voice_id".into(), Value::String(String::new()));
    m.insert("voice_note".into(), Value::String(String::new()));
    m.insert("model".into(), Value::String("eleven_turbo_v2_5".into()));
    m.insert(
        "key_slot".into(),
        Value::String("openbutler-elevenlabs".into()),
    );
    m.insert("master".into(), Value::String(ELEVENLABS_MASTER.into()));
    Value::Object(m)
}

fn defaults() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("agent_dir".into(), Value::String("~".into()));
    m.insert(
        "name".into(),
        Value::String(C::settings::DEFAULT_NAME.into()),
    );
    m.insert("model".into(), Value::String("claude-sonnet-5".into()));
    m.insert("deep_model".into(), Value::String("claude-opus-5".into()));
    m.insert("permission_mode".into(), Value::String("ask".into()));
    m.insert("visible_skills".into(), Value::Null);
    m.insert("extra_dirs".into(), Value::Array(vec![]));
    m.insert("ptt_key".into(), Value::String("home".into()));
    m.insert(
        "mic_mode".into(),
        Value::String(C::settings::DEFAULT_MIC_MODE.into()),
    );
    m.insert(
        "speed".into(),
        serde_json::json!(C::settings::DEFAULT_SPEED),
    );
    m.insert("resume_last_session".into(), Value::Bool(false));
    m.insert("show_usage".into(), Value::Bool(false));
    m.insert(
        "effort".into(),
        Value::String(C::settings::DEFAULT_EFFORT.into()),
    );
    m.insert(
        "voice".into(),
        Value::String(C::settings::DEFAULT_VOICE_NAME.into()),
    );
    m.insert(
        "stt_model".into(),
        Value::String(C::settings::DEFAULT_STT_MODEL.into()),
    );
    m.insert("stt_device".into(), Value::String("auto".into()));
    m.insert("stt_compute".into(), Value::String("int8".into()));
    m.insert("filter_hallucinations".into(), Value::Bool(true));
    m.insert("mic_device".into(), Value::String(String::new()));
    m.insert("elevenlabs".into(), elevenlabs_default());
    m.insert("signals_dir".into(), Value::String(String::new()));
    m.insert("board_state_dir".into(), Value::String(String::new()));
    m.insert(
        "thinking_sound".into(),
        Value::String("ui/face/assets/thinking.wav".into()),
    );
    m.insert(
        "greeting".into(),
        Value::String(C::settings::DEFAULT_GREETING.into()),
    );
    m.insert("greeting_open_mic".into(), Value::String(String::new()));
    m.insert(
        "signoff".into(),
        Value::String("Voice line closing. I'll be here when you need me.".into()),
    );
    m.insert("discipline_append".into(), Value::String(String::new()));
    // Phase 7 (Rust-only superset — Python has no wake keys): openWakeWord
    // "hey jarvis" gating. Appended last so the Python-shared key order
    // above is untouched.
    let mut wake = Map::new();
    wake.insert(
        "model".into(),
        Value::String(C::settings::DEFAULT_WAKE_MODEL.into()),
    );
    wake.insert(
        "threshold".into(),
        serde_json::json!(C::settings::DEFAULT_WAKE_THRESHOLD),
    );
    wake.insert(
        "patience".into(),
        serde_json::json!(C::settings::DEFAULT_WAKE_PATIENCE),
    );
    wake.insert(
        "attention_s".into(),
        serde_json::json!(C::settings::DEFAULT_WAKE_ATTENTION_S),
    );
    m.insert("wake".into(), Value::Object(wake));
    m
}

fn as_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(|x| x.as_str())
}

/// Deep-copy-ish merge: dict+dict updates one level, else replaces.
/// Mirrors config.load().
fn merge_user(mut cfg: Map<String, Value>, user: Map<String, Value>) -> Map<String, Value> {
    for (k, v) in user {
        match (cfg.get(&k), &v) {
            (Some(Value::Object(_)), Value::Object(u)) => {
                if let Some(Value::Object(c)) = cfg.get_mut(&k) {
                    for (sk, sv) in u {
                        c.insert(sk.clone(), sv.clone());
                    }
                }
            }
            _ => {
                cfg.insert(k, v);
            }
        }
    }
    cfg
}

fn get_str<'a>(cfg: &'a Map<String, Value>, key: &str) -> &'a str {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Load the merged config for the agent home dir.
pub fn load(home: &Path) -> Map<String, Value> {
    let mut cfg = defaults();
    let config_path =
        match std::env::var("VOICE_CONFIG").or_else(|_| std::env::var("BACKTALK_CONFIG")) {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => home.join("configs/voice.json"),
        };
    match std::fs::read_to_string(&config_path) {
        Ok(t) => match serde_json::from_str::<Value>(&t) {
            Ok(Value::Object(user)) => cfg = merge_user(cfg, user),
            Ok(_) => eprintln!("[config] voice.json is not valid JSON — using defaults"),
            Err(e) => eprintln!("[config] voice.json is not valid JSON ({e}) — using defaults"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "[config] cannot read {} ({e}) — using defaults",
            config_path.display()
        ),
    }
    // Env overlay (JSON > env > defaults), including the
    // placeholder-only replacement quirks.
    if let Ok(v) = std::env::var("AGENT_HOME").or_else(|_| std::env::var("AGENT_DIR")) {
        if !v.is_empty() && ["~", ""].contains(&get_str(&cfg, "agent_dir")) {
            cfg.insert("agent_dir".into(), Value::String(v));
        }
    }
    if let Ok(v) = std::env::var("AGENT_NAME") {
        if !v.is_empty() && ["", C::settings::DEFAULT_NAME].contains(&get_str(&cfg, "name")) {
            cfg.insert("name".into(), Value::String(v));
        }
    }
    if let Ok(v) = std::env::var("MEMORY_VAULT") {
        let empty_extra = cfg
            .get("extra_dirs")
            .and_then(|x| x.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        if !v.is_empty() && empty_extra {
            cfg.insert("extra_dirs".into(), Value::Array(vec![Value::String(v)]));
        }
    }
    if let Ok(v) = std::env::var("AGENT_HOME") {
        // `board_state_dir` is the current key; `barehands_state_dir` is
        // accepted as a legacy alias from the pre-OSS layout.
        let legacy = get_str(&cfg, "barehands_state_dir");
        if !legacy.is_empty() && get_str(&cfg, "board_state_dir").is_empty() {
            cfg.insert("board_state_dir".into(), Value::String(legacy.to_string()));
        }
        if !v.is_empty() && get_str(&cfg, "board_state_dir").is_empty() {
            cfg.insert(
                "board_state_dir".into(),
                Value::String(format!("{v}/state")),
            );
        }
    }
    if let Ok(v) = std::env::var("VOICE_NAME") {
        if !v.is_empty() && ["", C::settings::DEFAULT_VOICE_NAME].contains(&get_str(&cfg, "voice"))
        {
            cfg.insert("voice".into(), Value::String(v));
        }
    }
    if let Ok(v) = std::env::var("STT_MODEL") {
        if !v.is_empty()
            && ["", C::settings::DEFAULT_STT_MODEL].contains(&get_str(&cfg, "stt_model"))
        {
            cfg.insert("stt_model".into(), Value::String(v));
        }
    }
    // Expansion + derived fields.
    let agent_dir = C::expand(get_str(&cfg, "agent_dir"));
    cfg.insert("agent_dir".into(), Value::String(agent_dir));
    let extra: Vec<Value> = cfg
        .get("extra_dirs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|d| Value::String(C::expand(d.as_str().unwrap_or(""))))
                .collect()
        })
        .unwrap_or_default();
    cfg.insert("extra_dirs".into(), Value::Array(extra));
    let signals = C::expand(get_str(&cfg, "signals_dir"));
    cfg.insert(
        "signals_dir".into(),
        Value::String(if signals.is_empty() {
            home.join("bus").to_string_lossy().into_owned()
        } else {
            signals
        }),
    );
    let hands = C::expand(get_str(&cfg, "board_state_dir"));
    cfg.insert("board_state_dir".into(), Value::String(hands));
    let thinking = C::expand(get_str(&cfg, "thinking_sound"));
    cfg.insert(
        "thinking_sound".into(),
        Value::String(if thinking.is_empty() {
            String::new()
        } else if Path::new(&thinking).is_absolute() {
            thinking
        } else {
            home.join(thinking).to_string_lossy().into_owned()
        }),
    );
    let name = if get_str(&cfg, "name").is_empty() {
        "Assistant".to_string()
    } else {
        get_str(&cfg, "name").to_string()
    };
    let low = name.to_lowercase();
    cfg.insert(
        "quit_phrases".into(),
        Value::Array(vec![
            Value::String(format!("goodbye {low}")),
            Value::String(format!("good bye {low}")),
            Value::String("end voice mode".into()),
            Value::String(format!("hang up {low}")),
            Value::String("hang up".into()),
        ]),
    );
    let key_label = format!("the {} key", get_str(&cfg, "ptt_key").replace('_', " "));
    let mut greeting = get_str(&cfg, "greeting").to_string();
    if get_str(&cfg, "mic_mode") == "open" && !get_str(&cfg, "greeting_open_mic").is_empty() {
        greeting = get_str(&cfg, "greeting_open_mic").to_string();
    }
    greeting = greeting
        .replace("{name}", &name)
        .replace("{ptt_key}", &key_label);
    cfg.insert("greeting".into(), Value::String(greeting));
    let signoff = get_str(&cfg, "signoff").replace("{name}", &name);
    cfg.insert("signoff".into(), Value::String(signoff));
    cfg
}

/// The effective spoken-delivery discipline (base + discipline_append).
pub fn discipline(cfg: &Map<String, Value>) -> String {
    let mut d = DISCIPLINE_BASE.to_string();
    if let Some(a) = as_str(cfg.get("discipline_append")) {
        let a = a.trim();
        if !a.is_empty() {
            d.push(' ');
            d.push_str(a);
        }
    }
    d
}
