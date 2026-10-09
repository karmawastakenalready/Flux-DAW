//! Command-line front end: `flux sfz build|check|untracked|info|ranges|check-paths|samples|tighten|retune|even`.
//!
//! [`sfz_main`] is what the `flux` binary calls; [`run`] is the same with any output and an exit code instead
//! of printing errors, for tests and embedding.
//!
//! Exit codes: 0 ok, 1 the command ran and found problems (stale or broken wrappers, untracked files, missing
//! samples, errors in a file), 2 bad arguments or the command could not run.

use crate::audit::{PathReport, check_paths};
use crate::derive::{self, Outcome, Report, Request, Tool};
use crate::edit::SamplePaths;
use crate::error::{Error, Result};
use crate::even::EvenOptions;
use crate::fsutil::{lexical_normalize, relative_path, slash_path};
use crate::info::{count, describe, megabytes, spans_text};
use crate::instrument::Instrument;
use crate::manifest::{self, BuildAction, BuildOptions, CheckOptions, Reason, SetCheck, Status};
use crate::ranges::{Ranges, ranges};
use crate::retune::RetuneOptions;
use crate::samples::{SampleSummary, folder_samples, instrument_samples};
use crate::set::SetFile;
use crate::tighten::TightenOptions;
use crate::value::{note_name, parse_note_name};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Help text for `flux sfz`.
pub const HELP: &str = "\
flux sfz <command> [options]

Instrument sets (sfz/sets/<set>.toml; wrappers go to build/sfz/<set>/):
  check     [SET...] [--regenerate]   which wrappers are up to date, stale or broken (writes nothing;
                                      --regenerate also generates each one and compares it with the file)
  build     [SET...] [--only NAME]... [--force] [--dry-run]
                                      generate the stale wrappers and update <set>.manifest.json
                                      (--force: all of them, replacing hand edits)
  untracked                           files in the sets' output folders that no set makes

Reading instruments (nothing is written):
  info        FILE...                 what it plays: keys, keyswitches, velocity layers, controllers, samples
  ranges      FILE...                 the mapping in detail
  check-paths FILE...                 missing samples, case mismatches, includes not read, absolute paths
  samples     PATH... | --instrument FILE
                                      sample files: formats, lengths, loops (a folder's WAV/FLAC files, or
                                      the samples an SFZ file plays)

Derived copies (the source is never changed; OUT defaults to sfz/derived/<library>/<name>_<tool>.sfz, or next to
the source when that is a derived copy already):
  tighten SRC [OUT] --release S [--note TEXT]
                                      short release and no release samples (tight palm mutes)
  retune  SRC [OUT] [--keys LO-HI] [--max-cents C] [--add-to-tune] [--base DIR]
                                      measure each sample's pitch and correct its tune (--base DIR: SRC is a
                                      fragment whose samples are relative to DIR; the copy keeps its paths)
  even    SRC [OUT] [--release S] [--vels V,V...] [--window S] [--max-change DB] [--no-reset]
                                      bring every key to the median level of its velocity layer (via sfizz)

Options:
  --json          print one JSON document instead of text
  --sets DIR      set definitions (default: <root>/sfz/sets)
  --absolute      derived copies name their samples by absolute path (default: relative to the copy)

Relative paths are relative to the project root. Nothing is ever written inside instruments/.
Exit codes: 0 ok, 1 problems found (stale or broken wrappers, untracked files, missing samples, errors in
a file), 2 bad arguments or the command could not run.
";

/// Entry point for `flux sfz ...`: prints, and maps errors to exit code 2.
pub fn sfz_main(root: &Path, args: &[String]) -> ExitCode {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let code = run(root, args, &mut out);
    let _ = out.flush();
    match code {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Parsed command line: positional arguments, flags and `--key value` options.
#[derive(Debug, Default)]
struct Args {
    pos: Vec<String>,
    flags: Vec<String>,
    opts: BTreeMap<String, Vec<String>>,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

impl Args {
    fn parse(args: &[String], flags: &[&str], valued: &[&str]) -> Result<Self> {
        let mut a = Args::default();
        let mut it = args.iter();
        while let Some(s) = it.next() {
            if let Some(name) = s.strip_prefix("--") {
                let (name, inline) = match name.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (name, None),
                };
                if flags.contains(&name) {
                    if inline.is_some() {
                        return Err(invalid(format!("--{name} takes no value")));
                    }
                    a.flags.push(name.to_string());
                } else if valued.contains(&name) {
                    let v = match inline {
                        Some(v) => v,
                        None => it.next().ok_or_else(|| invalid(format!("--{name} needs a value")))?.clone(),
                    };
                    a.opts.entry(name.to_string()).or_default().push(v);
                } else {
                    return Err(invalid(format!("unknown option --{name}; run `flux sfz help`")));
                }
            } else {
                a.pos.push(s.clone());
            }
        }
        Ok(a)
    }

    fn flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }

    fn one(&self, k: &str) -> Option<&str> {
        self.opts.get(k).and_then(|v| v.last()).map(String::as_str)
    }

    fn number(&self, k: &str) -> Result<Option<f64>> {
        self.one(k)
            .map(|v| v.trim().parse::<f64>().ok().filter(|x| x.is_finite()).ok_or_else(|| invalid(format!("--{k} {v}: not a number"))))
            .transpose()
    }
}

/// What a command prints to, and where paths are shown from.
struct Ctx<'a> {
    root: PathBuf,
    out: &'a mut dyn Write,
    json: bool,
}

impl Ctx<'_> {
    fn line(&mut self, s: &str) {
        let _ = writeln!(self.out, "{s}");
    }

    fn print_json<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        let text = serde_json::to_string_pretty(v).map_err(|e| invalid(format!("serialising JSON: {e}")))?;
        self.line(&text);
        Ok(())
    }

    /// A path for display: relative to the root when inside it.
    fn rel(&self, p: &Path) -> String {
        relative_path(&self.root, p).filter(|r| !r.starts_with("..")).unwrap_or_else(|| slash_path(p))
    }

    /// A path from the command line: relative to the root.
    fn path(&self, p: &str) -> PathBuf {
        lexical_normalize(&self.root.join(p))
    }
}

/// Run `flux sfz <args>`, printing to `out`. Returns the exit code.
pub fn run(root: &Path, args: &[String], out: &mut dyn Write) -> Result<u8> {
    let root = lexical_normalize(&std::path::absolute(root).map_err(|e| Error::Io { path: root.to_path_buf(), source: e })?);
    let Some(cmd) = args.first().map(String::as_str) else {
        let _ = write!(out, "{HELP}");
        return Ok(2);
    };
    if matches!(cmd, "help" | "-h" | "--help") {
        let _ = write!(out, "{HELP}");
        return Ok(0);
    }
    let (flags, valued): (&[&str], &[&str]) = match cmd {
        "check" => (&["regenerate"], &["sets"]),
        "build" => (&["force", "dry-run"], &["sets", "only"]),
        "untracked" => (&[], &["sets"]),
        "info" | "ranges" | "check-paths" => (&[], &[]),
        "samples" => (&[], &["instrument"]),
        "tighten" => (&["absolute"], &["release", "note"]),
        "retune" => (&["absolute", "add-to-tune"], &["keys", "max-cents", "base"]),
        "even" => (&["absolute", "no-reset"], &["release", "vels", "window", "max-change"]),
        other => return Err(invalid(format!("unknown command `{other}`; run `flux sfz help`"))),
    };
    let flags: Vec<&str> = flags.iter().copied().chain(["json"]).collect();
    let a = Args::parse(&args[1..], &flags, valued)?;
    let mut ctx = Ctx { root, out, json: a.flag("json") };
    match cmd {
        "check" => cmd_check(&mut ctx, &a),
        "build" => cmd_build(&mut ctx, &a),
        "untracked" => cmd_untracked(&mut ctx, &a),
        "info" => cmd_info(&mut ctx, &a),
        "ranges" => cmd_ranges(&mut ctx, &a),
        "check-paths" => cmd_check_paths(&mut ctx, &a),
        "samples" => cmd_samples(&mut ctx, &a),
        "tighten" | "retune" | "even" => cmd_derive(&mut ctx, cmd, &a),
        other => Err(invalid(format!("unknown command `{other}`"))),
    }
}

// ---------------------------------------------------------------------------------------------------------
// sets

/// The sets named on the command line (all when none), from `--sets` or `<root>/sfz/sets`.
fn load_sets(ctx: &Ctx<'_>, a: &Args) -> Result<Vec<SetFile>> {
    let dir = a.one("sets").map_or_else(|| ctx.root.join("sfz").join("sets"), |d| ctx.path(d));
    SetFile::load_named(&dir, &ctx.root, &a.pos)
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::UpToDate => "up to date",
        Status::Direct => "direct",
        Status::Stale => "stale",
        Status::Broken => "broken",
    }
}

fn reason_text(r: &Reason) -> String {
    match r {
        Reason::NotBuilt => "not built".into(),
        Reason::RecipeChanged => "definition changed".into(),
        Reason::InputChanged(p) => format!("changed: {p}"),
        Reason::InputMissing(p) => format!("missing: {p}"),
        Reason::OutputMissing(p) => format!("output missing: {p}"),
        Reason::OutputModified(p) => format!("edited by hand: {p}"),
        Reason::OutputDiffers(p) => format!("differs from a fresh build: {p}"),
    }
}

/// How many reasons, diagnostics or paths are listed before "and N more".
const LIST_LIMIT: usize = 3;

fn listed(items: &[String], limit: usize) -> String {
    let mut s = items.iter().take(limit).cloned().collect::<Vec<_>>().join("; ");
    if items.len() > limit {
        s.push_str(&format!("; and {} more", items.len() - limit));
    }
    s
}

/// Columns separated by two spaces; every column but the last padded to its widest cell.
fn table(rows: &[Vec<String>]) -> Vec<String> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for (c, cell) in r.iter().enumerate() {
                line.push_str(cell);
                if c + 1 < r.len() {
                    line.extend(std::iter::repeat_n(' ', widths[c] - cell.chars().count() + 2));
                }
            }
            line
        })
        .collect()
}

fn check_summary(c: &SetCheck) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for i in &c.instruments {
        *counts.entry(status_name(i.status)).or_default() += 1;
    }
    let parts: Vec<String> = ["up to date", "direct", "stale", "broken"]
        .iter()
        .filter_map(|k| counts.get(k).map(|n| format!("{n} {k}")))
        .collect();
    format!("{}: {}: {}", c.set, count(c.instruments.len(), "instrument"), parts.join(", "))
}

fn cmd_check(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    let sets = load_sets(ctx, a)?;
    let options = CheckOptions { regenerate: a.flag("regenerate") };
    let checks = sets.iter().map(|s| manifest::check(s, options)).collect::<Result<Vec<_>>>()?;
    let bad = checks.iter().any(|c| !c.is_clean());
    if ctx.json {
        ctx.print_json(&checks)?;
        return Ok(u8::from(bad));
    }
    for c in &checks {
        let mut head = check_summary(c);
        if !c.manifest_found {
            head.push_str(&format!(" (no {}: never built by `flux sfz build`)", c.manifest));
        }
        ctx.line(&head);
        let mut rows = vec![];
        let mut not_built = vec![];
        for i in &c.instruments {
            if matches!(i.status, Status::UpToDate | Status::Direct) {
                continue;
            }
            if i.reasons == [Reason::NotBuilt] {
                not_built.push(i.name.as_str());
                continue;
            }
            let reasons: Vec<String> = i.reasons.iter().filter(|r| **r != Reason::NotBuilt).map(reason_text).collect();
            rows.push(vec![format!("  {}", i.name), status_name(i.status).into(), listed(&reasons, LIST_LIMIT)]);
        }
        for l in table(&rows) {
            ctx.line(&l);
        }
        if !not_built.is_empty() {
            let mut names = not_built.iter().take(10).copied().collect::<Vec<_>>().join(", ");
            if not_built.len() > 10 {
                names.push_str(&format!(" and {} more", not_built.len() - 10));
            }
            ctx.line(&format!("  not built yet: {names}"));
        }
        if !c.removed.is_empty() {
            ctx.line(&format!("  removed from the set since the last build: {}", c.removed.join(", ")));
        }
    }
    let stale = checks.iter().filter(|c| !c.is_clean()).count();
    ctx.line(&match stale {
        0 => "every set is up to date".to_string(),
        n => format!("{n} of {} need `flux sfz build`", count(checks.len(), "set")),
    });
    Ok(u8::from(bad))
}

fn cmd_build(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    let sets = load_sets(ctx, a)?;
    let only: Vec<String> =
        a.opts.get("only").into_iter().flatten().flat_map(|v| v.split(',')).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let reports = manifest::build_sets(&sets, &BuildOptions { force: a.flag("force"), dry_run: a.flag("dry-run"), only })?;
    let failed = reports.iter().any(|r| r.failed().next().is_some());
    if ctx.json {
        ctx.print_json(&reports)?;
        return Ok(u8::from(failed));
    }
    for r in &reports {
        let n = |act: BuildAction| r.instruments.iter().filter(|i| i.action == act).count();
        let built = if r.dry_run { "would build" } else { "built" };
        ctx.line(&format!(
            "{}: {built} {}, up to date {}, direct {}, failed {}{}",
            r.set,
            n(BuildAction::Built),
            n(BuildAction::UpToDate),
            n(BuildAction::Direct),
            n(BuildAction::Failed),
            if n(BuildAction::Skipped) > 0 { format!(", skipped {}", n(BuildAction::Skipped)) } else { String::new() }
        ));
        let mut rows = vec![];
        for i in &r.instruments {
            match i.action {
                BuildAction::Built => {
                    let why: Vec<String> = i
                        .reasons
                        .iter()
                        .map(|r| if *r == Reason::NotBuilt { "first build".to_string() } else { reason_text(r) })
                        .collect();
                    rows.push(vec![format!("  {}", i.name), built.into(), listed(&why, LIST_LIMIT)]);
                }
                BuildAction::Failed => rows.push(vec![format!("  {}", i.name), "FAILED".into(), i.error.clone().unwrap_or_default()]),
                _ => {}
            }
            if !i.diagnostics.is_empty() {
                let d: Vec<String> = i.diagnostics.iter().map(|d| d.to_string()).collect();
                rows.push(vec![format!("  {}", i.name), count(d.len(), "problem"), listed(&d, 2)]);
            }
        }
        for l in table(&rows) {
            ctx.line(&l);
        }
        for f in &r.deleted {
            ctx.line(&format!("  {} {f}", if r.dry_run { "would delete" } else { "deleted" }));
        }
        for f in &r.kept_modified {
            ctx.line(&format!("  kept (edited by hand, no longer generated): {f}"));
        }
    }
    Ok(u8::from(failed))
}

fn cmd_untracked(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    if !a.pos.is_empty() {
        return Err(invalid("untracked takes no set names: it looks at the output folders of every set"));
    }
    let sets = load_sets(ctx, a)?;
    let found = manifest::untracked(&sets)?;
    if ctx.json {
        ctx.print_json(&found)?;
    } else {
        let rows: Vec<Vec<String>> = found.iter().map(|u| vec![u.set.clone(), u.path.clone()]).collect();
        for l in table(&rows) {
            ctx.line(&l);
        }
        ctx.line(&format!("{} in the output folders of {}", count(found.len(), "untracked file or folder"), count(sets.len(), "set")));
    }
    Ok(u8::from(!found.is_empty()))
}

// ---------------------------------------------------------------------------------------------------------
// reading instruments

fn files_arg<'a>(a: &'a Args, what: &str) -> Result<&'a [String]> {
    if a.pos.is_empty() {
        return Err(invalid(format!("{what} needs at least one SFZ file")));
    }
    Ok(&a.pos)
}

fn cmd_info(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    let mut infos = vec![];
    for f in files_arg(a, "info")? {
        infos.push(describe(&Instrument::load(ctx.path(f))?));
    }
    let bad = infos.iter().any(|i| i.errors > 0);
    if ctx.json {
        ctx.print_json(&infos)?;
    } else {
        for (n, i) in infos.iter().enumerate() {
            if n > 0 {
                ctx.line("");
            }
            let text = i.to_text();
            ctx.line(text.trim_end());
        }
    }
    Ok(u8::from(bad))
}

fn ranges_text(name: &str, r: &Ranges) -> Vec<String> {
    let mut l = vec![];
    let mut head = format!("{name}: {}", count(r.regions, "region"));
    if r.playable_regions != r.regions {
        head.push_str(&format!(" ({} playable)", r.playable_regions));
    }
    l.push(head);
    l.push(format!("  keys: {}", spans_text(&r.keys)));
    if let (Some(lo), Some(hi)) = (r.keycenters.first(), r.keycenters.last()) {
        l.push(format!("  key centres: {} from {} to {}", r.keycenters.len(), note_name(*lo), note_name(*hi)));
    }
    if r.keyswitch_range.is_some() || !r.keyswitches.is_empty() {
        let mut s = "  keyswitches".to_string();
        if let Some(range) = r.keyswitch_range {
            s.push_str(&format!(" {}-{}", range.0, range.1));
        }
        if let Some(d) = r.keyswitch_default {
            s.push_str(&format!(", default {d}"));
        }
        l.push(s + ":");
        for k in &r.keyswitches {
            let key = match k.hi {
                Some(hi) if hi != k.key => format!("{}-{hi}", k.key),
                _ => k.key.to_string(),
            };
            l.push(format!(
                "    {key} {}: {}, {}, {}",
                k.label.as_deref().unwrap_or("(unnamed)"),
                spans_text(&k.keys),
                count(k.velocity_layers, "velocity layer"),
                count(k.regions, "region")
            ));
        }
    }
    if !r.held_keyswitches.is_empty() {
        let keys: Vec<String> = r.held_keyswitches.iter().map(u8::to_string).collect();
        l.push(format!("  held keyswitches (sw_down/sw_up): {}", keys.join(", ")));
    }
    let splits: Vec<String> = r.velocity_splits.iter().map(|s| format!("{}-{}", s.0, s.1)).collect();
    l.push(format!(
        "  velocity: {}; ranges {}{}",
        count(r.velocity_layers.max(1), "layer"),
        if splits.is_empty() { "none".to_string() } else { splits.join(", ") },
        if r.velocity_crossfades { "; crossfaded" } else { "" }
    ));
    l.push(format!(
        "  round robin: sequence {}, random {}; release regions: {}; controller-triggered regions: {}",
        r.round_robin.sequence, r.round_robin.random, r.release_regions, r.cc_triggered_regions
    ));
    for c in &r.ccs {
        let mut name = format!("CC{}", c.cc);
        if let Some(s) = c.standard {
            name.push_str(&format!(" {s}"));
        }
        if let Some(label) = &c.label {
            name.push_str(&format!(" \"{label}\""));
        }
        if let Some(d) = c.default {
            name.push_str(&format!(" (default {d})"));
        }
        l.push(format!("  {name}: {}", c.uses.join(", ")));
    }
    l
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| slash_path(p), |n| n.to_string_lossy().into_owned())
}

fn cmd_ranges(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    #[derive(Serialize)]
    struct Out {
        file: String,
        ranges: Ranges,
    }
    let mut all = vec![];
    for f in files_arg(a, "ranges")? {
        let path = ctx.path(f);
        let inst = Instrument::load(&path)?;
        all.push(Out { file: ctx.rel(&path), ranges: ranges(&inst) });
    }
    if ctx.json {
        ctx.print_json(&all)?;
    } else {
        for o in &all {
            for l in ranges_text(&file_name(Path::new(&o.file)), &o.ranges) {
                ctx.line(&l);
            }
        }
    }
    Ok(0)
}

fn paths_text(ctx: &Ctx<'_>, r: &PathReport) -> Vec<String> {
    let mut problems = vec![];
    if !r.missing.is_empty() {
        problems.push(count(r.missing.len(), "missing sample"));
    }
    if !r.case_mismatches.is_empty() {
        let n = r.case_mismatches.len();
        problems.push(if n == 1 { "1 case mismatch".to_string() } else { format!("{n} case mismatches") });
    }
    if !r.includes_not_read.is_empty() {
        problems.push(format!("{} not read", count(r.includes_not_read.len(), "include")));
    }
    let mut head = format!("{}: {}, {}: ", ctx.rel(&r.file), count(r.regions, "region"), count(r.samples, "sample"));
    head.push_str(&if problems.is_empty() { "ok".to_string() } else { problems.join(", ") });
    if r.absolute_samples > 0 {
        head.push_str(&format!("; {} named by absolute path (works only on this machine)", count(r.absolute_samples, "sample")));
    }
    let mut l = vec![head];
    let more = |n: usize, l: &mut Vec<String>| {
        if n > 10 {
            l.push(format!("  ... and {} more", n - 10));
        }
    };
    for m in r.missing.iter().take(10) {
        l.push(format!("  missing  {} ({}, first at {})", m.sample, count(m.regions, "region"), m.first_region));
    }
    more(r.missing.len(), &mut l);
    for m in r.case_mismatches.iter().take(10) {
        let on_disk = m.on_disk.as_deref().map(file_name).unwrap_or_default();
        l.push(format!("  case     {} (on disk: {on_disk})", m.sample));
    }
    more(r.case_mismatches.len(), &mut l);
    for i in r.includes_not_read.iter().take(10) {
        l.push(format!("  include  {} at {} ({})", i.path, i.location, i.reason.replace('_', " ")));
    }
    more(r.includes_not_read.len(), &mut l);
    l
}

fn cmd_check_paths(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    let mut reports = vec![];
    for f in files_arg(a, "check-paths")? {
        reports.push(check_paths(&Instrument::load(ctx.path(f))?));
    }
    let bad = reports.iter().any(|r| !r.is_ok());
    if ctx.json {
        ctx.print_json(&reports)?;
    } else {
        for r in &reports {
            for l in paths_text(ctx, r) {
                ctx.line(&l);
            }
        }
    }
    Ok(u8::from(bad))
}

fn samples_text(name: &str, s: &SampleSummary) -> Vec<String> {
    let mut head = format!("{name}: {}", count(s.files, "file"));
    if s.missing + s.unreadable > 0 {
        head.push_str(&format!(" ({} missing, {} unreadable)", s.missing, s.unreadable));
    }
    head.push_str(&format!(", {}", megabytes(s.total_bytes)));
    if let Some(d) = s.duration_s {
        head.push_str(&format!(", {:.1}-{:.1} s (median {:.1} s, {:.0} s in all)", d.min, d.max, d.median, d.total));
    }
    let mut l = vec![head];
    for f in &s.formats {
        let ch = match f.channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n} channels"),
        };
        let format = match f.format {
            crate::audio::AudioFormat::Wav => "WAV",
            crate::audio::AudioFormat::Flac => "FLAC",
        };
        l.push(format!("  {}: {} Hz {} {ch} {format}", count(f.files, "file"), f.sample_rate, f.encoding));
    }
    l.push(format!("  with loop points: {}; with a root key: {}", s.with_loops, s.with_root_key));
    for p in &s.problems {
        l.push(format!("  {p}"));
    }
    l
}

fn cmd_samples(ctx: &mut Ctx<'_>, a: &Args) -> Result<u8> {
    #[derive(Serialize)]
    struct Out {
        path: String,
        summary: SampleSummary,
    }
    let mut targets: Vec<PathBuf> = a.pos.iter().map(|p| ctx.path(p)).collect();
    targets.extend(a.opts.get("instrument").into_iter().flatten().map(|p| ctx.path(p)));
    if targets.is_empty() {
        return Err(invalid("samples needs a folder or an SFZ file"));
    }
    let mut all = vec![];
    for t in &targets {
        let summary = if t.is_dir() { folder_samples(t)? } else { instrument_samples(&Instrument::load(t)?) };
        all.push(Out { path: ctx.rel(t), summary });
    }
    let bad = all.iter().any(|o| o.summary.missing + o.summary.unreadable > 0);
    if ctx.json {
        ctx.print_json(&all)?;
    } else {
        for o in &all {
            for l in samples_text(&o.path, &o.summary) {
                ctx.line(&l);
            }
        }
    }
    Ok(u8::from(bad))
}

// ---------------------------------------------------------------------------------------------------------
// derived copies

/// A key from the command line: a MIDI number or a note name (C4 = 60).
fn key_arg(s: &str) -> Option<u8> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse::<u8>().ok().filter(|k| *k <= 127)
    } else {
        parse_note_name(s)
    }
}

/// `LO-HI` (`36-84`, `C2-C6`, `C-1-G9`).
fn key_range(s: &str) -> Result<(u8, u8)> {
    for (i, _) in s.match_indices('-').filter(|(i, _)| *i > 0) {
        if let (Some(lo), Some(hi)) = (key_arg(&s[..i]), key_arg(&s[i + 1..])) {
            if lo > hi {
                return Err(invalid(format!("--keys {s}: the lowest key comes first")));
            }
            return Ok((lo, hi));
        }
    }
    Err(invalid(format!("--keys {s}: expected LO-HI with MIDI numbers or note names, e.g. 36-84 or C2-C6")))
}

fn cmd_derive(ctx: &mut Ctx<'_>, cmd: &str, a: &Args) -> Result<u8> {
    let (src, out) = match a.pos.as_slice() {
        [src] => (src, None),
        [src, out] => (src, Some(ctx.path(out))),
        _ => return Err(invalid(format!("{cmd} takes SRC and optionally OUT"))),
    };
    let sample_paths = if a.flag("absolute") { SamplePaths::Absolute } else { SamplePaths::Relative };
    let tool = match cmd {
        "tighten" => {
            let release_s = a.number("release")?.ok_or_else(|| invalid("tighten needs --release SECONDS"))?;
            Tool::Tighten(TightenOptions { release_s, note: a.one("note").map(str::to_string), sample_paths })
        }
        "retune" => {
            let mut o = RetuneOptions { keep_existing_tune: a.flag("add-to-tune"), sample_paths, ..RetuneOptions::default() };
            o.keys = a.one("keys").map(key_range).transpose()?;
            if let Some(c) = a.number("max-cents")? {
                o.max_cents = c;
            }
            Tool::Retune(o)
        }
        _ => {
            let mut o = EvenOptions { release_s: a.number("release")?, reset_between_notes: !a.flag("no-reset"), sample_paths, ..EvenOptions::default() };
            if let Some(v) = a.one("vels") {
                o.velocities = v
                    .split(',')
                    .map(|x| x.trim().parse::<u8>().ok().filter(|v| (1..=127).contains(v)))
                    .collect::<Option<Vec<u8>>>()
                    .ok_or_else(|| invalid(format!("--vels {v}: expected velocities 1-127 separated by commas")))?;
            }
            if let Some(w) = a.number("window")? {
                o.window_s = w;
            }
            if let Some(m) = a.number("max-change")? {
                o.max_change_db = m;
            }
            Tool::Even(o)
        }
    };
    let request = Request { tool, source: ctx.path(src), out, base: a.one("base").map(|b| ctx.path(b)) };
    let outcome = derive::run(&ctx.root, &request)?;
    if ctx.json {
        ctx.print_json(&outcome)?;
        return Ok(0);
    }
    for l in derive_text(ctx, &outcome) {
        ctx.line(&l);
    }
    Ok(0)
}

fn derive_text(ctx: &Ctx<'_>, o: &Outcome) -> Vec<String> {
    let mut l: Vec<String> = o.written.iter().map(|p| format!("wrote {}", ctx.rel(p))).collect();
    l.push(match &o.report {
        Report::Tighten(r) => format!(
            "release {} s: {} replaced, {} removed, {} kept",
            r.release_s,
            count(r.release_opcodes_changed, "ampeg_release value"),
            count(r.release_regions_removed, "release region"),
            count(r.regions_kept, "region")
        ),
        Report::Retune(r) => {
            let why: Vec<String> = r.left_as_is_by_reason().iter().map(|(k, n)| format!("{n} {}", k.replace('_', " "))).collect();
            let why = if why.is_empty() { String::new() } else { format!(" ({})", why.join(", ")) };
            format!(
                "{} retuned, {} left as they were{why}, {} removed; {} measured",
                r.retuned,
                r.left_as_is,
                r.removed,
                count(r.keys.len(), "key")
            )
        }
        Report::Even(r) => {
            let targets: Vec<String> = r.targets_db.iter().zip(&r.velocities).map(|(t, v)| format!("{t:.1} dB at velocity {v}")).collect();
            format!(
                "{} evened over {} (targets {}), {} left alone",
                count(r.regions.len(), "region"),
                count(r.keys.len(), "key"),
                targets.join(", "),
                r.skipped
            )
        }
    });
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(root: &Path, args: &[&str]) -> (u8, String) {
        let mut out = vec![];
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let code = run(root, &args, &mut out).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn key_ranges() {
        assert_eq!(key_range("36-84").unwrap(), (36, 84));
        assert_eq!(key_range("C2-C6").unwrap(), (36, 84));
        assert_eq!(key_range("C-1-G9").unwrap(), (0, 127));
        assert!(key_range("84-36").is_err());
        assert!(key_range("x").is_err());
    }

    #[test]
    fn help_and_bad_arguments() {
        let root = std::env::temp_dir();
        let (code, text) = run_args(&root, &["help"]);
        assert_eq!(code, 0);
        assert!(text.contains("flux sfz <command>"));
        let mut out = vec![];
        assert!(run(&root, &["info".into(), "--bogus".into()], &mut out).is_err());
        assert!(run(&root, &["frobnicate".into()], &mut out).is_err());
    }

    #[test]
    fn sets_and_tools() {
        let root = std::env::temp_dir().join(format!("flux-sfz-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["instruments/lib", "sfz/sets"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("instruments/lib/a.wav"), b"").unwrap();
        std::fs::write(root.join("instruments/lib/p.sfz"), "<region> sample=a.wav key=60\n<region> sample=gone.wav key=62\n").unwrap();
        std::fs::write(
            root.join("sfz/sets/demo.toml"),
            "[instruments.p]\nsource = \"instruments/lib/p.sfz\"\nwrap = \"expr\"\n\n[instruments.d]\nsource = \"instruments/lib/p.sfz\"\nwrap = \"none\"\n",
        )
        .unwrap();

        let (code, text) = run_args(&root, &["check"]);
        assert_eq!(code, 1, "{text}");
        assert!(text.contains("demo: 2 instruments: 1 direct, 1 stale"), "{text}");
        let (code, text) = run_args(&root, &["build", "demo"]);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("demo: built 1"), "{text}");
        let (code, text) = run_args(&root, &["check", "--json"]);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("\"up_to_date\""), "{text}");
        let (code, _) = run_args(&root, &["untracked"]);
        assert_eq!(code, 0);
        std::fs::write(root.join("build/sfz/demo/stray.sfz"), "").unwrap();
        let (code, text) = run_args(&root, &["untracked"]);
        assert_eq!(code, 1);
        assert!(text.contains("build/sfz/demo/stray.sfz"), "{text}");

        // a missing sample is a problem for info and check-paths, not for ranges
        let (code, text) = run_args(&root, &["info", "instruments/lib/p.sfz"]);
        assert_eq!(code, 1, "{text}");
        assert!(text.starts_with("p.sfz: 2 regions (1 playable)"), "{text}");
        let (code, text) = run_args(&root, &["check-paths", "instruments/lib/p.sfz"]);
        assert_eq!(code, 1);
        assert!(text.contains("1 missing sample") && text.contains("missing  gone.wav"), "{text}");
        let (code, text) = run_args(&root, &["ranges", "instruments/lib/p.sfz"]);
        assert_eq!(code, 0);
        assert!(text.contains("keys: C4 (60)"), "{text}");

        let (code, text) = run_args(&root, &["tighten", "instruments/lib/p.sfz", "--release", "0.1"]);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("wrote sfz/derived/lib/p_tighten.sfz"), "{text}");
        let mut out = vec![];
        let into_library: Vec<String> =
            ["tighten", "instruments/lib/p.sfz", "instruments/lib/q.sfz", "--release", "0.1"].iter().map(|s| s.to_string()).collect();
        assert!(run(&root, &into_library, &mut out).is_err());
        assert!(!root.join("instruments/lib/q.sfz").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
