//! Vendored Kokoro TTS engine (adapted from `tts-rs` 2026.2.3, MIT,
//! Copyright rishiskhare and contributors), patched for ort 2.0.0-rc.13
//! (provider rename + typed builder errors). Upstream tracks ort loosely
//! (`^2.0.0-rc.10`) and does not compile against either rc.10 (missing
//! ViewRepr tensor impls) or rc.13 (renamed APIs) — hence the vendoring.
//! Only `ort`/`ndarray` API-shape lines in `engines/kokoro/model.rs` differ
//! from upstream; phonemizer/vocab/voices/engine are verbatim.

pub mod engines;

use std::path::Path;

/// The result of a synthesis (text-to-speech) operation.
///
/// Contains raw f32 audio samples and the sample rate of the output audio.
#[derive(Debug)]
pub struct SynthesisResult {
    /// Raw audio samples as f32 values
    pub samples: Vec<f32>,
    /// Sample rate of the audio (24000 for Kokoro)
    pub sample_rate: u32,
}

/// Common interface for text-to-speech synthesis engines.
pub trait SynthesisEngine {
    /// Parameters for configuring inference behavior (voice, speed, etc.)
    type SynthesisParams;
    /// Parameters for configuring model loading (threads, etc.)
    type ModelParams: Default;

    /// Load a model from the specified path using default parameters.
    fn load_model(&mut self, model_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        self.load_model_with_params(model_path, Self::ModelParams::default())
    }

    /// Load a model from the specified path with custom parameters.
    fn load_model_with_params(
        &mut self,
        model_path: &Path,
        params: Self::ModelParams,
    ) -> Result<(), Box<dyn std::error::Error>>;

    /// Unload the currently loaded model and free associated resources.
    fn unload_model(&mut self);

    /// Synthesize speech from the given text.
    fn synthesize(
        &mut self,
        text: &str,
        params: Option<Self::SynthesisParams>,
    ) -> Result<SynthesisResult, Box<dyn std::error::Error>>;
}
