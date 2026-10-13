//! A fixed 40x24 character screen with the M8's colour roles, drawn into
//! the middle of the terminal. Views write text at M8-style grid positions;
//! the screen takes care of scaling nothing and centring everything.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

pub const COLS: usize = 40;
pub const ROWS: usize = 24;

/// The M8's theme colour roles. The values are the M8's default theme
/// (m8-js test file DEFAULT.m8t).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Background,
    Empty,
    Info,
    Default,
    Value,
    Title,
    PlayMarker,
    Cursor,
    Selection,
    MeterLow,
    MeterMid,
    MeterPeak,
}

impl Role {
    pub fn color(self) -> Color {
        let (r, g, b) = match self {
            Role::Background => (0, 0, 0),
            Role::Empty => (30, 30, 40),
            Role::Info => (96, 96, 142),
            Role::Default => (140, 140, 186),
            Role::Value => (250, 250, 250),
            Role::Title => (50, 236, 255),
            Role::PlayMarker => (0, 255, 112),
            Role::Cursor => (50, 236, 255),
            Role::Selection => (255, 0, 210),
            Role::MeterLow => (0, 255, 80),
            Role::MeterMid => (255, 224, 0),
            Role::MeterPeak => (255, 48, 112),
        };
        Color::Rgb(r, g, b)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    fg: Role,
    bg: Role,
}

pub struct Screen {
    cells: [[Cell; COLS]; ROWS],
}

impl Screen {
    pub fn new() -> Self {
        Screen {
            cells: [[Cell {
                ch: ' ',
                fg: Role::Default,
                bg: Role::Background,
            }; COLS]; ROWS],
        }
    }

    /// Write `text` at column `x`, row `y`, clipped to the screen.
    pub fn text(&mut self, x: usize, y: usize, text: &str, fg: Role) {
        if y >= ROWS {
            return;
        }
        for (i, ch) in text.chars().enumerate() {
            if let Some(c) = self.cells[y].get_mut(x + i) {
                c.ch = ch;
                c.fg = fg;
            }
        }
    }

    /// Paint the background of `width` cells, e.g. for the cursor.
    pub fn highlight(&mut self, x: usize, y: usize, width: usize, bg: Role) {
        if y >= ROWS {
            return;
        }
        for c in self.cells[y].iter_mut().skip(x).take(width) {
            c.bg = bg;
            if bg != Role::Background {
                c.fg = Role::Background;
            }
        }
    }

    /// The text on row `y`, for tests.
    #[cfg(test)]
    pub fn row(&self, y: usize) -> String {
        self.cells[y].iter().map(|c| c.ch).collect()
    }

    /// Draw centred in `area`, filling the rest with the background.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let bg = Style::default().bg(Role::Background.color());
        buf.set_style(area, bg);
        let x0 = area.x + area.width.saturating_sub(COLS as u16) / 2;
        let y0 = area.y + area.height.saturating_sub(ROWS as u16) / 2;
        for (y, row) in self.cells.iter().enumerate() {
            let sy = y0 + y as u16;
            if sy >= area.y + area.height {
                break;
            }
            for (x, c) in row.iter().enumerate() {
                let sx = x0 + x as u16;
                if sx >= area.x + area.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((sx, sy)) {
                    cell.set_char(c.ch);
                    cell.set_style(Style::default().fg(c.fg.color()).bg(c.bg.color()));
                }
            }
        }
    }
}
