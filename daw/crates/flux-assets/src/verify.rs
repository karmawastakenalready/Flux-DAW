//! `verify`: re-hash what is installed and compare it with the sources and the lock.
//!
//! * git sources: `HEAD` must be the pinned commit, the sparse patterns must match the manifest, and every
//!   checked-out file must hash to its git blob id. A file that matches once CRLF is turned back into LF is
//!   reported as `eol-only` (a `core.autocrlf` rewrite), anything else as `modified`.
//! * files / manual / vendored sources: every pinned file must have its SHA-256.
//! * the lock: the folder's tree index must equal the one recorded at install time. For archive sources this
//!   is the check (the archive itself is not kept in the folder).
//! * files that no source accounts for are reported as `extra`, unless listed in the asset's `ignore`.
//!
//! `--quick` compares names and sizes only (no hashing).

use crate::fsutil::{self, EntryKind};
use crate::git::Git;
use crate::glob;
use crate::hash::{self, HashProgress, HashedTree, is_eol_only_difference};
use crate::lock::{read_index, spec_digest};
use crate::manifest::{Asset, PinnedFile, Source};
use crate::workspace::{Ui, Workspace};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Overall result for one asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Everything matches.
    Ok,
    /// Something differs; see the issues.
    Drift,
    /// The folder does not exist (or is empty).
    NotInstalled,
    /// The asset is for another platform.
    NotApplicable,
    /// Installed outside the project (pip, vendor installer); nothing to check.
    External,
    /// Installed, no drift found, but parts could not be checked (archive source without a lock entry).
    Unverified,
    /// The check itself failed (e.g. git missing).
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Drift => "DRIFT",
            State::NotInstalled => "not installed",
            State::NotApplicable => "n/a (other platform)",
            State::External => "external",
            State::Unverified => "unverified",
            State::Error => "ERROR",
        }
    }
}

/// Kinds of differences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueKind {
    /// Expected file is not there.
    Missing,
    /// Content differs.
    Modified,
    /// Differs from the git blob only by CRLF line endings.
    EolOnly,
    /// File no source accounts for.
    Extra,
    /// Git checkout is at another commit.
    WrongCommit,
    /// Sparse-checkout patterns differ from the manifest.
    SparseMismatch,
    /// Symbolic link or submodule checked out.
    Special,
    /// Folder differs from the lock's tree index.
    LockDiffers,
    /// Note: the manifest entry changed since the lock was written.
    LockStale,
    /// Note: no lock entry.
    Unlocked,
    /// Note: the git checkout is not configured for byte-exact line endings.
    NotByteExact,
}

impl IssueKind {
    /// Whether this kind means the install differs from what it should be (as opposed to a note).
    pub fn is_drift(self) -> bool {
        !matches!(self, IssueKind::LockStale | IssueKind::Unlocked | IssueKind::NotByteExact)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IssueKind::Missing => "missing",
            IssueKind::Modified => "modified",
            IssueKind::EolOnly => "eol-only",
            IssueKind::Extra => "extra",
            IssueKind::WrongCommit => "wrong-commit",
            IssueKind::SparseMismatch => "sparse-mismatch",
            IssueKind::Special => "link-or-submodule",
            IssueKind::LockDiffers => "lock-differs",
            IssueKind::LockStale => "lock-stale",
            IssueKind::Unlocked => "unlocked",
            IssueKind::NotByteExact => "not-byte-exact",
        }
    }
}

/// One difference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub kind: IssueKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Issue {
    fn at(kind: IssueKind, path: &str) -> Self {
        Self { kind, path: Some(path.to_string()), detail: None }
    }

    fn note(kind: IssueKind, detail: String) -> Self {
        Self { kind, path: None, detail: Some(detail) }
    }
}

/// Result of verifying one asset.
#[derive(Clone, Debug, Serialize)]
pub struct AssetCheck {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub state: State,
    /// Regular files in the folder (`.git` excluded).
    pub files: u64,
    pub bytes: u64,
    /// SHA-256 of the folder's tree index (full mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_sha256: Option<String>,
    /// What was checked, one line each.
    pub checked: Vec<String>,
    pub issues: Vec<Issue>,
    /// Error message when `state` is `Error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub seconds: f64,
}

impl AssetCheck {
    fn new(a: &Asset, state: State) -> Self {
        Self {
            id: a.id.clone(),
            path: a.path.clone(),
            state,
            files: 0,
            bytes: 0,
            tree_sha256: None,
            checked: vec![],
            issues: vec![],
            error: None,
            seconds: 0.0,
        }
    }

    /// Number of issues of a kind.
    pub fn count(&self, kind: IssueKind) -> usize {
        self.issues.iter().filter(|i| i.kind == kind).count()
    }

    /// Issue counts by kind, e.g. `modified 3, eol-only 75`.
    pub fn summary(&self) -> String {
        let mut by: BTreeMap<IssueKind, usize> = BTreeMap::new();
        for i in &self.issues {
            *by.entry(i.kind).or_default() += 1;
        }
        by.iter().map(|(k, n)| format!("{} {n}", k.as_str())).collect::<Vec<_>>().join(", ")
    }

    pub fn has_drift(&self) -> bool {
        self.issues.iter().any(|i| i.kind.is_drift())
    }
}

/// Verification options.
#[derive(Clone, Copy, Debug, Default)]
pub struct VerifyOptions {
    /// Compare names and sizes only.
    pub quick: bool,
}

/// Verify assets; problems with one asset never stop the others.
pub fn verify(ws: &Workspace, assets: &[&Asset], opts: VerifyOptions, ui: &mut dyn Ui) -> Vec<AssetCheck> {
    assets.iter().map(|a| verify_asset(ws, a, opts, ui)).collect()
}

/// Verify one asset.
pub fn verify_asset(ws: &Workspace, a: &Asset, opts: VerifyOptions, ui: &mut dyn Ui) -> AssetCheck {
    let t0 = Instant::now();
    let mut c = match ws.asset_dir(a) {
        None => AssetCheck::new(a, State::External),
        Some(_) if !a.applies_here() => AssetCheck::new(a, State::NotApplicable),
        Some(_) if !ws.is_installed(a) => AssetCheck::new(a, State::NotInstalled),
        Some(dir) => {
            let mut c = AssetCheck::new(a, State::Ok);
            let r = if opts.quick { quick(ws, a, &dir, &mut c) } else { full(ws, a, &dir, &mut c, ui) };
            if let Err(e) = r {
                c.state = State::Error;
                c.error = Some(format!("{e:#}"));
            }
            c
        }
    };
    c.seconds = t0.elapsed().as_secs_f64();
    c
}

/// Hash a folder while reporting progress through `ui`.
pub fn hash_with_progress(dir: &Path, threads: usize, label: &str, ui: &mut dyn Ui) -> Result<HashedTree> {
    let (bytes_total, _) = fsutil::tree_size(dir, &[".git"]).unwrap_or((0, 0));
    let prog = HashProgress::default();
    let result = std::thread::scope(|s| {
        let h = s.spawn(|| hash::hash_tree(dir, threads, Some(&prog)));
        while !h.is_finished() {
            ui.progress(label, prog.bytes_done.load(Ordering::Relaxed), Some(bytes_total));
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        h.join().map_err(|_| anyhow::anyhow!("hashing thread panicked"))?
    });
    ui.progress_end();
    result
}

fn full(ws: &Workspace, a: &Asset, dir: &Path, c: &mut AssetCheck, ui: &mut dyn Ui) -> Result<()> {
    let tree = hash_with_progress(dir, ws.threads, &format!("hashing {}", a.id), ui)?;
    c.files = tree.index.len() as u64;
    c.bytes = tree.index.total_bytes();
    c.tree_sha256 = Some(tree.index.digest());
    let mut covered: BTreeSet<String> = BTreeSet::new();
    let mut reported: BTreeSet<String> = BTreeSet::new();
    let mut archive_only = false;
    for src in &a.sources {
        match src {
            Source::Git(g) => {
                let git = Git::find()?;
                check_git_meta(&git, dir, g, c)?;
                let cmp = compare_git(&git, dir, &tree)?;
                covered.extend(cmp.tracked.iter().cloned());
                for (kind, list) in [
                    (IssueKind::Special, &cmp.special),
                    (IssueKind::Missing, &cmp.missing),
                    (IssueKind::Modified, &cmp.modified),
                    (IssueKind::EolOnly, &cmp.eol_only),
                ] {
                    for p in list {
                        c.issues.push(Issue::at(kind, p));
                        reported.insert(p.clone());
                    }
                }
                c.checked.push(format!("git {}: {} of {} checked-out files match their blob ids", short(&g.rev), cmp.ok, cmp.checked_out));
            }
            Source::Files(f) => {
                let ok = check_pinned(
                    f.files.iter().map(|x| (join(&f.dest, &x.path), x.sha256.as_str(), x.size)),
                    &tree,
                    c,
                    &mut covered,
                    &mut reported,
                );
                c.checked.push(format!("files: {ok} of {} pinned files match their sha256", f.files.len()));
            }
            Source::Manual(m) => {
                let ok = check_pinned(pinned(&m.files), &tree, c, &mut covered, &mut reported);
                c.checked.push(format!("manual: {ok} of {} pinned files match their sha256", m.files.len()));
            }
            Source::Vendored(v) => {
                let ok = check_pinned(pinned(&v.files), &tree, c, &mut covered, &mut reported);
                c.checked.push(format!("vendored: {ok} of {} pinned files match their sha256", v.files.len()));
            }
            Source::Archive(_) => archive_only = true,
            Source::External(_) => {}
        }
    }
    // Lock comparison.
    match ws.lock.get(&a.id) {
        Some(le) => {
            if le.spec_sha256 != spec_digest(a) {
                c.issues.push(Issue::note(IssueKind::LockStale, "the manifest entry changed since it was locked; run `flux assets lock`".into()));
            }
            match read_index(&ws.lock_path, le) {
                Ok(idx) => {
                    let d = idx.diff(&tree.index);
                    if archive_only {
                        covered.extend(idx.entries().iter().map(|e| e.path.clone()));
                        for p in d.removed.iter().filter(|p| !reported.contains(*p)) {
                            c.issues.push(Issue::at(IssueKind::Missing, p));
                        }
                        for p in d.changed.iter().filter(|p| !reported.contains(*p)) {
                            c.issues.push(Issue::at(IssueKind::Modified, p));
                        }
                        reported.extend(d.removed.iter().chain(&d.changed).cloned());
                    } else if !d.is_empty() {
                        c.issues.push(Issue::note(
                            IssueKind::LockDiffers,
                            format!("{} changed, {} added, {} removed since locking", d.changed.len(), d.added.len(), d.removed.len()),
                        ));
                    }
                    c.checked.push(if d.is_empty() {
                        format!("lock: tree matches ({} files, {})", idx.len(), short(&le.tree_sha256))
                    } else {
                        format!("lock: tree differs from {} ({} changed, {} added, {} removed)", short(&le.tree_sha256), d.changed.len(), d.added.len(), d.removed.len())
                    });
                }
                Err(e) => c.issues.push(Issue::note(IssueKind::LockDiffers, format!("{e:#}"))),
            }
        }
        None => {
            c.issues.push(Issue::note(IssueKind::Unlocked, "no lock entry; run `flux assets lock`".into()));
            if archive_only {
                c.checked.push("archive: not checked (no lock entry to compare with)".into());
            }
        }
    }
    // Files nobody accounts for.
    let archive_unchecked = archive_only && ws.lock.get(&a.id).is_none();
    if !archive_unchecked {
        for e in tree.index.entries() {
            if !covered.contains(&e.path) && !a.ignore.iter().any(|g| glob::matches(g, &e.path)) {
                c.issues.push(Issue::at(IssueKind::Extra, &e.path));
            }
        }
    }
    c.state = if c.has_drift() {
        State::Drift
    } else if archive_unchecked {
        State::Unverified
    } else {
        State::Ok
    };
    Ok(())
}

/// How a git checkout compares with its index.
#[derive(Clone, Debug, Default)]
pub struct GitCompare {
    /// Entries checked out (not skip-worktree).
    pub checked_out: usize,
    /// Checked-out files whose content matches the blob.
    pub ok: usize,
    pub missing: Vec<String>,
    pub modified: Vec<String>,
    /// Differ only by CRLF line endings.
    pub eol_only: Vec<String>,
    /// Symbolic links or submodules that are checked out.
    pub special: Vec<String>,
    /// Every path in the index, checked out or not.
    pub tracked: BTreeSet<String>,
}

/// Compare a checkout's files (already hashed in `tree`) with the blob ids in its index.
pub fn compare_git(git: &Git, dir: &Path, tree: &HashedTree) -> Result<GitCompare> {
    let mut r = GitCompare::default();
    for e in git.ls_files(dir)? {
        r.tracked.insert(e.path.clone());
        if !e.is_checked_out() {
            continue;
        }
        r.checked_out += 1;
        if e.is_special() {
            r.special.push(e.path);
            continue;
        }
        match tree.git_blobs.get(&e.path) {
            None => r.missing.push(e.path),
            Some(b) if *b == e.blob => r.ok += 1,
            Some(_) => {
                let content = std::fs::read(dir.join(&e.path)).with_context(|| format!("reading {}", e.path))?;
                if is_eol_only_difference(&content, &e.blob) {
                    r.eol_only.push(e.path);
                } else {
                    r.modified.push(e.path);
                }
            }
        }
    }
    Ok(r)
}

fn check_git_meta(git: &Git, dir: &Path, g: &crate::manifest::GitSource, c: &mut AssetCheck) -> Result<()> {
    c.issues.extend(git_meta_issues(git, dir, g)?);
    Ok(())
}

/// Commit, sparse-pattern and line-ending configuration problems of a git checkout
/// (`WrongCommit`, `SparseMismatch`, `NotByteExact`).
pub fn git_meta_issues(git: &Git, dir: &Path, g: &crate::manifest::GitSource) -> Result<Vec<Issue>> {
    let mut out = Vec::new();
    let head = git.head(dir)?;
    if head != g.rev {
        out.push(Issue::note(IssueKind::WrongCommit, format!("HEAD is {}, the manifest pins {}", short(&head), short(&g.rev))));
    }
    let actual = git.sparse_patterns(dir)?;
    let want: BTreeSet<&str> = g.sparse.iter().map(String::as_str).collect();
    match &actual {
        None if !want.is_empty() => {
            out.push(Issue::note(IssueKind::SparseMismatch, "checkout is not sparse but the manifest lists patterns".into()))
        }
        Some((pats, cone)) => {
            let have: BTreeSet<&str> = pats.iter().map(String::as_str).collect();
            if *cone != g.cone && !want.is_empty() {
                out.push(Issue::note(
                    IssueKind::SparseMismatch,
                    format!("checkout uses {} mode, the manifest {} mode", mode_name(*cone), mode_name(g.cone)),
                ));
            } else if want.is_empty() {
                out.push(Issue::note(IssueKind::SparseMismatch, "checkout is sparse but the manifest wants the whole tree".into()));
            } else if have != want {
                let missing: Vec<&&str> = want.difference(&have).collect();
                let extra: Vec<&&str> = have.difference(&want).collect();
                out.push(Issue::note(
                    IssueKind::SparseMismatch,
                    format!("patterns missing from the checkout: {missing:?}; extra patterns: {extra:?}"),
                ));
            }
        }
        None => {}
    }
    if !git.is_byte_exact(dir) {
        out.push(Issue::note(IssueKind::NotByteExact, "core.autocrlf is not disabled in this checkout; run `flux assets repair`".into()));
    }
    Ok(out)
}

fn mode_name(cone: bool) -> &'static str {
    if cone { "cone" } else { "non-cone" }
}

fn pinned(files: &[PinnedFile]) -> impl Iterator<Item = (String, &str, u64)> {
    files.iter().map(|f| (f.path.clone(), f.sha256.as_str(), f.size))
}

fn check_pinned<'a>(
    files: impl Iterator<Item = (String, &'a str, u64)>,
    tree: &HashedTree,
    c: &mut AssetCheck,
    covered: &mut BTreeSet<String>,
    reported: &mut BTreeSet<String>,
) -> usize {
    let mut ok = 0;
    for (path, sha, _size) in files {
        covered.insert(path.clone());
        match tree.index.get(&path) {
            None => {
                c.issues.push(Issue::at(IssueKind::Missing, &path));
                reported.insert(path);
            }
            Some(e) if e.sha256.eq_ignore_ascii_case(sha) => ok += 1,
            Some(_) => {
                c.issues.push(Issue::at(IssueKind::Modified, &path));
                reported.insert(path);
            }
        }
    }
    ok
}

fn quick(ws: &Workspace, a: &Asset, dir: &Path, c: &mut AssetCheck) -> Result<()> {
    let walked = fsutil::walk(dir, &[".git"])?;
    let sizes: BTreeMap<&str, u64> =
        walked.iter().filter(|e| e.kind == EntryKind::File).map(|e| (e.rel.as_str(), e.size)).collect();
    c.files = sizes.len() as u64;
    c.bytes = sizes.values().sum();
    if let Some(bad) = walked.iter().find(|e| e.kind != EntryKind::File) {
        c.issues.push(Issue::at(IssueKind::Special, &bad.rel));
    }
    let mut covered: BTreeSet<String> = BTreeSet::new();
    let mut have_complete_list = false;
    if let Some(le) = ws.lock.get(&a.id) {
        let idx = read_index(&ws.lock_path, le)?;
        for e in idx.entries() {
            covered.insert(e.path.clone());
            match sizes.get(e.path.as_str()) {
                None => c.issues.push(Issue::at(IssueKind::Missing, &e.path)),
                Some(s) if *s != e.size => c.issues.push(Issue::at(IssueKind::Modified, &e.path)),
                Some(_) => {}
            }
        }
        have_complete_list = true;
        c.checked.push(format!("lock: names and sizes of {} files", idx.len()));
    } else {
        for src in &a.sources {
            let expect: Vec<(String, u64)> = match src {
                Source::Files(f) => f.files.iter().map(|x| (join(&f.dest, &x.path), x.size)).collect(),
                Source::Manual(m) => m.files.iter().map(|x| (x.path.clone(), x.size)).collect(),
                Source::Vendored(v) => v.files.iter().map(|x| (x.path.clone(), x.size)).collect(),
                Source::Git(_) => {
                    let git = Git::find()?;
                    for e in git.ls_files(dir)?.into_iter().filter(|e| e.is_checked_out()) {
                        covered.insert(e.path.clone());
                        if !sizes.contains_key(e.path.as_str()) {
                            c.issues.push(Issue::at(IssueKind::Missing, &e.path));
                        }
                    }
                    have_complete_list = true;
                    continue;
                }
                _ => continue,
            };
            for (p, s) in expect {
                match sizes.get(p.as_str()) {
                    None => c.issues.push(Issue::at(IssueKind::Missing, &p)),
                    Some(have) if *have != s => c.issues.push(Issue::at(IssueKind::Modified, &p)),
                    Some(_) => {}
                }
                covered.insert(p);
            }
        }
        c.issues.push(Issue::note(IssueKind::Unlocked, "no lock entry; run `flux assets lock`".into()));
        c.checked.push("existence and sizes from the manifest (no lock entry)".into());
    }
    if have_complete_list {
        for p in sizes.keys() {
            if !covered.contains(*p) && !a.ignore.iter().any(|g| glob::matches(g, p)) {
                c.issues.push(Issue::at(IssueKind::Extra, p));
            }
        }
    }
    c.state = if c.has_drift() {
        State::Drift
    } else if !have_complete_list {
        State::Unverified
    } else {
        State::Ok
    };
    Ok(())
}

fn join(dest: &str, path: &str) -> String {
    if dest.is_empty() { path.to_string() } else { format!("{dest}/{path}") }
}

/// First 7 characters of a hash.
pub fn short(h: &str) -> &str {
    &h[..h.len().min(7)]
}
