//! The SFZ as text: `#include` inlining, the header comment, keyswitch keys, the library license, card names,
//! and the opcodes that make a sampler's output depend on what was played before.
//!
//! Everything here reads files as text only. The patterns mirror `tools/instrument_card.py` (which used Python
//! regular expressions with look-ahead; those are hand-coded here because the `regex` crate has no look-around).

use super::model::{Keyswitches, LibLicense, License};
use crate::json::OrderedMap;
use regex::Regex;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

pub const NOTES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// Note name of a MIDI key (C4 = 60).
pub fn note_name(k: i64) -> String {
    format!("{}{}", NOTES[k.rem_euclid(12) as usize], k.div_euclid(12) - 1)
}

/// `"60"`, `"C4"`, `"c#4"`, `"Eb2"` to a MIDI number (C4 = 60).
pub fn key_num(s: &str) -> Option<i64> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Ga-g])([#b]?)(-?\d+)$").unwrap());
    let s = s.trim();
    if let Some(m) = RE.captures(s) {
        let n = NOTES.iter().position(|x| x.eq_ignore_ascii_case(&m[1]))? as i64;
        let acc = match &m[2] {
            "#" => 1,
            "b" => -1,
            _ => 0,
        };
        return Some(n + acc + 12 * (m[3].parse::<i64>().ok()? + 1));
    }
    s.replace('_', "").parse().ok()
}

/// A key range `LO-HI` such as `36-96`, `C2-C7` or `C-1-G9` (split at the first `-` that follows a digit).
pub fn parse_key_range(s: &str) -> Option<(i64, i64)> {
    let b = s.as_bytes();
    let i = (1..b.len()).find(|&i| b[i] == b'-' && b[i - 1].is_ascii_digit())?;
    Some((key_num(&s[..i])?, key_num(&s[i + 1..])?))
}

/// A text file decoded leniently (invalid UTF-8 replaced) with universal newlines, as Python's text mode reads it.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(normalize_newlines(&String::from_utf8_lossy(&bytes)))
}

pub fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Lexical path normalization (Python's `os.path.normpath`): drops `.` and resolves `..` without touching the disk.
pub fn normpath(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = vec![];
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out.iter().collect()
}

/// The SFZ with its `#include "..."` files inlined (text only, up to 8 levels deep). Includes resolve against
/// the top file's folder first, then the including file's folder; a missing include becomes empty text.
pub fn sfz_text(path: &Path) -> std::io::Result<String> {
    let root = path.parent().unwrap_or(Path::new("")).to_path_buf();
    sfz_text_at(path, &root, 0)
}

fn sfz_text_at(path: &Path, root_dir: &Path, depth: usize) -> std::io::Result<String> {
    static INC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"#include\s+"([^"]+)""#).unwrap());
    let text = read_text(path)?;
    if depth > 8 {
        return Ok(text);
    }
    let here = path.parent().unwrap_or(Path::new(""));
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in INC.captures_iter(&text) {
        let whole = m.get(0).unwrap_or_else(|| unreachable!());
        out.push_str(&text[last..whole.start()]);
        for base in [root_dir, here] {
            let p = normpath(&base.join(&m[1]));
            if p.is_file() {
                out.push_str(&sfz_text_at(&p, root_dir, depth + 1)?);
                break;
            }
        }
        last = whole.end();
    }
    out.push_str(&text[last..]);
    Ok(out)
}

/// The SFZ's header comment: the `//` lines among its first three lines, joined (at most 240 characters).
pub fn about(text: &str) -> Option<String> {
    let lines: Vec<&str> = text
        .split('\n')
        .take(3)
        .filter(|l| l.starts_with("//"))
        .map(|l| l.trim_matches(|c| c == '/' || c == ' ').trim())
        .collect();
    let s: String = lines.join(" ").chars().take(240).collect();
    (!s.is_empty()).then_some(s)
}

fn opcode_keys(text: &str, op: &str) -> Vec<i64> {
    let re = Regex::new(&format!(r"\b{op}=([A-Ga-g][#b]?-?\d|\d+)")).unwrap_or_else(|_| unreachable!());
    let mut v: Vec<i64> = re.captures_iter(text).filter_map(|m| key_num(&m[1])).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The `sw_last` keyswitch keys with their labels, and the `sw_default` key; `None` without keyswitches.
pub fn keyswitches(text: &str, labels: &OrderedMap<String>) -> Option<Keyswitches> {
    let ks = opcode_keys(text, "sw_last");
    if ks.is_empty() {
        return None;
    }
    let keys = ks.iter().map(|k| (k.to_string(), labels.get(&k.to_string()).cloned().unwrap_or_default())).collect();
    Some(Keyswitches { keys, default: opcode_keys(text, "sw_default").first().copied() })
}

/// What, besides the note itself, decides which sample a note plays. sfizz keeps this state between notes, so
/// renders of such an instrument depend on the order of everything played before.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Statefulness {
    /// Random opcodes (`lorand`/`hirand`, `*_random`): every render draws from sfizz's random generator.
    pub random: bool,
    /// Round robin (`seq_length`), previous-note/velocity switches, CC-triggered regions: every note-on counts.
    pub every_note: bool,
    /// Keyswitch keys (`sw_last`, `sw_lokey`..`sw_hikey`, `sw_down`, `sw_up`, `sw_lolast`..`sw_hilast`); a note on
    /// one of these keys changes the articulation of later notes.
    pub keyswitch_keys: Vec<i64>,
}

impl Statefulness {
    /// Renders don't depend on order at all.
    pub fn is_stateless(&self) -> bool {
        !self.random && !self.every_note && self.keyswitch_keys.is_empty()
    }

    /// Whether playing `key` can change how later notes sound.
    pub fn key_changes_state(&self, key: i64) -> bool {
        self.every_note || self.keyswitch_keys.binary_search(&key).is_ok()
    }
}

/// Scan the (include-inlined) SFZ for state-carrying opcodes. Conservative: any doubt counts as state.
pub fn statefulness(text: &str) -> Statefulness {
    static RAND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(lorand|hirand|\w*_random\w*)\s*=").unwrap());
    static EVERY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(seq_length|seq_position|sw_previous|sw_vel|on_locc\d+|on_hicc\d+|start_locc\d+|start_hicc\d+|trigger\s*=\s*(first|legato))\b").unwrap()
    });
    let mut st = Statefulness { random: RAND.is_match(text), every_note: false, keyswitch_keys: vec![] };
    // seq_length=1 everywhere is not a round robin
    static SEQ: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bseq_length\s*=\s*(\d+)").unwrap());
    let real_rr = SEQ.captures_iter(text).any(|m| m[1].parse::<u32>().map_or(true, |n| n > 1));
    st.every_note = EVERY.captures_iter(text).any(|m| !m[1].starts_with("seq_") || real_rr);
    let mut keys = vec![];
    for op in ["sw_last", "sw_down", "sw_up"] {
        keys.extend(opcode_keys(text, op));
    }
    for (lo, hi) in [("sw_lokey", "sw_hikey"), ("sw_lolast", "sw_hilast")] {
        let (los, his) = (opcode_keys(text, lo), opcode_keys(text, hi));
        if let (Some(a), Some(b)) = (los.first(), his.last()) {
            keys.extend(*a.min(b)..=*a.max(b));
        } else if let Some(k) = los.first().or(his.last()) {
            keys.push(*k);
        }
    }
    keys.sort_unstable();
    keys.dedup();
    st.keyswitch_keys = keys;
    st
}

// ------------------------------------------------------------------------------------------------ licenses

/// License names and their patterns, most specific first (as in the Python tool).
const LICENSES: [(&str, &str); 10] = [
    ("CC BY-NC-SA", r"Attribution-?NonCommercial-?ShareAlike|\bBY-NC-SA\b"),
    ("CC BY-NC", r"Attribution-?NonCommercial|\bBY-NC\b"),
    ("CC BY-SA", r"Attribution-?ShareAlike|\bBY-SA\b"),
    ("CC0 1.0", r"\bCC0\b|CC-0|Creative Commons Zero|publicdomain/zero"),
    ("CC BY", r"\bAttribution \d\.\d|\bCC[- ]BY\b"),
    ("GeneralUser GS License", r"GeneralUser GS.{0,40}License"),
    ("MIT", r"\bMIT License\b"),
    ("GPL", r"GNU General Public License|\bGPL\b"),
    ("BSD", r"\bBSD\b"),
    ("public domain", r"(?i)\bpublic domain\b"),
];

static LICENSE_RES: LazyLock<Vec<(&'static str, Regex)>> =
    LazyLock::new(|| LICENSES.iter().map(|(n, p)| (*n, Regex::new(p).unwrap_or_else(|_| unreachable!()))).collect());

fn is_line_start(text: &str, p: usize) -> bool {
    p == 0 || text.as_bytes()[p - 1] == b'\n'
}

/// `^#+\s` at `p` (a Markdown heading line).
fn heading_at(text: &str, p: usize) -> bool {
    let rest = &text[p..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    hashes > 0 && rest[hashes..].chars().next().is_some_and(char::is_whitespace)
}

/// The bodies of "Samples" sections of a Markdown license text (each up to the next heading).
fn sample_sections(text: &str) -> Vec<&str> {
    static HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?mi)^#+\s*Samples?\b[^\n]*\n").unwrap());
    let mut out = vec![];
    let mut pos = 0;
    while let Some(h) = HEAD.find_at(text, pos) {
        let start = h.end();
        let end = (start..=text.len())
            .filter(|&p| text.is_char_boundary(p))
            .find(|&p| p == text.len() || (is_line_start(text, p) && heading_at(text, p)))
            .unwrap_or(text.len());
        out.push(&text[start..end]);
        pos = end.max(h.start() + 1);
        while pos < text.len() && !text.is_char_boundary(pos) {
            pos += 1;
        }
        if pos >= text.len() {
            break;
        }
    }
    out
}

/// The license a LICENSE/README text names: a "Samples" section first (libraries with several licenses), then
/// the first 800 characters (the title), then the whole text.
pub fn license_name(text: &str) -> Option<String> {
    static VER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Attribution (\d\.\d)|CC[- ]BY[- ](\d\.\d)").unwrap());
    let head: String = text.chars().take(800).collect();
    let mut parts = sample_sections(text);
    parts.push(&head);
    parts.push(text);
    for part in parts {
        for (name, re) in LICENSE_RES.iter() {
            if re.is_match(part) {
                if *name == "CC BY"
                    && let Some(v) = VER.captures(part)
                {
                    let ver = v.get(1).or(v.get(2)).map_or("", |m| m.as_str());
                    return Some(format!("CC BY {ver}"));
                }
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Lowercase alphanumerics only (for loose name matching).
pub fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect()
}

/// The license LICENSES.md states for a library (its table row that mentions the folder name).
pub fn licenses_md_row(root: &Path, lib_name: &str) -> Option<String> {
    let text = read_text(&root.join("LICENSES.md")).ok()?;
    let key = norm(lib_name);
    for line in text.split('\n') {
        let cells: Vec<&str> = line.trim().trim_matches('|').split('|').map(str::trim).collect();
        if line.starts_with('|') && cells.len() >= 3 && norm(line).contains(&key) {
            return Some(cells[2].replace("**", ""));
        }
    }
    None
}

/// `(?=\s+\w+=|\s*<|\s*$)` (multi-line) at byte `e`.
fn ref_end_at(text: &str, e: usize) -> bool {
    let rest = &text[e..];
    let ws = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
    let after_ws = &rest[ws..];
    if after_ws.is_empty() || after_ws.starts_with('<') || rest[..ws].contains('\n') {
        return true;
    }
    if ws > 0 {
        let word = after_ws.len() - after_ws.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').len();
        if word > 0 && after_ws[word..].starts_with('=') {
            return true;
        }
    }
    false
}

/// Values of `default_path=`/`sample=` opcodes (each up to the next opcode, header or line end).
fn sample_refs(text: &str) -> Vec<&str> {
    static OP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:default_path|sample)=").unwrap());
    let mut out = vec![];
    let mut pos = 0;
    while let Some(m) = OP.find_at(text, pos) {
        let cs = m.end();
        let line_end = text[cs..].find('\n').map_or(text.len(), |i| cs + i);
        let end = (cs + 1..=line_end).filter(|&e| text.is_char_boundary(e)).find(|&e| ref_end_at(text, e));
        match end {
            Some(e) if cs < line_end => {
                out.push(&text[cs..e]);
                pos = e;
            }
            _ => pos = m.start() + 1,
        }
        if pos >= text.len() {
            break;
        }
    }
    out
}

/// Values of `default_path=` (the rest of the line, trailing whitespace dropped).
fn default_paths(text: &str) -> Vec<&str> {
    static OP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"default_path=").unwrap());
    let mut out = vec![];
    let mut pos = 0;
    while let Some(m) = OP.find_at(text, pos) {
        let cs = m.end();
        let line_end = text[cs..].find('\n').map_or(text.len(), |i| cs + i);
        if cs < line_end {
            let line = &text[cs..line_end];
            let trimmed = line.trim_end_matches(char::is_whitespace).len().max(line.chars().next().map_or(1, char::len_utf8));
            out.push(&line[..trimmed]);
            pos = line_end;
        } else {
            pos = m.start() + 1;
        }
        if pos >= text.len() {
            break;
        }
    }
    out
}

fn eq_component(a: &Component, b: &Component) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// `path` relative to `base` when it lies inside it (component-wise, case-insensitive on Windows).
pub fn inside(path: &Path, base: &Path) -> Option<PathBuf> {
    let pc: Vec<Component> = path.components().collect();
    let bc: Vec<Component> = base.components().collect();
    if pc.len() < bc.len() || !bc.iter().zip(&pc).all(|(a, b)| eq_component(a, b)) {
        return None;
    }
    Some(pc[bc.len()..].iter().collect())
}

fn absolute(p: &Path) -> PathBuf {
    normpath(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// The library folders (under `<root>/instruments`) an SFZ draws from, and the license of each.
pub fn license_of(root: &Path, path: &Path, text: &str) -> License {
    let inst = absolute(&root.join("instruments"));
    let sdir = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let refs: std::collections::BTreeSet<&str> = sample_refs(text).into_iter().collect();
    let dps: Vec<String> = default_paths(text).into_iter().map(|d| d.replace('\\', "/")).collect();
    let mut libs: Vec<String> = vec![];
    let first = path.to_string_lossy().to_string();
    for r in std::iter::once(first.as_str()).chain(refs.into_iter().take(3000)) {
        let r = r.replace('\\', "/");
        let mut cands = vec![PathBuf::from(&r), sdir.join(&r)];
        cands.extend(dps.iter().map(|d| sdir.join(d).join(&r)));
        for c in cands {
            let Some(rel) = inside(&absolute(&c), &inst) else { continue };
            let comps: Vec<_> = rel.components().collect();
            if comps.len() < 2 {
                continue;
            }
            let top = comps[0].as_os_str().to_string_lossy().to_string();
            if !libs.contains(&top) && inst.join(&top).is_dir() {
                libs.push(top);
            }
        }
    }
    static LIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(licen[cs]e|copying)").unwrap());
    static README: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^readme").unwrap());
    let mut out = vec![];
    for lib in &libs {
        let d = inst.join(lib);
        let mut names: Vec<String> = std::fs::read_dir(&d)
            .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        names.sort();
        let files: Vec<&String> = names.iter().filter(|f| LIC.is_match(f)).chain(names.iter().filter(|f| README.is_match(f))).collect();
        let (mut name, mut src) = (None, None);
        for f in files {
            let p = d.join(f);
            let Ok(t) = read_text(&p) else { continue };
            let t: String = t.chars().take(200_000).collect();
            if let Some(n) = license_name(&t) {
                name = Some(n);
                src = Some(portable(root, &p));
                break;
            }
        }
        let md = licenses_md_row(root, lib);
        let agrees = matches!((&name, &md), (Some(n), Some(m)) if norm(m).contains(&norm(n)));
        out.push(LibLicense {
            library: lib.clone(),
            license: name.unwrap_or_else(|| "unknown".into()),
            file: src,
            licenses_md: md.unwrap_or_else(|| "not listed".into()),
            agrees,
        });
    }
    let mut names: Vec<&str> = vec![];
    for o in &out {
        if !names.contains(&o.license.as_str()) {
            names.push(&o.license);
        }
    }
    let name = if names.is_empty() { "unknown".to_string() } else { names.join(" + ") };
    let all = !out.is_empty() && out.iter().all(|o| o.agrees);
    License { name, libraries: out, all_in_licenses_md: all }
}

// ------------------------------------------------------------------------------------------------ names

/// A path relative to the project root with forward slashes (absolute when outside the root).
pub fn portable(root: &Path, path: &Path) -> String {
    let p = absolute(path);
    match inside(&p, &absolute(root)) {
        Some(rel) => rel.to_string_lossy().replace('\\', "/"),
        None => p.to_string_lossy().replace('\\', "/"),
    }
}

/// Readable name for an SFZ path: `far_roads/flute`, `VCSL/Folk Harp`, `sfz/makima_kit`, `makima/synth_bass`.
pub fn ident(root: &Path, path: &Path) -> String {
    let rel = portable(root, path);
    let parts: Vec<&str> = rel.split('/').collect();
    let last = parts.last().copied().unwrap_or("");
    let stem = Path::new(last).file_stem().map_or(last.to_string(), |s| s.to_string_lossy().to_string());
    if parts.len() >= 2 && parts[0] == "build" && parts[1] == "sfz" {
        let sub = &parts[2..parts.len().saturating_sub(1).max(2)];
        match sub {
            [] => format!("build/{stem}"),
            [a] => format!("{a}/{stem}"),
            [a, b, ..] => format!("{a}/{b}"),
        }
    } else if parts[0] == "instruments" && parts.len() > 2 {
        format!("{}/{stem}", parts[1])
    } else if parts[0] == "sfz" {
        format!("sfz/{stem}")
    } else {
        stem
    }
}

/// Lowercase, runs of anything but `a-z0-9` become `-`.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    out
}

/// Identity of an instrument for de-duplication: identical wrappers (same text after `#include`s, with only
/// absolute `default_path`s) are one instrument wherever they live.
pub fn content_key(path: &Path) -> std::io::Result<String> {
    let text = sfz_text(path)?;
    let dps = default_paths(&text);
    let absolute = !dps.is_empty()
        && dps.iter().all(|d| {
            let b = d.as_bytes();
            matches!(b.first(), Some(b'/') | Some(b'\\'))
                || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
        });
    let dir = if absolute { String::new() } else { path.parent().map_or(String::new(), |d| d.to_string_lossy().to_lowercase()) };
    Ok(format!("{text}\u{0}{dir}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names_and_numbers() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(61), "C#4");
        assert_eq!(key_num("C4"), Some(60));
        assert_eq!(key_num("c#4"), Some(61));
        assert_eq!(key_num("Eb2"), Some(39));
        assert_eq!(key_num("C-1"), Some(0));
        assert_eq!(key_num(" 72 "), Some(72));
        assert_eq!(key_num("x"), None);
        assert_eq!(parse_key_range("36-96"), Some((36, 96)));
        assert_eq!(parse_key_range("C2-C7"), Some((36, 96)));
        assert_eq!(parse_key_range("C-1-G9"), Some((0, 127)));
        assert_eq!(parse_key_range("C"), None);
    }

    #[test]
    fn about_and_keyswitches() {
        let t = "// Flute, legato\n// by someone\n<region> sample=a.wav sw_last=C1 sw_last=25\nsw_default=24";
        assert_eq!(about(t).as_deref(), Some("Flute, legato by someone"));
        assert_eq!(about("<region>"), None);
        let mut labels = OrderedMap::new();
        labels.insert("24", "sustain".to_string());
        let ks = keyswitches(t, &labels).unwrap();
        assert_eq!(ks.keys.keys().collect::<Vec<_>>(), vec!["24", "25"]);
        assert_eq!(ks.keys.get("24").unwrap(), "sustain");
        assert_eq!(ks.default, Some(24));
        assert!(keyswitches("<region> sample=a.wav", &labels).is_none());
    }

    #[test]
    fn sample_refs_stop_at_the_next_opcode() {
        let t = "<control> default_path=../lib/x/\n<region> sample=foo bar.wav key=60\n<region>sample=a.wav<region> sample=b.wav  \n";
        assert_eq!(sample_refs(t), vec!["../lib/x/", "foo bar.wav", "a.wav", "b.wav"]);
        assert_eq!(default_paths(t), vec!["../lib/x/"]);
    }

    #[test]
    fn license_names() {
        assert_eq!(license_name("Creative Commons Attribution 3.0 Unported").as_deref(), Some("CC BY 3.0"));
        assert_eq!(license_name("This is CC0 stuff").as_deref(), Some("CC0 1.0"));
        assert_eq!(license_name("Released into the Public Domain").as_deref(), Some("public domain"));
        // the Samples section wins over the code license in the title
        let t = "# MIT License for the scripts\n\n## Samples\nCC-BY 4.0 (Attribution 4.0)\n## Other\nGPL\n";
        assert_eq!(license_name(t).as_deref(), Some("CC BY 4.0"));
        assert_eq!(license_name("nothing here"), None);
        assert_eq!(norm("CC BY 3.0"), "ccby30");
    }

    #[test]
    fn names() {
        let root = Path::new("/r");
        assert_eq!(ident(root, Path::new("/r/build/sfz/far_roads/flute.sfz")), "far_roads/flute");
        assert_eq!(ident(root, Path::new("/r/build/sfz/x.sfz")), "build/x");
        assert_eq!(ident(root, Path::new("/r/build/sfz/a/b/c.sfz")), "a/b");
        assert_eq!(ident(root, Path::new("/r/instruments/VCSL/Chordophones/Folk Harp.sfz")), "VCSL/Folk Harp");
        assert_eq!(ident(root, Path::new("/r/sfz/makima_kit.sfz")), "sfz/makima_kit");
        assert_eq!(slugify("VCSL/Folk Harp ks24 36-96"), "vcsl-folk-harp-ks24-36-96");
        assert_eq!(slugify("--A__b--"), "a-b");
    }

    #[test]
    fn state_detection() {
        assert!(statefulness("<region> sample=a.wav").is_stateless());
        assert!(statefulness("<region> seq_length=1 seq_position=1").is_stateless());
        let rr = statefulness("<region> seq_length=2 seq_position=1");
        assert!(rr.every_note && rr.key_changes_state(60));
        let ks = statefulness("<group> sw_lokey=24 sw_hikey=26 sw_last=24\n<region> sample=a.wav");
        assert_eq!(ks.keyswitch_keys, vec![24, 25, 26]);
        assert!(ks.key_changes_state(25) && !ks.key_changes_state(60));
        assert!(statefulness("<region> lorand=0 hirand=0.5").random);
        assert!(statefulness("<region> offset_random=100").random);
        assert!(statefulness("<region> trigger=first").every_note);
    }

    #[test]
    fn normpath_resolves_dots() {
        assert_eq!(normpath(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normpath(Path::new("../x")), PathBuf::from("../x"));
    }
}
