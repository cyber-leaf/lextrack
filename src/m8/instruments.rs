//! Which CLAP plugin plays each M8 instrument.
//!
//! A map file has one `key = plugin` line per rule:
//!
//! ```text
//! # by instrument number (hex, as the M8 shows it)
//! 00 = ~/.clap/Surge XT.clap
//! 0A = ~/.clap/Odin2.clap
//! # by M8 instrument kind
//! fmsynth = ~/.clap/Dexed.clap
//! # everything else
//! default = ~/.clap/Surge XT.clap
//! ```
//!
//! A file holding several plugins takes the plugin id after an `@`:
//! `00 = ~/.clap/Bundle.clap@com.vendor.synth`. Relative paths are relative
//! to the map file. Instrument numbers win over kinds, kinds over `default`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::format::{Instrument, InstrumentKind, INSTRUMENTS};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PluginRef {
    pub path: PathBuf,
    /// Plugin id inside the file; the first plugin when `None`.
    pub id: Option<String>,
}

impl PluginRef {
    /// `path` or `path@id`. `~/` is expanded; relative paths are taken
    /// from `base`.
    pub fn parse(text: &str, base: &Path) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            bail!("missing plugin path");
        }
        let (path, id) = match text.rsplit_once('@') {
            Some((p, id)) if !id.contains(['/', '\\']) && !id.is_empty() => {
                (p.trim(), Some(id.trim().to_string()))
            }
            _ => (text, None),
        };
        let path = if let Some(rest) = path.strip_prefix("~/") {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("can't expand ~: no home directory")?;
            PathBuf::from(home).join(rest)
        } else {
            base.join(path)
        };
        Ok(PluginRef { path, id })
    }

    pub fn label(&self) -> String {
        let file = self
            .path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        match &self.id {
            Some(id) => format!("{file}@{id}"),
            None => file,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct InstrumentMap {
    by_number: HashMap<u8, PluginRef>,
    by_kind: HashMap<String, PluginRef>,
    default: Option<PluginRef>,
}

const KINDS: &[&str] = &["wavsynth", "macrosynth", "sampler", "midiout", "fmsynth"];

impl InstrumentMap {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("can't read {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        let mut map = Self::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("{}:{}: expected `key = plugin`", path.display(), n + 1);
            };
            map.set(key, value, base)
                .with_context(|| format!("{}:{}", path.display(), n + 1))?;
        }
        Ok(map)
    }

    /// Add one rule. `key` is a hex instrument number, a kind name or
    /// `default`.
    pub fn set(&mut self, key: &str, value: &str, base: &Path) -> Result<()> {
        let key = key.trim().to_ascii_lowercase();
        let plugin = PluginRef::parse(value, base)?;
        if key == "default" || key == "*" {
            self.default = Some(plugin);
        } else if KINDS.contains(&key.as_str()) || key.starts_with("kind") {
            self.by_kind.insert(key, plugin);
        } else {
            let n = u8::from_str_radix(&key, 16)
                .ok()
                .filter(|&n| (n as usize) < INSTRUMENTS)
                .with_context(|| {
                    format!(
                        "unknown key \"{key}\": use an instrument number 00-7F, \
                         default, or one of {}",
                        KINDS.join(", ")
                    )
                })?;
            self.by_number.insert(n, plugin);
        }
        Ok(())
    }

    pub fn set_default(&mut self, plugin: PluginRef) {
        self.default = Some(plugin);
    }

    pub fn is_empty(&self) -> bool {
        self.by_number.is_empty() && self.by_kind.is_empty() && self.default.is_none()
    }

    pub fn resolve(&self, number: u8, instrument: &Instrument) -> Option<&PluginRef> {
        if let Some(p) = self.by_number.get(&number) {
            return Some(p);
        }
        if instrument.kind != InstrumentKind::None {
            if let Some(p) = self.by_kind.get(&instrument.kind.name()) {
                return Some(p);
            }
        }
        self.default.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(kind: InstrumentKind) -> Instrument {
        Instrument {
            kind,
            name: String::new(),
            transpose: true,
            table_tick: 1,
            volume: 0,
            pitch: 0,
            fine_tune: 0x80,
            sample_path: String::new(),
        }
    }

    #[test]
    fn rules_and_precedence() {
        let base = Path::new("/maps");
        let mut m = InstrumentMap::default();
        m.set("0a", "a.clap", base).unwrap();
        m.set("FMSYNTH", "/p/fm.clap@com.x.fm", base).unwrap();
        m.set("default", "d.clap", base).unwrap();
        let fm = inst(InstrumentKind::FmSynth);
        assert_eq!(
            m.resolve(0x0A, &fm).unwrap().path,
            Path::new("/maps/a.clap")
        );
        let r = m.resolve(0x01, &fm).unwrap();
        assert_eq!(
            (r.path.as_path(), r.id.as_deref()),
            (Path::new("/p/fm.clap"), Some("com.x.fm"))
        );
        assert_eq!(
            m.resolve(0x01, &inst(InstrumentKind::Sampler))
                .unwrap()
                .path,
            Path::new("/maps/d.clap")
        );
        assert!(m.set("80", "x.clap", base).is_err());
        assert!(m.set("bogus", "x.clap", base).is_err());
    }

    #[test]
    fn at_sign_in_folder_names_is_not_an_id() {
        let r = PluginRef::parse("/home/me@host/x.clap", Path::new("/")).unwrap();
        assert_eq!(
            (r.path.as_path(), r.id),
            (Path::new("/home/me@host/x.clap"), None)
        );
    }
}
