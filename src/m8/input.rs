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
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::screen::{Role, COLS, ROWS};

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
    /// In the order the button strip shows them.
    pub const ALL: [Button; 8] = [
        Button::Left,
        Button::Up,
        Button::Down,
        Button::Right,
        Button::Shift,
        Button::Play,
        Button::Option,
        Button::Edit,
    ];

    fn bit(self) -> u8 {
        1 << Button::ALL.iter().position(|&b| b == self).unwrap_or(0)
    }

    fn label(self) -> &'static str {
        match self {
            Button::Up => "↑",
            Button::Down => "↓",
            Button::Left => "←",
            Button::Right => "→",
            Button::Shift => "SHIFT",
            Button::Play => "PLAY",
            Button::Option => "OPTION",
            Button::Edit => "EDIT",
        }
    }

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
                let b = match c.to_ascii_lowercase() {
                    'a' => Button::Shift,
                    's' => Button::Play,
                    'z' => Button::Option,
                    'x' => Button::Edit,
                    _ => return Input::None,
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

/// How long a button stays lit after a key when the terminal can't say
/// when the key was let go. Longer than the usual key-repeat delay, so a
/// held key stays lit while it repeats.
const LIT_FOR: Duration = Duration::from_millis(600);

/// A strip of the eight M8 buttons drawn beside the screen, lighting those
/// the keys being pressed stand for (modifiers included), plus the name of
/// the last key the terminal sent. A debugging aid: it shows what the
/// app thinks you're pressing.
///
/// With key-release events (kitty protocol) a button stays lit until its
/// key is released. Otherwise it stays lit for `LIT_FOR` after each press
/// or repeat.
#[derive(Default)]
pub struct Lights {
    /// The key, the buttons it lights, and when they go out (`None`: on
    /// release).
    down: Vec<(KeyCode, u8, Option<Instant>)>,
    last: String,
}

impl Lights {
    pub fn key(&mut self, k: KeyEvent, releases: bool, now: Instant) {
        let code = normalize(k.code);
        self.down
            .retain(|&(c, _, until)| c != code && until.map_or(true, |u| u > now));
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.last = key_name(k);
        let lit = buttons_for(k);
        if lit != 0 {
            let until = if releases { None } else { Some(now + LIT_FOR) };
            self.down.push((code, lit, until));
        }
    }

    /// The buttons lit at `now`.
    pub fn lit(&self, now: Instant) -> u8 {
        self.down
            .iter()
            .filter(|(_, _, until)| until.map_or(true, |u| u > now))
            .fold(0, |m, &(_, b, _)| m | b)
    }

    /// Draw the strip under the centred 40x24 screen, or to its right when
    /// the terminal has no row to spare. Nothing is drawn when neither fits.
    pub fn render(&self, area: Rect, buf: &mut Buffer, now: Instant) {
        let lit = self.lit(now);
        let x0 = area.x + area.width.saturating_sub(COLS as u16) / 2;
        let y0 = area.y + area.height.saturating_sub(ROWS as u16) / 2;
        let below = y0 + ROWS as u16;
        let right = x0 + COLS as u16 + 2;
        let end_x = area.x + area.width;
        let end_y = area.y + area.height;
        let mut put = |x: u16, y: u16, text: &str, role: Role, on: bool| {
            if y >= end_y {
                return x;
            }
            let style = if on {
                Style::default()
                    .fg(Role::Background.color())
                    .bg(Role::Cursor.color())
            } else {
                Style::default()
                    .fg(role.color())
                    .bg(Role::Background.color())
            };
            let mut x = x;
            for ch in text.chars() {
                if x >= end_x {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_style(style);
                }
                x += 1;
            }
            x
        };
        let key = format!("KEY {}", self.last);
        if below < end_y {
            // One row of " ← " " SHIFT " ..., centred under the screen.
            let width: usize = Button::ALL
                .iter()
                .map(|b| b.label().chars().count() + 2)
                .sum();
            let mut x = area.x + area.width.saturating_sub(width as u16) / 2;
            for b in Button::ALL {
                x = put(
                    x,
                    below,
                    &format!(" {} ", b.label()),
                    Role::Info,
                    lit & b.bit() != 0,
                );
            }
            if below + 1 < end_y && !self.last.is_empty() {
                put(x0, below + 1, &key, Role::Info, false);
            }
        } else if right + 6 <= end_x {
            for (i, b) in Button::ALL.into_iter().enumerate() {
                let label = format!(" {:<6} ", b.label());
                put(
                    right,
                    y0 + 1 + i as u16,
                    &label,
                    Role::Info,
                    lit & b.bit() != 0,
                );
            }
            if !self.last.is_empty() {
                put(right + 1, y0 + 10, "KEY", Role::Info, false);
                put(right + 1, y0 + 11, &self.last, Role::Info, false);
            }
        }
    }
}

/// Letters match whatever their case, so a release finds its press.
fn normalize(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        c => c,
    }
}

/// The buttons a key stands for, as bits of `Button::bit`, using the same
/// spellings as `Keys::translate`.
fn buttons_for(k: KeyEvent) -> u8 {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let mut m = 0;
    match k.code {
        KeyCode::Up => m |= Button::Up.bit(),
        KeyCode::Down => m |= Button::Down.bit(),
        KeyCode::Left => m |= Button::Left.bit(),
        KeyCode::Right => m |= Button::Right.bit(),
        KeyCode::Enter => m |= Button::Edit.bit(),
        KeyCode::Esc => m |= Button::Option.bit(),
        KeyCode::Delete | KeyCode::Backspace => m |= Button::Option.bit() | Button::Edit.bit(),
        KeyCode::Char(' ') => m |= Button::Play.bit(),
        KeyCode::Char(c) => {
            if ctrl && matches!(c.to_ascii_lowercase(), 'q' | 'c') {
                return 0;
            }
            m |= match c.to_ascii_lowercase() {
                'a' => Button::Shift.bit(),
                's' => Button::Play.bit(),
                'z' => Button::Option.bit(),
                'x' => Button::Edit.bit(),
                _ => return 0,
            };
            if c.is_ascii_uppercase() {
                m |= Button::Shift.bit();
            }
        }
        _ => return 0,
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) {
        m |= Button::Shift.bit();
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        m |= Button::Option.bit();
    }
    if ctrl {
        m |= Button::Edit.bit();
    }
    m
}

/// A key as the terminal sent it, e.g. "CTRL+RIGHT" or "SHIFT+Z".
fn key_name(k: KeyEvent) -> String {
    let mut s = String::new();
    let shifted = matches!(k.code, KeyCode::Char(c) if c.is_ascii_uppercase());
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        s += "CTRL+";
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        s += "ALT+";
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) || shifted {
        s += "SHIFT+";
    }
    let name = match k.code {
        KeyCode::Char(' ') => "SPACE".to_string(),
        KeyCode::Char(c) => c.to_uppercase().to_string(),
        KeyCode::Enter => "ENTER".to_string(),
        KeyCode::Esc => "ESC".to_string(),
        KeyCode::Delete => "DEL".to_string(),
        KeyCode::Backspace => "BKSP".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        c => format!("{c:?}").to_uppercase(),
    };
    s + &name
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
    fn lights_follow_keys_and_modifiers() {
        let t = Instant::now();
        let mut l = Lights::default();
        l.key(press(KeyCode::Right, KeyModifiers::CONTROL), false, t);
        assert_eq!(l.lit(t), Button::Right.bit() | Button::Edit.bit());
        assert_eq!(l.last, "CTRL+RIGHT");
        // Without release events they go out after a moment.
        assert_eq!(l.lit(t + LIT_FOR), 0);

        // With them, a held letter stays lit until it is released.
        let mut l = Lights::default();
        l.key(press(KeyCode::Char('x'), KeyModifiers::NONE), true, t);
        l.key(press(KeyCode::Left, KeyModifiers::NONE), true, t);
        let later = t + Duration::from_secs(5);
        assert_eq!(l.lit(later), Button::Edit.bit() | Button::Left.bit());
        let up = key(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Release);
        l.key(up, true, later);
        assert_eq!(l.lit(later), Button::Edit.bit());
        let up = key(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        l.key(up, true, later);
        assert_eq!(l.lit(later), 0);

        let mut l = Lights::default();
        l.key(press(KeyCode::Char('Z'), KeyModifiers::SHIFT), false, t);
        assert_eq!(l.lit(t), Button::Option.bit() | Button::Shift.bit());
        assert_eq!(l.last, "SHIFT+Z");
        l.key(press(KeyCode::Delete, KeyModifiers::NONE), false, t);
        assert_eq!(
            l.lit(t),
            Button::Option.bit() | Button::Shift.bit() | Button::Edit.bit()
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
