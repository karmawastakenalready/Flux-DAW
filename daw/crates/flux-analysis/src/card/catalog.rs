//! Carding many instruments: job lists (explicit files or "everything the songs use"), parallel runs with
//! optional process isolation, card files and the catalog README index.

use super::model::Card;
use super::sfztext::{self, note_name, portable};
use super::{CardOptions, Schema, card, default_name, flags, today_utc};
use crate::json::{py_str, to_string_indent1};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One instrument to card.
#[derive(Clone, Debug)]
pub struct Job {
    pub path: PathBuf,
    pub name: String,
    pub slug: String,
    /// Where the instrument is used; `None` leaves the card without the field.
    pub used_by: Option<Vec<String>>,
    pub aliases: Vec<String>,
}

/// Where each card is measured.
#[derive(Clone, Debug)]
pub enum Isolation {
    /// In this process (fast to start; a sfizz crash takes the process down, and instruments with random
    /// opcodes don't reproduce the reference because sfizz's random generator is shared).
    InProcess,
    /// Each card in its own child process, as the reference did: `exe args... --sfz ... --out-file ...` must run
    /// [`worker_main`] (the `flux-card card-worker` subcommand does).
    Subprocess { exe: PathBuf, args: Vec<String> },
}

/// How to run a batch.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub card: CardOptions,
    /// Instruments measured at once (0 = min(8, threads)).
    pub jobs: usize,
    pub isolation: Isolation,
    /// Per-card limit in subprocess mode.
    pub timeout: Duration,
}

impl RunOptions {
    pub fn new(card: CardOptions, isolation: Isolation) -> RunOptions {
        RunOptions { card, jobs: 0, isolation, timeout: Duration::from_secs(900) }
    }

    fn jobs(&self) -> usize {
        if self.jobs > 0 { self.jobs } else { rayon::current_num_threads().clamp(1, 8) }
    }
}

/// Write a card as `json.dump(card, indent=1, ensure_ascii=False)` would.
/// Write a card as `json.dump(card, f, indent=1, ensure_ascii=False)` does in text mode: the platform's line
/// endings (CRLF on Windows), so regenerated cards are byte-for-byte what the Python tool wrote.
pub fn write_card(card: &Card, path: &Path) -> Result<()> {
    let text = to_string_indent1(card)?;
    std::fs::write(path, native_newlines(&text).as_bytes()).with_context(|| format!("writing {}", path.display()))
}

/// `\n` as the platform writes text files (CRLF on Windows, as Python's text mode does).
fn native_newlines(s: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) { s.replace('\n', "\r\n").into() } else { s.into() }
}

pub fn read_card(path: &Path) -> Result<Card> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Jobs for explicit SFZ files. `name` needs exactly one file. Usage notes (`used_by`, `aliases`) of an existing
/// card with the same slug in `out` are kept.
pub fn jobs_for(paths: &[PathBuf], name: Option<&str>, opts: &CardOptions, out: &Path) -> Result<Vec<Job>> {
    if name.is_some() && paths.len() != 1 {
        bail!("a card name needs exactly one SFZ");
    }
    let mut jobs = vec![];
    for p in paths {
        let path = std::path::absolute(p).unwrap_or_else(|_| p.clone());
        let name = name.map(str::to_string).unwrap_or_else(|| default_name(&opts.root, &path));
        let slug = sfztext::slugify(&format!("{name}{}", opts.slug_suffix()));
        let (mut used_by, mut aliases) = (None, vec![]);
        if let Ok(prev) = read_card(&out.join(format!("{slug}.json"))) {
            used_by = prev.used_by;
            aliases = prev.aliases.unwrap_or_default();
        }
        jobs.push(Job { path, name, slug, used_by, aliases });
    }
    Ok(jobs)
}

/// Extra "used by" entries for [`gather_used`] from a JSON file: `[{"sfz": "build/sfz/x.sfz", "where": "set/name"},
/// ...]`, relative paths taken from `root`. (This is how the `instruments.py` sets reach the Rust tool.)
pub fn read_used_extra(root: &Path, file: &Path) -> Result<Vec<(PathBuf, String)>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        sfz: PathBuf,
        #[serde(rename = "where")]
        used_where: String,
    }
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let entries: Vec<Entry> =
        serde_json::from_str(&text).with_context(|| format!("{}: expected [{{\"sfz\": ..., \"where\": ...}}]", file.display()))?;
    Ok(entries.into_iter().map(|e| (root.join(e.sfz), e.used_where)).collect())
}

/// The SFZ files the songs use, with where: Flux projects (`daw/projects/*.flux.json` sfz tracks), `sfz/*.sfz`,
/// the VCSL candidates, and `extra` (path, where) entries from the caller. (The Python tool also asked
/// `instruments.py` for its sets; pass those as `extra`.)
pub fn gather_used(root: &Path, extra: &[(PathBuf, String)]) -> Vec<(PathBuf, Vec<String>)> {
    let mut used: Vec<(String, PathBuf, Vec<String>)> = vec![];
    let mut add = |p: PathBuf, w: String| {
        let p = std::path::absolute(&p).unwrap_or(p);
        let key = p.to_string_lossy().to_lowercase();
        match used.iter_mut().find(|(k, ..)| *k == key) {
            Some(e) => e.2.push(w),
            None => used.push((key, p, vec![w])),
        }
    };
    let sorted_files = |dir: &Path, ext: &str| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && p.to_string_lossy().ends_with(ext)).collect())
            .unwrap_or_default();
        v.sort();
        v
    };
    // projects saved before the rename end in .baton.json
    let projects = root.join("daw").join("projects");
    for proj in sorted_files(&projects, ".flux.json").into_iter().chain(sorted_files(&projects, ".baton.json")) {
        let song = proj.file_name().map_or(String::new(), |n| n.to_string_lossy().split('.').next().unwrap_or("").to_string());
        let Ok(v) = std::fs::read_to_string(&proj).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str::<Value>(&t)?)) else {
            continue;
        };
        for t in v.get("tracks").and_then(Value::as_array).into_iter().flatten() {
            let ins = t.get("instrument").cloned().unwrap_or(Value::Null);
            if ins.get("kind").and_then(Value::as_str) == Some("sfz")
                && let Some(p) = ins.get("path").and_then(Value::as_str).filter(|p| !p.is_empty())
            {
                let tname = t.get("name").map_or("None".to_string(), py_str);
                add(root.join(p), format!("{song}: {tname}"));
            }
        }
    }
    for p in sorted_files(&root.join("sfz"), ".sfz") {
        add(p, "sfz/".into());
    }
    for (p, w) in extra {
        add(p.clone(), w.clone());
    }
    let mut vcsl = vec![];
    collect_sfz(&root.join("instruments").join("VCSL"), &mut vcsl);
    vcsl.sort();
    const CANDIDATES: [&str; 5] = ["Folk Harp", "Strumstick", "Psaltery", "Recorder", "Frame Drum"];
    for p in vcsl {
        let base = p.file_name().map_or(String::new(), |n| n.to_string_lossy().to_string());
        if CANDIDATES.iter().any(|c| base.contains(c)) {
            add(p, "VCSL candidate".into());
        }
    }
    used.into_iter().map(|(_, p, w)| (p, w)).collect()
}

fn collect_sfz(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_sfz(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sfz")) {
            out.push(p);
        }
    }
}

/// Merge identical instruments (same text after `#include`s, see [`sfztext::content_key`]) into one job each.
pub fn dedupe(root: &Path, entries: &[(PathBuf, Vec<String>)], opts: &CardOptions) -> Vec<Job> {
    let mut groups: Vec<(String, Job)> = vec![];
    for (p, where_) in entries {
        let key = sfztext::content_key(p).unwrap_or_else(|_| p.to_string_lossy().to_lowercase());
        let gi = match groups.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let name = default_name(root, p);
                let slug = sfztext::slugify(&format!("{name}{}", opts.slug_suffix()));
                groups.push((key, Job { path: p.clone(), name, slug, used_by: Some(vec![]), aliases: vec![] }));
                groups.len() - 1
            }
        };
        let g = &mut groups[gi].1;
        let ub = g.used_by.get_or_insert_with(Vec::new);
        for w in where_ {
            if !ub.contains(w) {
                ub.push(w.clone());
            }
        }
        if p.to_string_lossy().to_lowercase() != g.path.to_string_lossy().to_lowercase() {
            g.aliases.push(portable(root, p));
        }
    }
    groups.into_iter().map(|(_, j)| j).collect()
}

/// Card every job into `out` (`<slug>.json`), then rebuild `out/README.md`. Cards are reported to `on_card` as
/// they finish (in completion order) and returned in job order. Large instruments start first.
pub fn card_many(jobs: &[Job], out: &Path, opts: &RunOptions, on_card: &(dyn Fn(&Card) + Sync)) -> Result<Vec<Card>> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    opts.card.key_range()?;
    let n = opts.jobs().min(jobs.len().max(1));
    let mut copts = opts.card.clone();
    if copts.key_workers == 0 {
        copts.key_workers = (rayon::current_num_threads() / n).clamp(1, 8);
    }
    // biggest first, so the longest card doesn't start last
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    let size = |j: &Job| sfztext::sfz_text(&j.path).map_or(0, |t| t.matches("<region>").count());
    let sizes: Vec<usize> = jobs.iter().map(size).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sizes[i]));
    // `n` instruments at once, each on its own thread; their notes render on the rayon pool
    let queue = std::sync::Mutex::new(order.into_iter());
    let cards: std::sync::Mutex<Vec<Option<Card>>> = std::sync::Mutex::new(vec![None; jobs.len()]);
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                    let Some(i) = next else { break };
                    let c = run_job(&jobs[i], out, &copts, opts);
                    on_card(&c);
                    cards.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(c);
                }
            });
        }
    });
    write_index(out, opts.card.schema)?;
    Ok(cards.into_inner().unwrap_or_else(|e| e.into_inner()).into_iter().flatten().collect())
}

fn run_job(job: &Job, out: &Path, copts: &CardOptions, opts: &RunOptions) -> Card {
    let t0 = Instant::now();
    let mut c = match &opts.isolation {
        Isolation::InProcess => match card(&job.path, Some(&job.name), copts) {
            Ok(mut c) => {
                c.slug = job.slug.clone();
                c
            }
            Err(e) => error_card(job, copts, format!("{e:#}")),
        },
        Isolation::Subprocess { exe, args } => run_worker(job, out, copts, exe, args, opts.timeout),
    };
    if job.used_by.is_some() {
        c.used_by = job.used_by.clone();
        c.aliases = Some(job.aliases.clone());
    }
    c.seconds = Some(crate::num::round_py(t0.elapsed().as_secs_f64(), 1));
    let dst = out.join(format!("{}.json", job.slug));
    if let Err(e) = write_card(&c, &dst) {
        c.errors.insert("write", format!("{e:#}"));
    }
    c
}

fn error_card(job: &Job, copts: &CardOptions, err: String) -> Card {
    let mut c = Card {
        name: job.name.clone(),
        slug: job.slug.clone(),
        sfz: portable(&copts.root, &job.path),
        measured: copts.measured.clone().unwrap_or_else(today_utc),
        error: Some(err),
        ..Default::default()
    };
    if let Ok(text) = sfztext::sfz_text(&job.path) {
        c.license = Some(sfztext::license_of(&copts.root, &job.path, &text));
    }
    c.flags = flags(&c, copts.schema == Schema::Extended);
    c
}

/// The command line of one worker run (see [`worker_main`]).
pub fn worker_args(job: &Job, copts: &CardOptions, out_file: &Path) -> Vec<String> {
    let mut a = vec![
        "--sfz".into(),
        job.path.to_string_lossy().into_owned(),
        "--name".into(),
        job.name.clone(),
        "--slug".into(),
        job.slug.clone(),
        "--out-file".into(),
        out_file.to_string_lossy().into_owned(),
        "--root".into(),
        copts.root.to_string_lossy().into_owned(),
        "--schema".into(),
        if copts.schema == Schema::Reference { "reference".into() } else { "extended".into() },
        "--key-workers".into(),
        copts.key_workers.to_string(),
    ];
    if let Some(k) = &copts.keys {
        a.extend(["--keys".into(), k.clone()]);
    }
    if let Some(k) = copts.ks {
        a.extend(["--ks".into(), k.to_string()]);
    }
    if let Some(m) = &copts.measured {
        a.extend(["--measured".into(), m.clone()]);
    }
    a
}

fn run_worker(job: &Job, out: &Path, copts: &CardOptions, exe: &Path, args: &[String], timeout: Duration) -> Card {
    let tmp = out.join(format!(".{}.worker.json", job.slug));
    let _ = std::fs::remove_file(&tmp);
    let mut cmd = Command::new(exe);
    cmd.args(args).args(worker_args(job, copts, &tmp)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let err = match cmd.spawn() {
        Err(e) => Some(format!("starting {}: {e}", exe.display())),
        Ok(mut child) => {
            // drain stderr on a thread so a chatty worker can't block on a full pipe
            let stderr = child.stderr.take();
            let reader = std::thread::spawn(move || {
                let mut s = String::new();
                if let Some(mut e) = stderr {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut s);
                }
                s
            });
            let t0 = Instant::now();
            let status = loop {
                match child.try_wait() {
                    Ok(Some(s)) => break Ok(s),
                    Ok(None) if t0.elapsed() > timeout => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(format!("timed out after {} s", timeout.as_secs()));
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(e) => break Err(format!("waiting for the worker: {e}")),
                }
            };
            let stderr = reader.join().unwrap_or_default();
            match status {
                Ok(s) if s.success() => None,
                Ok(s) => Some(
                    stderr.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string).unwrap_or_else(|| exit_text(s)),
                ),
                Err(e) => Some(e),
            }
        }
    };
    let card = read_card(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    match card {
        Some(c) => c,
        None => error_card(job, copts, err.unwrap_or_else(|| "no card written".into())),
    }
}

/// A failed worker's exit status worded as the reference does (`exit code 3221225477` for an access violation on
/// Windows, `exit code -11` for SIGSEGV elsewhere).
fn exit_text(s: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return format!("exit code -{sig}");
        }
    }
    match s.code() {
        #[cfg(windows)]
        Some(c) => format!("exit code {}", c as u32),
        #[cfg(not(windows))]
        Some(c) => format!("exit code {c}"),
        None => s.to_string(),
    }
}

/// Entry point of a card worker process. Arguments (as [`worker_args`] builds them): `--sfz PATH --name NAME
/// --slug SLUG --out-file FILE [--root DIR] [--schema reference|extended] [--key-workers N] [--keys LO-HI]
/// [--ks KEY] [--measured DATE]`. Writes the card (always, an error card if measuring failed) and returns the
/// process exit code.
pub fn worker_main(args: &[String]) -> i32 {
    match worker(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

fn worker(args: &[String]) -> Result<()> {
    let mut opts = CardOptions::default();
    let (mut sfz, mut name, mut slug, mut out) = (None, None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--sfz" => sfz = Some(PathBuf::from(val()?)),
            "--name" => name = Some(val()?),
            "--slug" => slug = Some(val()?),
            "--out-file" => out = Some(PathBuf::from(val()?)),
            "--root" => opts.root = PathBuf::from(val()?),
            "--schema" => {
                opts.schema = match val()?.as_str() {
                    "reference" => Schema::Reference,
                    "extended" => Schema::Extended,
                    s => bail!("unknown schema {s:?} (reference or extended)"),
                }
            }
            "--key-workers" => opts.key_workers = val()?.parse().context("--key-workers")?,
            "--keys" => opts.keys = Some(val()?),
            "--ks" => opts.ks = Some(val()?.parse().context("--ks")?),
            "--measured" => opts.measured = Some(val()?),
            other => bail!("unknown worker argument {other:?}"),
        }
    }
    let sfz = sfz.context("--sfz is required")?;
    let out = out.context("--out-file is required")?;
    let mut c = card(&sfz, name.as_deref(), &opts)?;
    if let Some(s) = slug {
        c.slug = s;
    }
    write_card(&c, &out)
}

// ------------------------------------------------------------------------------------------------ README index

const README_HEAD: &str = "# Instrument catalog

One report card per sampled instrument (SFZ): every instrument the songs use, plus candidates. The composer can't
listen, so instruments are measured here before they go into a song. Each name links to its JSON card with all the
numbers (per-key pitch and level, brightness per octave, velocity and CC response, keyswitches, license).

Regenerate everything with `python -I tools/instrument_card.py --used`, or card one file with
`python -I tools/instrument_card.py path/to/file.sfz`.

## Flags

| Flag | What it means | Threshold |
|---|---|---|
| harsh | Too much energy above 3 kHz, so it sounds thin and piercing. The rejected lyre measured -28.5 dB; the liked VSCO harp -49.6 dB. The verdicts behind this threshold came from plucked instruments: bowed strings, reeds and brass are naturally brighter (about -10 to -30 dB), so compare those within their own family | hi3k_db > -35 dB in the main range |
| sharp attack | The start of each note is a spiky click well above the body of the note. The rejected zither melody strings measured 17.9 dB | attack > 12 dB |
| uneven | Some notes are much louder or softer than their neighbours. Fix: `tools/sfz_even.py` | neighbouring keys differ by more than 6 dB |
| out of tune | Some notes are off pitch | more than 15 cents (a semitone is 100) |
| short sustain | Held notes die away early because the recordings are short | a held note's recording runs out before 3 s |
| late start | A plucked or struck note whose recording begins with silence, so it sounds behind the beat (it also makes its attack number meaningless) | attack arrives more than 25 ms after note-on |
";

const EXTENDED_FLAGS: &str = "| level hole | Single keys (or a few) much quieter than the keys around them, e.g. a missing or badly cut sample | at least 10 dB under the median of the keys within 4 semitones |
| pitch slide | The pitch moves during the note (it scoops into or drifts off its note), measured from 0.04-0.2 s to 0.3-0.5 s | more than 20 cents on a key |
| cc dropout | The note goes silent at high mod wheel (CC1) or expression (CC11) values where lower values sound | 40 dB under the loudest value |
";

const README_TAIL: &str = "| partial / error | Some measurements, or the whole card, failed. See the JSON | |

## Columns

- **keys**: main playable range (C4 = MIDI 60). \"+N isolated\" = single keys outside it (keyswitches, FX); \"N gaps\" = silent keys inside it. Kits list every key that sounds.
- **offset**: sounding note minus played key, in semitones. +12 means it sounds an octave above the key you write. \"kit\" = unpitched (drum or FX map); \"mixed\" = pitched, but the octave changes across the range (e.g. an organ with several stops). Single keys that sound in another octave are listed in the JSON (`offset_outliers`).
- **tuning**: the worst pitch error of any key, in cents, over the first half second (so a note that starts flat and scoops up into pitch counts as out of tune). Bells, glockenspiel and timpani have inharmonic overtones, so their pitch, offset and tuning readings are rough.
- **evenness**: the biggest level jump between neighbouring keys (sd = spread of all key levels), at velocity 90.
- **hi3k_db / attack**: brightness and attack spike, median over the main range at velocity 70 (lower = softer).
- **sustain**: \"held\" = the note sustains while the key is down (\"6 s+\" = still going after 6 s, \"held 4.5 s\" = the recording runs out after 4.5 s); \"rings\" = a plucked or struck note dies away (time to -40 dB). \"rel\" = time to fade after the key is released.
- **license**: from the library's own LICENSE or README. A dagger (†) means the library is missing from `LICENSES.md`, so add it before shipping.

## Cards

| Name | Keys | Offset | Tuning | Evenness | hi3k_db | Attack | Sustain | Flags | License |
|---|---|---|---|---|---|---|---|---|---|
";

/// The README text above the card rows.
pub fn readme_head(schema: Schema) -> String {
    let mut s = README_HEAD.to_string();
    if schema == Schema::Extended {
        s.push_str(EXTENDED_FLAGS);
    }
    s.push_str(README_TAIL);
    s
}

fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// One README table row for a card file's text (any card JSON, including ones written by the Python tool).
pub fn readme_row(card_json: &str) -> Result<String> {
    // flags are listed in the card's order, which `serde_json::Value` (a sorted map here) would lose
    #[derive(serde::Deserialize)]
    struct FlagOrder {
        #[serde(default)]
        flags: Option<crate::json::OrderedMap<serde::de::IgnoredAny>>,
    }
    let c: Value = serde_json::from_str(card_json).context("card JSON")?;
    let order: FlagOrder = serde_json::from_str(card_json).context("card flags")?;
    let flags: Vec<String> = order.flags.map(|f| f.keys().map(str::to_string).collect()).unwrap_or_default();
    Ok(row(&c, &flags))
}

fn row(c: &Value, flags: &[String]) -> String {
    let name = py_str(get(c, "name"));
    let slug = py_str(get(c, "slug"));
    let lic = get(c, "license");
    let mut lic_s = lic.get("name").map_or("unknown".to_string(), py_str);
    if !truthy(get(lic, "all_in_licenses_md")) {
        lic_s.push_str(" †");
    }
    if truthy(get(c, "error")) {
        let e: String = py_str(get(c, "error")).chars().take(80).collect();
        return format!("| [{name}]({slug}.json) | | | | | | | | error: {e} | {lic_s} |");
    }
    let (k, p, lv, t, s) = (get(c, "keys"), get(c, "pitch"), get(c, "level"), get(c, "timbre"), get(c, "sustain"));
    let kind = get(p, "kind").as_str().unwrap_or("");
    let tonal = kind == "pitched" || kind == "mixed";
    let int = |v: &Value| v.as_i64().unwrap_or(0);
    let keys = if tonal && truthy(get(k, "main")) {
        let m = get(k, "main");
        let mut s = format!("{} ({}-{})", py_str(get(k, "main_names")), py_str(&m[0]), py_str(&m[1]));
        if truthy(get(k, "isolated")) {
            s += &format!(" +{} isolated", get(k, "isolated").as_array().map_or(0, Vec::len));
        }
        if truthy(get(k, "gaps")) {
            s += &format!(", {} gaps", get(k, "gaps").as_array().map_or(0, Vec::len));
        }
        s
    } else if truthy(get(k, "span")) {
        let sp = get(k, "span");
        format!("{}-{}: {} keys", note_name(int(&sp[0])), note_name(int(&sp[1])), py_str(get(k, "count")))
    } else {
        String::new()
    };
    let off = if kind == "pitched" {
        format!("{:+}", int(get(p, "offset")))
    } else if tonal {
        "mixed".into()
    } else {
        "kit".into()
    };
    let tun = if tonal { format!("{:+.0}c", get(p, "worst_cents").as_f64().unwrap_or(0.0)) } else { "-".into() };
    let ev = if tonal && lv.get("max_jump_db").is_some() {
        format!("{} dB (sd {})", py_str(get(lv, "max_jump_db")), py_str(get(lv, "std_db")))
    } else {
        "-".into()
    };
    let tm = get(t, "main");
    let skind = get(s, "kind").as_str().unwrap_or("");
    let mut sus = if skind.starts_with("sustained") {
        let u = get(s, "usable_max_note_s");
        format!("held {}", if truthy(u) { format!("{} s", py_str(u)) } else { "6 s+".into() })
    } else if truthy(s) {
        let t40 = get(s, "t40_s");
        format!("rings {}", if truthy(t40) { format!("{} s", py_str(t40)) } else { "6 s+".into() })
    } else {
        String::new()
    };
    let rel = get(s, "release_s");
    if !rel.is_null() {
        let r = rel.as_f64().unwrap_or(0.0);
        sus += &if r >= 9.98 { ", rel 10 s+".to_string() } else { format!(", rel {} s", py_str(rel)) };
    }
    let att_v = tm.get("attack_db");
    let att = match att_v {
        None => String::new(),
        Some(v) if v.is_f64() && v.as_f64().is_some_and(|a| a > 60.0) => ">60".into(),
        Some(v) => py_str(v),
    };
    let hi3k = tm.get("hi3k_db").map_or(String::new(), py_str);
    let fl = if flags.is_empty() { "-".to_string() } else { flags.join(", ") };
    format!("| [{name}]({slug}.json) | {keys} | {off} | {tun} | {ev} | {hi3k} | {att} | {sus} | {fl} | {lic_s} |")
}

/// Rebuild `<dir>/README.md` from every card in `dir` (sorted by name). Returns the cards read.
pub fn write_index(dir: &Path, schema: Schema) -> Result<Vec<Value>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    files.sort();
    // (sort key, card, row); files that aren't card objects are skipped, as the reference does
    let mut cards: Vec<(String, Value, String)> = files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .filter_map(|t| {
            let c = serde_json::from_str::<Value>(&t).ok().filter(Value::is_object)?;
            let row = readme_row(&t).ok()?;
            Some((get(&c, "name").as_str().unwrap_or("").to_lowercase(), c, row))
        })
        .collect();
    cards.sort_by(|a, b| a.0.cmp(&b.0));
    let rows: Vec<&str> = cards.iter().map(|c| c.2.as_str()).collect();
    let text = format!("{}{}\n", readme_head(schema), rows.join("\n"));
    let path = dir.join("README.md");
    std::fs::write(&path, native_newlines(&text).as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(cards.into_iter().map(|c| c.1).collect())
}

/// One progress line per card (name, range, time, flags).
pub fn summary(c: &Card) -> String {
    let fl = if c.flags.is_empty() {
        "no flags".to_string()
    } else {
        c.flags.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("; ")
    };
    let range = c.keys.as_ref().map_or(String::new(), |k| k.main_names.clone());
    format!("{:45} {:9} {:5.1}s  {fl}", c.name, range, c.seconds.unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn readme_rows_match_the_reference_format() {
        let c = json!({"name": "far_roads/flute", "slug": "far-roads-flute",
            "license": {"name": "CC0 1.0", "all_in_licenses_md": true},
            "keys": {"main": [60, 96], "main_names": "C4-C7", "isolated": [], "gaps": [61], "span": [60, 96], "count": 36},
            "pitch": {"kind": "pitched", "offset": 0, "worst_cents": -12.5},
            "level": {"max_jump_db": 14.7, "std_db": 7.7},
            "timbre": {"main": {"hi3k_db": -40.0, "attack_db": 61.2}},
            "sustain": {"kind": "sustained", "usable_max_note_s": null, "release_s": 0.3},
            "flags": {"uneven": "x"}});
        assert_eq!(
            readme_row(&c.to_string()).unwrap(),
            "| [far_roads/flute](far-roads-flute.json) | C4-C7 (60-96), 1 gaps | +0 | -12c | 14.7 dB (sd 7.7) | -40.0 | >60 | held 6 s+, rel 0.3 s | uneven | CC0 1.0 |"
        );
        let kit = json!({"name": "kit", "slug": "kit", "keys": {"span": [36, 50], "count": 9}, "pitch": {"kind": "unpitched"},
            "timbre": {"main": {"hi3k_db": -20.5, "attack_db": 12.0}}, "sustain": {"kind": "decaying", "t40_s": 0.42, "release_s": 10.0}, "flags": {}});
        assert_eq!(readme_row(&kit.to_string()).unwrap(), "| [kit](kit.json) | C2-D3: 9 keys | kit | - | - | -20.5 | 12.0 | rings 0.42 s, rel 10 s+ | - | unknown † |");
        let err = json!({"name": "bad", "slug": "bad", "error": "load failed: sfizz could not load x.sfz"});
        assert_eq!(readme_row(&err.to_string()).unwrap(), "| [bad](bad.json) | | | | | | | | error: load failed: sfizz could not load x.sfz | unknown † |");
    }

    #[test]
    fn readme_rows_keep_the_card_flag_order() {
        let t = r#"{"name": "n", "slug": "n", "license": {"name": "CC0 1.0", "all_in_licenses_md": true},
            "keys": {"span": [36, 37], "count": 2}, "pitch": {"kind": "unpitched"},
            "flags": {"uneven": "a", "out of tune": "b", "harsh": "c"}}"#;
        assert!(readme_row(t).unwrap().ends_with("| uneven, out of tune, harsh | CC0 1.0 |"));
        let _ = readme_row("[1]"); // not a card: an error or an empty row, never a panic
        assert!(readme_row("{").is_err());
    }

    #[test]
    fn card_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Card { name: "a/b".into(), slug: "a-b".into(), sfz: "x.sfz".into(), measured: "2026-01-01".into(), ..Default::default() };
        c.keyswitches = Some(None);
        c.used_by = Some(vec!["song: lead".into()]);
        let p = dir.path().join("a-b.json");
        write_card(&c, &p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap().replace("\r\n", "\n");
        assert!(text.starts_with("{\n \"name\": \"a/b\",\n \"slug\": \"a-b\",\n \"sfz\": \"x.sfz\",\n \"about\": null,"));
        assert!(text.contains("\"keyswitches\": null"));
        let back = read_card(&p).unwrap();
        assert_eq!(back, c);
        let opts = CardOptions { root: dir.path().to_path_buf(), ..Default::default() };
        let jobs = jobs_for(&[dir.path().join("b.sfz")], Some("a/b"), &opts, dir.path()).unwrap();
        assert_eq!(jobs[0].slug, "a-b");
        assert_eq!(jobs[0].used_by, Some(vec!["song: lead".to_string()]));
        assert!(jobs_for(&[p.clone(), p], Some("x"), &opts, dir.path()).is_err());
        write_index(dir.path(), Schema::Reference).unwrap();
        let readme = std::fs::read_to_string(dir.path().join("README.md")).unwrap().replace("\r\n", "\n");
        assert!(readme.ends_with("| [a/b](a-b.json) |  | kit | - | - |  |  |  | - | unknown † |\n"), "{readme}");
    }
}
