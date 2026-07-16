//! Utterance endpointing via Silero VAD (sherpa-onnx). After the wake word
//! fires, feed post-wake chunks here; a completed speech segment (ended by
//! trailing silence) comes back ready for ASR.

use std::path::Path;

use anyhow::{anyhow, Result};
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};

/// Trailing silence that ends an utterance.
const MIN_SILENCE_S: f32 = 0.8;
/// Shortest speech burst that counts as an utterance at all.
const MIN_SPEECH_S: f32 = 0.25;
/// Hard cap on one utterance (matches DESIGN.md §1).
const MAX_SPEECH_S: f32 = 15.0;

pub struct Endpointer {
    vad: SileroVad,
}

impl Endpointer {
    pub fn new(model: &Path) -> Result<Self> {
        let vad = SileroVad::new(
            SileroVadConfig {
                model: model.to_string_lossy().into_owned(),
                min_silence_duration: MIN_SILENCE_S,
                min_speech_duration: MIN_SPEECH_S,
                max_speech_duration: MAX_SPEECH_S,
                threshold: 0.5,
                sample_rate: crate::SAMPLE_RATE,
                window_size: 512,
                provider: None,
                num_threads: Some(1),
                debug: false,
            },
            // internal ring buffer, must exceed max utterance length
            MAX_SPEECH_S * 2.0,
        )
        .map_err(|e| anyhow!("create silero vad: {e}"))?;
        Ok(Self { vad })
    }

    /// Reset state for a fresh capture window (call right after wake fires).
    pub fn clear(&mut self) {
        self.vad.clear();
    }

    /// Feed one chunk in ±1 scale (divide int16-range chunks by 32768 first).
    pub fn feed(&mut self, samples: Vec<f32>) {
        self.vad.accept_waveform(samples);
    }

    /// Is speech currently in progress?
    pub fn in_speech(&mut self) -> bool {
        self.vad.is_speech()
    }

    /// A finished segment (speech that ended with MIN_SILENCE_S of quiet),
    /// if one is ready. Samples are ±1 scale.
    pub fn segment(&mut self) -> Option<Vec<f32>> {
        if self.vad.is_empty() {
            return None;
        }
        let seg = self.vad.front();
        self.vad.pop();
        Some(seg.samples)
    }

    /// Force out whatever speech is buffered (used at the hard time cap).
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        self.vad.flush();
        self.segment()
    }
}
