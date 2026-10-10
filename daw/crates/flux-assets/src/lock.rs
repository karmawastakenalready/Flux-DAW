//! The lock file (`assets.lock`) records what is actually installed: for each asset the commit, archive
//! checksum and file count of every source, and a [`TreeIndex`] of the installed folder in
//! `assets.lock.d/<id>.tsv`. `verify` compares the disk against it.
//!
//! The lock is written only by `fetch` (after a verified install) and `lock` (after checking an existing
//! install against its sources). It is TOML and meant to be committed next to the manifest.

use crate::fsutil;
use crate::hash::{TreeIndex, sha256_bytes};
use crate::manifest::Asset;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Schema version of the lock file.
pub const LOCK_SCHEMA: u32 = 1;

/// The whole lock file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub schema: u32,
    #[serde(default, rename = "asset")]
    pub assets: Vec<LockedAsset>,
}

/// How a lock entry was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LockMethod {
    /// Installed by `fetch`.
    Fetch,
    /// An existing install that `lock` checked against its sources.
    Adopt,
    /// Installed by `import` from files downloaded by hand.
    Import,
}

/// One installed asset.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedAsset {
    pub id: String,
    /// Install folder relative to the root.
    pub path: String,
    pub method: LockMethod,
    /// RFC 3339 UTC.
    pub locked_at: String,
    /// Digest of the manifest entry's path and sources when this was locked; a different value means the
    /// manifest changed since (see [`spec_digest`]).
    pub spec_sha256: String,
    /// Regular files in the folder (`.git` excluded).
    pub files: u64,
    pub bytes: u64,
    /// SHA-256 of the tree index.
    pub tree_sha256: String,
    #[serde(default, rename = "source")]
    pub sources: Vec<LockedSource>,
}

/// What was verified for one source.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSource {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Git commit that is checked out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Verified archive checksum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Files this source contributed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    /// One line on how it was checked.
    pub verified: String,
}

impl Lock {
    pub fn new() -> Self {
        Self { schema: LOCK_SCHEMA, assets: Vec::new() }
    }

    /// Read a lock file; a missing file is an empty lock.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let lock: Lock = toml::from_str(&text).with_context(|| format!("{} is not a valid lock file", path.display()))?;
        if lock.schema != LOCK_SCHEMA {
            bail!("{}: lock schema {} is not supported (expected {LOCK_SCHEMA})", path.display(), lock.schema);
        }
        Ok(lock)
    }

    /// Write the lock file atomically, assets sorted by id.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut sorted = self.clone();
        sorted.assets.sort_by(|a, b| a.id.cmp(&b.id));
        let body = toml::to_string_pretty(&sorted).context("serialising the lock file")?;
        let text = format!(
            "# Written by `flux assets fetch` / `flux assets lock`. Do not edit by hand.\n\
             # Per-file checksums are in {}/<id>.tsv.\n\n{body}",
            index_dir_name(path)
        );
        fsutil::atomic_write(path, text.as_bytes())
    }

    pub fn get(&self, id: &str) -> Option<&LockedAsset> {
        self.assets.iter().find(|a| a.id == id)
    }

    /// Insert or replace the entry for `entry.id`.
    pub fn put(&mut self, entry: LockedAsset) {
        self.assets.retain(|a| a.id != entry.id);
        self.assets.push(entry);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.assets.len();
        self.assets.retain(|a| a.id != id);
        n != self.assets.len()
    }
}

fn index_dir_name(lock_path: &Path) -> String {
    format!("{}.d", lock_path.file_name().map_or("assets.lock".into(), |n| n.to_string_lossy()))
}

/// Folder holding the per-asset tree indexes: `<lock file>.d`.
pub fn index_dir(lock_path: &Path) -> PathBuf {
    fsutil::parent_dir(lock_path).join(index_dir_name(lock_path))
}

/// Where the tree index of an asset lives.
pub fn index_path(lock_path: &Path, id: &str) -> PathBuf {
    index_dir(lock_path).join(format!("{id}.tsv"))
}

/// Write an asset's tree index next to the lock file.
pub fn write_index(lock_path: &Path, id: &str, index: &TreeIndex) -> Result<()> {
    fsutil::atomic_write(&index_path(lock_path, id), index.to_tsv().as_bytes())
}

/// Read an asset's tree index and check it against the digest recorded in the lock.
pub fn read_index(lock_path: &Path, entry: &LockedAsset) -> Result<TreeIndex> {
    let p = index_path(lock_path, &entry.id);
    let text = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let idx = TreeIndex::parse_tsv(&text).with_context(|| format!("in {}", p.display()))?;
    let d = idx.digest();
    if d != entry.tree_sha256 {
        bail!("{} does not match the lock file (digest {d}, lock says {}); the lock is corrupt or was edited", p.display(), entry.tree_sha256);
    }
    Ok(idx)
}

/// Digest of the parts of a manifest entry that decide what gets installed (path and sources).
pub fn spec_digest(asset: &Asset) -> String {
    let v = serde_json::json!({ "path": asset.path, "sources": asset.sources });
    sha256_bytes(v.to_string().as_bytes())
}

/// Exclusive "someone is changing installs" marker next to the lock file, removed on drop.
#[derive(Debug)]
pub struct BusyGuard {
    path: PathBuf,
}

impl BusyGuard {
    /// Fails when another fetch/lock/repair holds the marker.
    pub fn acquire(lock_path: &Path) -> Result<Self> {
        let path = fsutil::parent_dir(lock_path).join(format!("{}.busy", lock_path.file_name().map_or("assets.lock".into(), |n| n.to_string_lossy())));
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "pid {} since {}", std::process::id(), fsutil::now_rfc3339());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let who = fs::read_to_string(&path).unwrap_or_default();
                bail!(
                    "another asset operation is running ({}). If none is, delete {}",
                    who.trim(),
                    path.display()
                )
            }
            Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::IndexEntry;

    #[test]
    fn round_trip() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("assets.lock");
        assert!(Lock::load(&lp).unwrap().assets.is_empty());
        let idx = TreeIndex::new(vec![IndexEntry { path: "a.wav".into(), size: 3, sha256: "a".repeat(64) }]).unwrap();
        let mut lock = Lock::new();
        let entry = LockedAsset {
            id: "x".into(),
            path: "instruments/x".into(),
            method: LockMethod::Adopt,
            locked_at: fsutil::now_rfc3339(),
            spec_sha256: "b".repeat(64),
            files: 1,
            bytes: 3,
            tree_sha256: idx.digest(),
            sources: vec![LockedSource { kind: "git".into(), rev: Some("c".repeat(40)), verified: "blob ids".into(), ..Default::default() }],
        };
        lock.put(entry.clone());
        lock.put(entry);
        assert_eq!(lock.assets.len(), 1);
        lock.save(&lp).unwrap();
        write_index(&lp, "x", &idx).unwrap();
        let back = Lock::load(&lp).unwrap();
        assert_eq!(back.assets[0].sources[0].rev.as_deref(), Some("c".repeat(40).as_str()));
        assert_eq!(read_index(&lp, &back.assets[0]).unwrap(), idx);
        assert!(index_path(&lp, "x").ends_with("assets.lock.d/x.tsv"));
        std::fs::write(index_path(&lp, "x"), idx.to_tsv().replace("a.wav", "b.wav")).unwrap();
        assert!(read_index(&lp, &back.assets[0]).is_err(), "edited index is detected");
    }

    #[test]
    fn busy_guard_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("assets.lock");
        let g = BusyGuard::acquire(&lp).unwrap();
        assert!(BusyGuard::acquire(&lp).is_err());
        drop(g);
        assert!(BusyGuard::acquire(&lp).is_ok());
    }
}
