//! Shallow, sparse, byte-exact git checkouts through the `git` command-line tool.
//!
//! Byte-exact matters: Git for Windows enables `core.autocrlf` system-wide, which rewrites line endings of text
//! files on checkout, so the files on disk no longer match the repository's blobs (and SFZ files with CRLF are
//! harmless, but verification can no longer tell a CRLF rewrite from a real modification). Every command here
//! runs with `core.autocrlf=false` and `core.eol=lf`, clones store those settings in the repository, and
//! `.git/info/attributes` turns off text conversion, filters and `ident` expansion for all paths.

use crate::manifest::GitSource;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Attributes that make checkouts byte-exact copies of the blobs.
pub const EXACT_ATTRIBUTES: &str = "* -text -eol -filter -ident -working-tree-encoding\n";

/// One entry of `git ls-files -s -t`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitEntry {
    /// `H` (checked out), `S` (skip-worktree, i.e. outside the sparse checkout), or another status letter.
    pub tag: char,
    /// File mode: 100644, 100755, 120000 (symlink) or 160000 (submodule).
    pub mode: u32,
    pub blob: String,
    pub path: String,
}

impl GitEntry {
    pub fn is_checked_out(&self) -> bool {
        self.tag != 'S'
    }

    /// A symbolic link or submodule; neither belongs in an asset folder.
    pub fn is_special(&self) -> bool {
        self.mode == 0o120000 || self.mode == 0o160000
    }
}

/// The `git` executable.
#[derive(Clone, Debug)]
pub struct Git {
    exe: PathBuf,
    /// Show git's own progress output (stderr) for long operations.
    pub show_progress: bool,
}

impl Git {
    /// Use `git` from PATH (or `FLUX_GIT`), checking that it runs.
    pub fn find() -> Result<Self> {
        let exe = std::env::var_os("FLUX_GIT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("git"));
        let g = Self { exe, show_progress: false };
        let v = g.run(None, &["--version"], None).context("git is required for git sources; install Git and make sure `git` is on PATH")?;
        if !v.starts_with("git version") {
            bail!("unexpected `git --version` output: {v:?}");
        }
        Ok(g)
    }

    /// `git --version`.
    pub fn version(&self) -> Result<String> {
        Ok(self.run(None, &["--version"], None)?.trim().to_string())
    }

    fn command(&self, dir: Option<&Path>, read_only: bool) -> Command {
        let mut c = Command::new(&self.exe);
        if read_only {
            c.arg("--no-optional-locks");
        }
        c.args(["-c", "core.autocrlf=false", "-c", "core.eol=lf", "-c", "core.safecrlf=false", "-c", "advice.detachedHead=false"]);
        if let Some(d) = dir {
            c.arg("-C").arg(d);
        }
        c.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never").env("LC_ALL", "C").env("LANG", "C");
        c
    }

    fn exec(&self, mut c: Command, args: &[&str], stdin: Option<&[u8]>, progress: bool) -> Result<String> {
        c.args(args);
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        c.stdout(Stdio::piped());
        c.stderr(if progress { Stdio::inherit() } else { Stdio::piped() });
        let mut child = c.spawn().with_context(|| format!("starting {}", self.exe.display()))?;
        if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
            si.write_all(data).context("writing to git")?;
        }
        let out = child.wait_with_output().context("waiting for git")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("`git {}` failed ({}): {}", args.join(" "), out.status, err.trim());
        }
        String::from_utf8(out.stdout).context("git printed something that is not UTF-8")
    }

    fn run(&self, dir: Option<&Path>, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
        self.exec(self.command(dir, false), args, stdin, false)
    }

    fn run_ro(&self, dir: &Path, args: &[&str]) -> Result<String> {
        self.exec(self.command(Some(dir), true), args, None, false)
    }

    fn run_long(&self, dir: Option<&Path>, args: &[&str]) -> Result<String> {
        self.exec(self.command(dir, false), args, None, self.show_progress)
    }

    /// Clone `src` into `dest` (which must not exist or be empty) and check out exactly `src.rev`.
    /// Returns the checked-out commit.
    pub fn clone_pinned(&self, src: &GitSource, dest: &Path) -> Result<String> {
        let dest_s = dest.to_str().context("destination path is not valid Unicode")?;
        let mut args = vec!["clone", "--no-checkout", "--depth", "1", "--no-tags"];
        let filtered = !src.sparse.is_empty();
        if filtered {
            args.push("--filter=blob:none");
        }
        if let Some(b) = &src.branch {
            args.extend(["--branch", b.as_str(), "--single-branch"]);
        }
        args.extend(["--", src.url.as_str(), dest_s]);
        self.run_long(None, &args).with_context(|| format!("cloning {}", src.url))?;
        self.make_byte_exact(dest)?;
        if !src.sparse.is_empty() {
            self.set_sparse(dest, &src.sparse, src.cone)?;
        }
        let head = self.head(dest)?;
        if head == src.rev {
            self.run_long(Some(dest), &["checkout"])?;
        } else {
            let mut fetch = vec!["fetch", "--depth", "1", "--no-tags"];
            if filtered {
                fetch.push("--filter=blob:none");
            }
            fetch.extend(["origin", src.rev.as_str()]);
            self.run_long(Some(dest), &fetch).with_context(|| format!("fetching pinned commit {}", src.rev))?;
            self.run_long(Some(dest), &["checkout", "--detach", src.rev.as_str()])?;
        }
        let now = self.head(dest)?;
        if now != src.rev {
            bail!("checkout ended at {now}, expected {}", src.rev);
        }
        Ok(now)
    }

    /// Store `core.autocrlf=false`/`core.eol=lf` in the repository and write [`EXACT_ATTRIBUTES`].
    pub fn make_byte_exact(&self, repo: &Path) -> Result<()> {
        self.run(Some(repo), &["config", "core.autocrlf", "false"], None)?;
        self.run(Some(repo), &["config", "core.eol", "lf"], None)?;
        let info = repo.join(".git").join("info");
        fs::create_dir_all(&info).with_context(|| format!("creating {}", info.display()))?;
        crate::fsutil::atomic_write(&info.join("attributes"), EXACT_ATTRIBUTES.as_bytes())
    }

    /// Whether the repository is already configured for byte-exact checkouts.
    pub fn is_byte_exact(&self, repo: &Path) -> bool {
        let local = |k: &str| self.run_ro(repo, &["config", "--local", "--get", k]).map(|s| s.trim().to_string()).unwrap_or_default();
        let attrs = fs::read_to_string(repo.join(".git").join("info").join("attributes")).unwrap_or_default();
        local("core.autocrlf") == "false" && attrs.contains("-text")
    }

    /// Replace the sparse-checkout patterns and update the working tree.
    pub fn set_sparse(&self, repo: &Path, patterns: &[String], cone: bool) -> Result<()> {
        let input = patterns.join("\n") + "\n";
        let mode = if cone { "--cone" } else { "--no-cone" };
        self.exec(self.command(Some(repo), false), &["sparse-checkout", "set", mode, "--stdin"], Some(input.as_bytes()), self.show_progress)
            .context("setting sparse-checkout patterns")?;
        Ok(())
    }

    /// The commit checked out.
    pub fn head(&self, repo: &Path) -> Result<String> {
        let h = self.run_ro(repo, &["rev-parse", "--verify", "HEAD^{commit}"])?.trim().to_string();
        if !crate::hash::is_sha1_hex(&h) {
            bail!("unexpected HEAD {h:?} in {}", repo.display());
        }
        Ok(h)
    }

    /// `remote.origin.url`, if set.
    pub fn origin_url(&self, repo: &Path) -> Result<Option<String>> {
        match self.run_ro(repo, &["config", "--get", "remote.origin.url"]) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(_) => Ok(None),
        }
    }

    /// Sparse-checkout patterns, or `None` when the checkout is not sparse.
    pub fn sparse_patterns(&self, repo: &Path) -> Result<Option<(Vec<String>, bool)>> {
        let enabled = self.run_ro(repo, &["config", "--bool", "--get", "core.sparseCheckout"]).map(|s| s.trim() == "true").unwrap_or(false);
        if !enabled {
            return Ok(None);
        }
        let cone = self.run_ro(repo, &["config", "--bool", "--get", "core.sparseCheckoutCone"]).map(|s| s.trim() == "true").unwrap_or(false);
        // In cone mode `list` prints the directories rather than the generated patterns.
        let text = self.run_ro(repo, &["sparse-checkout", "list"])?;
        let pats: Vec<String> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect();
        Ok(Some((pats, cone)))
    }

    /// Every index entry with its skip-worktree status.
    pub fn ls_files(&self, repo: &Path) -> Result<Vec<GitEntry>> {
        let out = self.run_ro(repo, &["ls-files", "-s", "-t", "-z"])?;
        parse_ls_files(&out)
    }

    /// Rewrite the given paths from the repository (after [`Git::make_byte_exact`], this undoes CRLF
    /// conversion and local edits). Missing blobs of a partial clone are fetched.
    pub fn restore_paths(&self, repo: &Path, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for p in paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        self.exec(
            self.command(Some(repo), false),
            &["checkout", "--force", "HEAD", "--pathspec-from-file=-", "--pathspec-file-nul"],
            Some(&input),
            self.show_progress,
        )
        .context("restoring files from the repository")?;
        Ok(())
    }

    /// `git ls-remote URL [REF]`: the remote's refs as `(commit, ref name)` pairs. Downloads no objects.
    pub fn ls_remote(&self, url: &str, reference: Option<&str>) -> Result<Vec<(String, String)>> {
        let mut args = vec!["ls-remote", "--", url];
        if let Some(r) = reference {
            args.push(r);
        }
        let out = self.run(None, &args, None).with_context(|| format!("contacting {url}"))?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(h, _)| crate::hash::is_sha1_hex(h))
            .map(|(h, r)| (h.to_string(), r.to_string()))
            .collect())
    }

    /// Re-apply the sparse patterns to the working tree (adds missing files, removes ones outside the patterns).
    pub fn reapply_sparse(&self, repo: &Path) -> Result<()> {
        self.run_long(Some(repo), &["sparse-checkout", "reapply"]).map(|_| ())
    }
}

/// Parse `git ls-files -s -t -z` output: `TAG MODE BLOB STAGE\tPATH\0`.
pub fn parse_ls_files(out: &str) -> Result<Vec<GitEntry>> {
    let mut v = Vec::new();
    for rec in out.split('\0').filter(|r| !r.is_empty()) {
        let (meta, path) = rec.split_once('\t').with_context(|| format!("bad ls-files record {rec:?}"))?;
        let mut it = meta.split(' ');
        let (Some(tag), Some(mode), Some(blob), Some(_stage)) = (it.next(), it.next(), it.next(), it.next()) else {
            bail!("bad ls-files record {rec:?}");
        };
        let mode = u32::from_str_radix(mode, 8).with_context(|| format!("bad mode in {rec:?}"))?;
        let tag = tag.chars().next().with_context(|| format!("bad tag in {rec:?}"))?;
        v.push(GitEntry { tag, mode, blob: blob.to_string(), path: path.to_string() });
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ls_files() {
        let out = "H 100644 ce013625030ba8dba906f756967f9e9ca394464a 0\tdir/a b.txt\0S 100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 0\tskip.wav\0H 120000 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 0\tlink\0";
        let e = parse_ls_files(out).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].path, "dir/a b.txt");
        assert!(e[0].is_checked_out() && !e[1].is_checked_out());
        assert!(e[2].is_special());
        assert!(parse_ls_files("garbage\0").is_err());
    }
}
