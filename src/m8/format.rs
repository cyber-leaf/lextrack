//! Reading Dirtywave M8 song files (`.m8s`).
//!
//! The layout follows m8-js by Jeremy Whitlock
//! (https://github.com/whitlockjc/m8-js, Apache License 2.0), mainly its
//! `loadSong`, `loadInstrument`, `loadTable` and `M8FileReader`. This is a
//! Rust port of those readers, trimmed to what playback needs: synth
//! parameters, mixer and effect settings are skipped because CLAP plugins
//! take the place of the M8's built-in instruments. See NOTICE.

use std::fmt;
use std::path::Path;

use anyhow::{bail, Context, Result};

pub const EMPTY: u8 = 0xFF;
pub const TRACKS: usize = 8;
pub const SONG_ROWS: usize = 256;
pub const CHAINS: usize = 255;
pub const PHRASES: usize = 255;
pub const INSTRUMENTS: usize = 128;
pub const TABLES: usize = 256;
pub const GROOVES: usize = 32;
pub const STEPS: usize = 16;

/// Bytes per instrument inside a song: parameters padded to 0x57, then a
/// 128-byte sample path.
const INSTRUMENT_SIZE: usize = 0x57 + 128;

const FILE_SONG: u8 = 0x00;

// Where things live in a song file (firmware 2.x layout).
const OFF_TRANSPOSE: usize = 0x8E;
const OFF_TEMPO: usize = 0x8F;
const OFF_NAME: usize = 0x94;
const OFF_GROOVES: usize = 0xEE;
const OFF_ROWS: usize = 0x2EE;
const OFF_PHRASES: usize = 0xAEE;
const OFF_CHAINS: usize = 0x9A5E;
const OFF_TABLES: usize = 0xBA3E;
const OFF_INSTRUMENTS: usize = 0x13A3E;

/// An empty song (m8-js's DEFAULT.m8s), the starting point for new songs.
const EMPTY_SONG: &[u8] = include_bytes!("../../tests/fixtures/DEFAULT.m8s");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fx {
    pub command: u8,
    pub value: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhraseStep {
    pub note: u8,
    pub volume: u8,
    pub instrument: u8,
    pub fx: [Fx; 3],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phrase {
    pub steps: [PhraseStep; STEPS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainStep {
    pub phrase: u8,
    /// Signed semitones, stored as a byte.
    pub transpose: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    pub steps: [ChainStep; STEPS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableStep {
    pub transpose: u8,
    pub volume: u8,
    pub fx: [Fx; 3],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub steps: [TableStep; STEPS],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Groove {
    /// Ticks per phrase step; the groove ends at the first 0xFF.
    pub steps: [u8; STEPS],
}

impl Groove {
    /// The groove's step lengths up to its end marker.
    pub fn ticks(&self) -> &[u8] {
        let n = self.steps.iter().position(|&s| s == EMPTY).unwrap_or(STEPS);
        &self.steps[..n]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstrumentKind {
    Wavsynth,
    Macrosynth,
    Sampler,
    MidiOut,
    FmSynth,
    /// A kind added by firmware newer than m8-js documents.
    Other(u8),
    None,
}

impl InstrumentKind {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x00 => Self::Wavsynth,
            0x01 => Self::Macrosynth,
            0x02 => Self::Sampler,
            0x03 => Self::MidiOut,
            0x04 => Self::FmSynth,
            0xFF => Self::None,
            n => Self::Other(n),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::Wavsynth => 0x00,
            Self::Macrosynth => 0x01,
            Self::Sampler => 0x02,
            Self::MidiOut => 0x03,
            Self::FmSynth => 0x04,
            Self::Other(n) => n,
            Self::None => 0xFF,
        }
    }

    /// The name used in instrument map files.
    pub fn name(&self) -> String {
        match self {
            Self::Wavsynth => "wavsynth".into(),
            Self::Macrosynth => "macrosynth".into(),
            Self::Sampler => "sampler".into(),
            Self::MidiOut => "midiout".into(),
            Self::FmSynth => "fmsynth".into(),
            Self::Other(n) => format!("kind{n:02x}"),
            Self::None => "none".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instrument {
    pub kind: InstrumentKind,
    pub name: String,
    /// Whether chain transposition applies to this instrument.
    pub transpose: bool,
    pub table_tick: u8,
    pub volume: u8,
    pub pitch: u8,
    pub fine_tune: u8,
    /// Sampler only.
    pub sample_path: String,
    /// Sampler only: the parameters after FINE TUNE.
    pub sampler: SamplerParams,
}

/// Number of sampler parameter bytes after FINE TUNE: play mode, slice,
/// start, loop start, length, degrade, filter (3), amp (2), mixer (5), two
/// envelopes and two LFOs (6 each).
pub const SAMPLER_PARAMS: usize = 40;

/// A sampler instrument's parameters, as the bytes the file stores. The
/// `S_*` constants index them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SamplerParams(pub [u8; SAMPLER_PARAMS]);

pub const S_PLAY: usize = 0;
pub const S_SLICE: usize = 1;
pub const S_START: usize = 2;
pub const S_LOOP: usize = 3;
pub const S_LENGTH: usize = 4;
pub const S_DEGRADE: usize = 5;
pub const S_FILTER: usize = 6;
pub const S_CUTOFF: usize = 7;
pub const S_RES: usize = 8;
pub const S_AMP: usize = 9;
pub const S_LIMIT: usize = 10;
pub const S_PAN: usize = 11;
pub const S_DRY: usize = 12;
pub const S_CHORUS: usize = 13;
pub const S_DELAY: usize = 14;
pub const S_REVERB: usize = 15;
/// Envelope `i` starts at `S_ENV + 6 * i`: destination, amount, attack,
/// hold, decay, retrigger.
pub const S_ENV: usize = 16;
/// LFO `i` starts at `S_LFO + 6 * i`: shape, destination, trigger mode,
/// frequency, amount, retrigger.
pub const S_LFO: usize = 28;

impl Default for SamplerParams {
    /// A new sampler's settings (m8-js's defaults).
    fn default() -> Self {
        let mut b = [0u8; SAMPLER_PARAMS];
        b[S_LENGTH] = 0xFF;
        b[S_CUTOFF] = 0xFF;
        b[S_PAN] = 0x80;
        b[S_DRY] = 0xC0;
        for i in 0..2 {
            let e = S_ENV + 6 * i;
            b[e + 1] = 0xFF; // amount
            b[e + 4] = 0x80; // decay
            let l = S_LFO + 6 * i;
            b[l + 3] = 0x10; // frequency
            b[l + 4] = 0xFF; // amount
        }
        SamplerParams(b)
    }
}

impl Instrument {
    /// A sampler setting by number: volume, pitch, fine tune, then the
    /// [`SamplerParams`] bytes. This is also the order of the sampler's
    /// effect commands.
    pub fn sampler_param(&self, i: usize) -> u8 {
        match i {
            0 => self.volume,
            1 => self.pitch,
            2 => self.fine_tune,
            _ => self.sampler.0[i - 3],
        }
    }

    /// Change the instrument's type. An empty instrument (whose settings
    /// are all 0xFF in the file) gets a new instrument's settings.
    pub fn set_kind(&mut self, kind: InstrumentKind) {
        if self.kind == InstrumentKind::None && kind != InstrumentKind::None {
            self.volume = 0;
            self.pitch = 0;
            self.fine_tune = 0x80;
            self.sampler = SamplerParams::default();
        }
        self.kind = kind;
    }

    pub fn set_sampler_param(&mut self, i: usize, v: u8) {
        match i {
            0 => self.volume = v,
            1 => self.pitch = v,
            2 => self.fine_tune = v,
            _ => self.sampler.0[i - 3] = v,
        }
    }
}

/// A song file. Not every field is used for playback yet.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Song {
    pub version: Version,
    pub directory: String,
    pub name: String,
    pub transpose: u8,
    pub tempo: f32,
    pub quantize: u8,
    pub key: u8,
    pub grooves: Vec<Groove>,
    /// `rows[row][track]` is a chain number, or 0xFF.
    pub rows: Vec<[u8; TRACKS]>,
    pub phrases: Vec<Phrase>,
    pub chains: Vec<Chain>,
    pub tables: Vec<Table>,
    pub instruments: Vec<Instrument>,
    /// The file as read; saving writes the edited parts back over it, so
    /// everything this reader skips survives a round trip.
    raw: Vec<u8>,
}

/// A cursor over the file, after m8-js's `M8FileReader`.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.bytes.len() {
            bail!(
                "the file ends early (needed {n} bytes at offset {:#x}, file is {:#x} bytes)",
                self.pos,
                self.bytes.len()
            );
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let b = self.bytes[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.bytes.len() {
            bail!("offset {pos:#x} is past the end of the file");
        }
        self.pos = pos;
        Ok(())
    }

    /// A fixed-size string field, ending at the first 0x00 or 0xFF.
    fn str(&mut self, len: usize) -> Result<String> {
        let raw = self.take(len)?;
        let end = raw
            .iter()
            .position(|&c| c == 0x00 || c == 0xFF)
            .unwrap_or(len);
        Ok(raw[..end].iter().map(|&c| c as char).collect())
    }

    fn fx(&mut self) -> Result<Fx> {
        Ok(Fx {
            command: self.u8()?,
            value: self.u8()?,
        })
    }
}

impl Song {
    pub fn load(path: &Path) -> Result<Song> {
        let bytes =
            std::fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
        Song::parse(&bytes).with_context(|| format!("{} is not a readable M8 song", path.display()))
    }

    /// A new, empty song.
    pub fn new_empty() -> Song {
        Song::parse(EMPTY_SONG).expect("the built-in empty song parses")
    }

    /// Write the song to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_bytes())
            .with_context(|| format!("can't write {}", path.display()))
    }

    /// The song as file bytes: the original file with the fields this
    /// program edits written back in place.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = self.raw.clone();
        b[OFF_TRANSPOSE] = self.transpose;
        b[OFF_TEMPO..OFF_TEMPO + 4].copy_from_slice(&self.tempo.to_le_bytes());
        put_str(&mut b[OFF_NAME..OFF_NAME + 12], &self.name, 0x00);
        for (i, g) in self.grooves.iter().enumerate() {
            let o = OFF_GROOVES + i * STEPS;
            b[o..o + STEPS].copy_from_slice(&g.steps);
        }
        for (i, row) in self.rows.iter().enumerate() {
            let o = OFF_ROWS + i * TRACKS;
            b[o..o + TRACKS].copy_from_slice(row);
        }
        for (i, p) in self.phrases.iter().enumerate() {
            for (j, st) in p.steps.iter().enumerate() {
                let o = OFF_PHRASES + (i * STEPS + j) * 9;
                b[o] = st.note;
                b[o + 1] = st.volume;
                b[o + 2] = st.instrument;
                for (k, fx) in st.fx.iter().enumerate() {
                    b[o + 3 + k * 2] = fx.command;
                    b[o + 4 + k * 2] = fx.value;
                }
            }
        }
        for (i, c) in self.chains.iter().enumerate() {
            for (j, st) in c.steps.iter().enumerate() {
                let o = OFF_CHAINS + (i * STEPS + j) * 2;
                b[o] = st.phrase;
                b[o + 1] = st.transpose;
            }
        }
        for (i, t) in self.tables.iter().enumerate() {
            for (j, st) in t.steps.iter().enumerate() {
                let o = OFF_TABLES + (i * STEPS + j) * 8;
                b[o] = st.transpose;
                b[o + 1] = st.volume;
                for (k, fx) in st.fx.iter().enumerate() {
                    b[o + 2 + k * 2] = fx.command;
                    b[o + 3 + k * 2] = fx.value;
                }
            }
        }
        for (i, inst) in self.instruments.iter().enumerate() {
            let o = OFF_INSTRUMENTS + i * INSTRUMENT_SIZE;
            b[o] = inst.kind.to_byte();
            let pad = if inst.kind == InstrumentKind::None {
                0xFF
            } else {
                0x00
            };
            put_str(&mut b[o + 1..o + 13], &inst.name, pad);
            b[o + 13] = inst.transpose as u8;
            b[o + 14] = inst.table_tick;
            if !matches!(inst.kind, InstrumentKind::MidiOut | InstrumentKind::None) {
                b[o + 15] = inst.volume;
                b[o + 16] = inst.pitch;
                b[o + 17] = inst.fine_tune;
            }
            if inst.kind == InstrumentKind::Sampler {
                b[o + 18..o + 18 + SAMPLER_PARAMS].copy_from_slice(&inst.sampler.0);
                put_str(&mut b[o + 0x57..o + 0x57 + 127], &inst.sample_path, 0x00);
            }
        }
        b
    }

    pub fn parse(bytes: &[u8]) -> Result<Song> {
        let mut r = Reader { bytes, pos: 0 };

        // Header: "M8VERSION", a byte, the version, a byte, the file type.
        if r.take(9)? != b"M8VERSION" {
            bail!("missing the M8VERSION header");
        }
        r.skip(1)?;
        let v = u16::from_le_bytes([r.u8()?, r.u8()?]);
        let version = Version {
            major: ((v >> 8) & 0x0F) as u8,
            minor: ((v >> 4) & 0x0F) as u8,
            patch: (v & 0x0F) as u8,
        };
        r.skip(1)?;
        let file_type = r.u8()?;
        if file_type != FILE_SONG {
            bail!("this is not a song file (file type {file_type:#04x})");
        }

        let directory = r.str(128)?;
        let transpose = r.u8()?;
        let tempo = f32::from_le_bytes(r.take(4)?.try_into().unwrap());
        let quantize = r.u8()?;
        let name = r.str(12)?;
        // MIDI settings: 9 single bytes, 8 track channels, 8 track
        // instruments, program change and input mode.
        r.skip(9 + 8 + 8 + 2)?;
        let key = r.u8()?;
        r.skip(18)?;
        // Mixer settings.
        r.skip(2 + 8 + 3 + 2 + 1 + 6 + 3 + 2)?;
        r.skip(5)?;

        debug_assert_eq!(r.pos, OFF_GROOVES);
        let mut grooves = Vec::with_capacity(GROOVES);
        for _ in 0..GROOVES {
            grooves.push(Groove {
                steps: r.take(STEPS)?.try_into().unwrap(),
            });
        }

        debug_assert_eq!(r.pos, OFF_ROWS);
        let mut rows = Vec::with_capacity(SONG_ROWS);
        for _ in 0..SONG_ROWS {
            rows.push(r.take(TRACKS)?.try_into().unwrap());
        }

        debug_assert_eq!(r.pos, OFF_PHRASES);
        let mut phrases = Vec::with_capacity(PHRASES);
        for _ in 0..PHRASES {
            let mut steps = [PhraseStep {
                note: EMPTY,
                volume: EMPTY,
                instrument: EMPTY,
                fx: [Fx::default(); 3],
            }; STEPS];
            for s in &mut steps {
                s.note = r.u8()?;
                s.volume = r.u8()?;
                s.instrument = r.u8()?;
                for fx in &mut s.fx {
                    *fx = r.fx()?;
                }
            }
            phrases.push(Phrase { steps });
        }

        debug_assert_eq!(r.pos, OFF_CHAINS);
        let mut chains = Vec::with_capacity(CHAINS);
        for _ in 0..CHAINS {
            let mut steps = [ChainStep {
                phrase: EMPTY,
                transpose: 0,
            }; STEPS];
            for s in &mut steps {
                s.phrase = r.u8()?;
                s.transpose = r.u8()?;
            }
            chains.push(Chain { steps });
        }

        debug_assert_eq!(r.pos, OFF_TABLES);
        let mut tables = Vec::with_capacity(TABLES);
        for _ in 0..TABLES {
            let mut steps = [TableStep {
                transpose: 0,
                volume: EMPTY,
                fx: [Fx::default(); 3],
            }; STEPS];
            for s in &mut steps {
                s.transpose = r.u8()?;
                s.volume = r.u8()?;
                for fx in &mut s.fx {
                    *fx = r.fx()?;
                }
            }
            tables.push(Table { steps });
        }

        debug_assert_eq!(r.pos, OFF_INSTRUMENTS);
        let mut instruments = Vec::with_capacity(INSTRUMENTS);
        for _ in 0..INSTRUMENTS {
            let start = r.pos;
            instruments.push(read_instrument(&mut r)?);
            r.seek(start + INSTRUMENT_SIZE)?;
        }

        Ok(Song {
            version,
            directory,
            name,
            transpose,
            tempo,
            quantize,
            key,
            grooves,
            rows,
            phrases,
            chains,
            tables,
            instruments,
            raw: bytes.to_vec(),
        })
    }

    /// Instruments that play a note somewhere in the arrangement.
    pub fn used_instruments(&self) -> Vec<u8> {
        let mut used = [false; INSTRUMENTS];
        let mut chain_seen = [false; CHAINS];
        for row in &self.rows {
            for &c in row {
                let c = c as usize;
                if c >= CHAINS || chain_seen[c] {
                    continue;
                }
                chain_seen[c] = true;
                for cs in &self.chains[c].steps {
                    let Some(p) = self.phrases.get(cs.phrase as usize) else {
                        continue;
                    };
                    for s in &p.steps {
                        if s.note != EMPTY {
                            // A note without an instrument plays the track's
                            // previous one; instrument 00 if there is none.
                            let i = if s.instrument == EMPTY {
                                0
                            } else {
                                s.instrument
                            };
                            if (i as usize) < INSTRUMENTS {
                                used[i as usize] = true;
                            }
                        }
                    }
                }
            }
        }
        (0..INSTRUMENTS as u8)
            .filter(|&i| used[i as usize])
            .collect()
    }
}

/// Write a fixed-size string field, ended by a 0x00 and padded with `pad`.
/// Unchanged text is left alone, so bytes after the end marker survive.
fn put_str(field: &mut [u8], text: &str, pad: u8) {
    let end = field
        .iter()
        .position(|&c| c == 0x00 || c == 0xFF)
        .unwrap_or(field.len());
    if field[..end] == *text.as_bytes() {
        return;
    }
    field.fill(pad);
    let n = text.len().min(field.len());
    field[..n].copy_from_slice(&text.as_bytes()[..n]);
    if n < field.len() {
        field[n] = 0x00;
    }
}

/// The instrument fields shared by every kind. m8-js reads the kind-specific
/// parameters next; playback doesn't need them.
fn read_instrument(r: &mut Reader) -> Result<Instrument> {
    let start = r.pos;
    let kind = InstrumentKind::from_byte(r.u8()?);
    let name = r.str(12)?;
    let transpose = r.u8()? != 0;
    let table_tick = r.u8()?;
    let (volume, pitch, fine_tune) = if kind == InstrumentKind::MidiOut {
        (0x00, 0x00, 0x80)
    } else {
        (r.u8()?, r.u8()?, r.u8()?)
    };
    let sampler = if kind == InstrumentKind::Sampler {
        SamplerParams(r.take(SAMPLER_PARAMS)?.try_into().unwrap())
    } else {
        SamplerParams::default()
    };
    r.seek(start + 0x57)?;
    let sample_path = if kind == InstrumentKind::Sampler {
        r.str(127)?
    } else {
        String::new()
    };
    Ok(Instrument {
        kind,
        name,
        transpose,
        table_tick,
        volume,
        pitch,
        fine_tune,
        sample_path,
        sampler,
    })
}

/// "C-4", "F#5"... in the M8's own octave numbering; "---" for no note.
pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
    ];
    if note == EMPTY {
        return "---".into();
    }
    if note >= 0x80 {
        return "OFF".into();
    }
    let oct = note / 12 + 1;
    let oct = match oct {
        0..=9 => char::from(b'0' + oct),
        10 => 'A',
        _ => 'B',
    };
    format!("{}{}", NAMES[(note % 12) as usize], oct)
}

/// Sequencer effect names, by command number (firmware 2.x).
pub const SEQ_FX: [&str; 0x17] = [
    "ARP", "CHA", "DEL", "GRV", "HOP", "KIL", "RAN", "RET", "REP", "NTH", "PSL", "PSN", "PVB",
    "PVX", "SCA", "SCG", "SED", "SNG", "TBL", "THO", "TIC", "TPO", "TSP",
];

/// Sampler effect names, from command 0x80 (firmware 2.6, after m8-js).
pub const SAMPLER_FX: [&str; 0x23] = [
    "VOL", "PIT", "FIN", "PLY", "STA", "LOP", "LEN", "DEG", "FLT", "CUT", "RES", "AMP", "LIM",
    "PAN", "DRY", "SCH", "SDL", "SRV", "EA1", "AT1", "HO1", "DE1", "ET1", "EA2", "AT2", "HO2",
    "DE2", "ET2", "LA1", "LF1", "LT1", "LA2", "LF2", "LT2", "SLI",
];

/// The first instrument effect command.
pub const INSTRUMENT_FX: u8 = 0x80;

/// Every effect command the phrase and table views offer, in order.
pub fn fx_commands() -> impl Iterator<Item = u8> {
    (0..SEQ_FX.len() as u8).chain(INSTRUMENT_FX..INSTRUMENT_FX + SAMPLER_FX.len() as u8)
}

pub fn fx_name(command: u8) -> String {
    if command == EMPTY {
        return "---".into();
    }
    let name = match command.checked_sub(INSTRUMENT_FX) {
        Some(i) => SAMPLER_FX.get(i as usize),
        None => SEQ_FX.get(command as usize),
    };
    match name {
        Some(n) => (*n).into(),
        None => format!("x{command:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Song {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        Song::load(&p).unwrap()
    }

    // Expected values were read from the same files with m8-js.
    #[test]
    fn reads_the_m8js_test_song() {
        let s = fixture("TESTING.m8s");
        assert_eq!(s.version.to_string(), "2.7.0");
        assert_eq!(s.name, "TESTING");
        assert_eq!(s.tempo, 128.0);
        assert_eq!(s.directory, "/Songs/");
        assert_eq!((s.transpose, s.quantize, s.key), (1, 1, 0));
        assert_eq!(s.grooves[0].ticks(), [6, 6]);
        assert_eq!(s.grooves[1].steps[..3], [15, 14, 13]);
        assert_eq!(
            s.chains[0].steps[1],
            ChainStep {
                phrase: 1,
                transpose: 2
            }
        );
        assert_eq!(s.chains[1].steps[0].phrase, EMPTY);
        assert_eq!(s.rows[0], [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(s.rows[1], [EMPTY; 8]);
        let p = &s.phrases[0].steps;
        assert_eq!((p[0].note, p[0].volume, p[0].instrument), (36, 1, 0));
        assert_eq!(
            p[0].fx[1],
            Fx {
                command: 1,
                value: 2
            }
        );
        assert_eq!((p[3].note, p[3].volume, p[3].instrument), (39, 4, 3));
        assert_eq!(
            p[3].fx[2],
            Fx {
                command: 11,
                value: 12
            }
        );
        let kinds: Vec<_> = s.instruments[..6]
            .iter()
            .map(|i| (i.kind, i.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (InstrumentKind::None, ""),
                (InstrumentKind::FmSynth, "DEF_FM"),
                (InstrumentKind::Macrosynth, "DEF_MAC"),
                (InstrumentKind::MidiOut, "DEF_MID"),
                (InstrumentKind::Sampler, "DEF_SAM"),
                (InstrumentKind::Wavsynth, "DEF_WAV"),
            ]
        );
        let i = &s.instruments[1];
        assert_eq!(
            (i.transpose, i.table_tick, i.volume, i.pitch, i.fine_tune),
            (true, 1, 0, 0, 128)
        );
        assert_eq!(s.instruments[127].kind, InstrumentKind::None);
        assert_eq!(s.phrases[254].steps[15].note, EMPTY);
    }

    #[test]
    fn reads_the_default_song() {
        let s = fixture("DEFAULT.m8s");
        assert_eq!(s.tempo, 120.0);
        assert!(s.rows.iter().all(|r| *r == [EMPTY; 8]));
        assert!(s.used_instruments().is_empty());
    }

    #[test]
    fn saving_unchanged_songs_is_byte_identical() {
        for name in ["TESTING.m8s", "DEFAULT.m8s"] {
            let s = fixture(name);
            assert!(s.to_bytes() == s.raw, "{name} changed on save");
        }
    }

    #[test]
    fn edits_survive_saving() {
        let mut s = Song::new_empty();
        s.name = "EDITED".into();
        s.tempo = 133.5;
        s.rows[3][7] = 0x12;
        s.phrases[0x12].steps[5].fx[2] = Fx {
            command: 0x05,
            value: 0x33,
        };
        s.chains[0x12].steps[15].transpose = 0xF4;
        s.tables[9].steps[1].volume = 0x40;
        s.grooves[2].steps[0] = 9;
        s.instruments[4].kind = InstrumentKind::FmSynth;
        s.instruments[4].name = "KEYS".into();
        let back = Song::parse(&s.to_bytes()).unwrap();
        assert_eq!((back.name.as_str(), back.tempo), ("EDITED", 133.5));
        assert_eq!(back.rows[3][7], 0x12);
        assert_eq!(back.phrases[0x12], s.phrases[0x12]);
        assert_eq!(back.chains[0x12], s.chains[0x12]);
        assert_eq!(back.tables[9], s.tables[9]);
        assert_eq!(back.grooves[2], s.grooves[2]);
        assert_eq!(back.instruments[4].kind, InstrumentKind::FmSynth);
        assert_eq!(back.instruments[4].name, "KEYS");
    }

    #[test]
    fn sampler_settings_survive_saving() {
        let s = fixture("TESTING.m8s");
        let sam = &s.instruments[4];
        assert_eq!(sam.sampler, SamplerParams::default());
        let mut s = Song::new_empty();
        let inst = &mut s.instruments[7];
        inst.set_kind(InstrumentKind::Sampler);
        assert_eq!((inst.volume, inst.pitch, inst.fine_tune), (0, 0, 0x80));
        inst.sample_path = "/Samples/drums/kick.wav".into();
        inst.set_sampler_param(3 + S_START, 0x40);
        inst.set_sampler_param(1, 0xF4);
        inst.sampler.0[S_LFO + 7] = 0x22;
        let back = Song::parse(&s.to_bytes()).unwrap();
        assert_eq!(back.instruments[7], s.instruments[7]);
        assert_eq!(back.instruments[7].sampler_param(3 + S_START), 0x40);
    }

    #[test]
    fn note_names() {
        assert_eq!(note_name(36), "C-4");
        assert_eq!(note_name(37), "C#4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(EMPTY), "---");
    }

    #[test]
    fn rejects_other_files() {
        assert!(Song::parse(b"hello").is_err());
        let mut b = b"M8VERSION\0\x00\x02\0\x01".to_vec();
        b.resize(200, 0);
        assert!(Song::parse(&b)
            .unwrap_err()
            .to_string()
            .contains("not a song"));
    }
}
