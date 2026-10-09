//! `flux-card`: instrument report cards from the command line (the Rust port of `tools/instrument_card.py`).
//!
//! ```text
//! flux-card SFZ [SFZ ...] [--out DIR] [--name NAME] [--keys LO-HI] [--ks KEY] [-j N]
//! flux-card --used [--used-extra FILE] [--out DIR]   # every SFZ the songs use + the VCSL candidates
//! flux-card --index [--out DIR]    # only rebuild DIR/README.md from the JSON cards
//! ```
//!
//! Options: `--schema reference|extended` (default extended), `--in-process` (no child process per card),
//! `--key-workers N` (sfizz instances per card), `--measured YYYY-MM-DD`, `--root DIR` (project root),
//! `--timeout SECONDS` (per card, default 900).

use anyhow::{Context, Result, bail};
use flux_analysis::card::catalog::{self, Isolation, RunOptions};
use flux_analysis::card::{CardOptions, Schema};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("card-worker") {
        std::process::exit(catalog::worker_main(&args[1..]));
    }
    if let Err(e) = run(&args) {
        eprintln!("flux-card: {e:#}");
        std::process::exit(2);
    }
}

fn run(args: &[String]) -> Result<()> {
    let mut copts = CardOptions::default();
    let (mut sfz, mut out, mut name, mut used_extra) = (vec![], None, None, None);
    let (mut used, mut index, mut in_process, mut jobs, mut timeout) = (false, false, false, 0usize, 900u64);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--out" => out = Some(PathBuf::from(val()?)),
            "--name" => name = Some(val()?),
            "--keys" => copts.keys = Some(val()?),
            "--ks" => copts.ks = Some(val()?.parse().context("--ks takes a MIDI key number")?),
            "--used" => used = true,
            "--used-extra" => used_extra = Some(PathBuf::from(val()?)),
            "--index" => index = true,
            "--in-process" => in_process = true,
            "-j" | "--jobs" => jobs = val()?.parse().context("--jobs")?,
            "--key-workers" => copts.key_workers = val()?.parse().context("--key-workers")?,
            "--measured" => copts.measured = Some(val()?),
            "--root" => copts.root = PathBuf::from(val()?),
            "--timeout" => timeout = val()?.parse().context("--timeout")?,
            "--schema" => {
                copts.schema = match val()?.as_str() {
                    "reference" => Schema::Reference,
                    "extended" => Schema::Extended,
                    s => bail!("unknown schema {s:?} (reference or extended)"),
                }
            }
            "-h" | "--help" => {
                println!("{}", include_str!("flux-card.txt"));
                return Ok(());
            }
            s if s.starts_with('-') => bail!("unknown option {s} (see --help)"),
            s => sfz.push(PathBuf::from(s)),
        }
    }
    let out = std::path::absolute(out.unwrap_or_else(|| copts.root.join("catalog")))?;
    std::fs::create_dir_all(&out)?;
    if index {
        catalog::write_index(&out, copts.schema)?;
        return Ok(());
    }
    copts.key_range()?;
    let job_list = if used {
        let extra = match &used_extra {
            Some(f) => catalog::read_used_extra(&copts.root, f)?,
            None => vec![],
        };
        let entries = catalog::gather_used(&copts.root, &extra);
        catalog::dedupe(&copts.root, &entries, &copts)
    } else {
        if sfz.is_empty() {
            bail!("give SFZ files or --used");
        }
        catalog::jobs_for(&sfz, name.as_deref(), &copts, &out)?
    };
    let isolation = if in_process {
        Isolation::InProcess
    } else {
        Isolation::Subprocess { exe: std::env::current_exe()?, args: vec!["card-worker".into()] }
    };
    let mut ropts = RunOptions::new(copts, isolation);
    ropts.jobs = jobs;
    ropts.timeout = Duration::from_secs(timeout);
    println!("carding {} instruments", job_list.len());
    let t0 = Instant::now();
    catalog::card_many(&job_list, &out, &ropts, &|c| println!("{}", catalog::summary(c)))?;
    println!("done in {:.1} s -> {}", t0.elapsed().as_secs_f64(), out.join("README.md").display());
    Ok(())
}
