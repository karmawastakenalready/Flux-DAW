//! The instrument library's own files: the manifest (`assets.toml`), Flux's SFZ mappings and rigs, the instrument
//! cards and the license texts. Released builds of Flux carry them as a zip; Setup and every update write them into
//! the library. A file the user has changed since is left alone (and reported), a file a newer Flux no longer has
//! is removed when unchanged: `.flux-seed.json` remembers what each one held when Flux wrote it.

use anyhow::{Context, Result, bail};
use flux_assets::{fsutil, hash};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// What the last sync wrote, in the library.
pub const RECORD: &str = ".flux-seed.json";

#[derive(Debug, Default, Deserialize, Serialize)]
struct Record {
    /// The Flux version that wrote the files.
    version: String,
    /// Library-relative path (with `/`) to the SHA-256 of what Flux wrote there.
    files: BTreeMap<String, String>,
}

/// What [`sync`] did, by library-relative path.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SeedReport {
    /// New files.
    pub written: Vec<String>,
    /// Files replaced by a newer version.
    pub updated: Vec<String>,
    /// Files the user changed, left as they are (Flux's version was not written).
    pub kept: Vec<String>,
    /// Files this Flux no longer has, deleted.
    pub removed: Vec<String>,
}

/// Write the seed (a zip of library files) into `library`.
pub fn sync(seed: &[u8], version: &str, library: &Path) -> Result<SeedReport> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(seed)).context("reading the library files built into Flux")?;
    fs::create_dir_all(library).with_context(|| format!("creating {}", library.display()))?;
    let record_path = library.join(RECORD);
    let old: Record = match fs::read(&record_path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Record::default(),
    };
    let mut new = Record { version: version.to_string(), files: BTreeMap::new() };
    let mut report = SeedReport::default();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let rel = f.name().to_string();
        let target = safe_join(library, &rel)?;
        let mut bytes = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut bytes).with_context(|| format!("reading {rel} from the built-in library files"))?;
        let want = hash::sha256_bytes(&bytes);
        let current = match target.is_file() {
            true => Some(hash::sha256_file(&target)?.0),
            false => None,
        };
        match current {
            None => {
                fsutil::atomic_write(&target, &bytes)?;
                report.written.push(rel.clone());
            }
            Some(c) if c == want => {}
            Some(c) if old.files.get(&rel) == Some(&c) => {
                fsutil::atomic_write(&target, &bytes)?;
                report.updated.push(rel.clone());
            }
            Some(_) => {
                report.kept.push(rel.clone());
                // still the user's: compare against what Flux last wrote, so it stays theirs next time too
                if let Some(h) = old.files.get(&rel) {
                    new.files.insert(rel, h.clone());
                }
                continue;
            }
        }
        new.files.insert(rel, want);
    }
    for (rel, h) in &old.files {
        if new.files.contains_key(rel) {
            continue;
        }
        let target = safe_join(library, rel)?;
        if target.is_file() && hash::sha256_file(&target)?.0 == *h {
            fs::remove_file(&target).with_context(|| format!("removing {}", target.display()))?;
            report.removed.push(rel.clone());
        }
    }
    fsutil::atomic_write(&record_path, &serde_json::to_vec_pretty(&new)?)?;
    Ok(report)
}

/// One file of a seed, by library-relative path.
pub fn read(seed: &[u8], rel: &str) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(seed)).context("reading the library files built into Flux")?;
    let mut f = zip.by_name(rel).with_context(|| format!("{rel} isn't among the library files built into Flux"))?;
    let mut bytes = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut bytes).with_context(|| format!("reading {rel} from the built-in library files"))?;
    Ok(bytes)
}

/// The Flux version that last wrote the library's files.
pub fn version(library: &Path) -> Option<String> {
    let r: Record = serde_json::from_slice(&fs::read(library.join(RECORD)).ok()?).ok()?;
    Some(r.version)
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("refusing the library file path {rel:?}");
    }
    Ok(root.join(p))
}

/// A seed zip of `files` (library-relative path, contents), as the build writes it.
pub fn build(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        safe_join(Path::new("."), name)?;
        w.start_file(name.as_str(), opts)?;
        w.write_all(bytes)?;
    }
    Ok(w.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(files: &[(&str, &str)]) -> Vec<u8> {
        build(&files.iter().map(|(n, b)| (n.to_string(), b.as_bytes().to_vec())).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn updates_only_what_the_user_left_alone() {
        let t = tempfile::tempdir().unwrap();
        let lib = t.path();
        let r = sync(&seed(&[("assets.toml", "v1"), ("sfz/a.sfz", "a1"), ("sfz/b.sfz", "b1"), ("sfz/gone.sfz", "g")]), "0.1.0", lib).unwrap();
        assert_eq!(r.written.len(), 4);
        assert_eq!(version(lib).as_deref(), Some("0.1.0"));
        let s = seed(&[("assets.toml", "a"), ("sfz/x.sfz", "x")]);
        assert_eq!(read(&s, "sfz/x.sfz").unwrap(), b"x");
        assert!(read(&s, "sfz/y.sfz").is_err());

        // the user edits a.sfz and gone.sfz
        fs::write(lib.join("sfz/a.sfz"), "mine").unwrap();
        fs::write(lib.join("sfz/gone.sfz"), "mine too").unwrap();
        let r = sync(&seed(&[("assets.toml", "v2"), ("sfz/a.sfz", "a2"), ("sfz/b.sfz", "b1"), ("sfz/new.sfz", "n")]), "0.2.0", lib).unwrap();
        assert_eq!(r.updated, ["assets.toml"]);
        assert_eq!(r.kept, ["sfz/a.sfz"]);
        assert_eq!(r.written, ["sfz/new.sfz"]);
        assert!(r.removed.is_empty(), "a changed file is never deleted");
        assert_eq!(fs::read_to_string(lib.join("assets.toml")).unwrap(), "v2");
        assert_eq!(fs::read_to_string(lib.join("sfz/a.sfz")).unwrap(), "mine");
        assert_eq!(fs::read_to_string(lib.join("sfz/gone.sfz")).unwrap(), "mine too");

        // still the user's on the next update; an unchanged file that left the seed goes
        let r = sync(&seed(&[("assets.toml", "v3"), ("sfz/a.sfz", "a3")]), "0.3.0", lib).unwrap();
        assert_eq!(r.kept, ["sfz/a.sfz"]);
        assert_eq!(r.removed, ["sfz/b.sfz", "sfz/new.sfz"]);
        assert!(!lib.join("sfz/b.sfz").exists());

        // once the user's copy matches Flux's again it is Flux's to update
        fs::write(lib.join("sfz/a.sfz"), "a4").unwrap();
        let r = sync(&seed(&[("assets.toml", "v3"), ("sfz/a.sfz", "a4")]), "0.4.0", lib).unwrap();
        assert!(r.kept.is_empty() && r.updated.is_empty(), "{r:?}");
        let r = sync(&seed(&[("assets.toml", "v3"), ("sfz/a.sfz", "a5")]), "0.5.0", lib).unwrap();
        assert_eq!(r.updated, ["sfz/a.sfz"]);
    }

    #[test]
    fn refuses_paths_outside_the_library() {
        assert!(build(&[("../x".into(), vec![])]).is_err());
        assert!(safe_join(Path::new("."), "C:/x").is_err());
        assert!(safe_join(Path::new("."), "/x").is_err());
        assert!(safe_join(Path::new("."), "a/b.txt").is_ok());
    }
}
