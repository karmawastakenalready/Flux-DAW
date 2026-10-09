//! A compact description of an instrument for an agent writing parts: what it plays, how to switch
//! articulations, which controllers do what, and what the samples are.
//!
//! [`describe`] gathers the facts ([`InstrumentInfo`], also serializable as JSON) and
//! [`InstrumentInfo::to_text`] writes them as a few short lines:
//!
//! ```text
//! violins.sfz: 131 regions, 131 samples (44100 Hz PCM_16 stereo WAV, 94.8 MB, 0.4-15.2 s)
//! keys: G3-D6 (55-86), 11 key centres
//! keyswitches 120-127 (default 120): 120 sus G3-D6, 121 trem G3-D6, 122 spic G3-D6, 123 pizz G3-D6
//! velocity: 2 layers, changing at 63; CC1 crossfades layers
//! round robin: 2 sequential, 2 random; release samples: none
//! controllers: CC1 modulation wheel (default 64): crossfade, cutoff, volume
//! loops: none
//! ```
//!
//! Lines are left out when they would say nothing (no keyswitches, no controllers, no problems); long lists
//! are cut after ten items (the JSON form has everything).

use crate::audio::SampleInfo;
use crate::instrument::{Instrument, SampleRef};
use crate::props::LoopMode;
use crate::ranges::{Ranges, Span, ranges};
use crate::samples::{SampleSummary, instrument_samples_with_infos};
use crate::value::note_name;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Everything [`describe`] found. See the [module documentation](self).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InstrumentInfo {
    pub path: PathBuf,
    pub ranges: Ranges,
    pub samples: SampleSummary,
    pub loops: Loops,
    /// Problems sfizz reports or that change what plays (missing samples, unknown headers...).
    pub errors: usize,
    pub warnings: usize,
    /// The first few errors, as `file:line:col: message`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub first_errors: Vec<String>,
}

/// How the playable regions loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Loops {
    /// Regions that loop while the key is held (`loop_continuous`, `loop_sustain`).
    pub looping_regions: usize,
    /// Of those, the ones that loop because the sample file has loop points and `loop_mode` is not set.
    pub from_sample_files: usize,
    /// Regions that play the whole sample whatever the note length (`loop_mode=one_shot`).
    pub one_shot_regions: usize,
}

const MAX_ERRORS: usize = 5;
/// How many items a line of [`InstrumentInfo::to_text`] lists before `...` (the JSON has them all).
const MAX_LIST: usize = 10;
/// How many uses of one controller the text lists.
const MAX_USES: usize = 4;

/// Describe an instrument (reads the headers of its samples).
pub fn describe(inst: &Instrument) -> InstrumentInfo {
    let (samples, infos) = instrument_samples_with_infos(inst);
    let errors: Vec<String> = inst.errors().map(ToString::to_string).collect();
    InstrumentInfo {
        path: inst.path().to_path_buf(),
        ranges: ranges(inst),
        samples,
        loops: loops(inst, &infos),
        errors: errors.len(),
        warnings: inst.diagnostics().len() - errors.len(),
        first_errors: errors.into_iter().take(MAX_ERRORS).collect(),
    }
}

fn loops(inst: &Instrument, infos: &HashMap<PathBuf, SampleInfo>) -> Loops {
    let mut l = Loops::default();
    for r in inst.playable_regions() {
        let file_loops = match r.sample() {
            SampleRef::File(f) => infos.get(&f.path).is_some_and(|i| !i.loops.is_empty()),
            _ => false,
        };
        match r.props().loop_mode {
            Some(LoopMode::LoopContinuous | LoopMode::LoopSustain) => l.looping_regions += 1,
            Some(LoopMode::OneShot) => l.one_shot_regions += 1,
            Some(LoopMode::NoLoop) => {}
            None if file_loops => {
                l.looping_regions += 1;
                l.from_sample_files += 1;
            }
            None => {}
        }
    }
    l
}

/// `C4 (60)` or `G3-D7 (55-98)`.
pub(crate) fn span_text(s: Span) -> String {
    if s.0 == s.1 { format!("{} ({})", note_name(s.0), s.0) } else { format!("{}-{} ({}-{})", note_name(s.0), note_name(s.1), s.0, s.1) }
}

pub(crate) fn spans_text(spans: &[Span]) -> String {
    if spans.is_empty() {
        return "none".into();
    }
    let shown: Vec<String> = spans.iter().take(MAX_LIST).map(|s| span_text(*s)).collect();
    let more = spans.len().saturating_sub(MAX_LIST);
    if more > 0 { format!("{} and {more} more spans", shown.join(", ")) } else { shown.join(", ") }
}

/// `1 region`, `3 regions`.
pub(crate) fn count(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

pub(crate) fn megabytes(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1_048_576.0)
}

impl InstrumentInfo {
    /// The description as short lines of text. See the [module documentation](self).
    pub fn to_text(&self) -> String {
        let r = &self.ranges;
        let s = &self.samples;
        let mut t = String::new();
        let name = self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let _ = write!(t, "{name}: {}", count(r.regions, "region"));
        if r.playable_regions != r.regions {
            let _ = write!(t, " ({} playable)", r.playable_regions);
        }
        let _ = write!(t, ", {}", count(s.files, "sample"));
        if let Some(f) = s.formats.first() {
            let ch = match f.channels {
                1 => "mono".to_owned(),
                2 => "stereo".to_owned(),
                n => format!("{n} channels"),
            };
            let format = match f.format {
                crate::audio::AudioFormat::Wav => "WAV",
                crate::audio::AudioFormat::Flac => "FLAC",
            };
            let _ = write!(t, " ({} Hz {} {ch} {format}", f.sample_rate, f.encoding);
            if s.formats.len() > 1 {
                let _ = write!(t, " and {}", count(s.formats.len() - 1, "other format"));
            }
            let _ = write!(t, ", {}", megabytes(s.total_bytes));
            if let Some(d) = s.duration_s {
                let _ = write!(t, ", {:.1}-{:.1} s", d.min, d.max);
            }
            t.push(')');
        }
        t.push('\n');

        let _ = writeln!(t, "keys: {}, {} key centres", spans_text(&r.keys), r.keycenters.len());

        if !r.keyswitches.is_empty() {
            t.push_str("keyswitches");
            if let Some(range) = r.keyswitch_range {
                let _ = write!(t, " {}-{}", range.0, range.1);
            }
            if let Some(d) = r.keyswitch_default {
                let _ = write!(t, " (default {d})");
            }
            t.push_str(": ");
            let items: Vec<String> = r
                .keyswitches
                .iter()
                .map(|k| {
                    let label = k.label.as_deref().unwrap_or("(unnamed)");
                    let keys = match (k.keys.first(), k.keys.last()) {
                        (Some(a), Some(b)) => format!("{}-{}", note_name(a.0), note_name(b.1)),
                        _ => "no keys".into(),
                    };
                    format!("{} {label} {keys}", k.key)
                })
                .collect();
            let _ = writeln!(t, "{}", items.join(", "));
        }
        if !r.held_keyswitches.is_empty() {
            let keys: Vec<String> = r.held_keyswitches.iter().map(|k| k.to_string()).collect();
            let _ = writeln!(t, "held keyswitches (sw_down/sw_up): {}", keys.join(", "));
        }

        let mut vel = match r.velocity_layers {
            0 | 1 => "velocity: 1 layer".to_owned(),
            n => {
                // where a new layer starts: the lower ends of the velocity ranges, above the lowest
                let lowest = r.velocity_splits.iter().map(|s| s.0).min().unwrap_or(0);
                let starts: BTreeSet<u8> = r.velocity_splits.iter().map(|s| s.0).filter(|v| *v > lowest).collect();
                let shown: Vec<String> = starts.iter().take(MAX_LIST).map(u8::to_string).collect();
                let more = if starts.len() > MAX_LIST { ", ..." } else { "" };
                match shown.len() {
                    0 => format!("velocity: {n} layers"),
                    1 => format!("velocity: {n} layers, changing at {}", shown[0]),
                    _ => format!("velocity: {n} layers, changing at {}{more}", shown.join(", ")),
                }
            }
        };
        if r.velocity_crossfades {
            vel.push_str(" (crossfaded)");
        }
        let xfade_ccs: Vec<String> =
            r.ccs.iter().filter(|c| c.uses.iter().any(|u| u == "crossfade")).map(|c| format!("CC{}", c.cc)).collect();
        if !xfade_ccs.is_empty() {
            let _ = write!(vel, "; {} crossfades layers", xfade_ccs.join("/"));
        }
        let _ = writeln!(t, "{vel}");

        let rr = match (r.round_robin.sequence, r.round_robin.random) {
            (s, n) if s > 1 && n > 1 => format!("{s} sequential, {n} random"),
            (s, _) if s > 1 => format!("{s} sequential"),
            (_, n) if n > 1 => format!("{n} random"),
            _ => "none".into(),
        };
        let rel = if r.release_regions > 0 { count(r.release_regions, "region") } else { "none".into() };
        let _ = write!(t, "round robin: {rr}; release samples: {rel}");
        if r.cc_triggered_regions > 0 {
            let _ = write!(t, "; {} started by controllers", count(r.cc_triggered_regions, "region"));
        }
        t.push('\n');

        if !r.ccs.is_empty() {
            let mut items: Vec<String> = r
                .ccs
                .iter()
                .take(MAX_LIST)
                .map(|c| {
                    let mut s = format!("CC{}", c.cc);
                    match (&c.label, c.standard) {
                        (Some(l), _) => {
                            let _ = write!(s, " \"{l}\"");
                        }
                        (None, Some(n)) => {
                            let _ = write!(s, " {n}");
                        }
                        _ => {}
                    }
                    if let Some(d) = c.default {
                        let _ = write!(s, " (default {d})");
                    }
                    if !c.uses.is_empty() {
                        let shown: Vec<&str> = c.uses.iter().take(MAX_USES).map(String::as_str).collect();
                        let _ = write!(s, ": {}{}", shown.join(", "), if c.uses.len() > MAX_USES { ", ..." } else { "" });
                    }
                    s
                })
                .collect();
            if r.ccs.len() > MAX_LIST {
                items.push(format!("and {} more", r.ccs.len() - MAX_LIST));
            }
            let _ = writeln!(t, "controllers: {}", items.join("; "));
        }

        let l = &self.loops;
        let mut loops = if l.looping_regions == 0 { "none".to_owned() } else { format!("{} loop", count(l.looping_regions, "region")) };
        if l.from_sample_files > 0 {
            let _ = write!(loops, " ({} from sample loop points)", l.from_sample_files);
        }
        if l.one_shot_regions > 0 {
            let _ = write!(loops, "; {} one-shot", count(l.one_shot_regions, "region"));
        }
        let _ = writeln!(t, "loops: {loops}");

        if s.missing > 0 || self.errors > 0 {
            let _ = write!(t, "problems: {}", count(self.errors, "error"));
            if s.missing > 0 {
                let _ = write!(t, ", {} (their regions are silent)", count(s.missing, "missing sample"));
            }
            if let Some(e) = self.first_errors.first() {
                let _ = write!(t, "; first: {e}");
            }
            t.push('\n');
        }
        t
    }
}

/// Describe the SFZ file at `path` (loads it).
pub fn describe_file(path: &Path) -> crate::Result<InstrumentInfo> {
    Ok(describe(&Instrument::load(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::LoadOptions;

    #[test]
    fn text_has_the_essentials() {
        let opts = LoadOptions { check_samples: false, ..LoadOptions::default() };
        let inst = Instrument::from_text(
            "<control> set_cc1=64\n<global> sw_lokey=24 sw_hikey=25 sw_default=24\n\
             <group> sw_last=24 sw_label=Sustain loop_mode=loop_continuous\n<region> sample=a.wav lokey=48 hikey=60 xfin_locc1=0 xfin_hicc1=127\n\
             <group> sw_last=25 sw_label=Pizz\n<region> sample=b.wav lokey=48 hikey=72 hivel=80\n<region> sample=c.wav lokey=48 hikey=72 lovel=81\n\
             <group> trigger=release\n<region> sample=d.wav lokey=48 hikey=72\n",
            std::env::temp_dir().join("flux-sfz-info.sfz"),
            &opts,
        )
        .unwrap();
        let info = describe(&inst);
        let text = info.to_text();
        assert!(text.contains("4 regions"), "{text}");
        assert!(text.contains("keys: C3-C5 (48-72)"), "{text}");
        assert!(text.contains("keyswitches 24-25 (default 24): 24 Sustain C3-C4, 25 Pizz C3-C5"), "{text}");
        assert!(text.contains("velocity: 2 layers, changing at 81; CC1 crossfades layers"), "{text}");
        assert!(text.contains("CC1 crossfades layers"), "{text}");
        assert!(text.contains("release samples: 1 region
"), "{text}");
        assert!(text.contains("CC1 modulation wheel (default 64): crossfade"), "{text}");
        assert!(text.contains("loops: 1 region loop"), "{text}");
        assert!(text.contains("problems: 0 errors, 4 missing samples"), "{text}");
        assert_eq!(text.lines().count(), 8, "{text}");
    }
}
