//! Finding `.clap` files in the standard CLAP search locations.

use std::path::{Path, PathBuf};

use crate::plugin::{PluginDescriptor, PluginLibrary};

/// The directories CLAP hosts are expected to search, per the CLAP spec,
/// followed by anything in `$CLAP_PATH`.
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);

    #[cfg(target_os = "linux")]
    {
        if let Some(h) = &home {
            paths.push(h.join(".clap"));
        }
        paths.push("/usr/lib/clap".into());
        paths.push("/usr/local/lib/clap".into());
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = &home {
            paths.push(h.join("Library/Audio/Plug-Ins/CLAP"));
        }
        paths.push("/Library/Audio/Plug-Ins/CLAP".into());
    }
    #[cfg(target_os = "windows")]
    {
        let _ = &home;
        if let Some(p) = std::env::var_os("COMMONPROGRAMFILES") {
            paths.push(PathBuf::from(p).join("CLAP"));
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            paths.push(
                PathBuf::from(p)
                    .join("Programs")
                    .join("Common")
                    .join("CLAP"),
            );
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = &home;

    if let Some(extra) = std::env::var_os("CLAP_PATH") {
        paths.extend(std::env::split_paths(&extra));
    }
    paths
}

/// Recursively collect `.clap` files (or bundles) below `dir`.
pub fn find_clap_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let is_clap = p
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("clap"));
        if is_clap {
            out.push(p);
        } else if p.is_dir() {
            find_clap_files(&p, out);
        }
    }
}

pub struct ScanResult {
    pub plugins: Vec<PluginDescriptor>,
    pub errors: Vec<String>,
}

/// Load every `.clap` in `files` just long enough to read its descriptors.
pub fn scan_files(files: &[PathBuf]) -> ScanResult {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    for f in files {
        match PluginLibrary::load(f).and_then(|lib| lib.descriptors()) {
            Ok(d) => plugins.extend(d),
            Err(e) => errors.push(format!("{}: {e:#}", f.display())),
        }
    }
    ScanResult { plugins, errors }
}

pub fn scan_default() -> ScanResult {
    let mut files = Vec::new();
    for dir in search_paths() {
        find_clap_files(&dir, &mut files);
    }
    files.dedup();
    scan_files(&files)
}
