//! Audio output settings, saved between runs in a small `key = value` file.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Which output to use. `None` everywhere means "the system default".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioSettings {
    /// Output device name, as listed by `lextrack --list-devices`.
    pub device: Option<String>,
    /// Sample rate in Hz.
    pub sample_rate: Option<u32>,
    /// Buffer size in frames.
    pub buffer_size: Option<u32>,
}

/// Choices offered on the settings screen (the device may support others,
/// which can still be given with `--sample-rate` / `--buffer`).
pub const SAMPLE_RATES: &[u32] = &[44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
pub const BUFFER_SIZES: &[u32] = &[32, 64, 128, 256, 512, 1024, 2048, 4096];

/// `~/.config/lextrack/settings.conf` on Linux, the platform equivalent
/// elsewhere. `$LEXTRACK_CONFIG` overrides it.
pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LEXTRACK_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let dir = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(dir.join("lextrack").join("settings.conf"))
}

impl AudioSettings {
    /// Read the saved settings; a missing or unreadable file gives defaults.
    pub fn load() -> Self {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Self::default();
        };
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() || value == "default" {
                continue;
            }
            match key.trim() {
                "device" => s.device = Some(value.to_string()),
                "sample_rate" => s.sample_rate = value.parse().ok(),
                "buffer_size" => s.buffer_size = value.parse().ok(),
                _ => {}
            }
        }
        s
    }

    fn render(&self) -> String {
        fn or_default<T: ToString>(v: &Option<T>) -> String {
            v.as_ref().map_or("default".into(), |v| v.to_string())
        }
        format!(
            "# lextrack audio settings. \"default\" means the system default.\n\
             device = {}\nsample_rate = {}\nbuffer_size = {}\n",
            or_default(&self.device),
            or_default(&self.sample_rate),
            or_default(&self.buffer_size),
        )
    }

    pub fn save(&self) -> Result<PathBuf> {
        let p = path().context("can't tell where to save settings (no home folder)")?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("can't create {}", dir.display()))?;
        }
        std::fs::write(&p, self.render())
            .with_context(|| format!("can't write {}", p.display()))?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = AudioSettings {
            device: Some("hw:CARD=PCH,DEV=0".into()),
            sample_rate: Some(96_000),
            buffer_size: None,
        };
        assert_eq!(AudioSettings::parse(&s.render()), s);
        assert_eq!(
            AudioSettings::parse(&AudioSettings::default().render()),
            AudioSettings::default()
        );
    }

    #[test]
    fn ignores_junk() {
        let s =
            AudioSettings::parse("# hi\nnonsense\nsample_rate = fast\nbuffer_size=128\nother=1");
        assert_eq!(s.sample_rate, None);
        assert_eq!(s.buffer_size, Some(128));
    }
}
