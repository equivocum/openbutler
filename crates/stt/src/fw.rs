// fw.rs — port of faster-whisper 1.2.1's decode loop
// (WhisperModel.generate_segments + generate_with_fallback +
// _split_segments_by_timestamps) against ct2rs::sys.
//
// Scope mirrors exactly what ears.transcribe() uses:
// temperatures=(0.0,) single attempt, language pinned, no word timestamps,
// no VAD/clip/hotword/prefix/initial-prompt options, condition_on_previous_text
// with prompt_reset inert (0.0 > 0.5 is false). Multi-window audio works;
// previous-text conditioning crosses windows as re-tokenized strings
// (single-window voice utterances never touch that path).
// Deliberate omissions (documented, not oversights):
// - language=None detection: sys::DetectionResult fields are private, so
//   there is nothing to read the detected tag from; pin --lang.
// - compression-ratio/logprob fallback SELECTION: single-temperature loop
//   always keeps its one candidate; metrics are still computed for the
//   no-speech skip, which IS implemented (0.6 / -1.0, same as upstream).

use ct2rs::sys;
use ct2rs::Tokenizer as _; // trait decode(Vec<String>) for id slices
use ndarray::{s, Array1, Array2, Array3, Axis};
use rustfft::num_complex::Complex;
use std::path::Path;
use std::sync::Arc;

const FPS_INV: f64 = 0.01; // seconds per mel frame
const TIME_PRECISION: f64 = 0.02; // seconds per timestamp token
const INPUT_STRIDE: i64 = 2; // seek units per timestamp unit
const PREV_TOKENS_MAX: usize = 223; // max_length // 2 - 1

pub struct FwSegment {
    pub id: usize,
    pub seek: i64,
    pub start: f32,
    pub end: f32,
    pub text: String,
    pub temperature: f32,
    pub avg_logprob: f32,
    pub no_speech_prob: f32,
}

struct Pre {
    n_fft: usize,
    hop: usize,
    feature_size: usize,
    nb_max_frames: usize,
    mel_filters: Array2<f64>,
}

pub struct FwEngine {
    model: sys::Whisper,
    tok: ct2rs::tokenizers::hf::Tokenizer,
    timestamp_begin: usize,
    eot: usize,
    pre: Pre,
    fft: Arc<dyn rustfft::Fft<f64>>,
    hann: Vec<f64>,
}

fn find_fallback_preprocessor() -> Option<std::path::PathBuf> {
    // faster-whisper ignores preprocessor_config.json, so most CT2 dirs
    // lack it — but large-v3 ships one and openai/whisper-medium snapshots
    // may have had one copied in manually. Reuse any cached copy rather
    // than failing on a fresh download.
    let hub = crate::hf_hub();
    let mut hits: Vec<std::path::PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&hub).ok()?;
    for e in entries.filter_map(|x| x.ok()) {
        let snap = e.path().join("snapshots");
        let subs = std::fs::read_dir(&snap).ok()?;
        for s in subs.filter_map(|x| x.ok()) {
            let cand = s.path().join("preprocessor_config.json");
            if cand.is_file() {
                hits.push(cand);
            }
        }
    }
    hits.sort();
    // Prefer a copy that already carries mel_filters (large-v3 style).
    for h in &hits {
        if let Ok(t) = std::fs::read_to_string(h) {
            if t.contains("mel_filters") {
                return Some(h.clone());
            }
        }
    }
    hits.into_iter().next()
}

fn load_preprocessor(dir: &Path) -> Result<Pre, String> {
    let cfg_path = dir.join("preprocessor_config.json");
    if !cfg_path.is_file() {
        if let Some(src) = find_fallback_preprocessor() {
            if std::fs::copy(&src, &cfg_path).is_ok() {
                eprintln!(
                    "[sstt] copied preprocessor_config.json from {}",
                    src.display()
                );
            }
        }
    }
    let t = std::fs::read_to_string(&cfg_path).map_err(|e| {
        format!(
            "preprocessor_config.json: {e} (copy one from openai/whisper-medium into {})",
            dir.display()
        )
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&t).map_err(|e| format!("preprocessor_config.json: {e}"))?;
    let u = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_u64())
            .map(|x| x as usize)
            .ok_or_else(|| format!("preprocessor_config.json: missing {k}"))
    };
    let rows: Vec<Vec<f64>> = v
        .get("mel_filters")
        .and_then(|x| serde_json::from_value(x.clone()).ok())
        .ok_or_else(|| "preprocessor_config.json: missing mel_filters".to_string())?;
    let cols = rows.first().map(|r| r.len()).unwrap_or(0);
    let flat: Vec<f64> = rows.into_iter().flatten().collect();
    let mel_filters = Array2::from_shape_vec((flat.len() / cols.max(1), cols), flat)
        .map_err(|e| format!("mel_filters shape: {e}"))?;
    Ok(Pre {
        n_fft: u("n_fft")?,
        hop: u("hop_length")?,
        feature_size: u("feature_size")?,
        nb_max_frames: u("nb_max_frames")?,
        mel_filters,
    })
}

impl FwEngine {
    pub fn load(dir: &Path) -> Result<Self, String> {
        // Same compute fallback as the plain path (int8 kernels need oneDNN).
        let mut cfg = ct2rs::Config::default();
        cfg.device = ct2rs::Device::CPU;
        cfg.compute_type = ct2rs::ComputeType::INT8;
        let model = match sys::Whisper::new(dir, cfg) {
            Ok(m) => m,
            Err(e) if format!("{e:?}").contains("int8") => {
                let mut cfg = ct2rs::Config::default();
                cfg.device = ct2rs::Device::CPU;
                cfg.compute_type = ct2rs::ComputeType::FLOAT32;
                sys::Whisper::new(dir, cfg).map_err(|e| format!("load: {e:?}"))?
            }
            Err(e) => return Err(format!("load: {e:?}")),
        };
        let mut tok =
            ct2rs::tokenizers::hf::Tokenizer::new(dir).map_err(|e| format!("tokenizer: {e:?}"))?;
        // Timestamp ids form a contiguous range ABOVE <|notimestamps|> and
        // are not addressable as "<|0.00|>" strings in this vocab — same
        // derivation as faster-whisper (timestamp_begin = no_timestamps + 1).
        // Likewise decode() must filter ids >= eot, timestamp tokens included.
        let timestamp_begin = tok
            .inner()
            .token_to_id("<|notimestamps|>")
            .ok_or_else(|| "tokenizer lacks <|notimestamps|>".to_string())?
            as usize
            + 1;
        let eot = tok
            .inner()
            .token_to_id("<|endoftext|>")
            .ok_or_else(|| "tokenizer lacks <|endoftext|>".to_string())? as usize;
        let pre = load_preprocessor(dir)?;
        // Periodic Hann(400), matching np.hanning(401)[:-1] (float64 here;
        // float32 there — rounding-level only).
        let hann: Vec<f64> = (0..pre.n_fft)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / pre.n_fft as f64).cos()))
            .collect();
        let fft = rustfft::FftPlanner::new().plan_fft_forward(pre.n_fft);
        Ok(FwEngine {
            model,
            tok,
            timestamp_begin,
            eot,
            pre,
            fft,
            hann,
        })
    }

    fn id_to_token(&mut self, id: usize) -> Option<String> {
        self.tok
            .inner()
            .id_to_token(id as u32)
            .map(|s| s.to_string())
    }

    fn decode_ids(&mut self, ids: &[usize]) -> String {
        // Mirror faster-whisper Tokenizer.decode: drop ids >= eot
        // (timestamp + special tokens) before decoding.
        let text_ids: Vec<usize> = ids.iter().cloned().filter(|i| *i < self.eot).collect();
        let toks: Vec<String> = text_ids
            .iter()
            .filter_map(|i| self.id_to_token(*i))
            .collect();
        self.tok.decode(toks).unwrap_or_default()
    }

    /// Full-audio mel frontend, numerically mirroring
    /// faster-whisper's FeatureExtractor.__call__ (which mirrors openai):
    /// +160 zero tail pad, centered reflect-200 STFT (periodic Hann-400,
    /// rfft, |.|^2), filter bank, log10(1e-10 floor), [..., :-1] last-frame
    /// drop, then GLOBAL max-8 clamp and (+4)/4 over the whole utterance.
    /// (An earlier revision used mel_spec's streaming STFT + per-frame norm;
    /// both differ — no centering, per-column norm, zeroed Nyquist bin —
    /// and decoded measurably worse. This version matches column-for-column.)
    fn mel_all(&self, samples: &[f32]) -> Array2<f32> {
        let l = samples.len();
        if l == 0 {
            return Array2::zeros((self.pre.feature_size, 0));
        }
        // Base signal: samples + 160 tail zeros (__call__ padding default).
        let mut base = Vec::with_capacity(l + 160);
        base.extend_from_slice(samples);
        base.extend(std::iter::repeat(0.0).take(160));
        let bl = base.len();
        // Center reflect-200 both sides (stft center=True, mode=reflect).
        // Clamped indices: exact for bl > 200, graceful below (numpy would
        // raise there; short clips degrade instead of crashing).
        let mut padded = Vec::with_capacity(bl + 400);
        for k in 0..200 {
            padded.push(base[(200 - k).min(bl - 1)]);
        }
        padded.extend_from_slice(&base);
        for k in 0..200 {
            padded.push(base[(bl - 2).saturating_sub(k).min(bl - 1)]);
        }
        // n_stft windows, then drop the last ([..., :-1]).
        let n_stft = 1 + (l + 160) / 160;
        let mut cols: Vec<Vec<f64>> = Vec::with_capacity(n_stft);
        let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); self.pre.n_fft];
        for k in 0..n_stft {
            let start = k * self.pre.hop;
            for i in 0..self.pre.n_fft {
                let s = padded.get(start + i).copied().unwrap_or(0.0) as f64;
                buf[i] = Complex::new(s * self.hann[i], 0.0);
            }
            self.fft.process(&mut buf);
            // Onesided bins 0..=200 kept with real energy (Nyquist included).
            let mags = Array1::from_vec(buf[..201].iter().map(|v| v.norm_sqr()).collect());
            let col = self
                .pre
                .mel_filters
                .dot(&mags)
                .mapv(|x| x.max(1e-10).log10());
            cols.push(col.to_vec());
        }
        cols.pop(); // [..., :-1]
        let f = cols.len();
        let flat: Vec<f64> = cols.into_iter().flatten().collect();
        // (frames, 80) -> transpose to (80, frames), then global norm.
        let a = Array2::from_shape_vec((f, self.pre.feature_size), flat)
            .unwrap_or_else(|_| Array2::zeros((0, self.pre.feature_size)))
            .reversed_axes();
        let mmax = a.fold(f64::NEG_INFINITY, |m, &x| m.max(x));
        a.mapv(|x| ((x.max(mmax - 8.0) + 4.0) / 4.0) as f32)
            .as_standard_layout()
            .into_owned()
    }

    /// Frontend probe for parity debugging: shape + stats + sample columns.
    pub fn mel_stats(
        &self,
        samples: &[f32],
    ) -> (usize, usize, f64, f64, f32, f32, Vec<f32>, Vec<f32>) {
        let m = self.mel_all(samples);
        let (r, c) = (m.nrows(), m.ncols());
        let n = (r * c) as f64;
        let sum: f64 = m.iter().map(|&x| x as f64).sum();
        let mean = sum / n;
        let var: f64 = m.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / n;
        let min = m.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = m.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let col = |j: usize| m.column(j.min(c.saturating_sub(1))).to_vec();
        (r, c, mean, var.sqrt(), min, max, col(0), col(1))
    }

    fn window_features(&self, feats: &Array2<f32>, seek: i64, segment_size: i64) -> Array3<f32> {
        let mut win = Array2::<f32>::zeros((self.pre.feature_size, self.pre.nb_max_frames));
        let avail = (feats.ncols() as i64 - seek).max(0).min(segment_size) as usize;
        if avail > 0 {
            let s = seek as usize;
            win.slice_mut(s![.., 0..avail])
                .assign(&feats.slice(s![.., s..s + avail]));
        }
        // (1, 80, 3000), standard layout for the StorageView borrow.
        let mut w3 = win.insert_axis(Axis(0));
        if !w3.is_standard_layout() {
            w3 = w3.as_standard_layout().into_owned();
        }
        w3
    }

    fn build_prompt(&mut self, previous: &[usize], lang_token: &str) -> Vec<String> {
        // Mirror Tokenizer.sot_sequence: English-only models (.en) take a
        // bare [<|startoftranscript|>]; language+task tokens exist only on
        // multilingual models. (Sending <|en|> to a .en model degrades the
        // decode — found by bench diff, verified against tokenizer.py.)
        let multilingual = self.model.is_multilingual();
        let mut p = Vec::new();
        if !previous.is_empty() {
            p.push("<|startofprev|>".to_string());
            let tail = if previous.len() > PREV_TOKENS_MAX {
                &previous[previous.len() - PREV_TOKENS_MAX..]
            } else {
                previous
            };
            for id in tail {
                if let Some(t) = self.id_to_token(*id) {
                    p.push(t);
                }
            }
        }
        p.push("<|startoftranscript|>".to_string());
        if multilingual {
            p.push(lang_token.to_string());
            p.push("<|transcribe|>".to_string());
        }
        p
    }

    #[allow(clippy::too_many_arguments)]
    fn split_by_timestamps(
        &self,
        tokens: &[usize],
        time_offset: f64,
        segment_size: i64,
        segment_duration: f64,
        seek: i64,
    ) -> (Vec<RawSeg>, i64, bool) {
        let tb = self.timestamp_begin;
        let n = tokens.len();
        let single_end = n >= 2 && tokens[n - 2] < tb && tb <= tokens[n - 1];
        let consecutive: Vec<usize> = (1..n)
            .filter(|&i| tokens[i] >= tb && tokens[i - 1] >= tb)
            .collect();
        if !consecutive.is_empty() {
            let mut slices = consecutive;
            if single_end {
                slices.push(n);
            }
            let mut segs = Vec::new();
            let mut last_slice = 0;
            let mut sk = seek;
            for cs in slices {
                let sliced = &tokens[last_slice..cs];
                let sp = (sliced[0] - tb) as f64;
                let ep = (sliced[sliced.len() - 1] - tb) as f64;
                segs.push(RawSeg {
                    start: time_offset + sp * TIME_PRECISION,
                    end: time_offset + ep * TIME_PRECISION,
                    tokens: sliced.to_vec(),
                });
                last_slice = cs;
            }
            if single_end {
                sk += segment_size;
            } else {
                sk += ((tokens[last_slice - 1] - tb) as i64) * INPUT_STRIDE;
            }
            (segs, sk, single_end)
        } else {
            let mut duration = segment_duration;
            let ts: Vec<usize> = tokens.iter().cloned().filter(|&t| t >= tb).collect();
            if let Some(&last) = ts.last() {
                if last != tb {
                    duration = ((last - tb) as f64) * TIME_PRECISION;
                }
            }
            (
                vec![RawSeg {
                    start: time_offset,
                    end: time_offset + duration,
                    tokens: tokens.to_vec(),
                }],
                seek + segment_size,
                single_end,
            )
        }
    }

    /// Transcribe 16kHz float samples (language pinned, e.g. "en").
    /// Returns segments with text, bounds, avg logprob and no-speech prob —
    /// the exact fields ears.transcribe() consumes.
    pub fn transcribe(&mut self, samples: &[f32], lang: &str) -> Result<Vec<FwSegment>, String> {
        if samples.is_empty() {
            return Ok(vec![]);
        }
        let feats = self.mel_all(samples);
        let content_frames = feats.ncols() as i64 - 1;
        if content_frames <= 0 {
            return Ok(vec![]);
        }
        let lang_token = format!("<|{lang}|>");
        let mut opts = ct2rs::WhisperOptions::default();
        opts.return_scores = true;
        opts.return_no_speech_prob = true;

        let mut seek: i64 = 0;
        let mut all_tokens: Vec<usize> = Vec::new();
        let mut idx = 0usize;
        let mut out = Vec::new();
        while seek < content_frames {
            let time_offset = seek as f64 * FPS_INV;
            let segment_size = (self.pre.nb_max_frames as i64).min(content_frames - seek);
            let segment_duration = segment_size as f64 * FPS_INV;

            let w3 = self.window_features(&feats, seek, segment_size);
            let shape = w3.shape().to_vec();
            let mut flat = w3
                .as_standard_layout()
                .into_owned()
                .into_raw_vec_and_offset()
                .0;
            let storage = sys::StorageView::new(&shape, flat.as_mut_slice(), ct2rs::Device::CPU)
                .map_err(|e| format!("storage: {e:?}"))?;
            let encoder = self
                .model
                .encode(&storage, false)
                .map_err(|e| format!("encode: {e:?}"))?;

            let prompt = self.build_prompt(&all_tokens, &lang_token);
            let res = self
                .model
                .generate(&encoder, &[prompt], &opts)
                .map_err(|e| format!("generate: {e:?}"))?;
            let r = res.into_iter().next().ok_or("empty generation")?;
            let tokens: Vec<usize> = r.sequences_ids.into_iter().next().unwrap_or_default();
            let seq_len = tokens.len();
            let avg_logprob = if seq_len == 0 {
                0.0
            } else {
                r.scores.first().copied().unwrap_or(0.0) * (seq_len as f32) / (seq_len as f32 + 1.0)
            };
            let no_speech = r.no_speech_prob;

            // No-voice-activity skip (no_speech_threshold=0.6,
            // log_prob_threshold=-1.0): drop the window without yielding.
            if no_speech > 0.6 && !(avg_logprob > -1.0) {
                seek += segment_size;
                continue;
            }

            let previous_seek = seek;
            let (current, new_seek, _) = self.split_by_timestamps(
                &tokens,
                time_offset,
                segment_size,
                segment_duration,
                seek,
            );
            seek = new_seek;
            for seg in current {
                let text = self.decode_ids(&seg.tokens);
                if seg.start == seg.end || text.trim().is_empty() {
                    continue;
                }
                all_tokens.extend(seg.tokens.iter().cloned());
                idx += 1;
                out.push(FwSegment {
                    id: idx,
                    seek: previous_seek,
                    start: seg.start as f32,
                    end: seg.end as f32,
                    text,
                    temperature: 0.0,
                    avg_logprob,
                    no_speech_prob: no_speech,
                });
            }
            // prompt_reset inert here (condition=true, 0.0 > 0.5 false).
        }
        Ok(out)
    }
}

struct RawSeg {
    start: f64,
    end: f64,
    tokens: Vec<usize>,
}
