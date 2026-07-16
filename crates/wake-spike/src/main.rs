//! J0 spike: openWakeWord "hey jarvis" detection in Rust via ort.
//!
//! openWakeWord is a 3-model ONNX cascade:
//!   1. melspectrogram.onnx : raw 16 kHz samples (int16 range!) -> mel frames [n, 32]
//!   2. embedding_model.onnx: sliding 76-frame mel window -> one 96-dim embedding
//!   3. hey_jarvis_v0.1.onnx: last 16 embeddings [1, 16, 96] -> sigmoid score
//!
//! CRITICAL (learned the hard way, verified against the reference impl): the
//! classifier is trained on STREAMING features — mel computed per 80 ms chunk
//! over a 1760-sample window (chunk + 480 samples history, WITH its segment
//! edge effects), one embedding per chunk on the last 76 mel frames, and a
//! feature buffer seeded from random-noise audio. Computing mel/embeddings
//! over the whole clip in one batch yields features different enough that the
//! head scores ~0.0 on true positives. So there is exactly one feature path
//! here (`Streamer`), used by both wav and mic modes, replicating
//! `AudioFeatures._streaming_features` step for step.
//!
//! Modes:
//!   wake-spike --wav FILE...   score wav file(s) (16 kHz mono s16), print max score
//!   wake-spike --mic           live monitor: prints scores + DETECTED lines
//!   wake-spike --dump FILE     print mel/embedding intermediates (debug)

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ndarray::{Array2, Array3, Array4};
use ort::session::Session;
use ort::value::TensorRef;

const SAMPLE_RATE: u32 = 16_000;
/// Samples per pipeline hop (80 ms), as in the reference implementation.
const CHUNK: usize = 1280;
/// History carried into each per-chunk melspec call (160 * 3, per reference).
const MEL_HISTORY: usize = 480;
/// Mel frames consumed per embedding (~775 ms of audio context).
const EMB_MEL_WINDOW: usize = 76;
/// Embeddings consumed per wake-model score (16 * 80 ms = 1.28 s context).
const WAKE_EMB_WINDOW: usize = 16;
const EMB_DIM: usize = 96;
/// 0.5 is openWakeWord's suggested default, but TTS tests show "hey <other
/// name>" phrases reaching 0.6–0.76 while true positives sit at 0.997+.
/// 0.85 keeps a huge margin on positives and rejects the near-miss family.
const DETECT_THRESHOLD: f32 = 0.85;

struct Oww {
    mel: Session,
    emb: Session,
    wake: Session,
}

impl Oww {
    fn load(dir: &Path) -> Result<Self> {
        let sess = |name: &str| -> Result<Session> {
            let p = dir.join(name);
            Session::builder()?
                .commit_from_file(&p)
                .with_context(|| format!("load {}", p.display()))
        };
        Ok(Self {
            mel: sess("melspectrogram.onnx")?,
            emb: sess("embedding_model.onnx")?,
            wake: sess("hey_jarvis_v0.1.onnx")?,
        })
    }

    /// Raw samples (int16 range, NOT normalized to ±1) -> mel frames [n, 32].
    /// openWakeWord applies `x/10 + 2` to the model output.
    fn melspec(&mut self, samples: &[f32]) -> Result<Array2<f32>> {
        let input = Array2::from_shape_vec((1, samples.len()), samples.to_vec())?;
        let outputs = self
            .mel
            .run(ort::inputs![TensorRef::from_array_view(&input)?])?;
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        // Output is [1, 1, frames, 32]; flatten to [frames, 32].
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

/// Exact replica of the reference streaming feature pipeline. Feed it 80 ms
/// chunks; it returns one wake score per chunk once warm.
struct Streamer {
    oww: Oww,
    /// Last CHUNK + MEL_HISTORY raw samples (int16 range).
    raw: VecDeque<f32>,
    /// Rolling mel frame buffer; only the trailing EMB_MEL_WINDOW frames are read.
    mel: Array2<f32>,
    /// Last 16 embeddings, seeded from noise audio (never zeros — zero
    /// embeddings are out-of-distribution for the head and suppress scores).
    feats: VecDeque<Vec<f32>>,
}

impl Streamer {
    fn new(oww: Oww) -> Result<Self> {
        let mut s = Self {
            oww,
            raw: VecDeque::new(),
            mel: Array2::zeros((0, 32)),
            feats: VecDeque::new(),
        };
        // Reference seeds its feature buffer with embeddings of 4 s of random
        // int16 noise in [-1000, 1000). Deterministic LCG here, same effect.
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

    /// Process one 1280-sample chunk; returns a score once the buffers are warm.
    fn push_chunk(&mut self, chunk: &[f32]) -> Result<Option<f32>> {
        debug_assert_eq!(chunk.len(), CHUNK);
        self.raw.extend(chunk.iter().copied());
        while self.raw.len() > CHUNK + MEL_HISTORY {
            self.raw.pop_front();
        }
        // Mel over chunk + history — segment edge effects and all; the
        // classifier expects them (see module docs).
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
}

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).context("open wav")?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.sample_rate == SAMPLE_RATE && spec.channels == 1,
        "expected 16 kHz mono, got {} Hz {} ch",
        spec.sample_rate,
        spec.channels
    );
    // Keep int16 range — openWakeWord models expect it (no /32768).
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map(|v| v * 32768.0))
            .collect::<Result<_, _>>()?,
    };
    Ok(samples)
}

fn run_wav(streamer: &mut Streamer, path: &Path) -> Result<()> {
    let samples = read_wav(path)?;
    let secs = samples.len() as f32 / SAMPLE_RATE as f32;
    let t0 = std::time::Instant::now();
    let mut scores = Vec::new();
    for chunk in samples.chunks_exact(CHUNK) {
        if let Some(s) = streamer.push_chunk(chunk)? {
            scores.push(s);
        }
    }
    let ms = t0.elapsed().as_millis();
    let max = scores.iter().cloned().fold(0.0f32, f32::max);
    let verdict = if max >= DETECT_THRESHOLD { "DETECTED" } else { "no" };
    println!(
        "{:<40} {:>5.1}s  max={:.3}  [{}]  ({} steps, {} ms)",
        path.file_name().unwrap_or_default().to_string_lossy(),
        secs,
        max,
        verdict,
        scores.len(),
        ms
    );
    Ok(())
}

/// Open the default input device, streaming raw blocks (normalized ±1 f32,
/// interleaved) over a channel. Returns the stream handle (keep alive!),
/// the receiver, and (device_rate, channels).
fn open_mic() -> Result<(cpal::Stream, std::sync::mpsc::Receiver<Vec<f32>>, u32, usize)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::mpsc;

    let device = cpal::default_host()
        .default_input_device()
        .context("no default input device")?;
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let config = device.default_input_config()?;
    let device_rate = config.sample_rate();
    let channels = config.channels() as usize;
    eprintln!("[mic] device: {name} ({device_rate} Hz, {channels} ch, {:?})", config.sample_format());

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let err_fn = |e| eprintln!("[mic] stream error: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config.into(),
            move |data: &[f32], _: &_| {
                let _ = tx.send(data.to_vec());
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.into(),
            move |data: &[i16], _: &_| {
                let _ = tx.send(data.iter().map(|&s| s as f32 / 32768.0).collect());
            },
            err_fn,
            None,
        )?,
        other => bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    Ok((stream, rx, device_rate, channels))
}

/// Downmix + naive linear resample one raw block to 16 kHz int16-range mono.
fn to_16k(block: &[f32], channels: usize, device_rate: u32) -> Vec<f32> {
    let mono: Vec<f32> = block
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
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

/// Live mic monitor. Prints a level/score telemetry line every ~2 s so a
/// silent/quiet/wrong device is visible immediately, plus DETECTED lines.
fn run_mic(mut streamer: Streamer) -> Result<()> {
    let (_stream, rx, device_rate, channels) = open_mic()?;
    println!("listening — say \"hey jarvis\" (Ctrl+C to quit)");

    let mut pending: Vec<f32> = Vec::new();
    let mut cooldown_until = std::time::Instant::now();
    let (mut win_peak, mut win_score, mut win_chunks) = (0f32, 0f32, 0usize);

    for block in rx {
        pending.extend(to_16k(&block, channels, device_rate));

        while pending.len() >= CHUNK {
            let chunk: Vec<f32> = pending.drain(..CHUNK).collect();
            let peak = chunk.iter().fold(0f32, |m, &s| m.max(s.abs()));
            win_peak = win_peak.max(peak);
            if let Some(score) = streamer.push_chunk(&chunk)? {
                win_score = win_score.max(score);
                if score >= DETECT_THRESHOLD && std::time::Instant::now() > cooldown_until {
                    println!("== DETECTED (score {score:.3}) ==");
                    cooldown_until =
                        std::time::Instant::now() + std::time::Duration::from_millis(1500);
                }
            }
            win_chunks += 1;
            if win_chunks >= 25 {
                // peak is in int16 units; ~330 = quiet room, 3000+ = speech at
                // normal mic gain. A flat ~0 peak means dead/wrong device.
                println!("  [2s] peak={win_peak:>6.0}  max_score={win_score:.3}");
                (win_peak, win_score, win_chunks) = (0.0, 0.0, 0);
            }
        }
    }
    Ok(())
}

/// Record N seconds from the mic through the exact same capture path as
/// --mic, save as 16 kHz mono s16 wav, and report levels. Produces a
/// reproducible artifact for offline scoring when live detection misbehaves.
fn run_rec(path: &Path, secs: u32) -> Result<()> {
    let (_stream, rx, device_rate, channels) = open_mic()?;
    println!("recording {secs}s — say \"hey jarvis\"...");
    let mut samples: Vec<f32> = Vec::new();
    let target = (SAMPLE_RATE * secs) as usize;
    for block in rx {
        samples.extend(to_16k(&block, channels, device_rate));
        if samples.len() >= target {
            break;
        }
    }
    samples.truncate(target);
    let peak = samples.iter().fold(0f32, |m, &s| m.max(s.abs()));
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in &samples {
        w.write_sample(s.clamp(-32768.0, 32767.0) as i16)?;
    }
    w.finalize()?;
    println!("saved {} — peak={peak:.0} rms={rms:.0} (int16 units)", path.display());
    Ok(())
}

fn models_dir() -> PathBuf {
    // dev layout: <workspace>/models/wake relative to cwd or its parents
    let mut dir = std::env::current_dir().unwrap();
    loop {
        let c = dir.join("models/wake");
        if c.exists() {
            return c;
        }
        if !dir.pop() {
            return PathBuf::from("models/wake");
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = models_dir();
    let mut oww = Oww::load(&dir).with_context(|| format!("models dir {}", dir.display()))?;
    eprintln!("[oww] models loaded from {}", dir.display());

    match args.first().map(String::as_str) {
        Some("--wav") => {
            anyhow::ensure!(args.len() > 1, "--wav needs at least one file");
            for f in &args[1..] {
                // Fresh streamer per file so clips don't bleed into each other.
                let mut streamer = Streamer::new(Oww::load(&dir)?)?;
                run_wav(&mut streamer, Path::new(f))?;
            }
            Ok(())
        }
        Some("--mic") => run_mic(Streamer::new(oww)?),
        Some("--rec") => {
            anyhow::ensure!(args.len() > 1, "--rec needs an output file");
            let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            run_rec(Path::new(&args[1]), secs)
        }
        // Debug: dump mel/embedding intermediates to diff against the
        // reference Python implementation (batch path, kept for diffing).
        Some("--dump") => {
            let samples = read_wav(Path::new(&args[1]))?;
            let mel = oww.melspec(&samples)?;
            let n = mel.len();
            println!(
                "mel shape {:?} mean {:.4} min {:.4} max {:.4}",
                mel.shape(),
                mel.iter().sum::<f32>() / n as f32,
                mel.iter().cloned().fold(f32::INFINITY, f32::min),
                mel.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
            );
            println!("mel[40][:8] {:?}", mel.slice(ndarray::s![40, ..8]));
            let w = mel.slice(ndarray::s![40..40 + EMB_MEL_WINDOW, ..]).to_owned();
            let e = oww.embed(&w)?;
            println!("emb(frames 40..116)[:8] {:?}", &e[..8]);
            Ok(())
        }
        _ => {
            println!("usage: wake-spike --wav FILE... | --mic | --rec FILE [SECS] | --dump FILE");
            Ok(())
        }
    }
}
