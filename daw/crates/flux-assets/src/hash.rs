//! Content hashes: SHA-256 of files and streams, git blob ids, and the tree index that pins an installed
//! asset folder file by file.
//!
//! # Tree index format
//!
//! One line per regular file, sorted by path (byte order), `/`-separated, `.git` directories excluded:
//!
//! ```text
//! <sha256 hex>\t<size in bytes>\t<relative path>\n
//! ```
//!
//! The tree digest is the SHA-256 of exactly those bytes. It changes when any file is added, removed,
//! renamed or modified. The format is simple on purpose, so any tool (`sha256sum`, a ten-line script) can
//! rebuild an index and compare it with `assets.lock.d/<id>.tsv`.

use crate::fsutil::{self, EntryKind};
use anyhow::{Context, Result, bail};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const BUF: usize = 1 << 20;

/// SHA-256 (lowercase hex) and length of everything `r` yields.
pub fn sha256_reader(mut r: impl Read) -> io::Result<(String, u64)> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; BUF];
    let mut n = 0u64;
    loop {
        let k = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex::encode(h.finalize()), n))
}

/// SHA-256 (lowercase hex) and size of a file.
pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    sha256_reader(f).with_context(|| format!("reading {}", path.display()))
}

/// SHA-256 of a byte slice (lowercase hex).
pub fn sha256_bytes(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// Git's object id for a blob with this content: `sha1("blob <len>\0" + content)`.
pub fn git_blob_id_bytes(b: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", b.len()).as_bytes());
    h.update(b);
    hex::encode(h.finalize())
}

/// Hashes of one file computed in a single read: SHA-256, the git blob id, and the size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHashes {
    pub sha256: String,
    pub git_blob: String,
    pub size: u64,
}

/// SHA-256 and git blob id of a file in one pass.
pub fn hash_file_both(path: &Path) -> Result<FileHashes> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata().with_context(|| format!("reading {}", path.display()))?.len();
    let mut s2 = Sha256::new();
    let mut s1 = Sha1::new();
    s1.update(format!("blob {len}\0").as_bytes());
    let mut buf = vec![0u8; BUF];
    let mut n = 0u64;
    loop {
        let k = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        s2.update(&buf[..k]);
        s1.update(&buf[..k]);
        n += k as u64;
    }
    if n != len {
        bail!("{} changed size while it was being hashed ({len} -> {n} bytes)", path.display());
    }
    Ok(FileHashes { sha256: hex::encode(s2.finalize()), git_blob: hex::encode(s1.finalize()), size: n })
}

/// Whether `content` matches the git blob `expected` once its CRLF line endings are turned back into LF, i.e.
/// the file differs from the repository only by line-ending conversion (`core.autocrlf`).
pub fn is_eol_only_difference(content: &[u8], expected: &str) -> bool {
    if !content.windows(2).any(|w| w == b"\r\n") {
        return false;
    }
    let mut lf = Vec::with_capacity(content.len());
    let mut i = 0;
    while i < content.len() {
        if content[i] == b'\r' && content.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        lf.push(content[i]);
        i += 1;
    }
    git_blob_id_bytes(&lf) == expected
}

/// One file in a [`TreeIndex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Every regular file below an asset folder with its size and SHA-256. See the module docs for the format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeIndex {
    entries: Vec<IndexEntry>,
}

/// Differences between an expected and an actual [`TreeIndex`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeDiff {
    /// In the actual tree only.
    pub added: Vec<String>,
    /// In the expected tree only.
    pub removed: Vec<String>,
    /// In both, with different content.
    pub changed: Vec<String>,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

impl TreeIndex {
    /// Build an index from entries in any order. Duplicate paths are an error.
    pub fn new(mut entries: Vec<IndexEntry>) -> Result<Self> {
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        for w in entries.windows(2) {
            if w[0].path == w[1].path {
                bail!("duplicate path in tree index: {}", w[0].path);
            }
        }
        for e in &entries {
            if e.path.is_empty() || e.path.contains(['\t', '\n', '\r', '\\']) || e.path.starts_with('/') {
                bail!("invalid path in tree index: {:?}", e.path);
            }
            if !is_sha256_hex(&e.sha256) {
                bail!("invalid sha256 for {} in tree index", e.path);
            }
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sum of all file sizes.
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.binary_search_by(|e| e.path.as_bytes().cmp(path.as_bytes())).ok().map(|i| &self.entries[i])
    }

    /// The canonical TSV text.
    pub fn to_tsv(&self) -> String {
        let mut s = String::with_capacity(self.entries.len() * 100);
        for e in &self.entries {
            s.push_str(&e.sha256);
            s.push('\t');
            s.push_str(&e.size.to_string());
            s.push('\t');
            s.push_str(&e.path);
            s.push('\n');
        }
        s
    }

    /// Parse the TSV text written by [`TreeIndex::to_tsv`].
    pub fn parse_tsv(text: &str) -> Result<Self> {
        let mut entries = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let mut it = line.splitn(3, '\t');
            let (Some(sha), Some(size), Some(path)) = (it.next(), it.next(), it.next()) else {
                bail!("line {}: expected '<sha256>\\t<size>\\t<path>'", i + 1);
            };
            let size: u64 = size.parse().with_context(|| format!("line {}: bad size '{size}'", i + 1))?;
            entries.push(IndexEntry { path: path.to_string(), size, sha256: sha.to_ascii_lowercase() });
        }
        Self::new(entries)
    }

    /// SHA-256 of the canonical TSV text.
    pub fn digest(&self) -> String {
        sha256_bytes(self.to_tsv().as_bytes())
    }

    /// What changed going from `self` (expected) to `actual`.
    pub fn diff(&self, actual: &TreeIndex) -> TreeDiff {
        let exp: BTreeMap<&str, &IndexEntry> = self.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        let act: BTreeMap<&str, &IndexEntry> = actual.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        let mut d = TreeDiff::default();
        for (p, e) in &exp {
            match act.get(p) {
                None => d.removed.push(p.to_string()),
                Some(a) if a.size != e.size || a.sha256 != e.sha256 => d.changed.push(p.to_string()),
                Some(_) => {}
            }
        }
        d.added = act.keys().filter(|p| !exp.contains_key(*p)).map(|p| p.to_string()).collect();
        d
    }

    /// Only the entries for which `keep(path)` is true.
    pub fn filtered(&self, keep: impl Fn(&str) -> bool) -> TreeIndex {
        TreeIndex { entries: self.entries.iter().filter(|e| keep(&e.path)).cloned().collect() }
    }
}

/// Whether `s` is 64 lowercase or uppercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `s` is a 40-digit hex SHA-1 (a git commit or blob id).
pub fn is_sha1_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Progress of a long hashing job, safe to read from another thread.
#[derive(Debug, Default)]
pub struct HashProgress {
    pub files_done: AtomicUsize,
    pub bytes_done: AtomicU64,
}

/// A hashed tree: the SHA-256 index plus the git blob id of every file.
#[derive(Clone, Debug, Default)]
pub struct HashedTree {
    pub index: TreeIndex,
    /// Git blob id per path (same order as `index`).
    pub git_blobs: BTreeMap<String, String>,
}

/// Hash every regular file below `root` (skipping `.git`), using up to `threads` threads.
///
/// Links, junctions and special files are errors: an installed asset must be plain files.
pub fn hash_tree(root: &Path, threads: usize, progress: Option<&HashProgress>) -> Result<HashedTree> {
    let walked = fsutil::walk(root, &[".git"])?;
    if let Some(bad) = walked.iter().find(|e| e.kind != EntryKind::File) {
        bail!(
            "{} is a {}; asset folders must contain only regular files",
            bad.path.display(),
            if bad.kind == EntryKind::Link { "symbolic link or junction" } else { "special file" }
        );
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<FileHashes>>> = Mutex::new(vec![None; walked.len()]);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let threads = threads.clamp(1, 64).min(walked.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= walked.len() || first_err.lock().map(|e| e.is_some()).unwrap_or(true) {
                        break;
                    }
                    match hash_file_both(&walked[i].path) {
                        Ok(h) => {
                            if let Some(p) = progress {
                                p.files_done.fetch_add(1, Ordering::Relaxed);
                                p.bytes_done.fetch_add(h.size, Ordering::Relaxed);
                            }
                            if let Ok(mut r) = results.lock() {
                                r[i] = Some(h);
                            }
                        }
                        Err(e) => {
                            if let Ok(mut fe) = first_err.lock() {
                                fe.get_or_insert(e);
                            }
                            break;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = first_err.into_inner().unwrap_or(None) {
        return Err(e);
    }
    let results = results.into_inner().map_err(|_| anyhow::anyhow!("a hashing thread panicked"))?;
    let mut entries = Vec::with_capacity(walked.len());
    let mut git_blobs = BTreeMap::new();
    for (w, h) in walked.iter().zip(results) {
        let h = h.with_context(|| format!("{} was not hashed", w.path.display()))?;
        git_blobs.insert(w.rel.clone(), h.git_blob);
        entries.push(IndexEntry { path: w.rel.clone(), size: h.size, sha256: h.sha256 });
    }
    Ok(HashedTree { index: TreeIndex::new(entries)?, git_blobs })
}

/// A sensible default thread count for disk-bound hashing.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hashes() {
        assert_eq!(sha256_bytes(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        // `git hash-object` of an empty file and of "hello\n"
        assert_eq!(git_blob_id_bytes(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(git_blob_id_bytes(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn eol_only() {
        let blob = git_blob_id_bytes(b"a\nb\n");
        assert!(is_eol_only_difference(b"a\r\nb\r\n", &blob));
        assert!(!is_eol_only_difference(b"a\r\nc\r\n", &blob));
        assert!(!is_eol_only_difference(b"a\nb\n", &blob), "identical content is not an eol difference");
    }

    #[test]
    fn tsv_round_trip_and_diff() {
        let e = |p: &str, s: u64, h: char| IndexEntry { path: p.into(), size: s, sha256: h.to_string().repeat(64) };
        let a = TreeIndex::new(vec![e("b/x", 2, 'b'), e("a", 1, 'a'), e("c", 3, 'c')]).unwrap();
        assert_eq!(a.entries()[0].path, "a");
        let back = TreeIndex::parse_tsv(&a.to_tsv()).unwrap();
        assert_eq!(a, back);
        assert_eq!(a.digest(), back.digest());
        let b = TreeIndex::new(vec![e("a", 1, 'a'), e("b/x", 2, 'f'), e("d", 4, 'd')]).unwrap();
        let d = a.diff(&b);
        assert_eq!(d.added, ["d"]);
        assert_eq!(d.removed, ["c"]);
        assert_eq!(d.changed, ["b/x"]);
        assert!(TreeIndex::new(vec![e("a", 1, 'a'), e("a", 1, 'a')]).is_err());
        assert!(TreeIndex::parse_tsv("zz\t1\ta\n").is_err());
        assert!(TreeIndex::parse_tsv("not a line").is_err());
    }

    #[test]
    fn hashes_a_tree() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/b.txt"), "hello\n").unwrap();
        std::fs::write(d.path().join("a.txt"), "").unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::write(d.path().join(".git/HEAD"), "x").unwrap();
        let p = HashProgress::default();
        let t = hash_tree(d.path(), 3, Some(&p)).unwrap();
        assert_eq!(t.index.len(), 2);
        assert_eq!(t.git_blobs["sub/b.txt"], "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(p.files_done.load(Ordering::Relaxed), 2);
        assert_eq!(t.index.get("sub/b.txt").unwrap().size, 6);
    }
}
