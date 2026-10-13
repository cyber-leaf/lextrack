//! The audio side: drives `clap_plugin.process` from a sound card callback
//! (via cpal), or from a silent timer thread when no device is available.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::process::clap_process;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use crate::plugin::{
    discard_output_events, mark_audio_thread, EventList, HostShared, ParamChange, PluginInstance,
    RawPlugin,
};

/// Largest block handed to the plugin in a single `process` call.
pub const MAX_BLOCK: usize = 1024;

/// State shared between the UI and the audio thread.
pub struct EngineShared {
    /// Parameter changes waiting for the next audio block.
    pub queue: Mutex<Vec<ParamChange>>,
    stop: AtomicBool,
    stopped: AtomicBool,
    /// Peak output level of the last blocks, as f32 bits. Decays in the UI.
    pub peak: AtomicU32,
    pub process_errors: AtomicU32,
}

impl EngineShared {
    pub fn send(&self, change: ParamChange) {
        if let Ok(mut q) = self.queue.lock() {
            q.push(change);
        }
    }

    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::AcqRel))
    }
}

enum Driver {
    Device(cpal::Stream),
    Null(JoinHandle<()>),
}

pub struct Engine {
    pub shared: Arc<EngineShared>,
    pub sample_rate: u32,
    pub output_name: String,
    driver: Option<Driver>,
}

/// Owns the plugin's audio buffers; lives on the audio thread.
struct Processor {
    plugin: RawPlugin,
    shared: Arc<EngineShared>,
    events: EventList,
    // Backing storage, then the pointer tables CLAP wants. The heap blocks of
    // the inner Vecs never move after construction.
    _in_data: Vec<Vec<f32>>,
    out_data: Vec<Vec<f32>>,
    _in_ptrs: Vec<Vec<*mut f32>>,
    _out_ptrs: Vec<Vec<*mut f32>>,
    in_bufs: Vec<clap_audio_buffer>,
    out_bufs: Vec<clap_audio_buffer>,
    main_out_channels: usize,
    started: bool,
    steady_time: i64,
}

unsafe impl Send for Processor {}

impl Processor {
    fn new(plugin: &PluginInstance, shared: Arc<EngineShared>) -> Self {
        let (ins, outs) = plugin.audio_ports();
        let alloc = |ports: &[u32]| -> (Vec<Vec<f32>>, Vec<Vec<*mut f32>>, Vec<clap_audio_buffer>) {
            let mut data = Vec::new();
            let mut ptrs = Vec::new();
            for &ch in ports {
                let mut port_ptrs = Vec::new();
                for _ in 0..ch {
                    let mut v = vec![0.0f32; MAX_BLOCK];
                    port_ptrs.push(v.as_mut_ptr());
                    data.push(v);
                }
                ptrs.push(port_ptrs);
            }
            let bufs = ptrs
                .iter_mut()
                .map(|p: &mut Vec<*mut f32>| clap_audio_buffer {
                    data32: p.as_mut_ptr(),
                    data64: std::ptr::null_mut(),
                    channel_count: p.len() as u32,
                    latency: 0,
                    constant_mask: 0,
                })
                .collect();
            (data, ptrs, bufs)
        };
        let (in_data, in_ptrs, in_bufs) = alloc(&ins);
        let (out_data, out_ptrs, out_bufs) = alloc(&outs);
        Processor {
            plugin: plugin.raw(),
            shared,
            events: EventList::new(),
            _in_data: in_data,
            out_data,
            _in_ptrs: in_ptrs,
            _out_ptrs: out_ptrs,
            in_bufs,
            out_bufs,
            main_out_channels: outs.first().copied().unwrap_or(0) as usize,
            started: false,
            steady_time: 0,
        }
    }

    /// Render interleaved audio into `out` with `channels` channels.
    fn render<T: Sample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        let frames_total = out.len() / channels.max(1);
        let mut done = 0;
        while done < frames_total {
            let n = (frames_total - done).min(MAX_BLOCK);
            let ok = self.process_block(n);
            let mut peak = 0.0f32;
            for f in 0..n {
                for c in 0..channels {
                    let s = if ok && self.main_out_channels > 0 {
                        self.out_data[c % self.main_out_channels][f]
                    } else {
                        0.0
                    };
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

    fn process_block(&mut self, frames: usize) -> bool {
        mark_audio_thread();
        let p = self.plugin.0;
        if self.shared.stop.load(Ordering::Acquire) {
            if self.started {
                if let Some(f) = unsafe { (*p).stop_processing } {
                    unsafe { f(p) };
                }
                self.started = false;
            }
            self.shared.stopped.store(true, Ordering::Release);
            return false;
        }
        if !self.started {
            let ok = match unsafe { (*p).start_processing } {
                Some(f) => unsafe { f(p) },
                None => true,
            };
            if !ok {
                return false;
            }
            self.started = true;
        }

        self.events.clear();
        if let Ok(mut q) = self.shared.queue.try_lock() {
            for c in q.drain(..) {
                self.events.push(c);
            }
        }
        let in_events = self.events.as_input();
        let out_events = discard_output_events();
        let process = clap_process {
            steady_time: self.steady_time,
            frames_count: frames as u32,
            transport: std::ptr::null(),
            audio_inputs: self.in_bufs.as_ptr(),
            audio_outputs: self.out_bufs.as_mut_ptr(),
            audio_inputs_count: self.in_bufs.len() as u32,
            audio_outputs_count: self.out_bufs.len() as u32,
            in_events: &in_events,
            out_events: &out_events,
        };
        self.steady_time += frames as i64;
        let status = match unsafe { (*p).process } {
            Some(f) => unsafe { f(p, &process) },
            None => 0,
        };
        if status == clap_sys::process::CLAP_PROCESS_ERROR {
            self.shared.process_errors.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

impl Engine {
    /// Activate the plugin and start processing. Falls back to a silent
    /// timer-driven engine when `use_device` is false or no device works.
    pub fn start(plugin: &mut PluginInstance, use_device: bool) -> Result<Engine> {
        let shared = Arc::new(EngineShared {
            queue: Mutex::new(Vec::with_capacity(256)),
            stop: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            process_errors: AtomicU32::new(0),
        });
        if use_device {
            match Self::start_device(plugin, shared.clone()) {
                Ok(e) => return Ok(e),
                Err(err) => plugin.shared.push_log(format!(
                    "[host] audio device unavailable ({err:#}); running without sound"
                )),
            }
        }
        Self::start_null(plugin, shared)
    }

    fn start_device(plugin: &mut PluginInstance, shared: Arc<EngineShared>) -> Result<Engine> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let sample_rate = config.sample_rate;
        let channels = config.channels as usize;
        let output_name = device
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "default output".into());

        plugin.activate(sample_rate as f64, 1, MAX_BLOCK as u32)?;
        let processor = Processor::new(plugin, shared.clone());
        let log = plugin.shared.clone();

        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, processor, channels, log),
            cpal::SampleFormat::F64 => build::<f64>(&device, config, processor, channels, log),
            cpal::SampleFormat::I16 => build::<i16>(&device, config, processor, channels, log),
            cpal::SampleFormat::I32 => build::<i32>(&device, config, processor, channels, log),
            cpal::SampleFormat::U16 => build::<u16>(&device, config, processor, channels, log),
            other => Err(anyhow!("unsupported sample format {other}")),
        };
        let stream = match stream.and_then(|s| s.play().map(|_| s).map_err(Into::into)) {
            Ok(s) => s,
            Err(e) => {
                plugin.deactivate();
                return Err(e);
            }
        };
        Ok(Engine {
            shared,
            sample_rate,
            output_name,
            driver: Some(Driver::Device(stream)),
        })
    }

    fn start_null(plugin: &mut PluginInstance, shared: Arc<EngineShared>) -> Result<Engine> {
        let sample_rate = 48_000;
        plugin.activate(sample_rate as f64, 1, MAX_BLOCK as u32)?;
        let mut processor = Processor::new(plugin, shared.clone());
        let block = 512usize;
        let thread_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("lextrack-audio".into())
            .spawn(move || {
                let mut buf = vec![0.0f32; block * 2];
                let period = Duration::from_secs_f64(block as f64 / sample_rate as f64);
                let mut next = Instant::now();
                loop {
                    processor.render(&mut buf, 2);
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
        Ok(Engine {
            shared,
            sample_rate,
            output_name: "none (silent)".into(),
            driver: Some(Driver::Null(handle)),
        })
    }

    pub fn is_silent(&self) -> bool {
        matches!(self.driver, Some(Driver::Null(_)))
    }

    /// Stop processing (on the audio thread) and deactivate the plugin.
    pub fn stop(&mut self, plugin: &mut PluginInstance) {
        self.shared.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_millis(1000);
        while !self.shared.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        match self.driver.take() {
            Some(Driver::Device(stream)) => drop(stream),
            Some(Driver::Null(h)) => {
                let _ = h.join();
            }
            None => {}
        }
        plugin.deactivate();
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut p: Processor,
    channels: usize,
    log: Arc<HostShared>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| p.render(data, channels),
        move |err| log.push_log(format!("[host] audio stream error: {err}")),
        None,
    )?;
    Ok(stream)
}
