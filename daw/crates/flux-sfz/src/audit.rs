//! Path checks: samples that are missing or only found because the file system ignores case, includes that
//! were not read, and sample paths that make an instrument hard to move.
//!
//! This replaces `tools/check_sfz_paths.py`, which matched `sample=` lines with a regex and took the first
//! `default_path` of the file for all of them; here every region's sample is resolved the way sfizz resolves
//! it (see [`crate::instrument`]).

use crate::error::Location;
use crate::fsutil::FileStatus;
use crate::instrument::{Instrument, SampleRef};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The result of [`check_paths`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PathReport {
    pub file: PathBuf,
    pub regions: usize,
    /// Distinct sample files referenced.
    pub samples: usize,
    /// Files that do not exist: sfizz drops their regions.
    pub missing: Vec<SampleProblem>,
    /// Files whose name on disk differs in case: they play here but not on a case-sensitive system.
    pub case_mismatches: Vec<SampleProblem>,
    /// `#include`s that were not read (missing file, a cycle, or nested too deep).
    pub includes_not_read: Vec<IncludeProblem>,
    /// Samples given by an absolute path (`default_path` or `sample`): the instrument only works on this
    /// machine.
    pub absolute_samples: usize,
}

impl PathReport {
    /// True when every sample and include was found with the exact name written.
    pub fn is_ok(&self) -> bool {
        self.missing.is_empty() && self.case_mismatches.is_empty() && self.includes_not_read.is_empty()
    }
}

/// A sample file with a problem.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SampleProblem {
    /// `default_path` + `sample`, as sfizz looks it up.
    pub sample: String,
    pub path: PathBuf,
    /// For case mismatches: the file as named on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_disk: Option<PathBuf>,
    /// Regions that use it.
    pub regions: usize,
    /// The first of them.
    pub first_region: Location,
}

/// An include that was not read.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IncludeProblem {
    pub location: Location,
    pub path: String,
    /// `missing`, or `not_followed` (an include cycle, or nested deeper than allowed).
    pub reason: &'static str,
}

/// Check the paths of an instrument (loaded with sample checks on, the default).
pub fn check_paths(inst: &Instrument) -> PathReport {
    let mut problems: BTreeMap<PathBuf, (SampleProblem, bool)> = BTreeMap::new();
    let mut samples = 0;
    let mut absolute = 0;
    let mut seen = std::collections::HashSet::new();
    for r in inst.regions() {
        let SampleRef::File(f) = r.sample() else { continue };
        if seen.insert(f.path.clone()) {
            samples += 1;
            let b = f.value.as_bytes();
            if f.value.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
                absolute += 1;
            }
        }
        let (on_disk, missing) = match &f.status {
            FileStatus::Missing => (None, true),
            FileStatus::CaseMismatch { on_disk } => (Some(on_disk.clone()), false),
            FileStatus::Found | FileStatus::Unchecked => continue,
        };
        problems
            .entry(f.path.clone())
            .or_insert_with(|| {
                let p = SampleProblem {
                    sample: f.value.clone(),
                    path: f.path.clone(),
                    on_disk,
                    regions: 0,
                    first_region: r.location().clone(),
                };
                (p, missing)
            })
            .0
            .regions += 1;
    }
    let (missing, case): (Vec<_>, Vec<_>) = problems.into_values().partition(|(_, m)| *m);
    let includes_not_read = inst
        .includes()
        .iter()
        .filter(|i| i.target.is_none())
        .map(|i| {
            let candidate = inst.dir().join(&i.path);
            IncludeProblem {
                location: i.location.clone(),
                path: i.path.clone(),
                reason: if candidate.is_file() { "not_followed" } else { "missing" },
            }
        })
        .collect();
    PathReport {
        file: inst.path().to_path_buf(),
        regions: inst.regions().len(),
        samples,
        missing: missing.into_iter().map(|(p, _)| p).collect(),
        case_mismatches: case.into_iter().map(|(p, _)| p).collect(),
        includes_not_read,
        absolute_samples: absolute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_missing_samples_and_includes() {
        let d = std::env::temp_dir().join(format!("flux-sfz-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Samples")).unwrap();
        std::fs::write(d.join("Samples").join("A.wav"), b"x").unwrap();
        std::fs::write(d.join("loop.sfz"), "#include \"loop.sfz\"\n").unwrap();
        std::fs::write(
            d.join("i.sfz"),
            "<control> default_path=Samples/\n<region> sample=A.wav\n<region> sample=a.wav key=61\n\
             <region> sample=gone.wav\n<region> sample=gone.wav key=62\n<region> sample=*sine\n\
             #include \"nothere.sfz\"\n#include \"loop.sfz\"\n",
        )
        .unwrap();
        let inst = Instrument::load(d.join("i.sfz")).unwrap();
        let r = check_paths(&inst);
        assert_eq!((r.regions, r.samples), (5, 3));
        let gone = r.missing.iter().find(|m| m.sample == "Samples/gone.wav").unwrap();
        assert_eq!((gone.regions, gone.first_region.line), (2, 4));
        if cfg!(windows) {
            assert_eq!(r.missing.len(), 1);
            assert_eq!(r.case_mismatches.len(), 1);
            assert_eq!(r.case_mismatches[0].on_disk.as_deref(), Some(d.join("Samples").join("A.wav").as_path()));
        }
        let inc: Vec<(&str, &str)> = r.includes_not_read.iter().map(|i| (i.path.as_str(), i.reason)).collect();
        assert_eq!(inc, [("nothere.sfz", "missing"), ("loop.sfz", "not_followed")]);
        assert!(!r.is_ok());
        assert_eq!(r.absolute_samples, 0);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
