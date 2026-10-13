//! Loading CLAP binaries and talking to plugin instances.
//!
//! Everything in here is a thin, safe-ish wrapper over the raw `clap-sys`
//! bindings. Thread rules follow the CLAP spec: methods documented as
//! main-thread must only be called from the thread that created the instance.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};

use anyhow::{anyhow, bail, Context, Result};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::log::*;
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::state::*;
use clap_sys::ext::thread_check::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::{clap_version_is_compatible, CLAP_VERSION};

// ---------------------------------------------------------------------------
// Library / entry point
// ---------------------------------------------------------------------------

/// A loaded `.clap` binary whose entry point has been initialised.
pub struct PluginLibrary {
    entry: *const clap_plugin_entry,
    pub path: PathBuf,
    // Must be dropped after `deinit` has been called, see `Drop`.
    _lib: libloading::Library,
}

// The entry point is required to be thread-safe by the spec.
unsafe impl Send for PluginLibrary {}
unsafe impl Sync for PluginLibrary {}

/// Static information about one plugin inside a `.clap` file.
#[derive(Clone, Debug)]
pub struct PluginDescriptor {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub vendor: String,
    #[allow(dead_code)]
    pub version: String,
    #[allow(dead_code)]
    pub description: String,
    #[allow(dead_code)]
    pub features: Vec<String>,
}

/// On macOS a `.clap` is a bundle directory; the binary lives inside it.
fn binary_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        let stem = path.file_stem().unwrap_or_default();
        let candidate = path.join("Contents").join("MacOS").join(stem);
        if candidate.exists() {
            return candidate;
        }
        // Fall back to the first file in Contents/MacOS.
        if let Ok(mut rd) = std::fs::read_dir(path.join("Contents").join("MacOS")) {
            if let Some(Ok(e)) = rd.next() {
                return e.path();
            }
        }
    }
    path.to_path_buf()
}

impl PluginLibrary {
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot find {}", path.display()))?;
        let lib = unsafe { libloading::Library::new(binary_path(&path)) }
            .with_context(|| format!("cannot load {}", path.display()))?;
        let entry: *const clap_plugin_entry = unsafe {
            let sym: libloading::Symbol<*const clap_plugin_entry> = lib
                .get(b"clap_entry\0")
                .context("not a CLAP plugin (no clap_entry symbol)")?;
            *sym
        };
        if entry.is_null() {
            bail!("clap_entry is null");
        }
        let e = unsafe { &*entry };
        if !clap_version_is_compatible(e.clap_version) {
            bail!(
                "incompatible CLAP version {}.{}.{}",
                e.clap_version.major,
                e.clap_version.minor,
                e.clap_version.revision
            );
        }
        let cpath = CString::new(path.to_string_lossy().as_bytes())?;
        let init = e.init.ok_or_else(|| anyhow!("clap_entry.init missing"))?;
        if !unsafe { init(cpath.as_ptr()) } {
            bail!("plugin entry init failed");
        }
        Ok(Arc::new(PluginLibrary {
            entry,
            path,
            _lib: lib,
        }))
    }

    fn factory(&self) -> Result<&clap_plugin_factory> {
        let get = unsafe { (*self.entry).get_factory }.ok_or_else(|| anyhow!("no get_factory"))?;
        let f = unsafe { get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
        if f.is_null() {
            bail!("plugin has no plugin factory");
        }
        Ok(unsafe { &*f })
    }

    pub fn descriptors(&self) -> Result<Vec<PluginDescriptor>> {
        let f = self.factory()?;
        let count = f.get_plugin_count.map_or(0, |c| unsafe { c(f) });
        let mut out = Vec::new();
        for i in 0..count {
            let Some(get) = f.get_plugin_descriptor else {
                break;
            };
            let d = unsafe { get(f, i) };
            if d.is_null() {
                continue;
            }
            out.push(unsafe { descriptor_from_raw(&self.path, &*d) });
        }
        Ok(out)
    }
}

impl Drop for PluginLibrary {
    fn drop(&mut self) {
        if let Some(deinit) = unsafe { (*self.entry).deinit } {
            unsafe { deinit() };
        }
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn cbuf(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

unsafe fn descriptor_from_raw(path: &Path, d: &clap_plugin_descriptor) -> PluginDescriptor {
    let mut features = Vec::new();
    if !d.features.is_null() {
        let mut p = d.features;
        while !(*p).is_null() {
            features.push(cstr(*p));
            p = p.add(1);
        }
    }
    PluginDescriptor {
        path: path.to_path_buf(),
        id: cstr(d.id),
        name: cstr(d.name),
        vendor: cstr(d.vendor),
        version: cstr(d.version),
        description: cstr(d.description),
        features,
    }
}

// ---------------------------------------------------------------------------
// Host side state shared with the plugin
// ---------------------------------------------------------------------------

thread_local! {
    static IS_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Mark the current thread as the audio thread (for `clap.thread-check`).
pub fn mark_audio_thread() {
    IS_AUDIO_THREAD.with(|c| c.set(true));
}

/// Flags the plugin raises from any thread; the UI loop services them on the
/// main thread.
pub struct HostShared {
    main_thread: ThreadId,
    pub restart_requested: AtomicBool,
    pub callback_requested: AtomicBool,
    pub rescan_flags: AtomicU32,
    pub log: Mutex<VecDeque<String>>,
}

impl HostShared {
    fn new() -> Self {
        HostShared {
            main_thread: thread::current().id(),
            restart_requested: AtomicBool::new(false),
            callback_requested: AtomicBool::new(false),
            rescan_flags: AtomicU32::new(0),
            log: Mutex::new(VecDeque::new()),
        }
    }

    pub fn push_log(&self, msg: String) {
        if let Ok(mut l) = self.log.lock() {
            l.push_back(msg);
            while l.len() > 200 {
                l.pop_front();
            }
        }
    }
}

unsafe fn shared<'a>(host: *const clap_host) -> Option<&'a HostShared> {
    if host.is_null() || (*host).host_data.is_null() {
        return None;
    }
    Some(&*((*host).host_data as *const HostShared))
}

unsafe extern "C" fn host_get_extension(
    _host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_PARAMS {
        &HOST_PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_LOG {
        &HOST_LOG as *const _ as *const c_void
    } else if id == CLAP_EXT_THREAD_CHECK {
        &HOST_THREAD_CHECK as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &HOST_STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &HOST_AUDIO_PORTS as *const _ as *const c_void
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.restart_requested.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_request_process(_host: *const clap_host) {
    // We process continuously, so nothing to do.
}

unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.callback_requested.store(true, Ordering::Release);
    }
}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

unsafe extern "C" fn host_params_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    if let Some(s) = shared(host) {
        s.rescan_flags
            .fetch_or(flags | CLAP_PARAM_RESCAN_VALUES, Ordering::AcqRel);
    }
}

unsafe extern "C" fn host_params_clear(_: *const clap_host, _: clap_id, _: clap_param_clear_flags) {
}

unsafe extern "C" fn host_params_request_flush(_host: *const clap_host) {
    // The audio thread runs continuously and flushes every block.
}

static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

unsafe extern "C" fn host_log(
    host: *const clap_host,
    severity: clap_log_severity,
    msg: *const c_char,
) {
    let level = match severity {
        CLAP_LOG_DEBUG => "debug",
        CLAP_LOG_INFO => "info",
        CLAP_LOG_WARNING => "warn",
        CLAP_LOG_ERROR => "error",
        CLAP_LOG_FATAL => "fatal",
        CLAP_LOG_HOST_MISBEHAVING => "host-misbehaving",
        CLAP_LOG_PLUGIN_MISBEHAVING => "plugin-misbehaving",
        _ => "log",
    };
    if let Some(s) = shared(host) {
        s.push_log(format!("[{level}] {}", cstr(msg)));
    }
}

static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_is_main_thread),
    is_audio_thread: Some(host_is_audio_thread),
};

unsafe extern "C" fn host_is_main_thread(host: *const clap_host) -> bool {
    shared(host).is_some_and(|s| s.main_thread == thread::current().id())
}

unsafe extern "C" fn host_is_audio_thread(_host: *const clap_host) -> bool {
    IS_AUDIO_THREAD.with(|c| c.get())
}

static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_mark_dirty),
};

unsafe extern "C" fn host_mark_dirty(_host: *const clap_host) {}

static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_audio_ports_flag_supported),
    rescan: Some(host_audio_ports_rescan),
};

unsafe extern "C" fn host_audio_ports_flag_supported(_: *const clap_host, flag: u32) -> bool {
    // Any port change is handled by a full restart.
    flag & CLAP_AUDIO_PORTS_RESCAN_LIST != 0 || flag != 0
}

unsafe extern "C" fn host_audio_ports_rescan(host: *const clap_host, _flags: u32) {
    host_request_restart(host);
}

// ---------------------------------------------------------------------------
// Plugin instance
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub id: clap_id,
    pub flags: u32,
    pub cookie: *mut c_void,
    pub name: String,
    #[allow(dead_code)]
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

impl ParamInfo {
    pub fn is_stepped(&self) -> bool {
        self.flags & CLAP_PARAM_IS_STEPPED != 0
    }
    pub fn is_hidden(&self) -> bool {
        self.flags & CLAP_PARAM_IS_HIDDEN != 0
    }
    pub fn is_readonly(&self) -> bool {
        self.flags & CLAP_PARAM_IS_READONLY != 0
    }
}

/// A parameter change on its way to the plugin.
#[derive(Clone, Copy, Debug)]
pub struct ParamChange {
    pub id: clap_id,
    pub cookie: *mut c_void,
    pub value: f64,
}
unsafe impl Send for ParamChange {}

/// Anything the UI sends to the plugin through the audio thread.
#[derive(Clone, Copy, Debug)]
pub enum HostEvent {
    Param(ParamChange),
    /// A note starting (`on`) or stopping, as a MIDI key number 0..=127.
    Note {
        key: i16,
        on: bool,
        velocity: f64,
    },
}
unsafe impl Send for HostEvent {}

/// How the plugin wants to receive notes on its first note input port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteInput {
    /// The plugin has no note input.
    None,
    /// CLAP note events (preferred).
    Clap { port: i16 },
    /// Raw MIDI 1.0 bytes.
    Midi { port: u16 },
}

/// A raw plugin pointer that may cross to the audio thread.
#[derive(Clone, Copy)]
pub struct RawPlugin(pub *const clap_plugin);
unsafe impl Send for RawPlugin {}
unsafe impl Sync for RawPlugin {}

pub struct PluginInstance {
    plugin: *const clap_plugin,
    params: *const clap_plugin_params,
    pub descriptor: PluginDescriptor,
    pub shared: Arc<HostShared>,
    active: bool,
    // Declared last so they are dropped after `destroy` has run in `Drop`.
    _host: Box<clap_host>,
    _lib: Arc<PluginLibrary>,
}

impl PluginInstance {
    pub fn new(lib: Arc<PluginLibrary>, id: &str) -> Result<Self> {
        let descriptor = lib
            .descriptors()?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(|| anyhow!("plugin id {id} not found in {}", lib.path.display()))?;
        let shared = Arc::new(HostShared::new());
        let host = Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: Arc::as_ptr(&shared) as *mut c_void,
            name: cptr!("lextrack"),
            vendor: cptr!("lextrack"),
            url: cptr!(""),
            version: cptr!("0.1.0"),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_request_restart),
            request_process: Some(host_request_process),
            request_callback: Some(host_request_callback),
        });
        let factory = lib.factory()?;
        let create = factory
            .create_plugin
            .ok_or_else(|| anyhow!("factory cannot create plugins"))?;
        let cid = CString::new(id)?;
        let plugin = unsafe { create(factory, &*host, cid.as_ptr()) };
        if plugin.is_null() {
            bail!("plugin {id} could not be created");
        }
        let mut inst = PluginInstance {
            plugin,
            params: ptr::null(),
            descriptor,
            shared,
            active: false,
            _host: host,
            _lib: lib,
        };
        let init = unsafe { (*plugin).init }.ok_or_else(|| anyhow!("plugin has no init"))?;
        if !unsafe { init(plugin) } {
            // Instance must still be destroyed; Drop does that.
            bail!("plugin init failed");
        }
        inst.params = inst.extension(CLAP_EXT_PARAMS) as *const clap_plugin_params;
        Ok(inst)
    }

    fn extension(&self, id: &CStr) -> *const c_void {
        match unsafe { (*self.plugin).get_extension } {
            Some(f) => unsafe { f(self.plugin, id.as_ptr()) },
            None => ptr::null(),
        }
    }

    pub fn raw(&self) -> RawPlugin {
        RawPlugin(self.plugin)
    }

    // --- activation --------------------------------------------------------

    pub fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        if self.active {
            return Ok(());
        }
        let f = unsafe { (*self.plugin).activate }.ok_or_else(|| anyhow!("no activate"))?;
        if !unsafe { f(self.plugin, sample_rate, min_frames, max_frames) } {
            bail!("plugin refused to activate at {sample_rate} Hz");
        }
        self.active = true;
        Ok(())
    }

    pub fn deactivate(&mut self) {
        if self.active {
            if let Some(f) = unsafe { (*self.plugin).deactivate } {
                unsafe { f(self.plugin) };
            }
            self.active = false;
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn on_main_thread(&self) {
        if let Some(f) = unsafe { (*self.plugin).on_main_thread } {
            unsafe { f(self.plugin) };
        }
    }

    /// The plugin's first note input port, and the dialect to use with it.
    pub fn note_input(&self) -> NoteInput {
        let ext = self.extension(CLAP_EXT_NOTE_PORTS) as *const clap_plugin_note_ports;
        let Some(ext) = (unsafe { ext.as_ref() }) else {
            return NoteInput::None;
        };
        if ext.count.map_or(0, |c| unsafe { c(self.plugin, true) }) == 0 {
            return NoteInput::None;
        }
        let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
        if !ext
            .get
            .is_some_and(|g| unsafe { g(self.plugin, 0, true, &mut info) })
        {
            return NoteInput::None;
        }
        if info.supported_dialects & CLAP_NOTE_DIALECT_CLAP != 0 {
            NoteInput::Clap { port: 0 }
        } else if info.supported_dialects & (CLAP_NOTE_DIALECT_MIDI | CLAP_NOTE_DIALECT_MIDI_MPE)
            != 0
        {
            NoteInput::Midi { port: 0 }
        } else {
            NoteInput::None
        }
    }

    /// Channel counts of every input and output audio port.
    pub fn audio_ports(&self) -> (Vec<u32>, Vec<u32>) {
        let ext = self.extension(CLAP_EXT_AUDIO_PORTS) as *const clap_plugin_audio_ports;
        if ext.is_null() {
            return (vec![], vec![]);
        }
        let ext = unsafe { &*ext };
        let ports = |is_input: bool| -> Vec<u32> {
            let n = ext.count.map_or(0, |c| unsafe { c(self.plugin, is_input) });
            (0..n)
                .filter_map(|i| {
                    let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
                    let ok = ext
                        .get
                        .is_some_and(|g| unsafe { g(self.plugin, i, is_input, &mut info) });
                    ok.then_some(info.channel_count)
                })
                .collect()
        };
        (ports(true), ports(false))
    }

    // --- parameters ----------------------------------------------------------

    pub fn params(&self) -> Vec<ParamInfo> {
        if self.params.is_null() {
            return vec![];
        }
        let p = unsafe { &*self.params };
        let n = p.count.map_or(0, |c| unsafe { c(self.plugin) });
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            if p.get_info
                .is_some_and(|g| unsafe { g(self.plugin, i, &mut info) })
            {
                out.push(ParamInfo {
                    id: info.id,
                    flags: info.flags,
                    cookie: info.cookie,
                    name: cbuf(&info.name),
                    module: cbuf(&info.module),
                    min: info.min_value,
                    max: info.max_value,
                    default: info.default_value,
                });
            }
        }
        out
    }

    pub fn param_value(&self, id: clap_id) -> Option<f64> {
        let p = unsafe { self.params.as_ref()? };
        let f = p.get_value?;
        let mut v = 0.0;
        unsafe { f(self.plugin, id, &mut v) }.then_some(v)
    }

    pub fn value_to_text(&self, id: clap_id, value: f64) -> Option<String> {
        let p = unsafe { self.params.as_ref()? };
        let f = p.value_to_text?;
        let mut buf = [0 as c_char; 256];
        unsafe { f(self.plugin, id, value, buf.as_mut_ptr(), buf.len() as u32) }.then(|| cbuf(&buf))
    }

    /// The plugin's saved state (`clap.state`), if it supports saving.
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let ext = self.extension(CLAP_EXT_STATE) as *const clap_plugin_state;
        let save = unsafe { ext.as_ref()?.save? };
        let mut buf: Vec<u8> = Vec::new();
        let stream = clap_ostream {
            ctx: &mut buf as *mut Vec<u8> as *mut c_void,
            write: Some(ostream_write),
        };
        unsafe { save(self.plugin, &stream) }.then_some(buf)
    }

    /// Restore state from [`save_state`](Self::save_state).
    pub fn load_state(&self, data: &[u8]) -> bool {
        let ext = self.extension(CLAP_EXT_STATE) as *const clap_plugin_state;
        let Some(load) = (unsafe { ext.as_ref() }).and_then(|e| e.load) else {
            return false;
        };
        let mut cursor: &[u8] = data;
        let stream = clap_istream {
            ctx: &mut cursor as *mut &[u8] as *mut c_void,
            read: Some(istream_read),
        };
        unsafe { load(self.plugin, &stream) }
    }

    /// Push parameter changes while the plugin is *not* active. While active,
    /// changes go through the audio engine instead.
    pub fn flush_inactive(&self, changes: &[ParamChange]) {
        if self.active || self.params.is_null() {
            return;
        }
        let Some(flush) = (unsafe { (*self.params).flush }) else {
            return;
        };
        let mut events = EventList::new(NoteInput::None);
        for c in changes {
            events.push(HostEvent::Param(*c));
        }
        let out = discard_output_events();
        unsafe { flush(self.plugin, &events.as_input(), &out) };
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        self.deactivate();
        if let Some(destroy) = unsafe { (*self.plugin).destroy } {
            unsafe { destroy(self.plugin) };
        }
    }
}

// ---------------------------------------------------------------------------
// Event lists
// ---------------------------------------------------------------------------

/// One event in an [`EventList`]. Each variant is a complete CLAP event
/// struct starting with its header, so a pointer to the header is a pointer
/// to the whole event.
enum RawEvent {
    Param(clap_event_param_value),
    Note(clap_event_note),
    Midi(clap_event_midi),
}

impl RawEvent {
    fn header(&self) -> &clap_event_header {
        match self {
            RawEvent::Param(e) => &e.header,
            RawEvent::Note(e) => &e.header,
            RawEvent::Midi(e) => &e.header,
        }
    }
}

fn header<T>(type_: u16) -> clap_event_header {
    clap_event_header {
        size: std::mem::size_of::<T>() as u32,
        time: 0,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: CLAP_EVENT_IS_LIVE,
    }
}

/// A reusable list of events handed to the plugin as `clap_input_events`.
/// It never allocates once capacity is reached.
pub struct EventList {
    events: Vec<RawEvent>,
    notes: NoteInput,
}

impl EventList {
    pub fn new(notes: NoteInput) -> Self {
        EventList {
            events: Vec::with_capacity(256),
            notes,
        }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn push(&mut self, e: HostEvent) {
        self.push_at(e, 0);
    }

    /// Add an event `time` frames into the block. Events must be added in
    /// time order.
    pub fn push_at(&mut self, e: HostEvent, time: u32) {
        let mut raw = match e {
            HostEvent::Param(c) => RawEvent::Param(clap_event_param_value {
                header: header::<clap_event_param_value>(CLAP_EVENT_PARAM_VALUE),
                param_id: c.id,
                cookie: c.cookie,
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value: c.value,
            }),
            HostEvent::Note { key, on, velocity } => match self.notes {
                NoteInput::None => return,
                NoteInput::Clap { port } => RawEvent::Note(clap_event_note {
                    header: header::<clap_event_note>(if on {
                        CLAP_EVENT_NOTE_ON
                    } else {
                        CLAP_EVENT_NOTE_OFF
                    }),
                    note_id: -1,
                    port_index: port,
                    channel: 0,
                    key,
                    velocity,
                }),
                NoteInput::Midi { port } => RawEvent::Midi(clap_event_midi {
                    header: header::<clap_event_midi>(CLAP_EVENT_MIDI),
                    port_index: port,
                    data: [
                        if on { 0x90 } else { 0x80 },
                        key.clamp(0, 127) as u8,
                        (velocity * 127.0).round().clamp(1.0, 127.0) as u8,
                    ],
                }),
            },
        };
        match &mut raw {
            RawEvent::Param(e) => e.header.time = time,
            RawEvent::Note(e) => e.header.time = time,
            RawEvent::Midi(e) => e.header.time = time,
        }
        self.events.push(raw);
    }

    /// The returned struct borrows `self`; keep `self` alive while it's used.
    pub fn as_input(&self) -> clap_input_events {
        clap_input_events {
            ctx: self as *const Self as *mut c_void,
            size: Some(event_list_size),
            get: Some(event_list_get),
        }
    }
}

unsafe extern "C" fn event_list_size(list: *const clap_input_events) -> u32 {
    let l = &*((*list).ctx as *const EventList);
    l.events.len() as u32
}

unsafe extern "C" fn event_list_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let l = &*((*list).ctx as *const EventList);
    match l.events.get(index as usize) {
        Some(e) => e.header(),
        None => ptr::null(),
    }
}

/// Output events the plugin emits (e.g. its own parameter gestures). The UI
/// re-reads parameter values directly, so these can be ignored.
pub fn discard_output_events() -> clap_output_events {
    clap_output_events {
        ctx: ptr::null_mut(),
        try_push: Some(discard_push),
    }
}

unsafe extern "C" fn discard_push(
    _: *const clap_output_events,
    _: *const clap_event_header,
) -> bool {
    true
}

unsafe extern "C" fn ostream_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let buf = &mut *((*stream).ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(
        buffer as *const u8,
        size as usize,
    ));
    size as i64
}

unsafe extern "C" fn istream_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let cursor = &mut *((*stream).ctx as *mut &[u8]);
    let n = (size as usize).min(cursor.len());
    std::ptr::copy_nonoverlapping(cursor.as_ptr(), buffer as *mut u8, n);
    *cursor = &cursor[n..];
    n as i64
}
