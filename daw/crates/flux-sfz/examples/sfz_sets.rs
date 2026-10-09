//! Build instrument sets into another folder, and list what a project's set output folders hold that no set
//! makes. Used by the verification in `build/flux-sfz_check/`.
//!
//! ```text
//! cargo run -p flux-sfz --example sfz_sets -- build ROOT SETS_DIR OUT_DIR relative|absolute REPORT.json
//! cargo run -p flux-sfz --example sfz_sets -- untracked ROOT SETS_DIR
//! cargo run -p flux-sfz --example sfz_sets -- specs ROOT SETS_DIR OUT.json
//! ```
//!
//! `build` loads every set in SETS_DIR, moves each set's output from `ROOT/build/sfz/...` to `OUT_DIR/...`,
//! builds it twice (the second build must find everything up to date) and writes the reports, the checks and
//! the per-instrument results as JSON. `untracked` lists the files in the real output folders that no set
//! accounts for. `specs` writes every set's instruments (after `from`) as JSON.

use flux_sfz::edit::SamplePaths;
use flux_sfz::manifest::{BuildAction, BuildOptions, CheckOptions, build, check, untracked};
use flux_sfz::set::SetFile;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("build") if args.len() == 6 => run_build(&args[1], &args[2], &args[3], &args[4], &args[5]),
        Some("untracked") if args.len() == 3 => run_untracked(&args[1], &args[2]),
        Some("specs") if args.len() == 4 => run_specs(&args[1], &args[2], &args[3]),
        _ => Err("usage: sfz_sets build ROOT SETS_DIR OUT_DIR relative|absolute REPORT.json | untracked ROOT SETS_DIR | \
                  specs ROOT SETS_DIR OUT.json"
            .into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sfz_sets: {e}");
            ExitCode::FAILURE
        }
    }
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn run_build(root: &str, sets_dir: &str, out: &str, mode: &str, report: &str) -> Res {
    let root = Path::new(root);
    let sample_paths = match mode {
        "relative" => SamplePaths::Relative,
        "absolute" => SamplePaths::Absolute,
        _ => return Err(format!("unknown sample path mode `{mode}`").into()),
    };
    let real_base = std::path::absolute(root)?.join("build").join("sfz");
    let out = std::path::absolute(out)?;
    let mut sets = SetFile::load_dir(Path::new(sets_dir), root)?;
    for set in &mut sets {
        let below = set.output.strip_prefix(&real_base).map_err(|_| format!("{}: output is not under build/sfz", set.name))?;
        set.output = out.join(below);
        set.sample_paths = sample_paths;
    }
    let mut reports = vec![];
    let mut failures = 0;
    for set in &sets {
        let first = build(set, &BuildOptions::default())?;
        failures += first.failed().count();
        let second = build(set, &BuildOptions::default())?;
        let rebuilt: Vec<&str> =
            second.instruments.iter().filter(|i| i.action == BuildAction::Built).map(|i| i.name.as_str()).collect();
        let checked = check(set, CheckOptions { regenerate: true })?;
        println!(
            "{}: {} instruments, built {}, direct {}, failed {}; second build rebuilt {:?}; check clean: {}",
            set.name,
            first.instruments.len(),
            first.instruments.iter().filter(|i| i.action == BuildAction::Built).count(),
            first.instruments.iter().filter(|i| i.action == BuildAction::Direct).count(),
            first.failed().count(),
            rebuilt,
            checked.is_clean()
        );
        for i in &first.instruments {
            for d in &i.diagnostics {
                println!("  {}: {d}", i.name);
            }
            if let Some(e) = &i.error {
                println!("  {}: FAILED: {e}", i.name);
            }
        }
        reports.push(json!({ "set": set.name, "output": set.output, "first": first, "second": second, "check": checked }));
    }
    let u = untracked(&sets)?;
    std::fs::write(report, serde_json::to_string_pretty(&json!({ "sets": reports, "untracked": u }))?)?;
    if failures > 0 {
        return Err(format!("{failures} instrument(s) failed").into());
    }
    Ok(())
}

fn run_untracked(root: &str, sets_dir: &str) -> Res {
    let sets = SetFile::load_dir(Path::new(sets_dir), Path::new(root))?;
    for u in untracked(&sets)? {
        println!("{}\t{}", u.set, u.path);
    }
    Ok(())
}

fn run_specs(root: &str, sets_dir: &str, out: &str) -> Res {
    let sets = SetFile::load_dir(Path::new(sets_dir), Path::new(root))?;
    let mut all = serde_json::Map::new();
    for set in &sets {
        let mut m = serde_json::Map::new();
        for (name, spec) in &set.instruments {
            m.insert(name.clone(), serde_json::to_value(spec)?);
        }
        all.insert(set.name.clone(), Value::Object(m));
    }
    std::fs::write(PathBuf::from(out), serde_json::to_string_pretty(&Value::Object(all))?)?;
    Ok(())
}
