//! Running the tools that make derived copies ([`tighten`], [`retune`], [`even`]) for a person or an agent:
//! one request, a default place for the copy, and the rule that a sample library is never written to.
//!
//! * The copy goes to `out`, or by default to `<root>/sfz/derived/<library>/<name>_<tool>.sfz` (`<library>` is
//!   the folder under `instruments/` the source is in; sources elsewhere go straight into `sfz/derived/`).
//! * Nothing is written inside `<root>/instruments/` (libraries are installed and verified as downloaded, see
//!   `flux assets`), and a copy never replaces one of the files it is made from.
//!
//! [`Report::summary`] trims a report to what an agent needs (a retune report lists every region).

use crate::edit::{OutputFile, write_files};
use crate::error::{Error, Result};
use crate::even::{EvenOptions, EvenReport, even};
use crate::fsutil::{lexical_normalize, slash_path};
use crate::instrument::{Instrument, LoadOptions};
use crate::retune::{RegionAction, RetuneOptions, RetuneReport, retune};
use crate::tighten::{TightenOptions, TightenReport, tighten};
use crate::value::note_name;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Which tool, with its options.
#[derive(Clone, Debug)]
pub enum Tool {
    Tighten(TightenOptions),
    Retune(RetuneOptions),
    Even(EvenOptions),
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self {
            Tool::Tighten(_) => "tighten",
            Tool::Retune(_) => "retune",
            Tool::Even(_) => "even",
        }
    }
}

/// One derived copy to make.
#[derive(Clone, Debug)]
pub struct Request {
    pub tool: Tool,
    /// The SFZ file to start from (absolute, or relative to the root).
    pub source: PathBuf,
    /// Where the copy goes (absolute, or relative to the root); see [`default_output`] when `None`.
    pub out: Option<PathBuf>,
    /// The source is a fragment (a region map that a program `#include`s after setting `default_path`): read
    /// it as if it were in this folder, the one its sample paths are relative to. Retune only: the copy keeps
    /// the sample paths as written, so the program can include it instead of the original.
    pub base: Option<PathBuf>,
}

/// What a tool reported.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum Report {
    Tighten(TightenReport),
    Retune(RetuneReport),
    Even(EvenReport),
}

/// The result of [`run`].
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    /// The files written (absolute).
    pub written: Vec<PathBuf>,
    pub report: Report,
}

fn absolute_under(root: &Path, p: &Path) -> PathBuf {
    lexical_normalize(&root.join(p))
}

fn starts_with_ci(path: &Path, dir: &Path) -> bool {
    if cfg!(windows) {
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        lower(path).starts_with(lower(dir))
    } else {
        path.starts_with(dir)
    }
}

/// The folder of sample libraries: nothing is ever written in it.
pub fn library_dir(root: &Path) -> PathBuf {
    root.join("instruments")
}

/// Where a copy goes when no output is given: `<root>/sfz/derived/<library>/<name>_<tool>.sfz`.
pub fn default_output(root: &Path, source: &Path, tool: &str) -> PathBuf {
    let source = absolute_under(root, source);
    let stem = source.file_stem().map_or_else(|| "instrument".into(), |s| s.to_string_lossy().into_owned());
    let mut out = root.join("sfz").join("derived");
    let libs = library_dir(root);
    if starts_with_ci(&source, &libs)
        && let Some(lib) = source.components().nth(libs.components().count())
    {
        out.push(lib.as_os_str());
    }
    out.join(format!("{stem}_{tool}.sfz"))
}

/// Refuse an output path inside the library folder, or that is not an `.sfz` file.
pub fn check_output(root: &Path, out: &Path) -> Result<()> {
    let out = absolute_under(root, out);
    if starts_with_ci(&out, &library_dir(root)) {
        return Err(Error::Invalid(format!(
            "refusing to write {}: sample libraries in instruments/ are never changed (write the copy under sfz/, \
             by default sfz/derived/)",
            slash_path(&out)
        )));
    }
    if !out.extension().is_some_and(|e| e.eq_ignore_ascii_case("sfz")) {
        return Err(Error::Invalid(format!("{}: the copy must be an .sfz file", slash_path(&out))));
    }
    Ok(())
}

/// Read a fragment as if it were the file of the same name in `base`.
pub fn load_fragment(source: &Path, base: &Path) -> Result<Instrument> {
    let bytes = crate::error::read_file(source)?;
    let (text, _) = crate::text::decode(&bytes);
    let name = source.file_name().ok_or_else(|| Error::Invalid(format!("{}: not a file", source.display())))?;
    Instrument::from_text(&text, base.join(name), &LoadOptions::default())
}

/// Make the copy and write it. Paths in `request` may be relative to `root`.
pub fn run(root: &Path, request: &Request) -> Result<Outcome> {
    let source = absolute_under(root, &request.source);
    let out = match &request.out {
        Some(o) => absolute_under(root, o),
        None => default_output(root, &source, request.tool.name()),
    };
    check_output(root, &out)?;
    let inst = match (&request.base, &request.tool) {
        (Some(base), Tool::Retune(_)) => load_fragment(&source, &absolute_under(root, base))?,
        (Some(_), tool) => return Err(Error::Invalid(format!("base is for retune (fragments), not {}", tool.name()))),
        (None, _) => Instrument::load(&source)?,
    };
    let (files, report): (Vec<OutputFile>, Report) = match &request.tool {
        Tool::Tighten(o) => tighten(&inst, &out, o).map(|(f, r)| (f, Report::Tighten(r)))?,
        Tool::Retune(o) => {
            let mut o = o.clone();
            o.keep_sample_paths |= request.base.is_some();
            retune(&inst, &out, &o).map(|(f, r)| (f, Report::Retune(r)))?
        }
        Tool::Even(o) => even(&inst, &out, o).map(|(f, r)| (f, Report::Even(r)))?,
    };
    // derived files never land in the library, whatever the layout put where
    for f in &files {
        check_output(root, &f.path)?;
    }
    write_files(&files)?;
    Ok(Outcome { written: files.into_iter().map(|f| f.path).collect(), report })
}

/// How many regions or keys a summary lists.
const MAX_LISTED: usize = 10;

fn round(x: f64, digits: i32) -> Value {
    if x.is_finite() {
        let m = 10f64.powi(digits);
        json!((x * m).round() / m)
    } else {
        Value::Null
    }
}

impl Report {
    /// The report without per-region detail: counts, per-key results in one line each, and the regions worth
    /// a look (the largest changes, the ones that could not be measured).
    pub fn summary(&self, root: &Path) -> Value {
        let rel = |p: &Path| crate::fsutil::relative_path(root, p).filter(|r| !r.starts_with("..")).unwrap_or_else(|| slash_path(p));
        match self {
            Report::Tighten(r) => json!({
                "tool": "tighten", "source": rel(&r.source), "output": rel(&r.output), "release_s": r.release_s,
                "release_regions_removed": r.release_regions_removed, "release_opcodes_changed": r.release_opcodes_changed,
                "regions_kept": r.regions_kept,
            }),
            Report::Retune(r) => {
                let keys: serde_json::Map<String, Value> =
                    r.keys.iter().map(|k| (format!("{} ({})", note_name(k.key), k.key), round(k.median_cents, 1))).collect();
                let problems: Vec<Value> = r
                    .regions
                    .iter()
                    .filter_map(|t| {
                        let what = match &t.action {
                            RegionAction::Unmeasurable { cents: Some(c) } => format!("measured {c:+.1} cents: not trusted"),
                            RegionAction::Unmeasurable { cents: None } => "no pitch found".to_string(),
                            RegionAction::Unreadable { error } => error.clone(),
                            _ => return None,
                        };
                        Some(json!({"region": t.region, "key": t.key, "sample": t.sample.as_deref().map(rel), "problem": what}))
                    })
                    .collect();
                json!({
                    "tool": "retune", "source": rel(&r.source), "output": rel(&r.output),
                    "retuned": r.retuned, "left_as_is": r.left_as_is, "removed": r.removed,
                    "median_cents_by_key": keys,
                    "problems": problems.iter().take(MAX_LISTED).collect::<Vec<_>>(),
                    "problems_total": problems.len(),
                })
            }
            Report::Even(r) => {
                let spread: Vec<Value> = (0..r.velocities.len())
                    .map(|li| {
                        let v: Vec<f64> = r.keys.iter().filter_map(|k| k.levels_db.get(li)).copied().filter(|x| x.is_finite()).collect();
                        let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(*x), b.max(*x)));
                        round(if v.is_empty() { f64::NAN } else { hi - lo }, 2)
                    })
                    .collect();
                let mut largest: Vec<_> = r.regions.iter().collect();
                largest.sort_by(|a, b| b.change_db.abs().total_cmp(&a.change_db.abs()));
                let largest: Vec<Value> = largest
                    .iter()
                    .take(MAX_LISTED)
                    .map(|g| json!({"region": g.region, "key": format!("{} ({})", note_name(g.key), g.key), "velocity": r.velocities.get(g.layer), "change_db": round(g.change_db, 2)}))
                    .collect();
                json!({
                    "tool": "even", "source": rel(&r.source), "output": rel(&r.output),
                    "velocities": r.velocities, "targets_db": r.targets_db.iter().map(|t| round(*t, 2)).collect::<Vec<_>>(),
                    "spread_before_db": spread, "keys": r.keys.len(), "regions_changed": r.regions.len(),
                    "skipped": r.skipped, "release_opcodes_changed": r.release_opcodes_changed, "largest_changes": largest,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_and_guard() {
        let root = std::env::temp_dir().join("flux-sfz-derive-root");
        let lib_src = root.join("instruments/VSCO-2-CE/Harp.sfz");
        assert_eq!(default_output(&root, &lib_src, "tighten"), root.join("sfz/derived/VSCO-2-CE/Harp_tighten.sfz"));
        assert_eq!(default_output(&root, Path::new("sfz/x.sfz"), "even"), root.join("sfz/derived/x_even.sfz"));
        assert!(check_output(&root, Path::new("instruments/VSCO-2-CE/Harp_x.sfz")).is_err());
        assert!(check_output(&root, Path::new("sfz/../instruments/lib/a.sfz")).is_err());
        assert!(check_output(&root, Path::new("sfz/derived/a.txt")).is_err());
        assert!(check_output(&root, Path::new("sfz/derived/a.sfz")).is_ok());
        if cfg!(windows) {
            assert!(check_output(&root, Path::new("Instruments/lib/a.sfz")).is_err());
        }
    }

    #[test]
    fn tighten_into_the_default_place() {
        let root = std::env::temp_dir().join(format!("flux-sfz-derive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("instruments/lib")).unwrap();
        std::fs::write(root.join("instruments/lib/p.sfz"), "<group> ampeg_release=2\n<region> sample=a.wav\n<region> sample=r.wav trigger=release\n")
            .unwrap();
        let request = Request {
            tool: Tool::Tighten(TightenOptions { release_s: 0.05, note: None, sample_paths: Default::default() }),
            source: "instruments/lib/p.sfz".into(),
            out: None,
            base: None,
        };
        let outcome = run(&root, &request).unwrap();
        assert_eq!(outcome.written, vec![root.join("sfz/derived/lib/p_tighten.sfz")]);
        let text = std::fs::read_to_string(&outcome.written[0]).unwrap();
        assert!(text.contains("ampeg_release=0.05") && text.contains("../../../instruments/lib/"), "{text}");
        let s = outcome.report.summary(&root);
        assert_eq!(s["release_regions_removed"], 1);
        assert_eq!(s["output"], "sfz/derived/lib/p_tighten.sfz");

        let into_library = Request { out: Some("instruments/lib/p2.sfz".into()), ..request.clone() };
        assert!(run(&root, &into_library).unwrap_err().to_string().contains("never changed"));
        let with_base = Request { base: Some("instruments/lib".into()), ..request };
        assert!(run(&root, &with_base).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
