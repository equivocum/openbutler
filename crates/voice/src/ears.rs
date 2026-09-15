// The ears — port of backtalk/ears.py: mic capture with VAD endpointing,
// transcribed in-process by the fw decode port (openbutler-stt lib).
//
// record_held() is hold-to-talk (the button is the VAD). listen_once() is
// open-mic mode: blocks until one complete utterance, then its transcript.
// Endpointing: opens after ~120ms sustained speech, closes after
// `silence_ms` trailing quiet. A `gate` callable suppresses listening
// (open mic ignores the speakers unless barge-in is on).

use openbutler_stt::fw::FwEngine;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const RATE: u32 = 16000;
pub const FRAME_MS: u32 = 30;
pub const FRAME_LEN: usize = 480; // samples per 30ms frame at 16k
pub const OPEN_FRAMES: u32 = 4; // ~120ms speech to open an utterance
pub const MAX_UTTER_S: f64 = 30.0;

// Whisper's stock hallucinations on noise and silence. Matched
// case-insensitively after stripping punctuation; a genuine sentence that
// merely CONTAINS these words still passes, only a bare match drops.
fn hallucinations() -> &'static [&'static str] {
    &[
        "thank you",
        "thank you very much",
        "thanks for watching",
        "thank you for watching",
        "please subscribe",
        "subtitles by",
        "bye",
    ]
}

// Above this average no-speech probability the clip was silence no matter
// what text came out...
const NO_SPEECH_DROP: f32 = 0.6;
// ...but a bare blacklist hit only needs an elevated score, so a
// clearly-spoken "thank you" (low score) still reaches the agent.
const NO_SPEECH_SUSPECT: f32 = 0.35;

fn log(line: &str) {
    crate::vlog::log(line);
}

/// Pure decision logic — no model, no mic — stays unit-testable.
pub fn is_hallucination(text: &str, probs: &[f32]) -> bool {
    let norm: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let norm = norm.split_whitespace().collect::<Vec<_>>().join(" ");
    let avg = if probs.is_empty() {
        0.0
    } else {
        probs.iter().sum::<f32>() / probs.len() as f32
    };
    if !probs.is_empty() && avg >= NO_SPEECH_DROP {
        return true;
    }
    hallucinations().contains(&norm.as_str()) && (probs.is_empty() || avg >= NO_SPEECH_SUSPECT)
}

/// Strip bracketed non-speech markers ([BLANK_AUDIO], (coughs)...).
pub fn strip_nonspeech(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct Ears {
    pub aggressiveness: u32,
    pub silence_frames: u32,
    stt_model: String,
    mic_device: String,
    filter_hallucinations: bool,
    engine: Mutex<Option<FwEngine>>,
    mic_checked: Mutex<bool>,
    mic_warned: Mutex<bool>,
}

impl Ears {
    pub fn new(cfg: &serde_json::Map<String, serde_json::Value>) -> Self {
        let s = |k: &str| {
            cfg.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ears {
            aggressiveness: 2,
            silence_frames: 480 / FRAME_MS,
            stt_model: {
                let m = s("stt_model");
                if m.is_empty() {
                    "small.en".into()
                } else {
                    m
                }
            },
            mic_device: s("mic_device"),
            filter_hallucinations: cfg
                .get("filter_hallucinations")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            engine: Mutex::new(None),
            mic_checked: Mutex::new(false),
            mic_warned: Mutex::new(false),
        }
    }

    // Language is pinned: auto-detect is a deliberate omission (the
    // sys::DetectionResult fields are private, so there is nothing to
    // read the detected tag from) — same scope as the fw port.
    fn lang(&self) -> &str {
        "en"
    }

    /// Load the STT model (first call pulls from the HF cache). Called at
    /// startup while the greeting plays, so the first real utterance
    /// doesn't pay the load.
    pub fn warm(&self) -> Result<(), String> {
        self.check_microphone();
        let mut g = self.engine.lock().unwrap();
        if g.is_none() {
            log(&format!("[ears] loading {} ...", self.stt_model));
            let dir = openbutler_stt::resolve_model(&self.stt_model)?;
            match FwEngine::load(&dir) {
                Ok(e) => {
                    *g = Some(e);
                    log("[ears] model ready (fw port)");
                }
                Err(e) => return Err(format!("[ears] load failed: {e}")),
            }
        }
        Ok(())
    }

    /// int16 mono 16kHz -> text, with the hallucination filter.
    pub fn transcribe(&self, pcm: &[i16]) -> Result<String, String> {
        self.warm()?;
        let audio: Vec<f32> = pcm.iter().map(|v| *v as f32 / 32768.0).collect();
        let mut g = self.engine.lock().unwrap();
        let e = g.as_mut().unwrap();
        let segs = e
            .transcribe(&audio, self.lang())
            .map_err(|e| format!("transcribe: {e}"))?;
        let probs: Vec<f32> = segs.iter().map(|s| s.no_speech_prob).collect();
        let text: String = segs
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let text = strip_nonspeech(text.trim());
        if !text.is_empty() && self.filter_hallucinations && is_hallucination(&text, &probs) {
            log(&format!("[ears] filtered hallucination: {text:?}"));
            return Ok(String::new());
        }
        Ok(text)
    }

    /// Say whether recording is possible at all, BEFORE the greeting.
    pub fn check_microphone(&self) -> bool {
        let mut checked = self.mic_checked.lock().unwrap();
        if *checked {
            return true;
        }
        *checked = true;
        drop(checked);
        match default_input_available() {
            true => true,
            false => {
                *self.mic_warned.lock().unwrap() = true;
                for line in mic_message("no default input device") {
                    log(&line);
                }
                false
            }
        }
    }

    /// Block until one utterance completes; Ok(None) on timeout/abort.
    /// An `abort` returning True closes the mic and returns None — how a
    /// live switch back to push-to-talk shuts the open mic down promptly.
    /// A panicking audio backend (cpal asserts on flaky ALSA devices)
    /// becomes a device-failure Err, never a dead mic thread.
    pub fn listen_once(
        &self,
        gate: &dyn Fn() -> bool,
        abort: &dyn Fn() -> bool,
        timeout_s: Option<f64>,
    ) -> Result<Option<String>, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.listen_inner(gate, abort, timeout_s)
        })) {
            Ok(r) => r,
            Err(_) => Err("audio backend failure during capture".into()),
        }
    }

    /// Open a persistent wake-word tap: raw 16k mono i16 frames WITHOUT
    /// VAD endpointing (the scorer decides). One tap owns one cpal
    /// stream; the caller drains it with read_tick(). Backend panics
    /// become errors, same as the other capture entry points.
    pub fn wake_tap(&self) -> Result<WakeTap, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            MicStream::open(&self.mic_device)
        })) {
            Ok(Ok(mic)) => Ok(WakeTap {
                stream: mic,
                acc: Vec::with_capacity(crate::wake::WAKE_TICK_SAMPLES * 2),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("audio backend failure during capture".into()),
        }
    }

    /// Like listen_once but with `preroll` samples (from the wake tap
    /// gap) prepended before STT so "hey Jarvis what time" doesn't lose
    /// "what". Panic-safe like the other entry points.
    pub fn listen_once_with_preroll(
        &self,
        preroll: Vec<i16>,
        gate: &dyn Fn() -> bool,
        abort: &dyn Fn() -> bool,
        timeout_s: Option<f64>,
    ) -> Result<Option<String>, String> {
        let preroll_clone = preroll.clone();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.listen_inner_with_preroll(preroll_clone, gate, abort, timeout_s)
        })) {
            Ok(r) => r,
            Err(_) => Err("audio backend failure during capture".into()),
        }
    }

    fn listen_inner_with_preroll(
        &self,
        preroll: Vec<i16>,
        gate: &dyn Fn() -> bool,
        abort: &dyn Fn() -> bool,
        timeout_s: Option<f64>,
    ) -> Result<Option<String>, String> {
        use wavekat_vad::backends::webrtc::{WebRtcVad, WebRtcVadMode};
        use wavekat_vad::VoiceActivityDetector;
        let mode = match self.aggressiveness {
            0 => WebRtcVadMode::Quality,
            1 => WebRtcVadMode::LowBitrate,
            3 => WebRtcVadMode::VeryAggressive,
            _ => WebRtcVadMode::Aggressive,
        };
        let mut vad = WebRtcVad::with_frame_duration(RATE, mode, FRAME_MS)
            .map_err(|e| format!("VAD init: {e}"))?;
        let mic = MicStream::open(&self.mic_device)?;
        let mut frames: Vec<i16> = Vec::new();
        let mut ring: VecDeque<[i16; FRAME_LEN]> = VecDeque::new();
        // Seed ring with tail of preroll so VAD's 120ms open sees it.
        if !preroll.is_empty() {
            for chunk in preroll.chunks(FRAME_LEN) {
                if chunk.len() == FRAME_LEN {
                    let mut f = [0i16; FRAME_LEN];
                    f.copy_from_slice(chunk);
                    ring.push_back(f);
                    while ring.len() > 8 {
                        ring.pop_front();
                    }
                }
            }
        }
        let (mut speech_run, mut silence_run, mut speech_total) = (0u32, 0u32, 0u32);
        let mut in_utterance = false;
        let t0 = Instant::now();
        loop {
            let frame = mic.read_frame()?;
            if abort() {
                return Ok(None);
            }
            if timeout_s
                .map(|t| t0.elapsed().as_secs_f64() > t && !in_utterance)
                .unwrap_or(false)
            {
                return Ok(None);
            }
            if gate() {
                ring.clear();
                continue;
            }
            let speech = vad.process(&frame, RATE).map(|p| p > 0.5).unwrap_or(false);
            if !in_utterance {
                ring.push_back(frame);
                while ring.len() > 8 {
                    ring.pop_front();
                }
                speech_run = if speech { speech_run + 1 } else { 0 };
                if speech_run >= OPEN_FRAMES {
                    in_utterance = true;
                    frames.extend(ring.iter().flatten().cloned());
                    // Prepend the gap audio before the VAD-opened frames.
                    if !preroll.is_empty() {
                        let mut combined = preroll.clone();
                        combined.extend(frames.drain(..));
                        frames = combined;
                    }
                    silence_run = 0;
                }
            } else {
                frames.extend(frame.iter().cloned());
                if speech {
                    speech_total += 1;
                    silence_run = 0;
                } else {
                    silence_run += 1;
                }
                if silence_run >= self.silence_frames
                    || (frames.len() as f64) / (RATE as f64) > MAX_UTTER_S
                {
                    if speech_total < 8 {
                        in_utterance = false;
                        frames.clear();
                        ring.clear();
                        speech_run = 0;
                        speech_total = 0;
                        continue;
                    }
                    return Ok(Some(self.transcribe(&frames)?));
                }
            }
        }
    }

    fn listen_inner(
        &self,
        gate: &dyn Fn() -> bool,
        abort: &dyn Fn() -> bool,
        timeout_s: Option<f64>,
    ) -> Result<Option<String>, String> {
        use wavekat_vad::backends::webrtc::{WebRtcVad, WebRtcVadMode};
        use wavekat_vad::VoiceActivityDetector;
        // wavekat modes are webrtcvad aggressiveness 0-3 by construction
        // (Quality..VeryAggressive); 2 preserves the verified parity.
        let mode = match self.aggressiveness {
            0 => WebRtcVadMode::Quality,
            1 => WebRtcVadMode::LowBitrate,
            3 => WebRtcVadMode::VeryAggressive,
            _ => WebRtcVadMode::Aggressive,
        };
        let mut vad = WebRtcVad::with_frame_duration(RATE, mode, FRAME_MS)
            .map_err(|e| format!("VAD init: {e}"))?;
        let mic = MicStream::open(&self.mic_device)?;
        let mut frames: Vec<i16> = Vec::new();
        let mut ring: VecDeque<[i16; FRAME_LEN]> = VecDeque::new();
        let (mut speech_run, mut silence_run, mut speech_total) = (0u32, 0u32, 0u32);
        let mut in_utterance = false;
        let t0 = Instant::now();
        loop {
            let frame = mic.read_frame()?;
            if abort() {
                return Ok(None);
            }
            if timeout_s
                .map(|t| t0.elapsed().as_secs_f64() > t && !in_utterance)
                .unwrap_or(false)
            {
                return Ok(None);
            }
            if gate() {
                ring.clear();
                continue;
            }
            let speech = vad.process(&frame, RATE).map(|p| p > 0.5).unwrap_or(false);
            if !in_utterance {
                ring.push_back(frame);
                while ring.len() > 8 {
                    ring.pop_front();
                }
                speech_run = if speech { speech_run + 1 } else { 0 };
                if speech_run >= OPEN_FRAMES {
                    in_utterance = true;
                    frames.extend(ring.iter().flatten().cloned());
                    silence_run = 0;
                }
            } else {
                frames.extend(frame.iter().cloned());
                if speech {
                    speech_total += 1;
                    silence_run = 0;
                } else {
                    silence_run += 1;
                }
                if silence_run >= self.silence_frames
                    || (frames.len() as f64) / (RATE as f64) > MAX_UTTER_S
                {
                    if speech_total < 8 {
                        // <240ms of actual speech: a noise blip, not a
                        // sentence — keep listening.
                        in_utterance = false;
                        frames.clear();
                        ring.clear();
                        speech_run = 0;
                        speech_total = 0;
                        continue;
                    }
                    return Ok(Some(self.transcribe(&frames)?));
                }
            }
        }
    }

    /// Hold-to-talk capture: record while is_held(), then transcribe.
    /// None for taps shorter than 0.25s (accidental presses). Backend
    /// panics become errors — this runs on the main thread, where a
    /// panic would take the whole voice line down.
    pub fn record_held(&self, is_held: &dyn Fn() -> bool) -> Result<Option<String>, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.record_inner(is_held)))
        {
            Ok(r) => r,
            Err(_) => Err("audio backend failure during capture".into()),
        }
    }

    fn record_inner(&self, is_held: &dyn Fn() -> bool) -> Result<Option<String>, String> {
        let mic = MicStream::open(&self.mic_device)?;
        let mut frames: Vec<i16> = Vec::new();
        let t0 = Instant::now();
        while is_held() && t0.elapsed().as_secs_f64() < 60.0 {
            match mic.read_frame_timeout(Duration::from_millis(200)) {
                Some(f) => frames.extend(f.iter().cloned()),
                None => {
                    if !is_held() {
                        break;
                    }
                }
            }
        }
        // A small tail so the last word isn't clipped at release.
        for _ in 0..6 {
            if let Some(f) = mic.read_frame_timeout(Duration::from_millis(200)) {
                frames.extend(f.iter().cloned());
            }
        }
        if (frames.len() as f64) / (RATE as f64) < 0.25 {
            return Ok(None);
        }
        Ok(Some(self.transcribe(&frames)?))
    }
}

fn default_input_available() -> bool {
    use cpal::traits::HostTrait;
    cpal::default_host().default_input_device().is_some()
}

fn mic_message(detail: &str) -> Vec<String> {
    vec![
        "[ears] NO WORKING MICROPHONE. Nothing can be recorded on this machine, so the talk key will have nothing to send.".into(),
        format!("[ears] the audio system said: {detail}"),
        "[ears] plug one in and start the voice line again. If one IS plugged in, check it is allowed in this system's microphone privacy settings -- and if you have several, put part of the one you want in \"mic_device\" in voice.json.".into(),
    ]
}

/// Turn a device-level failure into plain words. True when handled, so
/// the caller can skip the raw repr. Said in full once, then briefly.
pub fn explain_audio_failure(exc: &str, warned: &Mutex<bool>) -> bool {
    let text = exc.to_lowercase();
    let hints = [
        "error querying device",
        "invalid device",
        "device unavailable",
        "no default input",
        "invalid number of channels",
        "device not found",
    ];
    if !hints.iter().any(|h| text.contains(h))
        && !text.contains("cpal")
        && !text.contains("alsa")
        && !text.contains("portaudio")
    {
        return false;
    }
    let mut w = warned.lock().unwrap();
    if *w {
        log("[ears] still no working microphone.");
    } else {
        *w = true;
        for line in mic_message(exc) {
            log(&line);
        }
    }
    true
}

// ---- cpal capture: 16k mono i16 frames, converted when needed. ----

/// Persistent wake-word tap: 80ms (1280-sample) ticks of raw mic audio
/// for the openWakeWord scorer. No VAD, no endpointing, no STT.
pub struct WakeTap {
    stream: MicStream,
    acc: Vec<i16>,
}

impl WakeTap {
    /// Block until one full 1280-sample tick is accumulated (~80ms of
    /// live audio; longer if the device underruns). Errors on device
    /// stall like the other capture paths.
    pub fn read_tick(&mut self) -> Result<[i16; crate::wake::WAKE_TICK_SAMPLES], String> {
        while self.acc.len() < crate::wake::WAKE_TICK_SAMPLES {
            let f = self.stream.read_frame()?;
            self.acc.extend(f.iter().cloned());
        }
        let mut t = [0i16; crate::wake::WAKE_TICK_SAMPLES];
        t.copy_from_slice(&self.acc[..crate::wake::WAKE_TICK_SAMPLES]);
        self.acc.drain(..crate::wake::WAKE_TICK_SAMPLES);
        Ok(t)
    }

    /// Keep last `max` samples (up to 0.8s) as pre-roll for the next
    /// utterance — fixes the ~100ms gap when we close the wake tap and
    /// open the VAD tap. Without this, "hey Jarvis what time" loses
    /// "what".
    pub fn take_preroll(&mut self, max: usize) -> Vec<i16> {
        if self.acc.len() > max {
            self.acc.drain(..self.acc.len() - max);
        }
        std::mem::take(&mut self.acc)
    }
}

struct MicStream {
    queue: Arc<(Mutex<VecDeque<i16>>, Condvar)>,
    _stream: cpal::Stream,
}

impl MicStream {
    fn open(mic_want: &str) -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, StreamTrait};
        let host = cpal::default_host();
        let device = resolve_input(&host, mic_want)?;
        // Prefer exactly 16k mono i16; anything else goes through the
        // converter (Direct is i16-only — a 16k/mono U8 config through
        // the passthrough is a panic, found the hard way).
        let mut picked: Option<cpal::SupportedStreamConfig> = None;
        for sc in device
            .supported_input_configs()
            .map_err(|e| format!("input configs: {e}"))?
        {
            if sc.channels() == 1
                && sc.sample_format() == cpal::SampleFormat::I16
                && sc.min_sample_rate().0 <= RATE
                && RATE <= sc.max_sample_rate().0
            {
                picked = Some(sc.with_sample_rate(cpal::SampleRate(RATE)));
                break;
            }
        }
        let (cfg, conv) = match picked {
            Some(c) => (c, Conv::Direct),
            None => {
                let def = device
                    .default_input_config()
                    .map_err(|e| format!("default input: {e}"))?;
                let conv = Conv::new(def.sample_rate().0, def.channels());
                (def, conv)
            }
        };
        let direct = matches!(&conv, Conv::Direct);
        let fmt = cfg.sample_format();
        let config: cpal::StreamConfig = cfg.into();
        let queue: Arc<(Mutex<VecDeque<i16>>, Condvar)> =
            Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let conv = Arc::new(Mutex::new(conv));
        fn err_fn(e: cpal::StreamError) {
            eprintln!("[ears] input stream error: {e}");
        }
        // Every format lands as mono f32 in the converter (cpal makes us
        // ask explicitly; sounddevice resampled for us in Python).
        macro_rules! arm {
            ($t:ty) => {{
                use cpal::Sample as _;
                let q = queue.clone();
                let c = conv.clone();
                device
                    .build_input_stream(
                        &config,
                        move |data: &[$t], _| {
                            let f: Vec<f32> =
                                data.iter().map(|s| s.to_float_sample() as f32).collect();
                            let mut g = openbutler_common::ml(&c);
                            let frames = g.push_mono_f32(&f);
                            if !frames.is_empty() {
                                let (mu, cv) = &*q;
                                openbutler_common::ml(&mu).extend(frames);
                                cv.notify_all();
                            }
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("input stream: {e}"))?
            }};
        }
        let stream = match fmt {
            cpal::SampleFormat::I16 if direct => {
                let q = queue.clone();
                let c = conv.clone();
                device
                    .build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            let mut g = openbutler_common::ml(&c);
                            let frames = g.push_i16(data);
                            if !frames.is_empty() {
                                let (mu, cv) = &*q;
                                openbutler_common::ml(&mu).extend(frames);
                                cv.notify_all();
                            }
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("input stream: {e}"))?
            }
            cpal::SampleFormat::I8 => arm!(i8),
            cpal::SampleFormat::I16 => arm!(i16),
            cpal::SampleFormat::I24 => arm!(cpal::I24),
            cpal::SampleFormat::I32 => arm!(i32),
            cpal::SampleFormat::I64 => arm!(i64),
            cpal::SampleFormat::U8 => arm!(u8),
            cpal::SampleFormat::U16 => arm!(u16),
            cpal::SampleFormat::F32 => arm!(f32),
            cpal::SampleFormat::F64 => arm!(f64),
            other => return Err(format!("unsupported input sample format {other:?}")),
        };
        stream.play().map_err(|e| format!("mic play: {e}"))?;
        Ok(MicStream {
            queue,
            _stream: stream,
        })
    }

    fn read_frame(&self) -> Result<[i16; FRAME_LEN], String> {
        let (mu, cv) = &*self.queue;
        let mut g = openbutler_common::ml(&mu);
        let t0 = Instant::now();
        while g.len() < FRAME_LEN {
            if t0.elapsed() > Duration::from_secs(5) {
                return Err("mic read timeout — device stalled".into());
            }
            let (ng, r) = cv.wait_timeout(g, Duration::from_millis(100)).unwrap();
            g = ng;
            if r.timed_out() && g.len() < FRAME_LEN {
                continue;
            }
        }
        let mut f = [0i16; FRAME_LEN];
        for s in f.iter_mut() {
            *s = g.pop_front().unwrap_or(0);
        }
        Ok(f)
    }

    fn read_frame_timeout(&self, d: Duration) -> Option<[i16; FRAME_LEN]> {
        let (mu, cv) = &*self.queue;
        let mut g = openbutler_common::ml(&mu);
        let t0 = Instant::now();
        while g.len() < FRAME_LEN {
            if t0.elapsed() > d {
                return None;
            }
            let (ng, _) = cv.wait_timeout(g, Duration::from_millis(50)).unwrap();
            g = ng;
        }
        let mut f = [0i16; FRAME_LEN];
        for s in f.iter_mut() {
            *s = g.pop_front().unwrap_or(0);
        }
        Some(f)
    }
}

fn resolve_input(host: &cpal::Host, want: &str) -> Result<cpal::Device, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let want = want.trim();
    if want.is_empty() {
        return host
            .default_input_device()
            .ok_or("no default input device".into());
    }
    let mut fallback: Option<cpal::Device> = None;
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            let name = d.name().unwrap_or_default();
            if name == want {
                return Ok(d);
            }
            if fallback.is_none() && name.to_lowercase().contains(&want.to_lowercase()) {
                fallback = Some(d);
            }
        }
    }
    if let Some(d) = fallback {
        return Ok(d);
    }
    log(&format!(
        "[ears] mic_device {want:?} not found -- using the system default."
    ));
    host.default_input_device()
        .ok_or("no default input device".into())
}

/// Sample conversion into 16k mono i16 frames. `base` is the absolute
/// source-sample index of pending[0]; `out_idx` the next output index.
enum Conv {
    Direct,
    Resample {
        src_rate: u32,
        channels: u16,
        pending: VecDeque<f32>,
        base: u64,
        out_idx: u64,
    },
}

impl Conv {
    fn new(src_rate: u32, channels: u16) -> Self {
        Conv::Resample {
            src_rate,
            channels,
            pending: VecDeque::new(),
            base: 0,
            out_idx: 0,
        }
    }

    fn push_i16(&mut self, data: &[i16]) -> Vec<i16> {
        match self {
            Conv::Direct => data.to_vec(),
            Conv::Resample { .. } => {
                let f: Vec<f32> = data.iter().map(|v| *v as f32 / 32768.0).collect();
                self.push_mono_f32(&f)
            }
        }
    }

    fn push_mono_f32(&mut self, data: &[f32]) -> Vec<i16> {
        let (src_rate, channels, pending, base, out_idx) = match self {
            Conv::Resample {
                src_rate,
                channels,
                pending,
                base,
                out_idx,
            } => (*src_rate, *channels, pending, base, out_idx),
            Conv::Direct => unreachable!(),
        };
        let ch = channels.max(1) as usize;
        for frame in data.chunks(ch) {
            let m: f32 = frame.iter().sum::<f32>() / frame.len().max(1) as f32;
            pending.push_back(m);
        }
        let mut out = Vec::new();
        loop {
            let src_pos = *out_idx as f64 * src_rate as f64 / RATE as f64;
            let k = src_pos.floor() as u64;
            // Absolute -> relative; need k and k+1 present.
            if k < *base || (k + 1 - *base) as usize >= pending.len() {
                break;
            }
            let i = (k - *base) as usize;
            let f = (src_pos - k as f64) as f32;
            let v = pending[i] * (1.0 - f) + pending[i + 1] * f;
            out.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
            *out_idx += 1;
        }
        // Compact history, keeping one sample for the interpolator.
        let keep_from = src_pos_floor(*out_idx, src_rate)
            .saturating_sub(*base)
            .saturating_sub(1);
        let drop = (keep_from as usize).min(pending.len());
        for _ in 0..drop {
            pending.pop_front();
        }
        *base += drop as u64;
        out
    }
}

fn src_pos_floor(out_idx: u64, src_rate: u32) -> u64 {
    (out_idx as f64 * src_rate as f64 / RATE as f64).floor() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hallucinations_filtered() {
        assert!(is_hallucination("Thank you.", &[0.7, 0.8]));
        assert!(is_hallucination("Thank you.", &[]));
        assert!(!is_hallucination(
            "Thank you for the update, friend.",
            &[0.1]
        ));
        assert!(!is_hallucination("Thank you.", &[0.05]));
        assert!(!is_hallucination("hello world", &[]));
    }

    #[test]
    fn nonspeech_markers_stripped() {
        assert_eq!(strip_nonspeech("[BLANK_AUDIO] hello (coughs)"), "hello");
        assert_eq!(strip_nonspeech("  "), "");
    }

    #[test]
    fn direct_conv_passthrough() {
        let mut c = Conv::Direct;
        assert_eq!(c.push_i16(&[1, 2, 3]), vec![1, 2, 3]);
    }
}
