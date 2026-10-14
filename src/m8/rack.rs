//! The audio side of the M8 player: one CLAP plugin instance per mapped M8
//! instrument, driven by the sequencer with sample-accurate note events and
//! mixed to stereo.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use super::format::{Song, INSTRUMENTS, TRACKS};
use super::instruments::{InstrumentMap, PluginRef};
use super::sampler::{self, SamplerEngine, SamplerSet};
use super::sequencer::{samples_per_tick, Pos, SeqEvent, Sequencer};
use crate::audio::{find_device, pick_config, MAX_BLOCK};
use crate::plugin::{
    discard_output_events, mark_audio_thread, EventList, HostEvent, ParamChange, PluginInstance,
    PluginLibrary, RawPlugin,
};
use crate::settings::AudioSettings;

/// One M8 instrument and the plugin playing it. Lives on the main thread.
pub struct Slot {
    pub instrument: u8,
    pub source: PluginRef,
    pub plugin: PluginInstance,
}

/// The instruments that need a plugin: those the song plays, plus those
/// given a plugin of their own, less those the built-in sampler plays.
pub fn wanted_instruments(song: &Song, map: &InstrumentMap) -> Vec<u8> {
    let mut v = song.used_instruments();
    v.extend(map.numbered());
    v.sort_unstable();
    v.dedup();
    v.retain(|&n| !map.builtin_sampler(n, &song.instruments[n as usize]));
    v
}

/// Instantiate a plugin for every instrument the song plays. Returns the
/// slots and, for instruments without a plugin, why not.
pub fn load_slots(song: &Song, map: &InstrumentMap) -> (Vec<Slot>, Vec<(u8, String)>) {
    load_slots_reusing(song, map, Vec::new())
}

/// Like [`load_slots`], keeping plugins from `old` whose instrument still
/// has the same plugin, so their settings carry over.
pub fn load_slots_reusing(
    song: &Song,
    map: &InstrumentMap,
    old: Vec<Slot>,
) -> (Vec<Slot>, Vec<(u8, String)>) {
    let mut old: HashMap<u8, Slot> = old.into_iter().map(|s| (s.instrument, s)).collect();
    let mut libs: HashMap<PathBuf, Arc<PluginLibrary>> = HashMap::new();
    let mut slots = Vec::new();
    let mut missing = Vec::new();
    for n in wanted_instruments(song, map) {
        let Some(source) = map.resolve(n, &song.instruments[n as usize]) else {
            missing.push((n, "no plugin mapped".to_string()));
            continue;
        };
        if let Some(slot) = old.remove(&n).filter(|s| &s.source == source) {
            slots.push(slot);
            continue;
        }
        match instantiate(source, &mut libs) {
            Ok(plugin) => slots.push(Slot {
                instrument: n,
                source: source.clone(),
                plugin,
            }),
            Err(e) => missing.push((n, format!("{e:#}"))),
        }
    }
    (slots, missing)
}

fn instantiate(
    source: &PluginRef,
    libs: &mut HashMap<PathBuf, Arc<PluginLibrary>>,
) -> Result<PluginInstance> {
    let lib = match libs.get(&source.path) {
        Some(l) => l.clone(),
        None => {
            let l = PluginLibrary::load(&source.path)?;
            libs.insert(source.path.clone(), l.clone());
            l
        }
    };
    let id = match &source.id {
        Some(id) => id.clone(),
        None => lib
            .descriptors()?
            .first()
            .map(|d| d.id.clone())
            .ok_or_else(|| anyhow!("{} contains no plugins", source.path.display()))?,
    };
    PluginInstance::new(lib, &id)
}

enum Command {
    Play(usize),
    PlayChain(usize, usize),
    PlayPhrase(usize, Pos),
    Stop,
    Song(Arc<Song>),
    Mutes(u8, u8),
    /// Sound a note on an instrument outside the sequencer: (instrument,
    /// M8 note, on).
    Preview(u8, i16, bool),
    Param(u8, ParamChange),
    Samplers(Arc<SamplerSet>),
    Audition(Option<Arc<sampler::Sample>>),
}

/// State shared between the UI and the audio thread.
pub struct RackShared {
    commands: Mutex<Vec<Command>>,
    stop: AtomicBool,
    stopped: AtomicBool,
    peak: AtomicU32,
    tempo: AtomicU32,
    /// Per track: the packed position and note, see [`TrackView`].
    tracks: [AtomicU64; TRACKS],
    pub process_errors: AtomicU32,
}

/// What a track is doing, as the UI sees it.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackView {
    pub pos: Option<Pos>,
    /// The sounding M8 note and instrument.
    pub note: Option<(u8, u8)>,
}

impl TrackView {
    fn pack(pos: Option<Pos>, note: Option<(u8, i16)>) -> u64 {
        let mut v = 0u64;
        if let Some(p) = pos {
            v |= 1 << 63
                | (p.row as u64) << 32
                | (p.chain as u64) << 24
                | (p.chain_step as u64) << 20
                | (p.phrase as u64) << 12
                | (p.phrase_step as u64) << 8;
        }
        if let Some((inst, note)) = note {
            v |= 1 << 62 | (inst as u64) << 40 | (note.clamp(0, 255) as u64);
        }
        v
    }

    fn unpack(v: u64) -> Self {
        let pos = (v >> 63 & 1 == 1).then_some(Pos {
            row: (v >> 32 & 0xFF) as usize,
            chain: (v >> 24 & 0xFF) as usize,
            chain_step: (v >> 20 & 0xF) as usize,
            phrase: (v >> 12 & 0xFF) as usize,
            phrase_step: (v >> 8 & 0xF) as usize,
        });
        let note = (v >> 62 & 1 == 1).then_some(((v & 0xFF) as u8, (v >> 40 & 0xFF) as u8));
        TrackView { pos, note }
    }
}

impl RackShared {
    fn new(tempo: f32) -> Arc<Self> {
        Arc::new(RackShared {
            commands: Mutex::new(Vec::with_capacity(16)),
            stop: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            tempo: AtomicU32::new(tempo.to_bits()),
            tracks: Default::default(),
            process_errors: AtomicU32::new(0),
        })
    }

    fn send(&self, c: Command) {
        if let Ok(mut q) = self.commands.lock() {
            q.push(c);
        }
    }

    /// Play all tracks from song row `row`.
    pub fn play_from(&self, row: usize) {
        self.send(Command::Play(row));
    }

    /// Loop the chain on `row` of `track`.
    pub fn play_chain(&self, track: usize, row: usize) {
        self.send(Command::PlayChain(track, row));
    }

    /// Loop one phrase on `track`.
    pub fn play_phrase(&self, track: usize, pos: Pos) {
        self.send(Command::PlayPhrase(track, pos));
    }

    pub fn stop_song(&self) {
        self.send(Command::Stop);
    }

    /// Hand the audio thread an edited song.
    pub fn set_song(&self, song: Arc<Song>) {
        self.send(Command::Song(song));
    }

    pub fn set_mutes(&self, muted: u8, soloed: u8) {
        self.send(Command::Mutes(muted, soloed));
    }

    pub fn preview(&self, instrument: u8, note: i16, on: bool) {
        self.send(Command::Preview(instrument, note, on));
    }

    pub fn set_param(&self, instrument: u8, change: ParamChange) {
        self.send(Command::Param(instrument, change));
    }

    /// Hand the audio thread new samples for the built-in sampler.
    pub fn set_samplers(&self, set: Arc<SamplerSet>) {
        self.send(Command::Samplers(set));
    }

    /// Play a sample file as it is, or stop the one playing.
    pub fn audition(&self, sample: Option<Arc<sampler::Sample>>) {
        self.send(Command::Audition(sample));
    }

    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::AcqRel))
    }

    pub fn tempo(&self) -> f32 {
        f32::from_bits(self.tempo.load(Ordering::Relaxed))
    }

    pub fn track(&self, t: usize) -> TrackView {
        TrackView::unpack(self.tracks[t].load(Ordering::Relaxed))
    }

    pub fn is_playing(&self) -> bool {
        (0..TRACKS).any(|t| self.track(t).pos.is_some())
    }
}

/// One plugin's buffers and events; lives on the audio thread.
struct SlotProc {
    plugin: RawPlugin,
    events: EventList,
    // Backing storage, then the pointer tables CLAP wants. The inner Vecs'
    // heap blocks never move after construction.
    data: Vec<Vec<f32>>,
    _ptrs: Vec<Vec<*mut f32>>,
    in_bufs: Vec<clap_audio_buffer>,
    out_bufs: Vec<clap_audio_buffer>,
    /// Indexes into `_data` of the main output's channels.
    main_out: Vec<usize>,
    started: bool,
}

impl SlotProc {
    fn new(p: &PluginInstance) -> Self {
        let (ins, outs) = p.audio_ports();
        let mut data = Vec::new();
        let mut ptrs = Vec::new();
        let mut main_out = Vec::new();
        let mut add = |ports: &[u32], is_out: bool| -> Vec<(usize, u32)> {
            ports
                .iter()
                .enumerate()
                .map(|(i, &ch)| {
                    let mut port_ptrs = Vec::new();
                    for _ in 0..ch {
                        if is_out && i == 0 {
                            main_out.push(data.len());
                        }
                        let mut v = vec![0.0f32; MAX_BLOCK];
                        port_ptrs.push(v.as_mut_ptr());
                        data.push(v);
                    }
                    ptrs.push(port_ptrs);
                    (ptrs.len() - 1, ch)
                })
                .collect()
        };
        let ins = add(&ins, false);
        let outs = add(&outs, true);
        let buf = |ptrs: &mut Vec<Vec<*mut f32>>, (i, ch): (usize, u32)| clap_audio_buffer {
            data32: ptrs[i].as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: ch,
            latency: 0,
            constant_mask: 0,
        };
        let in_bufs = ins.into_iter().map(|x| buf(&mut ptrs, x)).collect();
        let out_bufs = outs.into_iter().map(|x| buf(&mut ptrs, x)).collect();
        SlotProc {
            plugin: p.raw(),
            events: EventList::new(p.note_input()),
            data,
            _ptrs: ptrs,
            in_bufs,
            out_bufs,
            main_out,
            started: false,
        }
    }

    fn process(&mut self, frames: usize, steady_time: i64) -> bool {
        let p = self.plugin.0;
        if !self.started {
            self.started = match unsafe { (*p).start_processing } {
                Some(f) => unsafe { f(p) },
                None => true,
            };
            if !self.started {
                return false;
            }
        }
        let in_events = self.events.as_input();
        let out_events = discard_output_events();
        let process = clap_process {
            steady_time,
            frames_count: frames as u32,
            transport: std::ptr::null(),
            audio_inputs: self.in_bufs.as_ptr(),
            audio_outputs: self.out_bufs.as_mut_ptr(),
            audio_inputs_count: self.in_bufs.len() as u32,
            audio_outputs_count: self.out_bufs.len() as u32,
            in_events: &in_events,
            out_events: &out_events,
        };
        let status = match unsafe { (*p).process } {
            Some(f) => unsafe { f(p, &process) },
            None => CLAP_PROCESS_ERROR,
        };
        status != CLAP_PROCESS_ERROR
    }

    fn stop(&mut self) {
        if self.started {
            if let Some(f) = unsafe { (*self.plugin.0).stop_processing } {
                unsafe { f(self.plugin.0) };
            }
            self.started = false;
        }
    }

    fn channel(&self, c: usize) -> Option<&[f32]> {
        if self.main_out.is_empty() {
            return None;
        }
        Some(&self.data[self.main_out[c % self.main_out.len()]])
    }
}

/// The sequencer plus every plugin; renders the mix. Lives on the audio
/// thread.
pub struct Processor {
    seq: Sequencer,
    sampler: SamplerEngine,
    slots: Vec<SlotProc>,
    slot_of: [Option<usize>; INSTRUMENTS],
    shared: Arc<RackShared>,
    sample_rate: f64,
    /// Added to M8 note numbers to get MIDI keys.
    note_offset: i16,
    /// Samples until the next sequencer tick.
    until_tick: f64,
    seq_events: Vec<SeqEvent>,
    mix: [Vec<f32>; 2],
    steady_time: i64,
}

unsafe impl Send for Processor {}

impl Processor {
    fn new(
        song: Arc<Song>,
        slots: &[Slot],
        samplers: Arc<SamplerSet>,
        shared: Arc<RackShared>,
        sample_rate: f64,
        note_offset: i16,
    ) -> Self {
        let mut slot_of = [None; INSTRUMENTS];
        for (i, s) in slots.iter().enumerate() {
            slot_of[s.instrument as usize] = Some(i);
        }
        Processor {
            seq: Sequencer::new(song),
            sampler: SamplerEngine::new(samplers, sample_rate),
            slots: slots.iter().map(|s| SlotProc::new(&s.plugin)).collect(),
            slot_of,
            shared,
            sample_rate,
            note_offset,
            until_tick: 0.0,
            seq_events: Vec::with_capacity(64),
            mix: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            steady_time: 0,
        }
    }

    /// Render interleaved audio into `out` with `channels` channels.
    fn render<T: Sample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        let frames_total = out.len() / channels.max(1);
        let mut done = 0;
        while done < frames_total {
            let n = (frames_total - done).min(MAX_BLOCK);
            self.process_block(n);
            let mut peak = 0.0f32;
            for f in 0..n {
                for c in 0..channels {
                    let s = self.mix[c % 2][f];
                    peak = peak.max(s.abs());
                    out[(done + f) * channels + c] = T::from_sample(s);
                }
            }
            let prev = f32::from_bits(self.shared.peak.load(Ordering::Relaxed));
            if peak > prev {
                self.shared.peak.store(peak.to_bits(), Ordering::Relaxed);
            }
            done += n;
        }
    }

    fn process_block(&mut self, frames: usize) {
        mark_audio_thread();
        for m in &mut self.mix {
            m[..frames].fill(0.0);
        }
        if self.shared.stop.load(Ordering::Acquire) {
            for s in &mut self.slots {
                s.stop();
            }
            self.shared.stopped.store(true, Ordering::Release);
            return;
        }

        for s in &mut self.slots {
            s.events.clear();
        }
        if let Ok(mut cmds) = self.shared.commands.try_lock() {
            for c in cmds.drain(..) {
                match c {
                    Command::Play(row) => {
                        self.seq.play_from(row, &mut self.seq_events);
                        self.until_tick = 0.0;
                    }
                    Command::PlayChain(track, row) => {
                        self.seq.play_chain(track, row, &mut self.seq_events);
                        self.until_tick = 0.0;
                    }
                    Command::PlayPhrase(track, pos) => {
                        self.seq.play_phrase(track, pos, &mut self.seq_events);
                        self.until_tick = 0.0;
                    }
                    Command::Stop => self.seq.stop(&mut self.seq_events),
                    Command::Song(song) => self.seq.set_song(song),
                    Command::Mutes(m, s) => self.seq.set_mutes(m, s),
                    Command::Preview(inst, note, on) => self.seq_events.push(if on {
                        SeqEvent::NoteOn {
                            track: TRACKS as u8,
                            instrument: inst,
                            note,
                            velocity: 0.8,
                            fx: Default::default(),
                        }
                    } else {
                        SeqEvent::NoteOff {
                            track: TRACKS as u8,
                            instrument: inst,
                            note,
                        }
                    }),
                    Command::Samplers(set) => self.sampler.set_samplers(set),
                    Command::Audition(sample) => self.sampler.audition(sample),
                    Command::Param(inst, change) => {
                        if let Some(i) = self.slot_of[inst as usize] {
                            self.slots[i].events.push_at(HostEvent::Param(change), 0);
                        }
                    }
                }
            }
        }
        self.dispatch(0);

        let mut t = self.until_tick;
        while self.seq.is_playing() && t < frames as f64 {
            self.seq.tick(&mut self.seq_events);
            self.dispatch(t as u32);
            t += samples_per_tick(self.seq.tempo(), self.sample_rate);
        }
        self.until_tick = (t - frames as f64).max(0.0);

        self.sampler.process(self.seq.song(), frames, &mut self.mix);

        for s in &mut self.slots {
            if !s.process(frames, self.steady_time) {
                self.shared.process_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            for (c, m) in self.mix.iter_mut().enumerate() {
                if let Some(src) = s.channel(c) {
                    for (d, x) in m[..frames].iter_mut().zip(src) {
                        *d += *x;
                    }
                }
            }
        }
        self.steady_time += frames as i64;
        self.publish();
    }

    /// Hand pending sequencer events to their plugins, `time` frames into
    /// the block.
    fn dispatch(&mut self, time: u32) {
        for e in self.seq_events.drain(..) {
            let (inst, note, on, velocity) = match e {
                SeqEvent::NoteOn {
                    track,
                    instrument,
                    note,
                    velocity,
                    fx,
                } => {
                    if self.sampler.plays(instrument) {
                        let v = track as usize;
                        self.sampler
                            .note_on_fx(time, v, instrument, note, velocity, fx);
                        continue;
                    }
                    (instrument, note, true, velocity)
                }
                SeqEvent::NoteOff {
                    track,
                    instrument,
                    note,
                } => {
                    if self.sampler.plays(instrument) {
                        self.sampler.note_off(time, track as usize);
                        continue;
                    }
                    (instrument, note, false, 0.0)
                }
                SeqEvent::Fx {
                    track,
                    instrument,
                    fx,
                } => {
                    if self.sampler.plays(instrument) {
                        self.sampler.fx(time, track as usize, instrument, fx);
                    }
                    continue;
                }
            };
            let Some(i) = self.slot_of[inst as usize] else {
                continue;
            };
            let key = (note + self.note_offset).clamp(0, 127);
            self.slots[i]
                .events
                .push_at(HostEvent::Note { key, on, velocity }, time);
        }
    }

    fn publish(&self) {
        self.shared
            .tempo
            .store((self.seq.tempo() as f32).to_bits(), Ordering::Relaxed);
        for t in 0..TRACKS {
            let v = TrackView::pack(self.seq.position(t), self.seq.sounding(t));
            self.shared.tracks[t].store(v, Ordering::Relaxed);
        }
    }
}

enum Driver {
    Device(cpal::Stream),
    Thread(JoinHandle<()>),
}

/// A running player: plugins activated, audio flowing.
pub struct Rack {
    pub shared: Arc<RackShared>,
    pub sample_rate: u32,
    pub output_name: String,
    driver: Option<Driver>,
}

impl Rack {
    /// Start playing through the sound card, or silently on a timer when
    /// `use_device` is false or the device can't be opened.
    pub fn start(
        song: Arc<Song>,
        slots: &mut [Slot],
        samplers: Arc<SamplerSet>,
        use_device: bool,
        settings: &AudioSettings,
        note_offset: i16,
    ) -> Result<(Rack, Option<String>)> {
        let shared = RackShared::new(song.tempo);
        let mut problem = None;
        if use_device {
            let s = (song.clone(), samplers.clone());
            match Self::start_device(s, slots, shared.clone(), settings, note_offset) {
                Ok(r) => return Ok((r, None)),
                Err(e) => {
                    deactivate(slots);
                    problem = Some(format!("{e:#}"));
                }
            }
        }
        let r = Self::start_timer((song, samplers), slots, shared, note_offset)?;
        Ok((r, problem))
    }

    fn start_device(
        (song, samplers): (Arc<Song>, Arc<SamplerSet>),
        slots: &mut [Slot],
        shared: Arc<RackShared>,
        settings: &AudioSettings,
        note_offset: i16,
    ) -> Result<Rack> {
        let device = find_device(settings.device.as_deref())?;
        let supported = pick_config(&device, settings.sample_rate)?;
        let format = supported.sample_format();
        let mut config: cpal::StreamConfig = supported.into();
        if let Some(frames) = settings.buffer_size {
            config.buffer_size = cpal::BufferSize::Fixed(frames);
        }
        let sample_rate = config.sample_rate.0;
        let channels = config.channels as usize;
        let output_name = device.name().unwrap_or_else(|_| "default output".into());
        activate(slots, sample_rate)?;
        let p = Processor::new(
            song,
            slots,
            samplers,
            shared.clone(),
            sample_rate as f64,
            note_offset,
        );
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, p, channels),
            cpal::SampleFormat::F64 => build::<f64>(&device, config, p, channels),
            cpal::SampleFormat::I16 => build::<i16>(&device, config, p, channels),
            cpal::SampleFormat::I32 => build::<i32>(&device, config, p, channels),
            cpal::SampleFormat::U16 => build::<u16>(&device, config, p, channels),
            other => Err(anyhow!("unsupported sample format {other}")),
        }?;
        stream.play()?;
        Ok(Rack {
            shared,
            sample_rate,
            output_name,
            driver: Some(Driver::Device(stream)),
        })
    }

    /// Process in real time without a sound card.
    fn start_timer(
        (song, samplers): (Arc<Song>, Arc<SamplerSet>),
        slots: &mut [Slot],
        shared: Arc<RackShared>,
        note_offset: i16,
    ) -> Result<Rack> {
        let sample_rate = 48_000;
        activate(slots, sample_rate)?;
        let mut p = Processor::new(
            song,
            slots,
            samplers,
            shared.clone(),
            sample_rate as f64,
            note_offset,
        );
        let block = 512usize;
        let thread_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("lextrack-m8-audio".into())
            .spawn(move || {
                let mut buf = vec![0.0f32; block * 2];
                let period = Duration::from_secs_f64(block as f64 / sample_rate as f64);
                let mut next = Instant::now();
                loop {
                    p.render(&mut buf, 2);
                    if thread_shared.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    next += period;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            })?;
        Ok(Rack {
            shared,
            sample_rate,
            output_name: "none (silent)".into(),
            driver: Some(Driver::Thread(handle)),
        })
    }

    /// Stop the audio thread and deactivate the plugins.
    pub fn stop(&mut self, slots: &mut [Slot]) {
        self.shared.stop_song();
        // Give the audio thread a block to deliver the note-offs.
        std::thread::sleep(Duration::from_millis(30));
        self.shared.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_millis(1000);
        while !self.shared.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        match self.driver.take() {
            Some(Driver::Device(s)) => drop(s),
            Some(Driver::Thread(h)) => {
                let _ = h.join();
            }
            None => {}
        }
        deactivate(slots);
    }
}

fn activate(slots: &mut [Slot], sample_rate: u32) -> Result<()> {
    for s in slots.iter_mut() {
        if let Err(e) = s.plugin.activate(sample_rate as f64, 1, MAX_BLOCK as u32) {
            deactivate(slots);
            return Err(e);
        }
    }
    Ok(())
}

fn deactivate(slots: &mut [Slot]) {
    for s in slots {
        s.plugin.deactivate();
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut p: Processor,
    channels: usize,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let errors = p.shared.clone();
    Ok(device.build_output_stream(
        &config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| p.render(data, channels),
        move |_err| {
            errors.process_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?)
}

/// Render `seconds` of the song from `row` into a 16-bit stereo WAV file,
/// plus a short tail after the notes stop. Runs faster than real time.
#[allow(clippy::too_many_arguments)]
pub fn render_wav(
    song: Arc<Song>,
    slots: &mut [Slot],
    samplers: Arc<SamplerSet>,
    row: usize,
    seconds: f64,
    sample_rate: u32,
    note_offset: i16,
    path: &Path,
) -> Result<()> {
    let shared = RackShared::new(song.tempo);
    activate(slots, sample_rate)?;
    let p = Processor::new(
        song,
        slots,
        samplers,
        shared.clone(),
        sample_rate as f64,
        note_offset,
    );
    shared.play_from(row);
    let total = (seconds * sample_rate as f64) as usize;
    let tail = sample_rate as usize;
    // Plugins expect process() on a thread other than the main one.
    let worker = std::thread::spawn(move || {
        let mut p = p;
        let mut samples = Vec::with_capacity((total + tail) * 2);
        let mut buf = vec![0.0f32; MAX_BLOCK * 2];
        let mut done = 0;
        let mut stopped = false;
        while done < total + tail {
            if done >= total && !stopped {
                p.shared.stop_song();
                stopped = true;
            }
            let n = (total + tail - done).min(MAX_BLOCK);
            p.render(&mut buf[..n * 2], 2);
            samples.extend_from_slice(&buf[..n * 2]);
            done += n;
        }
        p.shared.stop.store(true, Ordering::Release);
        p.render(&mut buf[..2], 2);
        samples
    });
    let samples = worker
        .join()
        .map_err(|_| anyhow!("the audio thread panicked"))?;
    deactivate(slots);
    write_wav(path, &samples, sample_rate)?;
    Ok(())
}

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&2u16.to_le_bytes()); // channels
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_view_round_trips() {
        let pos = Pos {
            row: 255,
            chain: 254,
            chain_step: 15,
            phrase: 200,
            phrase_step: 9,
        };
        let v = TrackView::unpack(TrackView::pack(Some(pos), Some((0x7F, 60))));
        assert_eq!(v.pos, Some(pos));
        assert_eq!(v.note, Some((60, 0x7F)));
        let v = TrackView::unpack(TrackView::pack(None, None));
        assert!(v.pos.is_none() && v.note.is_none());
    }
}
