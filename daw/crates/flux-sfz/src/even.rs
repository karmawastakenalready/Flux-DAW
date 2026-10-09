//! Evened copies: every key brought to the median level of its velocity layer, measured through sfizz.
//!
//! The port of `tools/sfz_even.py`. Each distinct `pitch_keycenter` is played at each velocity of
//! [`EvenOptions::velocities`] (one per velocity layer, softest first) and the RMS of the first
//! [`EvenOptions::window_s`] seconds is measured. A region is measured at the velocity where it plays at full
//! level: the first of the velocities inside its velocity range narrowed by its crossfades (above `xfin_hivel`,
//! below `xfout_lovel`), or the nearest one. Its `volume` then moves by the difference between the median
//! level of that velocity over all keys and its key's level, limited to [`EvenOptions::max_change_db`].
//!
//! Differences from the Python tool: the layer comes from the velocity ranges, not from the presence of
//! `xfin_lovel` in the region text (the same result for crossfaded two-layer maps such as VCSL's; hard velocity
//! splits are now measured in their own layer); the new volume is the region's *effective* volume plus the
//! change (an inherited group volume is kept); sound is cut between measurements unless
//! [`EvenOptions::reset_between_notes`] is off (the Python tool let each note's release ring into the next
//! measurement); and [`EvenOptions::release_s`] replaces every `ampeg_release`, not only those before the first
//! region.

use crate::edit::{Derived, Layout, OpcodeEdit, OutputFile, RegionEdit, SamplePaths};
use crate::error::{Error, Result};
use crate::instrument::Instrument;
use crate::opcode::canonical_name;
use crate::props::KeyCenter;
use flux_engine::sfizz::{Ev, Synth};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Options of [`even`].
#[derive(Clone, Debug)]
pub struct EvenOptions {
    /// One velocity per layer, softest first.
    pub velocities: Vec<u8>,
    /// Seconds of each note that are measured (from the note-on).
    pub window_s: f64,
    /// Largest volume change in dB, either way.
    pub max_change_db: f64,
    /// Replace every `ampeg_release` value with this many seconds.
    pub release_s: Option<f64>,
    pub sample_rate: u32,
    /// Cut all sound before each measurement (off reproduces the Python tool exactly).
    pub reset_between_notes: bool,
    pub sample_paths: SamplePaths,
}

impl Default for EvenOptions {
    fn default() -> Self {
        EvenOptions {
            velocities: vec![50, 120],
            window_s: 0.6,
            max_change_db: 12.0,
            release_s: None,
            sample_rate: 44_100,
            reset_between_notes: true,
            sample_paths: SamplePaths::Relative,
        }
    }
}

/// The measured levels of one key, one per velocity (dBFS RMS).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyLevels {
    pub key: u8,
    pub levels_db: Vec<f64>,
}

/// The change made to one region.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionLevel {
    pub region: usize,
    pub key: u8,
    /// The velocity (index into [`EvenOptions::velocities`]) it was measured at.
    pub layer: usize,
    pub level_db: f64,
    pub change_db: f64,
    /// The region's new `volume`.
    pub volume: f64,
}

/// What [`even`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvenReport {
    pub source: PathBuf,
    pub output: PathBuf,
    pub velocities: Vec<u8>,
    /// The median level of each velocity over all keys: what every key is brought to.
    pub targets_db: Vec<f64>,
    pub keys: Vec<KeyLevels>,
    pub regions: Vec<RegionLevel>,
    /// Regions left alone: release samples, controller triggers, missing samples, or a key centre taken from
    /// the sample file.
    pub skipped: usize,
    /// `ampeg_release` opcodes changed by [`EvenOptions::release_s`].
    pub release_opcodes_changed: usize,
}

/// The median (mean of the middle two for an even count), as `numpy.median`.
fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The index of the velocity a region is measured at: the first inside its full-level range, else the nearest.
fn layer_of(full: (u8, u8), velocities: &[u8]) -> usize {
    let (lo, hi) = full;
    if let Some(i) = velocities.iter().position(|v| (lo..=hi).contains(v)) {
        return i;
    }
    let distance = |v: u8| if v < lo { lo - v } else { v.saturating_sub(hi) };
    (0..velocities.len()).min_by_key(|&i| (distance(velocities[i]), i)).unwrap_or(0)
}

/// A synth playing `inst`: from its file when the file on disk is what was read, else from the text read.
fn synth_for(inst: &Instrument, sr: u32) -> Result<Synth> {
    let text = inst.files().first().map(|root| &root.document).filter(|doc| {
        std::fs::read(inst.path()).map_or(true, |on_disk| on_disk != doc.to_bytes())
    });
    let synth = match text {
        None => Synth::new(inst.path(), sr, 256),
        Some(doc) => Synth::from_string(&doc.to_text(), inst.path(), sr, 256),
    };
    synth.map_err(|e| Error::Render(format!("{e:#}")))
}

/// Measure `inst` through sfizz and make an evened copy to be written at `out` (nothing is written).
///
/// sfizz plays exactly what `inst` was read from: the file at [`Instrument::path`], or the text given to
/// [`Instrument::from_text`] (loaded as if it were at that path, so fragments and edited text work too).
pub fn even(inst: &Instrument, out: &Path, options: &EvenOptions) -> Result<(Vec<OutputFile>, EvenReport)> {
    if options.velocities.is_empty() || options.velocities.iter().any(|v| !(1..=127).contains(v)) {
        return Err(Error::Invalid("velocities must be 1..127, at least one".into()));
    }
    if !(options.window_s.is_finite() && options.window_s > 0.0 && options.window_s <= 60.0) {
        return Err(Error::Invalid("window_s must be between 0 and 60 seconds".into()));
    }
    if !(options.max_change_db.is_finite() && options.max_change_db >= 0.0) {
        return Err(Error::Invalid("max_change_db must be 0 or more".into()));
    }
    if options.release_s.is_some_and(|r| !(r.is_finite() && r >= 0.0)) {
        return Err(Error::Invalid("release_s must be 0 or more".into()));
    }
    // the regions to even, with their key and velocity layer
    let mut targets = vec![];
    let mut skipped = 0;
    for r in inst.regions() {
        let p = r.props();
        match p.pitch_keycenter {
            KeyCenter::Key(key) if r.is_playable() && p.is_note_on() && r.sample().file().is_some() => {
                let mut full = (p.lovel, p.hivel);
                if p.xfin_hivel > p.xfin_lovel {
                    full.0 = full.0.max(p.xfin_hivel);
                }
                if p.xfout_hivel > p.xfout_lovel {
                    full.1 = full.1.min(p.xfout_lovel);
                }
                targets.push((r.index(), key, layer_of(full, &options.velocities), p.volume));
            }
            _ => skipped += 1,
        }
    }
    let keys: BTreeSet<u8> = targets.iter().map(|t| t.1).collect();

    // measure every key at every velocity, in the Python tool's order
    let sr = options.sample_rate;
    let mut synth = synth_for(inst, sr)?;
    let window = (options.window_s * f64::from(sr)) as usize;
    let total = ((options.window_s + 0.2) * f64::from(sr)) as usize;
    let mut levels: BTreeMap<(u8, usize), f64> = BTreeMap::new();
    for &key in &keys {
        for (li, &vel) in options.velocities.iter().enumerate() {
            if options.reset_between_notes {
                synth.all_sound_off();
            }
            let (l, r) = synth.render(&[(10, Ev::On(key, vel)), (10 + window as u64, Ev::Off(key))], total);
            let end = (10 + window).min(l.len());
            let n = end.saturating_sub(10).max(1);
            let power: f64 = l[10..end]
                .iter()
                .zip(&r[10..end])
                .map(|(a, b)| {
                    let m = (f64::from(*a) + f64::from(*b)) / 2.0;
                    m * m
                })
                .sum::<f64>()
                / n as f64;
            levels.insert((key, li), 20.0 * (power.sqrt() + 1e-12).log10());
        }
    }
    let targets_db: Vec<f64> = (0..options.velocities.len())
        .map(|li| median(&mut keys.iter().map(|k| levels[&(*k, li)]).collect::<Vec<_>>()))
        .collect();

    let mut der = Derived::new(inst, out, Layout::Inline, options.sample_paths)?;
    let mut edits = vec![];
    let mut regions = vec![];
    for (region, key, layer, volume) in targets {
        let level_db = levels[&(key, layer)];
        let change_db = (targets_db[layer] - level_db).clamp(-options.max_change_db, options.max_change_db);
        let new = format!("{:.2}", volume + change_db);
        edits.push((region, RegionEdit::Set { name: "volume".into(), value: new.clone() }));
        regions.push(RegionLevel { region, key, layer, level_db, change_db, volume: new.parse().unwrap_or(volume) });
    }
    der.edit_regions(&edits)?;
    let mut release_opcodes_changed = 0;
    if let Some(rel) = options.release_s {
        let value = format!("{rel}");
        release_opcodes_changed = der.edit_opcodes(|name, v| {
            if canonical_name(name) == "ampeg_release" && v.trim() != value { OpcodeEdit::Set(value.clone()) } else { OpcodeEdit::Keep }
        });
    }
    der.set_header_comment(&[format!(
        "evened by flux-sfz from {} (per-key level matched to the median of each velocity layer)",
        der.source_label()
    )]);
    let files = der.finish()?;
    let report = EvenReport {
        source: inst.path().to_path_buf(),
        output: out.to_path_buf(),
        velocities: options.velocities.clone(),
        targets_db,
        keys: keys
            .iter()
            .map(|&key| KeyLevels { key, levels_db: (0..options.velocities.len()).map(|li| levels[&(key, li)]).collect() })
            .collect(),
        regions,
        skipped,
        release_opcodes_changed,
    };
    Ok((files, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_follow_full_level_ranges() {
        let v = [50, 120];
        // VCSL crossfaded pair: the soft layer fades out from 70 to 100, the loud one fades in
        assert_eq!(layer_of((0, 70), &v), 0);
        assert_eq!(layer_of((100, 127), &v), 1);
        // hard splits
        assert_eq!(layer_of((0, 63), &v), 0);
        assert_eq!(layer_of((64, 127), &v), 1);
        // no velocity inside: the nearest
        assert_eq!(layer_of((0, 30), &v), 0);
        assert_eq!(layer_of((125, 127), &v), 1);
        assert_eq!(layer_of((0, 127), &v), 0);
        assert_eq!(median(&mut [3.0, 1.0, 2.0, 10.0]), 2.5);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
    }
}
