//! The M8 song sequencer: walks song rows, chains and phrases for each of
//! the eight tracks and turns them into note events, one tick at a time.
//!
//! Timing follows the M8: 24 ticks per beat, and each phrase step lasts as
//! many ticks as the track's groove says (6 by default, a sixteenth note).
//! Tracks run independently. When a track's chain ends it moves to the next
//! song row; when that row is empty it jumps back to the first row of its
//! block (the rows since the last empty one), so a block loops.
//!
//! Effects handled so far: DEL, GRV, HOP, KIL and TPO. Others are ignored.

use std::sync::Arc;

use super::format::{Song, CHAINS, EMPTY, GROOVES, INSTRUMENTS, PHRASES, SONG_ROWS, STEPS, TRACKS};

pub const TICKS_PER_BEAT: f64 = 24.0;

const FX_DEL: u8 = 0x02;
const FX_GRV: u8 = 0x03;
const FX_HOP: u8 = 0x04;
const FX_KIL: u8 = 0x05;
const FX_TPO: u8 = 0x15;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeqEvent {
    /// `note` is an M8 note number (0 = C-1) after transposition.
    NoteOn {
        track: u8,
        instrument: u8,
        note: i16,
        velocity: f64,
    },
    NoteOff {
        track: u8,
        instrument: u8,
        note: i16,
    },
}

/// Where a track is in the arrangement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub row: usize,
    pub chain: usize,
    pub chain_step: usize,
    pub phrase: usize,
    pub phrase_step: usize,
}

/// What a track repeats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Looping {
    /// Follow the song rows (blocks loop on their own).
    #[default]
    Song,
    /// Repeat the current chain.
    Chain,
    /// Repeat the current phrase.
    Phrase,
}

#[derive(Clone, Copy, Default)]
struct Track {
    /// `None` when the track is stopped.
    pos: Option<Pos>,
    looping: Looping,
    /// Tick within the current step, and the step's length.
    tick: u32,
    len: u32,
    instrument: u8,
    groove: usize,
    /// The sounding note: (instrument, note).
    playing: Option<(u8, i16)>,
    /// A note waiting for its tick (DEL): (tick, instrument, note, velocity).
    trigger: Option<(u32, u8, i16, f64)>,
    /// Tick at which to cut the note (KIL, or an OFF note).
    kill: Option<u32>,
    hop: Option<u8>,
}

pub struct Sequencer {
    song: Arc<Song>,
    tracks: [Track; TRACKS],
    tempo: f64,
    /// Bit per track.
    muted: u8,
    soloed: u8,
}

/// Samples between two ticks at `tempo` BPM.
pub fn samples_per_tick(tempo: f64, sample_rate: f64) -> f64 {
    sample_rate * 60.0 / (tempo.max(1.0) * TICKS_PER_BEAT)
}

impl Sequencer {
    pub fn new(song: Arc<Song>) -> Self {
        let tempo = song.tempo as f64;
        Sequencer {
            song,
            tracks: [Track::default(); TRACKS],
            tempo,
            muted: 0,
            soloed: 0,
        }
    }

    /// Swap in an edited song. Tracks carry on from where they are.
    pub fn set_song(&mut self, song: Arc<Song>) {
        self.song = song;
    }

    /// Silence tracks: bit `t` of `muted` mutes track `t`; when any bit of
    /// `soloed` is set, only those tracks play.
    pub fn set_mutes(&mut self, muted: u8, soloed: u8) {
        self.muted = muted;
        self.soloed = soloed;
    }

    fn audible(&self, t: usize) -> bool {
        let bit = 1 << t;
        self.muted & bit == 0 && (self.soloed == 0 || self.soloed & bit != 0)
    }

    /// Loop one chain on one track; other tracks stop.
    pub fn play_chain(&mut self, track: usize, row: usize, out: &mut Vec<SeqEvent>) {
        self.stop(out);
        self.tempo = self.song.tempo as f64;
        let chain = self.song.rows[row.min(SONG_ROWS - 1)][track] as usize;
        if chain >= CHAINS {
            return;
        }
        self.tracks[track] = Track {
            pos: self.find_in_chain(row, chain, 0),
            looping: Looping::Chain,
            ..Track::default()
        };
    }

    /// Loop one phrase on one track; other tracks stop. `pos` gives the
    /// chain context (for transpose) and the phrase.
    pub fn play_phrase(&mut self, track: usize, pos: Pos, out: &mut Vec<SeqEvent>) {
        self.stop(out);
        self.tempo = self.song.tempo as f64;
        if pos.phrase >= PHRASES {
            return;
        }
        self.tracks[track] = Track {
            pos: Some(Pos {
                phrase_step: 0,
                ..pos
            }),
            looping: Looping::Phrase,
            ..Track::default()
        };
    }

    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    pub fn is_playing(&self) -> bool {
        self.tracks.iter().any(|t| t.pos.is_some())
    }

    pub fn position(&self, track: usize) -> Option<Pos> {
        self.tracks[track].pos
    }

    /// The note a track is sounding: (instrument, note).
    pub fn sounding(&self, track: usize) -> Option<(u8, i16)> {
        self.tracks[track].playing
    }

    /// Start every track from song row `row`. Tracks with an empty cell on
    /// that row stay silent.
    pub fn play_from(&mut self, row: usize, out: &mut Vec<SeqEvent>) {
        self.stop(out);
        self.tempo = self.song.tempo as f64;
        for t in 0..TRACKS {
            self.tracks[t] = Track {
                pos: self.find(t, row.min(SONG_ROWS - 1), 0),
                ..Track::default()
            };
        }
    }

    /// Stop all tracks and release their notes.
    pub fn stop(&mut self, out: &mut Vec<SeqEvent>) {
        for t in 0..TRACKS {
            self.release(t, out);
            self.tracks[t].pos = None;
        }
    }

    /// Advance one tick, appending the notes that start or stop on it.
    pub fn tick(&mut self, out: &mut Vec<SeqEvent>) {
        for t in 0..TRACKS {
            if self.tracks[t].pos.is_none() {
                continue;
            }
            if self.tracks[t].tick == 0 {
                self.start_step(t);
            }
            let tr = self.tracks[t];
            if let Some((at, inst, note, velocity)) = tr.trigger {
                if at == tr.tick && !self.audible(t) {
                    self.release(t, out);
                    self.tracks[t].trigger = None;
                } else if at == tr.tick {
                    self.release(t, out);
                    out.push(SeqEvent::NoteOn {
                        track: t as u8,
                        instrument: inst,
                        note,
                        velocity,
                    });
                    self.tracks[t].playing = Some((inst, note));
                    self.tracks[t].trigger = None;
                }
            }
            if tr.kill == Some(tr.tick) {
                self.release(t, out);
                self.tracks[t].kill = None;
            }
            let tr = &mut self.tracks[t];
            tr.tick += 1;
            if tr.tick >= tr.len {
                tr.tick = 0;
                self.next_step(t);
                if self.tracks[t].pos.is_none() {
                    self.release(t, out);
                }
            }
        }
    }

    fn release(&mut self, t: usize, out: &mut Vec<SeqEvent>) {
        if let Some((instrument, note)) = self.tracks[t].playing.take() {
            out.push(SeqEvent::NoteOff {
                track: t as u8,
                instrument,
                note,
            });
        }
    }

    /// Read the step under the track's position: effects, then the note.
    fn start_step(&mut self, t: usize) {
        let song = &self.song;
        let tr = &mut self.tracks[t];
        let Some(pos) = tr.pos else { return };
        let step = song.phrases[pos.phrase].steps[pos.phrase_step];
        tr.trigger = None;
        tr.kill = None;
        tr.hop = None;

        let mut delay = 0;
        for fx in step.fx {
            match fx.command {
                FX_DEL => delay = fx.value as u32,
                FX_GRV => tr.groove = fx.value as usize % GROOVES,
                FX_HOP => tr.hop = Some(fx.value),
                FX_KIL => tr.kill = Some(fx.value as u32),
                FX_TPO if fx.value > 0 => self.tempo = fx.value as f64,
                _ => {}
            }
        }

        let groove = song.grooves[tr.groove].ticks();
        tr.len = match groove.len() {
            0 => 6,
            n => groove[pos.phrase_step % n].max(1) as u32,
        };

        if (step.instrument as usize) < INSTRUMENTS {
            tr.instrument = step.instrument;
        }
        match step.note {
            EMPTY => {}
            n if n >= 0x80 => {
                tr.kill.get_or_insert(delay);
            }
            n => {
                let inst = tr.instrument;
                let mut note = n as i16;
                if song.instruments[inst as usize].transpose {
                    note += song.chains[pos.chain].steps[pos.chain_step].transpose as i8 as i16;
                }
                let velocity = match step.volume {
                    EMPTY => 1.0,
                    v => v.min(0x7F) as f64 / 127.0,
                };
                tr.trigger = Some((delay, inst, note, velocity));
            }
        }
    }

    fn next_step(&mut self, t: usize) {
        let tr = &mut self.tracks[t];
        let Some(mut pos) = tr.pos else { return };
        let next = match tr.hop.take() {
            Some(v) if (v as usize) < STEPS => Some(v as usize),
            Some(_) => None,
            None if pos.phrase_step + 1 < STEPS => Some(pos.phrase_step + 1),
            None => None,
        };
        if let Some(step) = next {
            pos.phrase_step = step;
            tr.pos = Some(pos);
            return;
        }
        self.tracks[t].pos = match self.tracks[t].looping {
            Looping::Song => self.find(t, pos.row, pos.chain_step + 1),
            Looping::Phrase => Some(Pos {
                phrase_step: 0,
                ..pos
            }),
            Looping::Chain => self
                .find_in_chain(pos.row, pos.chain, pos.chain_step + 1)
                .or_else(|| self.find_in_chain(pos.row, pos.chain, 0)),
        };
    }

    /// The first step with a phrase in `chain`, at or after `from`.
    fn find_in_chain(&self, row: usize, chain: usize, from: usize) -> Option<Pos> {
        (from..STEPS).find_map(|cs| {
            let phrase = self.song.chains[chain].steps[cs].phrase as usize;
            (phrase < PHRASES).then_some(Pos {
                row,
                chain,
                chain_step: cs,
                phrase,
                phrase_step: 0,
            })
        })
    }

    /// The first playable chain step at or after `row`/`chain_step`,
    /// following song order and block loops.
    fn find(&self, t: usize, mut row: usize, mut chain_step: usize) -> Option<Pos> {
        for _ in 0..=SONG_ROWS {
            let chain = self.song.rows[row][t] as usize;
            if chain >= CHAINS {
                return None;
            }
            for cs in chain_step..STEPS {
                let phrase = self.song.chains[chain].steps[cs].phrase as usize;
                if phrase < PHRASES {
                    return Some(Pos {
                        row,
                        chain,
                        chain_step: cs,
                        phrase,
                        phrase_step: 0,
                    });
                }
            }
            row = self.next_row(t, row);
            chain_step = 0;
        }
        None
    }

    fn next_row(&self, t: usize, row: usize) -> usize {
        let rows = &self.song.rows;
        if row + 1 < SONG_ROWS && rows[row + 1][t] != EMPTY {
            return row + 1;
        }
        let mut first = row;
        while first > 0 && rows[first - 1][t] != EMPTY {
            first -= 1;
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m8::format::Fx;
    use std::path::Path;

    fn blank() -> Song {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/DEFAULT.m8s");
        Song::load(&p).unwrap()
    }

    fn set_note(s: &mut Song, phrase: usize, step: usize, note: u8, inst: u8) {
        let st = &mut s.phrases[phrase].steps[step];
        st.note = note;
        st.instrument = inst;
    }

    fn set_fx(s: &mut Song, phrase: usize, step: usize, command: u8, value: u8) {
        s.phrases[phrase].steps[step].fx[0] = Fx { command, value };
    }

    /// Run `ticks` ticks, returning (tick, event) pairs.
    fn run(song: Song, ticks: usize) -> Vec<(usize, SeqEvent)> {
        let mut seq = Sequencer::new(Arc::new(song));
        let mut out = Vec::new();
        seq.play_from(0, &mut out);
        let mut log = Vec::new();
        for i in 0..ticks {
            seq.tick(&mut out);
            log.extend(out.drain(..).map(|e| (i, e)));
        }
        log
    }

    fn ons(log: &[(usize, SeqEvent)]) -> Vec<(usize, u8, i16)> {
        log.iter()
            .filter_map(|&(i, e)| match e {
                SeqEvent::NoteOn { track, note, .. } => Some((i, track, note)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plays_a_phrase_and_loops_the_block() {
        let mut s = blank();
        s.rows[0][0] = 0;
        s.chains[0].steps[0].phrase = 0;
        set_note(&mut s, 0, 0, 36, 1);
        set_note(&mut s, 0, 4, 40, EMPTY);
        let log = run(s, 16 * 6 + 1);
        assert_eq!(ons(&log), [(0, 0, 36), (24, 0, 40), (96, 0, 36)]);
        // The first note is released when the second starts, with the
        // instrument carried over.
        assert_eq!(
            log[1].1,
            SeqEvent::NoteOff {
                track: 0,
                instrument: 1,
                note: 36
            }
        );
        assert!(matches!(log[2].1, SeqEvent::NoteOn { instrument: 1, .. }));
    }

    #[test]
    fn follows_chains_rows_and_blocks() {
        let mut s = blank();
        // Track 1: rows 0-1 form a block, row 2 is empty.
        s.rows[0][1] = 0;
        s.rows[1][1] = 1;
        // Chain 0 plays phrases 0 then 1, chain 1 plays phrase 2 transposed.
        s.chains[0].steps[0].phrase = 0;
        s.chains[0].steps[2].phrase = 1; // an empty step in between is skipped
        s.chains[1].steps[0] = crate::m8::format::ChainStep {
            phrase: 2,
            transpose: (-12i8) as u8,
        };
        set_note(&mut s, 0, 0, 36, 0);
        set_note(&mut s, 1, 0, 38, 0);
        set_note(&mut s, 2, 0, 40, 0);
        let log = run(s, 96 * 4);
        assert_eq!(
            ons(&log),
            [(0, 1, 36), (96, 1, 38), (192, 1, 28), (288, 1, 36)]
        );
    }

    #[test]
    fn instruments_can_ignore_chain_transpose() {
        let mut s = blank();
        s.rows[0][0] = 0;
        s.chains[0].steps[0].phrase = 0;
        s.chains[0].steps[0].transpose = 5;
        s.instruments[2].transpose = false;
        set_note(&mut s, 0, 0, 36, 1);
        set_note(&mut s, 0, 1, 36, 2);
        assert_eq!(ons(&run(s, 12)), [(0, 0, 41), (6, 0, 36)]);
    }

    #[test]
    fn effects() {
        let mut s = blank();
        s.rows[0][0] = 0;
        s.chains[0].steps[0].phrase = 0;
        set_note(&mut s, 0, 0, 36, 0);
        set_fx(&mut s, 0, 0, FX_DEL, 3); // note on tick 3
        set_note(&mut s, 0, 1, 37, 0);
        set_fx(&mut s, 0, 1, FX_KIL, 2); // cut on tick 6 + 2
        set_fx(&mut s, 0, 2, FX_HOP, 0); // back to step 0 after step 2
        let log = run(s, 24);
        assert_eq!(ons(&log), [(3, 0, 36), (6, 0, 37), (21, 0, 36)]);
        assert!(log
            .iter()
            .any(|&(i, e)| i == 8 && matches!(e, SeqEvent::NoteOff { note: 37, .. })));
    }

    #[test]
    fn grooves_and_tempo() {
        let mut s = blank();
        s.grooves[1].steps[..3].copy_from_slice(&[7, 5, EMPTY]);
        s.rows[0][0] = 0;
        s.chains[0].steps[0].phrase = 0;
        for i in 0..4 {
            set_note(&mut s, 0, i, 36, 0);
        }
        set_fx(&mut s, 0, 0, FX_GRV, 1);
        s.phrases[0].steps[1].fx[1] = Fx {
            command: FX_TPO,
            value: 140,
        };
        let mut seq = Sequencer::new(Arc::new(s));
        let mut out = Vec::new();
        seq.play_from(0, &mut out);
        let mut starts = Vec::new();
        for i in 0..24 {
            seq.tick(&mut out);
            if out.iter().any(|e| matches!(e, SeqEvent::NoteOn { .. })) {
                starts.push(i);
            }
            out.clear();
        }
        assert_eq!(starts, [0, 7, 12, 19]);
        assert_eq!(seq.tempo(), 140.0);
    }

    #[test]
    fn loops_a_chain_or_a_phrase() {
        let mut s = blank();
        s.rows[0][2] = 0;
        s.rows[1][2] = 1;
        s.chains[0].steps[0].phrase = 0;
        s.chains[0].steps[1].phrase = 1;
        set_note(&mut s, 0, 0, 36, 0);
        set_note(&mut s, 1, 0, 38, 0);
        let song = Arc::new(s);
        let mut seq = Sequencer::new(song.clone());
        let mut out = Vec::new();
        seq.play_chain(2, 0, &mut out);
        let mut notes = Vec::new();
        for _ in 0..96 * 4 {
            seq.tick(&mut out);
            for e in out.drain(..) {
                if let SeqEvent::NoteOn { note, .. } = e {
                    notes.push(note);
                }
            }
        }
        assert_eq!(notes, [36, 38, 36, 38]);
        let pos = Pos {
            row: 0,
            chain: 0,
            chain_step: 1,
            phrase: 1,
            phrase_step: 5,
        };
        seq.play_phrase(2, pos, &mut out);
        notes.clear();
        for _ in 0..96 * 3 {
            seq.tick(&mut out);
            for e in out.drain(..) {
                if let SeqEvent::NoteOn { note, .. } = e {
                    notes.push(note);
                }
            }
        }
        assert_eq!(notes, [38, 38, 38]);
    }

    #[test]
    fn muted_tracks_stay_silent() {
        let mut s = blank();
        s.rows[0][0] = 0;
        s.rows[0][1] = 0;
        s.chains[0].steps[0].phrase = 0;
        set_note(&mut s, 0, 0, 36, 0);
        let mut seq = Sequencer::new(Arc::new(s));
        let mut out = Vec::new();
        seq.set_mutes(0b01, 0);
        seq.play_from(0, &mut out);
        seq.tick(&mut out);
        assert!(matches!(out[..], [SeqEvent::NoteOn { track: 1, .. }]));
        out.clear();
        seq.set_mutes(0, 0b01);
        seq.play_from(0, &mut out);
        out.clear();
        seq.tick(&mut out);
        assert!(matches!(out[..], [SeqEvent::NoteOn { track: 0, .. }]));
    }

    #[test]
    fn empty_song_stops() {
        let mut s = blank();
        s.rows[0][0] = 0; // a chain with no phrases
        let mut seq = Sequencer::new(Arc::new(s));
        let mut out = Vec::new();
        seq.play_from(0, &mut out);
        assert!(!seq.is_playing());
    }

    #[test]
    fn tick_length() {
        // 120 BPM: a sixteenth (6 ticks) is 125 ms.
        let spt = samples_per_tick(120.0, 48_000.0);
        assert!((spt * 6.0 - 6_000.0).abs() < 1e-9);
    }
}
