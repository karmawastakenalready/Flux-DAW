//! Where Flux keeps things.
//!
//! * The **library** ([`root`]): instruments, tools (sfizz), SFZ mappings, instrument cards, plugin states and the
//!   asset manifest (`assets.toml`). In a source checkout it is the checkout itself; an installed Flux uses the
//!   folder chosen in setup (kept in `settings.json`), by default `%LOCALAPPDATA%\Flux\Library`.
//! * The user's **documents** ([`projects_dir`], [`exports_dir`]): songs and their renders. A checkout keeps them
//!   in `daw/projects` and `output/` (both kept out of git); an installed Flux in `Documents\Flux`.

use crate::settings::{self, Settings};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ROOT: OnceLock<(PathBuf, RootKind)> = OnceLock::new();

/// How the library folder was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// `FLUX_ROOT` names it.
    Env,
    /// A folder above the current directory or the executable holds a library (a source checkout, or a library
    /// Flux was started from).
    Found,
    /// The library folder from setup (`settings.json`).
    Settings,
    /// Nothing named one: the default library folder, which may not exist yet.
    Default,
}

/// A library holds the asset manifest beside instruments, SFZ mappings or sfizz.
fn looks_like_root(p: &Path) -> bool {
    p.join("assets.toml").is_file()
        && (p.join("instruments").is_dir() || p.join("sfz").is_dir() || p.join("tools").join("sfizz").is_dir())
}

/// An environment variable, or its name from before the product was called Flux (`BATON_*`).
fn env(name: &str) -> Option<String> {
    std::env::var(name).or_else(|_| std::env::var(name.replacen("FLUX_", "BATON_", 1))).ok().filter(|v| !v.is_empty())
}

fn find_root() -> (PathBuf, RootKind) {
    if let Some(r) = env("FLUX_ROOT") {
        return (PathBuf::from(r), RootKind::Env);
    }
    let starts = [std::env::current_dir().ok(), std::env::current_exe().ok()];
    for s in starts.iter().flatten() {
        if let Some(d) = s.ancestors().find(|d| looks_like_root(d)) {
            return (d.to_path_buf(), RootKind::Found);
        }
    }
    if let Some(l) = Settings::load().library {
        return (l, RootKind::Settings);
    }
    (default_library(), RootKind::Default)
}

/// The library folder: `FLUX_ROOT`, else the first folder above the current directory or the executable that
/// holds a library (`assets.toml` beside `instruments/`, `sfz/` or `tools/sfizz`), else the library chosen in
/// setup, else [`default_library`].
pub fn root() -> &'static Path {
    &ROOT.get_or_init(find_root).0
}

/// How [`root`] was found.
pub fn root_kind() -> RootKind {
    ROOT.get_or_init(find_root).1
}

/// Whether the library is a source checkout of Flux (projects and exports then stay inside it, as before Flux had
/// an installer).
pub fn is_checkout() -> bool {
    root().join("daw").join("Cargo.toml").is_file()
}

/// Where setup suggests putting the library: `Library` in Flux's data folder.
pub fn default_library() -> PathBuf {
    settings::data_dir().map_or_else(|| PathBuf::from("Flux Library"), |d| d.join("Library"))
}

/// The folder for the user's projects and exports: `FLUX_DOCUMENTS`, else the one chosen in setup, else `Flux`
/// in the user's Documents.
pub fn documents() -> PathBuf {
    if let Some(d) = env("FLUX_DOCUMENTS") {
        return PathBuf::from(d);
    }
    Settings::load().documents.unwrap_or_else(|| user_documents().unwrap_or_else(|| root().to_path_buf()).join("Flux"))
}

/// Where projects are saved and listed: `daw/projects` in a checkout, else `Projects` in [`documents`].
pub fn projects_dir() -> PathBuf {
    if is_checkout() { root().join("daw").join("projects") } else { documents().join("Projects") }
}

/// Where exports go by default: `output/` in a checkout, else `Exports` in [`documents`].
pub fn exports_dir() -> PathBuf {
    if is_checkout() { root().join("output") } else { documents().join("Exports") }
}

/// Resolve a path that may be relative to the library.
pub fn resolve(p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() { pb } else { root().join(pb) }
}

/// A path as stored in projects: relative to the library (forward slashes) when inside it, so projects move with it.
pub fn portable(p: &Path) -> String {
    p.strip_prefix(root()).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

pub fn sfizz_dll() -> PathBuf {
    if let Some(p) = env("FLUX_SFIZZ") {
        return PathBuf::from(p);
    }
    root().join("tools").join("sfizz").join("bin").join("Release").join("sfizz.dll")
}

/// The user's Documents folder (where Windows has it, which may be OneDrive's).
pub fn user_documents() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents)
    }
    #[cfg(not(windows))]
    {
        home().map(|h| h.join("Documents"))
    }
}

/// The user's Downloads folder.
pub fn user_downloads() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Downloads)
    }
    #[cfg(not(windows))]
    {
        home().map(|h| h.join("Downloads"))
    }
}

#[cfg(not(windows))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// A Windows known folder (the shell's answer, so moved or redirected folders are found too).
#[cfg(windows)]
pub fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `id` and `p` are valid for the call; the shell stores a NUL-terminated string it allocated in `p`.
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
    let path = (hr >= 0 && !p.is_null()).then(|| {
        // SAFETY: on success `p` points to a NUL-terminated UTF-16 string, read up to (not including) the NUL.
        let wide = unsafe {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            std::slice::from_raw_parts(p, len)
        };
        PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: the string is the shell's to hand back, whether or not the call succeeded (null is allowed).
    unsafe { CoTaskMemFree(p as *const _) };
    path.filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_needs_its_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("instruments")).unwrap();
        assert!(!looks_like_root(d), "a folder that merely has instruments/ is not a library");
        std::fs::write(d.join("assets.toml"), "schema = 1\n").unwrap();
        assert!(looks_like_root(d));
        let other = dir.path().join("other");
        std::fs::create_dir_all(other.join("sfz")).unwrap();
        std::fs::write(other.join("assets.toml"), "").unwrap();
        assert!(looks_like_root(&other));
    }

    #[test]
    fn the_checkout_keeps_songs_where_they_were() {
        // the tests run inside the source checkout, which is the library
        assert!(is_checkout());
        assert!(matches!(root_kind(), RootKind::Found | RootKind::Env));
        assert_eq!(projects_dir(), root().join("daw").join("projects"));
        assert_eq!(exports_dir(), root().join("output"));
    }

    #[test]
    fn finds_the_users_folders() {
        let docs = user_documents().expect("Documents");
        assert!(docs.is_absolute(), "{}", docs.display());
        assert!(user_downloads().expect("Downloads").is_absolute());
    }
}
