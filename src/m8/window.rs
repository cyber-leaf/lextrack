//! The tracker in its own window: the M8's 320x240 screen drawn with a pixel
//! font, scaled to fit, with the eight buttons drawn underneath and lit while
//! they're held.
//!
//! A window gets every key press and release, so A, Z, X and S can be held
//! like the M8's buttons, and Shift / Alt / Ctrl work with arrows the same
//! way they do in a terminal. Keys are passed on to the tracker spelled as
//! terminal keys, so both UIs share one keyboard map. Letter buttons go by
//! where the key is, not by keyboard layout.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use font8x8::legacy::BASIC_LEGACY;
use ratatui::crossterm::event::{
    KeyCode as TermKey, KeyEvent as TermEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

use super::input::{Button, RELEASE_EVENTS};
use super::screen::{Role, Screen, COLS, ROWS};
use super::ui::App;

/// Pixels per character cell: the M8 screen is 320x240.
const CELL_W: usize = 8;
const CELL_H: usize = 10;
const SCREEN_W: usize = COLS * CELL_W;
const SCREEN_H: usize = ROWS * CELL_H;
/// The button strip under the screen.
const STRIP_H: usize = 44;
const W: usize = SCREEN_W;
const H: usize = SCREEN_H + STRIP_H;
const FRAME: Duration = Duration::from_millis(16);

/// Make the event loop first: it fails when there is no display to open a
/// window on, before anything else has started.
pub fn event_loop() -> Result<EventLoop<()>> {
    // winit panics when a system library it needs is missing (e.g.
    // libxkbcommon-x11); treat that like having no display.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(EventLoop::new);
    std::panic::set_hook(hook);
    match r {
        Ok(r) => Ok(r?),
        Err(p) => {
            let why = p
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| p.downcast_ref::<&str>().copied())
                .unwrap_or("the window system failed to start");
            anyhow::bail!("{why}")
        }
    }
}

/// Run the tracker in a window until it's closed or the user quits.
pub fn run(event_loop: EventLoop<()>, app: &mut App) -> Result<()> {
    RELEASE_EVENTS.store(true, Ordering::Relaxed);
    let mut w = Win {
        app,
        window: None,
        surface: None,
        pixels: vec![0; W * H],
        down: HashSet::new(),
        mods: ModifiersState::empty(),
        next_frame: Instant::now(),
        error: None,
    };
    event_loop.run_app(&mut w)?;
    match w.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

struct Win<'a> {
    app: &'a mut App,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    /// The unscaled picture, W x H.
    pixels: Vec<u32>,
    /// Physical keys held down.
    down: HashSet<KeyCode>,
    mods: ModifiersState,
    next_frame: Instant,
    error: Option<anyhow::Error>,
}

impl ApplicationHandler for Win<'_> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.open(el) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.keyboard(event),
            // Keys released while another window has focus never arrive.
            WindowEvent::Focused(false) => self.release_all(),
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw() {
                    self.error = Some(e);
                    el.exit();
                }
            }
            _ => {}
        }
        if self.app.wants_quit() {
            el.exit();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = now + FRAME;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

impl Win<'_> {
    fn open(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title("lextrack")
            .with_inner_size(LogicalSize::new((W * 3) as f64, (H * 3) as f64))
            .with_min_inner_size(LogicalSize::new(W as f64, H as f64));
        let window = Rc::new(el.create_window(attrs)?);
        let context = Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("can't draw in the window")?;
        let surface = Surface::new(&context, window.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("can't draw in the window")?;
        self.window = Some(window);
        self.surface = Some(surface);
        Ok(())
    }

    fn keyboard(&mut self, e: KeyEvent) {
        let pressed = e.state == ElementState::Pressed;
        if let PhysicalKey::Code(code) = e.physical_key {
            if pressed {
                self.down.insert(code);
            } else {
                self.down.remove(&code);
            }
        }
        let kind = match (pressed, e.repeat) {
            (false, _) => KeyEventKind::Release,
            (true, true) => KeyEventKind::Repeat,
            (true, false) => KeyEventKind::Press,
        };
        if let Some(code) = term_key(&e, self.mods.shift_key()) {
            self.app.key_event(TermEvent {
                code,
                modifiers: term_mods(self.mods),
                kind,
                state: KeyEventState::NONE,
            });
        }
    }

    fn release_all(&mut self) {
        for c in ['a', 's', 'z', 'x'] {
            self.app.key_event(TermEvent {
                code: TermKey::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Release,
                state: KeyEventState::NONE,
            });
        }
        self.down.clear();
        self.mods = ModifiersState::empty();
    }

    /// Whether an M8 button is held, by any of its keys.
    fn held(&self, b: Button) -> bool {
        let d = |k: KeyCode| self.down.contains(&k);
        match b {
            Button::Up => d(KeyCode::ArrowUp),
            Button::Down => d(KeyCode::ArrowDown),
            Button::Left => d(KeyCode::ArrowLeft),
            Button::Right => d(KeyCode::ArrowRight),
            Button::Shift => d(KeyCode::KeyA) || self.mods.shift_key(),
            Button::Play => d(KeyCode::KeyS) || d(KeyCode::Space),
            Button::Option => {
                d(KeyCode::KeyZ) || d(KeyCode::Escape) || d(KeyCode::Delete) || self.mods.alt_key()
            }
            Button::Edit => {
                d(KeyCode::KeyX)
                    || d(KeyCode::Enter)
                    || d(KeyCode::Delete)
                    || d(KeyCode::Backspace)
                    || self.mods.control_key()
            }
        }
    }

    fn redraw(&mut self) -> Result<()> {
        let screen = self.app.frame();
        draw_screen(&screen, &mut self.pixels);
        let lit: Vec<(Button, bool)> = BUTTONS.iter().map(|b| (b.0, self.held(b.0))).collect();
        draw_buttons(&lit, &mut self.pixels);

        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        surface
            .resize(w, h)
            .map_err(|e| anyhow::anyhow!("can't draw in the window: {e}"))?;
        let mut buf = surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("can't draw in the window: {e}"))?;
        scale_into(
            &self.pixels,
            &mut buf,
            size.width as usize,
            size.height as usize,
        );
        buf.present()
            .map_err(|e| anyhow::anyhow!("can't draw in the window: {e}"))?;
        Ok(())
    }
}

/// The terminal key a window key stands for.
fn term_key(e: &KeyEvent, shift: bool) -> Option<TermKey> {
    let letter = |c: char| {
        Some(TermKey::Char(if shift {
            c.to_ascii_uppercase()
        } else {
            c
        }))
    };
    // The button letters and Ctrl+Q go by key position, so they're in the
    // same place on every keyboard layout.
    if let PhysicalKey::Code(code) = e.physical_key {
        match code {
            KeyCode::KeyA => return letter('a'),
            KeyCode::KeyS => return letter('s'),
            KeyCode::KeyZ => return letter('z'),
            KeyCode::KeyX => return letter('x'),
            KeyCode::KeyQ => return letter('q'),
            KeyCode::KeyC => return letter('c'),
            _ => {}
        }
    }
    Some(match &e.logical_key {
        Key::Named(NamedKey::ArrowUp) => TermKey::Up,
        Key::Named(NamedKey::ArrowDown) => TermKey::Down,
        Key::Named(NamedKey::ArrowLeft) => TermKey::Left,
        Key::Named(NamedKey::ArrowRight) => TermKey::Right,
        Key::Named(NamedKey::Enter) => TermKey::Enter,
        Key::Named(NamedKey::Escape) => TermKey::Esc,
        Key::Named(NamedKey::Delete) => TermKey::Delete,
        Key::Named(NamedKey::Backspace) => TermKey::Backspace,
        Key::Named(NamedKey::Space) => TermKey::Char(' '),
        Key::Character(s) => TermKey::Char(s.chars().next()?),
        _ => return None,
    })
}

fn term_mods(m: ModifiersState) -> KeyModifiers {
    let mut k = KeyModifiers::NONE;
    if m.shift_key() {
        k |= KeyModifiers::SHIFT;
    }
    if m.alt_key() {
        k |= KeyModifiers::ALT;
    }
    if m.control_key() {
        k |= KeyModifiers::CONTROL;
    }
    k
}

fn rgb(role: Role) -> u32 {
    let (r, g, b) = role.rgb();
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Draw `ch` with its top-left corner at (x, y). Characters the font
/// doesn't have are left blank.
fn glyph(px: &mut [u32], x: usize, y: usize, ch: char, color: u32) {
    let Some(rows) = BASIC_LEGACY.get(ch as usize) else {
        return;
    };
    for (dy, bits) in rows.iter().enumerate() {
        for dx in 0..8 {
            if bits & (1 << dx) != 0 {
                px[(y + dy) * W + x + dx] = color;
            }
        }
    }
}

fn text(px: &mut [u32], x: usize, y: usize, s: &str, color: u32) {
    for (i, ch) in s.chars().enumerate() {
        glyph(px, x + i * CELL_W, y, ch, color);
    }
}

fn fill(px: &mut [u32], x: usize, y: usize, w: usize, h: usize, color: u32) {
    for row in px.chunks_mut(W).skip(y).take(h) {
        for p in row.iter_mut().skip(x).take(w) {
            *p = color;
        }
    }
}

fn draw_screen(s: &Screen, px: &mut [u32]) {
    for y in 0..ROWS {
        for x in 0..COLS {
            let (ch, fg, bg) = s.cell(x, y);
            let (x0, y0) = (x * CELL_W, y * CELL_H);
            fill(px, x0, y0, CELL_W, CELL_H, rgb(bg));
            glyph(px, x0, y0 + 1, ch, rgb(fg));
        }
    }
}

/// The M8's buttons as they sit on the device: the arrows on the left, SHIFT
/// PLAY over OPTION EDIT on the right. Positions are in the strip.
const BUTTONS: [(Button, &str, usize, usize, usize); 8] = [
    (Button::Up, "^", 76, 6, 24),
    (Button::Left, "<", 48, 24, 24),
    (Button::Down, "v", 76, 24, 24),
    (Button::Right, ">", 104, 24, 24),
    (Button::Shift, "SHIFT", 160, 6, 56),
    (Button::Play, "PLAY", 220, 6, 56),
    (Button::Option, "OPTION", 160, 24, 56),
    (Button::Edit, "EDIT", 220, 24, 56),
];

fn draw_buttons(lit: &[(Button, bool)], px: &mut [u32]) {
    let top = SCREEN_H;
    fill(px, 0, top, W, STRIP_H, rgb(Role::Background));
    fill(px, 0, top, W, 1, rgb(Role::Empty));
    for &(b, label, x, y, w) in &BUTTONS {
        let on = lit.iter().any(|&(l, on)| l == b && on);
        let (face, ink) = if on {
            (rgb(Role::Cursor), rgb(Role::Background))
        } else {
            (rgb(Role::Empty), rgb(Role::Info))
        };
        let (y, h) = (top + y, 14);
        fill(px, x, y, w, h, face);
        let tx = x + (w - label.len() * CELL_W) / 2;
        text(px, tx, y + 3, label, ink);
    }
}

/// Copy the picture into the window at the largest whole-number scale that
/// fits (or shrunk, if the window is smaller), centred on black.
fn scale_into(src: &[u32], dst: &mut [u32], dw: usize, dh: usize) {
    let bg = rgb(Role::Background);
    let fit = (dw as f64 / W as f64).min(dh as f64 / H as f64);
    let scale = if fit >= 1.0 { fit.floor() } else { fit };
    let (sw, sh) = ((W as f64 * scale) as usize, (H as f64 * scale) as usize);
    let (ox, oy) = ((dw - sw.min(dw)) / 2, (dh - sh.min(dh)) / 2);
    for (y, row) in dst.chunks_mut(dw).take(dh).enumerate() {
        if y < oy || y >= oy + sh {
            row.fill(bg);
            continue;
        }
        let sy = (((y - oy) as f64 / scale) as usize).min(H - 1);
        let src_row = &src[sy * W..(sy + 1) * W];
        for (x, p) in row.iter_mut().enumerate() {
            *p = if x < ox || x >= ox + sw {
                bg
            } else {
                src_row[(((x - ox) as f64 / scale) as usize).min(W - 1)]
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_fit_in_the_strip() {
        for &(_, label, x, y, w) in &BUTTONS {
            assert!(x + w <= W);
            assert!(y + 14 <= STRIP_H);
            assert!(label.len() * CELL_W <= w);
        }
    }

    #[test]
    fn scales_by_whole_numbers() {
        let mut src = vec![0; W * H];
        src[0] = 0xffffff;
        let (dw, dh) = (W * 2 + 10, H * 2);
        let mut dst = vec![1; dw * dh];
        scale_into(&src, &mut dst, dw, dh);
        assert_eq!(dst[5], 0xffffff);
        assert_eq!(dst[6], 0xffffff);
        assert_eq!(dst[7], 0);
        assert_eq!(dst[4], rgb(Role::Background));
    }
}
