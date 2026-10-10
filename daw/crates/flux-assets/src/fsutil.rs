//! Filesystem helpers: atomic writes, directory walks that never follow links, retrying renames,
//! byte-size formatting and parsing, and UTC timestamps.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A command for a helper program (git, tar) that opens no console window of its own, so running it from Flux's
/// window does not flash one up. Its output must be captured (a child without a console cannot write to ours).
pub fn quiet_command(exe: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Write `bytes` to `path` atomically: a temporary file in the same directory is written, flushed to disk and
/// then renamed over `path`, so readers never see a half-written file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = parent_dir(path);
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".flux-tmp-")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(bytes).with_context(|| format!("writing {}", tmp.path().display()))?;
    tmp.as_file().sync_all().with_context(|| format!("flushing {}", tmp.path().display()))?;
    tmp.persist(path).map_err(|e| e.error).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The directory containing `path` (`.` for a bare file name).
pub fn parent_dir(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

/// What a directory entry is. Links are reported, never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    /// A symbolic link, Windows junction or other name-surrogate reparse point.
    Link,
    /// Anything else that is not a regular file or directory (device, pipe, socket).
    Other,
}

/// One entry found by [`walk`].
#[derive(Clone, Debug)]
pub struct WalkEntry {
    /// Path relative to the walked root, `/`-separated.
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
    pub kind: EntryKind,
}

/// List every non-directory entry below `root`, sorted by relative path (byte order).
///
/// Directories named in `skip_dirs` (matched against the directory's own name, at any depth) are not entered;
/// `.git` is the usual one. Links are listed with [`EntryKind::Link`] and not followed. File names that are not
/// valid Unicode, or that contain a tab or newline (which would make indexes ambiguous), are errors.
pub fn walk(root: &Path, skip_dirs: &[&str]) -> Result<Vec<WalkEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let rd = fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
        for ent in rd {
            let ent = ent.with_context(|| format!("listing {}", dir.display()))?;
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                bail!("file name is not valid Unicode: {}", ent.path().display());
            };
            if name.contains(['\t', '\n', '\r']) {
                bail!("file name contains a tab or newline: {:?}", ent.path().display());
            }
            let child_rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
            let ft = ent.file_type().with_context(|| format!("reading the type of {}", ent.path().display()))?;
            if ft.is_dir() {
                if !skip_dirs.contains(&name) {
                    stack.push((ent.path(), child_rel));
                }
                continue;
            }
            let (kind, size) = if ft.is_symlink() {
                (EntryKind::Link, 0)
            } else if ft.is_file() {
                let md = ent.metadata().with_context(|| format!("reading {}", ent.path().display()))?;
                (EntryKind::File, md.len())
            } else {
                (EntryKind::Other, 0)
            };
            out.push(WalkEntry { rel: child_rel, path: ent.path(), size, kind });
        }
    }
    out.sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    Ok(out)
}

/// Total size in bytes and number of regular files below `root` (links not followed, `skip_dirs` skipped).
pub fn tree_size(root: &Path, skip_dirs: &[&str]) -> Result<(u64, u64)> {
    let entries = walk(root, skip_dirs)?;
    let files = entries.iter().filter(|e| e.kind == EntryKind::File);
    Ok(files.fold((0, 0), |(b, n), e| (b + e.size, n + 1)))
}

/// `fs::rename` that retries for a few seconds when Windows reports the path as in use (antivirus scanners and
/// indexers briefly hold freshly written files open).
pub fn rename_with_retry(from: &Path, to: &Path) -> Result<()> {
    let mut attempt = 0u32;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 20 && is_transient(&e) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("moving {} to {}", from.display(), to.display()));
            }
        }
    }
}

fn is_transient(e: &std::io::Error) -> bool {
    // 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION, 33 = ERROR_LOCK_VIOLATION
    e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// Lexically normalise a path: drop `.` components and resolve `..` against earlier components.
/// Leading `..` that cannot be resolved are kept.
pub fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// `path` relative to `root` as a `/`-separated string, if it lies inside `root` (compared lexically and,
/// on Windows, case-insensitively).
pub fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let root = normalize_lexically(root);
    let path = normalize_lexically(path);
    let rs: Vec<String> = root.components().map(|c| fold_case(&c.as_os_str().to_string_lossy())).collect();
    let ps: Vec<Component> = path.components().collect();
    if ps.len() < rs.len() {
        return None;
    }
    for (r, p) in rs.iter().zip(&ps) {
        if *r != fold_case(&p.as_os_str().to_string_lossy()) {
            return None;
        }
    }
    let rest: Vec<String> = ps[rs.len()..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Some(rest.join("/"))
}

fn fold_case(s: &str) -> String {
    if cfg!(windows) { s.to_lowercase() } else { s.to_string() }
}

/// Format a byte count with decimal units, e.g. `1.34 GB`.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else if v >= 100.0 { format!("{v:.0} {}", UNITS[u]) } else { format!("{v:.2} {}", UNITS[u]) }
}

/// Parse a human byte size: `123`, `500M`, `500MB`, `2G`, `2 GB`, `1.5GiB`, `64k`. Decimal units are powers
/// of 1000, `KiB`/`MiB`/`GiB`/`TiB` powers of 1024.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let num: f64 = num.parse().with_context(|| format!("'{s}' is not a size (e.g. 500M, 2G)"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0f64.powi(4),
        u => bail!("unknown size unit '{u}' in '{s}' (use B, k, M, G, T, KiB, MiB, GiB)"),
    };
    let v = num * mult;
    if !v.is_finite() || v < 0.0 || v > u64::MAX as f64 {
        bail!("size '{s}' is out of range");
    }
    Ok(v.round() as u64)
}

/// The current time as an RFC 3339 UTC timestamp with second precision, e.g. `2026-10-08T13:05:00Z`.
pub fn now_rfc3339() -> String {
    rfc3339_utc(SystemTime::now())
}

/// Format a time as an RFC 3339 UTC timestamp with second precision.
pub fn rfc3339_utc(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(UNIX_EPOCH + Duration::from_secs(1_791_464_706)), "2026-10-08T13:05:06Z");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("123").unwrap(), 123);
        assert_eq!(parse_size("500M").unwrap(), 500_000_000);
        assert_eq!(parse_size("2 GB").unwrap(), 2_000_000_000);
        assert_eq!(parse_size("1.5GiB").unwrap(), 1_610_612_736);
        assert_eq!(parse_size("64k").unwrap(), 64_000);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5 parsecs").is_err());
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_343_285_011), "1.34 GB");
        assert_eq!(fmt_bytes(748_600_000), "749 MB");
    }

    #[test]
    fn lexical_paths() {
        assert_eq!(normalize_lexically(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize_lexically(Path::new("../x")), PathBuf::from("../x"));
        let root = Path::new("/music");
        assert_eq!(relative_to(root, Path::new("/music/sfz/../instruments/VSCO/x.sfz")).as_deref(), Some("instruments/VSCO/x.sfz"));
        assert_eq!(relative_to(root, Path::new("/other/x")), None);
        assert_eq!(relative_to(root, Path::new("/music")).as_deref(), Some(""));
    }

    #[test]
    fn walk_sorts_and_skips() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("b/.git")).unwrap();
        fs::write(d.path().join("b/.git/config"), "x").unwrap();
        fs::write(d.path().join("b/z.txt"), "zz").unwrap();
        fs::write(d.path().join("a.txt"), "a").unwrap();
        let w = walk(d.path(), &[".git"]).unwrap();
        let names: Vec<&str> = w.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(names, ["a.txt", "b/z.txt"]);
        assert_eq!(tree_size(d.path(), &[".git"]).unwrap(), (3, 2));
    }

    #[test]
    fn atomic_write_replaces() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/f.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(p.parent().unwrap()).unwrap().count(), 1, "no temporary files left behind");
    }
}
