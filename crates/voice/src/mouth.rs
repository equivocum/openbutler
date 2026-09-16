// The mouth — port of backtalk/mouth.py: sentence-chunked TTS through
// one long-lived output stream.
//
// Default engine: Kokoro, via the openbutler-tts serve child.
// Optional premium: ElevenLabs on YOUR key (keychain/secret-tool/env,
// never a file) with Kokoro as automatic fallback: degrade, never mute.
//
// HARD-WON AUDIO LAW #1 — ONE long-lived cpal OutputStream, reused for
// every sentence for the life of the process. A fresh stream per sentence
// gives an audible onset blip or dead air on latch-happy setups.
// HARD-WON AUDIO LAW #2 — buffer ~0.75s before a sentence starts playing.
// With whole-sentence synthesis the buffer is inherently full, so the law
// holds by construction; short clips just play.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const KOKORO_RATE: u32 = 24000;
pub const EL_RATE: u32 = 44100;
const BLOCK: usize = 2205; // ~0.1s at 24k... 2205/24000 = 92ms

fn log(line: &str) {
    crate::vlog::log(line);
}

/// Split text into sentences on `(?<=[.!?])\s+`, mirroring _SENTENCE_RE.
/// Abbreviations like "U.S.", "Dr.", "e.g." are NOT split.
pub fn split_sentences(text: &str) -> Vec<String> {
    const ABBREVS: &[&str] = &[
        "U.S.", "U.S.A.", "U.K.", "E.U.", "U.N.", "Dr.", "Mr.", "Ms.", "Mrs.", "St.", "e.g.",
        "i.e.", "etc.",
    ];
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'.' | b'!' | b'?') {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > i + 1 {
                let candidate = &text[start..i + 1];
                if ABBREVS.iter().any(|a| candidate.ends_with(a)) {
                    i += 1;
                    continue;
                }
                let s = text[start..j].trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let tail = text[start..].trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// <<anything>> is a stage direction: lifted out, never spoken, published
/// on the bus when the audio carrying it starts. Bounded (1..=80 chars,
/// no angle brackets inside) so a runaway model cannot swallow a
/// paragraph into one tag.
pub fn strip_directions(raw: &str) -> (String, Vec<String>) {
    let mut found = Vec::new();
    let mut clean = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'<' && bytes[i + 1] == b'<' {
            if let Some(end) = raw[i + 2..].find(">>") {
                let body = &raw[i + 2..i + 2 + end];
                if !body.is_empty() && body.len() <= 80 && !body.contains(['<', '>']) {
                    let b = body.trim().to_string();
                    if !b.is_empty() {
                        found.push(b);
                    }
                    i += 2 + end + 2;
                    clean.push(' ');
                    continue;
                }
            }
        }
        clean.push(bytes[i] as char);
        i += 1;
    }
    // TTS hygiene: backticks and markdown fences are never speakable.
    let spoken = clean
        .replace('`', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (spoken, found)
}

#[derive(Clone)]
pub struct ElevenLabsCfg {
    pub enabled: bool,
    pub voice_id: String,
    pub model: String,
    pub key_slot: String,
    pub master: String,
}

#[derive(Clone)]
pub struct MouthCfg {
    pub voice: String,
    pub speed: f32,
    pub elevenlabs: ElevenLabsCfg,
    pub model_dir: std::path::PathBuf,
}

fn default_model_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("KOKORO_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(openbutler_common::expand(&d));
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".cache/jarvis/kokoro")
}

impl MouthCfg {
    pub fn from_map(cfg: &serde_json::Map<String, serde_json::Value>) -> Self {
        let str_of = |k: &str| {
            cfg.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let el = cfg.get("elevenlabs");
        let el_str = |k: &str| {
            el.and_then(|e| e.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let speed = cfg.get("speed").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        MouthCfg {
            voice: {
                let v = str_of("voice");
                if v.is_empty() {
                    "bm_lewis".into()
                } else {
                    v
                }
            },
            speed: if speed > 0.0 { speed } else { 1.0 },
            elevenlabs: ElevenLabsCfg {
                enabled: el
                    .and_then(|e| e.get("enabled"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                voice_id: el_str("voice_id"),
                model: {
                    let m = el_str("model");
                    if m.is_empty() {
                        "eleven_turbo_v2_5".into()
                    } else {
                        m
                    }
                },
                key_slot: {
                    let k = el_str("key_slot");
                    if k.is_empty() {
                        "openbutler-elevenlabs".into()
                    } else {
                        k
                    }
                },
                master: el_str("master"),
            },
            model_dir: default_model_dir(),
        }
    }
}

// ---- Spotify ducking (macOS) — port of ducking.py. No-op elsewhere. ----

struct DuckerInner {
    original: Option<i32>,
    gen: u64,
}

#[derive(Clone)]
pub struct Ducker {
    inner: Arc<Mutex<DuckerInner>>,
}

impl Ducker {
    fn new() -> Self {
        Ducker {
            inner: Arc::new(Mutex::new(DuckerInner {
                original: None,
                gen: 0,
            })),
        }
    }

    pub fn speech_start(&self) {
        let mut g = self.inner.lock().unwrap();
        g.gen += 1;
        if g.original.is_some() {
            return; // already ducked
        }
        match spotify_volume() {
            Some(v) if v > 30 => {
                let target = 30.max((v as f32 * 0.60) as i32).min(v - 1);
                g.original = Some(v);
                drop(g);
                set_volume(target);
            }
            _ => {}
        }
    }

    /// Schedule a debounced restore; resumed speech cancels it.
    pub fn speech_end(&self, debounce: f32) {
        {
            let mut g = self.inner.lock().unwrap();
            if g.original.is_none() {
                return;
            }
            g.gen += 1;
        }
        let inner = self.inner.clone();
        let gen = self.inner.lock().unwrap().gen;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f32(debounce.max(0.0)));
            let mut g = inner.lock().unwrap();
            if g.gen == gen {
                if let Some(v) = g.original.take() {
                    drop(g);
                    set_volume(v);
                }
            }
        });
    }

    /// Synchronous restore for shutdown paths — the debounce thread dies
    /// with the process otherwise, leaving the music stuck quiet.
    pub fn restore_now(&self) {
        let mut g = self.inner.lock().unwrap();
        g.gen += 1;
        if let Some(v) = g.original.take() {
            drop(g);
            set_volume(v);
        }
    }
}

fn osa(script: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn spotify_volume() -> Option<i32> {
    if osa("application \"Spotify\" is running").as_deref() != Some("true") {
        return None;
    }
    osa("tell application \"Spotify\" to get sound volume")?
        .parse()
        .ok()
}

fn set_volume(level: i32) {
    osa(&format!(
        "tell application \"Spotify\" to set sound volume to {level}"
    ));
}

// ---- Playback pump: one long-lived cpal stream fed from a queue. ----

struct Pump {
    queue: Arc<(Mutex<VecDeque<i16>>, Condvar)>,
    _stream: cpal::Stream,
    /// Device rate the queue is consumed at (producer resamples to this).
    rate: u32,
}

/// Build the long-lived output stream (audio law #1). Returns the stream
/// plus the device rate the producer must feed.
fn build_pump(
    rate: u32,
    queue: Arc<(Mutex<VecDeque<i16>>, Condvar)>,
) -> Result<(cpal::Stream, u32), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output device")?;
    // Prefer the engine rate in i16 mono; fall back to the device default
    // (sounddevice resampled for us in Python; cpal makes us ask).
    let supported = device
        .supported_output_configs()
        .map_err(|e| format!("output configs: {e}"))?;
    let mut picked: Option<cpal::SupportedStreamConfig> = None;
    for sc in supported {
        if sc.channels() == 1
            && sc.sample_format() == cpal::SampleFormat::I16
            && sc.min_sample_rate().0 <= rate
            && rate <= sc.max_sample_rate().0
        {
            picked = Some(sc.with_sample_rate(cpal::SampleRate(rate)));
            break;
        }
    }
    if picked.is_none() {
        picked = Some(
            device
                .default_output_config()
                .map_err(|e| format!("default output: {e}"))?,
        );
    }
    let cfg = picked.unwrap();
    let actual_rate = cfg.sample_rate().0;
    let channels = cfg.channels() as usize;
    let fmt = cfg.sample_format();
    let config: cpal::StreamConfig = cfg.into();
    let q = queue.clone();
    let err_fn = |e| eprintln!("[mouth] output stream error: {e}");
    let stream = match fmt {
        cpal::SampleFormat::I16 => device
            .build_output_stream(
                &config,
                move |out: &mut [i16], _| {
                    let (mu, _) = &*q;
                    let mut g = openbutler_common::ml(&mu);
                    for s in out.iter_mut() {
                        *s = g.pop_front().unwrap_or(0);
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("output stream: {e}"))?,
        cpal::SampleFormat::F32 => device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _| {
                    let (mu, _) = &*q;
                    let mut g = openbutler_common::ml(&mu);
                    for frame in out.chunks_mut(channels) {
                        let v = g.pop_front().unwrap_or(0) as f32 / 32768.0;
                        for s in frame.iter_mut() {
                            *s = v;
                        }
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("output stream: {e}"))?,
        cpal::SampleFormat::U8 => device
            .build_output_stream(
                &config,
                move |out: &mut [u8], _| {
                    let (mu, _) = &*q;
                    let mut g = openbutler_common::ml(&mu);
                    for frame in out.chunks_mut(channels) {
                        let v = g.pop_front().unwrap_or(0) as f32 / 32768.0;
                        let u = (v * 127.0 + 128.0).round().clamp(0.0, 255.0) as u8;
                        for s in frame.iter_mut() {
                            *s = u;
                        }
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("output stream: {e}"))?,
        other => return Err(format!("unsupported output sample format {other:?}")),
    };
    stream.play().map_err(|e| format!("play: {e}"))?;
    Ok((stream, actual_rate))
}

/// Linear resample i16 mono src_rate -> dst_rate.
fn resample(pcm: &[i16], src: u32, dst: u32) -> Vec<i16> {
    if src == dst || pcm.is_empty() {
        return pcm.to_vec();
    }
    let n = ((pcm.len() as u64 * dst as u64) / src as u64) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let pos = i as f64 * src as f64 / dst as f64;
        let k = pos.floor() as usize;
        let f = (pos - k as f64) as f32;
        let a = pcm[k.min(pcm.len() - 1)] as f32;
        let b = pcm[(k + 1).min(pcm.len() - 1)] as f32;
        out.push((a + (b - a) * f) as i16);
    }
    out
}

// ---- ElevenLabs key lookup (port of mouth._get_elevenlabs_key). ----

fn elevenlabs_key(slot: &str) -> String {
    let mut key = String::new();
    if cfg!(target_os = "macos") {
        if let Ok(o) = std::process::Command::new("security")
            .args(["find-generic-password", "-s", slot, "-w"])
            .output()
        {
            if o.status.success() {
                key = String::from_utf8_lossy(&o.stdout).trim().to_string();
            }
        }
    } else if cfg!(target_os = "linux") {
        if let Ok(o) = std::process::Command::new("secret-tool")
            .args(["lookup", "service", slot])
            .output()
        {
            if o.status.success() {
                key = String::from_utf8_lossy(&o.stdout).trim().to_string();
            }
        }
    }
    if key.is_empty() {
        key = std::env::var("ELEVENLABS_API_KEY").unwrap_or_default();
    }
    key
}

fn have(prog: &str) -> bool {
    std::env::var_os("PATH").map_or(false, |p| {
        std::env::split_paths(&p)
            .map(|d| d.join(prog))
            .any(|f| f.is_file())
    })
}

// ---- Mouth ----

pub struct Mouth {
    tx: std::sync::mpsc::Sender<(u64, String, Option<Vec<String>>)>,
    stop: Arc<AtomicBool>,
    gen: Arc<std::sync::atomic::AtomicU64>,
    speaking: Arc<AtomicBool>,
    /// Detached worker: runs to process end. The stop/gen protocol (not
    /// a join) is the shutdown path, so barge-in never blocks on audio.
    #[allow(dead_code)]
    worker: Option<std::thread::JoinHandle<()>>,
    pub ducker: Ducker,
    voice: Arc<Mutex<String>>,
    /// Shared live with the worker (speed is config-only at runtime —
    /// there is no /speed verb, matching Python).
    #[allow(dead_code)]
    speed: Arc<Mutex<f32>>,
    /// Long-lived TTS engine child, shared with the worker (lazy spawn
    /// on first synth so --help etc. never pay for it).
    serve: Arc<Mutex<SttsServe>>,
}

impl Mouth {
    pub fn new(cfg: MouthCfg, signals: Arc<crate::signals::Signals>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(u64, String, Option<Vec<String>>)>();
        let stop = Arc::new(AtomicBool::new(false));
        let gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let speaking = Arc::new(AtomicBool::new(false));
        let voice = Arc::new(Mutex::new(cfg.voice.clone()));
        let speed = Arc::new(Mutex::new(cfg.speed));
        let w_stop = stop.clone();
        let w_gen = gen.clone();
        let w_speaking = speaking.clone();
        let w_voice = voice.clone();
        let w_speed = speed.clone();
        let model_dir = cfg.model_dir.clone();
        let bin = find_stts_bin().unwrap_or_else(|_| std::path::PathBuf::from("openbutler-tts"));
        let serve = Arc::new(Mutex::new(SttsServe {
            bin,
            dir: model_dir,
            child: None,
            next_id: 0,
        }));
        let w_serve = serve.clone();
        let worker = std::thread::Builder::new()
            .name("mouth".into())
            .spawn(move || {
                worker_loop(
                    rx, w_stop, w_gen, w_speaking, w_voice, w_speed, w_serve, cfg, signals,
                );
            })
            .ok();
        Mouth {
            tx,
            stop,
            gen,
            speaking,
            worker,
            ducker: Ducker::new(),
            voice,
            speed,
            serve,
        }
    }

    pub fn speaking(&self) -> bool {
        self.speaking.load(Ordering::SeqCst)
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// Queue text (split to sentences) for speech.
    pub fn say(&self, text: &str) {
        let g = self.gen.load(Ordering::SeqCst);
        for s in split_sentences(text) {
            let _ = self.tx.send((g, s, None));
        }
    }

    /// Queue text as ONE TTS request, no sentence splitting — fuller
    /// chunks get livelier prosody. Directions ride along and publish
    /// when this chunk's audio STARTS.
    pub fn say_chunk(&self, text: &str, directions: Vec<String>) {
        let t = text.trim().to_string();
        if !t.is_empty() {
            let d = if directions.is_empty() {
                None
            } else {
                Some(directions)
            };
            let _ = self.tx.send((self.gen.load(Ordering::SeqCst), t, d));
        }
    }

    /// Barge-in: stop current playback and stale everything queued.
    /// Fresh speech (tagged after the bump) plays normally — which is
    /// why the quit signoff survives the interrupt that precedes it.
    pub fn shut_up(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Clear a stale stop before a new turn starts emitting. Without
    /// this, an interrupt followed by silence leaves the flag set and
    /// the NEXT turn's emit skips everything: a permanent mute. Called
    /// only once the old turn is fully joined (the worker's per-block
    /// cut has already consumed the flag).
    pub fn clear_stop(&self) {
        self.stop.store(false, Ordering::SeqCst);
    }

    pub fn shutdown(&self) {
        self.shut_up();
        self.ducker.restore_now();
        self.serve.lock().unwrap().kill();
    }

    pub fn wait_done(&self, timeout: Duration) {
        let t0 = Instant::now();
        while self.speaking() {
            if t0.elapsed() > timeout {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn set_voice(&self, name: &str) {
        *self.voice.lock().unwrap() = name.to_string();
    }

    /// The engine's own voice list, via the serve child (spawns it on
    /// first use, like the first spoken reply does).
    pub fn voices(&self) -> Vec<String> {
        self.serve.lock().unwrap().voices()
    }

    pub fn voice_known(&self, want: &str) -> bool {
        self.voices().iter().any(|v| v == want)
    }
}

struct SttsServe {
    bin: std::path::PathBuf,
    dir: std::path::PathBuf,
    child: Option<SttsChild>,
    next_id: u64,
}

struct SttsChild {
    proc: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

/// Locate the `openbutler-tts` binary: env override, then next to this
/// executable (both ship in the same dir), then PATH.
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

/// Serve-child stderr: appended to logs/stts-serve.log (same CWD-relative
/// convention as vlog's logs/backtalk.log) so a serve crash leaves its
/// dying words in a file instead of terminal scrollback. Falls back to
/// the terminal when the file can't be opened — logging must never break
/// the voice line.
pub(crate) fn serve_stderr() -> std::process::Stdio {
    let p = std::path::PathBuf::from("logs/stts-serve.log");
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map(|f| std::process::Stdio::from(f))
        .unwrap_or_else(|_| std::process::Stdio::inherit())
}

impl SttsServe {
    fn ensure(&mut self) -> Result<(), String> {
        if self.child.is_some() {
            return Ok(());
        }
        let mut proc = std::process::Command::new(&self.bin)
            .args(["serve", "--dir", &self.dir.to_string_lossy()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(serve_stderr())
            .spawn()
            .map_err(|e| {
                format!(
                    "spawn openbutler-tts serve: {e} (binary: {})",
                    self.bin.display()
                )
            })?;
        let stdin = std::io::BufWriter::new(proc.stdin.take().ok_or("stts stdin")?);
        let stdout = std::io::BufReader::new(proc.stdout.take().ok_or("stts stdout")?);
        self.child = Some(SttsChild {
            proc,
            stdin,
            stdout,
        });
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.proc.kill();
            let _ = c.proc.wait();
        }
    }

    fn request(&mut self, req: serde_json::Value) -> Result<serde_json::Value, String> {
        use std::io::{BufRead, Write};
        self.ensure()?;
        let c = self.child.as_mut().unwrap();
        writeln!(c.stdin, "{}", req).map_err(|e| format!("stts write: {e}"))?;
        c.stdin.flush().map_err(|e| format!("stts flush: {e}"))?;
        let mut line = String::new();
        c.stdout
            .read_line(&mut line)
            .map_err(|e| format!("stts read: {e}"))?;
        if line.trim().is_empty() {
            self.kill();
            return Err("stts serve died (EOF)".into());
        }
        serde_json::from_str(&line).map_err(|e| format!("stts bad reply: {e}"))
    }

    fn synth(&mut self, text: &str, voice: &str, speed: f32) -> Result<(u32, Vec<i16>), String> {
        use base64::Engine as _;
        let attempt = |s: &mut SttsServe, v: &str| {
            s.next_id += 1;
            let id = s.next_id;
            let rep =
                s.request(serde_json::json!({"id": id, "text": text, "voice": v, "speed": speed}))?;
            if rep.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                let rate = rep
                    .get("rate")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(KOKORO_RATE as u64) as u32;
                let b64 = rep.get("pcm_b64").and_then(|x| x.as_str()).unwrap_or("");
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| format!("pcm decode: {e}"))?;
                Ok((
                    rate,
                    raw.chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect(),
                ))
            } else {
                Err(rep
                    .get("error")
                    .and_then(|x| x.as_str())
                    .unwrap_or("synth failed")
                    .to_string())
            }
        };
        match attempt(self, voice) {
            Ok(a) => Ok(a),
            Err(e) => {
                self.kill(); // a failed engine may be wedged; respawn fresh
                if voice != "bm_lewis" {
                    log(&format!(
                        "[mouth] voice {voice:?} failed ({e}) — falling back to bm_lewis"
                    ));
                    attempt(self, "bm_lewis")
                } else {
                    Err(e)
                }
            }
        }
    }

    fn voices(&mut self) -> Vec<String> {
        match self.request(serde_json::json!({"cmd": "voices"})) {
            Ok(rep) => rep
                .get("voices")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => vec![],
        }
    }
}

/// ElevenLabs -> curl streaming mp3 -> ffmpeg decode -> 44.1k i16.
/// curl replaces Python's httpx (same POST, same body); ffmpeg is the
/// same local-master step Python already shells out to.
fn elevenlabs_synth(text: &str, el: &ElevenLabsCfg) -> Result<(u32, Vec<i16>), String> {
    if !(el.enabled && !el.voice_id.is_empty()) {
        return Err("elevenlabs not configured".into());
    }
    let key = elevenlabs_key(&el.key_slot);
    if key.is_empty() {
        return Err("elevenlabs key not found".into());
    }
    for prog in ["curl", "ffmpeg"] {
        if !have(prog) {
            return Err(format!("{prog} not on PATH"));
        }
    }
    let url = format!(
        "https://api.elevenlabs.io/v1/text-to-speech/{}/stream?output_format=mp3_44100_128",
        el.voice_id
    );
    let body = serde_json::json!({
        "text": text, "model_id": el.model,
        "voice_settings": {"stability": 0.5, "similarity_boost": 0.75},
    })
    .to_string();
    let mut curl = std::process::Command::new("curl")
        .args([
            "-sS",
            "--fail",
            "--max-time",
            "30",
            "-X",
            "POST",
            &url,
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("xi-api-key: {key}"),
            "--data-binary",
            "@-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    use std::io::Write;
    if let Some(mut stdin) = curl.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
    }
    let curl_out = curl.stdout.take().ok_or("curl stdout")?;
    let mut ff = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "quiet",
            "-i",
            "pipe:0",
            "-af",
            &el.master,
            "-f",
            "s16le",
            "-ar",
            "44100",
            "-ac",
            "1",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::from(curl_out))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    use std::io::Read;
    let mut raw = Vec::new();
    if let Some(mut so) = ff.stdout.take() {
        so.read_to_end(&mut raw)
            .map_err(|e| format!("ffmpeg read: {e}"))?;
    }
    let st = ff.wait().map_err(|e| format!("ffmpeg wait: {e}"))?;
    let _ = curl.wait();
    if !st.success() || raw.len() < 2 {
        return Err("elevenlabs fetch/decode failed".into());
    }
    let pcm: Vec<i16> = raw
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok((EL_RATE, pcm))
}

// Prefetched audio may replay only for the exact sentence + generation it
// was synthed for; anything else synthesizes fresh, so every queued
// sentence is heard.
fn pipeline_usable(stashed: Option<(u64, &str)>, gen: u64, sentence: &str) -> bool {
    match stashed {
        Some((sg, stext)) => sg == gen && stext == sentence,
        None => false,
    }
}

fn worker_loop(
    rx: std::sync::mpsc::Receiver<(u64, String, Option<Vec<String>>)>,
    stop: Arc<AtomicBool>,
    gen: Arc<std::sync::atomic::AtomicU64>,
    speaking: Arc<AtomicBool>,
    voice: Arc<Mutex<String>>,
    speed: Arc<Mutex<f32>>,
    serve: Arc<Mutex<SttsServe>>,
    cfg: MouthCfg,
    signals: Arc<crate::signals::Signals>,
) {
    let mut pump: Option<Pump> = None;
    let mut pump_rate: u32 = 0;
    let mut stashed: Option<(u64, String, Option<Vec<String>>)> = None;
    let mut next_synth: Option<std::thread::JoinHandle<Option<(u32, Vec<i16>)>>> = None;
    // Identity-tagged prefetch audio; pipeline_usable enforces the match.
    let mut stashed_audio: Option<(u64, String, u32, Vec<i16>)> = None;
    loop {
        let (g, sentence, directions) = match stashed.take() {
            Some(it) => it,
            None => match rx.recv() {
                Ok(it) => it,
                Err(_) => break, // all senders gone — process exit path
            },
        };
        // Stale generations (queued before a shut_up) never speak.
        if g != gen.load(Ordering::SeqCst) {
            continue;
        }
        // Opportunistic batching: if next sentences already queued (LLM
        // streamed fast), coalesce up to 2 more into one TTS request so
        // 7 sentences become ~3 synth calls instead of 7, halving gaps
        // without adding artificial wait.
        let mut batch_text = sentence;
        let mut batch_dirs = directions;
        for _ in 0..2 {
            if let Ok((ng, nsent, ndirs)) = rx.try_recv() {
                if ng != gen.load(Ordering::SeqCst) {
                    continue;
                }
                if nsent.trim().is_empty() {
                    continue;
                }
                batch_text.push(' ');
                batch_text.push_str(&nsent);
                if let Some(nd) = ndirs {
                    let bd = batch_dirs.get_or_insert_with(Vec::new);
                    bd.extend(nd);
                }
            } else {
                break;
            }
        }
        let sentence = batch_text;
        let directions = batch_dirs;
        // Prefetch next sentence BEFORE drain so we can start synthesizing
        // it on a background thread while the current audio plays. This
        // hides 150-350ms of synth under the drain + breath gap.
        let next_prefetch: Option<(u64, String, Option<Vec<String>>)> = {
            let deadline = Duration::from_millis(80);
            let mut result = None;
            loop {
                match rx.recv_timeout(deadline) {
                    Ok((ng, nsent, ndirs))
                        if ng == gen.load(Ordering::SeqCst) && !nsent.trim().is_empty() =>
                    {
                        result = Some((ng, nsent, ndirs));
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            result
        };
        stop.store(false, Ordering::SeqCst);
        if sentence.trim().is_empty() {
            continue;
        }
        speaking.store(true, Ordering::SeqCst);
        // Ducker is main-thread owned; mirror the speech_start here via bus.
        signals.static_stop(); // thinking sound dies when speech starts
        signals.set_state("speaking");
        let v = voice.lock().unwrap().clone();
        let sp = *speed.lock().unwrap();
        // Use pre-synthesized audio from pipeline only when it matches the
        // exact (possibly batch-merged) sentence of this generation;
        // otherwise synthesize fresh so every queued sentence is heard.
        let audio = match stashed_audio.take() {
            Some((sg, stext, rate, pcm)) if pipeline_usable(Some((sg, &stext)), g, &sentence) => {
                log(&format!(
                    "[mouth] pipeline hit — skipping synth for {} chars",
                    sentence.len()
                ));
                Some((rate, pcm))
            }
            stale => {
                if stale.is_some() {
                    log("[mouth] pipeline stash mismatch — synthesizing fresh");
                }
                None
            }
        };
        let audio = match audio {
            Some(a) => Some(a),
            None => match elevenlabs_synth(&sentence, &cfg.elevenlabs) {
                Ok(a) => {
                    log(&format!(
                        "[mouth] elevenlabs spoke {} chars",
                        sentence.len()
                    ));
                    Some(a)
                }
                Err(e) => {
                    if cfg.elevenlabs.enabled {
                        log(&format!(
                            "[mouth] elevenlabs failed ({e}) — falling back to {v}"
                        ));
                    }
                    match serve.lock().unwrap().synth(&sentence, &v, sp) {
                        Ok(a) => Some(a),
                        Err(err) => {
                            log(&format!("[mouth] synth/play error: {err}"));
                            None
                        }
                    }
                }
            },
        };
        if let Some((rate, pcm)) = audio {
            if pump.is_none() || pump_rate != rate {
                let queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
                match build_pump(rate, queue.clone()) {
                    Ok((stream, actual)) => {
                        if actual != rate {
                            log(&format!(
                                "[mouth] device runs {actual}Hz; resampling from {rate}Hz"
                            ));
                        }
                        pump = Some(Pump {
                            queue,
                            _stream: stream,
                            rate: actual,
                        });
                        pump_rate = rate;
                    }
                    Err(e) => {
                        log(&format!("[mouth] no audio device ({e}); skipping playback"));
                        pump = None;
                    }
                }
            }
            // Pipeline: start synthesizing the NEXT sentence on a
            // background thread while we drain the current one.
            // Hides 150-350ms synth behind the drain + breath gap.
            if let Some((_ng, ref nsent, ref _ndirs)) = next_prefetch {
                let serve_c = serve.clone();
                let v = v.clone();
                let sp_c = sp;
                let text = nsent.clone();
                next_synth = Some(std::thread::spawn(move || {
                    serve_c.lock().unwrap().synth(&text, &v, sp_c).ok()
                }));
            }
            if let Some(p) = pump.as_ref() {
                // AUDIO STARTS HERE: the buffer is full and the first
                // block is next. Directions publish now, on the word.
                if let Some(d) = directions.as_ref() {
                    signals.direction(d);
                }
                let data = resample(&pcm, rate, p.rate);
                let mut cut = false;
                for chunk in data.chunks(BLOCK) {
                    if stop.load(Ordering::SeqCst) {
                        cut = true;
                        break;
                    }
                    {
                        let (mu, _) = &*p.queue;
                        openbutler_common::ml(&mu).extend(chunk.iter().cloned());
                    }
                    signals.feed_waveform(chunk);
                    // Wait for the device to drain this block (bounded:
                    // a dead device must not wedge the voice line).
                    let t0 = Instant::now();
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            cut = true;
                            break;
                        }
                        let len = openbutler_common::ml(&p.queue.0).len();
                        if len < BLOCK || t0.elapsed() > Duration::from_secs(5) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    // Re-check after the blocking drain: a barge-in
                    // landing mid-block must not re-assert speaking.
                    if stop.load(Ordering::SeqCst) {
                        cut = true;
                        break;
                    }
                }
                if cut {
                    // Barge-in cut: pad a beat of silence — the stream
                    // itself NEVER stops (audio law #1).
                    let (mu, _) = &*p.queue;
                    let mut g = openbutler_common::ml(&mu);
                    g.clear();
                    g.extend(std::iter::repeat(0).take(BLOCK * 3));
                }
            }
        }
        // Join the background synth; stash tagged for pipeline_usable.
        if let Some(h) = next_synth.take() {
            let joined = h.join().ok().flatten();
            if let Some((ng, nsent, ndirs)) = next_prefetch {
                if let Some((rate, pcm)) = joined {
                    stashed_audio = Some((ng, nsent.clone(), rate, pcm));
                }
                stashed = Some((ng, nsent, ndirs));
            }
        }
        // Drain-check: the reply genuinely stopped only when the queue
        // is empty — not in the gap between two sentences. (The mpsc
        // queue doubles as the sentence backlog here; a peeked item is
        // stashed, never dropped.)
        if stashed.is_none() {
            match rx.try_recv() {
                Ok(it) => stashed = Some(it),
                Err(_) => {
                    // Tail drain + natural breath: when the LLM genuinely
                    // pauses between paragraphs, insert ~180ms of silence
                    // (2 BLOCKs at 24k) instead of the previous 1-2s
                    // starvation gap. If the next sentence was already
                    // queued, stashed would be Some and we never get here.
                    if let Some(p) = pump.as_ref() {
                        let t0 = Instant::now();
                        while openbutler_common::ml(&p.queue.0).len() >= BLOCK
                            && t0.elapsed() < Duration::from_secs(10)
                        {
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        // Natural breath before idling — only when we
                        // actually drained something (not on barge-in cut).
                        if !stop.load(Ordering::SeqCst) {
                            let (mu, _) = &*p.queue;
                            let mut g = openbutler_common::ml(&mu);
                            // 2 BLOCKs ≈180ms at 24k, ~130ms at 44.1k — a
                            // human breath, not a stall.
                            g.extend(std::iter::repeat(0).take(BLOCK * 2));
                            std::thread::sleep(Duration::from_millis(90));
                        }
                    }
                    speaking.store(false, Ordering::SeqCst);
                    signals.reply_done();
                    signals.set_state("idle");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_replays_only_exact_match() {
        // Exact sentence + generation: replay (the fast path).
        assert!(pipeline_usable(Some((3, "Hey.")), 3, "Hey."));
        // Batch-merged text covers more than the audio: fresh synth.
        assert!(!pipeline_usable(Some((3, "Hey.")), 3, "Hey. You."));
        assert!(!pipeline_usable(
            Some((3, "Hey, yeah I'm here!")),
            3,
            "Hey, yeah I'm here! What's on your mind?"
        ));
        // Barge-in revoked the generation: fresh synth, no leak.
        assert!(!pipeline_usable(Some((3, "Hey.")), 4, "Hey."));
        // Nothing stashed: fresh synth.
        assert!(!pipeline_usable(None, 3, "Hey."));
    }

    #[test]
    fn sentences_split_like_python() {
        assert_eq!(
            split_sentences("Hello world. How are you? Fine!"),
            vec!["Hello world.", "How are you?", "Fine!"]
        );
        assert_eq!(split_sentences("no punctuation"), vec!["no punctuation"]);
        assert_eq!(split_sentences("   "), Vec::<String>::new());
        // Abbreviations must not be split mid-sentence.
        assert_eq!(
            split_sentences("Iran and the U.S. are trading strikes"),
            vec!["Iran and the U.S. are trading strikes"]
        );
        assert_eq!(
            split_sentences("Dr. Smith went home. He was tired."),
            vec!["Dr. Smith went home.", "He was tired."]
        );
        assert_eq!(
            split_sentences("The U.K. and E.U. agree. Done."),
            vec!["The U.K. and E.U. agree.", "Done."]
        );
    }

    #[test]
    fn directions_lifted_not_spoken() {
        let (s, d) = strip_directions("Hello <<nod>> world");
        assert_eq!(s, "Hello world");
        assert_eq!(d, vec!["nod"]);
        let (s2, d2) = strip_directions("no tags here");
        assert_eq!(s2, "no tags here");
        assert!(d2.is_empty());
        // Runaway tag (too long / nested) stays literal-ish, never swallows.
        let big = format!("a <<{}>> b", "x".repeat(200));
        let (s3, d3) = strip_directions(&big);
        assert!(d3.is_empty());
        assert!(s3.contains("xxx"));
    }

    #[test]
    fn backticks_scrubbed() {
        let (s, _) = strip_directions("run `foo --bar` now");
        assert_eq!(s, "run foo --bar now");
    }

    #[test]
    fn resample_identity_and_double() {
        let p = vec![0i16, 1000, 2000, 3000];
        assert_eq!(resample(&p, 24000, 24000), p);
        let up = resample(&p, 100, 200);
        assert_eq!(up.len(), 8);
    }
}
