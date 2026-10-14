//! Sample recording, modelled on the M8's: record from an audio input or
//! resample the master output, wait for a level to arm, and save the take
//! as a WAV file for the sampler.
//!
//! Audio reaches a [`Capture`] from the input device's callback or from the
//! rack's mixer; the UI drains it a block at a time into a [`Take`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use super::sampler::Sample;
use crate::audio::find_input_device;

/// Longest take, in seconds.
pub const MAX_SECONDS: u32 = 600;

/// Where the audio comes from. M8 names where the M8 has the same thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Both channels of the input device.
    LeftRight,
    /// Its left channel, as a mono sample.
    InLeft,
    /// Its right channel, as a mono sample.
    InRight,
    /// The master output: resampling.
    Master,
}

pub const SOURCES: [Source; 4] = [
    Source::LeftRight,
    Source::InLeft,
    Source::InRight,
    Source::Master,
];

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::LeftRight => "L&R",
            Source::InLeft => "INL",
            Source::InRight => "INR",
            Source::Master => "MASTER",
        }
    }

    pub fn is_input(self) -> bool {
        self != Source::Master
    }

    pub fn is_mono(self) -> bool {
        matches!(self, Source::InLeft | Source::InRight)
    }
}

/// Audio on its way from the audio thread to the recorder. Always stereo;
/// mono sources fill both channels.
pub struct Capture {
    buf: Mutex<Vec<f32>>,
    recording: AtomicBool,
    triggered: AtomicBool,
    /// Level that starts an armed recording, 0 for none.
    threshold: AtomicU32,
    gain: AtomicU32,
    peak: AtomicU32,
    source: AtomicU32,
}

impl Capture {
    pub fn new() -> Arc<Self> {
        Arc::new(Capture {
            buf: Mutex::new(Vec::new()),
            recording: AtomicBool::new(false),
            triggered: AtomicBool::new(false),
            threshold: AtomicU32::new(0),
            gain: AtomicU32::new(1.0f32.to_bits()),
            peak: AtomicU32::new(0),
            source: AtomicU32::new(0),
        })
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn set_source(&self, s: Source) {
        let i = SOURCES.iter().position(|x| *x == s).unwrap_or(0);
        self.source.store(i as u32, Ordering::Relaxed);
    }

    fn source(&self) -> Source {
        SOURCES[self.source.load(Ordering::Relaxed) as usize % SOURCES.len()]
    }

    /// Start keeping audio, once it gets louder than `threshold` (0 keeps
    /// everything). `room` frames are reserved ahead.
    pub fn start(&self, threshold: f32, room: usize) {
        if let Ok(mut b) = self.buf.lock() {
            b.clear();
            b.reserve(room * 2);
        }
        self.threshold.store(threshold.to_bits(), Ordering::Relaxed);
        self.triggered.store(threshold <= 0.0, Ordering::Relaxed);
        self.recording.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.recording.store(false, Ordering::Release);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    /// Whether an armed recording has heard its level and started.
    pub fn triggered(&self) -> bool {
        self.triggered.load(Ordering::Relaxed)
    }

    /// The loudest sample since the last call, after the gain.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::AcqRel))
    }

    /// Swap out what has been captured since the last call. `spare` should
    /// be empty and is handed to the audio thread, so its capacity is reused.
    pub fn drain(&self, spare: &mut Vec<f32>) {
        if let Ok(mut b) = self.buf.lock() {
            std::mem::swap(&mut *b, spare);
        }
    }

    /// Feed `frames` frames; `frame(i)` gives frame `i` as (left, right).
    pub fn push(&self, frames: usize, frame: impl Fn(usize) -> (f32, f32)) {
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let recording = self.recording.load(Ordering::Acquire);
        let mut triggered = self.triggered.load(Ordering::Relaxed);
        let threshold = f32::from_bits(self.threshold.load(Ordering::Relaxed));
        let mut buf = if recording {
            self.buf.lock().ok()
        } else {
            None
        };
        let mut peak = 0.0f32;
        for i in 0..frames {
            let (l, r) = frame(i);
            let (l, r) = (l * gain, r * gain);
            let level = l.abs().max(r.abs());
            peak = peak.max(level);
            if !recording {
                continue;
            }
            if !triggered && level >= threshold {
                triggered = true;
                self.triggered.store(true, Ordering::Relaxed);
            }
            if triggered {
                if let Some(b) = buf.as_mut() {
                    b.push(l);
                    b.push(r);
                }
            }
        }
        let prev = f32::from_bits(self.peak.load(Ordering::Relaxed));
        if peak > prev {
            self.peak.store(peak.to_bits(), Ordering::Relaxed);
        }
    }

    /// Feed interleaved input with `channels` channels, picking the
    /// channels the source wants.
    fn push_input(&self, data: &[f32], channels: usize) {
        let ch = channels.max(1);
        let (l, r) = match self.source() {
            Source::InLeft => (0, 0),
            Source::InRight => (1.min(ch - 1), 1.min(ch - 1)),
            _ => (0, 1.min(ch - 1)),
        };
        self.push(data.len() / ch, |i| (data[i * ch + l], data[i * ch + r]));
    }
}

/// An open input device feeding a [`Capture`].
pub struct Input {
    _stream: cpal::Stream,
    pub sample_rate: u32,
    pub name: String,
}

/// Open the input device `name` (or the default one) into `capture`.
pub fn open_input(name: Option<&str>, capture: Arc<Capture>) -> Result<Input> {
    let device = find_input_device(name)?;
    let supported = device.default_input_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let sample_rate = config.sample_rate.0;
    let channels = config.channels as usize;
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, capture, channels),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, capture, channels),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, capture, channels),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, capture, channels),
        other => Err(anyhow!("unsupported input sample format {other}")),
    }?;
    stream.play()?;
    Ok(Input {
        _stream: stream,
        sample_rate,
        name: device.name().unwrap_or_else(|_| "default input".into()),
    })
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    capture: Arc<Capture>,
    channels: usize,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut conv: Vec<f32> = Vec::with_capacity(8192);
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            conv.clear();
            conv.extend(data.iter().map(|s| s.to_sample::<f32>()));
            capture.push_input(&conv, channels);
        },
        |_err| {},
        None,
    )?)
}

/// A recording, as it grows and once it is done.
pub struct Take {
    /// Left and right.
    pub channels: [Vec<f32>; 2],
    pub sample_rate: u32,
    pub mono: bool,
}

impl Take {
    pub fn new(sample_rate: u32, mono: bool) -> Self {
        Take {
            channels: [Vec::new(), Vec::new()],
            sample_rate,
            mono,
        }
    }

    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }

    /// Add interleaved stereo frames.
    pub fn extend(&mut self, interleaved: &[f32]) {
        for f in interleaved.chunks_exact(2) {
            self.channels[0].push(f[0]);
            self.channels[1].push(f[1]);
        }
    }

    /// The loudest sample in each of `n` columns, looking at no more than
    /// 256 frames per column so it stays cheap on long takes.
    pub fn peaks(&self, n: usize) -> Vec<f32> {
        let len = self.frames();
        (0..n)
            .map(|i| {
                let (a, b) = (len * i / n, len * (i + 1) / n);
                let step = ((b - a) / 256).max(1);
                (a..b)
                    .step_by(step)
                    .map(|f| self.channels[0][f].abs().max(self.channels[1][f].abs()))
                    .fold(0.0f32, f32::max)
            })
            .collect()
    }

    /// The take as a sample the sampler can play.
    pub fn to_sample(&self) -> Sample {
        let channels = if self.mono {
            vec![self.channels[0].clone()]
        } else {
            self.channels.to_vec()
        };
        Sample {
            channels,
            sample_rate: self.sample_rate,
            cues: Vec::new(),
        }
    }

    /// Write the take as a 16-bit WAV file, mono for mono sources.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let samples: Vec<f32> = if self.mono {
            self.channels[0].clone()
        } else {
            self.channels[0]
                .iter()
                .zip(&self.channels[1])
                .flat_map(|(l, r)| [*l, *r])
                .collect()
        };
        let ch = if self.mono { 1 } else { 2 };
        super::rack::write_wav(path, &samples, ch, self.sample_rate)
    }
}

/// The first `REC000.wav`, `REC001.wav`... not in `dir`, without the
/// extension.
pub fn next_name(dir: &Path) -> String {
    (0..1000)
        .map(|i| format!("REC{i:03}"))
        .find(|n| !wav_path(dir, n).exists())
        .unwrap_or_else(|| "REC".into())
}

/// `dir/NAME.wav`.
pub fn wav_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.wav"))
}

/// The folder recordings go in: `Samples/Recordings` under the card root.
pub fn recordings_dir(card_root: &Path) -> PathBuf {
    card_root.join("Samples").join("Recordings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armed_capture_starts_at_the_threshold() {
        let c = Capture::new();
        c.start(0.5, 16);
        let input = [0.1f32, 0.2, 0.6, 0.3, -0.1];
        c.push(input.len(), |i| (input[i], -input[i]));
        assert!(c.triggered());
        let mut got = Vec::new();
        c.drain(&mut got);
        assert_eq!(got, [0.6, -0.6, 0.3, -0.3, -0.1, 0.1]);
        assert!((c.take_peak() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn idle_capture_only_meters() {
        let c = Capture::new();
        c.set_gain(2.0);
        c.push(2, |_| (0.25, 0.0));
        let mut got = Vec::new();
        c.drain(&mut got);
        assert!(got.is_empty());
        assert!((c.take_peak() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mono_input_picks_a_channel() {
        let c = Capture::new();
        c.set_source(Source::InRight);
        c.start(0.0, 4);
        c.push_input(&[0.1, 0.9, 0.2, 0.8], 2);
        let mut got = Vec::new();
        c.drain(&mut got);
        assert_eq!(got, [0.9, 0.9, 0.8, 0.8]);
    }

    #[test]
    fn take_saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("lextrack-rec-{}", std::process::id()));
        let mut t = Take::new(44_100, true);
        t.extend(&[0.5, 0.5, -0.25, -0.25, 0.0, 0.0]);
        assert_eq!(next_name(&dir), "REC000");
        let path = wav_path(&dir, "REC000");
        t.save(&path).unwrap();
        assert_eq!(next_name(&dir), "REC001");
        let s = Sample::load(&path).unwrap();
        assert_eq!(s.channels.len(), 1);
        assert_eq!(s.sample_rate, 44_100);
        assert_eq!(s.frames(), 3);
        assert!((s.channels[0][0] - 0.5).abs() < 1e-3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
