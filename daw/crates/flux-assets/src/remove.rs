//! Removing installed assets (the Library window's Remove, `flux assets remove`). The folder is first renamed
//! aside and only then deleted, so an interrupted removal never leaves a half-deleted asset that looks
//! installed; its lock entry and tree index go with it.

use crate::fetch::remove_tree;
use crate::fsutil;
use crate::lock::{BusyGuard, index_path};
use crate::manifest::Source;
use crate::workspace::{Ui, Workspace};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;

/// Options for [`remove`].
#[derive(Clone, Debug, Default)]
pub struct RemoveOptions {
    /// Keep the asset's downloaded archives in the cache (a reinstall then needs no download).
    pub keep_downloads: bool,
}

/// What happened to one asset.
#[derive(Clone, Debug, Serialize)]
pub struct RemoveOutcome {
    pub id: String,
    /// `removed`, `not-installed`, `skipped` (installed by its maker's installer, or kept in the repository).
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Bytes deleted (the install and any downloads).
    pub freed: u64,
}

/// Remove assets by id.
pub fn remove(ws: &mut Workspace, ids: &[String], opts: &RemoveOptions, ui: &mut dyn Ui) -> Result<Vec<RemoveOutcome>> {
    let assets: Vec<_> = ws.loaded.select(ids)?.into_iter().cloned().collect();
    let _busy = BusyGuard::acquire(&ws.lock_path)?;
    let mut out = Vec::new();
    for a in &assets {
        let skip = |why: String| RemoveOutcome { id: a.id.clone(), result: "skipped".into(), message: Some(why), freed: 0 };
        if a.sources.iter().any(|s| matches!(s, Source::External(_))) {
            out.push(skip("installed by its maker's installer; remove it in Windows Settings > Apps".into()));
            continue;
        }
        if a.sources.iter().any(|s| matches!(s, Source::Vendored(_))) {
            out.push(skip("kept in the repository".into()));
            continue;
        }
        let Some(dir) = ws.asset_dir(a) else { continue };
        let mut freed = 0;
        if !opts.keep_downloads {
            for s in &a.sources {
                if let Source::Archive(x) = s {
                    let cache = ws.archive_cache(a, x);
                    for f in [cache.clone(), cache.with_file_name(format!("{}.part", file_name(&cache))), cache.with_file_name(format!("{}.part.json", file_name(&cache)))] {
                        if let Ok(md) = fs::metadata(&f)
                            && fs::remove_file(&f).is_ok()
                        {
                            freed += md.len();
                        }
                    }
                }
            }
        }
        let had_lock = ws.lock.remove(&a.id);
        let _ = fs::remove_file(index_path(&ws.lock_path, &a.id));
        if had_lock {
            ws.save_lock()?;
        }
        if !dir.exists() {
            out.push(RemoveOutcome { id: a.id.clone(), result: "not-installed".into(), message: None, freed });
            continue;
        }
        let (bytes, _) = fsutil::tree_size(&dir, &[]).unwrap_or((0, 0));
        let aside = fsutil::parent_dir(&dir).join(format!(".flux-removing-{}-{}", a.id, std::process::id()));
        fsutil::rename_with_retry(&dir, &aside).with_context(|| format!("removing {} (is a file in it open?)", dir.display()))?;
        ui.line(&format!("  {}: removing {}", a.id, ws.rel(&dir)));
        match remove_tree(&aside) {
            Ok(()) => out.push(RemoveOutcome { id: a.id.clone(), result: "removed".into(), message: None, freed: freed + bytes }),
            Err(e) => out.push(RemoveOutcome {
                id: a.id.clone(),
                result: "removed".into(),
                message: Some(format!("some files are left in {}: {e:#}", aside.display())),
                freed,
            }),
        }
    }
    Ok(out)
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{FetchOptions, fetch};
    use crate::manifest::parse;
    use crate::workspace::RecordingUi;
    use std::io::Write;

    #[test]
    fn removes_installs_and_their_downloads() {
        let root = tempfile::tempdir().unwrap();
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("kit/kick.wav", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"RIFF kick").unwrap();
        let zip = w.finish().unwrap().into_inner();
        let cache = root.path().join("tools/_dl/kit/kit.zip");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, &zip).unwrap();
        let text = format!(
            r#"
schema = 1
[policy]
allowed_hosts = ["github.com"]
[[asset]]
id = "kit"
name = "Kit"
kind = "sample-library"
scope = "audio"
path = "instruments/kit"
version = "1"
license = {{ id = "CC0-1.0" }}
[[asset.source]]
type = "archive"
url = "https://github.com/x/kit.zip"
sha256 = "{}"
size = {}
format = "zip"
cache = "tools/_dl/kit/kit.zip"

[[asset]]
id = "synth"
name = "Synth"
kind = "plugin"
scope = "tool"
version = "1"
license = {{ id = "MIT" }}
[[asset.source]]
type = "external"
page = "https://github.com/x/synth"
instructions = "Run its installer."
"#,
            crate::hash::sha256_bytes(&zip),
            zip.len()
        );
        let mut ws = Workspace::from_loaded(root.path(), parse(&text).unwrap(), root.path().join("assets.toml"), root.path().join("assets.lock")).unwrap();
        let mut ui = RecordingUi::default();
        let o = fetch(&mut ws, &["kit".into()], &FetchOptions { yes: true, ..Default::default() }, &mut ui).unwrap();
        assert_eq!(o[0].result, "installed", "{:?}", ui.lines);
        assert!(root.path().join("instruments/kit/kit/kick.wav").is_file());
        assert!(ws.lock.get("kit").is_some());

        let r = remove(&mut ws, &["kit".into(), "synth".into()], &RemoveOptions::default(), &mut ui).unwrap();
        assert_eq!(r[0].result, "removed");
        assert_eq!(r[0].freed, 9 + zip.len() as u64);
        assert_eq!(r[1].result, "skipped");
        assert!(!root.path().join("instruments/kit").exists());
        assert!(!cache.exists());
        assert!(ws.lock.get("kit").is_none());
        assert!(crate::lock::Lock::load(&root.path().join("assets.lock")).unwrap().get("kit").is_none());
        assert!(fs::read_dir(root.path().join("instruments")).unwrap().next().is_none(), "nothing left aside");
        // removing again is harmless
        assert_eq!(remove(&mut ws, &["kit".into()], &RemoveOptions::default(), &mut ui).unwrap()[0].result, "not-installed");
    }
}
