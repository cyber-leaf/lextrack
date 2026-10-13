//! The terminal interface: a plugin picker and the parameter editor.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap_sys::ext::params::{CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use ratatui::{Frame, Terminal};

use crate::engine::Engine;
use crate::notes::{self, Keyboard};
use crate::plugin::{
    HostEvent, NoteInput, ParamChange, ParamInfo, PluginDescriptor, PluginInstance,
};

const FRAME: Duration = Duration::from_millis(33);

// ---------------------------------------------------------------------------
// Plugin picker
// ---------------------------------------------------------------------------

/// Let the user pick one plugin. Returns `None` if they quit.
pub fn pick_plugin<B: Backend>(
    term: &mut Terminal<B>,
    plugins: &[PluginDescriptor],
) -> Result<Option<usize>> {
    let mut state = ListState::default().with_selected(Some(0));
    loop {
        term.draw(|f| {
            let [list_area, help] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
            let items: Vec<ListItem> = plugins
                .iter()
                .map(|p| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(p.name.clone(), Style::new().bold()),
                            Span::raw(format!("  {}  {}", p.vendor, p.version)).dark_gray(),
                        ]),
                        Line::from(format!(
                            "    {}  {}  {}",
                            p.features.join(", "),
                            p.id,
                            p.path.display()
                        ))
                        .dark_gray(),
                    ])
                })
                .collect();
            let list = List::new(items)
                .block(Block::bordered().title(" lextrack · choose a CLAP plugin "))
                .highlight_style(Style::new().bg(Color::Blue).fg(Color::White))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, list_area, &mut state);
            f.render_widget(
                Paragraph::new(" ↑↓ select   Enter load   q quit").dark_gray(),
                help,
            );
        })?;
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Release {
                continue;
            }
            let sel = state.selected().unwrap_or(0);
            match k.code {
                KeyCode::Up => state.select(Some(sel.saturating_sub(1))),
                KeyCode::Down => state.select(Some((sel + 1).min(plugins.len().saturating_sub(1)))),
                KeyCode::Enter => return Ok(Some(sel)),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None)
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parameter editor
// ---------------------------------------------------------------------------

struct ParamRow {
    info: ParamInfo,
    value: f64,
    text: String,
    /// While set, the UI shows the value it just sent instead of polling the
    /// plugin, which may not have processed the change yet.
    hold_until: Option<Instant>,
}

enum Mode {
    Normal,
    /// Typing a value for the focused parameter.
    Edit(String),
    /// Typing a filter string.
    Filter(String),
}

#[derive(Clone, Copy)]
enum StepSize {
    Fine,
    Normal,
    Coarse,
}

pub struct App {
    plugin: PluginInstance,
    engine: Option<Engine>,
    use_device: bool,
    rows: Vec<ParamRow>,
    /// Indexes into `rows` that pass the filter.
    visible: Vec<usize>,
    table: TableState,
    mode: Mode,
    filter: String,
    show_log: bool,
    status: Option<(String, Instant)>,
    meter: f32,
    keyboard: Keyboard,
    note_input: NoteInput,
    quit: bool,
}

impl App {
    pub fn new(mut plugin: PluginInstance, use_device: bool) -> Result<Self> {
        let engine = Engine::start(&mut plugin, use_device)?;
        let mut app = App {
            plugin,
            engine: Some(engine),
            use_device,
            rows: vec![],
            visible: vec![],
            table: TableState::default(),
            mode: Mode::Normal,
            filter: String::new(),
            show_log: false,
            status: None,
            meter: 0.0,
            keyboard: Keyboard::new(),
            note_input: NoteInput::None,
            quit: false,
        };
        app.note_input = app.plugin.note_input();
        app.reload_params();
        Ok(app)
    }

    pub fn run<B: Backend>(&mut self, term: &mut Terminal<B>) -> Result<()> {
        while !self.quit {
            self.service_host();
            self.refresh_values(term.size()?.height as usize);
            term.draw(|f| self.draw(f))?;
            if event::poll(FRAME)? {
                // Drain everything queued so held keys don't lag behind.
                loop {
                    if let Event::Key(k) = event::read()? {
                        if k.kind == KeyEventKind::Release {
                            self.on_key_release(k);
                        } else {
                            self.on_key(k);
                        }
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
        self.all_notes_off();
        if let Some(mut e) = self.engine.take() {
            // Give the audio thread a block to deliver the note-offs.
            std::thread::sleep(Duration::from_millis(30));
            e.stop(&mut self.plugin);
        }
    }

    // --- host requests ------------------------------------------------------

    fn service_host(&mut self) {
        let shared = self.plugin.shared.clone();
        if shared.callback_requested.swap(false, Ordering::AcqRel) {
            self.plugin.on_main_thread();
        }
        let rescan = shared.rescan_flags.swap(0, Ordering::AcqRel);
        if rescan & (CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_ALL) != 0 {
            self.reload_params();
        }
        for key in self.keyboard.expired(Instant::now()) {
            self.send_note(key, false);
        }
        if shared.restart_requested.swap(false, Ordering::AcqRel) {
            self.all_notes_off();
            if let Some(mut e) = self.engine.take() {
                e.stop(&mut self.plugin);
            }
            match Engine::start(&mut self.plugin, self.use_device) {
                Ok(e) => self.engine = Some(e),
                Err(err) => self.flash(format!("restart failed: {err:#}")),
            }
            self.note_input = self.plugin.note_input();
            self.reload_params();
        }
        if let Some(e) = &self.engine {
            let p = e.shared.take_peak();
            self.meter = p.max(self.meter * 0.85);
        }
    }

    fn reload_params(&mut self) {
        let selected_id = self.selected_row().map(|r| r.info.id);
        let old: HashMap<u32, (f64, String)> = self
            .rows
            .iter()
            .map(|r| (r.info.id, (r.value, r.text.clone())))
            .collect();
        self.rows = self
            .plugin
            .params()
            .into_iter()
            .filter(|p| !p.is_hidden())
            .map(|info| {
                let (value, text) = old
                    .get(&info.id)
                    .cloned()
                    .unwrap_or((info.default, String::new()));
                ParamRow {
                    info,
                    value,
                    text,
                    hold_until: None,
                }
            })
            .collect();
        for i in 0..self.rows.len() {
            self.poll_row(i);
        }
        self.apply_filter();
        if let Some(id) = selected_id {
            if let Some(pos) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].info.id == id)
            {
                self.table.select(Some(pos));
            }
        }
    }

    fn apply_filter(&mut self) {
        let selected_id = self.selected_row().map(|r| r.info.id);
        let f = self.filter.to_lowercase();
        self.visible = (0..self.rows.len())
            .filter(|&i| {
                f.is_empty()
                    || self.rows[i].info.name.to_lowercase().contains(&f)
                    || self.rows[i].info.module.to_lowercase().contains(&f)
            })
            .collect();
        // Keep the focused parameter focused if it is still listed.
        let keep = selected_id.and_then(|id| {
            self.visible
                .iter()
                .position(|&i| self.rows[i].info.id == id)
        });
        let sel = keep.unwrap_or_else(|| self.table.selected().unwrap_or(0));
        self.table.select(if self.visible.is_empty() {
            None
        } else {
            Some(sel.min(self.visible.len() - 1))
        });
    }

    fn poll_row(&mut self, i: usize) {
        let row = &mut self.rows[i];
        if row.hold_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        row.hold_until = None;
        if let Some(v) = self.plugin.param_value(row.info.id) {
            row.value = v;
        }
        row.text = self
            .plugin
            .value_to_text(row.info.id, row.value)
            .unwrap_or_else(|| format_number(row.value));
    }

    /// Re-read the values of the rows that are on screen.
    fn refresh_values(&mut self, screen_height: usize) {
        let start = self.table.offset();
        let end = (start + screen_height).min(self.visible.len());
        for k in start..end {
            self.poll_row(self.visible[k]);
        }
    }

    // --- input -----------------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Edit(mut s) => match k.code {
                KeyCode::Enter => self.commit_text(&s),
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    s.pop();
                    self.mode = Mode::Edit(s);
                }
                KeyCode::Char(c) => {
                    s.push(c);
                    self.mode = Mode::Edit(s);
                }
                _ => self.mode = Mode::Edit(s),
            },
            Mode::Filter(mut s) => {
                match k.code {
                    KeyCode::Enter => {}
                    KeyCode::Esc => s.clear(),
                    KeyCode::Backspace => {
                        s.pop();
                        self.mode = Mode::Filter(s.clone());
                    }
                    KeyCode::Char(c) => {
                        s.push(c);
                        self.mode = Mode::Filter(s.clone());
                    }
                    _ => self.mode = Mode::Filter(s.clone()),
                }
                self.filter = s;
                self.apply_filter();
            }
            Mode::Normal => self.on_normal_key(k),
        }
    }

    fn on_key_release(&mut self, k: KeyEvent) {
        if let KeyCode::Char(c) = k.code {
            if let Some(key) = self.keyboard.release(c) {
                self.send_note(key, false);
            }
        }
    }

    /// Note keys and octave keys. Returns true if the key was used.
    fn on_note_key(&mut self, k: KeyEvent) -> bool {
        if k.modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let KeyCode::Char(c) = k.code else {
            return false;
        };
        match c.to_ascii_lowercase() {
            'z' => self.keyboard.octave_down(),
            'x' => self.keyboard.octave_up(),
            c if notes::is_note_key(c) => {
                if self.note_input == NoteInput::None {
                    self.flash("this plugin doesn't take notes".into());
                } else if let Some(key) = self.keyboard.press(c, Instant::now()) {
                    self.send_note(key, true);
                }
            }
            _ => return false,
        }
        true
    }

    fn send_note(&mut self, key: i16, on: bool) {
        if let Some(e) = &self.engine {
            e.shared.send(HostEvent::Note {
                key,
                on,
                velocity: notes::VELOCITY,
            });
        }
    }

    fn all_notes_off(&mut self) {
        for key in self.keyboard.release_all() {
            self.send_note(key, false);
        }
    }

    fn on_normal_key(&mut self, k: KeyEvent) {
        if self.on_note_key(k) {
            return;
        }
        let size = if k.modifiers.contains(KeyModifiers::SHIFT) {
            StepSize::Fine
        } else if k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            StepSize::Coarse
        } else {
            StepSize::Normal
        };
        let n = self.visible.len();
        let sel = self.table.selected().unwrap_or(0);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.apply_filter();
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Up if n > 0 => self.table.select(Some(sel.saturating_sub(1))),
            KeyCode::Down if n > 0 => self.table.select(Some((sel + 1).min(n - 1))),
            KeyCode::PageUp if n > 0 => self.table.select(Some(sel.saturating_sub(10))),
            KeyCode::PageDown if n > 0 => self.table.select(Some((sel + 10).min(n - 1))),
            KeyCode::Left => self.nudge(-1.0, size),
            KeyCode::Right => self.nudge(1.0, size),
            KeyCode::Home => self.set_selected(|r| r.info.min),
            KeyCode::End => self.set_selected(|r| r.info.max),
            KeyCode::Delete | KeyCode::Backspace => self.set_selected(|r| r.info.default),
            KeyCode::Char(' ') => self.all_notes_off(),
            KeyCode::Enter => {
                if self.selected_row().is_some_and(|r| !r.info.is_readonly()) {
                    self.mode = Mode::Edit(String::new());
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Filter(self.filter.clone()),
            KeyCode::Tab => self.show_log = !self.show_log,
            _ => {}
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.table
            .selected()
            .and_then(|s| self.visible.get(s).copied())
    }

    fn selected_row(&self) -> Option<&ParamRow> {
        self.selected_index().map(|i| &self.rows[i])
    }

    fn nudge(&mut self, dir: f64, size: StepSize) {
        self.set_selected(|r| {
            let info = &r.info;
            let range = info.max - info.min;
            let next = if info.is_stepped() {
                let step = match size {
                    StepSize::Coarse => (range / 10.0).round().max(1.0),
                    _ => 1.0,
                };
                r.value.round() + dir * step
            } else {
                let frac = match size {
                    StepSize::Fine => 0.001,
                    StepSize::Normal => 0.01,
                    StepSize::Coarse => 0.1,
                };
                r.value + dir * frac * range
            };
            if info.is_periodic() && range > 0.0 {
                if next > info.max {
                    info.min + (next - info.max) - if info.is_stepped() { 1.0 } else { 0.0 }
                } else if next < info.min {
                    info.max - (info.min - next) + if info.is_stepped() { 1.0 } else { 0.0 }
                } else {
                    next
                }
            } else {
                next
            }
        });
    }

    fn set_selected(&mut self, f: impl FnOnce(&ParamRow) -> f64) {
        let Some(i) = self.selected_index() else {
            return;
        };
        let row = &self.rows[i];
        if row.info.is_readonly() {
            self.flash("this parameter is read-only".into());
            return;
        }
        let v = f(row).clamp(row.info.min, row.info.max);
        self.send_value(i, v);
    }

    fn commit_text(&mut self, text: &str) {
        let Some(i) = self.selected_index() else {
            return;
        };
        let id = self.rows[i].info.id;
        let parsed = self
            .plugin
            .text_to_value(id, text)
            .or_else(|| text.trim().parse::<f64>().ok());
        match parsed {
            Some(v) => {
                let info = &self.rows[i].info;
                let v = v.clamp(info.min, info.max);
                self.send_value(i, v);
            }
            None => self.flash(format!("could not understand \"{text}\"")),
        }
    }

    fn send_value(&mut self, i: usize, v: f64) {
        let change = ParamChange {
            id: self.rows[i].info.id,
            cookie: self.rows[i].info.cookie,
            value: v,
        };
        match &self.engine {
            Some(e) if self.plugin.is_active() => e.shared.send(HostEvent::Param(change)),
            _ => self.plugin.flush_inactive(&[change]),
        }
        let row = &mut self.rows[i];
        row.value = v;
        row.text = self
            .plugin
            .value_to_text(row.info.id, v)
            .unwrap_or_else(|| format_number(v));
        row.hold_until = Some(Instant::now() + Duration::from_millis(250));
    }

    fn flash(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    // --- drawing -----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let log_height = if self.show_log { 8 } else { 0 };
        let [header, body, log, footer] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(log_height),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_header(f, header);
        self.draw_params(f, body);
        if self.show_log {
            self.draw_log(f, log);
        }
        self.draw_footer(f, footer);

        if let Mode::Edit(s) = &self.mode {
            let name = self
                .selected_row()
                .map(|r| r.info.name.clone())
                .unwrap_or_default();
            let w = 50.min(f.area().width.saturating_sub(4));
            let area = Rect::new((f.area().width - w) / 2, f.area().height / 2 - 1, w, 3);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("{s}█")).block(
                    Block::bordered().title(format!(" {name}: type a value, Enter to set ")),
                ),
                area,
            );
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let d = &self.plugin.descriptor;
        let audio = match &self.engine {
            Some(e) if e.is_silent() => format!("no audio output · {} Hz", e.sample_rate),
            Some(e) => format!("{} · {} Hz", e.output_name, e.sample_rate),
            None => "stopped".into(),
        };
        let meter_w = 20usize;
        let db = 20.0 * self.meter.max(1e-6).log10();
        let fill = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * meter_w as f32) as usize;
        let meter_color = if self.meter >= 1.0 {
            Color::Red
        } else {
            Color::Green
        };
        let line2 = Line::from(vec![
            Span::raw(format!(" {audio}   out ")),
            Span::styled("█".repeat(fill), Style::new().fg(meter_color)),
            Span::styled("░".repeat(meter_w - fill), Style::new().dark_gray()),
            Span::raw(if self.meter > 1e-5 {
                format!(" {db:6.1} dB")
            } else {
                "   -inf dB".into()
            }),
        ]);
        let title = Line::from(vec![
            Span::styled(format!(" {} ", d.name), Style::new().bold()),
            Span::raw(format!("{} {} ", d.vendor, d.version)).dark_gray(),
        ]);
        let playing: Vec<String> = self
            .keyboard
            .sounding()
            .into_iter()
            .map(notes::note_name)
            .collect();
        let line3 = if self.note_input == NoteInput::None {
            Line::from(" no note input on this plugin").dark_gray()
        } else {
            Line::from(vec![
                Span::raw(format!(" keys {}  ", self.keyboard.range())),
                Span::raw(if notes::RELEASE_EVENTS.load(Ordering::Relaxed) {
                    "z/x octave  space all off   "
                } else {
                    "z/x octave  space all off  (timed notes)   "
                })
                .dark_gray(),
                Span::styled(playing.join(" "), Style::new().fg(Color::Magenta).bold()),
            ])
        };
        f.render_widget(
            Paragraph::new(vec![line2, line3])
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn draw_params(&mut self, f: &mut Frame, area: Rect) {
        let name_w = self
            .visible
            .iter()
            .map(|&i| display_name(&self.rows[i].info).chars().count())
            .max()
            .unwrap_or(10)
            .clamp(10, 40) as u16;
        let value_w = 16u16;
        let bar_w = area.width.saturating_sub(name_w + value_w + 8).max(5);
        let rows: Vec<Row> = self
            .visible
            .iter()
            .map(|&i| {
                let r = &self.rows[i];
                let ro = r.info.is_readonly();
                let range = r.info.max - r.info.min;
                let frac = if range > 0.0 {
                    (r.value - r.info.min) / range
                } else {
                    0.0
                };
                let style = if ro {
                    Style::new().dark_gray()
                } else {
                    Style::new()
                };
                Row::new(vec![
                    Cell::from(display_name(&r.info)),
                    Cell::from(r.text.clone()).style(Style::new().fg(Color::Cyan)),
                    Cell::from(bar(frac, bar_w as usize, r.info.is_stepped())),
                ])
                .style(style)
            })
            .collect();
        let count = if self.filter.is_empty() {
            format!(" {} parameters ", self.rows.len())
        } else {
            format!(
                " {} of {} parameters match \"{}\" ",
                self.visible.len(),
                self.rows.len(),
                self.filter
            )
        };
        let empty = self.rows.is_empty();
        let table = Table::new(
            rows,
            [
                Constraint::Length(name_w),
                Constraint::Length(value_w),
                Constraint::Min(5),
            ],
        )
        .block(Block::bordered().title(count))
        .row_highlight_style(
            Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
        f.render_stateful_widget(table, area, &mut self.table);
        if empty {
            let msg = if self.plugin.has_params() {
                "This plugin has no visible parameters."
            } else {
                "This plugin does not expose parameters (no clap.params extension)."
            };
            let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), 1);
            f.render_widget(Paragraph::new(msg).dark_gray(), inner);
        }
    }

    fn draw_log(&self, f: &mut Frame, area: Rect) {
        let lines: Vec<Line> = match self.plugin.shared.log.lock() {
            Ok(l) => l
                .iter()
                .rev()
                .take(area.height as usize)
                .rev()
                .map(|s| Line::from(s.clone()))
                .collect(),
            Err(_) => vec![],
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" plugin log ")),
            area,
        );
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        if let Some((msg, t)) = &self.status {
            if t.elapsed() < Duration::from_secs(3) {
                f.render_widget(Paragraph::new(format!(" {msg}")).yellow(), area);
                return;
            }
            self.status = None;
        }
        let text = match &self.mode {
            Mode::Filter(s) => format!(" filter: {s}█   (Enter keep, Esc clear)"),
            _ => " ↑↓ select  ←→ change  Shift fine  Ctrl coarse  Home/End min/max  Del default  Enter type  / filter  Tab log  q quit".into(),
        };
        f.render_widget(Paragraph::new(text).dark_gray(), area);
    }
}

fn display_name(p: &ParamInfo) -> String {
    let mut name = if p.module.is_empty() {
        p.name.clone()
    } else {
        format!("{} / {}", p.module, p.name)
    };
    if p.is_bypass() {
        name.push_str(" [bypass]");
    }
    if p.is_readonly() {
        name.push_str(" [ro]");
    }
    name
}

fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e9 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

fn bar(frac: f64, width: usize, stepped: bool) -> Line<'static> {
    let frac = frac.clamp(0.0, 1.0);
    if stepped {
        // A position marker reads better than a fill for choices.
        let pos = ((width.saturating_sub(1)) as f64 * frac).round() as usize;
        let s: String = (0..width)
            .map(|i| if i == pos { '●' } else { '─' })
            .collect();
        return Line::from(s);
    }
    const PARTS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let total = frac * width as f64;
    let full = total.floor() as usize;
    let mut s = "█".repeat(full.min(width));
    if full < width {
        let part = ((total - full as f64) * 8.0) as usize;
        if part > 0 {
            s.push(PARTS[part - 1]);
        }
    }
    let used = s.chars().count();
    Line::from(vec![
        Span::raw(s),
        Span::styled("·".repeat(width - used), Style::new().dark_gray()),
    ])
}

/// Print every parameter and its value; used by `--dump`.
pub fn dump(plugin: &PluginInstance) {
    let d = &plugin.descriptor;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{} ({}) by {} {}", d.name, d.id, d.vendor, d.version);
    let (ins, outs) = plugin.audio_ports();
    let _ = writeln!(out, "audio inputs {ins:?}, outputs {outs:?}");
    let params = plugin.params();
    let _ = writeln!(out, "{} parameters", params.len());
    for p in params {
        let v = plugin.param_value(p.id).unwrap_or(p.default);
        let text = plugin
            .value_to_text(p.id, v)
            .unwrap_or_else(|| format_number(v));
        let mut flags = Vec::new();
        if p.is_stepped() {
            flags.push("stepped");
        }
        if p.is_hidden() {
            flags.push("hidden");
        }
        if p.is_readonly() {
            flags.push("read-only");
        }
        if p.is_bypass() {
            flags.push("bypass");
        }
        let _ = writeln!(
            out,
            "  [{:>5}] {:<32} = {:<16} ({} .. {}, default {}) {}",
            p.id,
            display_name(&p),
            text,
            format_number(p.min),
            format_number(p.max),
            format_number(p.default),
            flags.join(",")
        );
    }
}
