// openbutler-stt library surface: the faster-whisper decode port (fw.rs)
// shared by the `transcribe-fw` spike and the voice orchestrator.
pub mod fw;

use std::path::{Path, PathBuf};

/// HuggingFace hub cache root (HF_HUB_CACHE or ~/.cache/huggingface/hub).
pub fn hf_hub() -> PathBuf {
    if let Ok(c) = std::env::var("HF_HUB_CACHE") {
        return PathBuf::from(c);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/huggingface/hub")
}

fn snapshots_of(model_dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(model_dir.join("snapshots"))
        .map(|r| r.filter_map(|e| e.ok().map(|x| x.path())).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Resolve a model spec (CT2 dir path or faster-whisper name like
/// `medium.en`) to its snapshot dir, mirroring the spike CLI.
pub fn resolve_model(spec: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(spec);
    if p.join("model.bin").is_file() {
        return Ok(p);
    }
    let dir = hf_hub().join(format!("models--Systran--faster-whisper-{spec}"));
    snapshots_of(&dir).into_iter().next_back().ok_or_else(|| {
        format!(
            "model '{spec}' not found (no snapshot under {})",
            dir.display()
        )
    })
}
