//! Playing notes from the computer keyboard.
//!
//! White keys are on `a s d f g h j k l ; '` and black keys on
//! `w e t y u o p`, like a piano keyboard laid over the home row. `z` and `x`
//! move down and up an octave.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// True when the terminal reports key releases, so notes can stop exactly
/// when the key is let go. Set at startup.
pub static RELEASE_EVENTS: AtomicBool = AtomicBool::new(false);

/// Without key releases, a tapped key plays for this long. It is longer than
/// the usual auto-repeat delay, so a key that is held down starts repeating
/// before the note would stop.
const TAP_LENGTH: Duration = Duration::from_millis(700);
/// ...and each auto-repeat of a held key keeps the note going this much longer.
const REPEAT_LENGTH: Duration = Duration::from_millis(150);

pub const VELOCITY: f64 = 0.8;

/// Semitones above the C at the start of the current octave.
fn offset(c: char) -> Option<i16> {
    Some(match c {
        'a' => 0,
        'w' => 1,
        's' => 2,
        'e' => 3,
        'd' => 4,
        'f' => 5,
        't' => 6,
        'g' => 7,
        'y' => 8,
        'h' => 9,
        'u' => 10,
        'j' => 11,
        'k' => 12,
        'o' => 13,
        'l' => 14,
        'p' => 15,
        ';' => 16,
        '\'' => 17,
        _ => return None,
    })
}

pub fn is_note_key(c: char) -> bool {
    offset(c.to_ascii_lowercase()).is_some()
}

pub fn note_name(key: i16) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[key.rem_euclid(12) as usize],
        key.div_euclid(12) - 1
    )
}

pub struct Keyboard {
    /// Octave of the `a` key; 4 makes `a` middle C (MIDI 60).
    pub octave: i16,
    /// Sounding notes by the key that started them, with the time they stop
    /// on their own (only when the terminal doesn't report releases).
    held: HashMap<char, (i16, Option<Instant>)>,
}

impl Keyboard {
    pub fn new() -> Self {
        Keyboard {
            octave: 4,
            held: HashMap::new(),
        }
    }

    fn releases() -> bool {
        RELEASE_EVENTS.load(Ordering::Relaxed)
    }

    pub fn octave_down(&mut self) {
        self.octave = (self.octave - 1).max(-1);
    }

    pub fn octave_up(&mut self) {
        // Keep the top key (`'`, 17 semitones up) within MIDI's 0..=127.
        self.octave = (self.octave + 1).min(8);
    }

    /// A note key went down (or auto-repeated). Returns the MIDI key to start,
    /// or `None` if that key is already sounding.
    pub fn press(&mut self, c: char, now: Instant) -> Option<i16> {
        let c = c.to_ascii_lowercase();
        let off = offset(c)?;
        if let Some((_, until)) = self.held.get_mut(&c) {
            // Still held: an auto-repeat, not a new strike.
            if let Some(t) = until {
                *t = (*t).max(now + REPEAT_LENGTH);
            }
            return None;
        }
        let key = 12 * (self.octave + 1) + off;
        if !(0..=127).contains(&key) {
            return None;
        }
        let until = (!Self::releases()).then(|| now + TAP_LENGTH);
        self.held.insert(c, (key, until));
        Some(key)
    }

    /// A note key was released. Returns the MIDI key to stop.
    pub fn release(&mut self, c: char) -> Option<i16> {
        self.held.remove(&c.to_ascii_lowercase()).map(|(k, _)| k)
    }

    /// Notes whose timed hold ran out.
    pub fn expired(&mut self, now: Instant) -> Vec<i16> {
        let mut out = Vec::new();
        self.held.retain(|_, (key, until)| {
            let done = until.is_some_and(|t| now >= t);
            if done {
                out.push(*key);
            }
            !done
        });
        out
    }

    /// Stop everything.
    pub fn release_all(&mut self) -> Vec<i16> {
        self.held.drain().map(|(_, (k, _))| k).collect()
    }

    pub fn sounding(&self) -> Vec<i16> {
        let mut v: Vec<i16> = self.held.values().map(|(k, _)| *k).collect();
        v.sort_unstable();
        v
    }

    /// The range the keys cover right now, e.g. "C4–F5".
    pub fn range(&self) -> String {
        let base = 12 * (self.octave + 1);
        format!("{}–{}", note_name(base), note_name((base + 17).min(127)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_a_piano() {
        let whites: Vec<i16> = "asdfghjkl;'".chars().map(|c| offset(c).unwrap()).collect();
        assert_eq!(whites, [0, 2, 4, 5, 7, 9, 11, 12, 14, 16, 17]);
        let blacks: Vec<i16> = "wetyuop".chars().map(|c| offset(c).unwrap()).collect();
        assert_eq!(blacks, [1, 3, 6, 8, 10, 13, 15]);
    }

    #[test]
    fn names_and_octaves() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(61), "C#4");
        let mut kb = Keyboard::new();
        assert_eq!(kb.press('a', Instant::now()), Some(60));
        kb.octave_up();
        assert_eq!(kb.press('s', Instant::now()), Some(74));
        // Releasing uses the key that was played, even after an octave change.
        assert_eq!(kb.release('a'), Some(60));
    }

    #[test]
    fn timed_release_and_repeat() {
        RELEASE_EVENTS.store(false, Ordering::Relaxed);
        let mut kb = Keyboard::new();
        let t0 = Instant::now();
        assert_eq!(kb.press('a', t0), Some(60));
        // A repeat while held doesn't restart the note.
        assert_eq!(kb.press('a', t0 + Duration::from_millis(600)), None);
        assert!(kb.expired(t0 + Duration::from_millis(710)).is_empty());
        assert_eq!(kb.expired(t0 + Duration::from_millis(760)), vec![60]);
    }
}
