//! Locating the music project root (where tools/, instruments/ and .venv/ live).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

fn looks_like_root(p: &Path) -> bool {
    p.join("tools").join("sfizz").is_dir() || p.join("instruments").is_dir()
}

/// An environment variable, or its name from before the product was called Flux (`BATON_*`).
fn env(name: &str) -> Option<String> {
    std::env::var(name).or_else(|_| std::env::var(name.replacen("FLUX_", "BATON_", 1))).ok()
}

/// Root of the music project: FLUX_ROOT, else the first ancestor of the current dir or the executable
/// that contains `tools/sfizz` or `instruments/`.
pub fn root() -> &'static Path {
    ROOT.get_or_init(|| {
        if let Some(r) = env("FLUX_ROOT") {
            return PathBuf::from(r);
        }
        let mut starts = vec![];
        if let Ok(c) = std::env::current_dir() {
            starts.push(c);
        }
        if let Ok(e) = std::env::current_exe() {
            starts.push(e);
        }
        for s in starts {
            let mut p: Option<&Path> = Some(s.as_path());
            while let Some(d) = p {
                if looks_like_root(d) {
                    return d.to_path_buf();
                }
                p = d.parent();
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    })
}

/// Resolve a path that may be relative to the project root.
pub fn resolve(p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root().join(pb)
    }
}

/// A path as stored in projects: relative to the root (forward slashes) when inside it, so projects move with it.
pub fn portable(p: &Path) -> String {
    p.strip_prefix(root()).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

pub fn sfizz_dll() -> PathBuf {
    if let Some(p) = env("FLUX_SFIZZ") {
        return PathBuf::from(p);
    }
    root().join("tools").join("sfizz").join("bin").join("Release").join("sfizz.dll")
}

