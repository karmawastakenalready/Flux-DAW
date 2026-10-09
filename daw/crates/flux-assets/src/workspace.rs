//! The project an asset command works on: root folder, manifest, lock file, and how to talk to the user.

use crate::fsutil;
use crate::http::HttpConfig;
use crate::lock::Lock;
use crate::manifest::{self, ArchiveSource, Asset, Loaded};
use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Default manifest file name, at the project root.
pub const MANIFEST_FILE: &str = "assets.toml";
/// Default lock file name, next to the manifest.
pub const LOCK_FILE: &str = "assets.lock";

/// How commands report progress and ask questions.
pub trait Ui {
    /// One line of normal output.
    fn line(&mut self, msg: &str);
    /// Transfer or hashing progress; may be called often.
    fn progress(&mut self, label: &str, done: u64, total: Option<u64>);
    /// The current progress display is finished.
    fn progress_end(&mut self) {}
    /// Ask a yes/no question; `false` when nobody can answer.
    fn confirm(&mut self, question: &str) -> bool;
}

/// Prints lines to stdout and progress to stderr; asks on stdin only when it is a terminal.
#[derive(Debug, Default)]
pub struct ConsoleUi {
    last_progress: Option<std::time::Instant>,
    progress_shown: bool,
}

impl Ui for ConsoleUi {
    fn line(&mut self, msg: &str) {
        self.progress_end();
        println!("{msg}");
    }

    fn progress(&mut self, label: &str, done: u64, total: Option<u64>) {
        if !std::io::stderr().is_terminal() {
            return;
        }
        let now = std::time::Instant::now();
        if self.last_progress.is_some_and(|t| now.duration_since(t).as_millis() < 100) && total != Some(done) {
            return;
        }
        self.last_progress = Some(now);
        let text = match total {
            Some(t) if t > 0 => format!(
                "  {label}: {} / {} ({:.0}%)",
                fsutil::fmt_bytes(done),
                fsutil::fmt_bytes(t),
                done as f64 * 100.0 / t as f64
            ),
            _ => format!("  {label}: {}", fsutil::fmt_bytes(done)),
        };
        eprint!("\r{text:<78}");
        let _ = std::io::stderr().flush();
        self.progress_shown = true;
    }

    fn progress_end(&mut self) {
        if self.progress_shown {
            eprint!("\r{:<78}\r", "");
            let _ = std::io::stderr().flush();
            self.progress_shown = false;
        }
    }

    fn confirm(&mut self, question: &str) -> bool {
        self.progress_end();
        if !std::io::stdin().is_terminal() {
            println!("{question} [y/N] (no terminal to answer; pass --yes)");
            return false;
        }
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim(), "y" | "Y" | "yes" | "Yes")
    }
}

/// Collects output in memory and answers every question with a fixed value. For tests and embedding.
#[derive(Debug, Default)]
pub struct RecordingUi {
    pub lines: Vec<String>,
    pub questions: Vec<String>,
    pub answer: bool,
}

impl Ui for RecordingUi {
    fn line(&mut self, msg: &str) {
        self.lines.push(msg.to_string());
    }
    fn progress(&mut self, _label: &str, _done: u64, _total: Option<u64>) {}
    fn confirm(&mut self, question: &str) -> bool {
        self.questions.push(question.to_string());
        self.answer
    }
}

/// Where the manifest and lock are, when not at their default places.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub manifest: Option<PathBuf>,
    pub lock: Option<PathBuf>,
}

/// A project root with its manifest and lock.
#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub lock_path: PathBuf,
    pub loaded: Loaded,
    pub lock: Lock,
    pub http: HttpConfig,
    /// Threads for hashing.
    pub threads: usize,
}

impl Workspace {
    /// Load `assets.toml` (or `opts.manifest`) and the lock next to it (or `opts.lock`).
    pub fn open(root: &Path, opts: &OpenOptions) -> Result<Self> {
        let manifest_path = opts.manifest.clone().unwrap_or_else(|| root.join(MANIFEST_FILE));
        let loaded = manifest::load(&manifest_path)?;
        let lock_path = opts.lock.clone().unwrap_or_else(|| fsutil::parent_dir(&manifest_path).join(LOCK_FILE));
        Self::from_loaded(root, loaded, manifest_path, lock_path)
    }

    /// Build from an already loaded manifest (tests, embedding).
    pub fn from_loaded(root: &Path, loaded: Loaded, manifest_path: PathBuf, lock_path: PathBuf) -> Result<Self> {
        let lock = Lock::load(&lock_path)?;
        let mut http = HttpConfig::new(loaded.manifest.policy.allowed_hosts.clone());
        http.allow_loopback_http = loaded.allow_local_urls;
        Ok(Self {
            root: root.to_path_buf(),
            manifest_path,
            lock_path,
            loaded,
            lock,
            http,
            threads: crate::hash::default_threads(),
        })
    }

    /// Absolute install folder of an asset.
    pub fn asset_dir(&self, a: &Asset) -> Option<PathBuf> {
        a.path.as_ref().map(|p| self.root.join(p))
    }

    /// Whether the asset's folder exists and is not empty.
    pub fn is_installed(&self, a: &Asset) -> bool {
        self.asset_dir(a).is_some_and(|d| std::fs::read_dir(d).is_ok_and(|mut it| it.next().is_some()))
    }

    /// Where an archive is (or will be) cached.
    pub fn archive_cache(&self, a: &Asset, src: &ArchiveSource) -> PathBuf {
        if let Some(c) = &src.cache {
            return self.root.join(c);
        }
        let name = src.file_name.clone().unwrap_or_else(|| file_name_from_url(&src.url).unwrap_or_else(|| format!("{}.{}", a.id, src.format.as_str())));
        self.root.join(&self.loaded.manifest.policy.download_dir).join(&a.id).join(name)
    }

    /// Folder for temporary work that must stay on a disk with room (next to the lock file).
    pub fn work_dir(&self) -> PathBuf {
        fsutil::parent_dir(&self.lock_path).to_path_buf()
    }

    /// Write the lock file.
    pub fn save_lock(&self) -> Result<()> {
        self.lock.save(&self.lock_path).with_context(|| format!("saving {}", self.lock_path.display()))
    }

    /// Path relative to the root, for messages.
    pub fn rel(&self, p: &Path) -> String {
        fsutil::relative_to(&self.root, p).unwrap_or_else(|| p.display().to_string())
    }
}

/// Last path component of a URL, without query, percent-decoded for plain ASCII.
pub fn file_name_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    let decoded = percent_decode(last);
    (!decoded.is_empty() && crate::manifest::check_rel_path(&decoded, "file name").is_ok() && !decoded.contains('/')).then_some(decoded)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_file_names() {
        assert_eq!(file_name_from_url("https://x/y/sfizz-1.2.3-win64.zip").as_deref(), Some("sfizz-1.2.3-win64.zip"));
        assert_eq!(file_name_from_url("https://x/a%20b.zip?x=1").as_deref(), Some("a b.zip"));
        assert_eq!(file_name_from_url("https://x/download?id=1"), Some("download".into()));
        assert_eq!(file_name_from_url("https://x/"), None);
        assert_eq!(file_name_from_url("https://x/%2E%2E"), None);
    }
}
