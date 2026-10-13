//! The built-in sampler, modelled on the M8's: instruments of type SAMPLER
//! play a WAV file with the M8's play modes, slices, start / loop / length
//! points, degrade, filter, amp and limiter, pan, and two AHD envelopes and
//! two LFOs. Each track plays one sample at a time, like the M8.
//!
//! Sample paths in M8 songs start at the SD card root ("/Samples/kick.wav").
//! The card root is found from the song's own folder and the folder the song
//! says it lives in ("/Songs/"), so a copy of an M8 card works as it is.

use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::format::{
    Fx, Instrument, Song, INSTRUMENTS, SAMPLER_PARAMS, S_AMP, S_CUTOFF, S_DEGRADE, S_ENV, S_FILTER,
    S_LENGTH, S_LFO, S_LIMIT, S_LOOP, S_PAN, S_PLAY, S_RES, S_SLICE, S_START, TRACKS,
};
use super::instruments::InstrumentMap;

// --- samples -------------------------------------------------------------------

/// A sample in memory.
pub struct Sample {
    /// One or two channels of the same length.
    pub channels: Vec<Vec<f32>>,
    pub sample_rate: u32,
    /// Slice markers (WAV cue points), in frames, sorted.
    pub cues: Vec<usize>,
}

impl Sample {
    pub fn load(path: &Path) -> Result<Sample> {
        let bytes =
            std::fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
        parse_wav(&bytes).with_context(|| format!("{} is not a readable WAV file", path.display()))
    }

    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    /// The loudest point in each of `n` equal parts, for drawing.
    pub fn peaks(&self, n: usize) -> Vec<f32> {
        let len = self.frames();
        (0..n)
            .map(|i| {
                let (a, b) = (len * i / n, (len * (i + 1) / n).max(len * i / n + 1));
                self.channels
                    .iter()
                    .flat_map(|c| c[a.min(len)..b.min(len)].iter())
                    .fold(0.0f32, |m, x| m.max(x.abs()))
            })
            .collect()
    }

    /// The part of the sample a note plays: the whole sample, or with
    /// slicing on, the slice the note picks (C-1 is the first). Slices come
    /// from the file's cue points when it has any, otherwise `slice` cuts
    /// the sample into that many equal parts.
    pub fn region(&self, slice: u8, note: i16) -> (usize, usize) {
        let len = self.frames();
        if slice == 0 || len == 0 {
            return (0, len);
        }
        let mut points: Vec<usize> = self.cues.iter().copied().filter(|&c| c < len).collect();
        if !points.is_empty() {
            if points[0] != 0 {
                points.insert(0, 0);
            }
            points.push(len);
            let i = (note.max(0) as usize) % (points.len() - 1);
            return (points[i], points[i + 1]);
        }
        let count = slice as usize;
        let i = (note.max(0) as usize) % count;
        (len * i / count, len * (i + 1) / count)
    }
}

/// Decode a WAV file: 8, 16, 24 or 32-bit PCM or 32/64-bit float, mono or
/// stereo (more channels keep the first two), with cue points.
pub fn parse_wav(b: &[u8]) -> Result<Sample> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("missing the RIFF/WAVE header");
    }
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut fmt = None;
    let mut data: Option<&[u8]> = None;
    let mut cues = Vec::new();
    let mut pos = 12;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = &b[pos + 8..(pos + 8).saturating_add(size).min(b.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let mut format = u16_at(pos + 8);
                if format == 0xFFFE && body.len() >= 26 {
                    format = u16_at(pos + 8 + 24);
                }
                fmt = Some((
                    format,
                    u16_at(pos + 10) as usize,
                    u32_at(pos + 12),
                    u16_at(pos + 20) as usize,
                    u16_at(pos + 22),
                ));
            }
            b"data" => data = Some(body),
            b"cue " if body.len() >= 4 => {
                let n = u32_at(pos + 8) as usize;
                for i in 0..n {
                    let o = pos + 12 + i * 24;
                    if o + 24 > pos + 8 + body.len() {
                        break;
                    }
                    cues.push(u32_at(o + 20) as usize);
                }
            }
            _ => {}
        }
        pos = pos + 8 + size + (size & 1);
    }
    let Some((format, channels, rate, align, bits)) = fmt else {
        bail!("no fmt chunk");
    };
    let Some(data) = data else {
        bail!("no data chunk");
    };
    let width = (bits as usize).div_ceil(8);
    if channels == 0 || align < channels * width || width == 0 {
        bail!("bad sample format ({channels} channels, {bits} bits)");
    }
    let read: fn(&[u8]) -> f32 = match (format, bits) {
        (1, 8) => |s| (s[0] as f32 - 128.0) / 128.0,
        (1, 16) => |s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
        (1, 24) => |s| (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
        (1, 32) => |s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
        (3, 32) => |s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        (3, 64) => |s| f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32,
        _ => bail!("unsupported sample format (format {format}, {bits} bits)"),
    };
    let frames = data.len() / align;
    let keep = channels.min(2);
    let mut out = vec![Vec::with_capacity(frames); keep];
    for f in data.chunks_exact(align) {
        for (c, ch) in out.iter_mut().enumerate() {
            ch.push(read(&f[c * width..]));
        }
    }
    cues.sort_unstable();
    cues.dedup();
    Ok(Sample {
        channels: out,
        sample_rate: rate.max(1),
        cues,
    })
}

/// The folder an M8 song's sample paths start from: the song's folder, less
/// the folders the song says it is in on the card ("/Songs/").
pub fn card_root(song_file_dir: &Path, song_dir_on_card: &str) -> PathBuf {
    let mut root = song_file_dir.to_path_buf();
    let parts: Vec<&str> = song_dir_on_card
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    let mut probe = root.clone();
    for part in parts.iter().rev() {
        match probe.file_name() {
            Some(n) if n.to_string_lossy().eq_ignore_ascii_case(part) => {
                probe.pop();
            }
            _ => return root,
        }
    }
    if !parts.is_empty() {
        root = probe;
    }
    root
}

/// Where a song's sample path points on this computer: under the card root,
/// as an absolute path, or under the song's folder or any folder above it.
pub fn resolve_sample_path(root: &Path, song_file_dir: &Path, sample: &str) -> Option<PathBuf> {
    if sample.is_empty() {
        return None;
    }
    let rel = sample.trim_start_matches(['/', '\\']);
    let under_root = root.join(rel);
    if under_root.is_file() {
        return Some(under_root);
    }
    let p = Path::new(sample);
    if p.is_absolute() && p.is_file() {
        return Some(p.to_path_buf());
    }
    song_file_dir
        .ancestors()
        .map(|d| d.join(rel))
        .find(|c| c.is_file())
}

/// The path to store in the song for `file`: from the card root ("/Samples/
/// kick.wav") when the file is under it, as the M8 does, otherwise absolute.
pub fn sample_path_for(root: &Path, file: &Path) -> String {
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match file.strip_prefix(&root) {
        Ok(rel) => {
            let parts: Vec<String> = rel
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            format!("/{}", parts.join("/"))
        }
        Err(_) => file.display().to_string(),
    }
}

/// Samples loaded so far, by file, so each is read once.
#[derive(Default)]
pub struct SampleBank {
    cache: HashMap<PathBuf, Result<Arc<Sample>, String>>,
}

impl SampleBank {
    pub fn get(&mut self, path: &Path) -> Result<Arc<Sample>, String> {
        self.cache
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                Sample::load(path)
                    .map(Arc::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .clone()
    }
}

/// Which instruments the built-in sampler plays, and their samples.
#[derive(Clone)]
pub struct SamplerSet {
    pub builtin: Vec<bool>,
    pub samples: Vec<Option<Arc<Sample>>>,
}

impl Default for SamplerSet {
    fn default() -> Self {
        SamplerSet {
            builtin: vec![false; INSTRUMENTS],
            samples: vec![None; INSTRUMENTS],
        }
    }
}

impl SamplerSet {
    /// Load the samples of every instrument the built-in sampler plays.
    /// Returns the set and, per instrument, why its sample didn't load.
    pub fn build(
        song: &Song,
        song_file_dir: &Path,
        map: &InstrumentMap,
        bank: &mut SampleBank,
    ) -> (SamplerSet, Vec<(u8, String)>) {
        let root = card_root(song_file_dir, &song.directory);
        let mut set = SamplerSet::default();
        let mut problems = Vec::new();
        for (n, inst) in song.instruments.iter().enumerate() {
            if !map.builtin_sampler(n as u8, inst) {
                continue;
            }
            set.builtin[n] = true;
            if inst.sample_path.is_empty() {
                continue;
            }
            match resolve_sample_path(&root, song_file_dir, &inst.sample_path) {
                Some(p) => match bank.get(&p) {
                    Ok(s) => set.samples[n] = Some(s),
                    Err(e) => problems.push((n as u8, e)),
                },
                None => problems.push((n as u8, format!("{} not found", inst.sample_path))),
            }
        }
        (set, problems)
    }

    /// Whether two sets play the same samples on the same instruments.
    pub fn same_as(&self, other: &SamplerSet) -> bool {
        self.builtin == other.builtin
            && self
                .samples
                .iter()
                .zip(&other.samples)
                .all(|(a, b)| match (a, b) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                })
    }
}

// --- parameters ----------------------------------------------------------------

/// A voice's parameters: volume, pitch, fine tune, then the sampler bytes.
const PARAMS: usize = 3 + SAMPLER_PARAMS;
const P_VOLUME: usize = 0;
const P_PITCH: usize = 1;
const P_FINE: usize = 2;
const P: usize = 3;

/// The parameter an instrument effect (0x80 and up) sets, if any.
fn fx_param(command: u8) -> Option<usize> {
    Some(match command {
        0x80..=0x82 => (command - 0x80) as usize,
        0x83 => P + S_PLAY,
        0x84..=0x91 => P + S_START + (command - 0x84) as usize,
        0x92..=0x96 => P + S_ENV + 1 + (command - 0x92) as usize,
        0x97..=0x9B => P + S_ENV + 7 + (command - 0x97) as usize,
        0x9C => P + S_LFO + 4,
        0x9D => P + S_LFO + 3,
        0x9E => P + S_LFO + 2,
        0x9F => P + S_LFO + 10,
        0xA0 => P + S_LFO + 9,
        0xA1 => P + S_LFO + 8,
        0xA2 => P + S_SLICE,
        _ => return None,
    })
}

pub const PLAY_MODES: [&str; 9] = [
    "FWD", "REV", "FWDLOOP", "REVLOOP", "FWD PP", "REV PP", "OSC", "OSC REV", "OSC PP",
];
pub const FILTERS: [&str; 6] = [
    "OFF", "LOWPASS", "HIGHPASS", "BANDPASS", "BANDSTOP", "LP>HP",
];
pub const LIMITS: [&str; 6] = ["CLIP", "SIN", "FOLD", "WRAP", "POST", "POST:AD"];
pub const MOD_DESTS: [&str; 10] = [
    "OFF", "VOLUME", "PITCH", "LOOP ST", "LENGTH", "DEGRADE", "CUTOFF", "RES", "AMP", "PAN",
];
pub const LFO_SHAPES: [&str; 20] = [
    "TRI", "SIN", "RAMP DN", "RAMP UP", "EXP DN", "EXP UP", "SQU DN", "SQU UP", "RANDOM", "DRUNK",
    "TRI T", "SIN T", "RAMPD T", "RAMPU T", "EXPD T", "EXPU T", "SQ. D T", "SQ. U T", "RAND T",
    "DRNK T",
];
pub const LFO_TRIGGERS: [&str; 4] = ["FREE", "RETRIG", "HOLD", "ONCE"];

const D_VOLUME: usize = 1;
const D_PITCH: usize = 2;
const D_LOOP: usize = 3;
const D_LENGTH: usize = 4;
const D_DEGRADE: usize = 5;
const D_CUTOFF: usize = 6;
const D_RES: usize = 7;
const D_AMP: usize = 8;
const D_PAN: usize = 9;

/// Envelope stage length for a parameter value: 0 to 10 seconds, finer at
/// the short end.
fn env_seconds(v: u8) -> f32 {
    let x = v as f32 / 255.0;
    x * x * x * 10.0
}

/// LFO rate for a parameter value: about 0.1 Hz to 150 Hz.
fn lfo_hz(v: u8) -> f32 {
    0.1 * 2f32.powf(v as f32 / 24.0)
}

/// Middle C (the M8's C-4, note 36) in Hz.
const C4_HZ: f32 = 261.625_58;
const C4: i16 = 36;

/// How often modulation and filter settings are updated, in frames.
const CONTROL: usize = 32;

// --- voices --------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Env {
    /// Seconds since the note started; `None` once finished.
    t: Option<f32>,
}

impl Env {
    fn value(&self, a: f32, h: f32, d: f32) -> f32 {
        let Some(t) = self.t else { return 0.0 };
        if t < a {
            t / a
        } else if t < a + h {
            1.0
        } else if t < a + h + d {
            1.0 - (t - a - h) / d
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Lfo {
    phase: f32,
    /// Random shapes: the value for this cycle.
    held: f32,
    /// ONCE: stopped after one cycle.
    done: bool,
}

/// A state-variable filter (TPT form), one per channel.
#[derive(Clone, Copy, Default)]
struct Svf {
    ic1: f32,
    ic2: f32,
}

#[derive(Clone, Copy, Default)]
struct SvfCoef {
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

impl SvfCoef {
    fn new(hz: f32, q: f32, sample_rate: f32) -> Self {
        let g = (PI * hz.min(sample_rate * 0.45) / sample_rate).tan();
        let k = 1.0 / q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        SvfCoef {
            a1,
            a2,
            a3: g * a2,
            k,
        }
    }
}

impl Svf {
    /// Low, band and high pass outputs.
    fn tick(&mut self, c: &SvfCoef, x: f32) -> (f32, f32, f32) {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v2, v1, x - c.k * v1 - v2)
    }
}

#[derive(Default)]
struct Voice {
    /// `None` when silent.
    sample: Option<Arc<Sample>>,
    /// Audition: the whole sample, as it is.
    raw: bool,
    inst: u8,
    note: i16,
    velocity: f32,
    /// Parameters set by instrument effects, by bit.
    over_mask: u64,
    over: Vec<u8>,
    region: (usize, usize),
    pos: f64,
    forward: bool,
    env: [Env; 2],
    lfo: [Lfo; 2],
    filter: [Svf; 2],
    crush_count: f32,
    crush_held: [f32; 2],
    /// Fading out (note off, or replaced by a new note): the gain left.
    fade: Option<f32>,
    rng: u32,
}

impl Voice {
    fn params(&self, inst: &Instrument) -> [u8; PARAMS] {
        let mut p = [0u8; PARAMS];
        p[P_VOLUME] = inst.volume;
        p[P_PITCH] = inst.pitch;
        p[P_FINE] = inst.fine_tune;
        p[P..].copy_from_slice(&inst.sampler.0);
        for (i, v) in p.iter_mut().enumerate() {
            if self.over_mask & (1 << i) != 0 {
                *v = self.over[i];
            }
        }
        p
    }

    fn apply_fx(&mut self, fx: &[Fx; 3]) {
        for f in fx {
            if let Some(i) = fx_param(f.command) {
                self.over_mask |= 1 << i;
                self.over[i] = f.value;
            }
        }
    }

    fn random(&mut self) -> f32 {
        // xorshift32
        let mut x = self.rng.max(1);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

enum Event {
    On {
        voice: usize,
        inst: u8,
        note: i16,
        velocity: f32,
        fx: [Fx; 3],
    },
    Off {
        voice: usize,
    },
    Fx {
        voice: usize,
        inst: u8,
        fx: [Fx; 3],
    },
    Audition(Option<Arc<Sample>>),
}

/// The voice that plays auditions and instrument previews.
pub const PREVIEW_VOICE: usize = TRACKS;

/// Plays every built-in sampler instrument. Lives on the audio thread.
pub struct SamplerEngine {
    set: Arc<SamplerSet>,
    voices: Vec<Voice>,
    /// Notes fading out after a new note took their voice.
    tails: Vec<Voice>,
    events: Vec<(u32, Event)>,
    sample_rate: f32,
}

impl SamplerEngine {
    pub fn new(set: Arc<SamplerSet>, sample_rate: f64) -> Self {
        let voice = |i: usize| Voice {
            rng: 0x9E37_79B9 ^ (i as u32 * 7919),
            over: vec![0; PARAMS],
            ..Voice::default()
        };
        SamplerEngine {
            set,
            voices: (0..=TRACKS).map(voice).collect(),
            tails: (0..=TRACKS).map(voice).collect(),
            events: Vec::with_capacity(256),
            sample_rate: sample_rate as f32,
        }
    }

    pub fn set_samplers(&mut self, set: Arc<SamplerSet>) {
        self.set = set;
    }

    /// Whether the built-in sampler plays `inst`.
    pub fn plays(&self, inst: u8) -> bool {
        self.set
            .builtin
            .get(inst as usize)
            .copied()
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn note_on(&mut self, time: u32, voice: usize, inst: u8, note: i16, velocity: f64) {
        self.note_on_fx(time, voice, inst, note, velocity, [Fx::default(); 3]);
    }

    /// A note with its step's instrument effects.
    pub fn note_on_fx(
        &mut self,
        time: u32,
        voice: usize,
        inst: u8,
        note: i16,
        velocity: f64,
        fx: [Fx; 3],
    ) {
        self.events.push((
            time,
            Event::On {
                voice: voice.min(TRACKS),
                inst,
                note,
                velocity: velocity as f32,
                fx,
            },
        ));
    }

    pub fn note_off(&mut self, time: u32, voice: usize) {
        self.events.push((
            time,
            Event::Off {
                voice: voice.min(TRACKS),
            },
        ));
    }

    /// Instrument effects on a step without a note: they change the note
    /// already playing.
    pub fn fx(&mut self, time: u32, voice: usize, inst: u8, fx: [Fx; 3]) {
        self.events.push((
            time,
            Event::Fx {
                voice: voice.min(TRACKS),
                inst,
                fx,
            },
        ));
    }

    /// Play a sample as it is on the preview voice, or stop it.
    pub fn audition(&mut self, sample: Option<Arc<Sample>>) {
        self.events.push((0, Event::Audition(sample)));
    }

    /// Add `frames` of every voice to `out`.
    pub fn process(&mut self, song: &Song, frames: usize, out: &mut [Vec<f32>; 2]) {
        let mut events = std::mem::take(&mut self.events);
        events.sort_by_key(|e| e.0);
        let mut next_event = 0;
        let mut done = 0;
        while done < frames {
            while next_event < events.len() && events[next_event].0 as usize <= done {
                let e = std::mem::replace(&mut events[next_event].1, Event::Audition(None));
                self.apply(e);
                next_event += 1;
            }
            let until = events
                .get(next_event)
                .map_or(frames, |e| e.0 as usize)
                .min(frames)
                .min(done + CONTROL);
            let [l, r] = out;
            let (l, r) = (&mut l[done..until], &mut r[done..until]);
            for v in self.voices.iter_mut().chain(self.tails.iter_mut()) {
                if v.sample.is_some() {
                    render(v, song, self.sample_rate, l, r);
                }
            }
            done = until;
        }
        for (_, e) in events.drain(next_event..) {
            self.apply(e);
        }
        events.clear();
        self.events = events;
    }

    fn apply(&mut self, e: Event) {
        match e {
            Event::On {
                voice,
                inst,
                note,
                velocity,
                fx,
            } => {
                let Some(sample) = self.set.samples.get(inst as usize).cloned().flatten() else {
                    self.release(voice);
                    return;
                };
                self.release(voice);
                let v = &mut self.voices[voice];
                v.sample = Some(sample);
                v.raw = false;
                v.inst = inst;
                v.note = note;
                v.velocity = velocity.clamp(0.0, 1.0);
                v.over_mask = 0;
                v.apply_fx(&fx);
                v.env = [Env { t: Some(0.0) }; 2];
                v.filter = [Svf::default(); 2];
                v.crush_count = 0.0;
                v.fade = None;
                v.region = (0, 0); // set on the first block
                v.pos = f64::NAN;
            }
            Event::Off { voice } => {
                let v = &mut self.voices[voice];
                if v.sample.is_some() && v.fade.is_none() {
                    v.fade = Some(1.0);
                }
            }
            Event::Fx { voice, inst, fx } => {
                let v = &mut self.voices[voice];
                if v.sample.is_some() && v.inst == inst {
                    v.apply_fx(&fx);
                }
            }
            Event::Audition(sample) => {
                self.release(PREVIEW_VOICE);
                if let Some(s) = sample {
                    let v = &mut self.voices[PREVIEW_VOICE];
                    v.region = (0, s.frames());
                    v.sample = Some(s);
                    v.raw = true;
                    v.pos = 0.0;
                    v.fade = None;
                }
            }
        }
    }

    /// Move a sounding voice to the tails to fade out quickly. Free-running
    /// LFOs carry on in the voice.
    fn release(&mut self, voice: usize) {
        if self.voices[voice].sample.is_none() {
            return;
        }
        std::mem::swap(&mut self.voices[voice], &mut self.tails[voice]);
        let (v, tail) = (&mut self.voices[voice], &self.tails[voice]);
        v.sample = None;
        v.lfo = tail.lfo;
        v.rng = tail.rng;
        let tail = &mut self.tails[voice];
        tail.fade = Some(tail.fade.unwrap_or(1.0));
    }
}

/// Render one control block of a voice, adding to `l` and `r`.
fn render(v: &mut Voice, song: &Song, sr: f32, l: &mut [f32], r: &mut [f32]) {
    let Some(sample) = v.sample.clone() else {
        return;
    };
    let n = l.len();
    let len = sample.frames();
    let ratio = sample.sample_rate as f32 / sr;
    let fade_step = 1.0 / (0.004 * sr);
    let ch = &sample.channels;
    let stereo = ch.len() > 1;

    if v.raw {
        for i in 0..n {
            if v.pos >= len as f64 {
                v.sample = None;
                return;
            }
            let g = fade(v, fade_step);
            let a = cubic(&ch[0], v.pos);
            let b = if stereo { cubic(&ch[1], v.pos) } else { a };
            l[i] += a * g;
            r[i] += b * g;
            v.pos += ratio as f64;
            if v.fade == Some(0.0) {
                v.sample = None;
                return;
            }
        }
        return;
    }

    let inst = &song.instruments[v.inst as usize];
    let p = v.params(inst);

    // Modulation: envelopes are 0..1, LFOs -1..1; amounts are 0..1.
    let mut m = [0f32; 10];
    let mut gain_mod = 1.0f32;
    let dt = n as f32 / sr;
    for e in 0..2 {
        let b = P + S_ENV + 6 * e;
        let (dest, amt) = (p[b] as usize, p[b + 1] as f32 / 255.0);
        let (a, h, d) = (
            env_seconds(p[b + 2]),
            env_seconds(p[b + 3]),
            env_seconds(p[b + 4]),
        );
        let val = v.env[e].value(a, h, d);
        if let Some(t) = v.env[e].t.as_mut() {
            *t += dt;
        }
        match dest {
            D_VOLUME => gain_mod *= 1.0 - amt + amt * val,
            2..=9 => m[dest] += amt * val,
            _ => {}
        }
    }
    for k in 0..2 {
        let b = P + S_LFO + 6 * k;
        let (shape, dest, trig) = (p[b] as usize % 10, p[b + 1] as usize, p[b + 2]);
        let amt = p[b + 4] as f32 / 255.0;
        let val = lfo_value(&v.lfo[k], shape);
        let lfo = &mut v.lfo[k];
        if !lfo.done {
            lfo.phase += lfo_hz(p[b + 3]) * dt;
            if lfo.phase >= 1.0 {
                lfo.phase -= lfo.phase.floor();
                if trig == 3 {
                    lfo.done = true;
                }
                let r = v.random();
                let lfo = &mut v.lfo[k];
                lfo.held = if shape == 9 {
                    (lfo.held + r * 0.3).clamp(-1.0, 1.0)
                } else {
                    r
                };
            }
        }
        match dest {
            D_VOLUME => gain_mod *= 1.0 - amt * (0.5 - 0.5 * val),
            2..=9 => m[dest] += amt * val,
            _ => {}
        }
    }
    let modded = |byte: u8, d: usize| (byte as f32 + m[d] * 255.0).clamp(0.0, 255.0);

    // Where to play.
    if v.region == (0, 0) {
        v.region = sample.region(p[P + S_SLICE], v.note);
    }
    let (r0, r1) = v.region;
    if r1 <= r0 + 1 {
        v.sample = None;
        return;
    }
    let span = (r1 - r0) as f64;
    let start = r0 as f64 + span * p[P + S_START] as f64 / 255.0;
    let end = (start + (r1 as f64 - start) * modded(p[P + S_LENGTH], D_LENGTH) as f64 / 255.0)
        .max(start + 1.0)
        .min(r1 as f64);
    let lp = (start + (end - start) * modded(p[P + S_LOOP], D_LOOP) as f64 / 255.0)
        .min(end - 1.0)
        .max(start);
    let mode = p[P + S_PLAY].min(8);

    // How fast: chain transpose and note (not when slicing), pitch, fine
    // tune and modulation, in semitones from the sample's own pitch.
    let slicing = p[P + S_SLICE] != 0;
    let mut semis =
        p[P_PITCH] as i8 as f32 + (p[P_FINE] as f32 - 128.0) / 128.0 + m[D_PITCH] * 12.0;
    if !slicing || mode >= 6 {
        semis += (v.note - C4) as f32;
    }
    let inc = if mode >= 6 {
        // Oscillator: the loop is one cycle at the note's pitch.
        let cycle = (end - lp).max(2.0) * if mode == 8 { 2.0 } else { 1.0 };
        (C4_HZ * 2f32.powf(semis / 12.0) / sr) as f64 * cycle
    } else {
        (ratio * 2f32.powf(semis / 12.0)) as f64
    };

    if v.pos.is_nan() {
        for k in 0..2 {
            if p[P + S_LFO + 6 * k + 2] != 0 {
                v.lfo[k] = Lfo {
                    held: v.lfo[k].held,
                    ..Lfo::default()
                };
            }
        }
        v.forward = !matches!(mode, 1 | 3 | 5 | 7);
        v.pos = match mode {
            0 | 2 | 4 => start,
            6 | 8 => lp,
            _ => end - 1.0,
        };
    }

    // Tone.
    let deg = modded(p[P + S_DEGRADE], D_DEGRADE) / 255.0;
    let hold = 1.0 + deg * deg * 31.0;
    let levels = 2f32.powf(16.0 - 12.0 * deg);
    let filter = p[P + S_FILTER] as usize;
    let cutoff = modded(p[P + S_CUTOFF], D_CUTOFF) / 255.0;
    let res = modded(p[P + S_RES], D_RES) / 255.0;
    let coef = SvfCoef::new(20.0 * 2f32.powf(cutoff * 10.0), 0.5 * 40f32.powf(res), sr);
    let drive = 10f32.powf(modded(p[P + S_AMP], D_AMP) / 255.0 * 24.0 / 20.0);
    let limit = p[P + S_LIMIT];
    let vol = 1.0 - p[P_VOLUME] as f32 / 255.0;
    let level = vol * vol * v.velocity * gain_mod;
    let pan = modded(p[P + S_PAN], D_PAN) / 255.0;
    let (pl, pr) = if stereo {
        ((2.0 * (1.0 - pan)).min(1.0), (2.0 * pan).min(1.0))
    } else {
        let a = pan * FRAC_PI_2;
        (
            a.cos() * std::f32::consts::SQRT_2,
            a.sin() * std::f32::consts::SQRT_2,
        )
    };

    for i in 0..n {
        let mut x = [cubic(&ch[0], v.pos), 0.0];
        x[1] = if stereo { cubic(&ch[1], v.pos) } else { x[0] };

        if deg > 0.0 {
            v.crush_count -= 1.0;
            if v.crush_count <= 0.0 {
                v.crush_count += hold;
                v.crush_held = x.map(|s| (s * levels).round() / levels);
            }
            x = v.crush_held;
        }
        for (c, s) in x.iter_mut().enumerate() {
            if filter != 0 {
                let (lo, band, hi) = v.filter[c].tick(&coef, *s);
                *s = match filter {
                    2 | 7 => hi,
                    3 | 8 => band,
                    4 | 9 => lo + hi,
                    _ => lo,
                };
            }
            *s = if limit >= 4 {
                shape(*s * drive * level, 0)
            } else {
                shape(*s * drive, limit) * level
            };
        }
        let g = fade(v, fade_step);
        l[i] += x[0] * pl * g;
        r[i] += x[1] * pr * g;
        if v.fade == Some(0.0) {
            v.sample = None;
            return;
        }

        // Advance.
        let pos = &mut v.pos;
        if v.forward {
            *pos += inc;
        } else {
            *pos -= inc;
        }
        match mode {
            0 if *pos >= end => v.sample = None,
            1 if *pos < start => v.sample = None,
            2 | 6 if *pos >= end => *pos = lp + (*pos - end) % (end - lp).max(1.0),
            3 | 7 if *pos < lp => *pos = end - 1.0 - (lp - *pos) % (end - lp).max(1.0),
            4 | 5 | 8 => {
                if v.forward && *pos >= end {
                    *pos = (2.0 * end - *pos).max(lp);
                    v.forward = false;
                } else if !v.forward && *pos < lp {
                    *pos = (2.0 * lp - *pos).min(end - 1.0);
                    v.forward = true;
                }
            }
            _ => {}
        }
        if v.sample.is_none() {
            return;
        }
    }
}

/// The fade gain for this frame; steps a fading voice down.
fn fade(v: &mut Voice, step: f32) -> f32 {
    match v.fade.as_mut() {
        Some(f) => {
            let g = *f;
            *f = (*f - step).max(0.0);
            g
        }
        None => 1.0,
    }
}

/// The limiter's shapes: CLIP, SIN, FOLD, WRAP.
fn shape(x: f32, limit: u8) -> f32 {
    match limit {
        1 => (x * FRAC_PI_2).sin(),
        2 => 1.0 - (((x + 1.0).rem_euclid(4.0)) - 2.0).abs(),
        3 => (x + 1.0).rem_euclid(2.0) - 1.0,
        _ => x.clamp(-1.0, 1.0),
    }
}

fn lfo_value(l: &Lfo, shape: usize) -> f32 {
    let p = l.phase;
    match shape {
        0 => 1.0 - 4.0 * (p - 0.5).abs(),
        1 => (2.0 * PI * p).sin(),
        2 => 1.0 - 2.0 * p,
        3 => 2.0 * p - 1.0,
        4 => 2.0 * (1.0 - p).powi(3) - 1.0,
        5 => 2.0 * p.powi(3) - 1.0,
        6 => {
            if p < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        7 => {
            if p < 0.5 {
                -1.0
            } else {
                1.0
            }
        }
        _ => l.held,
    }
}

/// The sample at fractional frame `pos`, cubic (Hermite) interpolated.
fn cubic(s: &[f32], pos: f64) -> f32 {
    let last = s.len() as isize - 1;
    if last < 0 {
        return 0.0;
    }
    let i = pos.floor() as isize;
    let t = (pos - pos.floor()) as f32;
    let at = |k: isize| s[k.clamp(0, last) as usize];
    let (y0, y1, y2, y3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::m8::format::InstrumentKind;

    /// A 16-bit WAV file's bytes, with optional cue points.
    pub fn wav(channels: &[Vec<f32>], rate: u32, cues: &[u32]) -> Vec<u8> {
        let n = channels[0].len();
        let ch = channels.len() as u16;
        let mut data = Vec::new();
        for f in 0..n {
            for c in channels {
                data.extend_from_slice(&((c[f] * 32767.0) as i16).to_le_bytes());
            }
        }
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2 * ch as u32).to_le_bytes());
        b.extend_from_slice(&(2 * ch).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        if !cues.is_empty() {
            b.extend_from_slice(b"cue ");
            b.extend_from_slice(&(4 + 24 * cues.len() as u32).to_le_bytes());
            b.extend_from_slice(&(cues.len() as u32).to_le_bytes());
            for (i, c) in cues.iter().enumerate() {
                b.extend_from_slice(&(i as u32).to_le_bytes());
                b.extend_from_slice(&c.to_le_bytes());
                b.extend_from_slice(b"data");
                b.extend_from_slice(&0u32.to_le_bytes());
                b.extend_from_slice(&0u32.to_le_bytes());
                b.extend_from_slice(&c.to_le_bytes());
            }
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        let size = (b.len() - 8) as u32;
        b[4..8].copy_from_slice(&size.to_le_bytes());
        b
    }

    /// A ramp from 0 up to just under 1, `n` frames long.
    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 / n as f32).collect()
    }

    #[test]
    fn reads_wav_files() {
        let s = parse_wav(&wav(&[ramp(100), vec![-0.5; 100]], 22_050, &[50, 10])).unwrap();
        assert_eq!(
            (s.frames(), s.channels.len(), s.sample_rate),
            (100, 2, 22_050)
        );
        assert!((s.channels[0][50] - 0.5).abs() < 1e-3);
        assert!((s.channels[1][3] + 0.5).abs() < 1e-3);
        assert_eq!(s.cues, [10, 50]);
        assert!(parse_wav(b"RIFF\0\0\0\0WAVE").is_err());
        assert!(parse_wav(b"hello").is_err());
    }

    #[test]
    fn slices() {
        let s = parse_wav(&wav(&[ramp(100)], 48_000, &[])).unwrap();
        assert_eq!(s.region(0, 40), (0, 100));
        assert_eq!(s.region(4, 0), (0, 25));
        assert_eq!(s.region(4, 2), (50, 75));
        assert_eq!(s.region(4, 5), (25, 50), "notes wrap around the slices");
        let s = parse_wav(&wav(&[ramp(100)], 48_000, &[30, 60])).unwrap();
        assert_eq!(s.region(1, 0), (0, 30));
        assert_eq!(s.region(1, 2), (60, 100));
    }

    #[test]
    fn finds_the_card_root() {
        let root = card_root(Path::new("/card/Songs/mine"), "/Songs/mine/");
        assert_eq!(root, Path::new("/card"));
        assert_eq!(
            card_root(Path::new("/home/me/music"), "/Songs/"),
            Path::new("/home/me/music")
        );
        assert_eq!(
            sample_path_for(Path::new("/card"), Path::new("/card/Samples/kick.wav")),
            "/Samples/kick.wav"
        );
        assert_eq!(
            sample_path_for(Path::new("/card"), Path::new("/else/kick.wav")),
            "/else/kick.wav"
        );
    }

    fn engine_with(sample: Sample, inst: &mut Instrument) -> (SamplerEngine, Song) {
        let mut song = Song::new_empty();
        inst.kind = InstrumentKind::Sampler;
        song.instruments[0] = inst.clone();
        let mut set = SamplerSet::default();
        set.builtin[0] = true;
        set.samples[0] = Some(Arc::new(sample));
        (SamplerEngine::new(Arc::new(set), 48_000.0), song)
    }

    fn run(e: &mut SamplerEngine, song: &Song, frames: usize) -> Vec<f32> {
        let mut out = [vec![0.0; frames], vec![0.0; frames]];
        e.process(song, frames, &mut out);
        out[0].clone()
    }

    fn sampler_inst() -> Instrument {
        let mut s = Song::new_empty();
        let mut i = s.instruments.remove(0);
        i.kind = InstrumentKind::Sampler;
        i.volume = 0;
        i.pitch = 0;
        i.fine_tune = 0x80;
        i.sampler = Default::default();
        i.sampler.0[S_PAN] = 0x80;
        i
    }

    #[test]
    fn plays_forward_reverse_and_stops() {
        let mut inst = sampler_inst();
        let s = parse_wav(&wav(&[ramp(1000)], 48_000, &[])).unwrap();
        let (mut e, mut song) = engine_with(s, &mut inst);
        e.note_on(0, 0, 0, C4, 1.0);
        let out = run(&mut e, &song, 1200);
        // At the sample's own pitch, frame by frame (centre pan is equal
        // power, so each side is at 1/sqrt(2) * sqrt(2) = 1).
        assert!((out[500] - 0.5).abs() < 0.01, "{}", out[500]);
        assert!(out[1100].abs() < 1e-6, "stops at the end");
        assert!(out[300] > out[200]);

        song.instruments[0].sampler.0[S_PLAY] = 1; // REV
        e.note_on(0, 0, 0, C4, 1.0);
        let out = run(&mut e, &song, 1200);
        assert!(out[300] < out[200], "reverse runs down the ramp");

        // An octave up plays twice as fast.
        song.instruments[0].sampler.0[S_PLAY] = 0;
        e.note_on(0, 0, 0, C4 + 12, 1.0);
        let out = run(&mut e, &song, 1200);
        assert!((out[250] - 0.5).abs() < 0.01);
        assert!(out[600].abs() < 1e-6);
    }

    #[test]
    fn loops_and_effects() {
        let mut inst = sampler_inst();
        inst.sampler.0[S_PLAY] = 2; // FWDLOOP
        inst.sampler.0[S_LOOP] = 0x80;
        let s = parse_wav(&wav(&[ramp(1000)], 48_000, &[])).unwrap();
        let (mut e, song) = engine_with(s, &mut inst);
        e.note_on(0, 0, 0, C4, 1.0);
        let out = run(&mut e, &song, 3000);
        assert!(out[2900].abs() > 0.4, "still looping in the second half");
        // STA 80 starts halfway.
        e.note_on_fx(
            0,
            0,
            0,
            C4,
            1.0,
            [
                Fx {
                    command: 0x84,
                    value: 0x80,
                },
                Fx::default(),
                Fx::default(),
            ],
        );
        let out = run(&mut e, &song, 10);
        assert!(out[5] > 0.45, "{}", out[5]);
        // A note off fades out quickly.
        e.note_off(0, 0);
        let out = run(&mut e, &song, 1000);
        assert!(out[999].abs() < 1e-6);
    }

    #[test]
    fn instrument_effect_parameters() {
        assert_eq!(fx_param(0x80), Some(P_VOLUME));
        assert_eq!(fx_param(0x83), Some(P + S_PLAY));
        assert_eq!(fx_param(0x84), Some(P + S_START));
        assert_eq!(fx_param(0x89), Some(P + S_CUTOFF));
        assert_eq!(fx_param(0x91), Some(P + 15));
        assert_eq!(fx_param(0x93), Some(P + S_ENV + 2));
        assert_eq!(fx_param(0x9A), Some(P + S_ENV + 6 + 4));
        assert_eq!(fx_param(0xA2), Some(P + S_SLICE));
        assert_eq!(fx_param(0x05), None);
    }
}
