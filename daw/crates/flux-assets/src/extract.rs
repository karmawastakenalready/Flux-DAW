//! Safe archive extraction.
//!
//! Archives are untrusted input. Every entry name is checked before anything is written: absolute paths, drive
//! prefixes, UNC paths, `..`, NUL and control characters, characters Windows forbids, alternate data streams,
//! reserved device names (`CON`, `NUL`, `COM1`...), trailing dots or spaces, duplicates and names that differ
//! only in case are rejected. Symbolic links, hard links, devices, FIFOs and sparse entries are rejected. Files
//! are created with `create_new`, so nothing that already exists is ever overwritten or followed. Limits on the
//! number of entries, the size of one entry and the total size are enforced on the bytes actually written, not
//! on what the headers claim. Callers extract into a fresh staging folder and move it into place afterwards.
//!
//! ZIP, TAR, TAR.GZ and TAR.XZ are handled in Rust. RAR and 7z are handed to the system's libarchive `bsdtar`
//! (`C:\Windows\System32\tar.exe` on Windows); callers must only do that for archives whose SHA-256 was
//! verified against the manifest. The listing is validated with the same rules first, then the extracted tree
//! is compared with the listing (names, sizes, no links) before anything is used.

use crate::fsutil::{self, EntryKind};
use crate::glob;
use crate::hash::{IndexEntry, TreeIndex, sha256_reader};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// Archive container formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum ArchiveFormat {
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "tar")]
    Tar,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.xz")]
    TarXz,
    /// Extracted with the system `bsdtar`.
    #[serde(rename = "rar")]
    Rar,
    /// Extracted with the system `bsdtar`.
    #[serde(rename = "7z")]
    SevenZ,
}

impl ArchiveFormat {
    /// Whether extraction needs the external `bsdtar`.
    pub fn is_external(self) -> bool {
        matches!(self, ArchiveFormat::Rar | ArchiveFormat::SevenZ)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Tar => "tar",
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::TarXz => "tar.xz",
            ArchiveFormat::Rar => "rar",
            ArchiveFormat::SevenZ => "7z",
        }
    }

    /// Guess from a file name (`.zip`, `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.rar`, `.7z`).
    pub fn from_file_name(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        [
            (".tar.gz", ArchiveFormat::TarGz),
            (".tgz", ArchiveFormat::TarGz),
            (".tar.xz", ArchiveFormat::TarXz),
            (".txz", ArchiveFormat::TarXz),
            (".tar", ArchiveFormat::Tar),
            (".zip", ArchiveFormat::Zip),
            (".rar", ArchiveFormat::Rar),
            (".7z", ArchiveFormat::SevenZ),
        ]
        .into_iter()
        .find(|(ext, _)| n.ends_with(ext))
        .map(|(_, f)| f)
    }
}

/// Resource limits for one extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Total bytes of all extracted files.
    pub max_total_bytes: u64,
    /// Number of entries (files and folders).
    pub max_entries: usize,
    /// Bytes of a single file.
    pub max_entry_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_total_bytes: 16_000_000_000, max_entries: 200_000, max_entry_bytes: 8_000_000_000 }
    }
}

/// How to map archive entries to files.
#[derive(Clone, Debug, Default)]
pub struct ExtractOptions {
    /// Leading path components to drop (like `tar --strip-components`).
    pub strip: usize,
    /// Keep only entries whose (stripped) path matches one of these globs; empty keeps everything.
    pub include: Vec<String>,
    pub limits: Limits,
    /// For RAR/7z: entries `bsdtar` cannot name and skips. Must match exactly.
    pub skipped_entries: usize,
}

/// What an extraction did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractReport {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    /// Entries left out by `include`.
    pub filtered_out: usize,
    /// Entries the external archiver skipped because it could not represent their names.
    pub unreadable_skipped: usize,
}

/// Check and normalise an archive entry name. Returns `None` for names that denote the archive root (`./`).
pub fn sanitize_entry_path(raw: &str) -> Result<Option<String>> {
    if raw.contains('\0') {
        bail!("entry name {raw:?} contains a NUL byte");
    }
    let s = raw.replace('\\', "/");
    if s.starts_with('/') {
        bail!("entry {raw:?} has an absolute path");
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        bail!("entry {raw:?} has a drive prefix");
    }
    let mut parts = Vec::new();
    for c in s.split('/') {
        match c {
            "" | "." => continue,
            ".." => bail!("entry {raw:?} contains '..'"),
            _ => {}
        }
        check_component(c).with_context(|| format!("entry {raw:?}"))?;
        parts.push(c);
    }
    Ok(if parts.is_empty() { None } else { Some(parts.join("/")) })
}

fn check_component(c: &str) -> Result<()> {
    if c.len() > 255 {
        bail!("a path component is longer than 255 bytes");
    }
    if let Some(ch) = c.chars().find(|&ch| (ch as u32) < 0x20 || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
        bail!("name contains the character {ch:?}, which is not allowed in file names on Windows");
    }
    if c.ends_with('.') || c.ends_with(' ') {
        bail!("component {c:?} ends with a dot or space");
    }
    let stem = c.split('.').next().unwrap_or(c).trim_end().to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|d| {
            matches!(d, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        })
    };
    let reserved =
        matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT");
    if reserved {
        bail!("{c:?} is a reserved device name on Windows");
    }
    Ok(())
}

/// Apply `strip` and `include` to a sanitised path. `Ok(None)` means "leave this entry out".
fn map_path(path: &str, is_dir: bool, opts: &ExtractOptions) -> Result<Option<String>> {
    let comps: Vec<&str> = path.split('/').collect();
    if comps.len() <= opts.strip {
        if is_dir {
            return Ok(None);
        }
        bail!("file {path:?} has fewer than {} leading folders to strip", opts.strip);
    }
    let rest = comps[opts.strip..].join("/");
    if !opts.include.is_empty() && !is_dir && !opts.include.iter().any(|g| glob::matches(g, &rest)) {
        return Ok(None);
    }
    Ok(Some(rest))
}

/// Receives the entries of an archive after validation.
trait Sink {
    fn dir(&mut self, path: &str) -> Result<()>;
    /// Consume the file content; returns the bytes read.
    fn file(&mut self, path: &str, content: &mut dyn Read, limit: u64) -> Result<u64>;
}

/// Tracks names already used, case-insensitively, so `a.txt` and `A.TXT` cannot both appear.
#[derive(Default)]
struct Names {
    seen: HashMap<String, bool>,
}

impl Names {
    fn add(&mut self, path: &str, is_dir: bool) -> Result<()> {
        // Every parent is implicitly a folder.
        let mut acc = String::new();
        let comps: Vec<&str> = path.split('/').collect();
        for (i, c) in comps.iter().enumerate() {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(&c.to_lowercase());
            let here_is_dir = i + 1 < comps.len() || is_dir;
            match self.seen.get(&acc) {
                Some(&was_dir) if was_dir && here_is_dir => {}
                Some(_) => bail!("entry {path:?} collides with another entry (same name ignoring case, or file vs folder)"),
                None => {
                    self.seen.insert(acc.clone(), here_is_dir);
                }
            }
        }
        Ok(())
    }
}

/// Counts entries and bytes against the limits.
struct Budget {
    limits: Limits,
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            bail!("archive has more than {} entries (limit)", self.limits.max_entries);
        }
        Ok(())
    }

    /// Bytes the next file may still have.
    fn file_limit(&self) -> u64 {
        self.limits.max_entry_bytes.min(self.limits.max_total_bytes.saturating_sub(self.bytes))
    }

    fn add_bytes(&mut self, path: &str, n: u64) -> Result<()> {
        if n > self.limits.max_entry_bytes {
            bail!("{path:?} is larger than the per-file limit of {} bytes", self.limits.max_entry_bytes);
        }
        self.bytes += n;
        if self.bytes > self.limits.max_total_bytes {
            bail!("archive expands to more than {} bytes (limit)", self.limits.max_total_bytes);
        }
        Ok(())
    }
}

/// Copy at most `limit` bytes; one byte more is an error (so oversized entries are caught on real data).
fn copy_limited(src: &mut dyn Read, dst: &mut dyn Write, limit: u64, path: &str) -> Result<u64> {
    let n = io::copy(&mut src.take(limit.saturating_add(1)), dst).with_context(|| format!("extracting {path:?}"))?;
    if n > limit {
        bail!("{path:?} is larger than the remaining size limit ({limit} bytes)");
    }
    Ok(n)
}

struct WriteSink<'a> {
    dest: &'a Path,
}

impl Sink for WriteSink<'_> {
    fn dir(&mut self, path: &str) -> Result<()> {
        let p = self.dest.join(path);
        fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))
    }

    fn file(&mut self, path: &str, content: &mut dyn Read, limit: u64) -> Result<u64> {
        let p = self.dest.join(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .with_context(|| format!("creating {} (it must not exist yet)", p.display()))?;
        let mut w = io::BufWriter::with_capacity(1 << 20, f);
        let n = copy_limited(content, &mut w, limit, path)?;
        let f = w.into_inner().map_err(|e| e.into_error()).with_context(|| format!("writing {}", p.display()))?;
        f.sync_all().with_context(|| format!("flushing {}", p.display()))?;
        Ok(n)
    }
}

struct HashSink {
    entries: Vec<IndexEntry>,
}

impl Sink for HashSink {
    fn dir(&mut self, _path: &str) -> Result<()> {
        Ok(())
    }

    fn file(&mut self, path: &str, content: &mut dyn Read, limit: u64) -> Result<u64> {
        let mut limited = content.take(limit.saturating_add(1));
        let (sha256, n) = sha256_reader(&mut limited).with_context(|| format!("reading {path:?}"))?;
        if n > limit {
            bail!("{path:?} is larger than the remaining size limit ({limit} bytes)");
        }
        self.entries.push(IndexEntry { path: path.to_string(), size: n, sha256 });
        Ok(n)
    }
}

/// Shared per-entry logic: validate, map, check limits, then hand the entry to the sink.
struct Walker<'a> {
    opts: &'a ExtractOptions,
    names: Names,
    budget: Budget,
    report: ExtractReport,
}

impl<'a> Walker<'a> {
    fn new(opts: &'a ExtractOptions) -> Self {
        Self {
            opts,
            names: Names::default(),
            budget: Budget { limits: opts.limits, entries: 0, bytes: 0 },
            report: ExtractReport::default(),
        }
    }

    fn dir(&mut self, sink: &mut dyn Sink, raw: &str) -> Result<()> {
        self.budget.entry()?;
        let Some(clean) = sanitize_entry_path(raw)? else { return Ok(()) };
        self.names.add(&clean, true)?;
        if let Some(mapped) = map_path(&clean, true, self.opts)? {
            sink.dir(&mapped)?;
            self.report.dirs += 1;
        }
        Ok(())
    }

    fn file(&mut self, sink: &mut dyn Sink, raw: &str, content: &mut dyn Read) -> Result<()> {
        self.budget.entry()?;
        let clean = sanitize_entry_path(raw)?.with_context(|| format!("file entry {raw:?} has no name"))?;
        self.names.add(&clean, false)?;
        match map_path(&clean, false, self.opts)? {
            Some(mapped) => {
                let n = sink.file(&mapped, content, self.budget.file_limit())?;
                self.budget.add_bytes(&clean, n)?;
                self.report.files += 1;
                self.report.bytes += n;
            }
            None => {
                // Still read it, so limits apply to what the archive really contains.
                let n = copy_limited(content, &mut io::sink(), self.budget.file_limit(), &clean)?;
                self.budget.add_bytes(&clean, n)?;
                self.report.filtered_out += 1;
            }
        }
        Ok(())
    }
}

/// Extract `archive` into `dest` (created if missing). Existing files in `dest` are never overwritten.
pub fn extract(archive: &Path, format: ArchiveFormat, dest: &Path, opts: &ExtractOptions) -> Result<ExtractReport> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    if format.is_external() {
        return extract_external(archive, dest, opts);
    }
    let mut sink = WriteSink { dest };
    walk_native(archive, format, opts, &mut sink).with_context(|| format!("extracting {}", archive.display()))
}

/// SHA-256 index of the files an extraction would produce, without writing them (ZIP and TAR family). RAR and
/// 7z are extracted into a temporary folder inside `work_dir` and hashed there.
pub fn index_archive(archive: &Path, format: ArchiveFormat, opts: &ExtractOptions, work_dir: &Path) -> Result<(TreeIndex, ExtractReport)> {
    if format.is_external() {
        fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
        let tmp = tempfile::Builder::new()
            .prefix(".flux-index-")
            .tempdir_in(work_dir)
            .with_context(|| format!("creating a temporary folder in {}", work_dir.display()))?;
        let report = extract_external(archive, tmp.path(), opts)?;
        let tree = crate::hash::hash_tree(tmp.path(), crate::hash::default_threads(), None)?;
        return Ok((tree.index, report));
    }
    let mut sink = HashSink { entries: Vec::new() };
    let report = walk_native(archive, format, opts, &mut sink).with_context(|| format!("reading {}", archive.display()))?;
    Ok((TreeIndex::new(sink.entries)?, report))
}

fn walk_native(archive: &Path, format: ArchiveFormat, opts: &ExtractOptions, sink: &mut dyn Sink) -> Result<ExtractReport> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut w = Walker::new(opts);
    match format {
        ArchiveFormat::Zip => walk_zip(BufReader::new(file), &mut w, sink)?,
        ArchiveFormat::Tar => walk_tar(BufReader::new(file), &mut w, sink)?,
        ArchiveFormat::TarGz => walk_tar(flate2::read::MultiGzDecoder::new(BufReader::new(file)), &mut w, sink)?,
        ArchiveFormat::TarXz => {
            // 1 GiB of decoder memory is far above any real preset (xz -9 uses 64 MiB) but stops absurd headers.
            let xz = lzma_rust2::XzReader::new_mem_limit(BufReader::new(file), true, 1 << 20);
            walk_tar(xz, &mut w, sink)?
        }
        ArchiveFormat::Rar | ArchiveFormat::SevenZ => unreachable!("external formats are handled elsewhere"),
    }
    Ok(w.report)
}

fn walk_zip<R: Read + io::Seek>(reader: R, w: &mut Walker, sink: &mut dyn Sink) -> Result<()> {
    let mut zip = zip::ZipArchive::new(reader).context("not a valid ZIP archive")?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).with_context(|| format!("reading ZIP entry {i}"))?;
        let name = f.name().to_string();
        if f.encrypted() {
            bail!("ZIP entry {name:?} is encrypted");
        }
        if let Some(mode) = f.unix_mode() {
            match mode & 0o170000 {
                0 | 0o100000 | 0o040000 => {}
                0o120000 => bail!("ZIP entry {name:?} is a symbolic link"),
                other => bail!("ZIP entry {name:?} is a special file (mode {other:o})"),
            }
        }
        if f.is_dir() {
            w.dir(sink, &name)?;
        } else {
            w.file(sink, &name, &mut f)?;
        }
    }
    Ok(())
}

fn walk_tar<R: Read>(reader: R, w: &mut Walker, sink: &mut dyn Sink) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    for entry in ar.entries().context("not a valid TAR archive")? {
        let mut e = entry.context("reading a TAR entry")?;
        let raw = e.path_bytes();
        let name = std::str::from_utf8(&raw).map_err(|_| anyhow::anyhow!("TAR entry name {:?} is not UTF-8", String::from_utf8_lossy(&raw)))?.to_string();
        let ty = e.header().entry_type();
        if ty.is_pax_global_extensions() || ty.is_pax_local_extensions() || ty.is_gnu_longname() || ty.is_gnu_longlink() {
            continue;
        }
        if ty.is_dir() {
            w.dir(sink, &name)?;
        } else if ty.is_file() || ty.is_contiguous() {
            w.file(sink, &name, &mut e)?;
        } else if ty.is_symlink() {
            bail!("TAR entry {name:?} is a symbolic link");
        } else if ty.is_hard_link() {
            bail!("TAR entry {name:?} is a hard link");
        } else if ty.is_gnu_sparse() {
            bail!("TAR entry {name:?} is a sparse file");
        } else {
            bail!("TAR entry {name:?} is a special file (type {:?})", ty.as_byte() as char);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// External bsdtar for RAR / 7z

/// The system libarchive `bsdtar`. On Windows this is always `%SystemRoot%\System32\tar.exe`, never a `tar`
/// found on PATH (Git for Windows puts GNU tar there, which cannot read RAR).
pub fn system_bsdtar() -> Result<PathBuf> {
    let exe = if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32").join("tar.exe")
    } else {
        PathBuf::from("bsdtar")
    };
    let out = crate::fsutil::quiet_command(&exe)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} (needed for RAR/7z archives)", exe.display()))?;
    let v = String::from_utf8_lossy(&out.stdout);
    if !v.contains("bsdtar") || !v.contains("libarchive") {
        bail!("{} is not libarchive's bsdtar (it says {:?})", exe.display(), v.trim());
    }
    Ok(exe)
}

/// One line of `bsdtar -tv`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    is_dir: bool,
    size: u64,
    name: String,
}

const UNREADABLE: &str = "Archive entry has empty or unreadable filename";

/// Run bsdtar and split its stderr into "unreadable name" skips and real problems.
fn run_bsdtar(exe: &Path, args: &[&std::ffi::OsStr]) -> Result<(String, usize)> {
    let out = crate::fsutil::quiet_command(exe).args(args).output().with_context(|| format!("running {}", exe.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut unreadable = 0;
    let mut problems = Vec::new();
    for line in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.contains(UNREADABLE) {
            unreadable += 1;
        } else if !line.contains("Error exit delayed from previous errors") {
            problems.push(line.to_string());
        }
    }
    if !problems.is_empty() || (!out.status.success() && unreadable == 0) {
        bail!("{} failed ({}): {}", exe.display(), out.status, problems.join("; "));
    }
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), unreadable))
}

/// Split off the first `n` whitespace-separated tokens and return them with the rest of the line.
fn split_tokens(line: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut toks = Vec::with_capacity(n);
    let mut rest = line;
    for _ in 0..n {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        toks.push(&rest[..end]);
        rest = &rest[end..];
    }
    let rest = rest.strip_prefix(' ').unwrap_or(rest).trim_start_matches(' ');
    Some((toks, rest))
}

fn parse_listing(text: &str) -> Result<Vec<Listed>> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        // mode nlink owner group size month day year|time name
        let (t, name) = split_tokens(line, 8).with_context(|| format!("cannot parse listing line {line:?}"))?;
        let kind = t[0].chars().next().unwrap_or('?');
        let size: u64 = t[4].parse().with_context(|| format!("bad size in listing line {line:?}"))?;
        match kind {
            '-' => out.push(Listed { is_dir: false, size, name: name.to_string() }),
            'd' => out.push(Listed { is_dir: true, size: 0, name: name.to_string() }),
            'l' => bail!("archive entry {name:?} is a symbolic link"),
            'h' => bail!("archive entry {name:?} is a hard link"),
            _ => bail!("archive entry {name:?} is a special file ({kind})"),
        }
        if name.contains(" -> ") || name.contains(" link to ") {
            bail!("archive entry {name:?} looks like a link");
        }
    }
    Ok(out)
}

fn extract_external(archive: &Path, dest: &Path, opts: &ExtractOptions) -> Result<ExtractReport> {
    let exe = system_bsdtar()?;
    let (listing, unreadable) = run_bsdtar(&exe, &["-tvf".as_ref(), archive.as_os_str()])
        .with_context(|| format!("listing {}", archive.display()))?;
    if unreadable != opts.skipped_entries {
        bail!(
            "{}: the system archiver could not read the names of {unreadable} entries (the manifest expects {}); \
             refusing to extract an incomplete or unexpected archive",
            archive.display(),
            opts.skipped_entries
        );
    }
    let listed = parse_listing(&listing)?;
    // Validate the listing with the same rules as native formats before extracting anything.
    let mut names = Names::default();
    let mut budget = Budget { limits: opts.limits, entries: 0, bytes: 0 };
    let mut files: BTreeMap<String, u64> = BTreeMap::new();
    for l in &listed {
        budget.entry()?;
        let Some(clean) = sanitize_entry_path(&l.name)? else { continue };
        names.add(&clean, l.is_dir)?;
        if !l.is_dir {
            budget.add_bytes(&clean, l.size)?;
            files.insert(clean, l.size);
        }
    }

    // Extract everything into a private folder next to `dest`, compare with the listing, then move the
    // selected files into `dest`.
    let parent = fsutil::parent_dir(dest);
    let raw = tempfile::Builder::new()
        .prefix(".flux-raw-")
        .tempdir_in(parent)
        .with_context(|| format!("creating a temporary folder in {}", parent.display()))?;
    let (_, unreadable2) = run_bsdtar(&exe, &["-xf".as_ref(), archive.as_os_str(), "-C".as_ref(), raw.path().as_os_str()])
        .with_context(|| format!("extracting {}", archive.display()))?;
    if unreadable2 != opts.skipped_entries {
        bail!("{}: extraction skipped {unreadable2} entries, expected {}", archive.display(), opts.skipped_entries);
    }
    let walked = fsutil::walk(raw.path(), &[])?;
    let mut found: BTreeMap<String, u64> = BTreeMap::new();
    for e in &walked {
        if e.kind != EntryKind::File {
            bail!("extraction produced {} which is not a regular file", e.path.display());
        }
        found.insert(e.rel.clone(), e.size);
    }
    if found != files {
        let extra: Vec<&String> = found.keys().filter(|k| !files.contains_key(*k)).take(5).collect();
        let missing: Vec<&String> = files.keys().filter(|k| !found.contains_key(*k)).take(5).collect();
        let resized: Vec<&String> = files.iter().filter(|(k, v)| found.get(*k).is_some_and(|s| s != *v)).map(|(k, _)| k).take(5).collect();
        bail!(
            "extracted files do not match the archive listing (unexpected: {extra:?}, missing: {missing:?}, size differs: {resized:?})"
        );
    }
    let mut report = ExtractReport { unreadable_skipped: unreadable, ..Default::default() };
    for l in listed.iter().filter(|l| l.is_dir) {
        if let Some(clean) = sanitize_entry_path(&l.name)?
            && let Some(mapped) = map_path(&clean, true, opts)?
        {
            fs::create_dir_all(dest.join(&mapped)).with_context(|| format!("creating {}", dest.join(&mapped).display()))?;
            report.dirs += 1;
        }
    }
    for (path, size) in &files {
        match map_path(path, false, opts)? {
            Some(mapped) => {
                let to = dest.join(&mapped);
                if to.exists() {
                    bail!("{} already exists", to.display());
                }
                if let Some(p) = to.parent() {
                    fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
                }
                fsutil::rename_with_retry(&raw.path().join(path), &to)?;
                report.files += 1;
                report.bytes += size;
            }
            None => report.filtered_out += 1,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        let ok = |s: &str| sanitize_entry_path(s).unwrap();
        assert_eq!(ok("a/b/c.txt").as_deref(), Some("a/b/c.txt"));
        assert_eq!(ok("./a//b/./c").as_deref(), Some("a/b/c"));
        assert_eq!(ok("dir\\file.wav").as_deref(), Some("dir/file.wav"));
        assert_eq!(ok("./"), None);
        assert_eq!(ok("見てね♡/説明.txt").as_deref(), Some("見てね♡/説明.txt"));
        for bad in [
            "/etc/passwd", "../x", "a/../../x", "C:/Windows/x", "c:x", "\\\\server\\share\\x", "a/b:stream", "a\0b",
            "NUL", "con.txt", "dir/COM1.log", "lpt9", "trailing.", "space ", "what?", "a|b", "tab\tname",
        ] {
            assert!(sanitize_entry_path(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(sanitize_entry_path("CONSOLE.txt").is_ok());
        assert!(sanitize_entry_path("COM10").is_ok());
    }

    #[test]
    fn case_collisions() {
        let mut n = Names::default();
        n.add("a/b.txt", false).unwrap();
        n.add("a/c.txt", false).unwrap();
        n.add("A", true).unwrap();
        assert!(n.add("a/B.TXT", false).is_err());
        assert!(n.add("a/b.txt/x", false).is_err(), "a file cannot also be a folder");
        assert!(n.add("a", false).is_err(), "a folder cannot also be a file");
    }

    #[test]
    fn strip_and_include() {
        let o = ExtractOptions { strip: 1, include: vec!["bin/**".into()], ..Default::default() };
        assert_eq!(map_path("top/bin/x.dll", false, &o).unwrap().as_deref(), Some("bin/x.dll"));
        assert_eq!(map_path("top/doc/readme", false, &o).unwrap(), None);
        assert_eq!(map_path("top", true, &o).unwrap(), None);
        assert!(map_path("loose.txt", false, &o).is_err());
    }

    #[test]
    fn parses_bsdtar_listing() {
        let l = parse_listing(
            "-rw-r--r--  0 0      0         106 Jun 25  2019 UI_METAL-GTX/GUI/aria_info.xml\n\
             drwxr-xr-x  0 0      0           0 Jul 07  2019 UI_METAL-GTX/\n\
             -rw-r--r--  0 0      0          12 Jul 07 12:30 dir/name with  spaces.txt\n",
        )
        .unwrap();
        assert_eq!(l.len(), 3);
        assert_eq!(l[0], Listed { is_dir: false, size: 106, name: "UI_METAL-GTX/GUI/aria_info.xml".into() });
        assert!(l[1].is_dir);
        assert_eq!(l[2].name, "dir/name with  spaces.txt");
        assert!(parse_listing("lrwxr-xr-x  0 0 0 0 Jul 07  2019 evil -> /etc\n").is_err());
        assert!(parse_listing("hrw-r--r--  0 0 0 0 Jul 07  2019 a link to b\n").is_err());
    }

    #[test]
    fn formats_from_names() {
        assert_eq!(ArchiveFormat::from_file_name("x.tar.gz"), Some(ArchiveFormat::TarGz));
        assert_eq!(ArchiveFormat::from_file_name("X.ZIP"), Some(ArchiveFormat::Zip));
        assert_eq!(ArchiveFormat::from_file_name("x.bin"), None);
    }
}
