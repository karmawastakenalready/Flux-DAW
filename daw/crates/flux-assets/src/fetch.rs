//! `fetch` (install from official sources), `lock` (record an existing install after checking it against its
//! sources) and `repair` (put a drifted install back to what the manifest pins).
//!
//! A fresh install is built in a staging folder next to its final place (same disk), verified, and only then
//! renamed into place, so an interrupted fetch never leaves a half-installed asset behind. Existing installs are
//! never touched unless `--force` (replace) or `repair` is used. Downloads above the approval threshold
//! (2 GB by default) are refused unless the limit is raised explicitly with `--max-size`.

use crate::extract::{self, ExtractOptions, Limits};
use crate::fsutil;
use crate::hash::{self, TreeIndex};
use crate::http::{Expect, HttpClient};
use crate::lock::{BusyGuard, LockMethod, LockedAsset, LockedSource, spec_digest, write_index};
use crate::manifest::{ArchiveSource, Asset, FileSpec, FilesSource, PinnedFile, Source};
use crate::verify::{self, IssueKind, VerifyOptions, compare_git, hash_with_progress};
use crate::workspace::{Ui, Workspace};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Options for [`fetch`].
#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    /// Fetch every required asset and every installed optional one (when no ids are given).
    pub all: bool,
    /// Do not ask before downloading (sizes above the limit are still refused).
    pub yes: bool,
    /// Per-asset download limit; defaults to the manifest's approval threshold.
    pub max_size: Option<u64>,
    /// Only show the plan.
    pub dry_run: bool,
    /// With `dry_run`: contact every source to check it is reachable and has the expected size.
    pub check_urls: bool,
    /// Replace existing installs.
    pub force: bool,
    /// Delete downloaded archives once their asset is installed (Setup does, to save space; by default they stay
    /// cached so nothing is downloaded twice).
    pub drop_archives: bool,
}

/// What fetch decided to do with an asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "action", content = "detail")]
pub enum Plan {
    /// Install from scratch (or replace with `--force`); bytes to download.
    Install { download: u64, replace: bool },
    /// Installed; these pinned files are missing and will be downloaded.
    AddFiles { files: Vec<String>, download: u64 },
    /// Installed and nothing to add.
    Present,
    /// Has to be downloaded by hand.
    Manual { page: String, instructions: String },
    /// Installed by another tool.
    External { page: String, instructions: String },
    /// Not for this platform.
    NotApplicable,
    /// Above the size limit.
    TooLarge { download: u64, limit: u64 },
}

/// Result for one asset.
#[derive(Clone, Debug, Serialize)]
pub struct FetchOutcome {
    pub id: String,
    pub plan: Plan,
    /// `installed`, `replaced`, `updated`, `present`, `skipped`, `planned`, `failed`, `refused`, `checked`,
    /// `cancelled`.
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub downloaded: u64,
    pub seconds: f64,
}

fn plan_for(ws: &Workspace, a: &Asset, opts: &FetchOptions) -> Result<Plan> {
    if !a.applies_here() {
        return Ok(Plan::NotApplicable);
    }
    if let Some(Source::External(e)) = a.sources.iter().find(|s| matches!(s, Source::External(_))) {
        return Ok(Plan::External { page: e.page.clone(), instructions: e.instructions.clone() });
    }
    if let Some(Source::Manual(m)) = a.sources.iter().find(|s| matches!(s, Source::Manual(_))) {
        if ws.is_installed(a) {
            return Ok(Plan::Present);
        }
        return Ok(Plan::Manual { page: m.page.clone(), instructions: m.instructions.clone() });
    }
    if a.sources.iter().any(|s| matches!(s, Source::Vendored(_))) {
        return Ok(Plan::Present);
    }
    let installed = ws.is_installed(a);
    for s in &a.sources {
        if let Source::Archive(x) = s
            && x.by_hand
            && (!installed || opts.force)
            && !fs::metadata(ws.archive_cache(a, x)).is_ok_and(|m| m.len() == x.size)
        {
            let page = x.page.clone().unwrap_or_else(|| x.url.clone());
            let instructions = format!(
                "Download the archive ({}) from this page in a browser; `flux assets import {}` (or Flux's Library window) then finds it in Downloads and installs it.",
                fsutil::fmt_bytes(x.size),
                a.id
            );
            return Ok(Plan::Manual { page, instructions });
        }
    }
    if !installed || opts.force {
        let mut download = 0;
        for s in &a.sources {
            download += match s {
                Source::Archive(x) => {
                    let cache = ws.archive_cache(a, x);
                    if fs::metadata(&cache).is_ok_and(|m| m.len() == x.size) { 0 } else { x.size }
                }
                other => other.download_size().unwrap_or(0),
            };
        }
        let limit = opts.max_size.unwrap_or(ws.loaded.manifest.policy.approval_threshold.0);
        if download > limit {
            return Ok(Plan::TooLarge { download, limit });
        }
        return Ok(Plan::Install { download, replace: installed });
    }
    let dir = ws.asset_dir(a).context("asset has no path")?;
    let mut files = Vec::new();
    let mut download = 0;
    for s in &a.sources {
        if let Source::Files(f) = s {
            for x in &f.files {
                let p = dir.join(join(&f.dest, &x.path));
                if fs::metadata(&p).map(|m| m.len()).ok() != Some(x.size) {
                    files.push(join(&f.dest, &x.path));
                    download += x.size;
                }
            }
        }
    }
    Ok(if files.is_empty() { Plan::Present } else { Plan::AddFiles { files, download } })
}

/// Fetch assets. `ids` empty requires `opts.all`.
pub fn fetch(ws: &mut Workspace, ids: &[String], opts: &FetchOptions, ui: &mut dyn Ui) -> Result<Vec<FetchOutcome>> {
    if ids.is_empty() && !opts.all {
        bail!("name the assets to fetch, or pass --all");
    }
    // `--all`: every required asset, plus optional ones that are already installed (to keep them complete).
    let selected: Vec<Asset> = if ids.is_empty() {
        ws.loaded.assets().iter().filter(|a| !a.optional || ws.is_installed(a)).cloned().collect()
    } else {
        ws.loaded.select(ids)?.into_iter().cloned().collect()
    };
    let mut plans = Vec::new();
    for a in &selected {
        plans.push((a, plan_for(ws, a, opts)?));
    }
    let total: u64 = plans
        .iter()
        .map(|(_, p)| match p {
            Plan::Install { download, .. } | Plan::AddFiles { download, .. } => *download,
            _ => 0,
        })
        .sum();
    for (a, p) in &plans {
        ui.line(&describe_plan(a, p));
    }
    if ids.is_empty() {
        let optional: Vec<&str> =
            ws.loaded.assets().iter().filter(|a| a.optional && !ws.is_installed(a)).map(|a| a.id.as_str()).collect();
        if !optional.is_empty() {
            ui.line(&format!("Optional and not installed, so not part of --all (name them to fetch): {}", optional.join(", ")));
        }
    }
    if opts.dry_run {
        let mut out = Vec::new();
        for (a, p) in plans {
            let (result, message) = if opts.check_urls { check_sources(ws, a, ui) } else { ("planned".to_string(), None) };
            out.push(FetchOutcome { id: a.id.clone(), plan: p, result, message, downloaded: 0, seconds: 0.0 });
        }
        return Ok(out);
    }
    let work: usize = plans.iter().filter(|(_, p)| matches!(p, Plan::Install { .. } | Plan::AddFiles { .. })).count();
    if work > 0 && !opts.yes {
        let q = format!("Download {} for {work} asset(s) from their official sources?", fsutil::fmt_bytes(total));
        if !ui.confirm(&q) {
            bail!("cancelled; nothing was downloaded");
        }
    }
    let _busy = if work > 0 { Some(BusyGuard::acquire(&ws.lock_path)?) } else { None };
    let mut out = Vec::new();
    for (a, p) in plans {
        let t0 = Instant::now();
        let failed = |e: anyhow::Error| {
            if crate::cancel::is_cancelled(&e) { ("cancelled".to_string(), None, 0) } else { ("failed".to_string(), Some(format!("{e:#}")), 0) }
        };
        let (result, message, downloaded) = if ws.cancel.is_cancelled() && matches!(p, Plan::Install { .. } | Plan::AddFiles { .. }) {
            ("cancelled".to_string(), None, 0)
        } else {
            match &p {
            Plan::Install { replace, .. } => match install(ws, a, *replace, ui) {
                Ok(n) => {
                    if opts.drop_archives {
                        drop_archives(ws, a, ui);
                    }
                    ((if *replace { "replaced" } else { "installed" }).to_string(), None, n)
                }
                Err(e) => failed(e),
            },
            Plan::AddFiles { files, .. } => match add_files(ws, a, files, ui) {
                Ok(n) => ("updated".to_string(), Some(format!("{} file(s) added", files.len())), n),
                Err(e) => failed(e),
            },
            Plan::Present => ("present".to_string(), None, 0),
            Plan::TooLarge { download, limit } => (
                "refused".to_string(),
                Some(format!(
                    "{} is above the limit of {}; the owner must approve it: pass --max-size {}",
                    fsutil::fmt_bytes(*download),
                    fsutil::fmt_bytes(*limit),
                    suggest_limit(*download)
                )),
                0,
            ),
            Plan::Manual { .. } | Plan::External { .. } | Plan::NotApplicable => ("skipped".to_string(), None, 0),
            }
        };
        if let Some(m) = &message {
            ui.line(&format!("  {}: {result}: {m}", a.id));
        } else {
            ui.line(&format!("  {}: {result}", a.id));
        }
        out.push(FetchOutcome { id: a.id.clone(), plan: p, result, message, downloaded, seconds: t0.elapsed().as_secs_f64() });
    }
    Ok(out)
}

fn suggest_limit(bytes: u64) -> String {
    let gb = (bytes as f64 / 1e9).ceil().max(1.0);
    format!("{gb:.0}G")
}

/// One line describing a plan.
pub fn describe_plan(a: &Asset, p: &Plan) -> String {
    match p {
        Plan::Install { download, replace } => format!(
            "{:<34} {} ({} to download)",
            a.id,
            if *replace { "replace" } else { "install" },
            fsutil::fmt_bytes(*download)
        ),
        Plan::AddFiles { files, download } => {
            format!("{:<34} add {} missing file(s) ({})", a.id, files.len(), fsutil::fmt_bytes(*download))
        }
        Plan::Present => format!("{:<34} present", a.id),
        Plan::Manual { page, instructions } => format!("{:<34} manual download: {page}\n{:<34} {instructions}", a.id, ""),
        Plan::External { page, .. } => format!("{:<34} installed by another tool ({page})", a.id),
        Plan::NotApplicable => format!("{:<34} not for this platform", a.id),
        Plan::TooLarge { download, limit } => format!(
            "{:<34} REFUSED: {} is over the {} limit (needs --max-size)",
            a.id,
            fsutil::fmt_bytes(*download),
            fsutil::fmt_bytes(*limit)
        ),
    }
}

/// `--dry-run --check-urls`: contact each source without downloading it.
fn check_sources(ws: &Workspace, a: &Asset, ui: &mut dyn Ui) -> (String, Option<String>) {
    let http = match HttpClient::new(ws.http.clone()) {
        Ok(h) => h,
        Err(e) => return ("failed".into(), Some(format!("{e:#}"))),
    };
    let mut notes = Vec::new();
    let mut ok = true;
    for s in &a.sources {
        match s {
            Source::Archive(x) => match http.probe(&x.url) {
                Ok(p) if p.total_size == Some(x.size) => notes.push(format!("archive reachable, {} bytes, ranges {}", x.size, yes_no(p.accepts_ranges))),
                Ok(p) => {
                    let cache = ws.archive_cache(a, x);
                    let cached = fs::metadata(&cache).is_ok_and(|m| m.len() == x.size);
                    let html = p.content_type.as_deref().is_some_and(|t| t.to_ascii_lowercase().contains("text/html"));
                    let what = if html {
                        format!("the server sent a web page ({}) instead of the archive", size_text(p.total_size))
                    } else {
                        format!("the server reports {}, the manifest pins {} bytes", size_text(p.total_size), x.size)
                    };
                    if cached {
                        notes.push(format!("{what}; the cached copy {} has the pinned size, so fetch installs from it", ws.rel(&cache)));
                    } else {
                        ok = false;
                        let page = x.page.as_deref().unwrap_or(&x.url);
                        notes.push(format!("{what}; download it by hand from {page} to {}", ws.rel(&cache)));
                    }
                }
                Err(e) => {
                    ok = false;
                    notes.push(format!("archive: {e:#}"));
                }
            },
            Source::Files(f) => {
                let mut bad = Vec::new();
                for x in &f.files {
                    match http.probe(&f.url_of(x)) {
                        Ok(p) if p.total_size == Some(x.size) => {}
                        Ok(p) => bad.push(format!("{} ({}, pinned {} bytes)", x.path, size_text(p.total_size), x.size)),
                        Err(e) => bad.push(format!("{}: {e:#}", x.path)),
                    }
                }
                if bad.is_empty() {
                    notes.push(format!("{} file(s) reachable with the expected sizes", f.files.len()));
                } else {
                    ok = false;
                    notes.push(format!("files with problems: {}", bad.join("; ")));
                }
            }
            Source::Git(g) => match ws.git().and_then(|git| git.ls_remote(&g.url, Some(g.branch.as_deref().unwrap_or("HEAD")))) {
                Ok(refs) => {
                    let tip = refs.first().map(|(h, _)| h.as_str());
                    if tip == Some(g.rev.as_str()) {
                        notes.push("git repository reachable; the pinned commit is the branch tip".into());
                    } else {
                        notes.push(format!(
                            "git repository reachable; branch tip is {} (pinned {} is fetched by id)",
                            tip.map_or("unknown", verify::short),
                            verify::short(&g.rev)
                        ));
                    }
                }
                Err(e) => {
                    ok = false;
                    notes.push(format!("git: {e:#}"));
                }
            },
            Source::Manual(m) => notes.push(format!("manual download, not probed ({})", m.page)),
            Source::Vendored(_) => notes.push("kept in this repository, nothing to download".into()),
            Source::External(e) => notes.push(format!("installed by another tool, not probed ({})", e.page)),
        }
    }
    let msg = notes.join("; ");
    ui.line(&format!("  {}: {}", a.id, msg));
    ((if ok { "checked" } else { "failed" }).to_string(), Some(msg))
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn size_text(size: Option<u64>) -> String {
    size.map_or_else(|| "no size".to_string(), |n| format!("{n} bytes"))
}

/// Install `a` into a staging folder, verify it, move it into place and lock it. Returns bytes downloaded.
fn install(ws: &mut Workspace, a: &Asset, replace: bool, ui: &mut dyn Ui) -> Result<u64> {
    let final_dir = ws.asset_dir(a).context("asset has no path")?;
    let parent = fsutil::parent_dir(&final_dir).to_path_buf();
    fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".flux-staging-{}-", a.id))
        .tempdir_in(&parent)
        .with_context(|| format!("creating a staging folder in {}", parent.display()))?;
    let http = HttpClient::new(ws.http.clone())?;
    let mut downloaded = 0u64;
    let mut sources = Vec::new();
    let mut git_used = None;
    for (i, s) in a.sources.iter().enumerate() {
        match s {
            Source::Git(g) => {
                let git = ws.git()?;
                ui.line(&format!("  {}: cloning {} at {}", a.id, g.url, verify::short(&g.rev)));
                let label = format!("{} download", a.id);
                let total = g.download_size.map(|s| s.0);
                let head = git.clone_pinned_with(g, staging.path(), &mut |n| ui.progress(&label, n, total))?;
                ui.progress_end();
                downloaded += g.download_size.map_or(0, |s| s.0);
                sources.push(LockedSource { kind: "git".into(), url: Some(g.url.clone()), rev: Some(head), verified: String::new(), ..Default::default() });
                git_used = Some((git, sources.len() - 1));
            }
            Source::Archive(x) => {
                let cache = ws.archive_cache(a, x);
                downloaded += ensure_archive(&http, a, x, &cache, ui)?;
                let dest = if x.dest.is_empty() { staging.path().to_path_buf() } else { staging.path().join(&x.dest) };
                ui.line(&format!("  {}: extracting {}", a.id, ws.rel(&cache)));
                let r = extract::extract(&cache, x.format, &dest, &extract_options(x))?;
                sources.push(LockedSource {
                    kind: "archive".into(),
                    url: Some(x.url.clone()),
                    sha256: Some(x.sha256.clone()),
                    size: Some(x.size),
                    files: Some(r.files as u64),
                    verified: archive_note(&r),
                    ..Default::default()
                });
            }
            Source::Files(f) => {
                for x in &f.files {
                    let to = staging.path().join(join(&f.dest, &x.path));
                    downloaded += download_file(&http, f, x, &to, &a.id, ui)?;
                }
                sources.push(files_locked(f));
            }
            other => bail!("source {} ({}) cannot be fetched automatically", i + 1, other.type_name()),
        }
        ws.cancel.check()?;
    }
    let tree = hash_with_progress(staging.path(), ws.threads, &format!("hashing {}", a.id), ui)?;
    ws.cancel.check()?;
    if let Some((git, idx)) = &git_used {
        let cmp = compare_git(git, staging.path(), &tree)?;
        if !(cmp.missing.is_empty() && cmp.modified.is_empty() && cmp.eol_only.is_empty() && cmp.special.is_empty()) {
            bail!(
                "fresh checkout does not match the repository (missing {}, modified {}, line endings {}, links {})",
                cmp.missing.len(),
                cmp.modified.len(),
                cmp.eol_only.len(),
                cmp.special.len()
            );
        }
        sources[*idx].files = Some(cmp.checked_out as u64);
        sources[*idx].verified = format!("{} checked-out files match their git blob ids", cmp.ok);
    }
    check_pinned_files(a, &tree.index)?;
    // Move into place.
    let staged = staging.keep();
    if replace {
        let backup = parent.join(format!(".flux-old-{}-{}", a.id, std::process::id()));
        fsutil::rename_with_retry(&final_dir, &backup)?;
        if let Err(e) = fsutil::rename_with_retry(&staged, &final_dir) {
            let _ = fsutil::rename_with_retry(&backup, &final_dir);
            let _ = fs::remove_dir_all(&staged);
            return Err(e);
        }
        if let Err(e) = remove_tree(&backup) {
            ui.line(&format!("  {}: could not remove the old copy {}: {e:#}", a.id, backup.display()));
        }
    } else if let Err(e) = fsutil::rename_with_retry(&staged, &final_dir) {
        let _ = fs::remove_dir_all(&staged);
        return Err(e);
    }
    record_lock(ws, a, &tree.index, LockMethod::Fetch, sources)?;
    Ok(downloaded)
}

/// Delete the cached archives of an installed asset.
fn drop_archives(ws: &Workspace, a: &Asset, ui: &mut dyn Ui) {
    for s in &a.sources {
        if let Source::Archive(x) = s {
            let cache = ws.archive_cache(a, x);
            if cache.is_file()
                && let Err(e) = fs::remove_file(&cache)
            {
                ui.line(&format!("  {}: could not delete the download {}: {e}", a.id, cache.display()));
            }
        }
    }
}

/// Remove a folder tree, clearing read-only flags (git marks pack files read-only on Windows).
pub fn remove_tree(p: &Path) -> Result<()> {
    if fs::remove_dir_all(p).is_ok() {
        return Ok(());
    }
    for e in fsutil::walk(p, &[])? {
        if let Ok(md) = fs::symlink_metadata(&e.path) {
            let mut perm = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = fs::set_permissions(&e.path, perm);
        }
    }
    fs::remove_dir_all(p).with_context(|| format!("removing {}", p.display()))
}

fn archive_note(r: &extract::ExtractReport) -> String {
    let mut s = format!("archive sha256 checked; {} files extracted and checked against the archive", r.files);
    if r.filtered_out > 0 {
        s.push_str(&format!(", {} left out by include", r.filtered_out));
    }
    if r.unreadable_skipped > 0 {
        s.push_str(&format!(", {} entries with unreadable names skipped", r.unreadable_skipped));
    }
    s
}

fn files_locked(f: &FilesSource) -> LockedSource {
    LockedSource {
        kind: "files".into(),
        url: Some(f.base_url.clone()),
        files: Some(f.files.len() as u64),
        verified: format!("{} files match their pinned sha256", f.files.len()),
        ..Default::default()
    }
}

fn extract_options(x: &ArchiveSource) -> ExtractOptions {
    // Bomb protection relative to the archive size; sample libraries barely compress, so 20x is generous.
    let cap = x.size.saturating_mul(20).clamp(256 << 20, 64_000_000_000);
    ExtractOptions {
        strip: x.strip,
        include: x.include.clone(),
        limits: Limits { max_total_bytes: cap, max_entries: 500_000, max_entry_bytes: cap },
        skipped_entries: x.skipped_entries,
    }
}

/// Make sure the cached archive exists with the pinned checksum; download it otherwise. Returns bytes downloaded.
fn ensure_archive(http: &HttpClient, a: &Asset, x: &ArchiveSource, cache: &Path, ui: &mut dyn Ui) -> Result<u64> {
    if cache.is_file() {
        let (sha, _) = hash::sha256_file(cache)?;
        if sha.eq_ignore_ascii_case(&x.sha256) {
            ui.line(&format!("  {}: using cached {}", a.id, cache.display()));
            return Ok(0);
        }
        ui.line(&format!("  {}: cached {} has a different checksum; downloading again", a.id, cache.display()));
    }
    ui.line(&format!("  {}: downloading {}", a.id, x.url));
    let label = format!("{} archive", a.id);
    let mut prog = |d: u64, t: Option<u64>| ui.progress(&label, d, t);
    let r = http.download(&x.url, cache, &Expect::pinned(&x.sha256, x.size), &mut prog)?;
    ui.progress_end();
    Ok(r.bytes - r.resumed_from)
}

fn download_file(http: &HttpClient, f: &FilesSource, x: &FileSpec, to: &Path, id: &str, ui: &mut dyn Ui) -> Result<u64> {
    let url = f.url_of(x);
    let label = format!("{id}: {}", x.path);
    let mut prog = |d: u64, t: Option<u64>| ui.progress(&label, d, t);
    let r = http.download(&url, to, &Expect::pinned(&x.sha256, x.size), &mut prog)?;
    ui.progress_end();
    Ok(r.bytes - r.resumed_from)
}

/// Every pinned file of files/manual/vendored sources must be in `index` with its checksum.
pub(crate) fn check_pinned_files(a: &Asset, index: &TreeIndex) -> Result<()> {
    let mut bad = Vec::new();
    let mut check = |path: String, sha: &str| match index.get(&path) {
        Some(e) if e.sha256.eq_ignore_ascii_case(sha) => {}
        Some(_) => bad.push(format!("{path} (checksum)")),
        None => bad.push(format!("{path} (missing)")),
    };
    for s in &a.sources {
        match s {
            Source::Files(f) => f.files.iter().for_each(|x| check(join(&f.dest, &x.path), &x.sha256)),
            Source::Manual(m) => m.files.iter().for_each(|x: &PinnedFile| check(x.path.clone(), &x.sha256)),
            Source::Vendored(v) => v.files.iter().for_each(|x| check(x.path.clone(), &x.sha256)),
            _ => {}
        }
    }
    if !bad.is_empty() {
        bail!("pinned files do not match: {}", bad.join(", "));
    }
    Ok(())
}

pub(crate) fn record_lock(ws: &mut Workspace, a: &Asset, index: &TreeIndex, method: LockMethod, sources: Vec<LockedSource>) -> Result<()> {
    write_index(&ws.lock_path, &a.id, index)?;
    ws.lock.put(LockedAsset {
        id: a.id.clone(),
        path: a.path.clone().unwrap_or_default(),
        method,
        locked_at: fsutil::now_rfc3339(),
        spec_sha256: spec_digest(a),
        files: index.len() as u64,
        bytes: index.total_bytes(),
        tree_sha256: index.digest(),
        sources,
    });
    ws.save_lock()
}

fn add_files(ws: &mut Workspace, a: &Asset, files: &[String], ui: &mut dyn Ui) -> Result<u64> {
    let dir = ws.asset_dir(a).context("asset has no path")?;
    let http = HttpClient::new(ws.http.clone())?;
    let wanted: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let mut n = 0;
    for s in &a.sources {
        if let Source::Files(f) = s {
            for x in &f.files {
                let rel = join(&f.dest, &x.path);
                if wanted.contains(rel.as_str()) {
                    n += download_file(&http, f, x, &dir.join(&rel), &a.id, ui)?;
                }
            }
        }
    }
    if ws.lock.get(&a.id).is_some() {
        relock(ws, a, ui)?;
    }
    Ok(n)
}

/// Re-hash an install whose lock entry is out of date because we changed it, keeping the source records.
fn relock(ws: &mut Workspace, a: &Asset, ui: &mut dyn Ui) -> Result<()> {
    let dir = ws.asset_dir(a).context("asset has no path")?;
    let tree = hash_with_progress(&dir, ws.threads, &format!("hashing {}", a.id), ui)?;
    let (method, sources) = ws.lock.get(&a.id).map(|l| (l.method, l.sources.clone())).unwrap_or((LockMethod::Adopt, vec![]));
    record_lock(ws, a, &tree.index, method, sources)
}

// ------------------------------------------------------------------------------------------------------------
// lock (adopt existing installs)

/// Result of locking one asset.
#[derive(Clone, Debug, Serialize)]
pub struct LockOutcome {
    pub id: String,
    /// `locked`, `refused`, `skipped`, `failed`.
    pub result: String,
    pub message: String,
    pub files: u64,
    pub bytes: u64,
    pub seconds: f64,
}

/// Check existing installs against their sources and record them in the lock. Installs with modified,
/// missing or unaccounted-for files are refused; files that differ from git only by CRLF line endings are
/// accepted and noted (run `repair` to fix them).
pub fn lock_existing(ws: &mut Workspace, ids: &[String], ui: &mut dyn Ui) -> Result<Vec<LockOutcome>> {
    let selected: Vec<Asset> = ws.loaded.select(ids)?.into_iter().cloned().collect();
    let _busy = BusyGuard::acquire(&ws.lock_path)?;
    let mut out = Vec::new();
    for a in &selected {
        let t0 = Instant::now();
        let (result, message, files, bytes) = if a.path.is_none() {
            ("skipped".into(), "installed outside the project".into(), 0, 0)
        } else if !a.applies_here() {
            ("skipped".into(), "not for this platform".into(), 0, 0)
        } else if !ws.is_installed(a) {
            ("skipped".into(), "not installed".into(), 0, 0)
        } else {
            match adopt(ws, a, ui) {
                Ok((idx, note)) => ("locked".into(), note, idx.len() as u64, idx.total_bytes()),
                Err(e) => {
                    let msg = format!("{e:#}");
                    let r = if msg.starts_with("refused:") { "refused" } else { "failed" };
                    (r.into(), msg.trim_start_matches("refused: ").to_string(), 0, 0)
                }
            }
        };
        ui.line(&format!("{:<34} {result}: {message}", a.id));
        out.push(LockOutcome { id: a.id.clone(), result, message, files, bytes, seconds: t0.elapsed().as_secs_f64() });
    }
    Ok(out)
}

fn adopt(ws: &mut Workspace, a: &Asset, ui: &mut dyn Ui) -> Result<(TreeIndex, String)> {
    let dir = ws.asset_dir(a).context("asset has no path")?;
    let tree = hash_with_progress(&dir, ws.threads, &format!("hashing {}", a.id), ui)?;
    let mut covered: BTreeSet<String> = BTreeSet::new();
    let mut sources = Vec::new();
    let mut notes = Vec::new();
    for s in &a.sources {
        match s {
            Source::Git(g) => {
                let git = ws.git()?;
                let head = git.head(&dir)?;
                for issue in verify::git_meta_issues(&git, &dir, g)? {
                    let detail = issue.detail.unwrap_or_default();
                    match issue.kind {
                        IssueKind::NotByteExact => {
                            notes.push("git may rewrite line endings in this checkout; `flux assets repair` sets byte-exact mode".into())
                        }
                        _ => bail!("refused: {detail}"),
                    }
                }
                let cmp = compare_git(&git, &dir, &tree)?;
                if !cmp.missing.is_empty() || !cmp.modified.is_empty() || !cmp.special.is_empty() {
                    bail!(
                        "refused: {} missing, {} modified, {} links/submodules (e.g. {:?}); run `flux assets repair {}`",
                        cmp.missing.len(),
                        cmp.modified.len(),
                        cmp.special.len(),
                        cmp.missing.iter().chain(&cmp.modified).chain(&cmp.special).take(3).collect::<Vec<_>>(),
                        a.id
                    );
                }
                covered.extend(cmp.tracked.iter().cloned());
                let mut v = format!("{} of {} checked-out files match their git blob ids", cmp.ok, cmp.checked_out);
                if !cmp.eol_only.is_empty() {
                    v.push_str(&format!("; {} differ only by CRLF line endings", cmp.eol_only.len()));
                    notes.push(format!("{} files differ from git only by CRLF line endings", cmp.eol_only.len()));
                }
                sources.push(LockedSource {
                    kind: "git".into(),
                    url: Some(g.url.clone()),
                    rev: Some(head),
                    files: Some(cmp.checked_out as u64),
                    verified: v,
                    ..Default::default()
                });
            }
            Source::Files(f) => {
                for x in &f.files {
                    covered.insert(join(&f.dest, &x.path));
                }
                sources.push(files_locked(f));
            }
            Source::Manual(m) => {
                covered.extend(m.files.iter().map(|x| x.path.clone()));
                sources.push(LockedSource { kind: "manual".into(), url: Some(m.page.clone()), files: Some(m.files.len() as u64), verified: format!("{} files match their pinned sha256", m.files.len()), ..Default::default() });
            }
            Source::Vendored(v) => {
                covered.extend(v.files.iter().map(|x| x.path.clone()));
                sources.push(LockedSource { kind: "vendored".into(), files: Some(v.files.len() as u64), verified: format!("{} files match their pinned sha256", v.files.len()), ..Default::default() });
            }
            Source::Archive(x) => {
                let cache = ws.archive_cache(a, x);
                if !cache.is_file() {
                    bail!("refused: the archive is needed to check the install but {} does not exist", ws.rel(&cache));
                }
                let (sha, _) = hash::sha256_file(&cache)?;
                if !sha.eq_ignore_ascii_case(&x.sha256) {
                    bail!("refused: {} has sha256 {sha}, the manifest pins {}", ws.rel(&cache), x.sha256);
                }
                ui.line(&format!("  {}: reading {}", a.id, ws.rel(&cache)));
                let (expected, report) = extract::index_archive(&cache, x.format, &extract_options(x), &ws.work_dir())?;
                let prefix = if x.dest.is_empty() { String::new() } else { format!("{}/", x.dest) };
                let mut problems = Vec::new();
                for e in expected.entries() {
                    let p = format!("{prefix}{}", e.path);
                    match tree.index.get(&p) {
                        Some(have) if have.sha256 == e.sha256 => {}
                        Some(_) => problems.push(format!("{p} (modified)")),
                        None => problems.push(format!("{p} (missing)")),
                    }
                    covered.insert(p);
                }
                if !problems.is_empty() {
                    bail!("refused: install differs from the archive: {} problem(s), e.g. {:?}", problems.len(), &problems[..problems.len().min(3)]);
                }
                sources.push(LockedSource {
                    kind: "archive".into(),
                    url: Some(x.url.clone()),
                    sha256: Some(x.sha256.clone()),
                    size: Some(x.size),
                    files: Some(expected.len() as u64),
                    verified: archive_note(&report),
                    ..Default::default()
                });
            }
            Source::External(_) => {}
        }
    }
    check_pinned_files(a, &tree.index).map_err(|e| anyhow::anyhow!("refused: {e:#}"))?;
    let extra: Vec<&str> = tree
        .index
        .entries()
        .iter()
        .map(|e| e.path.as_str())
        .filter(|p| !covered.contains(*p) && !a.ignore.iter().any(|g| crate::glob::matches(g, p)))
        .collect();
    if !extra.is_empty() {
        bail!(
            "refused: {} file(s) are not accounted for by any source (e.g. {:?}); add them to the asset's `ignore` or remove them",
            extra.len(),
            &extra[..extra.len().min(3)]
        );
    }
    record_lock(ws, a, &tree.index, LockMethod::Adopt, sources)?;
    let mut msg = format!("{} files, {}, tree {}", tree.index.len(), fsutil::fmt_bytes(tree.index.total_bytes()), verify::short(&tree.index.digest()));
    for n in notes {
        msg.push_str(&format!("; {n}"));
    }
    Ok((tree.index, msg))
}

// ------------------------------------------------------------------------------------------------------------
// repair

/// Result of [`repair`].
#[derive(Clone, Debug, Serialize)]
pub struct RepairOutcome {
    pub id: String,
    /// Files rewritten from git or downloaded again.
    pub restored: Vec<String>,
    /// Whether sparse patterns were reset to the manifest's.
    pub sparse_reset: bool,
    /// Files no source accounts for, left in place.
    pub extra_left: Vec<String>,
    /// State after the repair.
    pub state_after: verify::State,
}

/// Bring an install back to what the manifest pins: byte-exact git settings, correct sparse patterns, files
/// restored from git (CRLF rewrites, edits, deletions) and pinned files downloaded again. Archive contents
/// cannot be repaired file by file; use `fetch --force`. Extra files are reported, never deleted.
pub fn repair(ws: &mut Workspace, id: &str, yes: bool, ui: &mut dyn Ui) -> Result<RepairOutcome> {
    let a = ws.loaded.select(&[id.to_string()])?[0].clone();
    let dir = ws.asset_dir(&a).context("this asset is installed outside the project; nothing to repair")?;
    if !ws.is_installed(&a) {
        bail!("{id} is not installed; use `flux assets fetch {id}`");
    }
    let before = verify::verify_asset(ws, &a, VerifyOptions::default(), ui);
    if before.state == verify::State::Error {
        bail!("cannot check {id}: {}", before.error.unwrap_or_default());
    }
    let git_src = a.git_source().cloned();
    let fixable: Vec<String> = before
        .issues
        .iter()
        .filter(|i| matches!(i.kind, IssueKind::Missing | IssueKind::Modified | IssueKind::EolOnly))
        .filter_map(|i| i.path.clone())
        .collect();
    let needs_git_settings = before.issues.iter().any(|i| i.kind == IssueKind::NotByteExact);
    let sparse_wrong = before.issues.iter().any(|i| i.kind == IssueKind::SparseMismatch);
    if before.issues.iter().any(|i| i.kind == IssueKind::WrongCommit) {
        bail!("{id} is checked out at another commit; reinstall it with `flux assets fetch --force {id}`");
    }
    if fixable.is_empty() && !needs_git_settings && !sparse_wrong {
        let extra_left: Vec<String> = before.issues.iter().filter(|i| i.kind == IssueKind::Extra).filter_map(|i| i.path.clone()).collect();
        return Ok(RepairOutcome { id: id.into(), restored: vec![], sparse_reset: false, extra_left, state_after: before.state });
    }
    if !yes && !ui.confirm(&format!("Rewrite {} file(s) in {} from the official source?", fixable.len(), ws.rel(&dir))) {
        bail!("cancelled; nothing was changed");
    }
    let _busy = BusyGuard::acquire(&ws.lock_path)?;
    let pinned: BTreeSet<String> = a
        .sources
        .iter()
        .filter_map(|s| if let Source::Files(f) = s { Some(f) } else { None })
        .flat_map(|f| f.files.iter().map(move |x| join(&f.dest, &x.path)))
        .collect();
    let mut restored = Vec::new();
    if let Some(g) = &git_src {
        let mut git = ws.git()?;
        git.show_progress = true;
        git.make_byte_exact(&dir)?;
        if sparse_wrong {
            git.set_sparse(&dir, &g.sparse, g.cone)?;
        }
        let from_git: Vec<String> = fixable.iter().filter(|p| !pinned.contains(*p)).cloned().collect();
        git.restore_paths(&dir, &from_git)?;
        restored.extend(from_git);
    } else if fixable.iter().any(|p| !pinned.contains(p)) {
        bail!("{id} has drift in archive contents; reinstall it with `flux assets fetch --force {id}`");
    }
    let http = HttpClient::new(ws.http.clone())?;
    for s in &a.sources {
        if let Source::Files(f) = s {
            for x in &f.files {
                let rel = join(&f.dest, &x.path);
                if fixable.contains(&rel) {
                    download_file(&http, f, x, &dir.join(&rel), id, ui)?;
                    restored.push(rel);
                }
            }
        }
    }
    let after = verify::verify_asset(ws, &a, VerifyOptions::default(), ui);
    let extra_left: Vec<String> = after.issues.iter().filter(|i| i.kind == IssueKind::Extra).filter_map(|i| i.path.clone()).collect();
    // Extra files are left alone, so they do not stop the lock entry being brought up to date.
    let only_extras = after.issues.iter().all(|i| i.kind == IssueKind::Extra || !i.kind.is_drift());
    if only_extras && after.state != verify::State::Error && ws.lock.get(&a.id).is_some() {
        relock(ws, &a, ui)?;
    }
    Ok(RepairOutcome { id: id.into(), restored, sparse_reset: sparse_wrong, extra_left, state_after: after.state })
}

fn join(dest: &str, path: &str) -> String {
    if dest.is_empty() { path.to_string() } else { format!("{dest}/{path}") }
}

/// Paths below `root` that start with `.flux-staging-` or `.flux-old-` (left over by a crash).
pub fn leftovers(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with(".flux-staging-") || n.starts_with(".flux-old-") || n.starts_with(".flux-raw-")
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}
