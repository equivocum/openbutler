// wake.rs — openWakeWord scorer inside the tts serve child.
//
// Same ort process as Kokoro (the voice binary cannot link ort: the
// ort+sentencepiece protobuf-lite clash is what forced TTS into this
// child in the first place). Three tiny graphs, run per 80ms tick over
// trailing 2s of 16kHz mono float audio:
//
//   melspectrogram.onnx   [1, samples] -> [1, 1, ~197, 32]
//   embedding_model.onnx  [16, 76, 32, 1] -> [16, 1, 1, 96]
//   hey_jarvis_v0.1.onnx  [1, 16, 96] -> [1, 1]  (score 0..1)
//
// Windows: 16 embeddings from 76-mel-frame windows stepped 8 frames
// apart (76 + 15*8 = 196 frames ≈ 2s). Sessions take &mut on run in
// this ort version, so every method here is &mut.

use std::path::Path;

use ndarray::{Array2, Array3, Array4};
use ort::ep::CPU;
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;

const CTX_SAMPLES: usize = 32000; // 2s trailing context @16k
const MEL_FRAMES_PER_CTX: usize = 197;
const WIN_FRAMES: usize = 76;
const WIN_STEP: usize = 8;
const N_WIN: usize = 16;
const MEL_BANDS: usize = 32;
const EMB_DIM: usize = 96;

/// Minimum buffered audio before scoring (one 80ms tick).
pub const MIN_SAMPLES: usize = 1280;

pub struct WakeScorer {
    mel: Session,
    emb: Session,
    clf: Session,
    pcm: Vec<f32>,
}

fn open_session(path: &Path) -> Result<Session, String> {
    Session::builder()
        .map_err(|e| format!("wake ort builder: {e:?}"))?
        .with_optimization_level(GraphOptimizationLevel::Disable)
        .map_err(|e| format!("wake ort opt: {e:?}"))?
        .with_execution_providers(vec![CPU::default().build()])
        .map_err(|e| format!("wake ort providers: {e:?}"))?
        .commit_from_file(path)
        .map_err(|e| format!("wake load {}: {e:?}", path.display()))
}

impl WakeScorer {
    /// Load the three graphs from dir with the given classifier file.
    /// Kept lazy by callers so TTS-only startup pays nothing.
    pub fn load(dir: &Path, clf_file: &str) -> Result<Self, String> {
        for f in ["melspectrogram.onnx", "embedding_model.onnx", clf_file] {
            if !dir.join(f).is_file() {
                return Err(format!(
                    "wake model missing: {} (see models/wake.json)",
                    dir.join(f).display()
                ));
            }
        }
        Ok(Self {
            mel: open_session(&dir.join("melspectrogram.onnx"))?,
            emb: open_session(&dir.join("embedding_model.onnx"))?,
            clf: open_session(&dir.join(clf_file))?,
            pcm: Vec::with_capacity(CTX_SAMPLES * 2),
        })
    }

    /// Append 16kHz mono float samples ([-1, 1]); keeps trailing 2s.
    pub fn push(&mut self, samples: &[f32]) {
        self.pcm.extend_from_slice(samples);
        if self.pcm.len() > CTX_SAMPLES {
            let drop = self.pcm.len() - CTX_SAMPLES;
            self.pcm.drain(..drop);
        }
    }

    /// Clear the buffer (call after a detection, like upstream reset()).
    pub fn reset(&mut self) {
        self.pcm.clear();
    }

    pub fn buffered(&self) -> usize {
        self.pcm.len()
    }

    /// Score trailing context 0..1. Short buffers left-pad with zeros;
    /// under one tick returns 0.0 without running the graphs.
    ///
    /// Scaling follows openWakeWord's AudioFeatures exactly — the mel
    /// graph takes RAW int16-range floats (no /32768) and its output is
    /// transformed with `x/10 + 2` before windowing (`utils.py`
    /// `_get_melspectrogram`, default `melspec_transform`). Both were
    /// wrong in the first cut (near-zero scores on real speech).
    pub fn score(&mut self) -> Result<f32, String> {
        if self.pcm.len() < MIN_SAMPLES {
            return Ok(0.0);
        }
        let mut ctx = vec![0.0f32; CTX_SAMPLES.saturating_sub(self.pcm.len())];
        let keep = self.pcm.len().min(CTX_SAMPLES);
        ctx.extend_from_slice(&self.pcm[self.pcm.len() - keep..]);
        // Undo the [-1, 1] convention: the mel graph wants int16 range.
        for v in ctx.iter_mut() {
            *v *= 32768.0;
        }
        let arr = Array2::from_shape_vec((1, CTX_SAMPLES), ctx)
            .map_err(|e| format!("wake ctx shape: {e}"))?;
        let mel_frames: Vec<f32> = {
            let out = self
                .mel
                .run(inputs!["input" => TensorRef::from_array_view(arr.view()).map_err(|e| format!("wake mel input: {e:?}"))?])
                .map_err(|e| format!("wake mel run: {e:?}"))?;
            let (_, m) = out["output"]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("wake mel out: {e:?}"))?;
            m.to_vec()
        };
        // Layout [1, 1, T, 32] -> frame t, band b at t*32+b, with the
        // upstream log-mel transform applied. Take the trailing
        // windows; tolerate short outputs by clamping.
        let frames = mel_frames.len() / MEL_BANDS;
        let span = WIN_FRAMES + (N_WIN - 1) * WIN_STEP;
        let t0 = frames.saturating_sub(span);
        let mut win = Vec::with_capacity(N_WIN * WIN_FRAMES * MEL_BANDS);
        for i in 0..N_WIN {
            for r in 0..WIN_FRAMES {
                let t = (t0 + i * WIN_STEP + r).min(frames.saturating_sub(1));
                for b in 0..MEL_BANDS {
                    win.push(mel_frames[t * MEL_BANDS + b] / 10.0 + 2.0);
                }
            }
        }
        let warr = Array4::from_shape_vec((N_WIN, WIN_FRAMES, MEL_BANDS, 1), win)
            .map_err(|e| format!("wake win shape: {e}"))?;
        let embs: Vec<f32> = {
            let eout = self
                .emb
                .run(inputs!["input_1" => TensorRef::from_array_view(warr.view()).map_err(|e| format!("wake emb input: {e:?}"))?])
                .map_err(|e| format!("wake emb run: {e:?}"))?;
            let (_, e) = eout["conv2d_19"]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("wake emb out: {e:?}"))?;
            e.to_vec()
        };
        // [16, 1, 1, 96] flat is already 16*96 in window order.
        let carr = Array3::from_shape_vec((1, N_WIN, EMB_DIM), embs)
            .map_err(|e| format!("wake clf shape: {e}"))?;
        let score = {
            let cout = self
                .clf
                .run(inputs!["x.1" => TensorRef::from_array_view(carr.view()).map_err(|e| format!("wake clf input: {e:?}"))?])
                .map_err(|e| format!("wake clf run: {e:?}"))?;
            let (_, s) = cout["53"]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("wake clf out: {e:?}"))?;
            s.first().cloned().unwrap_or(0.0)
        };
        Ok(score.clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wake_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(".cache/jarvis/wake")
    }

    #[test]
    fn missing_dir_errors() {
        let r = WakeScorer::load(
            std::path::Path::new("/nonexistent-wake-dir-xyz"),
            "hey_jarvis_v0.1.onnx",
        );
        assert!(r.is_err());
    }

    #[test]
    fn silence_scores_near_zero() {
        let dir = wake_dir();
        if !dir.join("hey_jarvis_v0.1.onnx").is_file() {
            eprintln!("wake models absent, skipping");
            return;
        }
        let mut w = WakeScorer::load(&dir, "hey_jarvis_v0.1.onnx").unwrap();
        assert_eq!(w.score().unwrap(), 0.0); // empty buffer, no graphs run
        w.push(&vec![0.0f32; 32000]); // 2s digital silence
        let s = w.score().unwrap();
        assert!(s < 0.2, "silence scored {s}");
        assert_eq!(w.buffered(), 32000);
        w.push(&vec![0.0f32; 16000]); // truncation keeps trailing 2s
        assert_eq!(w.buffered(), 32000);
        w.reset();
        assert_eq!(w.buffered(), 0);
        assert_eq!(w.score().unwrap(), 0.0);
    }
}
