// Wake-word client — line-JSON over pipes to a `openbutler-tts serve`
// child (same binary that serves TTS; the wake verbs ride the same
// protocol). Mirrors SttsServe (mouth.rs) but owns a DEDICATED child:
// mouth kills its child on synth failure, which must never wipe wake
// feature buffers. One 80ms tick = one base64 i16le frame batch.
//
// Coupling note: serve loads the Kokoro TTS engine at startup, so the
// wake child currently requires the kokoro model dir to exist (it does
// everywhere the voice runs). Decoupling serve startup from TTS load
// is deferred work, not needed for Phase 7.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::Value;

const TICK_SAMPLES: usize = 1280; // 80ms @16k — openWakeWord's cadence

/// Underscored model id -> spoken phrase ("hey_jarvis" -> "hey jarvis").
pub fn phrase_for(model: &str) -> String {
    model.replace('_', " ")
}

/// Resolve the wake config to (model id, classifier file, spoken phrase):
/// `wake.model` names a models/wake.json registry entry (default
/// "hey_jarvis"); unknown ids fall back to `<id>_v0.1.onnx` so
/// custom-trained classifiers work by filename. `wake.phrase` overrides
/// the spoken phrase for custom models.
pub fn resolve(home: &Path, cfg: &serde_json::Map<String, Value>) -> (String, String, String) {
    let wake = cfg.get("wake");
    let model = wake
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("hey_jarvis")
        .to_string();
    let pins = home.join("models/wake.json");
    let (mut file, mut phrase) = (format!("{model}_v0.1.onnx"), phrase_for(&model));
    if let Ok(t) = std::fs::read_to_string(&pins) {
        if let Ok(serde_json::Value::Object(o)) = serde_json::from_str(&t) {
            if let Some(m) = o
                .get("models")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get(&model))
            {
                if let Some(f) = m.get("file").and_then(|v| v.as_str()) {
                    file = f.to_string();
                }
                if let Some(p) = m.get("phrase").and_then(|v| v.as_str()) {
                    phrase = p.to_string();
                }
            }
        }
    }
    if let Some(p) = wake
        .and_then(|v| v.get("phrase"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        phrase = p.to_string();
    }
    (model, file, phrase)
}

fn find_stts_bin() -> Result<std::path::PathBuf, String> {
    if let Ok(b) = std::env::var("OPENBUTLER_TTS_BIN").or_else(|_| std::env::var("JARVIS_STTS_BIN"))
    {
        if !b.is_empty() {
            return Ok(std::path::PathBuf::from(b));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let c = d.join("openbutler-tts");
            if c.is_file() {
                return Ok(c);
            }
        }
    }
    Ok(std::path::PathBuf::from("openbutler-tts"))
}

struct WakeChild {
    proc: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

pub struct WakeServe {
    bin: std::path::PathBuf,
    model_file: String,
    pub phrase: String,
    child: Option<WakeChild>,
    next_id: u64,
    pub unavailable: bool,
}

impl WakeServe {
    pub fn new(home: &Path, cfg: &serde_json::Map<String, Value>) -> Result<Self, String> {
        let (_, file, phrase) = resolve(home, cfg);
        Self::from_parts(file, phrase)
    }

    pub fn from_parts(model_file: String, phrase: String) -> Result<Self, String> {
        Ok(Self {
            bin: find_stts_bin()?,
            model_file,
            phrase,
            child: None,
            next_id: 0,
            unavailable: false,
        })
    }

    /// Switch models live: drops the serve child so the next tick loads
    /// the new classifier (buffers restart — safer than carryover).
    pub fn set_model(&mut self, home: &Path, cfg: &serde_json::Map<String, Value>) {
        let (_, file, phrase) = resolve(home, cfg);
        if file != self.model_file {
            self.model_file = file;
            self.phrase = phrase;
            self.kill();
        }
    }

    fn ensure(&mut self) -> Result<(), String> {
        if self.child.is_some() {
            return Ok(());
        }
        let mut proc = std::process::Command::new(&self.bin)
            .arg("serve")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(super::mouth::serve_stderr())
            .spawn()
            .map_err(|e| {
                format!(
                    "spawn openbutler-tts serve (wake): {e} (binary: {})",
                    self.bin.display()
                )
            })?;
        let stdin = std::io::BufWriter::new(proc.stdin.take().ok_or("wake stts stdin")?);
        let stdout = std::io::BufReader::new(proc.stdout.take().ok_or("wake stts stdout")?);
        self.child = Some(WakeChild {
            proc,
            stdin,
            stdout,
        });
        Ok(())
    }

    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.proc.kill();
            let _ = c.proc.wait();
        }
    }

    fn request(&mut self, req: serde_json::Value) -> Result<serde_json::Value, String> {
        self.ensure()?;
        let c = self.child.as_mut().unwrap();
        writeln!(c.stdin, "{req}").map_err(|e| format!("wake write: {e}"))?;
        c.stdin.flush().map_err(|e| format!("wake flush: {e}"))?;
        let mut line = String::new();
        c.stdout
            .read_line(&mut line)
            .map_err(|e| format!("wake read: {e}"))?;
        if line.trim().is_empty() {
            self.kill();
            return Err("wake serve died (EOF)".into());
        }
        serde_json::from_str(&line).map_err(|e| format!("wake bad reply: {e}"))
    }

    /// Score one 80ms batch of 16kHz mono i16 samples. Returns
    /// (score 0..1, server-side buffered samples).
    pub fn tick(&mut self, samples: &[i16]) -> Result<(f32, usize), String> {
        use base64::Engine as _;
        if samples.len() != TICK_SAMPLES {
            return Err(format!(
                "wake tick needs {TICK_SAMPLES} samples, got {}",
                samples.len()
            ));
        }
        self.next_id += 1;
        let id = self.next_id;
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let file = self.model_file.clone();
        let rep = self.request(serde_json::json!({
            "id": id, "cmd": "wake", "model_file": file,
            "pcm_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }))?;
        if rep.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            let score = rep.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            let buffered = rep.get("buffered").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            Ok((score, buffered))
        } else {
            Err(rep
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("wake failed")
                .to_string())
        }
    }

    pub fn reset(&mut self) {
        let _ = self.request(serde_json::json!({"cmd": "wake_reset"}));
    }

    pub fn shutdown(&mut self) {
        self.kill();
    }
}

/// One tick's worth of mic audio: exactly 1280 samples @16k.
pub const WAKE_TICK_SAMPLES: usize = TICK_SAMPLES;

pub fn encode_tick(samples: &[i16]) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_encode_roundtrip() {
        let s: Vec<i16> = (0..WAKE_TICK_SAMPLES as i16).collect();
        let b64 = encode_tick(&s);
        // 1280 i16 samples = 2560 bytes -> base64 ceil(2560/3)*4 = 3416.
        assert_eq!(b64.len(), 3416);
        assert!(b64.starts_with("AAAB"));
    }

    #[test]
    fn phrase_derives_from_model_id() {
        assert_eq!(phrase_for("hey_jarvis"), "hey jarvis");
        assert_eq!(phrase_for("alexa"), "alexa");
    }

    #[test]
    fn resolve_defaults_without_registry() {
        let home = std::env::temp_dir().join(format!("ob-wakeres-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let cfg = serde_json::Map::new();
        let (model, file, phrase) = resolve(&home, &cfg);
        assert_eq!(model, "hey_jarvis");
        assert_eq!(file, "hey_jarvis_v0.1.onnx");
        assert_eq!(phrase, "hey jarvis");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_uses_registry_and_phrase_override() {
        let home = std::env::temp_dir().join(format!("ob-wakeres2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(home.join("models"));
        std::fs::write(
            home.join("models/wake.json"),
            r#"{"models": {"alexa": {"file": "alexa_v0.1.onnx", "phrase": "alexa"}}}"#,
        )
        .unwrap();
        let mut w = serde_json::Map::new();
        w.insert("wake".into(), serde_json::json!({"model": "alexa"}));
        let (model, file, phrase) = resolve(&home, &w);
        assert_eq!(
            (model.as_str(), file.as_str(), phrase.as_str()),
            ("alexa", "alexa_v0.1.onnx", "alexa")
        );
        w.insert(
            "wake".into(),
            serde_json::json!({"model": "custom_x", "phrase": "hey custom"}),
        );
        let (_, file, phrase) = resolve(&home, &w);
        assert_eq!(file, "custom_x_v0.1.onnx");
        assert_eq!(phrase, "hey custom");
        let _ = std::fs::remove_dir_all(&home);
    }
}
