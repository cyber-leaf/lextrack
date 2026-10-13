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

impl Fx {
    pub fn is_empty(&self) -> bool {
        self.command == EMPTY
    }
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

        let mut grooves = Vec::with_capacity(GROOVES);
        for _ in 0..GROOVES {
            grooves.push(Groove {
                steps: r.take(STEPS)?.try_into().unwrap(),
            });
        }

        let mut rows = Vec::with_capacity(SONG_ROWS);
        for _ in 0..SONG_ROWS {
            rows.push(r.take(TRACKS)?.try_into().unwrap());
        }

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
    r.seek(start + 0x57)?;
    let sample_path = if kind == InstrumentKind::Sampler {
        r.str(128)?
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

pub fn fx_name(command: u8) -> String {
    match SEQ_FX.get(command as usize) {
        Some(n) => (*n).into(),
        None if command == EMPTY => "---".into(),
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
