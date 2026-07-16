//! jarvis-core — always-on wake-word listening, utterance endpointing, and
//! (later milestones) intent parsing, tool registry, execution, audit, kill.
//!
//! Depends on whispr-core for ASR; audio here is deliberately captured in the
//! openWakeWord convention (16 kHz mono f32 in **int16 range**, NOT ±1) and
//! converted at the boundaries: /32768 for VAD, whispr-style normalization
//! for ASR (see `mic::normalize_for_asr`).

pub mod audit;
pub mod clap_wake;
pub mod dialog;
pub mod engine;
pub mod exec;
pub mod kill;
pub mod llm;
pub mod mic;
pub mod registry;
pub mod tts;
pub mod vad;
pub mod wake;

pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per pipeline hop (80 ms) — the wake detector's native chunk size.
pub const CHUNK: usize = 1280;

use std::path::PathBuf;

/// Locate the jarvis models root (`<workspace>/models`), walking up from cwd.
pub fn models_root() -> anyhow::Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let c = dir.join("models/wake");
        if c.exists() {
            return Ok(dir.join("models"));
        }
        if !dir.pop() {
            anyhow::bail!("jarvis models root not found (looked for models/wake upward from cwd)");
        }
    }
}
