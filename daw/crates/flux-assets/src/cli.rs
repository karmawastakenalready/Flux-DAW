//! Command-line front end: `flux assets status|fetch|verify|lock|repair|licenses` and `flux flac`.
//!
//! [`assets_main`] and [`flac_main`] are what the `flux` binary calls; [`run_assets`] is the same with an
//! injectable [`Ui`] and an exit code instead of printing errors, for tests and embedding.
//!
//! Exit codes: 0 success, 1 the command ran but found problems (drift, refused or failed fetches, license
//! blockers), 2 bad arguments or the command could not run.

use crate::audit::{self, Finding, Severity};
use crate::credits;
use crate::fetch::{self, FetchOptions};
use crate::flac::{self, BitDepth, Dither, FlacOptions};
use crate::fsutil;
use crate::license::Scope;
use crate::lock::spec_digest;
use crate::manifest::Asset;
use crate::usage::{self, Usage};
use crate::verify::{self, IssueKind, State, VerifyOptions};
use crate::workspace::{ConsoleUi, OpenOptions, Ui, Workspace};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Help text for `flux assets`.
pub const ASSETS_HELP: &str = "\
flux assets <command> [options]

Commands:
  status   [ID...] [--no-usage] what is installed, locked, used, and each license's verdict (no hashing)
  verify   [ID...] [--quick]    re-hash installs and compare them with their sources and the lock
  fetch    ID... | --all        install from the official sources pinned in assets.toml
           [--yes] [--dry-run [--check-urls]] [--force] [--max-size SIZE]
  lock     [ID...]              check existing installs against their sources and record them in assets.lock
  repair   ID [--yes]           restore drifted files (git line endings, edits, deletions; pinned files)
  licenses [--out FILE] [--credits FILE] [--check FILE] [--texts DIR] [--print] [--no-usage]
                                license audit; writes the LICENSES.md overview / in-game credits text

Options for every command:
  --manifest FILE   manifest (default: <root>/assets.toml)
  --lock FILE       lock file (default: assets.lock next to the manifest)
  --json            print one JSON document instead of text

--no-usage skips scanning the project files in policy.usage_roots (the USED BY column, usage in LICENSES.md).
Exit codes: 0 ok, 1 problems found (drift, failed or refused fetches, license blockers), 2 bad arguments.
";

/// Help text for `flux flac`.
pub const FLAC_HELP: &str = "\
flux flac IN.wav OUT.flac [--bits 16|24] [--no-dither] [--seed N] [--title T] [--artist A]
                           [--tag NAME=VALUE]... [--loop START LENGTH] [--threads N] [--no-verify] [--json]

Integer WAVs are copied losslessly at their own depth; float WAVs are quantised (TPDF dither, seed 1, like
wav16 export). The result is decoded again and checked against the input's MD5 unless --no-verify.
";

/// Entry point for `flux assets ...`: prints, and maps errors to exit code 2.
pub fn assets_main(root: &Path, args: &[String]) -> ExitCode {
    let mut ui = ConsoleUi::default();
    match run_assets(root, args, &mut ui) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            ui.progress_end();
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Entry point for `flux flac ...`.
pub fn flac_main(args: &[String]) -> ExitCode {
    match run_flac(args, &mut ConsoleUi::default()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
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

impl Args {
    fn parse(args: &[String], flags: &[&str], valued: &[&str], two_valued: &[&str]) -> Result<Self> {
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
                        bail!("--{name} takes no value");
                    }
                    a.flags.push(name.to_string());
                } else if valued.contains(&name) {
                    let v = match inline {
                        Some(v) => v,
                        None => it.next().with_context(|| format!("--{name} needs a value"))?.clone(),
                    };
                    a.opts.entry(name.to_string()).or_default().push(v);
                } else if two_valued.contains(&name) {
                    let x = it.next().with_context(|| format!("--{name} needs two values"))?.clone();
                    let y = it.next().with_context(|| format!("--{name} needs two values"))?.clone();
                    a.opts.entry(name.to_string()).or_default().extend([x, y]);
                } else {
                    bail!("unknown option --{name}");
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
}

/// Lines go nowhere (JSON mode); progress still shows on a terminal; questions are answered "no".
#[derive(Default)]
struct QuietUi(ConsoleUi);

impl Ui for QuietUi {
    fn line(&mut self, _msg: &str) {}
    fn progress(&mut self, label: &str, done: u64, total: Option<u64>) {
        self.0.progress(label, done, total);
    }
    fn progress_end(&mut self) {
        self.0.progress_end();
    }
    fn confirm(&mut self, _q: &str) -> bool {
        false
    }
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v).context("serialising JSON")?);
    Ok(())
}

/// Run `flux assets <args>`. Returns the exit code.
pub fn run_assets(root: &Path, args: &[String], ui: &mut dyn Ui) -> Result<u8> {
    let (cmd, a) = match parse_command(args, ui)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let open = OpenOptions { manifest: a.one("manifest").map(|p| root.join(p)), lock: a.one("lock").map(|p| root.join(p)) };
    let mut ws = Workspace::open(root, &open)?;
    dispatch(&mut ws, &cmd, &a, ui)
}

/// [`run_assets`] on an already opened workspace (`--manifest` and `--lock` are accepted but ignored).
pub fn run_assets_in(ws: &mut Workspace, args: &[String], ui: &mut dyn Ui) -> Result<u8> {
    match parse_command(args, ui)? {
        Ok((cmd, a)) => dispatch(ws, &cmd, &a, ui),
        Err(code) => Ok(code),
    }
}

/// The command and its arguments, or the exit code after printing help.
fn parse_command(args: &[String], ui: &mut dyn Ui) -> Result<Result<(String, Args), u8>> {
    let Some(cmd) = args.first().map(String::as_str) else {
        ui.line(ASSETS_HELP);
        return Ok(Err(2));
    };
    if matches!(cmd, "help" | "-h" | "--help") {
        ui.line(ASSETS_HELP);
        return Ok(Err(0));
    }
    let (flags, valued): (&[&str], &[&str]) = match cmd {
        "status" => (&["no-usage"], &[]),
        "verify" => (&["quick"], &[]),
        "fetch" => (&["all", "yes", "dry-run", "check-urls", "force"], &["max-size"]),
        "lock" => (&[], &[]),
        "repair" => (&["yes"], &[]),
        "licenses" => (&["print", "no-usage"], &["out", "credits", "check", "texts"]),
        other => bail!("unknown command `{other}`; run `flux assets help`"),
    };
    let flags: Vec<&str> = flags.iter().copied().chain(["json"]).collect();
    let valued: Vec<&str> = valued.iter().copied().chain(["manifest", "lock"]).collect();
    let a = Args::parse(&args[1..], &flags, &valued, &[])?;
    Ok(Ok((cmd.to_string(), a)))
}

fn dispatch(ws: &mut Workspace, cmd: &str, a: &Args, ui: &mut dyn Ui) -> Result<u8> {
    let json = a.flag("json");
    let mut quiet = QuietUi::default();
    let ui: &mut dyn Ui = if json { &mut quiet } else { ui };
    match cmd {
        "status" => cmd_status(ws, a, json, ui),
        "verify" => cmd_verify(ws, a, json, ui),
        "fetch" => cmd_fetch(ws, a, json, ui),
        "lock" => cmd_lock(ws, a, json, ui),
        "repair" => cmd_repair(ws, a, json, ui),
        "licenses" => cmd_licenses(ws, a, json, ui),
        other => bail!("unknown command `{other}`"),
    }
}

fn scan_usage(ws: &Workspace, a: &Args) -> Option<Usage> {
    if a.flag("no-usage") || ws.loaded.manifest.policy.usage_roots.is_empty() {
        return None;
    }
    usage::scan(&ws.root, &ws.loaded, &ws.places).ok()
}

/// Status of one asset (no hashing).
#[derive(Clone, Debug, Serialize)]
pub struct AssetStatus {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    pub license: String,
    /// `ok`, `ok, credit`, `approved`, `NEEDS APPROVAL`, `BLOCKED`.
    pub verdict: String,
    /// `installed`, `not installed`, `external`, `n/a`.
    pub install: String,
    pub files: u64,
    pub bytes: u64,
    /// `locked`, `stale` (manifest changed since), or `-`.
    pub lock: String,
    /// Usage roots referencing it; `None` when usage was not scanned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_by: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
    pub optional: bool,
}

/// Status of every selected asset.
pub fn status(ws: &Workspace, assets: &[&Asset], usage: Option<&Usage>) -> Vec<AssetStatus> {
    assets
        .iter()
        .map(|a| {
            let (v, _, _) = audit::verdict(ws, a);
            let (install, files, bytes) = match ws.asset_dir(a) {
                None if ws.external_installs(a).is_empty() => ("external".to_string(), 0, 0),
                None if !a.applies_here() => ("n/a".to_string(), 0, 0),
                None if ws.is_installed(a) => ("external, found".to_string(), 0, 0),
                None => ("external, not found".to_string(), 0, 0),
                Some(_) if !a.applies_here() => ("n/a".to_string(), 0, 0),
                Some(d) if ws.is_installed(a) => {
                    let (b, f) = fsutil::tree_size(&d, &[".git"]).unwrap_or((0, 0));
                    ("installed".to_string(), f, b)
                }
                Some(_) => ("not installed".to_string(), 0, 0),
            };
            let lock = match ws.lock.get(&a.id) {
                Some(l) if l.spec_sha256 == spec_digest(a) => "locked",
                Some(_) => "stale",
                None => "-",
            };
            AssetStatus {
                id: a.id.clone(),
                name: a.name.clone(),
                scope: a.scope,
                license: a.license.id.clone(),
                verdict: credits::verdict_label(v).to_string(),
                install,
                files,
                bytes,
                lock: lock.to_string(),
                used_by: usage.map(|u| u.roots_using(&a.id).into_iter().map(String::from).collect()),
                download_size: a.download_size(),
                optional: a.optional,
            }
        })
        .collect()
}

fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::Audio => "audio",
        Scope::Tool => "tool",
        Scope::App => "app",
    }
}

fn findings_block(findings: &[Finding], ui: &mut dyn Ui) {
    let blockers = findings.iter().filter(|f| f.severity == Severity::Blocker).count();
    let warnings = findings.iter().filter(|f| f.severity == Severity::Warning).count();
    ui.line(&format!("License findings: {blockers} blocker(s), {warnings} warning(s)"));
    let rows: Vec<Vec<String>> = findings
        .iter()
        .filter(|f| f.severity >= Severity::Warning)
        .map(|f| vec![f.severity.as_str().to_string(), f.asset.clone(), f.code.to_string(), f.message.clone()])
        .collect();
    for l in table(&rows) {
        ui.line(&format!("  {l}"));
    }
}

/// Lay out rows as columns separated by two spaces; every column but the last is padded to its widest cell.
fn table(rows: &[Vec<String>]) -> Vec<String> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for (c, cell) in r.iter().enumerate() {
                if c + 1 == r.len() {
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    line.extend(std::iter::repeat_n(' ', widths[c] - cell.chars().count() + 2));
                }
            }
            line
        })
        .collect()
}

fn cmd_status(ws: &Workspace, a: &Args, json: bool, ui: &mut dyn Ui) -> Result<u8> {
    let assets = ws.loaded.select(&a.pos)?;
    let usage = scan_usage(ws, a);
    let rows = status(ws, &assets, usage.as_ref());
    let findings = audit::audit(ws, &assets, usage.as_ref());
    if json {
        #[derive(Serialize)]
        struct Out<'a> {
            manifest: String,
            lock: String,
            assets: &'a [AssetStatus],
            findings: &'a [Finding],
        }
        print_json(&Out { manifest: ws.rel(&ws.manifest_path), lock: ws.rel(&ws.lock_path), assets: &rows, findings: &findings })?;
        return Ok(0);
    }
    let locked = rows.iter().filter(|r| r.lock != "-").count();
    ui.line(&format!(
        "{}: {} assets; lock {}: {} locked; usage: {}",
        ws.rel(&ws.manifest_path),
        rows.len(),
        ws.rel(&ws.lock_path),
        locked,
        usage.as_ref().map_or("not scanned".to_string(), |u| format!("{} roots, {} files followed", u.roots.len(), u.files_scanned))
    ));
    let mut cells = vec![["ID", "SCOPE", "LICENSE", "POLICY", "INSTALL", "LOCK", "USED BY"].map(String::from).to_vec()];
    for r in &rows {
        let install = if r.install == "installed" { format!("{} files, {}", r.files, fsutil::fmt_bytes(r.bytes)) } else { r.install.clone() };
        let used = match &r.used_by {
            None => "-".to_string(),
            Some(v) if v.is_empty() => if r.scope == Scope::Audio { "not used".into() } else { "-".into() },
            Some(v) if v.len() <= 2 => v.join(", "),
            Some(v) => format!("{} files ({}, ...)", v.len(), v[0]),
        };
        cells.push(vec![r.id.clone(), scope_name(r.scope).into(), r.license.clone(), r.verdict.clone(), install, r.lock.clone(), used]);
    }
    for l in table(&cells) {
        ui.line(&l);
    }
    ui.line("");
    findings_block(&findings, ui);
    Ok(0)
}

fn cmd_verify(ws: &Workspace, a: &Args, json: bool, ui: &mut dyn Ui) -> Result<u8> {
    let assets = ws.loaded.select(&a.pos)?;
    let opts = VerifyOptions { quick: a.flag("quick") };
    let mut checks = Vec::new();
    let width = assets.iter().map(|x| x.id.len()).max().unwrap_or(0);
    for asset in &assets {
        let c = verify::verify_asset(ws, asset, opts, ui);
        if !json {
            print_check(&c, width, ui);
        }
        checks.push(c);
    }
    let bad = checks.iter().any(|c| matches!(c.state, State::Drift | State::Error));
    if json {
        print_json(&checks)?;
    } else {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for c in &checks {
            *counts.entry(c.state.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
        ui.line(&format!("Summary: {}", parts.join(", ")));
    }
    Ok(u8::from(bad))
}

/// How many paths of one issue kind are listed before "... and N more".
const LIST_LIMIT: usize = 10;

/// One asset's verify result; `width` aligns the id column across assets.
fn print_check(c: &verify::AssetCheck, width: usize, ui: &mut dyn Ui) {
    let size = if c.files > 0 { format!("{} files, {}", c.files, fsutil::fmt_bytes(c.bytes)) } else { String::new() };
    ui.line(&format!("{:<width$}  {:<13}  {:<20}  {:.1} s", c.id, c.state.as_str(), size, c.seconds));
    if let Some(e) = &c.error {
        ui.line(&format!("    error: {e}"));
    }
    for line in &c.checked {
        ui.line(&format!("    {line}"));
    }
    if !c.issues.is_empty() {
        ui.line(&format!("    issues: {}", c.summary()));
    }
    let mut shown: BTreeMap<IssueKind, usize> = BTreeMap::new();
    for i in &c.issues {
        let n = shown.entry(i.kind).or_default();
        *n += 1;
        if *n <= LIST_LIMIT {
            let what = i.path.as_deref().or(i.detail.as_deref()).unwrap_or("");
            ui.line(&format!("    {:<16} {what}", i.kind.as_str()));
        }
    }
    for (k, n) in shown {
        if n > LIST_LIMIT {
            ui.line(&format!("    {:<16} ... and {} more", k.as_str(), n - LIST_LIMIT));
        }
    }
}

fn cmd_fetch(ws: &mut Workspace, a: &Args, json: bool, ui: &mut dyn Ui) -> Result<u8> {
    let max_size = a.one("max-size").map(fsutil::parse_size).transpose()?;
    let opts = FetchOptions {
        all: a.flag("all"),
        yes: a.flag("yes"),
        max_size,
        dry_run: a.flag("dry-run"),
        check_urls: a.flag("check-urls"),
        force: a.flag("force"),
    };
    if opts.check_urls && !opts.dry_run {
        bail!("--check-urls only works with --dry-run");
    }
    if json && !opts.yes && !opts.dry_run {
        bail!("--json cannot ask for confirmation; add --yes (or --dry-run)");
    }
    let out = fetch::fetch(ws, &a.pos, &opts, ui)?;
    let bad = out.iter().any(|o| matches!(o.result.as_str(), "failed" | "refused"));
    if json {
        print_json(&out)?;
    } else {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for o in &out {
            *counts.entry(o.result.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
        let dl: u64 = out.iter().map(|o| o.downloaded).sum();
        ui.line(&format!("Summary: {}; downloaded {}", parts.join(", "), fsutil::fmt_bytes(dl)));
    }
    Ok(u8::from(bad))
}

fn cmd_lock(ws: &mut Workspace, a: &Args, json: bool, ui: &mut dyn Ui) -> Result<u8> {
    let out = fetch::lock_existing(ws, &a.pos, ui)?;
    let bad = out.iter().any(|o| matches!(o.result.as_str(), "failed" | "refused"));
    if json {
        print_json(&out)?;
    } else {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for o in &out {
            *counts.entry(o.result.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
        ui.line(&format!("Summary: {}; lock written to {}", parts.join(", "), ws.rel(&ws.lock_path)));
    }
    Ok(u8::from(bad))
}

fn cmd_repair(ws: &mut Workspace, a: &Args, json: bool, ui: &mut dyn Ui) -> Result<u8> {
    let [id] = a.pos.as_slice() else { bail!("repair takes exactly one asset id") };
    if json && !a.flag("yes") {
        bail!("--json cannot ask for confirmation; add --yes");
    }
    let r = fetch::repair(ws, id, a.flag("yes"), ui)?;
    if json {
        print_json(&r)?;
    } else {
        ui.line(&format!(
            "{}: {} file(s) restored{}; now {}",
            r.id,
            r.restored.len(),
            if r.sparse_reset { ", sparse patterns reset" } else { "" },
            r.state_after.as_str()
        ));
        if !r.extra_left.is_empty() {
            ui.line(&format!("  {} extra file(s) left in place (not from any source), e.g. {}", r.extra_left.len(), r.extra_left[0]));
        }
    }
    Ok(u8::from(r.state_after != State::Ok))
}

fn cmd_licenses(ws: &Workspace, a: &Args, json: bool, ui: &mut dyn Ui) -> Result<u8> {
    let usage = scan_usage(ws, a);
    let all: Vec<&Asset> = ws.loaded.assets().iter().collect();
    let findings = audit::audit(ws, &all, usage.as_ref());
    let md = credits::licenses_markdown(ws, usage.as_ref(), &findings);
    let credits_txt = credits::credits_text(ws, usage.as_ref());
    let mut written = Vec::new();
    if let Some(p) = a.one("out") {
        let p = ws.root.join(p);
        fsutil::atomic_write(&p, md.as_bytes())?;
        written.push(ws.rel(&p));
    }
    if let Some(p) = a.one("credits") {
        let p = ws.root.join(p);
        fsutil::atomic_write(&p, credits_txt.as_bytes())?;
        written.push(ws.rel(&p));
    }
    let missing = match a.one("check") {
        Some(p) => {
            let p = ws.root.join(p);
            let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            Some((ws.rel(&p), credits::missing_from(ws, &text, usage.as_ref())))
        }
        None => None,
    };
    let texts = match a.one("texts") {
        Some(d) => Some(credits::collect_texts(ws, &ws.root.join(d))?),
        None => None,
    };
    let blockers = findings.iter().filter(|f| f.severity == Severity::Blocker).count();
    let bad = blockers > 0 || missing.as_ref().is_some_and(|(_, m)| !m.is_empty()) || texts.as_ref().is_some_and(|(_, f)| !f.is_empty());
    if json {
        #[derive(Serialize)]
        struct Out<'a> {
            findings: &'a [Finding],
            credits: &'a str,
            written: &'a [String],
            #[serde(skip_serializing_if = "Option::is_none")]
            missing_from_existing: Option<&'a Vec<String>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            texts: Option<&'a Vec<credits::CollectedText>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            text_failures: Option<&'a Vec<String>>,
        }
        print_json(&Out {
            findings: &findings,
            credits: &credits_txt,
            written: &written,
            missing_from_existing: missing.as_ref().map(|(_, m)| m),
            texts: texts.as_ref().map(|(t, _)| t),
            text_failures: texts.as_ref().map(|(_, f)| f),
        })?;
        return Ok(u8::from(bad));
    }
    if a.flag("print") {
        ui.line(&md);
    }
    findings_block(&findings, ui);
    ui.line("");
    ui.line("In-game credits:");
    for l in credits_txt.lines() {
        ui.line(&format!("  {l}"));
    }
    if let Some((file, m)) = &missing {
        if m.is_empty() {
            ui.line(&format!("{file} mentions every installed or used sound library"));
        } else {
            ui.line(&format!("{file} does not mention {} installed/used sound librar(ies): {}", m.len(), m.join(", ")));
        }
    }
    if let Some((t, f)) = &texts {
        let copied = t.iter().filter(|x| x.origin == "install").count();
        let pinned = t.iter().filter(|x| x.pinned).count();
        ui.line(&format!(
            "License texts: {} collected ({copied} copied from installs, {} downloaded, {pinned} of them matched a pinned sha256), {} problem(s)",
            t.len(),
            t.len() - copied,
            f.len()
        ));
        for x in f {
            ui.line(&format!("  {x}"));
        }
    }
    for w in &written {
        ui.line(&format!("wrote {w}"));
    }
    Ok(u8::from(bad))
}

/// Run `flux flac <args>`. Returns the exit code.
pub fn run_flac(args: &[String], ui: &mut dyn Ui) -> Result<u8> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        ui.line(FLAC_HELP);
        return Ok(if args.is_empty() { 2 } else { 0 });
    }
    let a = Args::parse(
        args,
        &["no-dither", "no-verify", "json"],
        &["bits", "seed", "title", "artist", "tag", "threads"],
        &["loop"],
    )?;
    let [src, dst] = a.pos.as_slice() else { bail!("expected IN.wav OUT.flac") };
    let bits = match a.one("bits").unwrap_or("16") {
        "16" => BitDepth::B16,
        "24" => BitDepth::B24,
        b => bail!("--bits must be 16 or 24, not {b}"),
    };
    let seed = a.one("seed").map(|s| s.parse::<u64>().with_context(|| format!("--seed {s}"))).transpose()?;
    let dither = if a.flag("no-dither") { Dither::None } else { Dither::Tpdf { seed: seed.unwrap_or(1) } };
    let mut opts = FlacOptions { bits, dither, verify: !a.flag("no-verify"), ..FlacOptions::default() };
    if let Some(t) = a.one("title") {
        opts.tags.push(("TITLE".into(), t.into()));
    }
    if let Some(t) = a.one("artist") {
        opts.tags.push(("ARTIST".into(), t.into()));
    }
    for t in a.opts.get("tag").into_iter().flatten() {
        let (k, v) = t.split_once('=').with_context(|| format!("--tag {t}: expected NAME=VALUE"))?;
        opts.tags.push((k.into(), v.into()));
    }
    if let Some(l) = a.opts.get("loop") {
        let p = |s: &str| s.parse::<u64>().with_context(|| format!("--loop: {s} is not a sample count"));
        let l = &l[l.len() - 2..];
        opts.loop_points = Some(flac::LoopPoints { start: p(&l[0])?, length: p(&l[1])? });
    }
    if let Some(t) = a.one("threads") {
        opts.threads = Some(t.parse().with_context(|| format!("--threads {t}"))?);
    }
    let (src, dst) = (PathBuf::from(src), PathBuf::from(dst));
    let r = flac::wav_to_flac(&src, &dst, &opts)?;
    if a.flag("json") {
        print_json(&r)?;
    } else {
        ui.line(&format!(
            "{}: {} Hz, {} ch, {}-bit, {:.2} s -> {} ({:.1}% of PCM), encoded in {:.2} s{}{}",
            dst.display(),
            r.sample_rate,
            r.channels,
            r.bits,
            r.duration_seconds(),
            fsutil::fmt_bytes(r.bytes),
            r.ratio() * 100.0,
            r.encode_seconds,
            if r.verified { format!(", verified (MD5 {})", r.md5) } else { ", NOT verified".into() },
            if r.clipped + r.non_finite > 0 { format!("; {} clipped, {} non-finite samples", r.clipped, r.non_finite) } else { String::new() },
        ));
    }
    Ok(0)
}

