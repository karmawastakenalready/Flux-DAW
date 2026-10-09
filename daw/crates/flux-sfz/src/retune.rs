//! Retuned copies: every sample measured against the key it is mapped to, and a `tune=` correction written
//! into a copy of the instrument.
//!
//! Each sample is measured with [`PitchAnalyzer::measure_tuning`](crate::pitch::PitchAnalyzer::measure_tuning).
//! A region is left as it is when there is nothing to trust ([`Unmeasured`] says why): no harmonic of its key
//! stands out, the harmonics barely rise above the noise (release samples, pedal and key noise), they disagree,
//! or the correction would be larger than [`RetuneOptions::max_cents`] (probably another note).
//!
//! It replaces `tools/sfz_retune.py`. Differences:
//!
//! * The measurement. The Python tool took the strongest of harmonics 1-3, which on a low plucked note is
//!   usually the fundamental, pulled tens of cents off by the instrument's body: its corrections put low
//!   pizzicato notes out of tune. It also retuned noise samples to whatever peak it found. Here the median of
//!   up to ten harmonics decides, and noise is left alone.
//! * The region's key is its effective `pitch_keycenter` (inherited from a group, or set by `key=`; sfizz's
//!   default 60 when there is none, where the Python tool used `lokey`); `pitch_keycenter=sample` uses the
//!   root key stored in the sample file.
//! * By default the correction *replaces* the region's tune (`tune = -round(cents)`): the measurement is of
//!   the raw sample, so an existing tune that was itself a correction would otherwise be applied twice. Set
//!   [`RetuneOptions::keep_existing_tune`] to add it to the existing tune, as the Python tool did.
//! * Rounding is half to even, as Python's `round`.

use crate::audio::SampleInfo;
use crate::edit::{Derived, Layout, OutputFile, RegionEdit, SamplePaths};
use crate::error::{Error, Result};
use crate::instrument::{Instrument, SampleRef};
use crate::pitch::{PitchAnalyzer, Tuning, measure_sample_tuning};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Corrections larger than this many cents are not trusted (the measurement probably locked onto another
/// note): the region is left as it is.
pub const MAX_CENTS: f64 = 60.0;

/// Options of [`retune`].
#[derive(Clone, Debug)]
pub struct RetuneOptions {
    /// Keep only regions whose lowest key (`lokey`) is in this range; the others are removed.
    pub keys: Option<(u8, u8)>,
    pub max_cents: f64,
    /// Add the correction to the region's existing tune instead of replacing it.
    pub keep_existing_tune: bool,
    pub sample_paths: SamplePaths,
    /// Leave sample paths as written: for a fragment that a program including it gives a `default_path`
    /// (see [`Derived::keep_sample_paths`]).
    pub keep_sample_paths: bool,
}

impl Default for RetuneOptions {
    fn default() -> Self {
        RetuneOptions {
            keys: None,
            max_cents: MAX_CENTS,
            keep_existing_tune: false,
            sample_paths: SamplePaths::Relative,
            keep_sample_paths: false,
        }
    }
}

/// What happened to one region.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum RegionAction {
    Retuned { cents: f64, tune: f64 },
    /// Nothing to trust: left as it is.
    Unmeasurable {
        /// What was measured, if anything.
        #[serde(skip_serializing_if = "Option::is_none")]
        cents: Option<f64>,
        reason: Unmeasured,
    },
    /// The sample could not be read (missing, not audio): left as it is.
    Unreadable { error: String },
    /// Outside [`RetuneOptions::keys`]: removed.
    Removed,
    /// No sample file (a generator or nothing): left as it is.
    NoSample,
}

impl RegionAction {
    /// Why the region was left as it is (`None` if it was retuned or removed).
    pub fn left_because(&self) -> Option<&'static str> {
        match self {
            RegionAction::Retuned { .. } | RegionAction::Removed => None,
            RegionAction::Unmeasurable { reason, .. } => Some(reason.name()),
            RegionAction::Unreadable { .. } => Some("unreadable"),
            RegionAction::NoSample => Some("no_sample"),
        }
    }
}

/// Why a sample was not measured, or its measurement not trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unmeasured {
    /// `pitch_keycenter=sample` and the file stores no root key: nothing to measure against.
    NoKey,
    /// No harmonic of the key stands out (silence, a very short sample, or another note).
    NoPitch,
    /// The key's harmonics barely rise above the spectrum around them: noise rather than a note, or another note.
    Noisy,
    /// The harmonics disagree about the pitch.
    Inconsistent,
    /// Further from the key than [`RetuneOptions::max_cents`]: probably another note.
    TooFar,
}

impl Unmeasured {
    /// The name in reports (as serialized).
    pub fn name(self) -> &'static str {
        match self {
            Unmeasured::NoKey => "no_key",
            Unmeasured::NoPitch => "no_pitch",
            Unmeasured::Noisy => "noisy",
            Unmeasured::Inconsistent => "inconsistent",
            Unmeasured::TooFar => "too_far",
        }
    }

    /// The reason in a sentence, with what was measured.
    pub fn describe(self, cents: Option<f64>) -> String {
        let measured = cents.map_or_else(String::new, |c| format!(" (measured {c:+.1} cents)"));
        match self {
            Unmeasured::NoKey => "no key to measure against: pitch_keycenter=sample and the file stores no root key".into(),
            Unmeasured::NoPitch => "no harmonic of the key stands out".into(),
            Unmeasured::Noisy => format!("the key's harmonics barely stand out: noise, or another note{measured}"),
            Unmeasured::Inconsistent => format!("the harmonics disagree about the pitch{measured}"),
            Unmeasured::TooFar => format!("further from the key than max_cents, probably another note{measured}"),
        }
    }

    /// Whether a measurement is to be trusted, and if not, why.
    fn check(t: &Tuning, max_cents: f64) -> Option<Unmeasured> {
        if t.prominence_db < crate::pitch::MIN_PROMINENCE_DB {
            Some(Unmeasured::Noisy)
        } else if !t.trusted() {
            Some(Unmeasured::Inconsistent)
        } else if t.cents.abs() > max_cents {
            Some(Unmeasured::TooFar)
        } else {
            None
        }
    }
}

/// One region of a [`RetuneReport`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionTuning {
    pub region: usize,
    /// The key it was measured against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<PathBuf>,
    #[serde(flatten)]
    pub action: RegionAction,
}

/// The measurements of one key.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyTuning {
    pub key: u8,
    pub samples: usize,
    pub median_cents: f64,
    pub min_cents: f64,
    pub max_cents: f64,
}

/// What [`retune`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetuneReport {
    pub source: PathBuf,
    pub output: PathBuf,
    pub retuned: usize,
    pub left_as_is: usize,
    pub removed: usize,
    pub keys: Vec<KeyTuning>,
    pub regions: Vec<RegionTuning>,
}

impl RetuneReport {
    /// How many regions were left as they were, by [`RegionAction::left_because`].
    pub fn left_as_is_by_reason(&self) -> BTreeMap<&'static str, usize> {
        let mut by = BTreeMap::new();
        for r in &self.regions {
            if let Some(why) = r.action.left_because() {
                *by.entry(why).or_default() += 1;
            }
        }
        by
    }
}

/// Python's `round`: half to even.
fn round_half_even(x: f64) -> f64 {
    x.round_ties_even()
}

/// Measure every sample of `inst` and make a retuned copy to be written at `out` (nothing is written).
pub fn retune(inst: &Instrument, out: &Path, options: &RetuneOptions) -> Result<(Vec<OutputFile>, RetuneReport)> {
    if !(options.max_cents.is_finite() && options.max_cents > 0.0) {
        return Err(Error::Invalid("max_cents must be a positive number".into()));
    }
    let mut der = Derived::new(inst, out, Layout::Inline, options.sample_paths)?;
    if options.keep_sample_paths {
        der.keep_sample_paths();
    }
    let mut analyzer = PitchAnalyzer::new();
    let mut cache: HashMap<(PathBuf, u8), std::result::Result<Option<Tuning>, String>> = HashMap::new();
    let mut root_keys: HashMap<PathBuf, Option<u8>> = HashMap::new();
    let mut edits = vec![];
    let mut regions = vec![];
    let mut per_key: BTreeMap<u8, Vec<f64>> = BTreeMap::new();
    for r in inst.regions() {
        let p = r.props();
        let file = match r.sample() {
            SampleRef::File(f) => Some(f),
            _ => None,
        };
        let key = p.keycenter().or_else(|| {
            let f = file?;
            *root_keys.entry(f.path.clone()).or_insert_with(|| SampleInfo::read(&f.path).ok().and_then(|i| i.root_key))
        });
        let mut entry = RegionTuning { region: r.index(), key, sample: file.map(|f| f.path.clone()), action: RegionAction::NoSample };
        if let Some((lo, hi)) = options.keys
            && !(lo..=hi).contains(&p.lokey)
        {
            edits.push((r.index(), RegionEdit::Remove));
            entry.action = RegionAction::Removed;
            regions.push(entry);
            continue;
        }
        let (Some(f), Some(key)) = (file, key) else {
            if file.is_some() {
                entry.action = RegionAction::Unmeasurable { cents: None, reason: Unmeasured::NoKey };
            }
            regions.push(entry);
            continue;
        };
        let measured = cache
            .entry((f.path.clone(), key))
            .or_insert_with(|| measure_sample_tuning(&f.path, key, &mut analyzer).map_err(|e| e.to_string()))
            .clone();
        entry.action = match measured {
            Err(error) => RegionAction::Unreadable { error },
            Ok(None) => RegionAction::Unmeasurable { cents: None, reason: Unmeasured::NoPitch },
            Ok(Some(t)) => match Unmeasured::check(&t, options.max_cents) {
                Some(reason) => RegionAction::Unmeasurable { cents: Some(t.cents), reason },
                None => {
                    per_key.entry(key).or_default().push(t.cents);
                    let base = if options.keep_existing_tune { p.tune } else { 0.0 };
                    let tune = base - round_half_even(t.cents);
                    edits.push((r.index(), RegionEdit::Set { name: "tune".into(), value: format_number(tune) }));
                    RegionAction::Retuned { cents: t.cents, tune }
                }
            },
        };
        regions.push(entry);
    }
    der.edit_regions(&edits)?;
    der.set_header_comment(&[format!(
        "retuned copy of {} (flux-sfz retune): per-sample tune= measured against pitch_keycenter{}",
        der.source_label(),
        if options.keep_existing_tune { ", added to the existing tune" } else { "" }
    )]);
    let files = der.finish()?;
    let keys = per_key
        .into_iter()
        .map(|(key, mut v)| {
            v.sort_by(f64::total_cmp);
            let n = v.len();
            let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
            KeyTuning { key, samples: n, median_cents: median, min_cents: v[0], max_cents: v[n - 1] }
        })
        .collect();
    let count = |f: fn(&RegionAction) -> bool| regions.iter().filter(|r| f(&r.action)).count();
    let report = RetuneReport {
        source: inst.path().to_path_buf(),
        output: out.to_path_buf(),
        retuned: count(|a| matches!(a, RegionAction::Retuned { .. })),
        removed: count(|a| matches!(a, RegionAction::Removed)),
        left_as_is: count(|a| !matches!(a, RegionAction::Retuned { .. } | RegionAction::Removed)),
        keys,
        regions,
    };
    Ok((files, report))
}

/// A number as an SFZ value: no fraction when it is whole (`-7`, `12.5`).
pub(crate) fn format_number(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 { format!("{}", x as i64) } else { format!("{x}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::LoadOptions;

    /// A 16-bit mono WAV of a harmonic tone.
    fn tone_wav(path: &Path, hz: f64) {
        wav(path, |t| (0.5 * (2.0 * std::f64::consts::PI * hz * t).sin() + 0.2 * (4.0 * std::f64::consts::PI * hz * t).sin()) * 0.8);
    }

    /// A 16-bit mono WAV of a decaying noise burst.
    fn noise_wav(path: &Path) {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        wav(path, |t| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * (-4.0 * t).exp()
        });
    }

    /// A one-second, 16-bit mono WAV of `f(t)`.
    fn wav(path: &Path, mut f: impl FnMut(f64) -> f64) {
        let sr = 44100u32;
        let n = sr as usize;
        let mut data = vec![];
        for i in 0..n {
            let v = f(i as f64 / f64::from(sr));
            data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
        }
        let mut b = vec![];
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&sr.to_le_bytes());
        b.extend_from_slice(&(sr * 2).to_le_bytes());
        for v in [2u16, 16] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn measures_and_corrects() {
        let d = std::env::temp_dir().join(format!("flux-sfz-retune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("lib/s")).unwrap();
        // A4 played 20 cents sharp, C4 in tune, and a key far off
        tone_wav(&d.join("lib/s/a.wav"), 440.0 * 2f64.powf(20.0 / 1200.0));
        tone_wav(&d.join("lib/s/c.wav"), 261.6256);
        noise_wav(&d.join("lib/s/release.wav"));
        std::fs::write(
            d.join("lib/map.sfz"),
            "<control> default_path=s/\n<group> tune=5\n<region> sample=a.wav key=69\n<region> sample=c.wav lokey=59 hikey=61 pitch_keycenter=60\n\
             <region> sample=c.wav key=90\n<region> sample=gone.wav key=50\n<region> sample=release.wav key=60 trigger=release\n\
             <region> sample=c.wav key=63\n",
        )
        .unwrap();
        let inst = Instrument::load_with(d.join("lib/map.sfz"), &LoadOptions::default()).unwrap();
        let out = d.join("out/map.sfz");
        let (files, report) = retune(&inst, &out, &RetuneOptions { keys: Some((0, 80)), ..RetuneOptions::default() }).unwrap();
        assert_eq!((report.retuned, report.removed, report.left_as_is), (2, 1, 3));
        let RegionAction::Retuned { cents, tune } = report.regions[0].action else { panic!("{:?}", report.regions[0]) };
        assert!((cents - 20.0).abs() < 1.0, "{cents}");
        assert_eq!(tune, -20.0);
        assert!(matches!(report.regions[2].action, RegionAction::Removed));
        assert!(matches!(report.regions[3].action, RegionAction::Unreadable { .. }));
        // the noise is left alone, and so is a sample 300 cents from its key
        assert!(matches!(report.regions[4].action, RegionAction::Unmeasurable { reason: Unmeasured::Noisy, .. }), "{:?}", report.regions[4]);
        assert!(matches!(report.regions[5].action, RegionAction::Unmeasurable { .. }), "{:?}", report.regions[5]);
        let why = report.left_as_is_by_reason();
        assert_eq!((why.values().sum::<usize>(), why["unreadable"]), (3, 1), "{why:?}");
        let text = files[0].document.to_text();
        let copy = Instrument::from_text(&text, &out, &LoadOptions::default()).unwrap();
        assert_eq!(copy.regions().len(), 5);
        assert_eq!(copy.regions()[0].props().tune, -20.0);
        assert_eq!(copy.regions()[0].sample().file().unwrap().path, d.join("lib").join("s").join("a.wav"));

        // keeping the existing tune adds the group's 5 cents
        let keep = RetuneOptions { keep_existing_tune: true, ..RetuneOptions::default() };
        let (_, report) = retune(&inst, &out, &keep).unwrap();
        assert!(matches!(report.regions[0].action, RegionAction::Retuned { tune, .. } if tune == -15.0));
        assert_eq!((report.keys[0].key, report.keys[1].key, report.keys[1].samples), (60, 69, 1));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rounding_and_numbers() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(-3.5), -4.0);
        assert_eq!(format_number(-7.0), "-7");
        assert_eq!(format_number(12.5), "12.5");
    }
}
