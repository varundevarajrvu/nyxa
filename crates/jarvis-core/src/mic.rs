//! Microphone capture for the always-on listener. Unlike whispr's gated
//! `Recorder`, this streams continuously: cpal blocks are downmixed/resampled
//! on a converter thread and delivered as fixed 1280-sample (80 ms) chunks of
//! 16 kHz mono f32 in **int16 range** (the openWakeWord convention).
//!
//! Privacy invariant lives at this seam: chunks flow only to the wake
//! detector until it fires; nothing is persisted or transcribed before that.

use std::sync::mpsc::{self, Receiver};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{CHUNK, SAMPLE_RATE};

pub struct Mic {
    _stream: cpal::Stream,
    rx: Receiver<Vec<f32>>,
}

impl Mic {
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .context("no default input device (is a microphone connected?)")?;
        let name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        let config = device.default_input_config()?;
        let device_rate = config.sample_rate();
        let channels = config.channels() as usize;
        eprintln!(
            "[mic] device: {name} ({device_rate} Hz, {channels} ch, {:?})",
            config.sample_format()
        );

        // cpal callback -> raw blocks -> converter thread -> 1280-chunks.
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<f32>>();
        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<f32>>();
        let err_fn = |e| eprintln!("[mic] stream error: {e}");
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                config.into(),
                move |data: &[f32], _: &_| {
                    let _ = raw_tx.send(data.to_vec());
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                config.into(),
                move |data: &[i16], _: &_| {
                    let _ = raw_tx.send(data.iter().map(|&s| s as f32 / 32768.0).collect());
                },
                err_fn,
                None,
            )?,
            other => bail!("unsupported sample format {other:?}"),
        };
        stream.play().context("start input stream")?;

        std::thread::spawn(move || {
            let mut pending: Vec<f32> = Vec::new();
            for block in raw_rx {
                pending.extend(to_16k(&block, channels, device_rate));
                while pending.len() >= CHUNK {
                    let chunk: Vec<f32> = pending.drain(..CHUNK).collect();
                    if chunk_tx.send(chunk).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self { _stream: stream, rx: chunk_rx })
    }

    /// Blocking iterator over 80 ms chunks. Ends only if the stream dies.
    pub fn chunks(&self) -> &Receiver<Vec<f32>> {
        &self.rx
    }
}

/// Downmix + naive linear resample one raw block (±1 f32, interleaved) to
/// 16 kHz int16-range mono. Good enough for wake detection; ASR quality is
/// protected by re-normalization at the utterance boundary.
fn to_16k(block: &[f32], channels: usize, device_rate: u32) -> Vec<f32> {
    let mono: Vec<f32> = block
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    if mono.is_empty() {
        return mono;
    }
    let ratio = device_rate as f64 / SAMPLE_RATE as f64;
    let out_len = (mono.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = mono[idx.min(mono.len() - 1)];
            let b = mono[(idx + 1).min(mono.len() - 1)];
            (a + (b - a) * frac) * 32768.0
        })
        .collect()
}

/// Prepare an endpointed utterance (±1 scale) for whispr's ASR: remove DC
/// offset and peak-normalize to 0.9, skipping near-silence — mirrors
/// whispr-core::audio's private normalize, which measured real accuracy wins.
pub fn normalize_for_asr(samples: &mut [f32]) {
    if samples.is_empty() {
        return;
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let mut peak = 0f32;
    for s in samples.iter_mut() {
        *s -= mean;
        peak = peak.max(s.abs());
    }
    if peak > 0.01 {
        let gain = 0.9 / peak;
        for s in samples.iter_mut() {
            *s *= gain;
        }
    }
}
