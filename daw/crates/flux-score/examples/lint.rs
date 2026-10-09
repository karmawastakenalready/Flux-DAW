//! Lints a Flux project file from the command line and prints the report.
//!
//! ```text
//! cargo run -p flux-score --example lint -- <project.flux.json> [--catalog DIR] [--checks clash,nct,range,mud]
//!     [--melody "Flute,Oboe"] [--transpose "Cello=12,Bass=-12"] [--from N] [--to N] [--max 40] [--json]
//! ```
//!
//! The text layout is tools/score_lint.py's, so the two can be diffed.

use flux_core::{Project, TrackRef};
use flux_score::lint::{Check, InstrumentCatalog, LintOptions, lint};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: lint <project.flux.json> [--catalog DIR] [--checks a,b] [--melody A,B] [--transpose Name=N,...] [--from N] [--to N] [--max N] [--json]";

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut project_path = None;
    let mut opts = LintOptions::default();
    let mut max = 40;
    let mut json = false;
    while let Some(a) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{what} needs a value\n{USAGE}"));
        match a.as_str() {
            "--catalog" => {
                let dir = value("--catalog")?;
                let (cat, skipped) = InstrumentCatalog::load_dir(Path::new(&dir)).map_err(|e| format!("--catalog {dir}: {e}"))?;
                for s in skipped {
                    eprintln!("catalog: skipped {s}");
                }
                opts.catalog = Some(cat);
            }
            "--checks" => {
                let list = value("--checks")?;
                let checks = list.split(',').map(|c| Check::parse(c).ok_or_else(|| format!("unknown check {c:?}"))).collect::<Result<Vec<_>, _>>()?;
                opts.checks = Some(checks);
            }
            "--melody" => opts.melody = Some(value("--melody")?.split(',').map(|m| TrackRef::Name(m.trim().to_string())).collect()),
            "--transpose" => {
                for pair in value("--transpose")?.split(',') {
                    let (name, n) = pair.split_once('=').ok_or_else(|| format!("--transpose {pair:?}: write Name=semitones"))?;
                    let n: i32 = n.trim().parse().map_err(|_| format!("--transpose {pair:?}: {n:?} is not a whole number"))?;
                    opts.transpose.push((TrackRef::Name(name.trim().to_string()), n));
                }
            }
            "--from" => opts.from_bar = Some(value("--from")?.parse().map_err(|_| "--from needs a bar number".to_string())?),
            "--to" => opts.to_bar = Some(value("--to")?.parse().map_err(|_| "--to needs a bar number".to_string())?),
            "--max" => max = value("--max")?.parse().map_err(|_| "--max needs a number".to_string())?,
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            p if !p.starts_with('-') && project_path.is_none() => project_path = Some(p.to_string()),
            other => return Err(format!("unexpected argument {other:?}\n{USAGE}")),
        }
    }
    let path = project_path.ok_or_else(|| USAGE.to_string())?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
    let mut project: Project = serde_json::from_str(&text).map_err(|e| format!("parsing {path}: {e}"))?;
    // score_lint.py names the project by its file
    if let Some(file) = Path::new(&path).file_name() {
        project.name = file.to_string_lossy().into_owned();
    }
    let report = lint(&project, &opts);
    if json {
        let out = serde_json::to_string_pretty(&report.to_json(max)).map_err(|e| e.to_string())?;
        println!("{out}");
    } else {
        print!("{}", report.to_text(max));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
