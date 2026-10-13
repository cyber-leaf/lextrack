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
}

#[derive(Clone, Debug, Default)]
pub struct InstrumentMap {
    by_number: HashMap<u8, PluginRef>,
    by_kind: HashMap<String, PluginRef>,
    default: Option<PluginRef>,
    /// Saved plugin settings (`clap.state`) per instrument number.
    pub states: HashMap<u8, Vec<u8>>,
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
            if let Some(num) = key.trim().strip_prefix("state ") {
                let num = u8::from_str_radix(num.trim(), 16).ok();
                let data = base64_decode(value.trim());
                match (num, data) {
                    (Some(num), Some(data)) => map.states.insert(num, data),
                    _ => bail!("{}:{}: bad plugin state line", path.display(), n + 1),
                };
                continue;
            }
            map.set(key, value, base)
                .with_context(|| format!("{}:{}", path.display(), n + 1))?;
        }
        Ok(map)
    }

    /// Write the rules and plugin states, with absolute paths.
    pub fn save(&self, path: &Path, song_name: &str) -> Result<()> {
        let mut out = format!(
            "# lextrack instruments for {song_name}: which CLAP plugin plays each\n\
             # M8 instrument, and the plugins' saved settings.\n"
        );
        let line = |key: &str, p: &PluginRef| {
            let path = p.path.display();
            match &p.id {
                Some(id) => format!("{key} = {path}@{id}\n"),
                None => format!("{key} = {path}\n"),
            }
        };
        let mut nums: Vec<_> = self.by_number.iter().collect();
        nums.sort_by_key(|(n, _)| **n);
        for (n, p) in nums {
            out += &line(&format!("{n:02X}"), p);
        }
        let mut kinds: Vec<_> = self.by_kind.iter().collect();
        kinds.sort_by(|a, b| a.0.cmp(b.0));
        for (k, p) in kinds {
            out += &line(k, p);
        }
        if let Some(p) = &self.default {
            out += &line("default", p);
        }
        let mut states: Vec<_> = self.states.iter().collect();
        states.sort_by_key(|(n, _)| **n);
        for (n, data) in states {
            out += &format!("state {n:02X} = {}\n", base64_encode(data));
        }
        std::fs::write(path, out).with_context(|| format!("can't write {}", path.display()))
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

    /// Instrument numbers with a plugin of their own.
    pub fn numbered(&self) -> impl Iterator<Item = u8> + '_ {
        self.by_number.keys().copied()
    }

    /// Give one instrument its own plugin, or remove its own rule.
    pub fn set_number(&mut self, n: u8, plugin: Option<PluginRef>) {
        match plugin {
            Some(p) => self.by_number.insert(n, p),
            None => self.by_number.remove(&n),
        };
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

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|&c| c != b'=') {
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
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
    fn base64_round_trips() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 200) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("lextrack-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.lextrack");
        let mut m = InstrumentMap::default();
        m.set("03", "/p/a.clap@x.y", &dir).unwrap();
        m.set("fmsynth", "/p/fm.clap", &dir).unwrap();
        m.states.insert(3, vec![1, 2, 3, 250]);
        m.save(&path, "SONG").unwrap();
        let back = InstrumentMap::load(&path).unwrap();
        assert_eq!(back.by_number, m.by_number);
        assert_eq!(back.by_kind, m.by_kind);
        assert_eq!(back.states, m.states);
        std::fs::remove_dir_all(&dir).unwrap();
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
