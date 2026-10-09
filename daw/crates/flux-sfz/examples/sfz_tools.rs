//! Run the SFZ tools of this crate from the command line and compare their results with the Python tools they
//! replace. Used by the verification in `build/flux-sfz_check/`.
//!
//! ```text
//! sfz_tools tighten SRC OUT RELEASE_S NOTE REPORT.json
//! sfz_tools retune  SRC BASE_DIR OUT REPORT.json add|replace     (SRC is a fragment whose samples are relative to BASE_DIR)
//! sfz_tools even    SRC OUT REPORT.json RELEASE_S|- reset|noreset
//! sfz_tools compare A B OUT.json [BASE_DIR]                        (region by region; BASE_DIR loads both as fragments)
//! sfz_tools info    SFZ...                                         (the text description)
//! sfz_tools ranges  LIST.txt OUT.json                              (ranges and path checks of every file listed)
//! sfz_tools samples LIST.txt OUT.json                              (folder summaries of every folder listed)
//! ```

use flux_sfz::edit::{OutputFile, SamplePaths, write_files};
use flux_sfz::even::{EvenOptions, even};
use flux_sfz::info::describe;
use flux_sfz::retune::{RetuneOptions, retune};
use flux_sfz::tighten::{TightenOptions, tighten};
use flux_sfz::{Instrument, LoadOptions, SampleRef};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let s: Vec<&str> = a.iter().map(String::as_str).collect();
    let r = match s.as_slice() {
        ["tighten", src, out, rel, note, report] => run_tighten(src, out, rel, note, report),
        ["retune", src, base, out, report, mode] => run_retune(src, base, out, report, mode),
        ["even", src, out, report, rel, reset] => run_even(src, out, report, rel, reset),
        ["compare", a, b, out] => compare(a, b, out, None),
        ["compare", a, b, out, base] => compare(a, b, out, Some(base)),
        ["info", files @ ..] if !files.is_empty() => info(files),
        ["ranges", list, out] => ranges(list, out),
        ["samples", list, out] => samples(list, out),
        _ => Err("usage: see the documentation at the top of examples/sfz_tools.rs".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sfz_tools: {e}");
            ExitCode::FAILURE
        }
    }
}

fn write_json(path: &str, value: &impl Serialize) -> Res {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn finish(files: &[OutputFile], report: &impl Serialize, report_path: &str) -> Res {
    write_files(files)?;
    write_json(report_path, report)?;
    for f in files {
        println!("wrote {}", f.path.display());
    }
    Ok(())
}

fn run_tighten(src: &str, out: &str, rel: &str, note: &str, report: &str) -> Res {
    let inst = Instrument::load(src)?;
    let options = TightenOptions { release_s: rel.parse()?, note: Some(note.to_owned()), sample_paths: SamplePaths::Relative };
    let (files, r) = tighten(&inst, &std::path::absolute(out)?, &options)?;
    println!("{} release regions removed, {} ampeg_release changed, {} regions kept", r.release_regions_removed, r.release_opcodes_changed, r.regions_kept);
    finish(&files, &r, report)
}

/// A fragment (a map included by a program that sets `default_path`), loaded as if it were in `base`.
fn load_fragment(src: &str, base: &str) -> Res<Instrument> {
    let (text, _) = flux_sfz::text::decode(&std::fs::read(src)?);
    let name = Path::new(src).file_name().ok_or("no file name")?;
    Ok(Instrument::from_text(&text, std::path::absolute(base)?.join(name), &LoadOptions::default())?)
}

fn run_retune(src: &str, base: &str, out: &str, report: &str, mode: &str) -> Res {
    let inst = load_fragment(src, base)?;
    let options = RetuneOptions { keep_existing_tune: mode == "add", keep_sample_paths: true, ..RetuneOptions::default() };
    let (files, r) = retune(&inst, &std::path::absolute(out)?, &options)?;
    println!("{} retuned, {} left as is, {} removed, {} keys", r.retuned, r.left_as_is, r.removed, r.keys.len());
    finish(&files, &r, report)
}

fn run_even(src: &str, out: &str, report: &str, rel: &str, reset: &str) -> Res {
    let inst = Instrument::load(src)?;
    let options = EvenOptions {
        release_s: if rel == "-" { None } else { Some(rel.parse()?) },
        reset_between_notes: reset == "reset",
        ..EvenOptions::default()
    };
    let (files, r) = even(&inst, &std::path::absolute(out)?, &options)?;
    println!("{} regions evened, targets {:?} dB, {} skipped", r.regions.len(), r.targets_db, r.skipped);
    finish(&files, &r, report)
}

fn sample_key(r: &flux_sfz::Region) -> String {
    match r.sample() {
        SampleRef::File(f) => {
            let s = f.path.to_string_lossy().replace('\\', "/");
            if cfg!(windows) { s.to_lowercase() } else { s }
        }
        SampleRef::Generator { name } => name.clone(),
        SampleRef::None => String::new(),
    }
}

/// Region-by-region comparison: the sample each plays and every interpreted property. Numeric differences
/// are summarised per property (how many regions, largest difference).
fn compare(a: &str, b: &str, out: &str, base: Option<&str>) -> Res {
    let (ia, ib) = match base {
        Some(base) => (load_fragment(a, base)?, load_fragment(b, base)?),
        None => (Instrument::load(a)?, Instrument::load(b)?),
    };
    let mut fields: BTreeMap<String, (usize, f64, Vec<String>)> = BTreeMap::new();
    let mut sample_diffs = vec![];
    for (ra, rb) in ia.regions().iter().zip(ib.regions()) {
        if sample_key(ra) != sample_key(rb) {
            sample_diffs.push(format!("region {}: {} vs {}", ra.index(), sample_key(ra), sample_key(rb)));
        }
        let (Value::Object(pa), Value::Object(pb)) = (serde_json::to_value(ra.props())?, serde_json::to_value(rb.props())?) else {
            return Err("properties are not objects".into());
        };
        for (k, va) in &pa {
            let vb = pb.get(k).unwrap_or(&Value::Null);
            if va == vb {
                continue;
            }
            let e = fields.entry(k.clone()).or_insert((0, 0.0, vec![]));
            e.0 += 1;
            if let (Some(x), Some(y)) = (va.as_f64(), vb.as_f64()) {
                e.1 = e.1.max((x - y).abs());
            }
            if e.2.len() < 5 {
                e.2.push(format!("region {}: {va} vs {vb}", ra.index()));
            }
        }
    }
    let differing: BTreeMap<&String, Value> =
        fields.iter().map(|(k, (n, d, ex))| (k, json!({"regions": n, "max_abs_difference": d, "examples": ex}))).collect();
    let errors = |i: &Instrument| i.errors().count();
    let result = json!({
        "a": a, "b": b,
        "regions": [ia.regions().len(), ib.regions().len()],
        "playable": [ia.playable_regions().count(), ib.playable_regions().count()],
        "errors": [errors(&ia), errors(&ib)],
        "sample_differences": sample_diffs.len(),
        "sample_examples": sample_diffs.iter().take(5).collect::<Vec<_>>(),
        "property_differences": differing,
    });
    println!(
        "{} vs {}: regions {} / {}, {} sample differences, properties differing: {:?}",
        a,
        b,
        ia.regions().len(),
        ib.regions().len(),
        sample_diffs.len(),
        fields.iter().map(|(k, v)| format!("{k} ({} regions, max {})", v.0, v.1)).collect::<Vec<_>>()
    );
    write_json(out, &result)
}

fn info(files: &[&str]) -> Res {
    for f in files {
        let inst = Instrument::load(f)?;
        print!("{}", describe(&inst).to_text());
        println!();
    }
    Ok(())
}

fn list(path: &str) -> Res<Vec<PathBuf>> {
    let (text, _) = flux_sfz::text::decode(&std::fs::read(path)?);
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect())
}

fn ranges(list_path: &str, out: &str) -> Res {
    let mut results = vec![];
    for p in list(list_path)? {
        let inst = Instrument::load(&p)?;
        // as if every sample existed (what a text-only tool sees)
        let unchecked = Instrument::load_with(&p, &LoadOptions { check_samples: false, ..LoadOptions::default() })?;
        results.push(json!({
            "file": p,
            "ranges": flux_sfz::ranges::ranges(&inst),
            "keys_if_all_samples_existed": flux_sfz::ranges::ranges(&unchecked).keys,
            "paths": flux_sfz::audit::check_paths(&inst),
        }));
    }
    println!("{} files", results.len());
    write_json(out, &results)
}

fn samples(list_path: &str, out: &str) -> Res {
    let mut results = vec![];
    for p in list(list_path)? {
        results.push(json!({"folder": p, "summary": flux_sfz::samples::folder_samples(&p)?}));
    }
    println!("{} folders", results.len());
    write_json(out, &results)
}
