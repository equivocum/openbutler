// openbutler-tts — TTS server: Kokoro-82M via tts-rs (ONNX + espeak-ng).
//   voices [--dir D]                          list voices in the archive
//   synth <text> --voice V [--dir D] [--out F] [--speed S]
//                                             text -> 24kHz wav + JSON stats
// Model dir layout (~/.cache/jarvis/kokoro): kokoro.onnx (any .onnx),
// voices-v1.0.bin (thewh1teagle/kokoro-onnx model-files-v1.0).
// (kokoroxide was evaluated first: uninstallable — its ort ^1.16 deps are
// all yanked. tts-rs on ort 2.x is the maintained path.)

use std::path::{Path, PathBuf};
use std::time::Instant;

use openbutler_tts::tts;
use tts::engines::kokoro::{KokoroEngine, KokoroInferenceParams, KokoroModelParams};
use tts::SynthesisEngine;

mod wake;

const NAME: &str = "openbutler-tts";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn model_dir(dir_flag: Option<&str>) -> PathBuf {
    if let Some(d) = dir_flag {
        return PathBuf::from(openbutler_common::expand(d));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/jarvis/kokoro")
}

/// Wake-word model dir: JARVIS_WAKE_DIR or ~/.cache/jarvis/wake.
/// Pins + sha256 live in models/wake.json (Apache-2.0).
fn wake_dir() -> PathBuf {
    if let Ok(d) = std::env::var("JARVIS_WAKE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(openbutler_common::expand(&d));
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/jarvis/wake")
}

fn load_engine(dir: &Path) -> Result<KokoroEngine, String> {
    if !dir.join("voices-v1.0.bin").is_file() {
        return Err(format!("missing {}", dir.join("voices-v1.0.bin").display()));
    }
    let mut engine = KokoroEngine::new();
    let params = KokoroModelParams {
        num_threads: None,
        optimized_model_cache_path: Some(dir.join("kokoro.opt.onnx")),
    };
    engine
        .load_model_with_params(dir, params)
        .map_err(|e| format!("model load: {e}"))?;
    Ok(engine)
}

fn cmd_voices(dir: &Path) -> i32 {
    match load_engine(dir) {
        Ok(e) => {
            for n in e.list_voices() {
                println!("{n}");
            }
            0
        }
        Err(e) => {
            eprintln!("{NAME}: {e}");
            1
        }
    }
}

fn write_wav_24k(path: &str, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 24000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| format!("wav: {e}"))?;
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        w.write_sample(v).map_err(|e| format!("wav: {e}"))?;
    }
    w.finalize().map_err(|e| format!("wav: {e}"))?;
    Ok(())
}

fn cmd_synth(text: &str, voice: &str, dir: &Path, out: &str, speed: f32) -> i32 {
    let t0 = Instant::now();
    let mut engine = match load_engine(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let load_ms = t0.elapsed().as_millis();
    let params = KokoroInferenceParams {
        voice: voice.to_string(),
        speed,
        style_index: None,
    };
    let t1 = Instant::now();
    let res = match engine.synthesize(text, Some(params)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{NAME}: synth failed: {e}");
            return 1;
        }
    };
    let synth_ms = t1.elapsed().as_millis();
    if let Err(e) = write_wav_24k(out, &res.samples) {
        eprintln!("{NAME}: {e}");
        return 1;
    }
    println!(
        "{}",
        serde_json::json!({
            "voice": voice, "out": out, "speed": speed,
            "sample_rate": res.sample_rate,
            "audio_s": res.samples.len() as f64 / res.sample_rate as f64,
            "load_ms": load_ms, "synth_ms": synth_ms,
        })
    );
    0
}

fn cmd_serve(dir: &Path) -> i32 {
    use base64::Engine as _;
    use std::io::{BufRead, Write};
    let mut engine = match load_engine(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    eprintln!("{NAME}: serving from {}", dir.display());
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    // Wake scorer: lazy (TTS-only callers pay nothing) + stateful
    // (feature context lives across frames; reset after detections).
    // One slot: a `wake` request naming a different classifier drops the
    // loaded one (model switches are rare; correctness over speed).
    let mut waker: Option<wake::WakeScorer> = None;
    let mut waker_file = String::new();
    let wdir = wake_dir();
    let mut ensure_waker =
        |waker: &mut Option<wake::WakeScorer>, file: &str| -> Result<(), String> {
            if waker.is_none() || waker_file != file {
                *waker = Some(wake::WakeScorer::load(&wdir, file)?);
                waker_file = file.to_string();
            }
            Ok(())
        };
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::json!({"ok": false, "error": format!("bad request: {e}")})
                );
                continue;
            }
        };
        if req.get("cmd").and_then(|v| v.as_str()) == Some("voices") {
            let _ = writeln!(
                out,
                "{}",
                serde_json::json!({"ok": true, "voices": engine.list_voices()})
            );
            continue;
        }
        if req.get("cmd").and_then(|v| v.as_str()) == Some("quit") {
            break;
        }
        if req.get("cmd").and_then(|v| v.as_str()) == Some("wake_reset") {
            if let Some(w) = waker.as_mut() {
                w.reset();
            }
            let _ = writeln!(out, "{}", serde_json::json!({"ok": true}));
            continue;
        }
        if req.get("cmd").and_then(|v| v.as_str()) == Some("wake") {
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let rep = (|| -> Result<serde_json::Value, String> {
                let file = req
                    .get("model_file")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty() && !s.contains(['/', '\\']))
                    .unwrap_or("hey_jarvis_v0.1.onnx")
                    .to_string();
                ensure_waker(&mut waker, &file)?;
                let w = waker.as_mut().unwrap();
                if req.get("reset").and_then(|v| v.as_bool()).unwrap_or(false) {
                    w.reset();
                }
                let b64 = req.get("pcm_b64").and_then(|v| v.as_str()).unwrap_or("");
                if !b64.is_empty() {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|e| format!("pcm decode: {e}"))?;
                    if raw.len() % 2 != 0 {
                        return Err("pcm_b64 length must be even (i16le)".into());
                    }
                    let f: Vec<f32> = raw
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                        .collect();
                    w.push(&f);
                }
                let score = w.score()?;
                Ok(
                    serde_json::json!({"id": id, "ok": true, "score": score, "buffered": w.buffered()}),
                )
            })();
            match rep {
                Ok(v) => {
                    let _ = writeln!(out, "{v}");
                }
                Err(e) => {
                    let _ = writeln!(
                        out,
                        "{}",
                        serde_json::json!({"id": id, "ok": false, "error": e})
                    );
                }
            }
            continue;
        }
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let text = req
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let voice = req
            .get("voice")
            .and_then(|v| v.as_str())
            .unwrap_or("bm_lewis")
            .to_string();
        let speed = req.get("speed").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        if text.trim().is_empty() {
            let _ = writeln!(
                out,
                "{}",
                serde_json::json!({"id": id, "ok": false, "error": "empty text"})
            );
            continue;
        }
        let params = KokoroInferenceParams {
            voice,
            speed,
            style_index: None,
        };
        match engine.synthesize(&text, Some(params)) {
            Ok(res) => {
                let bytes: Vec<u8> = res
                    .samples
                    .iter()
                    .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
                    .collect();
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::json!({
                        "id": id, "ok": true, "rate": res.sample_rate,
                        "audio_s": res.samples.len() as f64 / res.sample_rate as f64,
                        "pcm_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    })
                );
            }
            Err(e) => {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::json!({"id": id, "ok": false, "error": format!("synth: {e}")})
                );
            }
        }
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("-V") | Some("--version") => {
            println!("{NAME} {VERSION}");
            0
        }
        Some("voices") => {
            let mut dir_flag: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--dir" && i + 1 < args.len() {
                    dir_flag = Some(args[i + 1].clone());
                    i += 1;
                }
                i += 1;
            }
            cmd_voices(&model_dir(dir_flag.as_deref()))
        }
        Some("synth") => {
            let text = args.get(1).cloned().unwrap_or_default();
            if text.is_empty() {
                eprintln!("usage: {NAME} synth <text> --voice V [--dir D] [--out F] [--speed S]");
                1
            } else {
                let (mut voice, mut dir_flag, mut out, mut speed) =
                    ("bm_lewis".to_string(), None, "out.wav".to_string(), 1.0f32);
                let mut i = 2;
                while i < args.len() {
                    match args[i].as_str() {
                        "--voice" if i + 1 < args.len() => {
                            voice = args[i + 1].clone();
                            i += 1;
                        }
                        "--dir" if i + 1 < args.len() => {
                            dir_flag = Some(args[i + 1].clone());
                            i += 1;
                        }
                        "--out" if i + 1 < args.len() => {
                            out = args[i + 1].clone();
                            i += 1;
                        }
                        "--speed" if i + 1 < args.len() => {
                            speed = args[i + 1].parse().unwrap_or(1.0);
                            i += 1;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                let dir = model_dir(dir_flag.as_deref());
                cmd_synth(&text, &voice, &dir, &out, speed)
            }
        }
        Some("serve") => {
            let mut dir_flag: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--dir" && i + 1 < args.len() {
                    dir_flag = Some(args[i + 1].clone());
                    i += 1;
                }
                i += 1;
            }
            cmd_serve(&model_dir(dir_flag.as_deref()))
        }
        _ => {
            println!("{NAME} {VERSION} — Phase 4 TTS spike (tts-rs Kokoro)");
            println!("  voices [--dir D] | synth <text> --voice V [--dir D] [--out F] [--speed S] | serve [--dir D]");
            0
        }
    };
    std::process::exit(code);
}
