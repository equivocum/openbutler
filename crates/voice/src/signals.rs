// Signal bus — port of backtalk/signals.py.
//
// Tiny files any other program can watch; faces read the notes:
//   .voice_state        idle | listening | thinking | speaking
//   .voice_waveform     JSON {ts, samples: [64 floats]} while audio plays
//   .voice_loading_pid  exists while the thinking sound is playing
//   .voice_direction    JSON {ts, directions: [...]} at audio start
//   .voice_reply_done   JSON {ts} when a reply fully drains
//   .voice_rate_limits  JSON {window: {utilization, resets_at}} — only
//                       written when show_usage is on (privacy default:
//                       spend renders on a face that may face a camera)
// The board seam mirrors state + waveform into board_state_dir.
// Every write is wrapped: the bus must never crash the voice line.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn write_file(path: &Path, body: &str) {
    if path.as_os_str().is_empty() {
        return;
    }
    let _ = std::fs::write(path, body);
}

pub struct Signals {
    state_file: PathBuf,
    waveform_file: PathBuf,
    loading_pid_file: PathBuf,
    direction_file: PathBuf,
    reply_done_file: PathBuf,
    #[allow(dead_code)]
    rate_limit_file: PathBuf,
    bh_state: PathBuf,
    bh_wave: PathBuf,
    thinking_sound: String,
    last_waveform_write: Mutex<f64>,
    static_proc: Mutex<Option<Child>>,
    #[allow(dead_code)]
    rate_limits: Mutex<serde_json::Map<String, serde_json::Value>>,
}

impl Signals {
    pub fn new(signals_dir: &str, board_state_dir: &str, thinking_sound: &str) -> Self {
        let dir = PathBuf::from(signals_dir);
        let bh = PathBuf::from(board_state_dir);
        let (bh_state, bh_wave) = if board_state_dir.is_empty() {
            (PathBuf::new(), PathBuf::new())
        } else {
            (bh.join("state"), bh.join("wave.json"))
        };
        Signals {
            state_file: dir.join(".voice_state"),
            waveform_file: dir.join(".voice_waveform"),
            loading_pid_file: dir.join(".voice_loading_pid"),
            direction_file: dir.join(".voice_direction"),
            reply_done_file: dir.join(".voice_reply_done"),
            rate_limit_file: dir.join(".voice_rate_limits"),
            bh_state,
            bh_wave,
            thinking_sound: thinking_sound.to_string(),
            last_waveform_write: Mutex::new(0.0),
            static_proc: Mutex::new(None),
            rate_limits: Mutex::new(serde_json::Map::new()),
        }
    }

    /// Write the state. Never raises — the show must go on.
    pub fn set_state(&self, name: &str) {
        write_file(&self.state_file, name);
        if !self.bh_state.as_os_str().is_empty() {
            write_file(&self.bh_state, name);
        }
    }

    /// Feed one PCM block (int16) — throttled to ~15Hz, downsampled to 64
    /// points. Re-asserts state="speaking": this only runs while the mouth
    /// is audibly playing, so the bus self-heals within ~70ms if a stray
    /// writer stomps the state mid-speech.
    pub fn feed_waveform(&self, pcm: &[i16]) {
        if pcm.is_empty() {
            return;
        }
        let now = now_ts();
        {
            let mut last = self.last_waveform_write.lock().unwrap();
            if now - *last < 1.0 / 15.0 {
                return;
            }
            *last = now;
        }
        // np.linspace(0, n-1, 64) indices, matching the Python bus exactly.
        let mut raw = Vec::with_capacity(64);
        for k in 0..64 {
            let idx = ((k as f64) * (pcm.len() - 1) as f64 / 63.0).round() as usize;
            raw.push(pcm[idx.min(pcm.len() - 1)] as f64);
        }
        let body = serde_json::json!({"ts": now, "samples": raw}).to_string();
        write_file(&self.waveform_file, &body);
        if !self.bh_wave.as_os_str().is_empty() {
            let norm: Vec<f64> = raw
                .iter()
                .map(|v| (v.abs() / 32768.0).clamp(0.0, 1.0))
                .collect();
            write_file(
                &self.bh_wave,
                &serde_json::json!({"ts": now, "samples": norm}).to_string(),
            );
        }
        self.set_state("speaking");
    }

    /// Stage directions the agent wrote into its reply, published at the
    /// moment the audio carrying them starts playing. Never raises.
    pub fn direction(&self, items: &[String]) {
        if items.is_empty() {
            return;
        }
        write_file(
            &self.direction_file,
            &serde_json::json!({"ts": now_ts(), "directions": items}).to_string(),
        );
    }

    /// One reply has finished speaking and its audio has fully drained.
    /// Distinct from idle, which also flickers between sentences.
    pub fn reply_done(&self) {
        write_file(
            &self.reply_done_file,
            &serde_json::json!({"ts": now_ts()}).to_string(),
        );
    }

    /// One usage window's reading — merged, never replaced (readings
    /// arrive one window at a time; a face wants both at once).
    /// NOTHING CALLS THIS UNLESS show_usage IS ON (privacy default).
    /// No caller on the OpenCode engine (no usage hook like the SDK
    /// brain's _pull_rate_limits); kept as the ported contract for the
    /// bus, whichever engine grows one. Never raises.
    #[allow(dead_code)]
    pub fn set_rate_limit(&self, window: &str, utilization: Option<f64>, resets_at: f64) {
        if window.is_empty() {
            return;
        }
        let util = match utilization {
            Some(v) => serde_json::Value::from(v),
            None => serde_json::Value::Null,
        };
        {
            let mut m = self.rate_limits.lock().unwrap();
            m.insert(
                window.to_string(),
                serde_json::json!({"utilization": util, "resets_at": resets_at}),
            );
            write_file(
                &self.rate_limit_file,
                &serde_json::Value::Object(m.clone()).to_string(),
            );
        }
    }

    /// Optional thinking sound — plays while the brain works.
    pub fn static_start(&self) {
        if self.thinking_sound.is_empty() || !Path::new(&self.thinking_sound).exists() {
            return;
        }
        self.static_stop();
        let cmd = match player_cmd(&self.thinking_sound) {
            Some(c) => c,
            None => return,
        };
        match std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                write_file(&self.loading_pid_file, &child.id().to_string());
                *self.static_proc.lock().unwrap() = Some(child);
            }
            Err(_) => {}
        }
    }

    pub fn static_stop(&self) {
        if let Some(mut child) = self.static_proc.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.loading_pid_file);
    }
}

fn player_cmd(path: &str) -> Option<Vec<String>> {
    if cfg!(target_os = "macos") {
        return Some(vec![
            "afplay".into(),
            "-v".into(),
            "0.35".into(),
            path.into(),
        ]);
    }
    for cand in ["ffplay", "aplay", "paplay"] {
        if have_on_path(cand) {
            if cand == "ffplay" {
                return Some(vec![
                    "ffplay".into(),
                    "-nodisp".into(),
                    "-autoexit".into(),
                    "-loglevel".into(),
                    "quiet".into(),
                    "-volume".into(),
                    "35".into(),
                    path.into(),
                ]);
            }
            return Some(vec![cand.into(), path.into()]);
        }
    }
    None
}

fn have_on_path(prog: &str) -> bool {
    std::env::var_os("PATH").map_or(false, |p| {
        std::env::split_paths(&p)
            .map(|d| d.join(prog))
            .any(|f| f.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_files_land_where_expected() {
        let dir = std::env::temp_dir().join(format!("openbutler-sigtest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let bh = dir.join("bh");
        let _ = std::fs::create_dir_all(&bh);
        let s = Signals::new(dir.to_str().unwrap(), bh.to_str().unwrap(), "");
        s.set_state("thinking");
        assert_eq!(
            std::fs::read_to_string(dir.join(".voice_state")).unwrap(),
            "thinking"
        );
        assert_eq!(
            std::fs::read_to_string(bh.join("state")).unwrap(),
            "thinking"
        );
        let pcm = vec![1000i16; 2205];
        // Force past the throttle.
        *s.last_waveform_write.lock().unwrap() = 0.0;
        s.feed_waveform(&pcm);
        let w: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".voice_waveform")).unwrap())
                .unwrap();
        assert_eq!(w["samples"].as_array().unwrap().len(), 64);
        let bw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(bh.join("wave.json")).unwrap()).unwrap();
        assert_eq!(bw["samples"].as_array().unwrap().len(), 64);
        s.direction(&["nod".to_string()]);
        let d: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".voice_direction")).unwrap())
                .unwrap();
        assert_eq!(d["directions"], serde_json::json!(["nod"]));
        s.reply_done();
        assert!(dir.join(".voice_reply_done").is_file());
        // No board dir: must not crash, must still write the main bus.
        let s2 = Signals::new(dir.to_str().unwrap(), "", "");
        s2.set_state("idle");
        assert_eq!(
            std::fs::read_to_string(dir.join(".voice_state")).unwrap(),
            "idle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
