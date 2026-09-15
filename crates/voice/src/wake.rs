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

const TICK_SAMPLES: usize = 1280; // 80ms @16k — openWakeWord's cadence

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
    child: Option<WakeChild>,
    next_id: u64,
    pub unavailable: bool,
}

impl WakeServe {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            bin: find_stts_bin()?,
            child: None,
            next_id: 0,
            unavailable: false,
        })
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
        let rep = self.request(serde_json::json!({
            "id": id, "cmd": "wake",
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
}
