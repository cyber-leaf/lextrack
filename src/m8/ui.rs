//! The tracker screen, modelled on the M8: SONG, CHAIN, PHRASE, INSTRUMENT,
//! TABLE, GROOVE and PROJECT views on a 40x24 character screen, moved
//! between with SHIFT+arrows, edited with EDIT/OPTION combos, and played
//! with PLAY. CLAP plugins take the place of the M8's synth engines, so the
//! instrument view picks a plugin and shows its parameters.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap_sys::ext::params::{CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Terminal;

use super::format::{
    fx_commands, fx_name, note_name, InstrumentKind, SamplerParams, Song, CHAINS, EMPTY, GROOVES,
    INSTRUMENTS, PHRASES, SEQ_FX, SONG_ROWS, STEPS, S_AMP, S_CHORUS, S_CUTOFF, S_DEGRADE, S_DELAY,
    S_DRY, S_ENV, S_FILTER, S_LENGTH, S_LFO, S_LIMIT, S_LOOP, S_PAN, S_PLAY, S_RES, S_REVERB,
    S_SLICE, S_START, TABLES, TRACKS,
};
use super::input::{Button, Input, Keys, Lights, Press, RELEASE_EVENTS};
use super::instruments::{InstrumentMap, PluginRef};
use super::rack::{load_slots_reusing, render_wav, wanted_instruments, Rack, Slot};
use super::recorder::{self, Capture, Source, Take, SOURCES};
use super::sampler::{
    self, card_root, resolve_sample_path, sample_path_for, SampleBank, SamplerSet, FILTERS,
    LFO_SHAPES, LFO_TRIGGERS, LIMITS, MOD_DESTS, PLAY_MODES,
};
use super::screen::{Role, Screen, ROWS};
use super::sequencer::Pos;
use crate::audio;
use crate::plugin::{ParamChange, ParamInfo, PluginDescriptor};
use crate::settings::{self, AudioSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    Song,
    Chain,
    Phrase,
    Instrument,
    Table,
    Groove,
    Project,
}

/// Mini-map cells: (letter, column, row, view). Views without a `View`
/// exist on the M8 but not here yet, and are drawn dim.
const MAP: &[(char, usize, usize, Option<View>)] = &[
    ('P', 1, 0, Some(View::Project)),
    ('G', 3, 0, Some(View::Groove)),
    ('M', 4, 0, None),
    ('L', 0, 1, None),
    ('S', 1, 1, Some(View::Song)),
    ('C', 2, 1, Some(View::Chain)),
    ('P', 3, 1, Some(View::Phrase)),
    ('I', 4, 1, Some(View::Instrument)),
    ('T', 5, 1, Some(View::Table)),
    ('V', 1, 2, None),
    ('E', 1, 3, None),
];

/// What a grid column holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Chain,
    Phrase,
    Transpose,
    Note,
    Volume,
    Instrument,
    FxCmd,
    FxVal,
    Ticks,
}

impl Kind {
    fn can_be_empty(self) -> bool {
        !matches!(self, Kind::Transpose | Kind::FxVal)
    }

    fn max(self) -> u8 {
        match self {
            Kind::Chain | Kind::Phrase | Kind::Ticks => 0xFE,
            Kind::Instrument | Kind::Note | Kind::Volume => 0x7F,
            Kind::FxCmd => SEQ_FX.len() as u8 - 1,
            Kind::Transpose | Kind::FxVal => 0xFF,
        }
    }

    fn big_step(self) -> i32 {
        match self {
            Kind::Note => 12,
            _ => 16,
        }
    }

    fn default(self) -> u8 {
        match self {
            Kind::Note => 36,
            Kind::Volume => 0x64,
            Kind::Ticks => 6,
            _ => 0,
        }
    }
}

const SONG_COLS: [Kind; 8] = [Kind::Chain; 8];
const CHAIN_COLS: [Kind; 2] = [Kind::Phrase, Kind::Transpose];
const PHRASE_COLS: [Kind; 9] = [
    Kind::Note,
    Kind::Volume,
    Kind::Instrument,
    Kind::FxCmd,
    Kind::FxVal,
    Kind::FxCmd,
    Kind::FxVal,
    Kind::FxCmd,
    Kind::FxVal,
];
const TABLE_COLS: [Kind; 8] = [
    Kind::Transpose,
    Kind::Volume,
    Kind::FxCmd,
    Kind::FxVal,
    Kind::FxCmd,
    Kind::FxVal,
    Kind::FxCmd,
    Kind::FxVal,
];
const GROOVE_COLS: [Kind; 1] = [Kind::Ticks];

/// Screen column and width of each grid column.
const SONG_GEOM: [(usize, usize); 8] = [
    (3, 2),
    (6, 2),
    (9, 2),
    (12, 2),
    (15, 2),
    (18, 2),
    (21, 2),
    (24, 2),
];
const CHAIN_GEOM: [(usize, usize); 2] = [(3, 2), (6, 2)];
const PHRASE_GEOM: [(usize, usize); 9] = [
    (2, 3),
    (6, 2),
    (9, 2),
    (12, 3),
    (16, 2),
    (19, 3),
    (23, 2),
    (26, 3),
    (30, 2),
];
const TABLE_GEOM: [(usize, usize); 8] = [
    (2, 2),
    (5, 2),
    (8, 3),
    (12, 2),
    (15, 3),
    (19, 2),
    (22, 3),
    (26, 2),
];
const GROOVE_GEOM: [(usize, usize); 1] = [(2, 2)];

/// Top screen row of grid data.
const GRID_Y: usize = 3;
const VISIBLE: usize = 16;
/// Left edge of the right-hand panel.
const PANEL_X: usize = 33;

const DOUBLE_TAP: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Default)]
struct Cursor {
    row: usize,
    col: usize,
}

/// Where in the song a chain or phrase view came from.
#[derive(Clone, Copy, Debug)]
struct Ctx {
    track: usize,
    row: usize,
    chain_step: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelMode {
    Box,
    Columns,
    Rows,
    All,
}

#[derive(Clone, Copy)]
struct Selection {
    anchor: Cursor,
    mode: SelMode,
}

struct Clip {
    view: View,
    col: usize,
    kinds: Vec<Kind>,
    cells: Vec<Vec<u8>>,
}

enum Modal {
    Plugins {
        instrument: u8,
        list: Vec<PluginDescriptor>,
        cur: usize,
    },
    Songs {
        dir: PathBuf,
        list: Vec<PathBuf>,
        cur: usize,
    },
    Samples {
        instrument: u8,
        dir: PathBuf,
        list: Vec<PathBuf>,
        cur: usize,
    },
    Text {
        label: &'static str,
        value: String,
        target: TextTarget,
    },
}

#[derive(Clone, Copy)]
enum TextTarget {
    SongName,
    InstrumentName(u8),
    RecordingFile,
}

/// Rows of the sample recording screen.
const REC_ROWS: [&str; 8] = [
    "START", "SRC", "DEVICE", "VOL", "ARM", "PLAY", "NAME", "SAVE",
];
const REC_START: usize = 0;
const REC_SRC: usize = 1;
const REC_DEVICE: usize = 2;
const REC_VOL: usize = 3;
const REC_ARM: usize = 4;
const REC_PLAY: usize = 5;
const REC_NAME: usize = 6;
const REC_SAVE: usize = 7;

/// The sample recording screen, opened from REC. on a sampler's SAMPLE row.
struct Recording {
    instrument: u8,
    row: usize,
    source: Source,
    /// Input device; `None` is the system default.
    device: Option<String>,
    devices: Vec<String>,
    /// Input gain: 80 is unity.
    vol: u8,
    /// Level that starts the recording; 00 starts it at once.
    arm: u8,
    /// Song row to play from while recording.
    play: Option<u8>,
    name: String,
    capture: Arc<Capture>,
    input: Option<recorder::Input>,
    problem: Option<String>,
    take: Option<Take>,
    /// The finished take, for listening to.
    sample: Option<Arc<sampler::Sample>>,
    /// Handed back and forth with the capture.
    spare: Vec<f32>,
    meter: f32,
    started_song: bool,
}

impl Recording {
    fn gain(&self) -> f32 {
        self.vol as f32 / 128.0
    }
}

/// Instrument view rows before the plugin parameters.
const INST_FIELDS: usize = 5;

/// How a sampler setting is shown.
#[derive(Clone, Copy)]
enum Show {
    Hex,
    Names(&'static [&'static str]),
    /// 00 is OFF.
    Slice,
}

/// The sampler's rows on the instrument view, after the common ones:
/// (label, setting number for [`crate::m8::format::Instrument::sampler_param`],
/// display).
const SAMPLER_ROWS: [(&str, usize, Show); 39] = [
    ("VOLUME", 0, Show::Hex),
    ("PITCH", 1, Show::Hex),
    ("FINE TUNE", 2, Show::Hex),
    ("PLAY", 3 + S_PLAY, Show::Names(&PLAY_MODES)),
    ("SLICE", 3 + S_SLICE, Show::Slice),
    ("START", 3 + S_START, Show::Hex),
    ("LOOP ST", 3 + S_LOOP, Show::Hex),
    ("LENGTH", 3 + S_LENGTH, Show::Hex),
    ("DEGRADE", 3 + S_DEGRADE, Show::Hex),
    ("FILTER", 3 + S_FILTER, Show::Names(&FILTERS)),
    ("CUTOFF", 3 + S_CUTOFF, Show::Hex),
    ("RES", 3 + S_RES, Show::Hex),
    ("AMP", 3 + S_AMP, Show::Hex),
    ("LIMIT", 3 + S_LIMIT, Show::Names(&LIMITS)),
    ("PAN", 3 + S_PAN, Show::Hex),
    ("DRY", 3 + S_DRY, Show::Hex),
    ("CHORUS", 3 + S_CHORUS, Show::Hex),
    ("DELAY", 3 + S_DELAY, Show::Hex),
    ("REVERB", 3 + S_REVERB, Show::Hex),
    ("ENV1 DEST", 3 + S_ENV, Show::Names(&MOD_DESTS)),
    ("ENV1 AMT", 3 + S_ENV + 1, Show::Hex),
    ("ENV1 ATK", 3 + S_ENV + 2, Show::Hex),
    ("ENV1 HOLD", 3 + S_ENV + 3, Show::Hex),
    ("ENV1 DEC", 3 + S_ENV + 4, Show::Hex),
    ("ENV2 DEST", 3 + S_ENV + 6, Show::Names(&MOD_DESTS)),
    ("ENV2 AMT", 3 + S_ENV + 7, Show::Hex),
    ("ENV2 ATK", 3 + S_ENV + 8, Show::Hex),
    ("ENV2 HOLD", 3 + S_ENV + 9, Show::Hex),
    ("ENV2 DEC", 3 + S_ENV + 10, Show::Hex),
    ("LFO1 SHAPE", 3 + S_LFO, Show::Names(&LFO_SHAPES)),
    ("LFO1 DEST", 3 + S_LFO + 1, Show::Names(&MOD_DESTS)),
    ("LFO1 TRIG", 3 + S_LFO + 2, Show::Names(&LFO_TRIGGERS)),
    ("LFO1 FREQ", 3 + S_LFO + 3, Show::Hex),
    ("LFO1 AMT", 3 + S_LFO + 4, Show::Hex),
    ("LFO2 SHAPE", 3 + S_LFO + 6, Show::Names(&LFO_SHAPES)),
    ("LFO2 DEST", 3 + S_LFO + 7, Show::Names(&MOD_DESTS)),
    ("LFO2 TRIG", 3 + S_LFO + 8, Show::Names(&LFO_TRIGGERS)),
    ("LFO2 FREQ", 3 + S_LFO + 9, Show::Hex),
    ("LFO2 AMT", 3 + S_LFO + 10, Show::Hex),
];

/// A new sampler's value for a setting number.
fn sampler_default(i: usize) -> u8 {
    match i {
        0 | 1 => 0,
        2 => 0x80,
        _ => SamplerParams::default().0[i - 3],
    }
}

/// Project view rows: (label, screen row).
const PROJECT_FIELDS: [(&str, usize); 9] = [
    ("TEMPO", 2),
    ("TRANSPOSE", 3),
    ("NAME", 4),
    ("PROJECT", 6),
    ("RENDER", 7),
    ("OUTPUT", 10),
    ("RATE", 11),
    ("BUFFER", 12),
    ("APPLY", 13),
];

pub struct Options {
    pub path: Option<PathBuf>,
    pub map: InstrumentMap,
    pub use_device: bool,
    pub audio: AudioSettings,
    pub note_offset: i16,
    /// Song row the cursor starts on.
    pub row: usize,
}

pub struct App {
    song: Song,
    path: Option<PathBuf>,
    map: InstrumentMap,
    slots: Vec<Slot>,
    missing: Vec<(u8, String)>,
    /// Samples for the built-in sampler, and the ones that didn't load.
    bank: SampleBank,
    samplers: Arc<SamplerSet>,
    sample_problems: Vec<(u8, String)>,
    rack: Option<Rack>,
    use_device: bool,
    audio: AudioSettings,
    note_offset: i16,
    audio_problem: Option<String>,

    view: View,
    cursors: HashMap<View, Cursor>,
    song_top: usize,
    chain: usize,
    chain_ctx: Option<Ctx>,
    phrase: usize,
    phrase_ctx: Option<Ctx>,
    instrument: u8,
    inst_top: usize,
    table: usize,
    groove: usize,
    project_col: usize,
    /// 0 for the sample name, 1 for REC. on a sampler's SAMPLE row.
    sample_col: usize,
    recording: Option<Box<Recording>>,
    render_seconds: u32,
    devices: Vec<String>,

    last: HashMap<Kind, u8>,
    last_edit: Option<(Instant, View, usize, usize)>,
    selection: Option<Selection>,
    clip: Option<Clip>,
    modal: Option<Modal>,
    confirm: Option<(Instant, &'static str)>,
    status: Option<(String, Instant)>,
    preview: Option<(Instant, u8, i16)>,
    /// Shown values of parameters just changed, until the plugin catches up.
    param_hold: HashMap<(u8, u32), (f64, Instant)>,
    params: Vec<ParamInfo>,
    params_for: Option<(u8, usize)>,
    /// Installed plugins, scanned the first time they're needed.
    installed: Option<Vec<PluginDescriptor>>,
    muted: u8,
    soloed: u8,
    meter: f32,
    dirty: bool,
    unsaved: bool,
    keys: Keys,
    lights: Lights,
    quit: bool,
}

impl App {
    pub fn new(song: Song, o: Options) -> App {
        let mut app = App {
            song,
            path: o.path,
            map: o.map,
            slots: Vec::new(),
            missing: Vec::new(),
            bank: SampleBank::default(),
            samplers: Arc::new(SamplerSet::default()),
            sample_problems: Vec::new(),
            rack: None,
            use_device: o.use_device,
            audio: o.audio,
            note_offset: o.note_offset,
            audio_problem: None,
            view: View::Song,
            cursors: HashMap::new(),
            song_top: 0,
            chain: 0,
            chain_ctx: None,
            phrase: 0,
            phrase_ctx: None,
            instrument: 0,
            inst_top: 0,
            table: 0,
            groove: 0,
            project_col: 0,
            sample_col: 0,
            recording: None,
            render_seconds: 60,
            devices: Vec::new(),
            last: HashMap::new(),
            last_edit: None,
            selection: None,
            clip: None,
            modal: None,
            confirm: None,
            status: None,
            preview: None,
            param_hold: HashMap::new(),
            params: Vec::new(),
            params_for: None,
            installed: None,
            muted: 0,
            soloed: 0,
            meter: 0.0,
            dirty: false,
            unsaved: false,
            keys: Keys::default(),
            lights: Lights::default(),
            quit: false,
        };
        app.cursors.insert(
            View::Song,
            Cursor {
                row: o.row.min(SONG_ROWS - 1),
                col: 0,
            },
        );
        app.rebuild_audio();
        app
    }

    pub fn run<B: Backend>(&mut self, term: &mut Terminal<B>) -> Result<()> {
        while !self.quit {
            self.tick();
            term.draw(|f| {
                let mut s = Screen::new();
                self.draw(&mut s);
                s.render(f.area(), f.buffer_mut());
                self.lights.render(f.area(), f.buffer_mut(), Instant::now());
            })?;
            if event::poll(Duration::from_millis(25))? {
                loop {
                    if let Event::Key(k) = event::read()? {
                        self.key(k);
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn shutdown(mut self) {
        self.stop_audio();
    }

    // --- housekeeping --------------------------------------------------------

    fn tick(&mut self) {
        let mut restart = false;
        for s in &self.slots {
            let sh = &s.plugin.shared;
            if sh.callback_requested.swap(false, Ordering::AcqRel) {
                s.plugin.on_main_thread();
            }
            let rescan = sh.rescan_flags.swap(0, Ordering::AcqRel);
            if rescan & (CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_ALL) != 0 {
                self.params_for = None;
            }
            restart |= sh.restart_requested.swap(false, Ordering::AcqRel);
        }
        if restart {
            self.rebuild_audio();
        }
        if self.dirty {
            self.dirty = false;
            if self.needs_new_plugins() {
                self.rebuild_audio();
            } else {
                self.sync_samplers();
                if let Some(r) = &self.rack {
                    r.shared.set_song(Arc::new(self.song.clone()));
                }
            }
        }
        if let Some((at, inst, note)) = self.preview {
            if Instant::now() >= at {
                if let Some(r) = &self.rack {
                    r.shared.preview(inst, note, false);
                }
                self.preview = None;
            }
        }
        if let Some(r) = &self.rack {
            self.meter = r.shared.take_peak().max(self.meter * 0.8);
        }
        self.tick_recording();
        let now = Instant::now();
        self.param_hold.retain(|_, (_, until)| *until > now);
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.unsaved = true;
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Whether the song now plays an instrument whose plugin isn't loaded.
    fn needs_new_plugins(&self) -> bool {
        let have: HashSet<u8> = self
            .slots
            .iter()
            .map(|s| s.instrument)
            .chain(self.missing.iter().map(|m| m.0))
            .collect();
        wanted_instruments(&self.song, &self.map)
            .into_iter()
            .any(|n| !have.contains(&n))
    }

    fn song_dir(&self) -> PathBuf {
        super::app::song_dir(self.path.as_deref())
    }

    /// Load the samples the built-in sampler needs, and hand them to the
    /// audio thread when they changed.
    fn sync_samplers(&mut self) {
        let (set, problems) =
            SamplerSet::build(&self.song, &self.song_dir(), &self.map, &mut self.bank);
        self.sample_problems = problems;
        if !set.same_as(&self.samplers) {
            self.samplers = Arc::new(set);
            if let Some(r) = &self.rack {
                r.shared.set_samplers(self.samplers.clone());
            }
        }
    }

    fn stop_audio(&mut self) {
        if let Some(mut r) = self.rack.take() {
            r.stop(&mut self.slots);
        }
    }

    /// Load plugins for every instrument that needs one and (re)start audio.
    /// Playback resumes from where it was.
    fn rebuild_audio(&mut self) {
        let resume = self
            .rack
            .as_ref()
            .and_then(|r| (0..TRACKS).find_map(|t| r.shared.track(t).pos.map(|p| p.row)));
        self.stop_audio();
        let before: HashSet<(u8, PluginRef)> = self
            .slots
            .iter()
            .map(|s| (s.instrument, s.source.clone()))
            .collect();
        let old = std::mem::take(&mut self.slots);
        let (slots, missing) = load_slots_reusing(&self.song, &self.map, old);
        for s in &slots {
            if !before.contains(&(s.instrument, s.source.clone())) {
                if let Some(state) = self.map.states.get(&s.instrument) {
                    s.plugin.load_state(state);
                }
            }
        }
        self.slots = slots;
        self.missing = missing;
        self.params_for = None;
        self.sync_samplers();
        match Rack::start(
            Arc::new(self.song.clone()),
            &mut self.slots,
            self.samplers.clone(),
            self.use_device,
            &self.audio,
            self.note_offset,
        ) {
            Ok((rack, problem)) => {
                rack.shared.set_mutes(self.muted, self.soloed);
                if let Some(rec) = self.recording.as_ref().filter(|r| !r.source.is_input()) {
                    rack.shared.set_capture(Some(rec.capture.clone()));
                }
                if let Some(row) = resume {
                    rack.shared.play_from(row);
                }
                self.rack = Some(rack);
                self.audio_problem = problem;
            }
            Err(e) => self.flash(format!("AUDIO: {e:#}")),
        }
    }

    // --- keys ------------------------------------------------------------------

    fn key(&mut self, k: KeyEvent) {
        if matches!(self.modal, Some(Modal::Text { .. })) {
            if k.kind != KeyEventKind::Release {
                self.text_key(k);
            }
            return;
        }
        let releases = RELEASE_EVENTS.load(Ordering::Relaxed);
        self.lights.key(k, releases, Instant::now());
        match self.keys.translate(k, releases) {
            Input::Quit => self.quit = true,
            Input::Press(p) => {
                if self.modal.is_some() && self.recording.is_none() {
                    self.modal_press(p);
                } else {
                    self.press(p);
                }
            }
            Input::None => {}
        }
    }

    fn press(&mut self, p: Press) {
        if self.recording.is_some() {
            return self.record_press(p);
        }
        let h = p.held;
        if p.button == Button::Play {
            return self.play(p);
        }
        if p.button.is_direction() && h.shift && !h.option && !h.edit {
            return self.navigate(p.button);
        }
        if self.selection.is_some() && self.grid().is_some() {
            return self.selection_press(p);
        }
        match p.button {
            Button::Option if h.shift => self.start_selection(),
            Button::Shift if h.option => self.mute_track(),
            Button::Option if h.edit => self.delete(),
            Button::Edit if h.option => self.delete(),
            Button::Edit if h.shift => self.paste(),
            Button::Edit if h.none() => self.edit_tap(),
            b if b.is_direction() && h.edit => self.edit_adjust(b),
            b if b.is_direction() && h.option => self.option_move(b),
            b if b.is_direction() && h.none() => self.move_cursor(b),
            _ => {}
        }
    }

    // --- cursor and views --------------------------------------------------------

    fn cur(&self) -> Cursor {
        self.cursors.get(&self.view).copied().unwrap_or_default()
    }

    fn set_cur(&mut self, c: Cursor) {
        self.cursors.insert(self.view, c);
    }

    /// Rows and column kinds of the current view, if it is a grid.
    fn grid(&self) -> Option<(usize, &'static [Kind])> {
        match self.view {
            View::Song => Some((SONG_ROWS, &SONG_COLS)),
            View::Chain => Some((STEPS, &CHAIN_COLS)),
            View::Phrase => Some((STEPS, &PHRASE_COLS)),
            View::Table => Some((STEPS, &TABLE_COLS)),
            View::Groove => Some((STEPS, &GROOVE_COLS)),
            _ => None,
        }
    }

    /// Rows and columns the cursor can visit.
    fn extent(&self) -> (usize, usize) {
        match self.view {
            View::Instrument if self.is_sampler(self.instrument) => {
                (INST_FIELDS + SAMPLER_ROWS.len(), 1)
            }
            View::Instrument => (INST_FIELDS + self.params.len(), 1),
            View::Project => (PROJECT_FIELDS.len(), 1),
            _ => self.grid().map_or((1, 1), |(r, c)| (r, c.len())),
        }
    }

    fn move_cursor(&mut self, b: Button) {
        let (rows, cols) = self.extent();
        let mut c = self.cur();
        if self.view == View::Project && c.row == 3 {
            match b {
                Button::Left => self.project_col = self.project_col.saturating_sub(1),
                Button::Right => self.project_col = (self.project_col + 1).min(2),
                _ => {}
            }
        }
        if self.view == View::Instrument && c.row == 2 && self.is_sampler(self.instrument) {
            match b {
                Button::Left => self.sample_col = 0,
                Button::Right => self.sample_col = 1,
                _ => {}
            }
        }
        match b {
            Button::Up => c.row = c.row.saturating_sub(1),
            Button::Down => c.row = (c.row + 1).min(rows - 1),
            Button::Left => c.col = c.col.saturating_sub(1),
            Button::Right => c.col = (c.col + 1).min(cols - 1),
            _ => {}
        }
        self.set_cur(c);
    }

    /// SHIFT + direction: move between views along the M8's map.
    fn navigate(&mut self, b: Button) {
        let cur = self.cur();
        let next = match (self.view, b) {
            (View::Song, Button::Right) => {
                let chain = self.song.rows[cur.row][cur.col];
                if chain == EMPTY {
                    return self.flash("NO CHAIN HERE");
                }
                self.chain = chain as usize;
                self.chain_ctx = Some(Ctx {
                    track: cur.col,
                    row: cur.row,
                    chain_step: 0,
                });
                View::Chain
            }
            (View::Song, Button::Up) => View::Project,
            (View::Project, Button::Down) => View::Song,
            (View::Chain, Button::Left) => View::Song,
            (View::Chain, Button::Right) => {
                let ph = self.song.chains[self.chain].steps[cur.row].phrase;
                if ph == EMPTY {
                    return self.flash("NO PHRASE HERE");
                }
                self.phrase = ph as usize;
                self.phrase_ctx = self.chain_ctx.map(|c| Ctx {
                    chain_step: cur.row,
                    ..c
                });
                View::Phrase
            }
            (View::Phrase, Button::Left) => {
                if let Some(c) = self.phrase_ctx {
                    let chain = self.song.rows[c.row][c.track];
                    if chain != EMPTY {
                        self.chain = chain as usize;
                        self.chain_ctx = Some(c);
                        self.cursors.insert(
                            View::Chain,
                            Cursor {
                                row: c.chain_step,
                                col: 0,
                            },
                        );
                    }
                }
                View::Chain
            }
            (View::Phrase, Button::Right) => {
                let steps = &self.song.phrases[self.phrase].steps;
                let inst = (0..=cur.row)
                    .rev()
                    .map(|r| steps[r].instrument)
                    .find(|&i| i != EMPTY)
                    .unwrap_or(self.instrument);
                self.instrument = inst.min(INSTRUMENTS as u8 - 1);
                View::Instrument
            }
            (View::Phrase, Button::Up) => View::Groove,
            (View::Groove, Button::Down) => View::Phrase,
            (View::Instrument, Button::Left) => View::Phrase,
            (View::Instrument, Button::Right) => {
                self.table = self.instrument as usize;
                View::Table
            }
            (View::Table, Button::Left) => View::Instrument,
            _ => return,
        };
        self.selection = None;
        self.view = next;
    }

    /// OPTION + direction: next/previous chain, phrase, instrument...
    fn option_move(&mut self, b: Button) {
        let step: i32 = match b {
            Button::Left | Button::Up => -1,
            _ => 1,
        };
        let vertical = matches!(b, Button::Up | Button::Down);
        match self.view {
            View::Song => {
                if vertical {
                    let mut c = self.cur();
                    c.row = (c.row as i32 + step * 16).clamp(0, SONG_ROWS as i32 - 1) as usize;
                    self.set_cur(c);
                }
            }
            View::Chain => {
                let Some(mut ctx) = self.chain_ctx else {
                    return;
                };
                if vertical {
                    let r = ctx.row as i32 + step;
                    if !(0..SONG_ROWS as i32).contains(&r) {
                        return;
                    }
                    ctx.row = r as usize;
                } else {
                    let t = ctx.track as i32 + step;
                    if !(0..TRACKS as i32).contains(&t) {
                        return;
                    }
                    ctx.track = t as usize;
                }
                let chain = self.song.rows[ctx.row][ctx.track];
                if chain != EMPTY {
                    self.chain = chain as usize;
                    self.chain_ctx = Some(ctx);
                }
            }
            View::Phrase => {
                let Some(mut ctx) = self.phrase_ctx else {
                    return;
                };
                if vertical {
                    let chain = self.song.rows[ctx.row][ctx.track];
                    if chain == EMPTY {
                        return;
                    }
                    let steps = &self.song.chains[chain as usize].steps;
                    let mut s = ctx.chain_step as i32 + step;
                    while (0..STEPS as i32).contains(&s) {
                        if steps[s as usize].phrase != EMPTY {
                            ctx.chain_step = s as usize;
                            self.phrase = steps[s as usize].phrase as usize;
                            self.phrase_ctx = Some(ctx);
                            return;
                        }
                        s += step;
                    }
                } else {
                    let t = ctx.track as i32 + step;
                    if !(0..TRACKS as i32).contains(&t) {
                        return;
                    }
                    ctx.track = t as usize;
                    let chain = self.song.rows[ctx.row][ctx.track];
                    if chain == EMPTY {
                        return;
                    }
                    let steps = &self.song.chains[chain as usize].steps;
                    let pick = if steps[ctx.chain_step].phrase != EMPTY {
                        Some(ctx.chain_step)
                    } else {
                        (0..STEPS).find(|&s| steps[s].phrase != EMPTY)
                    };
                    if let Some(s) = pick {
                        ctx.chain_step = s;
                        self.phrase = steps[s].phrase as usize;
                        self.phrase_ctx = Some(ctx);
                    }
                }
            }
            View::Instrument => {
                let d = if vertical { step * 16 } else { step };
                self.instrument =
                    (self.instrument as i32 + d).clamp(0, INSTRUMENTS as i32 - 1) as u8;
            }
            View::Table => {
                let d = if vertical { step * 16 } else { step };
                self.table = (self.table as i32 + d).clamp(0, TABLES as i32 - 1) as usize;
            }
            View::Groove => {
                let d = if vertical { step * 16 } else { step };
                self.groove = (self.groove as i32 + d).clamp(0, GROOVES as i32 - 1) as usize;
            }
            View::Project => {}
        }
    }

    // --- playback ---------------------------------------------------------------

    fn play(&mut self, p: Press) {
        let Some(rack) = &self.rack else {
            return self.flash("NO AUDIO");
        };
        let sh = rack.shared.clone();
        let h = p.held;
        if h.edit {
            // Preview the instrument.
            let inst = match self.view {
                View::Instrument | View::Table => self.instrument,
                _ => self.last.get(&Kind::Instrument).copied().unwrap_or(0),
            };
            return self.preview_note(inst, 36);
        }
        let track = self.current_track();
        if h.option && h.shift {
            self.muted = 0;
            self.soloed = 0;
            sh.set_mutes(0, 0);
            return;
        }
        if h.option {
            self.soloed ^= 1 << track;
            sh.set_mutes(self.muted, self.soloed);
            return;
        }
        if sh.is_playing() {
            return sh.stop_song();
        }
        let song_row = self.cursors.get(&View::Song).map_or(0, |c| c.row);
        match (self.view, h.shift) {
            (View::Chain, false) => match self.chain_ctx {
                Some(c) => sh.play_chain(c.track, c.row),
                None => sh.play_from(song_row),
            },
            (View::Chain, true) => sh.play_from(self.chain_ctx.map_or(song_row, |c| c.row)),
            (View::Phrase | View::Instrument | View::Table, false) => {
                let c = self.phrase_ctx.unwrap_or(Ctx {
                    track: 0,
                    row: song_row,
                    chain_step: 0,
                });
                let chain = self.song.rows[c.row][c.track];
                sh.play_phrase(
                    c.track,
                    Pos {
                        row: c.row,
                        chain: if chain == EMPTY { 0 } else { chain as usize },
                        chain_step: c.chain_step,
                        phrase: self.phrase,
                        phrase_step: 0,
                    },
                );
            }
            (View::Phrase | View::Instrument | View::Table, true) => {
                sh.play_from(self.phrase_ctx.map_or(song_row, |c| c.row))
            }
            _ => sh.play_from(song_row),
        }
    }

    fn current_track(&self) -> usize {
        match self.view {
            View::Song => self.cur().col,
            View::Chain => self.chain_ctx.map_or(0, |c| c.track),
            _ => self.phrase_ctx.map_or(0, |c| c.track),
        }
    }

    fn mute_track(&mut self) {
        let t = self.current_track();
        self.muted ^= 1 << t;
        if let Some(r) = &self.rack {
            r.shared.set_mutes(self.muted, self.soloed);
        }
    }

    fn preview_note(&mut self, inst: u8, note: u8) {
        let Some(r) = &self.rack else { return };
        if let Some((_, i, n)) = self.preview.take() {
            r.shared.preview(i, n, false);
        }
        r.shared.preview(inst, note as i16, true);
        self.preview = Some((
            Instant::now() + Duration::from_millis(400),
            inst,
            note as i16,
        ));
    }

    // --- grid cells -------------------------------------------------------------

    fn cell(&self, view: View, r: usize, c: usize) -> u8 {
        match view {
            View::Song => self.song.rows[r][c],
            View::Chain => {
                let s = self.song.chains[self.chain].steps[r];
                [s.phrase, s.transpose][c]
            }
            View::Phrase => {
                let s = &self.song.phrases[self.phrase].steps[r];
                match c {
                    0 => s.note,
                    1 => s.volume,
                    2 => s.instrument,
                    _ => {
                        let fx = s.fx[(c - 3) / 2];
                        if (c - 3) % 2 == 0 {
                            fx.command
                        } else {
                            fx.value
                        }
                    }
                }
            }
            View::Table => {
                let s = &self.song.tables[self.table].steps[r];
                match c {
                    0 => s.transpose,
                    1 => s.volume,
                    _ => {
                        let fx = s.fx[(c - 2) / 2];
                        if (c - 2) % 2 == 0 {
                            fx.command
                        } else {
                            fx.value
                        }
                    }
                }
            }
            View::Groove => self.song.grooves[self.groove].steps[r],
            _ => EMPTY,
        }
    }

    fn set_cell(&mut self, view: View, r: usize, c: usize, v: u8) {
        match view {
            View::Song => self.song.rows[r][c] = v,
            View::Chain => {
                let s = &mut self.song.chains[self.chain].steps[r];
                match c {
                    0 => s.phrase = v,
                    _ => s.transpose = v,
                }
            }
            View::Phrase => {
                let s = &mut self.song.phrases[self.phrase].steps[r];
                match c {
                    0 => s.note = v,
                    1 => s.volume = v,
                    2 => s.instrument = v,
                    _ => {
                        let fx = &mut s.fx[(c - 3) / 2];
                        if (c - 3) % 2 == 0 {
                            fx.command = v
                        } else {
                            fx.value = v
                        }
                    }
                }
            }
            View::Table => {
                let s = &mut self.song.tables[self.table].steps[r];
                match c {
                    0 => s.transpose = v,
                    1 => s.volume = v,
                    _ => {
                        let fx = &mut s.fx[(c - 2) / 2];
                        if (c - 2) % 2 == 0 {
                            fx.command = v
                        } else {
                            fx.value = v
                        }
                    }
                }
            }
            View::Groove => self.song.grooves[self.groove].steps[r] = v,
            _ => return,
        }
        self.changed();
    }

    fn kind_at(&self, c: usize) -> Option<Kind> {
        self.grid().and_then(|(_, cols)| cols.get(c).copied())
    }

    // --- editing ---------------------------------------------------------------

    /// EDIT on its own: insert on an empty cell, a new chain/phrase/instrument
    /// on a double tap.
    fn edit_tap(&mut self) {
        match self.view {
            View::Instrument => return self.instrument_edit(0),
            View::Project => return self.project_edit(0),
            _ => {}
        }
        let c = self.cur();
        let Some(kind) = self.kind_at(c.col) else {
            return;
        };
        let now = Instant::now();
        let double = self.last_edit.is_some_and(|(t, v, r, col)| {
            now - t < DOUBLE_TAP && v == self.view && r == c.row && col == c.col
        });
        self.last_edit = Some((now, self.view, c.row, c.col));
        let val = self.cell(self.view, c.row, c.col);
        if double && matches!(kind, Kind::Chain | Kind::Phrase | Kind::Instrument) {
            if let Some(n) = self.unused(kind) {
                self.set_cell(self.view, c.row, c.col, n);
                self.last.insert(kind, n);
                self.flash(format!("NEW {} {n:02X}", kind_name(kind)));
            }
            return;
        }
        if val == EMPTY && kind.can_be_empty() {
            let v = self.last.get(&kind).copied().unwrap_or(kind.default());
            self.insert(c, kind, v);
        } else if kind == Kind::Note && val < 0x80 {
            self.preview_step(c.row);
        }
    }

    /// Put `v` into an empty cell, with what the M8 fills in alongside it.
    fn insert(&mut self, c: Cursor, kind: Kind, v: u8) {
        self.set_cell(self.view, c.row, c.col, v);
        match kind {
            Kind::Note if self.view == View::Phrase => {
                if self.cell(View::Phrase, c.row, 2) == EMPTY {
                    let inst = self.last.get(&Kind::Instrument).copied().unwrap_or(0);
                    self.set_cell(View::Phrase, c.row, 2, inst);
                }
                self.preview_step(c.row);
            }
            Kind::FxCmd => {
                let val = self.last.get(&Kind::FxVal).copied().unwrap_or(0);
                self.set_cell(self.view, c.row, c.col + 1, val);
            }
            _ => {}
        }
    }

    fn preview_step(&mut self, row: usize) {
        let s = self.song.phrases[self.phrase].steps[row];
        if s.note >= 0x80 {
            return;
        }
        let inst = if s.instrument == EMPTY {
            0
        } else {
            s.instrument
        };
        self.preview_note(inst, s.note);
    }

    /// EDIT + direction: change the value under the cursor.
    fn edit_adjust(&mut self, b: Button) {
        let delta_for = |kind_big: i32| match b {
            Button::Left => -1,
            Button::Right => 1,
            Button::Down => -kind_big,
            _ => kind_big,
        };
        match self.view {
            View::Instrument => return self.instrument_edit(delta_for(16)),
            View::Project => return self.project_edit(delta_for(10)),
            _ => {}
        }
        let c = self.cur();
        let Some(kind) = self.kind_at(c.col) else {
            return;
        };
        let val = self.cell(self.view, c.row, c.col);
        if val == EMPTY && kind.can_be_empty() {
            let v = self.last.get(&kind).copied().unwrap_or(kind.default());
            return self.insert(c, kind, v);
        }
        if kind == Kind::Note && val >= 0x80 {
            return;
        }
        let d = delta_for(kind.big_step());
        let v = if kind == Kind::FxCmd {
            // Sequencer effects, then the instrument (sampler) effects.
            let all: Vec<u8> = fx_commands().collect();
            let i = all.iter().position(|&c| c == val).unwrap_or(0) as i32;
            all[(i + d).clamp(0, all.len() as i32 - 1) as usize]
        } else if kind == Kind::Transpose {
            (val as i32 + d).rem_euclid(256) as u8
        } else {
            (val as i32 + d).clamp(0, kind.max() as i32) as u8
        };
        if v != val {
            self.set_cell(self.view, c.row, c.col, v);
        }
        self.last.insert(kind, v);
        if kind == Kind::Note {
            self.preview_step(c.row);
        }
    }

    /// EDIT + OPTION: clear the value (an empty note becomes OFF).
    fn delete(&mut self) {
        match self.view {
            View::Instrument => return self.instrument_reset(),
            View::Project => return,
            _ => {}
        }
        let c = self.cur();
        let Some(kind) = self.kind_at(c.col) else {
            return;
        };
        let val = self.cell(self.view, c.row, c.col);
        if val != EMPTY && kind.can_be_empty() {
            self.last.insert(kind, val);
        }
        match kind {
            Kind::Note if val == EMPTY => self.set_cell(self.view, c.row, c.col, 0x80),
            Kind::Transpose | Kind::FxVal => self.set_cell(self.view, c.row, c.col, 0),
            Kind::FxCmd => {
                self.set_cell(self.view, c.row, c.col, EMPTY);
                self.set_cell(self.view, c.row, c.col + 1, 0);
            }
            _ => self.set_cell(self.view, c.row, c.col, EMPTY),
        }
    }

    /// The first chain, phrase or instrument nothing uses yet.
    fn unused(&self, kind: Kind) -> Option<u8> {
        let s = &self.song;
        match kind {
            Kind::Chain => (0..CHAINS).find(|&c| {
                !s.rows.iter().any(|r| r.contains(&(c as u8)))
                    && s.chains[c].steps.iter().all(|st| st.phrase == EMPTY)
            }),
            Kind::Phrase => (0..PHRASES).find(|&p| {
                !s.chains
                    .iter()
                    .any(|c| c.steps.iter().any(|st| st.phrase == p as u8))
                    && s.phrases[p]
                        .steps
                        .iter()
                        .all(|st| st.note == EMPTY && st.instrument == EMPTY)
            }),
            Kind::Instrument => {
                let used = s.used_instruments();
                let own: HashSet<u8> = self.map.numbered().collect();
                (0..INSTRUMENTS).find(|&i| {
                    s.instruments[i].kind == InstrumentKind::None
                        && !used.contains(&(i as u8))
                        && !own.contains(&(i as u8))
                })
            }
            _ => None,
        }
        .map(|n| n as u8)
    }

    // --- selection, copy and paste ---------------------------------------------

    fn start_selection(&mut self) {
        if self.grid().is_none() {
            return;
        }
        self.selection = Some(Selection {
            anchor: self.cur(),
            mode: SelMode::Box,
        });
        self.flash("SELECT: OPTION COPIES, EDIT+OPTION CUTS");
    }

    /// The selected rectangle: (row0, col0, row1, col1), inclusive.
    fn selected(&self) -> Option<(usize, usize, usize, usize)> {
        let s = self.selection?;
        let (rows, cols) = self.extent();
        let c = self.cur();
        let (r0, r1) = (s.anchor.row.min(c.row), s.anchor.row.max(c.row));
        let (c0, c1) = (s.anchor.col.min(c.col), s.anchor.col.max(c.col));
        Some(match s.mode {
            SelMode::Box => (r0, c0, r1, c1),
            SelMode::Columns => (0, c0, rows - 1, c1),
            SelMode::Rows => (r0, 0, r1, cols - 1),
            SelMode::All => (0, 0, rows - 1, cols - 1),
        })
    }

    fn selection_press(&mut self, p: Press) {
        let h = p.held;
        match p.button {
            Button::Option if h.shift => {
                let s = self.selection.as_mut().unwrap();
                s.mode = match s.mode {
                    SelMode::Box => SelMode::Columns,
                    SelMode::Columns => SelMode::Rows,
                    SelMode::Rows => SelMode::All,
                    SelMode::All => {
                        self.selection = None;
                        return;
                    }
                };
            }
            Button::Option if h.edit => {
                self.copy_selection();
                self.clear_selection_cells();
                self.selection = None;
                self.flash("CUT");
            }
            Button::Edit if h.option => {
                self.copy_selection();
                self.clear_selection_cells();
                self.selection = None;
                self.flash("CUT");
            }
            Button::Option if h.none() => {
                self.copy_selection();
                self.selection = None;
                self.flash("COPIED");
            }
            Button::Edit if h.shift => {
                self.selection = None;
                self.clone_under_cursor();
            }
            b if b.is_direction() && h.none() => self.move_cursor(b),
            _ => {}
        }
    }

    fn copy_selection(&mut self) {
        let Some((r0, c0, r1, c1)) = self.selected() else {
            return;
        };
        let Some((_, kinds)) = self.grid() else {
            return;
        };
        let cells = (r0..=r1)
            .map(|r| (c0..=c1).map(|c| self.cell(self.view, r, c)).collect())
            .collect();
        self.clip = Some(Clip {
            view: self.view,
            col: c0,
            kinds: kinds[c0..=c1].to_vec(),
            cells,
        });
    }

    fn clear_selection_cells(&mut self) {
        let Some((r0, c0, r1, c1)) = self.selected() else {
            return;
        };
        for r in r0..=r1 {
            for c in c0..=c1 {
                let kind = self.kind_at(c).unwrap();
                let v = if kind.can_be_empty() { EMPTY } else { 0 };
                self.set_cell(self.view, r, c, v);
            }
        }
    }

    /// SHIFT + EDIT: paste at the cursor. Columns of another kind are pasted
    /// back into the columns they came from.
    fn paste(&mut self) {
        let Some(clip) = self.clip.take() else {
            return self.flash("NOTHING TO PASTE");
        };
        let (rows, kinds) = match self.grid() {
            Some(g) if clip.view == self.view => g,
            _ => {
                self.clip = Some(clip);
                return self.flash("CAN'T PASTE HERE");
            }
        };
        let c = self.cur();
        let fits = c.col + clip.kinds.len() <= kinds.len()
            && kinds[c.col..c.col + clip.kinds.len()] == clip.kinds[..];
        let col0 = if fits { c.col } else { clip.col };
        for (i, row) in clip.cells.iter().enumerate() {
            let r = c.row + i;
            if r >= rows {
                break;
            }
            for (j, &v) in row.iter().enumerate() {
                self.set_cell(self.view, r, col0 + j, v);
            }
        }
        self.clip = Some(clip);
    }

    /// SHIFT+OPTION then EDIT on a chain, phrase or instrument number: copy
    /// it to an unused number and point the cell there.
    fn clone_under_cursor(&mut self) {
        let c = self.cur();
        let Some(kind) = self.kind_at(c.col) else {
            return;
        };
        let val = self.cell(self.view, c.row, c.col);
        if val == EMPTY || !matches!(kind, Kind::Chain | Kind::Phrase | Kind::Instrument) {
            return;
        }
        let Some(n) = self.unused(kind) else {
            return self.flash("NONE FREE");
        };
        let (from, to) = (val as usize, n as usize);
        match kind {
            Kind::Chain => self.song.chains[to] = self.song.chains[from].clone(),
            Kind::Phrase => self.song.phrases[to] = self.song.phrases[from].clone(),
            _ => {
                self.song.instruments[to] = self.song.instruments[from].clone();
                if let Some(slot) = self.slots.iter().find(|s| s.instrument == val) {
                    self.map.set_number(n, Some(slot.source.clone()));
                    if let Some(state) = slot.plugin.save_state() {
                        self.map.states.insert(n, state);
                    }
                }
            }
        }
        self.set_cell(self.view, c.row, c.col, n);
        self.flash(format!("CLONED {} {val:02X} TO {n:02X}", kind_name(kind)));
    }

    // --- instrument view ------------------------------------------------------

    fn slot(&self, inst: u8) -> Option<&Slot> {
        self.slots.iter().find(|s| s.instrument == inst)
    }

    fn refresh_params(&mut self) {
        let key = (self.instrument, self.slots.len());
        if self.params_for == Some(key) {
            return;
        }
        self.params = self
            .slot(self.instrument)
            .map(|s| {
                s.plugin
                    .params()
                    .into_iter()
                    .filter(|p| !p.is_hidden())
                    .collect()
            })
            .unwrap_or_default();
        self.params_for = Some(key);
        let mut c = self
            .cursors
            .get(&View::Instrument)
            .copied()
            .unwrap_or_default();
        c.row = c.row.min(INST_FIELDS + self.params.len().max(1) - 1);
        self.cursors.insert(View::Instrument, c);
    }

    /// EDIT (delta 0) or EDIT+direction on an instrument view row.
    fn instrument_edit(&mut self, delta: i32) {
        let n = self.instrument;
        let row = self.cur().row;
        let sampler = self.is_sampler(n);
        let inst = &mut self.song.instruments[n as usize];
        match row {
            0 => {
                const KINDS: [InstrumentKind; 6] = [
                    InstrumentKind::Wavsynth,
                    InstrumentKind::Macrosynth,
                    InstrumentKind::Sampler,
                    InstrumentKind::MidiOut,
                    InstrumentKind::FmSynth,
                    InstrumentKind::None,
                ];
                let i = KINDS.iter().position(|k| *k == inst.kind).unwrap_or(5) as i32;
                let i = (i + delta.signum()).rem_euclid(KINDS.len() as i32);
                inst.set_kind(KINDS[i as usize]);
                self.changed();
            }
            1 if delta == 0 => {
                self.modal = Some(Modal::Text {
                    label: "NAME",
                    value: inst.name.clone(),
                    target: TextTarget::InstrumentName(n),
                })
            }
            2 if delta == 0 && sampler && self.sample_col == 1 => self.open_recorder(n),
            2 if sampler && self.sample_col == 1 => {}
            2 if delta == 0 && sampler => self.open_sample_browser(n),
            2 if sampler => self.step_sample(n, delta),
            2 if delta == 0 => self.open_plugin_browser(n),
            2 => self.step_plugin(n, delta),
            3 if delta != 0 => {
                inst.transpose = !inst.transpose;
                self.changed();
            }
            4 if delta != 0 => {
                inst.table_tick = (inst.table_tick as i32 + delta).clamp(0, 0xFF) as u8;
                self.changed();
            }
            r if r >= INST_FIELDS && delta != 0 && sampler => {
                self.nudge_sampler(r - INST_FIELDS, delta)
            }
            r if r >= INST_FIELDS && delta != 0 => self.nudge_param(r - INST_FIELDS, delta),
            _ => {}
        }
    }

    /// EDIT+OPTION on the instrument view: plugin row removes the plugin,
    /// a parameter goes back to its default.
    fn instrument_reset(&mut self) {
        let n = self.instrument;
        let sampler = self.is_sampler(n);
        match self.cur().row {
            2 if sampler => {
                self.song.instruments[n as usize].sample_path.clear();
                self.changed();
            }
            r if r >= INST_FIELDS && sampler => {
                if let Some(&(_, i, _)) = SAMPLER_ROWS.get(r - INST_FIELDS) {
                    self.song.instruments[n as usize].set_sampler_param(i, sampler_default(i));
                    self.changed();
                }
            }
            2 => {
                self.map.set_number(n, None);
                self.map.states.remove(&n);
                self.unsaved = true;
                self.rebuild_audio();
            }
            r if r >= INST_FIELDS => {
                if let Some(p) = self.params.get(r - INST_FIELDS).cloned() {
                    self.set_param(&p, p.default);
                }
            }
            _ => {}
        }
    }

    fn param_value(&self, p: &ParamInfo) -> f64 {
        if let Some((v, _)) = self.param_hold.get(&(self.instrument, p.id)) {
            return *v;
        }
        self.slot(self.instrument)
            .and_then(|s| s.plugin.param_value(p.id))
            .unwrap_or(p.default)
    }

    fn nudge_param(&mut self, i: usize, delta: i32) {
        let Some(p) = self.params.get(i).cloned() else {
            return;
        };
        if p.is_readonly() {
            return;
        }
        let range = p.max - p.min;
        let v = self.param_value(&p);
        let next = if p.is_stepped() {
            let step = if delta.abs() > 1 {
                (range / 10.0).round().max(1.0)
            } else {
                1.0
            };
            v.round() + delta.signum() as f64 * step
        } else {
            let frac = if delta.abs() > 1 { 0.1 } else { 0.01 };
            v + delta.signum() as f64 * frac * range
        };
        self.set_param(&p, next.clamp(p.min, p.max));
    }

    fn set_param(&mut self, p: &ParamInfo, v: f64) {
        let n = self.instrument;
        let change = ParamChange {
            id: p.id,
            cookie: p.cookie,
            value: v,
        };
        let Some(slot) = self.slot(n) else { return };
        match &self.rack {
            Some(r) if slot.plugin.is_active() => r.shared.set_param(n, change),
            _ => slot.plugin.flush_inactive(&[change]),
        }
        self.param_hold
            .insert((n, p.id), (v, Instant::now() + Duration::from_millis(300)));
        self.unsaved = true;
    }

    /// Whether the built-in sampler plays instrument `n`.
    fn is_sampler(&self, n: u8) -> bool {
        self.map
            .builtin_sampler(n, &self.song.instruments[n as usize])
    }

    fn nudge_sampler(&mut self, row: usize, delta: i32) {
        let Some(&(_, i, show)) = SAMPLER_ROWS.get(row) else {
            return;
        };
        let inst = &mut self.song.instruments[self.instrument as usize];
        let v = inst.sampler_param(i) as i32;
        let next = match show {
            Show::Names(names) => (v + delta.signum()).clamp(0, names.len() as i32 - 1),
            Show::Hex | Show::Slice => (v + delta).clamp(0, 0xFF),
        };
        inst.set_sampler_param(i, next as u8);
        self.changed();
    }

    /// The card root and the song's folder, for sample paths.
    fn sample_dirs(&self) -> (PathBuf, PathBuf) {
        let dir = self.song_dir();
        (card_root(&dir, &self.song.directory), dir)
    }

    /// The file instrument `n` plays, if it is found.
    fn sample_file(&self, n: u8) -> Option<PathBuf> {
        let (root, dir) = self.sample_dirs();
        resolve_sample_path(&root, &dir, &self.song.instruments[n as usize].sample_path)
    }

    fn open_sample_browser(&mut self, instrument: u8) {
        let (root, dir) = self.sample_dirs();
        let file = self.sample_file(instrument);
        let start = file
            .as_ref()
            .and_then(|f| f.parent().map(|d| d.to_path_buf()))
            .or_else(|| Some(root.join("Samples")).filter(|d| d.is_dir()))
            .unwrap_or(dir);
        self.browse_samples(instrument, start, file);
    }

    fn browse_samples(&mut self, instrument: u8, dir: PathBuf, select: Option<PathBuf>) {
        let mut list = vec![dir.join("..")];
        list.extend(dir_entries(&dir, is_wav));
        let cur = select
            .and_then(|f| list.iter().position(|p| *p == f))
            .unwrap_or(0);
        self.modal = Some(Modal::Samples {
            instrument,
            dir,
            list,
            cur,
        });
        self.audition_cursor();
    }

    /// In the sample browser: play the file under the cursor.
    fn audition_cursor(&mut self) {
        let Some(Modal::Samples { list, cur, .. }) = &self.modal else {
            return;
        };
        let file = list[*cur].clone();
        self.audition(Some(&file));
    }

    /// Play a sample file as it is, or stop (`None`, or not a sample).
    fn audition(&mut self, file: Option<&std::path::Path>) {
        let sample = match file.filter(|f| is_wav(f) && f.is_file()) {
            Some(f) => match self.bank.get(f) {
                Ok(s) => Some(s),
                Err(e) => {
                    self.flash(e);
                    None
                }
            },
            None => None,
        };
        if let Some(r) = &self.rack {
            r.shared.audition(sample);
        }
    }

    /// EDIT+LEFT/RIGHT on SAMPLE: the previous or next file in its folder.
    fn step_sample(&mut self, instrument: u8, delta: i32) {
        let Some(file) = self.sample_file(instrument) else {
            return self.open_sample_browser(instrument);
        };
        let dir = file.parent().map(|d| d.to_path_buf()).unwrap_or_default();
        let files: Vec<PathBuf> = dir_entries(&dir, is_wav)
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
        let Some(i) = files.iter().position(|p| *p == file) else {
            return;
        };
        let j = (i as i32 + delta.signum()).clamp(0, files.len() as i32 - 1) as usize;
        if j != i {
            self.choose_sample(instrument, &files[j]);
            self.audition(Some(&files[j]));
        }
    }

    /// Give `instrument` a sample file. A name taken from the old sample's
    /// file follows the new one.
    fn choose_sample(&mut self, instrument: u8, file: &std::path::Path) {
        let (root, _) = self.sample_dirs();
        let path = sample_path_for(&root, file);
        if path.len() > 127 {
            return self.flash("PATH TOO LONG FOR AN M8 SONG (127 MAX)");
        }
        let stem = |p: &std::path::Path| -> String {
            p.file_stem()
                .map(|s| s.to_string_lossy().to_uppercase())
                .unwrap_or_default()
                .chars()
                .take(12)
                .collect()
        };
        let inst = &mut self.song.instruments[instrument as usize];
        inst.set_kind(InstrumentKind::Sampler);
        let old = stem(std::path::Path::new(&inst.sample_path));
        if inst.name.is_empty() || (!old.is_empty() && inst.name == old) {
            inst.name = stem(file);
        }
        inst.sample_path = path;
        self.changed();
    }

    /// Every plugin in the standard CLAP folders and `$CLAP_PATH`, plus the
    /// ones the instrument map names wherever they are.
    fn installed_plugins(&mut self) -> Vec<PluginDescriptor> {
        if let Some(list) = &self.installed {
            return list.clone();
        }
        let mut files = Vec::new();
        for dir in crate::discovery::search_paths() {
            crate::discovery::find_clap_files(&dir, &mut files);
        }
        files.extend(self.map.plugins().map(|p| p.path.clone()));
        files.sort();
        files.dedup();
        let scan = crate::discovery::scan_files(&files);
        let mut list = scan.plugins;
        list.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.path.cmp(&b.path))
        });
        list.dedup_by(|a, b| a.path == b.path && a.id == b.id);
        if !scan.errors.is_empty() {
            self.flash(format!(
                "{} PLUGIN FILE(S) FAILED TO LOAD",
                scan.errors.len()
            ));
        }
        self.installed = Some(list.clone());
        list
    }

    /// Position of the instrument's current plugin in `list`.
    fn current_plugin(&self, instrument: u8, list: &[PluginDescriptor]) -> Option<usize> {
        let src = &self.slot(instrument)?.source;
        list.iter()
            .position(|d| d.path == src.path && src.id.as_ref().map_or(true, |id| *id == d.id))
    }

    fn open_plugin_browser(&mut self, instrument: u8) {
        // Scan again each time, so newly installed plugins show up.
        self.installed = None;
        let list = self.installed_plugins();
        if list.is_empty() {
            return self.flash("NO CLAP PLUGINS FOUND");
        }
        let cur = self.current_plugin(instrument, &list).unwrap_or(0);
        self.modal = Some(Modal::Plugins {
            instrument,
            list,
            cur,
        });
    }

    /// EDIT+LEFT/RIGHT on PLUGIN: the previous or next installed plugin.
    fn step_plugin(&mut self, instrument: u8, delta: i32) {
        let list = self.installed_plugins();
        if list.is_empty() {
            return self.flash("NO CLAP PLUGINS FOUND");
        }
        let i = match self.current_plugin(instrument, &list) {
            Some(i) => (i as i32 + delta.signum()).rem_euclid(list.len() as i32) as usize,
            None if delta > 0 => 0,
            None => list.len() - 1,
        };
        self.choose_plugin(instrument, &list[i]);
    }

    /// Give `instrument` its own plugin.
    fn choose_plugin(&mut self, instrument: u8, d: &PluginDescriptor) {
        self.map.set_number(
            instrument,
            Some(PluginRef {
                path: d.path.clone(),
                id: Some(d.id.clone()),
            }),
        );
        self.map.states.remove(&instrument);
        let inst = &mut self.song.instruments[instrument as usize];
        if inst.kind == InstrumentKind::None {
            inst.set_kind(InstrumentKind::Wavsynth);
        }
        if inst.name.is_empty() {
            inst.name = d.name.to_uppercase().chars().take(12).collect();
        }
        self.changed();
        self.rebuild_audio();
        self.flash(format!("{} ON {instrument:02X}", d.name));
    }

    // --- project view -----------------------------------------------------------

    fn project_edit(&mut self, delta: i32) {
        let row = self.cur().row;
        match row {
            0 if delta != 0 => {
                let step = if delta.abs() > 1 { 10.0 } else { 1.0 };
                self.song.tempo =
                    (self.song.tempo + delta.signum() as f32 * step).clamp(1.0, 999.0);
                self.changed();
            }
            1 if delta != 0 => {
                self.song.transpose = (self.song.transpose as i32 + delta).rem_euclid(256) as u8;
                self.changed();
            }
            2 if delta == 0 => {
                self.modal = Some(Modal::Text {
                    label: "NAME",
                    value: self.song.name.clone(),
                    target: TextTarget::SongName,
                })
            }
            3 if delta == 0 => match self.project_col {
                0 => self.save(),
                1 => {
                    if self.confirmed("LOAD") {
                        self.open_song_browser();
                    }
                }
                _ => {
                    if self.confirmed("NEW") {
                        self.new_song();
                    }
                }
            },
            4 if delta != 0 => {
                self.render_seconds = (self.render_seconds as i32 + delta).clamp(1, 3600) as u32;
            }
            4 => self.render(),
            5 if delta != 0 => {
                if self.devices.is_empty() {
                    self.devices = audio::output_devices();
                }
                let mut opts: Vec<Option<String>> = vec![None];
                opts.extend(self.devices.iter().cloned().map(Some));
                self.audio.device = cycle(&opts, &self.audio.device, delta);
            }
            6 if delta != 0 => {
                let mut opts = vec![None];
                opts.extend(settings::SAMPLE_RATES.iter().copied().map(Some));
                self.audio.sample_rate = cycle(&opts, &self.audio.sample_rate, delta);
            }
            7 if delta != 0 => {
                let mut opts = vec![None];
                opts.extend(settings::BUFFER_SIZES.iter().copied().map(Some));
                self.audio.buffer_size = cycle(&opts, &self.audio.buffer_size, delta);
            }
            8 if delta == 0 => {
                self.use_device = true;
                self.rebuild_audio();
                match self.audio.save() {
                    Ok(_) => self.flash("AUDIO SETTINGS SAVED"),
                    Err(e) => self.flash(format!("{e:#}")),
                }
            }
            _ => {}
        }
    }

    /// Ask for a second EDIT before throwing away unsaved work.
    fn confirmed(&mut self, what: &'static str) -> bool {
        if !self.unsaved {
            return true;
        }
        if let Some((at, w)) = self.confirm {
            if w == what && at.elapsed() < Duration::from_secs(3) {
                self.confirm = None;
                return true;
            }
        }
        self.confirm = Some((Instant::now(), what));
        self.flash(format!("UNSAVED CHANGES. EDIT AGAIN TO {what}"));
        false
    }

    fn song_path(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(|| {
            let name = if self.song.name.is_empty() {
                "UNTITLED"
            } else {
                &self.song.name
            };
            PathBuf::from(format!("{name}.m8s"))
        })
    }

    fn save(&mut self) {
        let path = self.song_path();
        for s in &self.slots {
            if let Some(state) = s.plugin.save_state() {
                self.map.states.insert(s.instrument, state);
            }
        }
        let result = self
            .song
            .save(&path)
            .and_then(|_| self.map.save(&super::app::sidecar(&path), &self.song.name));
        match result {
            Ok(()) => {
                self.path = Some(path.clone());
                self.unsaved = false;
                self.flash(format!("SAVED {}", file_name(&path)));
            }
            Err(e) => self.flash(format!("SAVE FAILED: {e:#}")),
        }
    }

    fn open_song_browser(&mut self) {
        let dir = self
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        self.browse_songs(dir);
    }

    fn browse_songs(&mut self, dir: PathBuf) {
        let mut list = vec![dir.join("..")];
        list.extend(dir_entries(&dir, |p| {
            p.extension().is_some_and(|e| e.eq_ignore_ascii_case("m8s"))
        }));
        self.modal = Some(Modal::Songs { dir, list, cur: 0 });
    }

    fn load_song(&mut self, path: PathBuf) {
        match super::app::load_song_and_map(&path) {
            Ok((song, map)) => {
                self.stop_audio();
                self.slots.clear();
                self.song = song;
                self.map = map;
                self.path = Some(path.clone());
                self.reset_views();
                self.rebuild_audio();
                self.flash(format!("LOADED {}", file_name(&path)));
            }
            Err(e) => self.flash(format!("{e:#}")),
        }
    }

    fn new_song(&mut self) {
        self.stop_audio();
        self.slots.clear();
        self.song = Song::new_empty();
        self.map = InstrumentMap::default();
        self.path = None;
        self.reset_views();
        self.rebuild_audio();
        self.flash("NEW SONG");
    }

    fn reset_views(&mut self) {
        self.cursors.clear();
        self.song_top = 0;
        self.chain = 0;
        self.chain_ctx = None;
        self.phrase = 0;
        self.phrase_ctx = None;
        self.instrument = 0;
        self.table = 0;
        self.groove = 0;
        self.selection = None;
        self.unsaved = false;
        self.muted = 0;
        self.soloed = 0;
        self.view = View::Song;
    }

    fn render(&mut self) {
        let out = self.song_path().with_extension("wav");
        self.stop_audio();
        let rate = self.audio.sample_rate.unwrap_or(48_000);
        let r = render_wav(
            Arc::new(self.song.clone()),
            &mut self.slots,
            self.samplers.clone(),
            0,
            self.render_seconds as f64,
            rate,
            self.note_offset,
            &out,
        );
        self.rebuild_audio();
        match r {
            Ok(()) => self.flash(format!("RENDERED {}", file_name(&out))),
            Err(e) => self.flash(format!("RENDER FAILED: {e:#}")),
        }
    }

    // --- sample recording ---------------------------------------------------------

    /// Open the recording screen for sampler `instrument`.
    fn open_recorder(&mut self, instrument: u8) {
        let (root, _) = self.sample_dirs();
        let capture = Capture::new();
        let rec = Recording {
            instrument,
            row: REC_START,
            source: Source::LeftRight,
            device: None,
            devices: audio::input_devices(),
            vol: 0x80,
            arm: 0,
            play: None,
            name: recorder::next_name(&recorder::recordings_dir(&root)),
            capture,
            input: None,
            problem: None,
            take: None,
            sample: None,
            spare: Vec::new(),
            meter: 0.0,
            started_song: false,
        };
        self.recording = Some(Box::new(rec));
        self.connect_recording();
    }

    /// Point the capture at the chosen source: open the input device, or
    /// tap the master output.
    fn connect_recording(&mut self) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        rec.input = None;
        rec.problem = None;
        rec.capture.set_source(rec.source);
        rec.capture.set_gain(rec.gain());
        if rec.source.is_input() {
            match recorder::open_input(rec.device.as_deref(), rec.capture.clone()) {
                Ok(i) => rec.input = Some(i),
                Err(e) => rec.problem = Some(format!("{e:#}")),
            }
        }
        let tap = (!rec.source.is_input()).then(|| rec.capture.clone());
        if let Some(r) = &self.rack {
            r.shared.set_capture(tap);
        }
    }

    fn close_recorder(&mut self) {
        self.stop_recording();
        if let Some(r) = &self.rack {
            r.shared.set_capture(None);
            r.shared.audition(None);
        }
        self.recording = None;
    }

    /// The sample rate recordings from the current source come at.
    fn recording_rate(&self) -> Option<u32> {
        let rec = self.recording.as_ref()?;
        match &rec.input {
            Some(i) => Some(i.sample_rate),
            None if !rec.source.is_input() => self.rack.as_ref().map(|r| r.sample_rate),
            None => None,
        }
    }

    fn start_recording(&mut self) {
        let Some(rate) = self.recording_rate() else {
            return self.flash("NO AUDIO SOURCE TO RECORD");
        };
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        let room = rate as usize / 2;
        rec.take = Some(Take::new(rate, rec.source.is_mono()));
        rec.sample = None;
        rec.spare = Vec::with_capacity(room * 2);
        rec.capture.start(rec.arm as f32 / 255.0, room);
        rec.started_song = false;
        if let Some(r) = &self.rack {
            r.shared.audition(None);
            if let Some(row) = rec.play {
                r.shared.play_from(row as usize);
                rec.started_song = true;
            }
        }
    }

    fn stop_recording(&mut self) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        if !rec.capture.is_recording() {
            return;
        }
        rec.capture.stop();
        if rec.started_song {
            if let Some(r) = &self.rack {
                r.shared.stop_song();
            }
            rec.started_song = false;
        }
        Self::drain_recording(rec);
        match rec.take.as_ref() {
            Some(t) if t.frames() > 0 => rec.sample = Some(Arc::new(t.to_sample())),
            _ => {
                rec.take = None;
                self.flash("NOTHING RECORDED");
            }
        }
    }

    fn drain_recording(rec: &mut Recording) {
        rec.capture.drain(&mut rec.spare);
        if let Some(t) = rec.take.as_mut() {
            t.extend(&rec.spare);
        }
        rec.spare.clear();
    }

    fn tick_recording(&mut self) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        rec.meter = rec.capture.take_peak().max(rec.meter * 0.8);
        if !rec.capture.is_recording() {
            return;
        }
        Self::drain_recording(rec);
        let full = rec
            .take
            .as_ref()
            .is_some_and(|t| t.seconds() >= recorder::MAX_SECONDS as f64);
        if full {
            self.stop_recording();
            self.flash("LONGEST RECORDING REACHED");
        }
    }

    /// Write the take into the recordings folder and load it into the
    /// instrument.
    fn save_recording(&mut self) {
        let (root, _) = self.sample_dirs();
        let Some(rec) = self.recording.as_ref() else {
            return;
        };
        if rec.capture.is_recording() {
            return self.flash("STOP THE RECORDING FIRST");
        }
        let Some(take) = rec.take.as_ref() else {
            return self.flash("NOTHING TO SAVE YET");
        };
        let path = recorder::wav_path(&recorder::recordings_dir(&root), &rec.name);
        if path.exists() {
            let again = matches!(self.confirm, Some((at, "OVERWRITE")) if at.elapsed() < Duration::from_secs(3));
            if !again {
                self.confirm = Some((Instant::now(), "OVERWRITE"));
                return self.flash(format!("{} EXISTS. EDIT AGAIN TO OVERWRITE", rec.name));
            }
            self.confirm = None;
        }
        if let Err(e) = take.save(&path) {
            return self.flash(format!("SAVE FAILED: {e:#}"));
        }
        let instrument = rec.instrument;
        self.bank.forget(&path);
        self.close_recorder();
        self.sample_col = 0;
        self.choose_sample(instrument, &path);
        self.flash(format!("SAVED {}", file_name(&path)));
    }

    fn record_press(&mut self, p: Press) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        let h = p.held;
        let busy = rec.capture.is_recording();
        match p.button {
            Button::Up if h.none() => rec.row = rec.row.saturating_sub(1),
            Button::Down if h.none() => rec.row = (rec.row + 1).min(REC_ROWS.len() - 1),
            Button::Option if h.edit => match rec.row {
                REC_ARM => rec.arm = 0,
                REC_PLAY => rec.play = None,
                REC_VOL => {
                    rec.vol = 0x80;
                    rec.capture.set_gain(rec.gain());
                }
                _ => {}
            },
            Button::Edit if h.option => {
                if rec.row == REC_ARM {
                    rec.arm = 0;
                } else if rec.row == REC_PLAY {
                    rec.play = None;
                }
            }
            Button::Option if h.none() => {
                if busy {
                    return self.stop_recording();
                }
                let unsaved = rec.take.is_some();
                let again = matches!(self.confirm, Some((at, "DISCARD")) if at.elapsed() < Duration::from_secs(3));
                if unsaved && !again {
                    self.confirm = Some((Instant::now(), "DISCARD"));
                    return self.flash("NOT SAVED. OPTION AGAIN TO DISCARD");
                }
                self.confirm = None;
                self.close_recorder();
            }
            Button::Edit if h.none() => match rec.row {
                REC_START if busy => self.stop_recording(),
                REC_START => self.start_recording(),
                REC_NAME => {
                    self.modal = Some(Modal::Text {
                        label: "NAME",
                        value: rec.name.clone(),
                        target: TextTarget::RecordingFile,
                    })
                }
                REC_SAVE => self.save_recording(),
                _ => {}
            },
            b if b.is_direction() && h.edit => {
                let delta = match b {
                    Button::Left => -1,
                    Button::Right => 1,
                    Button::Down => -16,
                    _ => 16,
                };
                match rec.row {
                    REC_SRC if !busy => {
                        rec.source = cycle(&SOURCES, &rec.source, delta);
                        self.connect_recording();
                    }
                    REC_DEVICE if !busy && rec.source.is_input() => {
                        let mut opts: Vec<Option<String>> = vec![None];
                        opts.extend(rec.devices.iter().cloned().map(Some));
                        rec.device = cycle(&opts, &rec.device, delta);
                        self.connect_recording();
                    }
                    REC_VOL => {
                        rec.vol = (rec.vol as i32 + delta).clamp(0, 0xFF) as u8;
                        rec.capture.set_gain(rec.gain());
                    }
                    REC_ARM if !busy => rec.arm = (rec.arm as i32 + delta).clamp(0, 0xFF) as u8,
                    REC_PLAY if !busy => {
                        rec.play = match rec.play {
                            None => Some(0),
                            Some(v) => Some((v as i32 + delta).clamp(0, 0xFF) as u8),
                        }
                    }
                    _ => {}
                }
            }
            Button::Play => {
                if busy {
                    return self.stop_recording();
                }
                let sample = rec.sample.clone();
                if let Some(r) = &self.rack {
                    r.shared.audition(sample);
                }
            }
            _ => {}
        }
    }

    // --- modal screens ------------------------------------------------------------

    fn modal_press(&mut self, p: Press) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        let samples = matches!(modal, Modal::Samples { .. });
        let (len, cur) = match modal {
            Modal::Plugins { list, cur, .. } => (list.len(), cur),
            Modal::Songs { list, cur, .. } => (list.len(), cur),
            Modal::Samples { list, cur, .. } => (list.len(), cur),
            Modal::Text { .. } => return,
        };
        let page = if p.held.option { 16 } else { 1 };
        let before = *cur;
        match p.button {
            Button::Up => *cur = cur.saturating_sub(page),
            Button::Down => *cur = (*cur + page).min(len.saturating_sub(1)),
            Button::Option if p.held.none() => {
                self.modal = None;
                if samples {
                    self.audition(None);
                }
            }
            Button::Edit if p.held.none() => self.modal_choose(),
            Button::Play if samples => self.audition_cursor(),
            _ => {}
        }
        if matches!(&self.modal, Some(Modal::Samples { cur, .. }) if *cur != before) {
            self.audition_cursor();
        }
    }

    fn modal_choose(&mut self) {
        match self.modal.take() {
            Some(Modal::Plugins {
                instrument,
                list,
                cur,
            }) => self.choose_plugin(instrument, &list[cur]),
            Some(Modal::Samples {
                instrument,
                list,
                cur,
                dir,
            }) => {
                let p = list[cur].clone();
                if p.is_dir() {
                    let back = p.ends_with("..").then(|| dir.canonicalize().unwrap_or(dir));
                    let next = p.canonicalize().unwrap_or(p);
                    self.browse_samples(instrument, next, back);
                } else {
                    self.choose_sample(instrument, &p);
                    self.audition(None);
                }
            }
            Some(Modal::Songs { list, cur, .. }) => {
                let p = list[cur].clone();
                if p.is_dir() {
                    let dir = p.canonicalize().unwrap_or(p);
                    self.browse_songs(dir);
                } else {
                    self.load_song(p);
                }
            }
            other => self.modal = other,
        }
    }

    fn text_key(&mut self, k: KeyEvent) {
        let Some(Modal::Text { value, target, .. }) = self.modal.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Enter => {
                let v = value.clone();
                match *target {
                    TextTarget::SongName => self.song.name = v,
                    TextTarget::InstrumentName(n) => self.song.instruments[n as usize].name = v,
                    TextTarget::RecordingFile => {
                        let v: String = v
                            .trim()
                            .chars()
                            .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
                            .collect();
                        if let Some(rec) = self.recording.as_mut().filter(|_| !v.is_empty()) {
                            rec.name = v;
                        }
                        self.modal = None;
                        return;
                    }
                }
                self.modal = None;
                self.changed();
            }
            KeyCode::Esc => self.modal = None,
            KeyCode::Backspace => {
                value.pop();
            }
            KeyCode::Char(c) if (' '..='~').contains(&c) && value.len() < 12 => {
                value.push(c.to_ascii_uppercase())
            }
            _ => {}
        }
    }

    // --- drawing ---------------------------------------------------------------

    fn draw(&mut self, s: &mut Screen) {
        if self.view == View::Instrument {
            self.refresh_params();
        }
        if self.recording.is_some() {
            self.draw_recording(s);
            self.draw_status(s);
            return;
        }
        match &self.modal {
            Some(Modal::Plugins { list, cur, .. }) => {
                let rows: Vec<(String, String)> = list
                    .iter()
                    .map(|d| (d.name.clone(), d.vendor.clone()))
                    .collect();
                draw_list(s, "CLAP PLUGINS", &rows, *cur);
                self.draw_status(s);
                return;
            }
            Some(Modal::Samples { list, cur, dir, .. }) => {
                let rows: Vec<(String, String)> = list
                    .iter()
                    .map(|p| {
                        let name = if p.ends_with("..") {
                            "/..".to_string()
                        } else {
                            file_name(p)
                        };
                        let dir = if p.is_dir() { "DIR" } else { "" };
                        (name, dir.to_string())
                    })
                    .collect();
                let title = format!("SAMPLES {}", file_name(dir));
                draw_list(s, &title, &rows, *cur);
                s.text(0, 22, "PLAY LISTENS AGAIN", Role::Info);
                self.draw_status(s);
                return;
            }
            Some(Modal::Songs { list, cur, dir }) => {
                let rows: Vec<(String, String)> = list
                    .iter()
                    .map(|p| {
                        let name = if p.ends_with("..") {
                            "/..".to_string()
                        } else {
                            file_name(p)
                        };
                        (
                            name,
                            if p.is_dir() {
                                "DIR".into()
                            } else {
                                String::new()
                            },
                        )
                    })
                    .collect();
                let title = format!("LOAD {}", file_name(dir));
                draw_list(s, &title, &rows, *cur);
                self.draw_status(s);
                return;
            }
            _ => {}
        }
        match self.view {
            View::Song => self.draw_song(s),
            View::Chain => self.draw_chain(s),
            View::Phrase => self.draw_phrase(s),
            View::Instrument => self.draw_instrument(s),
            View::Table => self.draw_table(s),
            View::Groove => self.draw_groove(s),
            View::Project => self.draw_project(s),
        }
        self.draw_panel(s);
        self.draw_status(s);
    }

    fn positions(&self) -> [Option<Pos>; TRACKS] {
        let mut out = [None; TRACKS];
        if let Some(r) = &self.rack {
            for (t, o) in out.iter_mut().enumerate() {
                *o = r.shared.track(t).pos;
            }
        }
        out
    }

    /// Highlight the cursor and selection over a grid drawn at `geom`.
    fn grid_overlay(&self, s: &mut Screen, geom: &[(usize, usize)], top: usize) {
        let c = self.cur();
        if let Some((r0, c0, r1, c1)) = self.selected() {
            for r in r0..=r1 {
                if r < top || r >= top + VISIBLE {
                    continue;
                }
                for &(x, w) in &geom[c0..=c1] {
                    s.highlight(x, GRID_Y + r - top, w, Role::Selection);
                }
            }
        }
        if c.row >= top && c.row < top + VISIBLE {
            let (x, w) = geom[c.col.min(geom.len() - 1)];
            s.highlight(x, GRID_Y + c.row - top, w, Role::Cursor);
        }
    }

    fn draw_song(&mut self, s: &mut Screen) {
        let c = self.cur();
        if c.row < self.song_top {
            self.song_top = c.row;
        } else if c.row >= self.song_top + VISIBLE {
            self.song_top = c.row + 1 - VISIBLE;
        }
        let top = self.song_top;
        s.text(0, 0, "SONG", Role::Title);
        for (t, &(x, _)) in SONG_GEOM.iter().enumerate() {
            s.text(x, 2, &format!("{}", t + 1), Role::Info);
        }
        let pos = self.positions();
        for i in 0..VISIBLE {
            let r = top + i;
            let y = GRID_Y + i;
            s.text(0, y, &format!("{r:02X}"), row_role(r));
            for t in 0..TRACKS {
                let v = self.song.rows[r][t];
                let playing = pos[t].is_some_and(|p| p.row == r);
                if playing {
                    s.text(SONG_GEOM[t].0 - 1, y, ">", Role::PlayMarker);
                }
                let role = if playing {
                    Role::PlayMarker
                } else if v == EMPTY {
                    Role::Empty
                } else {
                    Role::Value
                };
                s.text(SONG_GEOM[t].0, y, &hex_or(v, "--"), role);
            }
        }
        self.grid_overlay(s, &SONG_GEOM, top);
    }

    fn draw_chain(&self, s: &mut Screen) {
        s.text(0, 0, &format!("CHAIN {:02X}", self.chain), Role::Title);
        s.text(3, 2, "PH", Role::Info);
        s.text(6, 2, "TSP", Role::Info);
        let pos = self.positions();
        for i in 0..STEPS {
            let y = GRID_Y + i;
            let st = self.song.chains[self.chain].steps[i];
            s.text(0, y, &format!("{i:X}"), row_role(i));
            if pos
                .iter()
                .flatten()
                .any(|p| p.chain == self.chain && p.chain_step == i)
            {
                s.text(1, y, ">", Role::PlayMarker);
            }
            let empty = st.phrase == EMPTY;
            s.text(
                3,
                y,
                &hex_or(st.phrase, "--"),
                if empty { Role::Empty } else { Role::Value },
            );
            s.text(
                6,
                y,
                &format!("{:02X}", st.transpose),
                if empty { Role::Empty } else { Role::Value },
            );
        }
        self.grid_overlay(s, &CHAIN_GEOM, 0);
    }

    fn draw_phrase(&self, s: &mut Screen) {
        s.text(0, 0, &format!("PHRASE {:02X}", self.phrase), Role::Title);
        for (x, label) in [
            (2, "N"),
            (6, "V"),
            (9, "I"),
            (12, "FX1"),
            (19, "FX2"),
            (26, "FX3"),
        ] {
            s.text(x, 2, label, Role::Info);
        }
        let pos = self.positions();
        let ctx_track = self.phrase_ctx.map(|c| c.track);
        for i in 0..STEPS {
            let y = GRID_Y + i;
            let st = &self.song.phrases[self.phrase].steps[i];
            s.text(0, y, &format!("{i:X}"), row_role(i));
            let here = pos.iter().enumerate().any(|(t, p)| {
                p.is_some_and(|p| p.phrase == self.phrase && p.phrase_step == i)
                    && (ctx_track.is_none() || ctx_track == Some(t))
            });
            if here {
                s.text(1, y, ">", Role::PlayMarker);
            }
            let note_role = if st.note == EMPTY {
                Role::Empty
            } else {
                Role::Value
            };
            s.text(2, y, &note_name(st.note), note_role);
            let vol_role = if st.volume == EMPTY {
                Role::Empty
            } else {
                Role::Value
            };
            s.text(6, y, &hex_or(st.volume, "--"), vol_role);
            let inst_role = if st.instrument == EMPTY {
                Role::Empty
            } else {
                Role::Value
            };
            s.text(9, y, &hex_or(st.instrument, "--"), inst_role);
            for (k, fx) in st.fx.iter().enumerate() {
                let (cx, vx) = (PHRASE_GEOM[3 + 2 * k].0, PHRASE_GEOM[4 + 2 * k].0);
                draw_fx(s, cx, vx, y, fx.command, fx.value);
            }
        }
        self.grid_overlay(s, &PHRASE_GEOM, 0);
    }

    fn draw_table(&self, s: &mut Screen) {
        s.text(0, 0, &format!("TABLE {:02X}", self.table), Role::Title);
        for (x, label) in [(2, "N"), (5, "V"), (8, "FX1"), (15, "FX2"), (22, "FX3")] {
            s.text(x, 2, label, Role::Info);
        }
        for i in 0..STEPS {
            let y = GRID_Y + i;
            let st = &self.song.tables[self.table].steps[i];
            s.text(0, y, &format!("{i:X}"), row_role(i));
            let tsp_role = if st.transpose == 0 {
                Role::Empty
            } else {
                Role::Value
            };
            s.text(2, y, &format!("{:02X}", st.transpose), tsp_role);
            let vol_role = if st.volume == EMPTY {
                Role::Empty
            } else {
                Role::Value
            };
            s.text(5, y, &hex_or(st.volume, "--"), vol_role);
            for (k, fx) in st.fx.iter().enumerate() {
                let (cx, vx) = (TABLE_GEOM[2 + 2 * k].0, TABLE_GEOM[3 + 2 * k].0);
                draw_fx(s, cx, vx, y, fx.command, fx.value);
            }
        }
        self.grid_overlay(s, &TABLE_GEOM, 0);
    }

    fn draw_groove(&self, s: &mut Screen) {
        s.text(0, 0, &format!("GROOVE {:02X}", self.groove), Role::Title);
        let g = &self.song.grooves[self.groove];
        for i in 0..STEPS {
            let y = GRID_Y + i;
            s.text(0, y, &format!("{i:X}"), row_role(i));
            let v = g.steps[i];
            let role = if v == EMPTY { Role::Empty } else { Role::Value };
            s.text(2, y, &hex_or(v, "--"), role);
        }
        let ticks: u32 = g.ticks().iter().map(|&t| t as u32).sum();
        let per_phrase = if g.ticks().is_empty() {
            96
        } else {
            (0..STEPS)
                .map(|i| g.ticks()[i % g.ticks().len()] as u32)
                .sum()
        };
        s.text(
            0,
            20,
            &format!("{ticks} TICKS, {per_phrase} PER PHRASE"),
            Role::Info,
        );
        self.grid_overlay(s, &GROOVE_GEOM, 0);
    }

    fn draw_instrument(&mut self, s: &mut Screen) {
        let n = self.instrument;
        let inst = self.song.instruments[n as usize].clone();
        s.text(0, 0, &format!("INST. {n:02X}"), Role::Title);
        let kind = match inst.kind {
            InstrumentKind::None => "NONE".to_string(),
            k => k.name().to_uppercase(),
        };
        let plugin = match self.slot(n) {
            Some(slot) => slot.plugin.descriptor.name.clone(),
            None => match self.missing.iter().find(|m| m.0 == n) {
                Some((_, why)) => format!("! {why}"),
                None => "--".into(),
            },
        };
        let own = self.map.numbered().any(|x| x == n);
        let plugin = if self.slot(n).is_some() && !own {
            format!("{plugin} (DEF)")
        } else {
            plugin
        };
        let name = if inst.name.is_empty() {
            "--------".into()
        } else {
            inst.name.clone()
        };
        let sampler = self.is_sampler(n);
        let (source_label, plugin) = if sampler {
            let file = std::path::Path::new(&inst.sample_path)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned());
            let shown = match (file, self.sample_problems.iter().find(|m| m.0 == n)) {
                (_, Some((_, why))) => format!("! {why}"),
                (Some(f), None) => f,
                (None, None) => "--".into(),
            };
            ("SAMPLE", shown.to_uppercase())
        } else {
            ("PLUGIN", plugin)
        };
        let fields = [
            ("TYPE", kind),
            ("NAME", name),
            (source_label, plugin),
            ("TRANSP.", if inst.transpose { "ON" } else { "OFF" }.into()),
            ("TBL. TIC", format!("{:02X}", inst.table_tick)),
        ];
        let c = self.cur();
        for (i, (label, value)) in fields.iter().enumerate() {
            let y = 2 + i;
            s.text(0, y, label, Role::Default);
            // A sampler's SAMPLE row ends in REC., as on the M8.
            let rec = sampler && i == 2;
            let room = if rec { PANEL_X - 16 } else { PANEL_X - 10 };
            let v: String = value.chars().take(room).collect();
            s.text(9, y, &v, Role::Value);
            if rec {
                s.text(PANEL_X - 6, y, "REC.", Role::Value);
            }
            if c.row == i {
                if rec && self.sample_col == 1 {
                    s.highlight(PANEL_X - 6, y, 4, Role::Cursor);
                } else {
                    s.highlight(9, y, v.chars().count().max(2), Role::Cursor);
                }
            }
        }
        if c.row == 2 && sampler && self.sample_col == 1 {
            s.text(0, 7, "EDIT: RECORD A SAMPLE", Role::Info);
        } else if c.row == 2 {
            s.text(0, 7, "EDIT: LIST, EDIT+L/R: NEXT", Role::Info);
        }
        if sampler {
            return self.draw_sampler(s, c.row);
        }
        if self.params.is_empty() {
            let msg = if self.slot(n).is_some() {
                "NO PARAMETERS"
            } else {
                "EDIT ON PLUGIN TO CHOOSE ONE"
            };
            s.text(0, 8, msg, Role::Info);
            return;
        }
        s.text(
            0,
            8,
            &format!("PARAMETERS ({})", self.params.len()),
            Role::Info,
        );
        let visible = 11;
        if c.row >= INST_FIELDS {
            let i = c.row - INST_FIELDS;
            if i < self.inst_top {
                self.inst_top = i;
            } else if i >= self.inst_top + visible {
                self.inst_top = i + 1 - visible;
            }
        }
        let params = self.params.clone();
        for (k, p) in params.iter().enumerate().skip(self.inst_top).take(visible) {
            let y = 9 + k - self.inst_top;
            let label: String = p.name.to_uppercase().chars().take(16).collect();
            s.text(0, y, &label, Role::Default);
            let v = self.param_value(p);
            let text = self
                .slot(n)
                .and_then(|sl| sl.plugin.value_to_text(p.id, v))
                .unwrap_or_else(|| format!("{v:.2}"));
            let text: String = text.chars().take(PANEL_X - 18).collect();
            let role = if p.is_readonly() {
                Role::Info
            } else {
                Role::Value
            };
            s.text(17, y, &text, role);
            if c.row == INST_FIELDS + k {
                s.highlight(17, y, text.chars().count().max(2), Role::Cursor);
            }
        }
    }

    /// The sample recording screen.
    fn draw_recording(&self, s: &mut Screen) {
        let Some(rec) = self.recording.as_deref() else {
            return;
        };
        let busy = rec.capture.is_recording();
        s.text(0, 0, "SAMPLE RECORD", Role::Title);
        s.text(31, 0, &format!("INST. {:02X}", rec.instrument), Role::Info);
        let start = match (busy, rec.capture.triggered()) {
            (true, false) => "ARMED",
            (true, true) => "STOP",
            _ => "START",
        };
        let device = if !rec.source.is_input() {
            "--".to_string()
        } else if let Some(i) = &rec.input {
            i.name.clone()
        } else {
            rec.device.clone().unwrap_or_else(|| "DEFAULT".into())
        };
        let values = [
            start.to_string(),
            rec.source.name().to_string(),
            device,
            format!("{:02X}", rec.vol),
            if rec.arm == 0 {
                "OFF".into()
            } else {
                format!("{:02X}", rec.arm)
            },
            rec.play.map_or("--".into(), |r| format!("{r:02X}")),
            rec.name.clone(),
            "SAVE".to_string(),
        ];
        for (i, (label, value)) in REC_ROWS.iter().zip(&values).enumerate() {
            let y = 2 + i + (i >= REC_NAME) as usize;
            s.text(0, y, label, Role::Default);
            let v: String = value.to_uppercase().chars().take(30).collect();
            let role = match i {
                REC_START if busy => Role::MeterPeak,
                REC_SAVE if rec.take.is_none() => Role::Info,
                _ => Role::Value,
            };
            s.text(9, y, &v, role);
            if rec.row == i {
                s.highlight(9, y, v.chars().count().max(2), Role::Cursor);
            }
        }

        // Level meter, with the ARM level marked.
        let width = 30;
        let y = 13;
        s.text(0, y, "LEVEL", Role::Default);
        let db = 20.0 * rec.meter.max(1e-6).log10();
        let col = |level: f32| -> usize {
            let db = 20.0 * level.max(1e-6).log10();
            (((db + 48.0) / 48.0).clamp(0.0, 1.0) * width as f32).round() as usize
        };
        let filled = col(rec.meter);
        for i in 0..width {
            let role = if i >= filled {
                Role::Empty
            } else if i >= width - 3 {
                Role::MeterPeak
            } else if i >= width * 3 / 4 {
                Role::MeterMid
            } else {
                Role::MeterLow
            };
            s.text(9 + i, y, "\u{2588}", role);
        }
        if rec.arm > 0 {
            let x = col(rec.arm as f32 / 255.0).min(width - 1);
            s.text(9 + x, y + 1, "^", Role::Info);
        }
        if rec.meter > 1e-6 {
            s.text(9, y + 1, &format!("{db:.0} DB"), Role::Info);
        }

        // The take so far.
        if let Some(t) = &rec.take {
            const BARS: [char; 8] = [
                '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
                '\u{2588}',
            ];
            let width = 39;
            let peaks = t.peaks(width);
            for (x, pk) in peaks.iter().enumerate() {
                let level = (pk.min(1.0) * 7.0).round() as usize;
                let ch = if *pk < 1e-4 { ' ' } else { BARS[level.min(7)] };
                s.text(x, 16, &ch.to_string(), Role::Value);
            }
            let secs = t.seconds();
            s.text(
                0,
                17,
                &format!(
                    "{:02}:{:05.2}  {}HZ {}",
                    (secs / 60.0) as u32,
                    secs % 60.0,
                    t.sample_rate,
                    if t.mono { "MONO" } else { "STEREO" }
                ),
                Role::Info,
            );
        }

        if let Some(p) = &rec.problem {
            let p: String = format!("! {p}").to_uppercase().chars().take(40).collect();
            s.text(0, 19, &p, Role::MeterPeak);
        }
        let hint = if busy {
            "EDIT OR OPTION STOPS"
        } else if rec.take.is_some() {
            "PLAY LISTENS, OPTION GOES BACK"
        } else {
            "EDIT ON START RECORDS, OPTION GOES BACK"
        };
        s.text(0, 21, hint, Role::Info);
        let dir = sample_path_for(
            &self.sample_dirs().0,
            &recorder::recordings_dir(&self.sample_dirs().0),
        );
        let to: String = format!("TO {dir}/").chars().take(40).collect();
        s.text(0, 22, &to, Role::Info);
    }

    /// The sampler's waveform and settings, below the common rows.
    fn draw_sampler(&mut self, s: &mut Screen, cur_row: usize) {
        let n = self.instrument;
        let inst = self.song.instruments[n as usize].clone();
        match self.samplers.samples[n as usize].clone() {
            Some(sample) => draw_wave(s, 8, PANEL_X - 1, &sample, &inst),
            None if cur_row != 2 => s.text(0, 8, "EDIT ON SAMPLE TO CHOOSE ONE", Role::Info),
            None => {}
        }
        let visible = 12;
        if cur_row >= INST_FIELDS {
            let i = cur_row - INST_FIELDS;
            if i < self.inst_top {
                self.inst_top = i;
            } else if i >= self.inst_top + visible {
                self.inst_top = i + 1 - visible;
            }
        }
        for (k, &(label, i, show)) in SAMPLER_ROWS
            .iter()
            .enumerate()
            .skip(self.inst_top)
            .take(visible)
        {
            let y = 11 + k - self.inst_top;
            s.text(0, y, label, Role::Default);
            let v = inst.sampler_param(i);
            let text = match show {
                Show::Hex => format!("{v:02X}"),
                Show::Slice if v == 0 => "OFF".into(),
                Show::Slice => format!("{v:02X}"),
                Show::Names(names) => names
                    .get(v as usize)
                    .map_or(format!("{v:02X}"), |s| s.to_string()),
            };
            s.text(11, y, &text, Role::Value);
            if cur_row == INST_FIELDS + k {
                s.highlight(11, y, text.chars().count().max(2), Role::Cursor);
            }
        }
    }

    fn draw_project(&self, s: &mut Screen) {
        s.text(0, 0, "PROJECT", Role::Title);
        let c = self.cur();
        let device = self
            .audio
            .device
            .clone()
            .unwrap_or_else(|| "DEFAULT".into());
        let or_default = |v: Option<u32>| v.map_or("DEFAULT".to_string(), |v| v.to_string());
        let tempo = if self.song.tempo.fract() == 0.0 {
            format!("{:.0}", self.song.tempo)
        } else {
            format!("{:.2}", self.song.tempo)
        };
        let values = [
            tempo,
            format!("{:02X}", self.song.transpose),
            if self.song.name.is_empty() {
                "--------".into()
            } else {
                self.song.name.clone()
            },
            String::new(),
            format!("{} S", self.render_seconds),
            device,
            or_default(self.audio.sample_rate),
            or_default(self.audio.buffer_size),
            "RESTART AUDIO".into(),
        ];
        s.text(0, 9, "AUDIO OUTPUT", Role::Info);
        for (i, ((label, y), v)) in PROJECT_FIELDS.iter().zip(values.iter()).enumerate() {
            s.text(0, *y, label, Role::Default);
            if i == 3 {
                for (k, a) in ["SAVE", "LOAD", "NEW"].iter().enumerate() {
                    let x = 11 + k * 6;
                    s.text(x, *y, a, Role::Value);
                    if c.row == 3 && self.project_col == k {
                        s.highlight(x, *y, a.len(), Role::Cursor);
                    }
                }
                continue;
            }
            let v: String = v.chars().take(PANEL_X - 12).collect();
            s.text(11, *y, &v, Role::Value);
            if c.row == i {
                s.highlight(11, *y, v.chars().count().max(2), Role::Cursor);
            }
        }
        let file = self
            .path
            .as_ref()
            .map_or("NOT SAVED YET".into(), |p| file_name(p));
        let mark = if self.unsaved { "*" } else { "" };
        s.text(0, 16, &format!("FILE {file}{mark}"), Role::Info);
        if let Some(r) = &self.rack {
            s.text(
                0,
                17,
                &format!("{} HZ, {}", r.sample_rate, r.output_name),
                Role::Info,
            );
        }
        if let Some(p) = &self.audio_problem {
            s.text(0, 18, &format!("SOUND CARD FAILED: {p}"), Role::Info);
        }
    }

    /// Meters, tempo, per-track notes and the mini-map on the right.
    fn draw_panel(&self, s: &mut Screen) {
        let x = PANEL_X;
        let db = 20.0 * self.meter.max(1e-6).log10();
        let width = 7;
        let filled = (((db + 48.0) / 48.0).clamp(0.0, 1.0) * width as f32).round() as usize;
        for i in 0..width {
            let role = if i >= filled {
                Role::Empty
            } else if i >= 6 {
                Role::MeterPeak
            } else if i >= 4 {
                Role::MeterMid
            } else {
                Role::MeterLow
            };
            s.text(x + i, 0, "\u{2588}", role);
        }
        let tempo = self
            .rack
            .as_ref()
            .map_or(self.song.tempo, |r| r.shared.tempo());
        s.text(x, 1, &format!("{tempo:>7.1}"), Role::Value);
        for t in 0..TRACKS {
            let y = 3 + t;
            let bit = 1 << t;
            let silenced = self.muted & bit != 0 || (self.soloed != 0 && self.soloed & bit == 0);
            s.text(
                x,
                y,
                &format!("{}", t + 1),
                if silenced { Role::Empty } else { Role::Info },
            );
            let note = self.rack.as_ref().and_then(|r| r.shared.track(t).note);
            match note {
                Some((n, _)) if !silenced => s.text(x + 2, y, &note_name(n), Role::Value),
                _ => s.text(x + 2, y, "---", Role::Empty),
            }
            if self.soloed & bit != 0 {
                s.text(x + 6, y, "S", Role::Title);
            } else if self.muted & bit != 0 {
                s.text(x + 6, y, "M", Role::Info);
            }
        }
        for &(ch, mx, my, view) in MAP {
            let role = match view {
                Some(v) if v == self.view => Role::Title,
                Some(_) => Role::Default,
                None => Role::Empty,
            };
            s.text(x + mx, 19 + my, &ch.to_string(), role);
        }
    }

    fn draw_status(&self, s: &mut Screen) {
        if let Some(Modal::Text { label, value, .. }) = &self.modal {
            s.text(0, ROWS - 1, &format!("{label}: {value}_"), Role::Title);
            return;
        }
        if let Some((msg, at)) = &self.status {
            if at.elapsed() < Duration::from_secs(3) {
                s.text(0, ROWS - 1, &msg.to_uppercase(), Role::Info);
            }
        }
    }
}

fn cycle<T: Clone + PartialEq>(opts: &[T], cur: &T, delta: i32) -> T {
    let i = opts.iter().position(|o| o == cur).unwrap_or(0) as i32;
    opts[(i + delta.signum()).rem_euclid(opts.len() as i32) as usize].clone()
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Chain => "CHAIN",
        Kind::Phrase => "PHRASE",
        _ => "INSTRUMENT",
    }
}

fn row_role(r: usize) -> Role {
    if r % 4 == 0 {
        Role::Default
    } else {
        Role::Info
    }
}

fn hex_or(v: u8, empty: &str) -> String {
    if v == EMPTY {
        empty.to_string()
    } else {
        format!("{v:02X}")
    }
}

fn draw_fx(s: &mut Screen, cx: usize, vx: usize, y: usize, cmd: u8, val: u8) {
    if cmd == EMPTY {
        s.text(cx, y, "---", Role::Empty);
        s.text(vx, y, "00", Role::Empty);
    } else {
        s.text(cx, y, &fx_name(cmd), Role::Value);
        s.text(vx, y, &format!("{val:02X}"), Role::Value);
    }
}

fn draw_list(s: &mut Screen, title: &str, rows: &[(String, String)], cur: usize) {
    s.text(0, 0, title, Role::Title);
    let visible = 18;
    let top = cur.saturating_sub(visible - 1);
    for (i, (name, detail)) in rows.iter().enumerate().skip(top).take(visible) {
        let y = 2 + i - top;
        let name: String = name.to_uppercase().chars().take(26).collect();
        s.text(0, y, &name, Role::Value);
        let detail: String = detail.to_uppercase().chars().take(12).collect();
        s.text(27, y, &detail, Role::Info);
        if i == cur {
            s.highlight(0, y, name.chars().count().max(2), Role::Cursor);
        }
    }
    s.text(0, 21, "EDIT CHOOSES, OPTION GOES BACK", Role::Info);
}

/// A folder's subfolders, then its files that pass `keep`, sorted by name.
fn dir_entries(dir: &std::path::Path, keep: impl Fn(&std::path::Path) -> bool) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for p in rd.flatten().map(|e| e.path()) {
        let hidden = p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        if hidden {
            continue;
        }
        if p.is_dir() {
            dirs.push(p);
        } else if keep(&p) {
            files.push(p);
        }
    }
    let key = |p: &PathBuf| file_name(p).to_lowercase();
    dirs.sort_by_key(key);
    files.sort_by_key(key);
    dirs.extend(files);
    dirs
}

fn is_wav(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"))
}

/// A sample's waveform on row `y`, `width` wide, with the part that plays
/// bright and, below it, the start (S), loop (L) and end (E) points or the
/// slices.
fn draw_wave(
    s: &mut Screen,
    y: usize,
    width: usize,
    sample: &sampler::Sample,
    inst: &crate::m8::format::Instrument,
) {
    const BARS: [char; 8] = [
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    let peaks = sample.peaks(width);
    let top = peaks.iter().fold(0.0f32, |m, &p| m.max(p)).max(1e-6);
    let len = sample.frames().max(1) as f64;
    let col = |frame: f64| ((frame / len * width as f64).max(0.0) as usize).min(width - 1);
    let p = |i: usize| inst.sampler_param(i) as f64 / 255.0;
    let slice = inst.sampler_param(3 + S_SLICE);
    let (from, to) = if slice == 0 {
        let start = len * p(3 + S_START);
        let end = start + (len - start) * p(3 + S_LENGTH);
        let lp = start + (end - start) * p(3 + S_LOOP);
        if inst.sampler_param(3 + S_PLAY) >= 2 {
            s.text(col(lp), y + 1, "L", Role::Info);
        }
        s.text(col(end - 1.0), y + 1, "E", Role::Info);
        s.text(col(start), y + 1, "S", Role::Info);
        (col(start), col(end - 1.0))
    } else {
        let count = if sample.cues.is_empty() {
            slice as usize
        } else {
            sample.cues.len() + 1
        };
        for k in 0..count.min(width) {
            let (a, _) = sample.region(slice, k as i16);
            s.text(col(a as f64), y + 1, "|", Role::Info);
        }
        (0, width - 1)
    };
    for (x, pk) in peaks.iter().enumerate() {
        let level = ((pk / top) * 7.0).round() as usize;
        let role = if (from..=to).contains(&x) {
            Role::Value
        } else {
            Role::Info
        };
        let ch = if *pk == 0.0 { ' ' } else { BARS[level.min(7)] };
        s.text(x, y, &ch.to_string(), role);
    }
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut a = App {
            song: Song::new_empty(),
            path: None,
            map: InstrumentMap::default(),
            slots: Vec::new(),
            missing: Vec::new(),
            bank: SampleBank::default(),
            samplers: Arc::new(SamplerSet::default()),
            sample_problems: Vec::new(),
            rack: None,
            use_device: false,
            audio: AudioSettings::default(),
            note_offset: 24,
            audio_problem: None,
            view: View::Song,
            cursors: HashMap::new(),
            song_top: 0,
            chain: 0,
            chain_ctx: None,
            phrase: 0,
            phrase_ctx: None,
            instrument: 0,
            inst_top: 0,
            table: 0,
            groove: 0,
            project_col: 0,
            sample_col: 0,
            recording: None,
            render_seconds: 60,
            devices: Vec::new(),
            last: HashMap::new(),
            last_edit: None,
            selection: None,
            clip: None,
            modal: None,
            confirm: None,
            status: None,
            preview: None,
            param_hold: HashMap::new(),
            params: Vec::new(),
            params_for: None,
            installed: None,
            muted: 0,
            soloed: 0,
            meter: 0.0,
            dirty: false,
            unsaved: false,
            keys: Keys::default(),
            lights: Lights::default(),
            quit: false,
        };
        a.dirty = false;
        a
    }

    fn tap(a: &mut App, b: Button) {
        a.press(Press::new(b));
    }

    fn with(a: &mut App, b: Button, f: impl FnOnce(&mut super::super::input::Held)) {
        a.press(Press::with(b, f));
    }

    #[test]
    fn write_a_song_from_scratch() {
        let mut a = app();
        // Song: EDIT inserts chain 00, double tap makes a new one.
        tap(&mut a, Button::Edit);
        assert_eq!(a.song.rows[0][0], 0);
        tap(&mut a, Button::Right);
        tap(&mut a, Button::Edit);
        tap(&mut a, Button::Edit);
        assert_eq!(
            a.song.rows[0][1], 1,
            "chain 00 is in use, so the new one is 01"
        );
        // A single EDIT repeats the last chain.
        a.last_edit = None;
        a.set_cur(Cursor { row: 1, col: 0 });
        tap(&mut a, Button::Edit);
        assert_eq!(a.song.rows[1][0], 1);

        // Into chain 00: a phrase, transposed up 5.
        a.set_cur(Cursor { row: 0, col: 0 });
        with(&mut a, Button::Right, |h| h.shift = true);
        assert_eq!(a.view, View::Chain);
        tap(&mut a, Button::Edit);
        assert_eq!(a.song.chains[0].steps[0].phrase, 0);
        tap(&mut a, Button::Right);
        for _ in 0..5 {
            with(&mut a, Button::Right, |h| h.edit = true);
        }
        assert_eq!(a.song.chains[0].steps[0].transpose, 5);

        // Into the phrase: a note with its instrument, then an octave up.
        tap(&mut a, Button::Left);
        with(&mut a, Button::Right, |h| h.shift = true);
        assert_eq!(a.view, View::Phrase);
        tap(&mut a, Button::Edit);
        let st = a.song.phrases[0].steps[0];
        assert_eq!((st.note, st.instrument), (36, 0));
        with(&mut a, Button::Up, |h| h.edit = true);
        assert_eq!(a.song.phrases[0].steps[0].note, 48);
        // The next note reuses the last value.
        tap(&mut a, Button::Down);
        tap(&mut a, Button::Edit);
        assert_eq!(a.song.phrases[0].steps[1].note, 48);
        // Delete clears it, and deleting again writes OFF.
        with(&mut a, Button::Option, |h| h.edit = true);
        assert_eq!(a.song.phrases[0].steps[1].note, EMPTY);
        with(&mut a, Button::Option, |h| h.edit = true);
        assert_eq!(a.song.phrases[0].steps[1].note, 0x80);

        // An effect: EDIT on FX1 inserts ARP 00, EDIT+RIGHT moves to CHA.
        a.set_cur(Cursor { row: 0, col: 3 });
        tap(&mut a, Button::Edit);
        assert_eq!(a.song.phrases[0].steps[0].fx[0].command, 0);
        with(&mut a, Button::Right, |h| h.edit = true);
        assert_eq!(a.song.phrases[0].steps[0].fx[0].command, 1);

        // Back to the song with SHIFT+LEFT twice.
        with(&mut a, Button::Left, |h| h.shift = true);
        with(&mut a, Button::Left, |h| h.shift = true);
        assert_eq!(a.view, View::Song);
        assert!(a.unsaved);
    }

    #[test]
    fn copy_and_paste_in_the_song() {
        let mut a = app();
        a.song.rows[0][0] = 3;
        a.song.rows[1][0] = 4;
        with(&mut a, Button::Option, |h| h.shift = true);
        tap(&mut a, Button::Down);
        tap(&mut a, Button::Option);
        assert!(a.selection.is_none());
        a.set_cur(Cursor { row: 5, col: 2 });
        with(&mut a, Button::Edit, |h| h.shift = true);
        assert_eq!((a.song.rows[5][2], a.song.rows[6][2]), (3, 4));
        // Cut empties the source.
        a.set_cur(Cursor { row: 5, col: 2 });
        with(&mut a, Button::Option, |h| h.shift = true);
        with(&mut a, Button::Option, |h| h.edit = true);
        assert_eq!(a.song.rows[5][2], EMPTY);
    }

    #[test]
    fn clone_a_phrase() {
        let mut a = app();
        a.song.chains[0].steps[0].phrase = 0;
        a.song.phrases[0].steps[0].note = 40;
        a.song.rows[0][0] = 0;
        a.view = View::Chain;
        with(&mut a, Button::Option, |h| h.shift = true);
        with(&mut a, Button::Edit, |h| h.shift = true);
        let p = a.song.chains[0].steps[0].phrase;
        assert_ne!(p, 0);
        assert_eq!(a.song.phrases[p as usize].steps[0].note, 40);
    }

    #[test]
    fn sampler_instrument() {
        let dir = std::env::temp_dir().join(format!("lextrack-sampler-{}", std::process::id()));
        let samples = dir.join("Samples");
        std::fs::create_dir_all(&samples).unwrap();
        std::fs::create_dir_all(dir.join("Songs")).unwrap();
        let tone: Vec<f32> = (0..400).map(|i| (i as f32 * 0.1).sin()).collect();
        for name in ["a.wav", "b.wav"] {
            let bytes = crate::m8::sampler::tests::wav(std::slice::from_ref(&tone), 44_100, &[]);
            std::fs::write(samples.join(name), bytes).unwrap();
        }
        let mut a = app();
        a.path = Some(dir.join("Songs").join("X.m8s"));
        a.view = View::Instrument;
        a.choose_sample(0, &samples.join("a.wav"));
        let inst = &a.song.instruments[0];
        assert_eq!(inst.kind, InstrumentKind::Sampler);
        assert_eq!(
            (inst.name.as_str(), inst.sample_path.as_str()),
            ("A", "/Samples/a.wav")
        );
        a.sync_samplers();
        assert!(a.samplers.builtin[0] && a.samplers.samples[0].is_some());

        // EDIT+RIGHT on SAMPLE: the next file, and the name follows it.
        a.set_cur(Cursor { row: 2, col: 0 });
        with(&mut a, Button::Right, |h| h.edit = true);
        assert_eq!(a.song.instruments[0].sample_path, "/Samples/b.wav");
        assert_eq!(a.song.instruments[0].name, "B");

        // START is the sixth sampler row.
        a.set_cur(Cursor {
            row: INST_FIELDS + 5,
            col: 0,
        });
        with(&mut a, Button::Up, |h| h.edit = true);
        assert_eq!(a.song.instruments[0].sampler.0[S_START], 0x10);
        with(&mut a, Button::Option, |h| h.edit = true);
        assert_eq!(a.song.instruments[0].sampler.0[S_START], 0);

        a.sync_samplers();
        let mut s = Screen::new();
        a.draw(&mut s);
        assert!(s.row(4).starts_with("SAMPLE   B.WAV"), "{}", s.row(4));
        assert!(s.row(9).starts_with('S'), "{}", s.row(9));
        assert!(s.row(11).starts_with("VOLUME     00"), "{}", s.row(11));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_a_sample() {
        let dir = std::env::temp_dir().join(format!("lextrack-record-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Songs")).unwrap();
        let mut a = app();
        a.path = Some(dir.join("Songs").join("X.m8s"));
        a.song.directory = "/Songs/".into();
        a.view = View::Instrument;
        a.song.instruments[0].set_kind(InstrumentKind::Sampler);

        // RIGHT on SAMPLE moves to REC., and EDIT there opens the recorder.
        a.set_cur(Cursor { row: 2, col: 0 });
        tap(&mut a, Button::Right);
        let mut s = Screen::new();
        a.draw(&mut s);
        assert!(s.row(4).contains("REC."), "{}", s.row(4));
        tap(&mut a, Button::Edit);
        assert!(a.recording.is_some());
        assert_eq!(a.recording.as_ref().unwrap().name, "REC000");

        // SRC: EDIT+LEFT from L&R wraps round to MASTER.
        tap(&mut a, Button::Down);
        with(&mut a, Button::Left, |h| h.edit = true);
        assert_eq!(a.recording.as_ref().unwrap().source, Source::Master);

        // Feed the capture as the rack would, then stop with EDIT on START.
        {
            let rec = a.recording.as_mut().unwrap();
            rec.take = Some(Take::new(48_000, false));
            rec.capture.start(0.0, 64);
            rec.capture.push(64, |i| ((i as f32 / 64.0), -0.5));
        }
        a.tick_recording();
        a.recording.as_mut().unwrap().row = REC_START;
        tap(&mut a, Button::Edit);
        let rec = a.recording.as_ref().unwrap();
        assert!(!rec.capture.is_recording());
        assert_eq!(rec.take.as_ref().unwrap().frames(), 64);
        assert!(rec.sample.is_some());
        let mut s = Screen::new();
        a.draw(&mut s);
        assert!(s.row(0).starts_with("SAMPLE RECORD"), "{}", s.row(0));
        assert!(s.row(17).contains("48000HZ STEREO"), "{}", s.row(17));

        // SAVE writes the file and loads it into the instrument.
        a.recording.as_mut().unwrap().row = REC_SAVE;
        tap(&mut a, Button::Edit);
        assert!(a.recording.is_none());
        let file = dir.join("Samples/Recordings/REC000.wav");
        assert!(file.is_file());
        let inst = &a.song.instruments[0];
        assert_eq!(inst.sample_path, "/Samples/Recordings/REC000.wav");
        assert_eq!(inst.name, "REC000");
        a.sync_samplers();
        assert_eq!(a.samplers.samples[0].as_ref().unwrap().frames(), 64);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsaved_take_asks_before_closing() {
        let mut a = app();
        a.view = View::Instrument;
        a.open_recorder(0);
        a.recording.as_mut().unwrap().take = Some(Take::new(48_000, true));
        tap(&mut a, Button::Option);
        assert!(a.recording.is_some());
        tap(&mut a, Button::Option);
        assert!(a.recording.is_none());
    }

    #[test]
    fn instrument_effects_can_be_picked() {
        let mut a = app();
        a.view = View::Phrase;
        a.set_cur(Cursor { row: 0, col: 3 });
        tap(&mut a, Button::Edit);
        with(&mut a, Button::Up, |h| h.edit = true);
        assert_eq!(a.song.phrases[0].steps[0].fx[0].command, 0x10);
        with(&mut a, Button::Up, |h| h.edit = true);
        // Past the sequencer effects come the sampler's, from 0x80.
        assert_eq!(a.song.phrases[0].steps[0].fx[0].command, 0x89);
        assert_eq!(fx_name(0x89), "CUT");
    }

    #[test]
    fn views_draw() {
        let mut a = app();
        a.song.rows[0][0] = 0;
        a.song.chains[0].steps[0].phrase = 0;
        a.song.phrases[0].steps[0] = crate::m8::format::PhraseStep {
            note: 36,
            volume: 0x40,
            instrument: 1,
            fx: [crate::m8::format::Fx {
                command: 5,
                value: 2,
            }; 3],
        };
        for v in [
            View::Song,
            View::Chain,
            View::Phrase,
            View::Instrument,
            View::Table,
            View::Groove,
            View::Project,
        ] {
            a.view = v;
            let mut s = Screen::new();
            a.draw(&mut s);
        }
        a.view = View::Phrase;
        let mut s = Screen::new();
        a.draw(&mut s);
        assert_eq!(&s.row(0)[..9], "PHRASE 00");
        assert_eq!(&s.row(3)[..18], "0 C-4 40 01 KIL 02");
    }
}
