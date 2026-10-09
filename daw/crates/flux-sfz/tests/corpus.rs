//! Every SFZ file of the repository (`instruments/`, `sfz/`, `build/sfz/`) must load without panicking, and
//! every file must survive a parse/write round trip byte for byte.
//!
//! Ignored by default because it needs the downloaded instrument libraries (12 GB); run it with
//! `cargo test -p flux-sfz --test corpus -- --ignored --nocapture`. Set `FLUX_SFZ_REPORT=<file.json>` to
//! write every diagnostic to a file.

use flux_sfz::{Document, Instrument, Severity};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().expect("repository root")
}

fn sfz_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            sfz_files(&p, out);
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sfz")) {
            out.push(p);
        }
    }
}

/// Group messages by their text without file-specific details, for the summary.
fn message_kind(msg: &str) -> String {
    let mut out = String::new();
    let mut in_tick = false;
    for c in msg.chars() {
        if c == '`' {
            in_tick = !in_tick;
            out.push_str(if in_tick { "`.." } else { "`" });
        } else if !in_tick {
            out.push(c);
        }
    }
    for prefix in ["sample not found", "cannot include"] {
        if out.starts_with(prefix) {
            return prefix.to_owned();
        }
    }
    out.chars().filter(|c| !c.is_ascii_digit()).collect()
}

#[test]
#[ignore = "needs the instrument libraries; run with --ignored"]
fn every_corpus_file_loads_and_round_trips() {
    let root = repo_root();
    let mut files = vec![];
    for d in ["instruments", "sfz", "build/sfz"] {
        sfz_files(&root.join(d), &mut files);
    }
    assert!(!files.is_empty(), "no SFZ files under {}", root.display());

    let mut round_trip_failures = vec![];
    let mut load_failures = vec![];
    let mut loaded = vec![];
    // files that other files include are fragments, not meant to be loaded on their own
    let mut included = HashSet::new();
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        let (doc, _) = Document::parse_bytes(&bytes, f);
        if doc.to_bytes() != bytes {
            round_trip_failures.push(f.clone());
        }
        match Instrument::load(f) {
            Ok(inst) => {
                for s in &inst.files()[1..] {
                    included.insert(s.path.canonicalize().unwrap_or_else(|_| s.path.clone()));
                }
                loaded.push((f.clone(), inst));
            }
            Err(e) => load_failures.push(format!("{}: {e}", f.display())),
        }
    }

    let mut kinds: BTreeMap<(Severity, String), usize> = BTreeMap::new();
    let (mut programs, mut with_errors, mut regions, mut playable) = (0, 0, 0, 0);
    let mut report = vec![];
    for (f, inst) in &loaded {
        let fragment = included.contains(&f.canonicalize().unwrap());
        let rel = f.strip_prefix(&root).unwrap_or(f).to_string_lossy().replace('\\', "/");
        for d in inst.diagnostics() {
            report.push(serde_json::json!({ "file": rel, "fragment": fragment, "diagnostic": d.to_string() }));
        }
        if fragment {
            continue;
        }
        programs += 1;
        regions += inst.regions().len();
        playable += inst.playable_regions().count();
        with_errors += usize::from(inst.errors().next().is_some());
        for d in inst.diagnostics() {
            *kinds.entry((d.severity, message_kind(&d.message))).or_default() += 1;
        }
    }

    println!("{} files: {programs} programs, {} fragments included by other files", files.len(), files.len() - programs);
    println!("programs: {regions} regions, {playable} with their sample present");
    println!("{with_errors} programs with errors; diagnostics of programs by kind:");
    for ((sev, kind), n) in &kinds {
        println!("  {n:6} {sev:?}: {kind}");
    }
    if let Ok(out) = std::env::var("FLUX_SFZ_REPORT") {
        std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        println!("report written to {out}");
    }
    assert!(load_failures.is_empty(), "files that could not be loaded: {load_failures:#?}");
    assert!(round_trip_failures.is_empty(), "files that did not round-trip: {round_trip_failures:#?}");
}
