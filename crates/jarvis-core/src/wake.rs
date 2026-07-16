//! openWakeWord "hey jarvis" detection via ort. Ported from the J0 spike
//! (`crates/wake-spike`), which validated it against the reference Python
//! implementation (positives 0.997+ identical to 3 decimals) and live mic.
//!
//! CRITICAL: the classifier is trained on STREAMING features — mel computed
//! per 80 ms chunk over a 1760-sample window (chunk + 480 samples history,
//! WITH its segment edge effects), one embedding per chunk on the last 76 mel
//! frames, and a feature buffer seeded from random-noise audio (never zeros).
//! Batch/whole-clip features score ~0.0 on true positives. Keep this the
//! single feature path.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ndarray::{Array2, Array3, Array4};
use ort::session::Session;
use ort::value::TensorRef;

use crate::{CHUNK, SAMPLE_RATE};

/// History carried into each per-chunk melspec call (160 * 3, per reference).
const MEL_HISTORY: usize = 480;
/// Mel frames consumed per embedding (~775 ms of audio context).
const EMB_MEL_WINDOW: usize = 76;
/// Embeddings consumed per wake-model score (16 * 80 ms = 1.28 s context).
const WAKE_EMB_WINDOW: usize = 16;
const EMB_DIM: usize = 96;

/// 0.5 is openWakeWord's suggested default, but TTS tests show "hey <other
/// name>" phrases reaching 0.6–0.76 while true positives sit at 0.997+.
pub const DEFAULT_THRESHOLD: f32 = 0.85;

struct Oww {
    mel: Session,
    emb: Session,
    wake: Session,
}

impl Oww {
    fn load(dir: &Path, keyword_model: &str) -> Result<Self> {
        let sess = |name: &str| -> Result<Session> {
            let p = dir.join(name);
            Session::builder()?
                .commit_from_file(&p)
                .with_context(|| format!("load {}", p.display()))
        };
        Ok(Self {
            mel: sess("melspectrogram.onnx")?,
            emb: sess("embedding_model.onnx")?,
            wake: sess(keyword_model)?,
        })
    }

    /// Raw samples (int16 range) -> mel frames [n, 32] (with the /10+2 shift).
    fn melspec(&mut self, samples: &[f32]) -> Result<Array2<f32>> {
        let input = Array2::from_shape_vec((1, samples.len()), samples.to_vec())?;
        let outputs = self
            .mel
            .run(ort::inputs![TensorRef::from_array_view(&input)?])?;
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let frames = shape.iter().product::<i64>() as usize / 32;
        let mel = Array2::from_shape_vec((frames, 32), data.to_vec())?;
        Ok(mel.mapv(|x| x / 10.0 + 2.0))
    }

    /// One 76-frame mel window -> one 96-dim embedding.
    fn embed(&mut self, mel_window: &Array2<f32>) -> Result<Vec<f32>> {
        debug_assert_eq!(mel_window.shape(), [EMB_MEL_WINDOW, 32]);
        let input = Array4::from_shape_vec(
            (1, EMB_MEL_WINDOW, 32, 1),
            mel_window.iter().copied().collect(),
        )?;
        let outputs = self
            .emb
            .run(ort::inputs![TensorRef::from_array_view(&input)?])?;
        let (_, data) = outputs[0].try_extract_tensor::<f32>()?;
        Ok(data.to_vec())
    }

    /// Last 16 embeddings -> sigmoid wake score.
    fn score(&mut self, embs: &VecDeque<Vec<f32>>) -> Result<f32> {
        debug_assert_eq!(embs.len(), WAKE_EMB_WINDOW);
        let flat: Vec<f32> = embs.iter().flatten().copied().collect();
        let input = Array3::from_shape_vec((1, WAKE_EMB_WINDOW, EMB_DIM), flat)?;
        let outputs = self
            .wake
            .run(ort::inputs![TensorRef::from_array_view(&input)?])?;
        let (_, data) = outputs[0].try_extract_tensor::<f32>()?;
        Ok(data[0])
    }
}

/// Streaming wake-word detector. Feed 80 ms chunks (int16-range f32);
/// `detect` applies the threshold + a refractory cooldown.
pub struct WakeDetector {
    oww: Oww,
    raw: VecDeque<f32>,
    mel: Array2<f32>,
    feats: VecDeque<Vec<f32>>,
    threshold: f32,
    cooldown_until: Instant,
}

impl WakeDetector {
    /// `models_dir` is the jarvis `models/wake` directory; `model_file` is the
    /// wake-word ONNX to load (e.g. a custom "hey_nyxa.onnx").
    pub fn load(models_dir: &Path, model_file: &str, threshold: f32) -> Result<Self> {
        let oww = Oww::load(models_dir, model_file)?;
        let mut s = Self {
            oww,
            raw: VecDeque::new(),
            mel: Array2::zeros((0, 32)),
            feats: VecDeque::new(),
            threshold,
            cooldown_until: Instant::now(),
        };
        // Seed the feature buffer with embeddings of 4 s of pseudo-random
        // int16 noise in [-1000, 1000) (deterministic LCG) — mirrors the
        // reference; zero embeddings are out-of-distribution and suppress
        // scores for the first ~1.3 s.
        let mut state = 0x2545F491_u64;
        let noise: Vec<f32> = (0..SAMPLE_RATE as usize * 4)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((state >> 33) % 2000) as f32 - 1000.0
            })
            .collect();
        for chunk in noise.chunks_exact(CHUNK) {
            s.push_chunk(chunk)?;
        }
        anyhow::ensure!(s.feats.len() == WAKE_EMB_WINDOW, "seed did not fill feature buffer");
        Ok(s)
    }

    /// Process one 1280-sample chunk; returns the raw score once warm.
    pub fn push_chunk(&mut self, chunk: &[f32]) -> Result<Option<f32>> {
        debug_assert_eq!(chunk.len(), CHUNK);
        self.raw.extend(chunk.iter().copied());
        while self.raw.len() > CHUNK + MEL_HISTORY {
            self.raw.pop_front();
        }
        let seg: Vec<f32> = self.raw.iter().copied().collect();
        let new_mel = self.oww.melspec(&seg)?;
        self.mel = ndarray::concatenate![ndarray::Axis(0), self.mel, new_mel];
        let n = self.mel.shape()[0];
        if n > 4 * EMB_MEL_WINDOW {
            self.mel = self.mel.slice(ndarray::s![n - 2 * EMB_MEL_WINDOW.., ..]).to_owned();
        }

        let n = self.mel.shape()[0];
        if n < EMB_MEL_WINDOW {
            return Ok(None);
        }
        let window = self.mel.slice(ndarray::s![n - EMB_MEL_WINDOW.., ..]).to_owned();
        let emb = self.oww.embed(&window)?;
        self.feats.push_back(emb);
        while self.feats.len() > WAKE_EMB_WINDOW {
            self.feats.pop_front();
        }
        if self.feats.len() < WAKE_EMB_WINDOW {
            return Ok(None);
        }
        Ok(Some(self.oww.score(&self.feats)?))
    }

    /// Push a chunk and apply threshold + 1.5 s refractory cooldown.
    /// Returns the score when it fired.
    pub fn detect(&mut self, chunk: &[f32]) -> Result<Option<f32>> {
        let score = self.push_chunk(chunk)?;
        if let Some(s) = score {
            if s >= self.threshold && Instant::now() > self.cooldown_until {
                self.cooldown_until = Instant::now() + Duration::from_millis(1500);
                return Ok(Some(s));
            }
        }
        Ok(None)
    }
}
