//! Turns terminal keys into presses of the M8's eight buttons.
//!
//! The M8 has UP, DOWN, LEFT, RIGHT, SHIFT, PLAY, OPTION and EDIT, and most
//! actions are one button pressed while others are held. A terminal can't
//! report a modifier key on its own, so each button has two spellings:
//!
//! - Arrows with Shift / Alt / Ctrl held are SHIFT / OPTION / EDIT + arrow.
//! - Letter keys stand for buttons: A = SHIFT, S = PLAY, Z = OPTION,
//!   X = EDIT; Space is PLAY and Enter is EDIT too. In terminals that report
//!   key releases (kitty protocol), letters can be held like real buttons:
//!   hold X and tap Right for EDIT+RIGHT. Elsewhere a capital letter adds
//!   SHIFT (Shift+Z is SHIFT+OPTION) and Delete is EDIT+OPTION.

use std::sync::atomic::AtomicBool;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Set when the terminal reports key releases.
pub static RELEASE_EVENTS: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    Shift,
    Play,
    Option,
    Edit,
}

impl Button {
    pub fn is_direction(self) -> bool {
        matches!(
            self,
            Button::Up | Button::Down | Button::Left | Button::Right
        )
    }
}

/// Buttons held while another one is pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    pub shift: bool,
    pub option: bool,
    pub edit: bool,
    pub play: bool,
}

impl Held {
    pub fn none(&self) -> bool {
        !(self.shift || self.option || self.edit || self.play)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub button: Button,
    pub held: Held,
}

#[cfg(test)]
impl Press {
    pub fn new(button: Button) -> Self {
        Press {
            button,
            held: Held::default(),
        }
    }

    pub fn with(button: Button, f: impl FnOnce(&mut Held)) -> Self {
        let mut p = Press::new(button);
        f(&mut p.held);
        p
    }
}

/// What a key means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Press(Press),
    /// A held letter button let go (only with release events).
    Release(Button),
    Quit,
    None,
}

/// Tracks letter keys held down, when the terminal reports releases.
#[derive(Default)]
pub struct Keys {
    held: Held,
}

impl Keys {
    pub fn translate(&mut self, k: KeyEvent, releases: bool) -> Input {
        let letter = match k.code {
            KeyCode::Char(c) => Some(c.to_ascii_lowercase()),
            _ => None,
        };
        if k.kind == KeyEventKind::Release {
            if let Some(c) = letter {
                self.set_held(c, false);
                if let Some(b) = letter_button(c) {
                    return Input::Release(b);
                }
            }
            if k.code == KeyCode::Enter {
                return Input::Release(Button::Edit);
            }
            return Input::None;
        }
        let repeat = k.kind == KeyEventKind::Repeat;
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(letter, Some('q') | Some('c')) {
            return Input::Quit;
        }

        let mut held = if releases { self.held } else { Held::default() };
        held.shift |= k.modifiers.contains(KeyModifiers::SHIFT);
        held.option |= k.modifiers.contains(KeyModifiers::ALT);
        held.edit |= k.modifiers.contains(KeyModifiers::CONTROL);

        let button = match k.code {
            KeyCode::Up => Button::Up,
            KeyCode::Down => Button::Down,
            KeyCode::Left => Button::Left,
            KeyCode::Right => Button::Right,
            KeyCode::Enter => Button::Edit,
            KeyCode::Esc => Button::Option,
            KeyCode::Delete | KeyCode::Backspace => {
                held.edit = true;
                Button::Option
            }
            KeyCode::Char(' ') => Button::Play,
            KeyCode::Char(c) => {
                let shifted = c.is_ascii_uppercase();
                held.shift |= shifted;
                let Some(b) = letter_button(c.to_ascii_lowercase()) else {
                    return Input::None;
                };
                if repeat {
                    return Input::None;
                }
                if releases {
                    self.set_held(c.to_ascii_lowercase(), true);
                }
                // Letters don't count as held for their own press.
                match b {
                    Button::Shift => held.shift = shifted,
                    Button::Play => held.play = false,
                    Button::Option => held.option = false,
                    Button::Edit => held.edit = false,
                    _ => {}
                }
                b
            }
            _ => return Input::None,
        };
        if button == Button::Shift && !releases && !held.option {
            // A SHIFT tap does nothing on the M8 (OPTION+SHIFT mutes).
            return Input::None;
        }
        Input::Press(Press { button, held })
    }

    fn set_held(&mut self, c: char, down: bool) {
        match c {
            'a' => self.held.shift = down,
            's' => self.held.play = down,
            'z' => self.held.option = down,
            'x' => self.held.edit = down,
            _ => {}
        }
    }
}

fn letter_button(c: char) -> Option<Button> {
    Some(match c {
        'a' => Button::Shift,
        's' => Button::Play,
        'z' => Button::Option,
        'x' => Button::Edit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        key(code, m, KeyEventKind::Press)
    }

    #[test]
    fn modifier_spellings() {
        let mut k = Keys::default();
        let p = k.translate(press(KeyCode::Right, KeyModifiers::CONTROL), false);
        assert_eq!(
            p,
            Input::Press(Press::with(Button::Right, |h| h.edit = true))
        );
        let p = k.translate(press(KeyCode::Up, KeyModifiers::SHIFT), false);
        assert_eq!(p, Input::Press(Press::with(Button::Up, |h| h.shift = true)));
        let p = k.translate(press(KeyCode::Char('Z'), KeyModifiers::SHIFT), false);
        assert_eq!(
            p,
            Input::Press(Press::with(Button::Option, |h| h.shift = true))
        );
        let p = k.translate(press(KeyCode::Delete, KeyModifiers::NONE), false);
        assert_eq!(
            p,
            Input::Press(Press::with(Button::Option, |h| h.edit = true))
        );
        let p = k.translate(press(KeyCode::Char('q'), KeyModifiers::CONTROL), false);
        assert_eq!(p, Input::Quit);
    }

    #[test]
    fn held_letters_with_release_events() {
        let mut k = Keys::default();
        let p = k.translate(press(KeyCode::Char('x'), KeyModifiers::NONE), true);
        assert_eq!(p, Input::Press(Press::new(Button::Edit)));
        let p = k.translate(press(KeyCode::Left, KeyModifiers::NONE), true);
        assert_eq!(
            p,
            Input::Press(Press::with(Button::Left, |h| h.edit = true))
        );
        let p = k.translate(press(KeyCode::Char('z'), KeyModifiers::NONE), true);
        assert_eq!(
            p,
            Input::Press(Press::with(Button::Option, |h| h.edit = true))
        );
        k.translate(
            key(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            true,
        );
        k.translate(
            key(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            true,
        );
        let p = k.translate(press(KeyCode::Left, KeyModifiers::NONE), true);
        assert_eq!(p, Input::Press(Press::new(Button::Left)));
        // A held A is SHIFT.
        k.translate(press(KeyCode::Char('a'), KeyModifiers::NONE), true);
        let p = k.translate(press(KeyCode::Right, KeyModifiers::NONE), true);
        assert_eq!(
            p,
            Input::Press(Press::with(Button::Right, |h| h.shift = true))
        );
    }

    #[test]
    fn letters_are_taps_without_release_events() {
        let mut k = Keys::default();
        k.translate(press(KeyCode::Char('x'), KeyModifiers::NONE), false);
        let p = k.translate(press(KeyCode::Left, KeyModifiers::NONE), false);
        assert_eq!(p, Input::Press(Press::new(Button::Left)));
        assert_eq!(
            k.translate(press(KeyCode::Char('a'), KeyModifiers::NONE), false),
            Input::None
        );
    }
}
