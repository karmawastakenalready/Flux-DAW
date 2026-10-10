//! The released build (`--features dist`): one program that is Flux and its own Setup. Downloaded as
//! `FluxSetup-<version>.exe`, it opens Setup, which copies the same file into the app folder as `flux.exe`, with
//! `flux.com` for terminals. It is a windowed program, so starting it from a terminal opens no console: `flux.com`
//! waits for commands and passes on their output (build.rs, shim/), and the windows show their errors in a box.

use flux_setup::job::Bundle;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(windows)]
const SHIM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/flux.com"));
#[cfg(not(windows))]
const SHIM: &[u8] = &[];

/// What this build carries for Setup.
pub const BUNDLE: Bundle = Bundle {
    version: env!("CARGO_PKG_VERSION"),
    seed: include_bytes!(concat!(env!("OUT_DIR"), "/seed.zip")),
    shim: SHIM,
    extra: &[
        ("LICENSE-MIT.txt", include_bytes!("../../../../LICENSE-MIT")),
        ("LICENSE-APACHE.txt", include_bytes!("../../../../LICENSE-APACHE")),
        ("THIRD-PARTY-NOTICES.txt", include_bytes!(concat!(env!("OUT_DIR"), "/notices.txt"))),
    ],
    homepage: "https://github.com/karmawastakenalready/Flux-DAW",
};

/// Whether this program is a downloaded Setup: `FluxSetup-0.2.0.exe`, also as a browser renames it
/// (`FluxSetup-0.2.0 (1).exe`).
fn is_setup_exe(exe: &Path) -> bool {
    exe.file_stem().and_then(OsStr::to_str).and_then(|s| s.get(..9)).is_some_and(|s| s.eq_ignore_ascii_case("fluxsetup"))
}

/// The folder of the installed Flux this program is, if it is one.
fn installed_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join(flux_setup::install::FILES_RECORD).is_file().then_some(dir)
}

/// Before the usual commands: a downloaded Setup runs as `setup`; commands get the terminal they were typed in;
/// Setup and Uninstall run here. `Some` is the exit code of a command run here.
pub fn start(args: &mut Vec<String>) -> Option<ExitCode> {
    if std::env::current_exe().is_ok_and(|e| is_setup_exe(&e)) && args.first().map(String::as_str) != Some("setup") {
        args.insert(0, "setup".into());
    }
    let window = crate::window::opens_window(args);
    if !window {
        console::attach();
    }
    let here = installed_dir();
    if let Some(dir) = &here {
        flux_setup::install::cleanup_replaced(dir);
    }
    let code = match args.first().map(String::as_str) {
        Some("setup") if window => report(crate::gui::run_setup(BUNDLE, args.iter().any(|a| a == "--update"))),
        Some("setup") => flux_setup::cli::setup_main(&args[1..], BUNDLE),
        Some("uninstall") => {
            let Some(dir) = here.or_else(|| flux_setup::install::installed().map(|i| i.dir)) else {
                return Some(report(Err(anyhow::anyhow!("Flux isn't installed for this user, so there is nothing to uninstall"))));
            };
            if window { report(crate::gui::run_uninstall(dir)) } else { flux_setup::cli::uninstall_main(&dir, &args[1..]) }
        }
        _ => return None,
    };
    Some(code)
}

/// A window's result as an exit code, its error shown in a box (there is no terminal to print it to).
pub fn report(r: anyhow::Result<()>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error_box(&format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Show an error in a message box (and on stderr, should anything be reading it).
pub fn error_box(text: &str) {
    eprintln!("flux: {text}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide("Flux"));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call; no owner window.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
    }
}

#[cfg(windows)]
mod console {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_UNKNOWN, GetFileType};
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE};

    /// Write to the terminal a command was typed in. A windowed program has no console of its own: started by
    /// `flux.com` it inherits the terminal's handles, which work once attached to it; started straight from a
    /// terminal it has none, and attaching gives it the terminal's (which doesn't wait for it, so the prompt comes
    /// back first: `flux.com` is the way). Output to a pipe or file (an agent's MCP server, a redirect) is left as
    /// it is.
    pub fn attach() {
        // SAFETY: plain Win32 calls on this process's own handles (attaching twice just fails).
        unsafe {
            let out = GetStdHandle(STD_OUTPUT_HANDLE);
            let kind = if out.is_null() || out == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE { FILE_TYPE_UNKNOWN } else { GetFileType(out) };
            if kind == FILE_TYPE_CHAR || kind == FILE_TYPE_UNKNOWN {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }
}

#[cfg(not(windows))]
mod console {
    pub fn attach() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_a_downloaded_setup() {
        assert!(is_setup_exe(Path::new(r"C:\Users\x\Downloads\FluxSetup-0.2.0.exe")));
        assert!(is_setup_exe(Path::new(r"C:\Users\x\Downloads\fluxsetup-0.2.0 (1).exe")));
        assert!(is_setup_exe(Path::new("FluxSetup.exe")));
        assert!(!is_setup_exe(Path::new(r"C:\Users\x\AppData\Local\Programs\Flux\flux.exe")));
        assert!(!is_setup_exe(Path::new("Flux-Setup.exe")));
    }

    #[test]
    fn carries_the_library_files() {
        let manifest = flux_setup::seed::read(BUNDLE.seed, "assets.toml").unwrap();
        assert!(!manifest.is_empty());
        assert!(flux_setup::seed::read(BUNDLE.seed, "LICENSES.md").is_ok());
        #[cfg(windows)]
        assert_eq!(&BUNDLE.shim[..2], b"MZ");
    }
}
