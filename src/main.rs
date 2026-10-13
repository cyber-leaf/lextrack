//! lextrack: an M8-style terminal tracker that plays CLAP plugins.

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

mod audio;
mod discovery;
mod m8;
mod plugin;
mod settings;

use std::io::Write;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::Result;
use ratatui::crossterm::{execute, terminal};

use crate::m8::input::RELEASE_EVENTS;

fn main() {
    if let Err(e) = run() {
        eprintln!("lextrack: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    // `lextrack m8 ...` was the command before the tracker became the app.
    if args.peek().is_some_and(|a| a == "m8") {
        args.next();
    }
    let result = m8::app::run(args);
    if let Some(log) = log_path() {
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
            eprintln!(
                "(console output from the plugins and audio system was saved to {})",
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
pub struct TtyGuard {
    #[cfg(unix)]
    saved: Option<(i32, i32)>,
}

impl TtyGuard {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            ratatui::crossterm::cursor::Hide
        )?;
        enable_key_releases();
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
    pub fn writer(&self) -> Result<Box<dyn Write + Send>> {
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

/// Ask the terminal to report key releases, so letter keys can be held like
/// the M8's buttons. Terminals that speak the kitty keyboard protocol (kitty,
/// foot, WezTerm, Ghostty, Alacritty...) support this; elsewhere modifier
/// keys stand in for held buttons. The Windows console always reports
/// releases.
fn enable_key_releases() {
    use ratatui::crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    let supported = if cfg!(windows) {
        true
    } else if terminal::supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )
        .is_ok()
    } else {
        false
    };
    RELEASE_EVENTS.store(supported, std::sync::atomic::Ordering::Relaxed);
}

fn restore_terminal_raw() {
    if !cfg!(windows) && RELEASE_EVENTS.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = execute!(
            std::io::stdout(),
            ratatui::crossterm::event::PopKeyboardEnhancementFlags
        );
    }
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
