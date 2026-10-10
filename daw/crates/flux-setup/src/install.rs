//! Installing the app for this user. `%LOCALAPPDATA%\Programs\Flux` holds `flux.exe` (the app) and `flux.com` (the
//! same commands for terminals: Windows runs `flux.com` before `flux.exe` when `flux` is typed). Setup also adds a
//! Start menu shortcut, an entry in Settings > Apps and, optionally, the folder on PATH. No administrator rights are
//! needed. Uninstalling removes all of it. The instrument library goes only when the user asks
//! ([`remove_library`]), and the user's songs never go.

use crate::seed;
use anyhow::{Context, Result, bail};
use flux_assets::fsutil;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// The app's entry in Settings > Apps (under `HKEY_CURRENT_USER`).
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Flux";
/// Lets Run (Win+R) and `start flux` find the app.
pub const APP_PATHS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\flux.exe";
/// The files Setup wrote to the app folder, one name per line (Uninstall removes those and no others).
pub const FILES_RECORD: &str = ".flux-files";

/// What Setup puts in the app folder.
pub struct AppFiles<'a> {
    /// Flux itself (the running Setup is the same program).
    pub exe: &'a Path,
    /// The console launcher, `flux.com`.
    pub shim: &'a [u8],
    /// Other files by name: license texts, third-party notices.
    pub extra: Vec<(&'static str, &'a [u8])>,
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub version: String,
    /// Put the app folder on this user's PATH, so `flux` works in any terminal.
    pub add_to_path: bool,
    pub desktop_shortcut: bool,
    /// The project's page, shown in Settings > Apps.
    pub homepage: String,
}

/// The app folder: `%LOCALAPPDATA%\Programs\Flux`.
pub fn default_app_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|d| PathBuf::from(d).join("Programs").join("Flux"))
}

/// An installed Flux, as Settings > Apps knows it.
#[derive(Clone, Debug, Serialize)]
pub struct Installed {
    pub dir: PathBuf,
    pub version: String,
}

impl Installed {
    pub fn exe(&self) -> PathBuf {
        self.dir.join("flux.exe")
    }
}

/// Copy Flux into `dir` and register it for this user. Files of a running Flux are renamed aside (Windows lets a
/// running program's file be renamed, not replaced) and deleted the next time Flux starts ([`cleanup_replaced`]).
pub fn install_app(dir: &Path, files: &AppFiles, opts: &InstallOptions) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = dir.join("flux.exe");
    let mut names = vec!["flux.exe".to_string(), "flux.com".to_string()];
    if !same_file(files.exe, &exe) {
        put(dir, "flux.exe", |tmp| fs::copy(files.exe, tmp).map(|_| ()).with_context(|| format!("copying {}", files.exe.display())))?;
    }
    put(dir, "flux.com", |tmp| fs::write(tmp, files.shim).context("writing flux.com"))?;
    for (name, bytes) in &files.extra {
        put(dir, name, |tmp| fs::write(tmp, bytes).with_context(|| format!("writing {name}")))?;
        names.push(name.to_string());
    }
    fsutil::atomic_write(&dir.join(FILES_RECORD), names.join("\n").as_bytes())?;
    let size: u64 = names.iter().filter_map(|n| fs::metadata(dir.join(n)).ok()).map(|m| m.len()).sum();
    os::register(dir, &exe, opts, size)?;
    let desc = "Flux: an agent-first DAW for looping game music";
    if let Some(start) = os::start_menu() {
        os::shortcut(&start.join("Flux.lnk"), &exe, desc)?;
    }
    if let Some(desk) = os::desktop() {
        let lnk = desk.join("Flux.lnk");
        if opts.desktop_shortcut {
            os::shortcut(&lnk, &exe, desc)?;
        } else if lnk.exists() {
            fs::remove_file(&lnk).with_context(|| format!("removing {}", lnk.display()))?;
        }
    }
    os::set_on_path(dir, opts.add_to_path)
}

/// Whether `dir` is on this user's PATH.
pub fn on_path(dir: &Path) -> bool {
    path_with(&os::user_path(), &dir.display().to_string()).is_none()
}

/// Whether Flux has a shortcut on the desktop.
pub fn desktop_shortcut() -> bool {
    os::desktop().is_some_and(|d| d.join("Flux.lnk").exists())
}

/// Write a file through a temporary name, moving a file in use aside first.
fn put(dir: &Path, name: &str, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let dest = dir.join(name);
    let tmp = dir.join(format!(".{name}.new"));
    write(&tmp)?;
    if dest.exists() && fs::remove_file(&dest).is_err() {
        let aside = dir.join(format!("{name}.old-{}-{}", std::process::id(), stamp()));
        fs::rename(&dest, &aside).with_context(|| format!("moving the running {} aside", dest.display()))?;
    }
    fsutil::rename_with_retry(&tmp, &dest)
}

fn stamp() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Delete the files an update moved aside while Flux was running (quietly: one still running stays).
pub fn cleanup_replaced(dir: &Path) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().contains(".old-") {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// The Flux this user installed, if any.
pub fn installed() -> Option<Installed> {
    os::installed()
}

/// Unregister Flux (Settings > Apps, shortcuts, PATH) and delete its app folder once this process has exited.
/// Close every other Flux first ([`running_from`]).
pub fn uninstall_app(dir: &Path) -> Result<()> {
    for lnk in [os::start_menu(), os::desktop()].into_iter().flatten().map(|d| d.join("Flux.lnk")) {
        if lnk.exists() {
            fs::remove_file(&lnk).with_context(|| format!("removing {}", lnk.display()))?;
        }
    }
    os::set_on_path(dir, false)?;
    os::unregister()?;
    let mut names: Vec<String> = fs::read_to_string(dir.join(FILES_RECORD)).unwrap_or_default().lines().map(str::to_string).collect();
    for n in ["flux.exe", "flux.com", FILES_RECORD] {
        if !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    }
    names.retain(|n| !n.is_empty() && !n.contains(['/', '\\', '"', '*', '?']) && n != "..");
    os::remove_after_exit(dir, &names)
}

/// Processes running a program from `dir` (other than this one): Flux windows and the agent servers Flux runs.
pub fn running_from(dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(dir) = fs::canonicalize(dir) else { return vec![] };
    os::processes()
        .into_iter()
        .filter(|(pid, _)| *pid != std::process::id())
        // flux.com only waits for the flux.exe it started (perhaps this one)
        .filter(|(_, p)| !p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("flux.com")))
        .filter(|(_, p)| fs::canonicalize(p).is_ok_and(|p| p.starts_with(&dir)))
        .collect()
}

/// What [`remove_library`] did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LibraryRemoval {
    pub freed: u64,
    /// What was left: the plugin settings songs saved (`plugin-states`) and anything holding a kept folder.
    pub kept: Vec<PathBuf>,
}

/// Delete an instrument library Setup made. `plugin-states` (the plugin settings saved songs use) is kept when it
/// holds any, and so is whatever contains a folder in `keep` (the songs folder, should it be inside the library).
pub fn remove_library(library: &Path, keep: &[PathBuf]) -> Result<LibraryRemoval> {
    if !library.join(seed::RECORD).is_file() {
        bail!("{} is not an instrument library Setup made; Flux won't delete it", library.display());
    }
    let mut out = LibraryRemoval::default();
    let entries: Vec<_> = fs::read_dir(library).with_context(|| format!("reading {}", library.display()))?.flatten().map(|e| e.path()).collect();
    for p in entries {
        let saved_states = p.file_name().is_some_and(|n| n == "plugin-states") && fs::read_dir(&p).is_ok_and(|mut d| d.next().is_some());
        if saved_states || keep.iter().any(|k| k.starts_with(&p)) {
            out.kept.push(p);
            continue;
        }
        let md = fs::symlink_metadata(&p)?;
        if md.is_dir() {
            out.freed += fsutil::tree_size(&p, &[]).map_or(0, |s| s.0);
            flux_assets::fetch::remove_tree(&p)?;
        } else {
            out.freed += md.len();
            fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    if out.kept.is_empty() {
        fs::remove_dir(library).with_context(|| format!("removing {}", library.display()))?;
    }
    Ok(out)
}

/// `path` (a `;`-separated PATH value) with `dir` added at the end, or `None` when it is there already.
pub fn path_with(path: &str, dir: &str) -> Option<String> {
    if path.split(';').any(|p| same_dir(p, dir)) {
        return None;
    }
    let base = path.trim_end_matches(';');
    Some(if base.is_empty() { dir.to_string() } else { format!("{base};{dir}") })
}

/// `path` without `dir`, or `None` when it isn't there.
pub fn path_without(path: &str, dir: &str) -> Option<String> {
    if !path.split(';').any(|p| same_dir(p, dir)) {
        return None;
    }
    Some(path.split(';').filter(|p| !p.is_empty() && !same_dir(p, dir)).collect::<Vec<_>>().join(";"))
}

fn same_dir(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().trim_matches('"').trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    !a.trim().is_empty() && n(a) == n(b)
}

#[cfg(windows)]
mod os {
    use super::{InstallOptions, Installed, path_with, path_without};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ};

    pub fn register(dir: &Path, exe: &Path, opts: &InstallOptions, size: u64) -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (k, _) = hkcu.create_subkey(super::UNINSTALL_KEY).context("registering Flux in Settings > Apps")?;
        let exe_s = exe.display().to_string();
        let today: String = flux_assets::fsutil::now_rfc3339().chars().filter(char::is_ascii_digit).take(8).collect();
        k.set_value("DisplayName", &"Flux")?;
        k.set_value("DisplayVersion", &opts.version)?;
        k.set_value("Publisher", &"The Flux contributors")?;
        k.set_value("DisplayIcon", &format!("{exe_s},0"))?;
        k.set_value("InstallLocation", &dir.display().to_string())?;
        k.set_value("UninstallString", &format!("\"{exe_s}\" uninstall"))?;
        k.set_value("QuietUninstallString", &format!("\"{exe_s}\" uninstall --quiet"))?;
        k.set_value("URLInfoAbout", &opts.homepage)?;
        k.set_value("HelpLink", &opts.homepage)?;
        k.set_value("InstallDate", &today)?;
        k.set_value("EstimatedSize", &u32::try_from(size / 1024).unwrap_or(u32::MAX))?;
        k.set_value("NoModify", &1u32)?;
        k.set_value("NoRepair", &1u32)?;
        let (a, _) = hkcu.create_subkey(super::APP_PATHS_KEY).context("registering flux.exe's App Path")?;
        a.set_value("", &exe_s)?;
        a.set_value("Path", &dir.display().to_string())?;
        Ok(())
    }

    pub fn unregister() -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        for key in [super::UNINSTALL_KEY, super::APP_PATHS_KEY] {
            match hkcu.delete_subkey_all(key) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("removing HKCU\\{key}")),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn installed() -> Option<Installed> {
        let k = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(super::UNINSTALL_KEY, KEY_READ).ok()?;
        let dir: String = k.get_value("InstallLocation").ok()?;
        let version: String = k.get_value("DisplayVersion").unwrap_or_default();
        Some(Installed { dir: PathBuf::from(dir), version })
    }

    /// This user's PATH as stored (`%VARIABLES%` unexpanded), with its registry type.
    fn read_path(env: &RegKey) -> (String, winreg::enums::RegType) {
        match env.get_raw_value("Path") {
            Ok(v) => {
                let wide: Vec<u16> = v.bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&c| c != 0).collect();
                (String::from_utf16_lossy(&wide), v.vtype)
            }
            Err(_) => (String::new(), REG_EXPAND_SZ),
        }
    }

    pub fn user_path() -> String {
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Environment", KEY_READ).map(|env| read_path(&env).0).unwrap_or_default()
    }

    /// Add `dir` to (or take it off) this user's PATH, and tell running programs the environment changed.
    pub fn set_on_path(dir: &Path, on: bool) -> Result<()> {
        let env = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE).context("opening HKCU\\Environment")?;
        let (current, vtype) = read_path(&env);
        let d = dir.display().to_string();
        let Some(new) = (if on { path_with(&current, &d) } else { path_without(&current, &d) }) else { return Ok(()) };
        let vtype = if vtype == REG_SZ { REG_SZ } else { REG_EXPAND_SZ };
        let bytes: Vec<u8> = new.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        env.set_raw_value("Path", &winreg::RegValue { bytes: bytes.into(), vtype }).context("updating PATH")?;
        broadcast_environment();
        Ok(())
    }

    fn broadcast_environment() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
        let what: Vec<u16> = "Environment".encode_utf16().chain([0]).collect();
        let mut result = 0usize;
        // SAFETY: a broadcast with a NUL-terminated string that outlives the call; hung windows are skipped.
        unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr() as isize, SMTO_ABORTIFHUNG, 3000, &mut result) };
    }

    pub fn start_menu() -> Option<PathBuf> {
        flux_engine::paths::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Programs)
    }

    pub fn desktop() -> Option<PathBuf> {
        flux_engine::paths::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Desktop)
    }

    pub fn shortcut(lnk: &Path, exe: &Path, description: &str) -> Result<()> {
        let mut s = mslnk::ShellLink::new(exe).with_context(|| format!("making a shortcut to {}", exe.display()))?;
        s.set_name(Some(description.to_string()));
        s.set_working_dir(exe.parent().map(|d| d.display().to_string()));
        s.set_icon_location(Some(exe.display().to_string()));
        if let Some(d) = lnk.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        s.create_lnk(lnk).with_context(|| format!("writing {}", lnk.display()))
    }

    /// A hidden `cmd` deletes `names` in `dir`, and `dir` if that empties it, once this process (which may be
    /// `dir\flux.exe`) has exited.
    pub fn remove_after_exit(dir: &Path, names: &[String]) -> Result<()> {
        use std::os::windows::process::CommandExt;
        let q = |p: &Path| format!("\"{}\"", p.display());
        let exe = q(&dir.join("flux.exe"));
        let mut s = format!("/d /c for /l %i in (1,1,30) do @if exist {exe} (del /f /q {exe} >nul 2>&1 & if exist {exe} ping -n 2 127.0.0.1 >nul)");
        for n in names {
            s.push_str(&format!(" & del /f /q {} >nul 2>&1", q(&dir.join(n))));
        }
        s.push_str(&format!(" & del /f /q {} >nul 2>&1 & rd {} >nul 2>&1", q(&dir.join("*.old-*")), q(dir)));
        let sys = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        flux_assets::fsutil::quiet_command(sys.join("System32").join("cmd.exe"))
            .raw_arg(s)
            .current_dir(&sys)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("starting the removal of the app folder")?;
        Ok(())
    }

    /// Every process whose program file name starts with "flux" (case-insensitive), with its full path.
    pub fn processes() -> Vec<(u32, PathBuf)> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
        let mut out = Vec::new();
        // SAFETY: plain Win32 calls on handles this function opens and closes; buffers are sized as the calls require.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
                if name.starts_with("flux") {
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.th32ProcessID);
                    if !h.is_null() {
                        let mut buf = vec![0u16; 32768];
                        let mut n = buf.len() as u32;
                        if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) != 0 {
                            out.push((e.th32ProcessID, PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize]))));
                        }
                        CloseHandle(h);
                    }
                }
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }
}

#[cfg(not(windows))]
mod os {
    use super::{InstallOptions, Installed};
    use anyhow::{Result, bail};
    use std::path::{Path, PathBuf};

    pub fn register(_: &Path, _: &Path, _: &InstallOptions, _: u64) -> Result<()> {
        bail!("Flux's Setup is for Windows; elsewhere build Flux from source")
    }
    pub fn unregister() -> Result<()> {
        Ok(())
    }
    pub fn installed() -> Option<Installed> {
        None
    }
    pub fn user_path() -> String {
        String::new()
    }
    pub fn set_on_path(_: &Path, _: bool) -> Result<()> {
        Ok(())
    }
    pub fn start_menu() -> Option<PathBuf> {
        None
    }
    pub fn desktop() -> Option<PathBuf> {
        None
    }
    pub fn shortcut(_: &Path, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    pub fn remove_after_exit(dir: &Path, names: &[String]) -> Result<()> {
        for n in names {
            let _ = std::fs::remove_file(dir.join(n));
        }
        let _ = std::fs::remove_dir(dir);
        Ok(())
    }
    pub fn processes() -> Vec<(u32, PathBuf)> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_path_values() {
        let d = r"C:\Users\x\AppData\Local\Programs\Flux";
        assert_eq!(path_with(r"C:\a;C:\b", d).unwrap(), format!(r"C:\a;C:\b;{d}"));
        assert_eq!(path_with("", d).unwrap(), d);
        assert_eq!(path_with(r"C:\a;", d).unwrap(), format!(r"C:\a;{d}"));
        assert_eq!(path_with(&format!(r"C:\a;{}\", d.to_uppercase()), d), None, "already there, in other case, with a slash");
        assert_eq!(path_without(&format!(r"C:\a;{d};C:\b"), d).unwrap(), r"C:\a;C:\b");
        assert_eq!(path_without(r"C:\a;C:\b", d), None);
        assert_eq!(path_without(&format!("\"{d}\""), d).unwrap(), "");
    }

    #[test]
    fn installs_files_replacing_running_ones() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("Flux");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("flux.com"), "old shim").unwrap();
        put(&dir, "flux.com", |tmp| fs::write(tmp, "new shim").map_err(Into::into)).unwrap();
        assert_eq!(fs::read_to_string(dir.join("flux.com")).unwrap(), "new shim");
        // a running program's file can't be deleted, only renamed: it is moved aside and cleaned up later
        #[cfg(windows)]
        {
            let sys = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
            fs::copy(sys.join("PING.EXE"), dir.join("flux.exe")).unwrap();
            let mut child = std::process::Command::new(dir.join("flux.exe"))
                .args(["-n", "4", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert_eq!(running_from(&dir).len(), 1);
            put(&dir, "flux.exe", |tmp| fs::write(tmp, "new flux").map_err(Into::into)).unwrap();
            assert_eq!(fs::read_to_string(dir.join("flux.exe")).unwrap(), "new flux");
            let aside: Vec<_> = fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("flux.exe.old-")).collect();
            assert_eq!(aside.len(), 1);
            cleanup_replaced(&dir);
            assert!(aside[0].path().exists(), "still running: left for next time");
            child.kill().unwrap();
            child.wait().unwrap();
            cleanup_replaced(&dir);
            assert!(!aside[0].path().exists());
        }
        assert!(dir.join("flux.com").exists());
    }

    #[test]
    fn removes_a_library_but_keeps_saved_plugin_settings_and_songs() {
        let t = tempfile::tempdir().unwrap();
        let lib = t.path().join("Library");
        fs::create_dir_all(lib.join("instruments/piano")).unwrap();
        fs::write(lib.join("instruments/piano/a.wav"), "12345").unwrap();
        fs::write(lib.join("assets.toml"), "x").unwrap();
        assert!(remove_library(&lib, &[]).is_err(), "only a library Setup made is deleted");
        fs::write(lib.join(seed::RECORD), "{}").unwrap();
        fs::create_dir_all(lib.join("plugin-states")).unwrap();
        fs::write(lib.join("plugin-states/abc.vstpreset"), "s").unwrap();
        fs::create_dir_all(lib.join("Songs/Projects")).unwrap();
        let r = remove_library(&lib, &[lib.join("Songs")]).unwrap();
        assert_eq!(r.freed, 5 + 1 + 2);
        assert_eq!(r.kept.len(), 2);
        assert!(lib.join("plugin-states/abc.vstpreset").exists() && lib.join("Songs/Projects").exists());
        assert!(!lib.join("instruments").exists() && !lib.join("assets.toml").exists());

        // nothing to keep: the folder goes too
        let lib2 = t.path().join("Library2");
        fs::create_dir_all(lib2.join("plugin-states")).unwrap();
        fs::write(lib2.join(seed::RECORD), "{}").unwrap();
        remove_library(&lib2, &[]).unwrap();
        assert!(!lib2.exists());
    }

    #[test]
    fn finds_this_test_among_processes() {
        if cfg!(windows) {
            let me = std::env::current_exe().unwrap();
            let all = os::processes();
            // the test binary is called flux_setup-<hash>.exe
            assert!(all.iter().any(|(pid, p)| *pid == std::process::id() && fs::canonicalize(p).ok() == fs::canonicalize(&me).ok()), "{all:?}");
            assert!(running_from(me.parent().unwrap()).iter().all(|(pid, _)| *pid != std::process::id()), "this process is left out");
        }
    }
}
