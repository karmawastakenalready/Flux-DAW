//! Installing what was downloaded by hand. Some assets can only be fetched in a browser: behind a sign-in
//! (TONE3000 captures) or from a host that answers big downloads with a web page (Google Drive). The person
//! downloads them; [`search`] recognises the files by size and SHA-256 wherever they landed (loose in a folder
//! such as Downloads, or inside a zip they came in), and [`import`] installs them the way `fetch` would: an
//! archive goes into the download cache and is installed from there, and an asset's loose pinned files are
//! staged, verified, moved into place and locked. Nothing that fails its checksum is ever used.

use crate::cancel::Cancel;
use crate::fetch::{self, FetchOptions, FetchOutcome};
use crate::fsutil;
use crate::hash;
use crate::lock::{BusyGuard, LockMethod, LockedSource};
use crate::manifest::{Asset, Source};
use crate::packs::Delivery;
use crate::verify::hash_with_progress;
use crate::workspace::{Ui, Workspace};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// How deep [`search`] looks below a folder it is given (Downloads/Some Tone/capture.nam is depth 1).
const SEARCH_DEPTH: usize = 2;
/// Zips bigger than this are not opened when searching.
const MAX_ZIP: u64 = 8 << 30;

/// A file an asset still needs from a download made by hand.
#[derive(Clone, Debug, Serialize)]
pub struct Wanted {
    pub asset: String,
    /// The file's name in the manifest (an archive's cache name, or a pinned file's path).
    pub name: String,
    pub sha256: String,
    pub size: u64,
    /// The page to download it from.
    pub page: String,
    #[serde(skip)]
    pub target: Target,
}

/// Where a wanted file goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The download cache of an archive that is downloaded by hand.
    Archive(PathBuf),
    /// A pinned file of a manual source, relative to the asset folder.
    File(String),
}

/// Where a wanted file was found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Origin {
    File { path: PathBuf },
    Zip { zip: PathBuf, entry: String },
}

impl Origin {
    /// For messages.
    pub fn describe(&self) -> String {
        match self {
            Origin::File { path } => path.display().to_string(),
            Origin::Zip { zip, entry } => format!("{} (inside {})", entry, zip.display()),
        }
    }
}

/// A wanted file and where it is.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    pub wanted: Wanted,
    pub origin: Origin,
}

/// The files still to be downloaded by hand for `ids` (every asset when empty): assets for this platform that
/// are installed by hand and not installed yet. A by-hand archive already in the cache is not wanted.
pub fn wanted(ws: &Workspace, ids: &[String]) -> Result<Vec<Wanted>> {
    let mut out = Vec::new();
    for a in ws.loaded.select(ids)? {
        if !a.applies_here() || a.delivery() != Delivery::ByHand || ws.is_installed(a) {
            continue;
        }
        for s in &a.sources {
            match s {
                Source::Archive(x) if x.by_hand => {
                    let cache = ws.archive_cache(a, x);
                    if fs::metadata(&cache).is_ok_and(|m| m.len() == x.size) {
                        continue;
                    }
                    let name = cache.file_name().map_or_else(|| a.id.clone(), |n| n.to_string_lossy().into_owned());
                    let page = x.page.clone().unwrap_or_else(|| x.url.clone());
                    out.push(Wanted { asset: a.id.clone(), name, sha256: x.sha256.to_ascii_lowercase(), size: x.size, page, target: Target::Archive(cache) });
                }
                Source::Manual(m) => {
                    for f in &m.files {
                        out.push(Wanted {
                            asset: a.id.clone(),
                            name: f.path.clone(),
                            sha256: f.sha256.to_ascii_lowercase(),
                            size: f.size,
                            page: m.page.clone(),
                            target: Target::File(f.path.clone()),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Look for wanted files in `places`: files, and folders searched [`SEARCH_DEPTH`] levels deep, including the
/// zips found there. Only files (and zip entries) whose size matches something wanted are hashed.
pub fn search(wanted: &[Wanted], places: &[PathBuf], cancel: &Cancel, ui: &mut dyn Ui) -> Result<Vec<Found>> {
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, w) in wanted.iter().enumerate() {
        by_size.entry(w.size).or_default().push(i);
    }
    let mut found: Vec<Option<Origin>> = vec![None; wanted.len()];
    let mut files = Vec::new();
    for p in places {
        if p.is_file() {
            files.push(p.clone());
        } else if p.is_dir() {
            collect_files(p, SEARCH_DEPTH, &mut files);
        }
    }
    let open = |found: &[Option<Origin>], size: u64| by_size.get(&size).is_some_and(|ix| ix.iter().any(|&i| found[i].is_none()));
    for f in files {
        cancel.check()?;
        if found.iter().all(Option::is_some) {
            break;
        }
        let Ok(md) = fs::metadata(&f) else { continue };
        if open(&found, md.len()) {
            ui.line(&format!("  checking {}", f.display()));
            if let Ok((sha, _)) = hash::sha256_file(&f) {
                for &i in &by_size[&md.len()] {
                    if found[i].is_none() && wanted[i].sha256 == sha {
                        found[i] = Some(Origin::File { path: f.clone() });
                    }
                }
            }
        }
        let is_zip = f.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"));
        if is_zip && md.len() <= MAX_ZIP {
            // a zip that is not readable is simply not what we are looking for
            let _ = search_zip(&f, wanted, &by_size, &mut found, cancel);
        }
    }
    Ok(found.into_iter().zip(wanted).filter_map(|(o, w)| o.map(|origin| Found { wanted: w.clone(), origin })).collect())
}

fn collect_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        if t.is_file() {
            out.push(e.path());
        } else if t.is_dir() && depth > 0 {
            subdirs.push(e.path());
        }
    }
    for d in subdirs {
        collect_files(&d, depth - 1, out);
    }
}

fn search_zip(zip_path: &Path, wanted: &[Wanted], by_size: &HashMap<u64, Vec<usize>>, found: &mut [Option<Origin>], cancel: &Cancel) -> Result<()> {
    let mut zip = zip::ZipArchive::new(fs::File::open(zip_path)?)?;
    for n in 0..zip.len() {
        cancel.check()?;
        let entry = zip.by_index(n)?;
        if !entry.is_file() {
            continue;
        }
        let Some(ix) = by_size.get(&entry.size()) else { continue };
        if ix.iter().all(|&i| found[i].is_some()) {
            continue;
        }
        let name = entry.name().to_string();
        let (sha, _) = hash::sha256_reader(entry)?;
        for &i in ix {
            if found[i].is_none() && wanted[i].sha256 == sha {
                found[i] = Some(Origin::Zip { zip: zip_path.to_path_buf(), entry: name.clone() });
            }
        }
    }
    Ok(())
}

/// What [`import`] did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    /// Assets installed from loose files.
    pub installed: Vec<String>,
    /// Fetches run for assets whose archive was put in the cache.
    pub fetched: Vec<FetchOutcome>,
    /// Assets that still miss files, with the files' names.
    pub incomplete: Vec<(String, Vec<String>)>,
    /// Assets that could not be installed, with why.
    pub failed: Vec<(String, String)>,
}

/// Install assets from found files. Each asset is installed only when every file it wants was found; `opts`
/// are the fetch options for archives (installed from the cache by [`fetch::fetch`]).
pub fn import(ws: &mut Workspace, found: &[Found], opts: &FetchOptions, ui: &mut dyn Ui) -> Result<ImportReport> {
    let mut by_asset: BTreeMap<&str, Vec<&Found>> = BTreeMap::new();
    for f in found {
        by_asset.entry(f.wanted.asset.as_str()).or_default().push(f);
    }
    let mut report = ImportReport::default();
    for (id, files) in by_asset {
        ws.cancel.check()?;
        let a = ws.loaded.asset(id).with_context(|| format!("no asset '{id}'"))?.clone();
        let still: Vec<Wanted> = wanted(ws, std::slice::from_ref(&a.id))?;
        let missing: Vec<String> =
            still.iter().filter(|w| !files.iter().any(|f| f.wanted.target == w.target)).map(|w| w.name.clone()).collect();
        if !missing.is_empty() {
            report.incomplete.push((a.id.clone(), missing));
            continue;
        }
        let archives: Vec<&&Found> = files.iter().filter(|f| matches!(f.wanted.target, Target::Archive(_))).collect();
        let result = if archives.is_empty() {
            install_files(ws, &a, &files, ui).map(|()| report.installed.push(a.id.clone()))
        } else {
            (|| -> Result<()> {
                for f in &archives {
                    let Target::Archive(cache) = &f.wanted.target else { unreachable!() };
                    ui.line(&format!("  {}: using {}", a.id, f.origin.describe()));
                    place(&f.origin, cache, &f.wanted.sha256, ws.cancel.clone())?;
                }
                let mut o = opts.clone();
                o.yes = true;
                report.fetched.extend(fetch::fetch(ws, std::slice::from_ref(&a.id), &o, ui)?);
                Ok(())
            })()
        };
        if let Err(e) = result {
            if crate::cancel::is_cancelled(&e) {
                return Err(e);
            }
            ui.line(&format!("  {}: failed: {e:#}", a.id));
            report.failed.push((a.id.clone(), format!("{e:#}")));
        }
    }
    Ok(report)
}

/// Copy (or hard-link, on the same disk) a found file to `to`, checking its SHA-256 on the way.
fn place(origin: &Origin, to: &Path, sha256: &str, cancel: Cancel) -> Result<()> {
    if let Some(d) = to.parent() {
        fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let tmp = fsutil::parent_dir(to).join(format!(".{}.importing", to.file_name().map_or("file".into(), |n| n.to_string_lossy())));
    let _ = fs::remove_file(&tmp);
    let copied = (|| -> Result<()> {
        match origin {
            Origin::File { path } => {
                if fs::hard_link(path, &tmp).is_err() {
                    copy_checked(&mut fs::File::open(path).with_context(|| format!("reading {}", path.display()))?, &tmp, &cancel)?;
                }
            }
            Origin::Zip { zip, entry } => {
                let mut z = zip::ZipArchive::new(fs::File::open(zip)?).with_context(|| format!("reading {}", zip.display()))?;
                let mut e = z.by_name(entry).with_context(|| format!("{entry} in {}", zip.display()))?;
                copy_checked(&mut e, &tmp, &cancel)?;
            }
        }
        let (sha, _) = hash::sha256_file(&tmp)?;
        if sha != sha256 {
            bail!("{} changed since it was found (checksum differs)", origin.describe());
        }
        Ok(())
    })();
    if let Err(e) = copied {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fsutil::rename_with_retry(&tmp, to)
}

fn copy_checked(from: &mut dyn Read, to: &Path, cancel: &Cancel) -> Result<()> {
    let mut out = fs::File::create(to).with_context(|| format!("writing {}", to.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        cancel.check()?;
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    out.sync_all()?;
    Ok(())
}

/// Install an asset made only of hand-downloaded files: stage them, verify every pinned file, move the folder
/// into place and lock it.
fn install_files(ws: &mut Workspace, a: &Asset, files: &[&Found], ui: &mut dyn Ui) -> Result<()> {
    let final_dir = ws.asset_dir(a).context("asset has no path")?;
    let parent = fsutil::parent_dir(&final_dir).to_path_buf();
    fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    let _busy = BusyGuard::acquire(&ws.lock_path)?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".flux-staging-{}-", a.id))
        .tempdir_in(&parent)
        .with_context(|| format!("creating a staging folder in {}", parent.display()))?;
    for f in files {
        if let Target::File(rel) = &f.wanted.target {
            ui.line(&format!("  {}: {} from {}", a.id, rel, f.origin.describe()));
            place(&f.origin, &staging.path().join(rel), &f.wanted.sha256, ws.cancel.clone())?;
        }
    }
    let tree = hash_with_progress(staging.path(), ws.threads, &format!("hashing {}", a.id), ui)?;
    fetch::check_pinned_files(a, &tree.index)?;
    ws.cancel.check()?;
    // an empty folder left behind (by a removal, or made by hand) is not an install; replace it
    if final_dir.is_dir() && fs::read_dir(&final_dir).is_ok_and(|mut d| d.next().is_none()) {
        fs::remove_dir(&final_dir).with_context(|| format!("removing the empty {}", final_dir.display()))?;
    }
    let staged = staging.keep();
    if let Err(e) = fsutil::rename_with_retry(&staged, &final_dir) {
        let _ = fs::remove_dir_all(&staged);
        return Err(e);
    }
    let pages: Vec<String> = a.sources.iter().filter_map(|s| if let Source::Manual(m) = s { Some(m.page.clone()) } else { None }).collect();
    let n = files.len() as u64;
    let source = LockedSource {
        kind: "manual".into(),
        url: pages.first().cloned(),
        files: Some(n),
        verified: format!("{n} files downloaded by hand match their pinned sha256"),
        ..Default::default()
    };
    fetch::record_lock(ws, a, &tree.index, LockMethod::Import, vec![source])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse;
    use crate::workspace::RecordingUi;

    fn manifest(nam_a: &[u8], nam_b: &[u8], rar: &[u8]) -> String {
        format!(
            r#"
schema = 1
[policy]
allowed_hosts = ["drive.google.com"]

[[asset]]
id = "captures"
name = "Captures"
kind = "amp-capture"
scope = "audio"
path = "instruments/nam/captures"
version = "1"
license = {{ id = "CC0-1.0" }}
[[asset.source]]
type = "manual"
page = "https://example.com/tone"
instructions = "Sign in and download the captures."
[[asset.source.file]]
path = "a.nam"
sha256 = "{}"
size = {}
[[asset.source.file]]
path = "b.nam"
sha256 = "{}"
size = {}

[[asset]]
id = "gtx"
name = "Guitar"
kind = "sample-library"
scope = "audio"
path = "instruments/gtx"
version = "1"
license = {{ id = "CC0-1.0" }}
[[asset.source]]
type = "archive"
url = "https://drive.google.com/uc?id=x"
page = "https://example.com/gtx"
sha256 = "{}"
size = {}
format = "zip"
cache = "tools/_dl/gtx/gtx.zip"
by_hand = true
"#,
            hash::sha256_bytes(nam_a),
            nam_a.len(),
            hash::sha256_bytes(nam_b),
            nam_b.len(),
            hash::sha256_bytes(rar),
            rar.len()
        )
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn finds_hand_downloads_and_installs_them() {
        let (nam_a, nam_b) = (b"capture A weights".as_slice(), b"capture B, other weights".as_slice());
        let gtx = zip_of(&[("Guitar/notes.txt", b"six strings")]);
        let root = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let text = manifest(nam_a, nam_b, &gtx);
        let loaded = parse(&text).unwrap();
        let mut ws = Workspace::from_loaded(root.path(), loaded, root.path().join("assets.toml"), root.path().join("assets.lock")).unwrap();
        let mut ui = RecordingUi::default();

        let all = wanted(&ws, &[]).unwrap();
        assert_eq!(all.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["a.nam", "b.nam", "gtx.zip"]);
        // nothing downloaded yet; the fetch plan sends people to the page
        assert!(search(&all, &[downloads.path().to_path_buf()], &Cancel::new(), &mut ui).unwrap().is_empty());
        let plan = fetch::fetch(&mut ws, &["gtx".into()], &FetchOptions { dry_run: true, ..Default::default() }, &mut ui).unwrap();
        assert!(matches!(&plan[0].plan, fetch::Plan::Manual { page, .. } if page == "https://example.com/gtx"));

        // one capture arrives loose (renamed by the browser), the other inside a zip, and the guitar archive
        // with another name in a sub-folder; a decoy with the right size but other bytes is ignored
        fs::write(downloads.path().join("a (1).nam"), nam_a).unwrap();
        fs::write(downloads.path().join("decoy.nam"), b"capture B, OTHER weights").unwrap();
        fs::write(downloads.path().join("Tone pack.zip"), zip_of(&[("pack/b.nam", nam_b), ("pack/readme.txt", b"hi")])).unwrap();
        fs::create_dir_all(downloads.path().join("drive")).unwrap();
        fs::write(downloads.path().join("drive").join("UI_GTX.zip"), &gtx).unwrap();
        let found = search(&all, &[downloads.path().to_path_buf()], &Cancel::new(), &mut ui).unwrap();
        assert_eq!(found.len(), 3, "{found:?}");
        let b = found.iter().find(|f| f.wanted.name == "b.nam").unwrap();
        assert_eq!(b.origin, Origin::Zip { zip: downloads.path().join("Tone pack.zip"), entry: "pack/b.nam".into() });

        let r = import(&mut ws, &found, &FetchOptions::default(), &mut ui).unwrap();
        assert_eq!(r.installed, ["captures"], "{r:?} {:?}", ui.lines);
        assert!(r.failed.is_empty() && r.incomplete.is_empty(), "{r:?}");
        assert_eq!(r.fetched.len(), 1);
        assert_eq!(r.fetched[0].result, "installed", "{:?}", r.fetched[0]);
        let caps = root.path().join("instruments/nam/captures");
        assert_eq!(fs::read(caps.join("b.nam")).unwrap(), nam_b);
        assert_eq!(fs::read(root.path().join("instruments/gtx/Guitar/notes.txt")).unwrap(), b"six strings");
        assert_eq!(ws.lock.get("captures").unwrap().method, LockMethod::Import);
        assert!(wanted(&ws, &[]).unwrap().is_empty());
        // the downloads are left where they were
        assert!(downloads.path().join("a (1).nam").is_file());
    }

    #[test]
    fn half_a_download_installs_nothing() {
        let (nam_a, nam_b) = (b"capture A weights".as_slice(), b"capture B, other weights".as_slice());
        let root = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let loaded = parse(&manifest(nam_a, nam_b, b"zip")).unwrap();
        let mut ws = Workspace::from_loaded(root.path(), loaded, root.path().join("assets.toml"), root.path().join("assets.lock")).unwrap();
        let mut ui = RecordingUi::default();
        fs::write(downloads.path().join("a.nam"), nam_a).unwrap();
        let all = wanted(&ws, &["captures".into()]).unwrap();
        let found = search(&all, &[downloads.path().join("a.nam")], &Cancel::new(), &mut ui).unwrap();
        let r = import(&mut ws, &found, &FetchOptions::default(), &mut ui).unwrap();
        assert_eq!(r.incomplete, [("captures".to_string(), vec!["b.nam".to_string()])]);
        assert!(!root.path().join("instruments/nam/captures").exists());
    }
}
