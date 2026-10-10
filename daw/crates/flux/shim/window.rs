//! Which command lines open Flux's window. Shared by `flux.com` (which waits for commands but not for the window)
//! and the installed `flux.exe` (which writes to the terminal for commands, and shows a window's errors in a box).

use std::ffi::OsStr;
use std::path::Path;

/// Whether `flux ARGS` opens the app's window rather than running a command: no arguments, `gui`, a song file,
/// Setup, or Uninstall (both without `--quiet` or `--help`).
pub fn opens_window<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let Some(first) = args.first().map(AsRef::as_ref) else { return true };
    let asks = |flag: &str| args.iter().any(|a| a.as_ref() == flag);
    match first.to_str() {
        Some("gui") => true,
        Some("setup" | "uninstall") => !(asks("--quiet") || asks("--help") || asks("-h") || asks("help")),
        Some(a) if a.starts_with('-') => false,
        // song files (`.flux.json`, `.json`, `.mid`); command names have no dot or slash
        _ => first.to_string_lossy().contains(['.', '/', '\\']) && Path::new(first).exists(),
    }
}
