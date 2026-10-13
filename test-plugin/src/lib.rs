//! A tiny CLAP plugin for trying out lextrack: a test-tone generator with a
//! mix of continuous, stepped, boolean, read-only and hidden parameters.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::params::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::version::CLAP_VERSION;

const ID: &CStr = c"org.lextrack.test-tone";

struct Features([*const c_char; 3]);
unsafe impl Sync for Features {}
static FEATURES: Features =
    Features([c"instrument".as_ptr(), c"synthesizer".as_ptr(), ptr::null()]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: ID.as_ptr(),
    name: c"Test Tone".as_ptr(),
    vendor: c"lextrack".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"0.1.0".as_ptr(),
    description: c"Sine/saw/square test tone".as_ptr(),
    features: &FEATURES.0 as *const _ as *const *const c_char,
};

// Parameter ids.
const P_LEVEL: clap_id = 0;
const P_FREQ: clap_id = 1;
const P_WAVE: clap_id = 2;
const P_MUTE: clap_id = 3;
const P_PAN: clap_id = 4;
const P_DETUNE: clap_id = 5;
const P_PEAK: clap_id = 6;
const P_HIDDEN: clap_id = 7;

struct ParamDef {
    id: clap_id,
    name: &'static str,
    module: &'static str,
    min: f64,
    max: f64,
    default: f64,
    flags: u32,
}

const AUTO: u32 = CLAP_PARAM_IS_AUTOMATABLE;
const STEP: u32 = CLAP_PARAM_IS_STEPPED;
const PARAMS: &[ParamDef] = &[
    ParamDef {
        id: P_LEVEL,
        name: "Level",
        module: "Output",
        min: -60.0,
        max: 0.0,
        default: -18.0,
        flags: AUTO,
    },
    ParamDef {
        id: P_PAN,
        name: "Pan",
        module: "Output",
        min: -1.0,
        max: 1.0,
        default: 0.0,
        flags: AUTO,
    },
    ParamDef {
        id: P_MUTE,
        name: "Mute",
        module: "Output",
        min: 0.0,
        max: 1.0,
        default: 0.0,
        flags: AUTO | STEP,
    },
    ParamDef {
        id: P_FREQ,
        name: "Frequency",
        module: "Oscillator",
        min: 20.0,
        max: 2000.0,
        default: 220.0,
        flags: AUTO,
    },
    ParamDef {
        id: P_WAVE,
        name: "Waveform",
        module: "Oscillator",
        min: 0.0,
        max: 2.0,
        default: 0.0,
        flags: AUTO | STEP | CLAP_PARAM_IS_ENUM,
    },
    ParamDef {
        id: P_DETUNE,
        name: "Detune",
        module: "Oscillator",
        min: -12.0,
        max: 12.0,
        default: 0.0,
        flags: AUTO | STEP,
    },
    ParamDef {
        id: P_PEAK,
        name: "Peak",
        module: "Meter",
        min: 0.0,
        max: 1.0,
        default: 0.0,
        flags: CLAP_PARAM_IS_READONLY,
    },
    ParamDef {
        id: P_HIDDEN,
        name: "Secret",
        module: "",
        min: 0.0,
        max: 1.0,
        default: 0.5,
        flags: CLAP_PARAM_IS_HIDDEN,
    },
];

const WAVES: [&str; 3] = ["Sine", "Saw", "Square"];

struct Plugin {
    raw: clap_plugin,
    values: [AtomicU64; 8],
    sample_rate: f64,
    phase: f64,
}

impl Plugin {
    fn get(&self, id: clap_id) -> f64 {
        f64::from_bits(self.values[id as usize].load(Ordering::Relaxed))
    }
    fn set(&self, id: clap_id, v: f64) {
        self.values[id as usize].store(v.to_bits(), Ordering::Relaxed)
    }
    fn apply_events(&self, events: *const clap_input_events) {
        if events.is_null() {
            return;
        }
        let ev = unsafe { &*events };
        let n = ev.size.map_or(0, |s| unsafe { s(ev) });
        for i in 0..n {
            let h = unsafe { ev.get.unwrap()(ev, i) };
            if h.is_null() {
                continue;
            }
            let h = unsafe { &*h };
            if h.space_id == CLAP_CORE_EVENT_SPACE_ID && h.type_ == CLAP_EVENT_PARAM_VALUE {
                let e = unsafe { &*(h as *const _ as *const clap_event_param_value) };
                if let Some(def) = PARAMS.iter().find(|d| d.id == e.param_id) {
                    self.set(def.id, e.value.clamp(def.min, def.max));
                }
            }
        }
    }
}

unsafe fn this<'a>(p: *const clap_plugin) -> &'a mut Plugin {
    &mut *((*p).plugin_data as *mut Plugin)
}

unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn destroy(p: *const clap_plugin) {
    drop(Box::from_raw((*p).plugin_data as *mut Plugin));
}
unsafe extern "C" fn activate(p: *const clap_plugin, sr: f64, _: u32, _: u32) -> bool {
    this(p).sample_rate = sr;
    true
}
unsafe extern "C" fn deactivate(_: *const clap_plugin) {}
unsafe extern "C" fn start_processing(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn stop_processing(_: *const clap_plugin) {}
unsafe extern "C" fn reset(p: *const clap_plugin) {
    this(p).phase = 0.0;
}

unsafe extern "C" fn process(
    p: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    let s = this(p);
    let pr = &*process;
    s.apply_events(pr.in_events);
    if pr.audio_outputs_count == 0 {
        return CLAP_PROCESS_CONTINUE;
    }
    let out = &*pr.audio_outputs;
    let (l, r) = (*out.data32, *out.data32.add(1));
    let gain = if s.get(P_MUTE) >= 0.5 {
        0.0
    } else {
        10f64.powf(s.get(P_LEVEL) / 20.0)
    };
    let pan = s.get(P_PAN);
    let (gl, gr) = (gain * (1.0 - pan.max(0.0)), gain * (1.0 + pan.min(0.0)));
    let freq = s.get(P_FREQ) * 2f64.powf(s.get(P_DETUNE).round() / 12.0);
    let wave = s.get(P_WAVE).round() as i32;
    let inc = freq / s.sample_rate;
    let mut peak = 0.0f64;
    for i in 0..pr.frames_count as usize {
        let x = match wave {
            0 => (s.phase * std::f64::consts::TAU).sin(),
            1 => 2.0 * s.phase - 1.0,
            _ => {
                if s.phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
        };
        s.phase = (s.phase + inc).fract();
        *l.add(i) = (x * gl) as f32;
        *r.add(i) = (x * gr) as f32;
        peak = peak.max((x * gl).abs()).max((x * gr).abs());
    }
    s.set(P_PEAK, peak.min(1.0));
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn get_extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_PARAMS {
        &EXT_PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &EXT_PORTS as *const _ as *const c_void
    } else {
        ptr::null()
    }
}
unsafe extern "C" fn on_main_thread(_: *const clap_plugin) {}

// --- params ------------------------------------------------------------------

static EXT_PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn params_count(_: *const clap_plugin) -> u32 {
    PARAMS.len() as u32
}

fn copy_str(dst: &mut [c_char], s: &str) {
    let n = s.len().min(dst.len() - 1);
    for (d, b) in dst.iter_mut().zip(s.bytes().take(n)) {
        *d = b as c_char;
    }
    dst[n] = 0;
}

unsafe extern "C" fn params_get_info(
    _: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    let Some(d) = PARAMS.get(index as usize) else {
        return false;
    };
    let info = &mut *info;
    info.id = d.id;
    info.flags = d.flags;
    info.cookie = ptr::null_mut();
    copy_str(&mut info.name, d.name);
    copy_str(&mut info.module, d.module);
    info.min_value = d.min;
    info.max_value = d.max;
    info.default_value = d.default;
    true
}

unsafe extern "C" fn params_get_value(p: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
    if (id as usize) >= PARAMS.len() {
        return false;
    }
    *out = this(p).get(id);
    true
}

fn format(id: clap_id, v: f64) -> String {
    match id {
        P_LEVEL => format!("{v:.1} dB"),
        P_FREQ => format!("{v:.1} Hz"),
        P_WAVE => WAVES[(v.round() as usize).min(2)].to_string(),
        P_MUTE => if v >= 0.5 { "On" } else { "Off" }.to_string(),
        P_PAN if v.abs() < 0.005 => "Center".to_string(),
        P_PAN if v < 0.0 => format!("{:.0}% L", -v * 100.0),
        P_PAN => format!("{:.0}% R", v * 100.0),
        P_DETUNE => format!("{:+} st", v.round() as i32),
        P_PEAK => format!("{:.1} dB", 20.0 * v.max(1e-6).log10()),
        _ => format!("{v:.3}"),
    }
}

unsafe extern "C" fn params_value_to_text(
    _: *const clap_plugin,
    id: clap_id,
    v: f64,
    buf: *mut c_char,
    cap: u32,
) -> bool {
    if (id as usize) >= PARAMS.len() || cap == 0 {
        return false;
    }
    copy_str(
        std::slice::from_raw_parts_mut(buf, cap as usize),
        &format(id, v),
    );
    true
}

unsafe extern "C" fn params_text_to_value(
    _: *const clap_plugin,
    id: clap_id,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    let t = CStr::from_ptr(text).to_string_lossy().trim().to_lowercase();
    if id == P_WAVE {
        if let Some(i) = WAVES.iter().position(|w| w.to_lowercase() == t) {
            *out = i as f64;
            return true;
        }
    }
    let num: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        .collect();
    match num.parse::<f64>() {
        Ok(v) => {
            *out = v;
            true
        }
        Err(_) => false,
    }
}

unsafe extern "C" fn params_flush(
    p: *const clap_plugin,
    input: *const clap_input_events,
    _: *const clap_output_events,
) {
    this(p).apply_events(input);
}

// --- audio ports ---------------------------------------------------------------

static EXT_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(ports_count),
    get: Some(ports_get),
};

unsafe extern "C" fn ports_count(_: *const clap_plugin, is_input: bool) -> u32 {
    if is_input {
        0
    } else {
        1
    }
}

unsafe extern "C" fn ports_get(
    _: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if is_input || index != 0 {
        return false;
    }
    let info = &mut *info;
    info.id = 0;
    copy_str(&mut info.name, "Out");
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
    info.in_place_pair = u32::MAX;
    true
}

// --- factory / entry -------------------------------------------------------------

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(factory_create),
};

unsafe extern "C" fn factory_count(_: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_descriptor(
    _: *const clap_plugin_factory,
    i: u32,
) -> *const clap_plugin_descriptor {
    if i == 0 {
        &DESCRIPTOR
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn factory_create(
    _: *const clap_plugin_factory,
    _host: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    if CStr::from_ptr(id) != ID {
        return ptr::null();
    }
    let plugin = Box::into_raw(Box::new(Plugin {
        raw: clap_plugin {
            desc: &DESCRIPTOR,
            plugin_data: ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        values: std::array::from_fn(|i| {
            let default = PARAMS
                .iter()
                .find(|d| d.id as usize == i)
                .map_or(0.0, |d| d.default);
            AtomicU64::new(default.to_bits())
        }),
        sample_rate: 48_000.0,
        phase: 0.0,
    }));
    (*plugin).raw.plugin_data = plugin as *mut c_void;
    &(*plugin).raw
}

unsafe extern "C" fn entry_init(_: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if CStr::from_ptr(id) == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const _ as *const c_void
    } else {
        ptr::null()
    }
}

#[no_mangle]
#[used]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};
