// openbutler-stt — STT engine: CTranslate2 Whisper via ct2rs,
// loading the SAME Systran CT2 dirs faster-whisper uses (no conversion).
//   models                                  list cached faster-whisper models
//   transcribe <wav> [--model M] [--lang L] 16-bit mono wav -> JSON transcript
// Mirrors ears.transcribe(): temperature deterministic, language pinned.

use std::path::Path;
use std::time::Instant;

use openbutler_stt::{fw, hf_hub, resolve_model};

const NAME: &str = "openbutler-stt";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn cmd_models() -> i32 {
    let hub = hf_hub();
    let mut found = 0;
    if let Ok(rd) = std::fs::read_dir(&hub) {
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("models--Systran--faster-whisper-"))
            .collect();
        names.sort();
        for n in names {
            let short = n.trim_start_matches("models--Systran--faster-whisper-");
            match resolve_model(short) {
                Ok(p) => {
                    println!("{short} -> {}", p.display());
                    found += 1;
                }
                Err(e) => println!("{short}: {e}"),
            }
        }
    }
    if found == 0 {
        println!("no cached faster-whisper models under {}", hub.display());
        return 1;
    }
    0
}

fn read_wav_mono16(path: &Path) -> Result<Vec<f32>, String> {
    let mut r = hound::WavReader::open(path).map_err(|e| format!("wav open: {e}"))?;
    let spec = r.spec();
    if spec.sample_rate != 16000 || spec.channels != 1 {
        return Err(format!(
            "want 16kHz mono, got {}Hz {}ch (convert: ffmpeg -i in.wav -ac 1 -ar 16000 out.wav)",
            spec.sample_rate, spec.channels
        ));
    }
    match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| {
                s.map(|v| v as f32 / 32768.0)
                    .map_err(|e| format!("wav: {e}"))
            })
            .collect(),
        hound::SampleFormat::Float => r
            .samples::<f32>()
            .map(|s| s.map_err(|e| format!("wav: {e}")))
            .collect(),
    }
}

fn cmd_transcribe(wav: &str, model_spec: &str, lang: &str, plain: bool) -> i32 {
    let dir = match resolve_model(model_spec) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let samples = match read_wav_mono16(Path::new(wav)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    eprintln!("{NAME}: loading {} ...", dir.display());
    let t0 = Instant::now();
    // Mirror faster-whisper: device cpu, compute int8 (config stt_device auto->cpu here, stt_compute int8).
    // The vendored CTranslate2 build lacks int8 CPU kernels on this VNNI-only
    // box (pip's wheel bundles oneDNN; ours doesn't — see Phase-5 note to try
    // the `dnnl` feature). Fall back to float32: same weights, same decode,
    // so the transcript comparison stays valid; only speed differs.
    let mut cfg = ct2rs::Config::default();
    cfg.device = ct2rs::Device::CPU;
    cfg.compute_type = ct2rs::ComputeType::INT8;
    let whisper = match ct2rs::Whisper::new(&dir, cfg) {
        Ok(w) => w,
        Err(e) if format!("{e:?}").contains("int8") => {
            eprintln!("{NAME}: no int8 kernels in this build, falling back to float32 (text parity unaffected)");
            let mut cfg = ct2rs::Config::default();
            cfg.device = ct2rs::Device::CPU;
            cfg.compute_type = ct2rs::ComputeType::FLOAT32;
            match ct2rs::Whisper::new(&dir, cfg) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("{NAME}: load failed: {e}");
                    return 1;
                }
            }
        }
        Err(e) => {
            eprintln!("{NAME}: load failed: {e}");
            return 1;
        }
    };
    let load_ms = t0.elapsed().as_millis();
    let mut opts = ct2rs::WhisperOptions::default();
    opts.return_no_speech_prob = true;
    let t1 = Instant::now();
    // Segment-level decode with DTW word timings. NOTE: this path does not
    // surface CTranslate2's no_speech_prob (only sys::Whisper.generate does,
    // which needs faster-whisper's per-chunk prompt loop around it).
    // Sourcing that score is tracked Phase-5 engine work; the spike asserts
    // text + latency parity, which is the engine question.
    let segs = if plain {
        whisper
            .generate(&samples, Some(lang), false, &opts)
            .map(|texts| {
                texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| ct2rs::Segment {
                        id: i,
                        text,
                        start: 0.0,
                        end: 0.0,
                        words: None,
                    })
                    .collect::<Vec<_>>()
            })
    } else {
        whisper.generate_segments(&samples, Some(lang), &opts)
    };
    let segs = match segs {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{NAME}: transcribe failed: {e}");
            return 1;
        }
    };
    let infer_ms = t1.elapsed().as_millis();
    let mut text = String::new();
    let out_segs: Vec<serde_json::Value> = segs
        .iter()
        .map(|s| {
            text.push_str(&s.text);
            text.push(' ');
            serde_json::json!({"id": s.id, "start": s.start, "end": s.end, "text": s.text})
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "text": text.trim(),
            "segments": out_segs,
            "avg_no_speech_prob": serde_json::Value::Null,
            "audio_s": samples.len() as f64 / 16000.0,
            "load_ms": load_ms,
            "infer_ms": infer_ms,
        })
    );
    0
}

fn cmd_mel_stats(wav: &str, model_spec: &str) -> i32 {
    let dir = match resolve_model(model_spec) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let samples = match read_wav_mono16(std::path::Path::new(wav)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let fw = match fw::FwEngine::load(&dir) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let (r, c, mean, std, min, max, col0, col1) = fw.mel_stats(&samples);
    println!("shape: ({r}, {c})");
    println!("mean {mean:.6} std {std:.6} min {min:.6} max {max:.6}");
    println!(
        "col0: {}",
        col0.iter()
            .map(|x| format!("{x:.4}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "col1: {}",
        col1.iter()
            .map(|x| format!("{x:.4}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    0
}

fn cmd_transcribe_fw(wav: &str, model_spec: &str, lang: &str) -> i32 {
    let dir = match resolve_model(model_spec) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let samples = match read_wav_mono16(std::path::Path::new(wav)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    eprintln!("{NAME}: loading {} ...", dir.display());
    let t0 = std::time::Instant::now();
    let mut fw = match fw::FwEngine::load(&dir) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let load_ms = t0.elapsed().as_millis();
    let t1 = std::time::Instant::now();
    let segs = match fw.transcribe(&samples, lang) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{NAME}: transcribe failed: {e}");
            return 1;
        }
    };
    let infer_ms = t1.elapsed().as_millis();
    let mut text = String::new();
    let mut probs = Vec::new();
    let out_segs: Vec<serde_json::Value> = segs
        .iter()
        .map(|s| {
            text.push_str(&s.text);
            text.push(' ');
            probs.push(s.no_speech_prob as f64);
            serde_json::json!({"id": s.id, "start": s.start, "end": s.end,
                "text": s.text, "no_speech_prob": s.no_speech_prob,
                "avg_logprob": s.avg_logprob, "temperature": s.temperature})
        })
        .collect();
    let avg = if probs.is_empty() {
        0.0
    } else {
        probs.iter().sum::<f64>() / probs.len() as f64
    };
    println!(
        "{}",
        serde_json::json!({
            "text": text.trim(),
            "segments": out_segs,
            "avg_no_speech_prob": avg,
            "audio_s": samples.len() as f64 / 16000.0,
            "load_ms": load_ms,
            "infer_ms": infer_ms,
        })
    );
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("-V") | Some("--version") => {
            println!("{NAME} {VERSION}");
            0
        }
        Some("models") => cmd_models(),
        Some("mel-stats") => {
            let wav = args.get(1).cloned().unwrap_or_default();
            if wav.is_empty() {
                eprintln!("usage: {NAME} mel-stats <wav> [--model M]");
                1
            } else {
                let mut model = "medium.en".to_string();
                let mut i = 2;
                while i < args.len() {
                    if args[i] == "--model" && i + 1 < args.len() {
                        model = args[i + 1].clone();
                        i += 1;
                    }
                    i += 1;
                }
                cmd_mel_stats(&wav, &model)
            }
        }
        Some("transcribe") => {
            let wav = args.get(1).cloned().unwrap_or_default();
            if wav.is_empty() {
                eprintln!("usage: {NAME} transcribe <wav> [--model M] [--lang L] [--plain]");
                1
            } else {
                let mut model = "medium.en".to_string();
                let mut lang = "en".to_string();
                let mut plain = false;
                let mut i = 2;
                while i < args.len() {
                    match args[i].as_str() {
                        "--model" if i + 1 < args.len() => {
                            model = args[i + 1].clone();
                            i += 1;
                        }
                        "--lang" if i + 1 < args.len() => {
                            lang = args[i + 1].clone();
                            i += 1;
                        }
                        "--plain" => plain = true,
                        _ => {}
                    }
                    i += 1;
                }
                cmd_transcribe(&wav, &model, &lang, plain)
            }
        }
        Some("transcribe-fw") => {
            let wav = args.get(1).cloned().unwrap_or_default();
            if wav.is_empty() {
                eprintln!("usage: {NAME} transcribe-fw <wav> [--model M] [--lang L]");
                1
            } else {
                let mut model = "medium.en".to_string();
                let mut lang = "en".to_string();
                let mut i = 2;
                while i < args.len() {
                    match args[i].as_str() {
                        "--model" if i + 1 < args.len() => {
                            model = args[i + 1].clone();
                            i += 1;
                        }
                        "--lang" if i + 1 < args.len() => {
                            lang = args[i + 1].clone();
                            i += 1;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                cmd_transcribe_fw(&wav, &model, &lang)
            }
        }
        _ => {
            println!("{NAME} {VERSION} — Phase 3 STT spike (ct2rs, same CT2 weights)");
            println!("  models | transcribe <wav> [--model M] [--lang L] [--plain] | transcribe-fw <wav> [--model M] [--lang L]");
            0
        }
    };
    std::process::exit(code);
}
