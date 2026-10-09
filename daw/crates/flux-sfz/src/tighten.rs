//! Tight copies: a short release and no release-triggered samples, so the gaps between notes stay silent
//! (palm-muted parts through heavy amp gain).
//!
//! The port of `tools/sfz_tighten.py`, without its limits: the source may `#include` files and have
//! `<control>` blocks (the copy is one file with the includes inlined, see [`crate::edit`]), and regions are
//! removed one by one by their effective `trigger`, wherever it is set. As in the Python tool, every
//! `ampeg_release` opcode gets the new time and regions without one keep sfizz's default (0.001 s, already
//! shorter).

use crate::edit::{Derived, Layout, OpcodeEdit, OutputFile, RegionEdit, SamplePaths};
use crate::error::{Error, Result};
use crate::instrument::Instrument;
use crate::opcode::canonical_name;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Options of [`tighten`].
#[derive(Clone, Debug)]
pub struct TightenOptions {
    /// The release time in seconds: replaces every `ampeg_release` value.
    pub release_s: f64,
    /// Added to the provenance comment (credits, why).
    pub note: Option<String>,
    pub sample_paths: SamplePaths,
}

/// What [`tighten`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TightenReport {
    pub source: PathBuf,
    pub output: PathBuf,
    pub release_s: f64,
    /// Regions played on note-off, removed.
    pub release_regions_removed: usize,
    /// `ampeg_release` opcodes whose value was replaced.
    pub release_opcodes_changed: usize,
    pub regions_kept: usize,
}

/// Make a tight copy of `inst` to be written at `out`. Nothing is written: write the files with
/// [`crate::edit::write_files`].
pub fn tighten(inst: &Instrument, out: &Path, options: &TightenOptions) -> Result<(Vec<OutputFile>, TightenReport)> {
    if !(options.release_s.is_finite() && options.release_s >= 0.0) {
        return Err(Error::Invalid(format!("release {} s: give a time of 0 s or more", options.release_s)));
    }
    let release = format!("{}", options.release_s);
    let mut der = Derived::new(inst, out, Layout::Inline, options.sample_paths)?;
    let edits: Vec<(usize, RegionEdit)> =
        inst.regions().iter().filter(|r| r.props().is_release()).map(|r| (r.index(), RegionEdit::Remove)).collect();
    let removed = edits.len();
    der.edit_regions(&edits)?;
    let changed = der.edit_opcodes(|name, value| {
        if canonical_name(name) == "ampeg_release" && value.trim() != release {
            OpcodeEdit::Set(release.clone())
        } else {
            OpcodeEdit::Keep
        }
    });
    let mut comment = vec![format!("tight copy of {} (flux-sfz tighten)", der.source_label())];
    let mut second = format!("release {release} s, {removed} release-triggered region(s) removed.");
    if let Some(n) = options.note.as_deref().filter(|n| !n.trim().is_empty()) {
        second.push(' ');
        second.push_str(n.trim());
    }
    comment.push(second);
    der.set_header_comment(&comment);
    let files = der.finish()?;
    let report = TightenReport {
        source: inst.path().to_path_buf(),
        output: out.to_path_buf(),
        release_s: options.release_s,
        release_regions_removed: removed,
        release_opcodes_changed: changed,
        regions_kept: inst.regions().len() - removed,
    };
    Ok((files, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::LoadOptions;

    #[test]
    fn drops_release_regions_and_shortens_releases() {
        let d = std::env::temp_dir().join(format!("flux-sfz-tighten-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::write(
            d.join("lib/bass.sfz"),
            "<global> ampeg_release=0.5\n<group>\n<region> sample=a.wav key=40\n<region> sample=b.wav key=41 ampeg_release=2\n\
             <group> trigger=release\n<region> sample=r.wav key=40\n<group> ampeg_release=1.5\n<region> sample=c.wav key=42\n",
        )
        .unwrap();
        let opts = LoadOptions { check_samples: false, ..LoadOptions::default() };
        let inst = Instrument::load_with(d.join("lib/bass.sfz"), &opts).unwrap();
        let options = TightenOptions { release_s: 0.04, note: Some("CC0".into()), sample_paths: SamplePaths::Relative };
        let (files, report) = tighten(&inst, &d.join("out/tight.sfz"), &options).unwrap();
        assert_eq!((report.release_regions_removed, report.release_opcodes_changed, report.regions_kept), (1, 3, 3));
        let text = files[0].document.to_text();
        assert!(text.starts_with("// tight copy of "), "{text}");
        assert!(text.contains("release 0.04 s, 1 release-triggered region(s) removed. CC0"), "{text}");
        let tight = Instrument::from_text(&text, d.join("out/tight.sfz"), &opts).unwrap();
        assert_eq!(tight.regions().len(), 3);
        assert!(tight.regions().iter().all(|r| r.props().ampeg_release == 0.04 && !r.props().is_release()));
        let samples: Vec<_> = tight.regions().iter().map(|r| r.sample().file().unwrap().path.clone()).collect();
        assert_eq!(samples[0], d.join("lib").join("a.wav"));

        // regions without any release keep sfizz's shorter default
        std::fs::write(d.join("lib/plain.sfz"), "<region> sample=a.wav\n").unwrap();
        let inst = Instrument::load_with(d.join("lib/plain.sfz"), &opts).unwrap();
        let (files, report) = tighten(&inst, &d.join("out/plain.sfz"), &options).unwrap();
        assert_eq!(report.release_opcodes_changed, 0);
        assert!(!files[0].document.to_text().contains("ampeg_release"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
