//! The command line: open a song in the tracker, or print or render it.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use super::format::{Song, EMPTY, SONG_ROWS, TRACKS};
use super::instruments::{InstrumentMap, PluginRef};
use super::rack::{load_slots, render_wav};
use super::sampler::{SampleBank, SamplerSet};
use super::ui::{App, Options};
use crate::settings::{self, AudioSettings};
use crate::{audio, discovery};

const USAGE: &str = "\
lextrack: an M8-style tracker that plays CLAP plugins

USAGE:
    lextrack [SONG.m8s] [OPTIONS]

Opens a Dirtywave M8 song (or a new one) in an M8-style tracker. SAMPLER
instruments play their WAV files on the built-in sampler; every other M8
instrument is played by a CLAP plugin instance: pick plugins in the
instrument view, or with the options below. Plugin choices and settings are
saved next to the song in SONG.lextrack.

OPTIONS:
    --map <FILE>          Instrument map (see README): lines like
                          `00 = ~/.clap/Surge XT.clap`, `fmsynth = ...`,
                          `default = ...`. Replaces SONG.lextrack.
    --plugin <PATH[@ID]>  Plugin for every instrument the map doesn't cover
    --inst <KEY=PATH[@ID]>  One map rule on the command line (repeatable)
    --row <HEX>           Song row to start from (default 00)
    --note-offset <N>     MIDI key for M8 note C-1 (default 24, so C-4 = 60)
    --info                Print the song and the plugin for each instrument
    --render <OUT.wav>    Render to a WAV file instead of opening the tracker
    --seconds <S>         Length to render (default 30)
    --no-audio            Don't open a sound card
    --device <NAME>       Audio output device (see --list-devices), or \"default\"
    --sample-rate <HZ>    Sample rate, e.g. 48000, or \"default\"
    --buffer <FRAMES>     Buffer size, e.g. 256, or \"default\"
    --list                List installed CLAP plugins and exit
    --list-devices        List audio output devices and exit
    -h, --help            Show this help

BUTTONS (the M8 has eight):
    Arrows                UP DOWN LEFT RIGHT
    A                     SHIFT  (or hold the Shift key with an arrow)
    Z / Esc               OPTION (or hold Alt with an arrow)
    X / Enter             EDIT   (or hold Ctrl with an arrow)
    S / Space             PLAY
    Delete                EDIT+OPTION
    Ctrl+Q                Quit

In terminals that report key releases (kitty, foot, WezTerm, Ghostty...),
A, Z, X and S can be held like the M8's buttons: hold X and press Right
for EDIT+RIGHT. Elsewhere, use the modifier keys, and a capital letter adds
SHIFT (Shift+Z is SHIFT+OPTION).

    SHIFT+arrows          Move between views (song, chain, phrase,
                          instrument, table; project above song, groove
                          above phrase)
    EDIT                  Insert the last value; twice quickly for a new
                          chain, phrase or instrument
    EDIT+arrows           Change a value (UP/DOWN in bigger steps)
    EDIT+OPTION           Delete (on an empty note: note off)
    OPTION+arrows         Next/previous chain, phrase, instrument, track...
    SHIFT+OPTION          Select; OPTION copies, EDIT+OPTION cuts,
                          SHIFT+OPTION again widens the selection,
                          SHIFT+EDIT clones
    SHIFT+EDIT            Paste
    PLAY                  Play the song, chain or phrase, or stop
    SHIFT+PLAY            Play the song from the chain or phrase
    EDIT+PLAY             Preview the instrument
    OPTION+PLAY           Solo the track; OPTION+SHIFT mutes it,
                          OPTION+SHIFT+PLAY clears mutes and solos
";

struct Opts {
    song: Option<PathBuf>,
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
    list: bool,
    list_devices: bool,
}

fn parse(args: impl Iterator<Item = OsString>) -> Result<Opts> {
    let mut o = Opts {
        song: None,
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
        list: false,
        list_devices: false,
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
            Some("--list") => o.list = true,
            Some("--list-devices") => o.list_devices = true,
            Some(s) if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            _ => o.song = Some(PathBuf::from(arg)),
        }
    }
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

/// Where a song's plugin choices and plugin settings are kept.
pub fn sidecar(song: &Path) -> PathBuf {
    song.with_extension("lextrack")
}

/// The folder a song file is in (the current folder for a new song).
pub fn song_dir(song: Option<&Path>) -> PathBuf {
    let dir = song
        .and_then(|p| p.parent())
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), |d| d.to_path_buf());
    dir.canonicalize().unwrap_or(dir)
}

/// A song and the instrument map saved next to it, if any.
pub fn load_song_and_map(path: &Path) -> Result<(Song, InstrumentMap)> {
    let song = Song::load(path)?;
    let side = sidecar(path);
    let map = if side.exists() {
        InstrumentMap::load(&side)?
    } else {
        InstrumentMap::default()
    };
    Ok((song, map))
}

pub fn run(args: impl Iterator<Item = OsString>) -> Result<()> {
    let o = parse(args)?;
    if o.list_devices {
        list_devices();
        return Ok(());
    }
    if o.list {
        list_plugins();
        return Ok(());
    }

    let (song, mut map) = match &o.song {
        Some(p) if p.exists() => load_song_and_map(p)?,
        Some(_) | None => (Song::new_empty(), InstrumentMap::default()),
    };
    if let Some(p) = &o.map {
        map = InstrumentMap::load(p)?;
    }
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

    let song_dir = song_dir(o.song.as_deref());
    if o.info {
        print_info(&song, &map, &song_dir);
        return Ok(());
    }

    if let Some(out) = &o.render {
        let (samplers, sample_problems) =
            SamplerSet::build(&song, &song_dir, &map, &mut SampleBank::default());
        let sampled = samplers.samples.iter().filter(|s| s.is_some()).count();
        if map.is_empty() && sampled == 0 {
            bail!("no plugins chosen. Use --plugin PATH.clap for every instrument, or --map FILE");
        }
        let (mut slots, missing) = load_slots(&song, &map);
        for (n, why) in missing.iter().chain(&sample_problems) {
            eprintln!("instrument {n:02X} will be silent: {why}");
        }
        if slots.is_empty() && sampled == 0 {
            bail!("none of the song's instruments has a working plugin or sample");
        }
        for s in &slots {
            if let Some(state) = map.states.get(&s.instrument) {
                s.plugin.load_state(state);
            }
        }
        let rate = o.sample_rate.flatten().unwrap_or(48_000);
        render_wav(
            Arc::new(song.clone()),
            &mut slots,
            Arc::new(samplers),
            o.row,
            o.seconds,
            rate,
            o.note_offset,
            out,
        )?;
        eprintln!(
            "rendered {:.1} s of \"{}\" with {} plugin(s) and {sampled} sample(s) to {}",
            o.seconds,
            song.name,
            slots.len(),
            out.display()
        );
        return Ok(());
    }

    if !std::io::stdout().is_terminal() {
        bail!("lextrack needs a terminal (use --info or --render for plain output)");
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
    let mut term = Terminal::new(CrosstermBackend::new(tty.writer()?))?;
    let mut app = App::new(
        song,
        Options {
            path: o.song,
            map,
            use_device: !o.no_audio,
            audio,
            note_offset: o.note_offset,
            row: o.row,
        },
    );
    let r = app.run(&mut term);
    app.shutdown();
    drop(term);
    drop(tty);
    r
}

fn list_devices() {
    let default = audio::default_device_name();
    let devices = audio::output_devices();
    if devices.is_empty() {
        println!("no audio output devices found");
    }
    for d in devices {
        let mark = if Some(&d) == default.as_ref() {
            "  (default)"
        } else {
            ""
        };
        println!("{d}{mark}");
    }
    if let Some(p) = settings::path() {
        let s = AudioSettings::load();
        println!("\nsaved settings ({}):", p.display());
        println!("  device      {}", s.device.as_deref().unwrap_or("default"));
        println!(
            "  sample rate {}",
            s.sample_rate.map_or("default".into(), |r| r.to_string())
        );
        println!(
            "  buffer      {}",
            s.buffer_size.map_or("default".into(), |b| b.to_string())
        );
    }
}

fn list_plugins() {
    let scan = discovery::scan_default();
    for p in &scan.plugins {
        println!(
            "{:<30} {:<20} {:<40} {}",
            p.name,
            p.vendor,
            p.id,
            p.path.display()
        );
    }
    for e in &scan.errors {
        eprintln!("skipped {e}");
    }
    if scan.plugins.is_empty() {
        eprintln!("no CLAP plugins found in:");
        for d in discovery::search_paths() {
            eprintln!("  {}", d.display());
        }
    }
}

fn print_info(song: &Song, map: &InstrumentMap, song_dir: &Path) {
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
    let root = super::sampler::card_root(song_dir, &song.directory);
    for n in used {
        let inst = &song.instruments[n as usize];
        let plugin = if map.builtin_sampler(n, inst) {
            match super::sampler::resolve_sample_path(&root, song_dir, &inst.sample_path) {
                Some(p) => format!("sample {}", p.display()),
                None if inst.sample_path.is_empty() => "(no sample)".into(),
                None => format!("sample {} (not found)", inst.sample_path),
            }
        } else {
            map.resolve(n, inst)
                .map_or("(no plugin)".to_string(), |p| p.path.display().to_string())
        };
        println!(
            "instrument {n:02X} {:<12} {:<10} -> {plugin}",
            inst.name,
            inst.kind.name()
        );
    }
}
