//! A small glob matcher for `/`-separated relative paths: `*` and `?` stay inside one path component,
//! `**` spans any number of components (including none). Matching is case-sensitive.

use crate::fsutil::{self, EntryKind};
use anyhow::Result;
use std::path::Path;

/// Whether `path` matches `pattern`.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat, &segs)
}

fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| match_segments(rest, &segs[i..])),
        Some((p, rest)) => match segs.split_first() {
            Some((s, srest)) => match_component(p.as_bytes(), s.as_bytes()) && match_segments(rest, srest),
            None => false,
        },
    }
}

/// `*` and `?` wildcard match of one component (iterative, linear backtracking).
fn match_component(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether the pattern contains a wildcard.
pub fn has_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

/// Files below `root` matching `pattern` (relative, `/`-separated), as sorted relative paths. Directories named
/// `.git`, `target`, `node_modules` and `.venv` are not searched.
pub fn expand(root: &Path, pattern: &str) -> Result<Vec<String>> {
    if !has_wildcard(pattern) {
        return Ok(if root.join(pattern).is_file() { vec![pattern.to_string()] } else { vec![] });
    }
    // Walk only below the literal prefix of the pattern.
    let comps: Vec<&str> = pattern.split('/').collect();
    let fixed = comps.iter().take_while(|c| !has_wildcard(c)).count();
    let base_rel = comps[..fixed].join("/");
    let base = if base_rel.is_empty() { root.to_path_buf() } else { root.join(&base_rel) };
    if !base.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for e in fsutil::walk(&base, &[".git", "target", "node_modules", ".venv"])? {
        if e.kind != EntryKind::File {
            continue;
        }
        let rel = if base_rel.is_empty() { e.rel } else { format!("{base_rel}/{}", e.rel) };
        if matches(pattern, &rel) {
            out.push(rel);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components() {
        assert!(matches("*.sfz", "a.sfz"));
        assert!(!matches("*.sfz", "dir/a.sfz"));
        assert!(matches("Percussion/TB_hit_*", "Percussion/TB_hit_01.wav"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "abbc"));
        assert!(matches("*a*b*", "xxaxxbxx"));
        assert!(!matches("*a*b", "xxaxxbxx"));
    }

    #[test]
    fn double_star() {
        assert!(matches("sfz/**/*.sfz", "sfz/a.sfz"));
        assert!(matches("sfz/**/*.sfz", "sfz/x/y/a.sfz"));
        assert!(matches("**", "anything/at/all"));
        assert!(matches("Surge XT.vst3/**", "Surge XT.vst3/Contents/x64-win/Surge XT.vst3"));
        assert!(!matches("Surge XT.vst3/**", "Surge XT Effects.vst3/Contents/x"));
    }

    #[test]
    fn expands() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("p/q")).unwrap();
        std::fs::write(d.path().join("p/a.json"), "").unwrap();
        std::fs::write(d.path().join("p/q/b.json"), "").unwrap();
        std::fs::write(d.path().join("top.py"), "").unwrap();
        assert_eq!(expand(d.path(), "p/*.json").unwrap(), ["p/a.json"]);
        assert_eq!(expand(d.path(), "p/**/*.json").unwrap(), ["p/a.json", "p/q/b.json"]);
        assert_eq!(expand(d.path(), "top.py").unwrap(), ["top.py"]);
        assert!(expand(d.path(), "missing/*.x").unwrap().is_empty());
    }
}
