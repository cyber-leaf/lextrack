//! `lextrack m8`: command line and tracker screen for playing M8 songs.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use super::format::{fx_name, note_name, Song, EMPTY, SONG_ROWS, STEPS, TRACKS};
use super::instruments::{InstrumentMap, PluginRef};
use super::rack::{load_slots, render_wav, Rack, Slot};
use crate::settings::AudioSettings;

const USAGE: &str = "\
lextrack m8: play Dirtywave M8 songs through CLAP plugins

USAGE:
    lextrack m8 SONG.m8s [OPTIONS]

Each M8 instrument the song uses is played by a CLAP plugin instance.
Choose the plugins with a map file, or give one plugin for everything.

OPTIONS:
    --map <FILE>          Instrument map (see README): lines like
                          `00 = ~/.clap/Surge XT.clap`, `fmsynth = ...`,
                          `default = ...`
    --plugin <PATH[@ID]>  Plugin for every instrument the map doesn't cover
    --inst <KEY=PATH[@ID]>  One map rule on the command line (repeatable)
    --row <HEX>           Song row to start from (default 00)
    --note-offset <N>     MIDI key for M8 note C-1 (default 24, so C-4 = 60)
    --info                Print the song and the plugin for each instrument
    --render <OUT.wav>    Render to a WAV file instead of playing live
    --seconds <S>         Length to render (default 30)
    --no-audio            Don't open a sound card
    --device <NAME>, --sample-rate <HZ>, --buffer <FRAMES>
                          Audio output, as for plain lextrack
    -h, --help            Show this help

KEYS:
    Up/Down  PgUp/PgDn    Move the song cursor     Left/Right  Select track
    Space                 Play from the cursor row, or stop
    q / Esc               Quit
";

struct Opts {
    song: PathBuf,
    map: Option<PathBuf>,
    plugin: Option<String>,
    inst: Vec<String>,
    row: usize,
    note_offset: i16,
    info: bool,
    render: Option<PathBuf>,
    seconds: f64,
    no_audio: bool,
    device: Option<Option<String>>,
    sample_rate: Option<Option<u32>>,
    buffer: Option<Option<u32>>,
}

fn parse(args: impl Iterator<Item = OsString>) -> Result<Opts> {
    let mut song = None;
    let mut o = Opts {
        song: PathBuf::new(),
        map: None,
        plugin: None,
        inst: vec![],
        row: 0,
        note_offset: 24,
        info: false,
        render: None,
        seconds: 30.0,
        no_audio: false,
        device: None,
        sample_rate: None,
        buffer: None,
    };
    let mut it = args;
    let value = |it: &mut dyn Iterator<Item = OsString>, flag: &str| -> Result<String> {
        Ok(it
            .next()
            .with_context(|| format!("{flag} needs a value"))?
            .to_string_lossy()
            .into_owned())
    };
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Some("--map") => o.map = Some(value(&mut it, "--map")?.into()),
            Some("--plugin") => o.plugin = Some(value(&mut it, "--plugin")?),
            Some("--inst") => o.inst.push(value(&mut it, "--inst")?),
            Some("--row") => {
                let v = value(&mut it, "--row")?;
                o.row = usize::from_str_radix(&v, 16)
                    .ok()
                    .filter(|&r| r < SONG_ROWS)
                    .with_context(|| format!("--row expects a hex row 00-FF, not \"{v}\""))?;
            }
            Some("--note-offset") => o.note_offset = value(&mut it, "--note-offset")?.parse()?,
            Some("--info") => o.info = true,
            Some("--render") => o.render = Some(value(&mut it, "--render")?.into()),
            Some("--seconds") => o.seconds = value(&mut it, "--seconds")?.parse()?,
            Some("--no-audio") => o.no_audio = true,
            Some("--device") => {
                let v = value(&mut it, "--device")?;
                o.device = Some((v != "default").then_some(v));
            }
            Some("--sample-rate") => {
                o.sample_rate = Some(number_or_default(&value(&mut it, "--sample-rate")?)?)
            }
            Some("--buffer") => o.buffer = Some(number_or_default(&value(&mut it, "--buffer")?)?),
            Some(s) if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            _ => song = Some(PathBuf::from(arg)),
        }
    }
    o.song = song.with_context(|| format!("which song?\n\n{USAGE}"))?;
    Ok(o)
}

fn number_or_default(v: &str) -> Result<Option<u32>> {
    if v == "default" {
        return Ok(None);
    }
    Ok(Some(v.parse().with_context(|| {
        format!("expected a number or \"default\", not \"{v}\"")
    })?))
}

pub fn run(args: impl Iterator<Item = OsString>) -> Result<()> {
    let o = parse(args)?;
    let song = Arc::new(Song::load(&o.song)?);

    let mut map = match &o.map {
        Some(p) => InstrumentMap::load(p)?,
        None => InstrumentMap::default(),
    };
    let cwd = Path::new(".");
    if let Some(p) = &o.plugin {
        map.set_default(PluginRef::parse(p, cwd)?);
    }
    for rule in &o.inst {
        let (k, v) = rule
            .split_once('=')
            .with_context(|| format!("--inst expects KEY=PLUGIN, not \"{rule}\""))?;
        map.set(k, v, cwd)?;
    }

    if o.info {
        print_info(&song, &map);
        return Ok(());
    }
    if map.is_empty() {
        bail!(
            "no plugins chosen. Use --plugin PATH.clap for every instrument, \
             or --map FILE (see lextrack m8 --help)"
        );
    }

    let (mut slots, missing) = load_slots(&song, &map);
    if slots.is_empty() {
        for (n, why) in &missing {
            eprintln!("instrument {n:02X}: {why}");
        }
        bail!("none of the song's instruments has a working plugin");
    }

    if let Some(out) = &o.render {
        for (n, why) in &missing {
            eprintln!("instrument {n:02X} will be silent: {why}");
        }
        let rate = o.sample_rate.flatten().unwrap_or(48_000);
        render_wav(
            song.clone(),
            &mut slots,
            o.row,
            o.seconds,
            rate,
            o.note_offset,
            out,
        )?;
        eprintln!(
            "rendered {:.1} s of \"{}\" with {} plugin(s) to {}",
            o.seconds,
            song.name,
            slots.len(),
            out.display()
        );
        return Ok(());
    }

    if !std::io::stdout().is_terminal() {
        bail!("lextrack m8 needs a terminal (use --info or --render for plain output)");
    }
    let mut audio = AudioSettings::load();
    if let Some(d) = o.device.clone() {
        audio.device = d;
    }
    if let Some(r) = o.sample_rate {
        audio.sample_rate = r;
    }
    if let Some(b) = o.buffer {
        audio.buffer_size = b;
    }
    // Enter the terminal first: it also routes console output from plugins
    // and the audio system to a log file, away from the screen.
    let tty = crate::TtyGuard::enter()?;
    let (mut rack, problem) =
        Rack::start(song.clone(), &mut slots, !o.no_audio, &audio, o.note_offset)?;
    let mut term = Terminal::new(CrosstermBackend::new(tty.writer()?))?;
    let mut ui = Tracker {
        song: song.clone(),
        cursor: o.row,
        track: 0,
        quit: false,
        meter: 0.0,
        problem,
        missing,
    };
    let r = ui.run(&mut term, &rack, &slots);
    rack.stop(&mut slots);
    drop(term);
    drop(tty);
    r
}

fn print_info(song: &Song, map: &InstrumentMap) {
    println!(
        "{} (M8 firmware {}), {} BPM",
        if song.name.is_empty() {
            "(untitled)"
        } else {
            &song.name
        },
        song.version,
        song.tempo
    );
    if song.version.major >= 3 {
        println!(
            "note: files from firmware 3 and later are read with the 2.x layout; \
             instrument details may be wrong"
        );
    }
    for t in 0..TRACKS {
        let rows: Vec<usize> = (0..SONG_ROWS)
            .filter(|&r| song.rows[r][t] != EMPTY)
            .collect();
        match (rows.first(), rows.last()) {
            (Some(a), Some(b)) => println!(
                "track {}: {} rows with chains, {a:02X}-{b:02X}",
                t + 1,
                rows.len()
            ),
            _ => println!("track {}: empty", t + 1),
        }
    }
    let used = song.used_instruments();
    if used.is_empty() {
        println!("no instrument plays a note");
    }
    for n in used {
        let inst = &song.instruments[n as usize];
        let plugin = map
            .resolve(n, inst)
            .map_or("(no plugin)".to_string(), |p| p.path.display().to_string());
        println!(
            "instrument {n:02X} {:<12} {:<10} -> {plugin}",
            inst.name,
            inst.kind.name()
        );
    }
}

struct Tracker {
    song: Arc<Song>,
    cursor: usize,
    track: usize,
    quit: bool,
    meter: f32,
    problem: Option<String>,
    missing: Vec<(u8, String)>,
}

impl Tracker {
    fn run<B: Backend>(
        &mut self,
        term: &mut Terminal<B>,
        rack: &Rack,
        slots: &[Slot],
    ) -> Result<()> {
        while !self.quit {
            for s in slots {
                if s.plugin
                    .shared
                    .callback_requested
                    .swap(false, Ordering::AcqRel)
                {
                    s.plugin.on_main_thread();
                }
            }
            self.meter = rack.shared.take_peak().max(self.meter * 0.85);
            term.draw(|f| self.draw(f, rack, slots))?;
            if event::poll(Duration::from_millis(30))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Release {
                        self.key(k.code, rack);
                    }
                }
            }
        }
        Ok(())
    }

    fn key(&mut self, code: KeyCode, rack: &Rack) {
        let last = SONG_ROWS - 1;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(16),
            KeyCode::PageDown => self.cursor = (self.cursor + 16).min(last),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = last,
            KeyCode::Left => self.track = self.track.saturating_sub(1),
            KeyCode::Right => self.track = (self.track + 1).min(TRACKS - 1),
            KeyCode::Char(' ') | KeyCode::Enter => {
                if rack.shared.is_playing() {
                    rack.shared.stop_song();
                } else {
                    rack.shared.play_from(self.cursor);
                }
            }
            _ => {}
        }
    }

    fn draw(&self, f: &mut Frame, rack: &Rack, slots: &[Slot]) {
        let inst_rows = (slots.len() + self.missing.len()).clamp(1, 8) as u16 + 2;
        let [header, body, insts, footer] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(inst_rows),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_header(f, header, rack);
        let [song_area, chain_area, phrase_area] = Layout::horizontal([
            Constraint::Length(4 + 3 * TRACKS as u16 + 2),
            Constraint::Length(14),
            Constraint::Min(30),
        ])
        .areas(body);
        self.draw_song(f, song_area, rack);
        self.draw_chain_and_phrase(f, chain_area, phrase_area, rack);
        self.draw_instruments(f, insts, slots);
        f.render_widget(
            Paragraph::new(" ↑↓ row  ←→ track  Space play/stop  q quit")
                .style(Style::default().fg(Color::DarkGray)),
            footer,
        );
    }

    fn draw_header(&self, f: &mut Frame, area: Rect, rack: &Rack) {
        let playing = rack.shared.is_playing();
        let state = if playing {
            Span::styled(
                " PLAYING ",
                Style::default().fg(Color::Black).bg(Color::Green),
            )
        } else {
            Span::styled(
                " STOPPED ",
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            )
        };
        let db = 20.0 * self.meter.max(1e-6).log10();
        let width = 20usize;
        let filled = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * width as f32) as usize;
        let mut line2 = vec![
            Span::raw(format!("{} · {} Hz   ", rack.output_name, rack.sample_rate)),
            Span::styled("█".repeat(filled), Style::default().fg(Color::Green)),
            Span::styled(
                "░".repeat(width - filled),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!(" {:>6.1} dB", db.max(-99.0))),
        ];
        if let Some(p) = &self.problem {
            line2.push(Span::styled(
                format!("   sound card failed: {p}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        let line2 = Line::from(line2);
        let title = format!(
            " {} · M8 {} · {:.0} BPM ",
            if self.song.name.is_empty() {
                "(untitled)"
            } else {
                &self.song.name
            },
            self.song.version,
            rack.shared.tempo()
        );
        f.render_widget(
            Paragraph::new(vec![Line::from(vec![state]), line2])
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn draw_song(&self, f: &mut Frame, area: Rect, rack: &Rack) {
        let visible = area.height.saturating_sub(3) as usize;
        let top = self
            .cursor
            .saturating_sub(visible / 2)
            .min(SONG_ROWS.saturating_sub(visible));
        let views: Vec<_> = (0..TRACKS).map(|t| rack.shared.track(t)).collect();
        let header: String = (1..=TRACKS).map(|t| format!("{t:<3}")).collect();
        let mut lines = vec![Line::styled(
            format!("    {header}"),
            Style::default().fg(Color::DarkGray),
        )];
        for row in top..(top + visible).min(SONG_ROWS) {
            let mut spans = vec![Span::styled(
                format!("{row:02X}  "),
                if row == self.cursor {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )];
            for (t, v) in views.iter().enumerate() {
                let c = self.song.rows[row][t];
                let text = if c == EMPTY {
                    "--".into()
                } else {
                    format!("{c:02X}")
                };
                let mut style = if c == EMPTY {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                if v.pos.is_some_and(|p| p.row == row) {
                    style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
                }
                if row == self.cursor && t == self.track {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                spans.push(Span::styled(text, style));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::from(spans));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" SONG ")),
            area,
        );
    }

    fn draw_chain_and_phrase(
        &self,
        f: &mut Frame,
        chain_area: Rect,
        phrase_area: Rect,
        rack: &Rack,
    ) {
        let t = self.track;
        let view = rack.shared.track(t);
        // Follow the playing position; when stopped, show the cursor row.
        let (chain, chain_step, phrase, phrase_step) = match view.pos {
            Some(p) => (
                Some(p.chain),
                Some(p.chain_step),
                Some(p.phrase),
                Some(p.phrase_step),
            ),
            None => {
                let c = self.song.rows[self.cursor][t];
                let chain = (c != EMPTY).then_some(c as usize);
                let phrase = chain.and_then(|c| {
                    self.song.chains[c]
                        .steps
                        .iter()
                        .map(|s| s.phrase)
                        .find(|&p| p != EMPTY)
                        .map(|p| p as usize)
                });
                (chain, None, phrase, None)
            }
        };
        let dim = Style::default().fg(Color::DarkGray);
        let now = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);

        let mut lines = Vec::new();
        if let Some(c) = chain {
            for (i, s) in self.song.chains[c].steps.iter().enumerate() {
                let text = if s.phrase == EMPTY {
                    format!("{i:X} -- --")
                } else {
                    format!("{i:X} {:02X} {:02X}", s.phrase, s.transpose)
                };
                let style = if Some(i) == chain_step {
                    now
                } else if s.phrase == EMPTY {
                    dim
                } else {
                    Style::default()
                };
                lines.push(Line::styled(text, style));
            }
        }
        let title = match chain {
            Some(c) => format!(" CHAIN {c:02X} "),
            None => " CHAIN ".into(),
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
            chain_area,
        );

        let mut lines = Vec::new();
        if let Some(p) = phrase {
            for (i, s) in self.song.phrases[p].steps.iter().enumerate().take(STEPS) {
                let hex = |b: u8| {
                    if b == EMPTY {
                        "--".to_string()
                    } else {
                        format!("{b:02X}")
                    }
                };
                let mut text = format!(
                    "{i:X} {} {} {}",
                    note_name(s.note),
                    hex(s.volume),
                    hex(s.instrument)
                );
                for fx in &s.fx {
                    if fx.is_empty() {
                        text.push_str(" --- --");
                    } else {
                        text.push_str(&format!(" {} {:02X}", fx_name(fx.command), fx.value));
                    }
                }
                let style = if Some(i) == phrase_step {
                    now
                } else if s.note == EMPTY {
                    dim
                } else {
                    Style::default()
                };
                lines.push(Line::styled(text, style));
            }
        }
        let sounding = match view.note {
            Some((note, inst)) => format!(" · {} on {inst:02X}", note_name(note)),
            None => String::new(),
        };
        let title = match phrase {
            Some(p) => format!(" PHRASE {p:02X} · track {}{sounding} ", t + 1),
            None => format!(" PHRASE · track {}{sounding} ", t + 1),
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
            phrase_area,
        );
    }

    fn draw_instruments(&self, f: &mut Frame, area: Rect, slots: &[Slot]) {
        let mut lines = Vec::new();
        for s in slots {
            let inst = &self.song.instruments[s.instrument as usize];
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:02X} ", s.instrument),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!("{:<12} {:<10} ", inst.name, inst.kind.name())),
                Span::styled(
                    format!("{} ({})", s.plugin.descriptor.name, s.source.label()),
                    Style::default().fg(Color::Cyan),
                ),
            ]));
        }
        for (n, why) in &self.missing {
            let inst = &self.song.instruments[*n as usize];
            lines.push(Line::from(vec![
                Span::styled(format!("{n:02X} "), Style::default().fg(Color::Yellow)),
                Span::raw(format!("{:<12} {:<10} ", inst.name, inst.kind.name())),
                Span::styled(format!("silent: {why}"), Style::default().fg(Color::Red)),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" INSTRUMENTS "),
            ),
            area,
        );
    }
}
