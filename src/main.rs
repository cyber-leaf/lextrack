//! lextrack: a terminal host for CLAP audio plugins.

/// `&'static CStr` from a string literal (C-string literals need Rust 1.77).
#[allow(unused_macros)]
macro_rules! cstr {
    ($s:literal) => {
        unsafe { ::std::ffi::CStr::from_bytes_with_nul_unchecked(concat!($s, "\0").as_bytes()) }
    };
}

/// A `*const c_char` to a NUL-terminated string literal.
macro_rules! cptr {
    ($s:literal) => {
        concat!($s, "\0").as_ptr() as *const ::std::ffi::c_char
    };
}

mod discovery;
mod engine;
mod plugin;
mod ui;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{bail, Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{execute, terminal};
use ratatui::Terminal;

use crate::plugin::{PluginDescriptor, PluginInstance, PluginLibrary};

const USAGE: &str = "\
lextrack: a terminal host for CLAP plugins

USAGE:
    lextrack [OPTIONS] [PLUGIN.clap]

With no plugin path, lextrack scans the standard CLAP folders and lets you pick.

OPTIONS:
    --id <ID>       Plugin id to load when a file contains several plugins
    --index <N>     Same, by position (0-based)
    --no-audio      Don't open a sound card; process silently instead
    --list          List installed plugins and exit
    --dump          Print the plugin's parameters and exit (no UI)
    -h, --help      Show this help

KEYS:
    Up/Down         Select a parameter       Left/Right   Change its value
    Shift+Left/Right  Fine step              Ctrl+Left/Right  Coarse step
    Home/End        Minimum/maximum          d            Default value
    Enter           Type a value             /            Filter by name
    PgUp/PgDn       Jump 10 rows             L            Show plugin log
    q / Esc         Quit
";

struct Args {
    path: Option<PathBuf>,
    id: Option<String>,
    index: Option<usize>,
    no_audio: bool,
    list: bool,
    dump: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        path: None,
        id: None,
        index: None,
        no_audio: false,
        list: false,
        dump: false,
    };
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Some("--id") => {
                a.id = Some(
                    it.next()
                        .context("--id needs a value")?
                        .to_string_lossy()
                        .into(),
                )
            }
            Some("--index") => {
                a.index = Some(
                    it.next()
                        .context("--index needs a value")?
                        .to_string_lossy()
                        .parse()?,
                )
            }
            Some("--no-audio") => a.no_audio = true,
            Some("--list") => a.list = true,
            Some("--dump") => a.dump = true,
            Some(s) if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            _ => a.path = Some(PathBuf::from(arg)),
        }
    }
    Ok(a)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("lextrack: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;

    if args.list {
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
        return Ok(());
    }

    // Work out which plugins are candidates.
    let candidates: Vec<PluginDescriptor> = match &args.path {
        Some(path) => PluginLibrary::load(path)?.descriptors()?,
        None => {
            let scan = discovery::scan_default();
            if scan.plugins.is_empty() {
                let dirs: Vec<String> = discovery::search_paths()
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect();
                bail!(
                    "no CLAP plugins found. Pass a path to a .clap file, or install plugins in: {}",
                    dirs.join(", ")
                );
            }
            scan.plugins
        }
    };
    if candidates.is_empty() {
        bail!("that file contains no plugins");
    }
    let preselected = if let Some(id) = &args.id {
        Some(
            candidates
                .iter()
                .position(|d| &d.id == id)
                .with_context(|| format!("no plugin with id {id}"))?,
        )
    } else if let Some(i) = args.index {
        if i >= candidates.len() {
            bail!(
                "--index {i} is out of range (the file has {} plugins)",
                candidates.len()
            );
        }
        Some(i)
    } else if candidates.len() == 1 || args.dump {
        Some(0)
    } else {
        None
    };

    if args.dump {
        let d = &candidates[preselected.unwrap_or(0)];
        let plugin = PluginInstance::new(PluginLibrary::load(&d.path)?, &d.id)?;
        ui::dump(&plugin);
        return Ok(());
    }

    if !std::io::stdout().is_terminal() {
        bail!("lextrack needs a terminal (use --dump for plain output)");
    }

    let tty = TtyGuard::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(tty.writer()?))?;
    let result = (|| -> Result<()> {
        let choice = match preselected {
            Some(i) => i,
            None => match ui::pick_plugin(&mut term, &candidates)? {
                Some(i) => i,
                None => return Ok(()),
            },
        };
        let d = &candidates[choice];
        let plugin = PluginInstance::new(PluginLibrary::load(&d.path)?, &d.id)?;
        let mut app = ui::App::new(plugin, !args.no_audio)?;
        let r = app.run(&mut term);
        app.shutdown();
        r
    })();
    drop(term);
    drop(tty);
    if let Some(log) = log_path() {
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
            eprintln!(
                "(console output from the plugin and audio system was saved to {})",
                log.display()
            );
        }
    }
    result
}

fn log_path() -> Option<PathBuf> {
    Some(std::env::temp_dir().join("lextrack-plugin-output.log"))
}

/// Puts the terminal in raw/alternate-screen mode and restores it on drop or
/// panic. On Unix it also points stdout/stderr at a log file so that plugins
/// printing to the console don't scribble over the UI.
struct TtyGuard {
    #[cfg(unix)]
    saved: Option<(i32, i32)>,
}

impl TtyGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            ratatui::crossterm::cursor::Hide
        )?;
        let guard = TtyGuard {
            #[cfg(unix)]
            saved: redirect_console(),
        };
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            #[cfg(unix)]
            restore_console();
            restore_terminal_raw();
            prev(info);
        }));
        Ok(guard)
    }

    /// Where the UI should draw: the real terminal.
    fn writer(&self) -> Result<Box<dyn Write + Send>> {
        #[cfg(unix)]
        if let Some((out, _)) = self.saved {
            use std::os::fd::FromRawFd;
            let dup = unsafe { libc::dup(out) };
            if dup >= 0 {
                return Ok(Box::new(std::io::BufWriter::new(unsafe {
                    std::fs::File::from_raw_fd(dup)
                })));
            }
        }
        Ok(Box::new(std::io::BufWriter::new(std::io::stdout())))
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.saved.take().is_some() {
            restore_console();
        }
        restore_terminal_raw();
    }
}

fn restore_terminal_raw() {
    let _ = execute!(
        std::io::stdout(),
        terminal::LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
    let _ = terminal::disable_raw_mode();
}

#[cfg(unix)]
fn redirect_console() -> Option<(i32, i32)> {
    use std::os::fd::AsRawFd;
    let _ = std::io::stdout().flush();
    let log = std::fs::File::create(log_path()?).ok()?;
    unsafe {
        let out = libc::dup(1);
        let err = libc::dup(2);
        if out < 0 || err < 0 {
            return None;
        }
        libc::dup2(log.as_raw_fd(), 1);
        libc::dup2(log.as_raw_fd(), 2);
        SAVED_FDS.store(((out as i64) << 32) | err as i64, Ordering::SeqCst);
        Some((out, err))
    }
}

/// The real stdout/stderr while the console is redirected, packed as
/// `out << 32 | err`; -1 when not redirected.
#[cfg(unix)]
static SAVED_FDS: AtomicI64 = AtomicI64::new(-1);

#[cfg(unix)]
fn restore_console() {
    let packed = SAVED_FDS.swap(-1, Ordering::SeqCst);
    if packed < 0 {
        return;
    }
    let (out, err) = ((packed >> 32) as i32, (packed & 0xffff_ffff) as i32);
    unsafe {
        libc::dup2(out, 1);
        libc::dup2(err, 2);
        libc::close(out);
        libc::close(err);
    }
}
