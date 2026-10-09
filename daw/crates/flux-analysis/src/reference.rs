//! Reference matching (analysis only): how a mix differs from a reference track, as ranked, actionable
//! suggestions plus the numbers behind them, for the whole song and section by section.
//!
//! Everything is compared at equal loudness: band levels are loudness-normalised (see [`crate::profile`]), so a
//! reference mastered louder does not read as "brighter" or "bassier". Each difference is scored against a
//! just-noticeable step (1.5 dB of band balance, 3 LU of loudness range, 2 dB of peak-to-loudness ratio, 0.1 of
//! width, 3 dB of transient contrast, 6 dB of low-end side level, 1.5 LU of mono fold-down); suggestions with a
//! score of 1 or more are reported, highest first. When the mix's stems are given, band suggestions name the
//! stems that carry the band. A mix section compared with the whole reference (no reference section of the same
//! name) gets no loudness-range or peak-to-loudness advice: a section is always less dynamic than a whole song.

use crate::audio::AudioRef;
use crate::meta::{Section, check_sections};
use crate::num::median;
use crate::profile::{LOW_END_HZ, LOW_END_MATTERS_DB, Profile, SPECTRUM_N, profile};
use crate::spectral::{StereoSpectrum, third_octave_edges};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::Serialize;

/// What to compare.
#[derive(Clone, Debug, Default)]
pub struct ReferenceOptions {
    /// Sections of the mix to compare one by one (empty: the whole song only).
    pub mix_sections: Vec<Section>,
    /// Sections of the reference. A mix section is compared with the reference section of the same name
    /// (case-insensitive), or else with the whole reference.
    pub reference_sections: Vec<Section>,
    /// Most suggestions per section.
    pub section_suggestions: usize,
}

/// One suggestion.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Suggestion {
    /// Difference over its just-noticeable step (1 = just noticeable).
    pub score: f64,
    /// "balance", "dynamics", "transients", "width", "low_end" or "mono".
    pub area: &'static str,
    pub message: String,
    /// Frequency range concerned, Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_hz: Option<[f64; 2]>,
    /// Mix minus reference, in the area's unit (dB, LU or width).
    pub difference: f64,
    /// Stems carrying the range in the mix, loudest first, with their share of its power.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stems: Vec<StemShare>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StemShare {
    pub stem: String,
    pub share: f64,
}

/// Mix minus reference, per measure (`None` where either side has no value).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProfileDiff {
    pub lufs: Option<f64>,
    pub lra_lu: Option<f64>,
    pub plr_db: Option<f64>,
    pub crest_db: f64,
    /// Mix over reference onset rate.
    pub onset_rate_ratio: Option<f64>,
    pub transient_contrast_db: Option<f64>,
    pub transient_tail_db: Option<f64>,
    /// Per 1/3-octave band, smoothed over three neighbouring bands (about an octave).
    pub third_octaves_db: Vec<(String, Option<f64>)>,
    pub octave_width: Vec<(String, Option<f64>)>,
    pub low_end_side_db: Option<f64>,
    /// Mix over reference spectral centroid.
    pub centroid_ratio: Option<f64>,
    pub mud_db: Option<f64>,
    pub harsh_db: Option<f64>,
    pub air_db: Option<f64>,
    pub fold_down_lu: Option<f64>,
}

/// The whole-song comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub mix: Profile,
    pub reference: Profile,
    pub diff: ProfileDiff,
    pub suggestions: Vec<Suggestion>,
}

/// One section of the mix against its reference.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionComparison {
    pub section: String,
    pub start_s: f64,
    pub end_s: f64,
    /// The reference section compared with, or "whole".
    pub against: String,
    /// The section's loudness relative to its whole song, mix minus reference (only against a named reference
    /// section), LU.
    pub relative_loudness_lu: Option<f64>,
    pub diff: ProfileDiff,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReferenceReport {
    pub whole: Comparison,
    pub sections: Vec<SectionComparison>,
}

fn sub(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(a? - b?).filter(|v| v.is_finite())
}

/// Mix minus reference.
pub fn diff(mix: &Profile, rf: &Profile) -> ProfileDiff {
    let (m, r) = (&mix.dynamics, &rf.dynamics);
    let mut raw: Vec<(String, Option<f64>)> = mix
        .third_octaves
        .iter()
        .map(|b| {
            let other = rf.third_octaves.iter().find(|x| x.band == b.band).and_then(|x| x.level_db);
            // bands far under the loudness of either track carry nothing worth matching
            let audible = |v: Option<f64>| v.filter(|&x| x > -60.0);
            (b.band.clone(), sub(audible(b.level_db), audible(other)))
        })
        .collect();
    // Align the two balances on their median difference: at equal loudness a boosted region pushes every other
    // band down a little, and reading that as "everything else is low" would hide the one real difference.
    let present: Vec<f64> = raw.iter().filter_map(|b| b.1).collect();
    if !present.is_empty() {
        let mid = median(&present);
        for b in raw.iter_mut() {
            b.1 = b.1.map(|v| v - mid);
        }
    }
    let smooth: Vec<(String, Option<f64>)> = (0..raw.len())
        .map(|i| {
            let lo = i.saturating_sub(1);
            let hi = (i + 2).min(raw.len());
            let v: Vec<f64> = raw[lo..hi].iter().filter_map(|x| x.1).collect();
            let s = raw[i].1.and((!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64));
            (raw[i].0.clone(), s)
        })
        .collect();
    ProfileDiff {
        lufs: sub(m.lufs, r.lufs),
        lra_lu: sub(m.lra_lu, r.lra_lu),
        plr_db: sub(m.plr_db, r.plr_db),
        crest_db: m.crest_db - r.crest_db,
        onset_rate_ratio: (r.onsets_per_s > 0.0 && m.onsets_per_s > 0.0).then(|| m.onsets_per_s / r.onsets_per_s),
        transient_contrast_db: sub(m.transient_contrast_db, r.transient_contrast_db),
        transient_tail_db: sub(m.transient_tail_db, r.transient_tail_db),
        third_octaves_db: smooth,
        octave_width: mix
            .octaves
            .iter()
            .map(|b| (b.band.clone(), sub(b.width, rf.octaves.iter().find(|x| x.band == b.band).and_then(|x| x.width))))
            .collect(),
        low_end_side_db: sub(mix.mono.low_end_side_db, rf.mono.low_end_side_db),
        centroid_ratio: match (mix.tone.centroid_hz, rf.tone.centroid_hz) {
            (Some(a), Some(b)) if b > 0.0 => Some(a / b),
            _ => None,
        },
        mud_db: sub(mix.tone.mud_db, rf.tone.mud_db),
        harsh_db: sub(mix.tone.harsh_db, rf.tone.harsh_db),
        air_db: sub(mix.tone.air_db, rf.tone.air_db),
        fold_down_lu: sub(mix.mono.fold_down_lu, rf.mono.fold_down_lu),
    }
}

fn hz(f: f64) -> String {
    if f >= 1000.0 { format!("{:.1} kHz", f / 1000.0).replace(".0 kHz", " kHz") } else { format!("{f:.0} Hz") }
}

/// Names for the parts of the spectrum a range touches.
fn region_name(lo: f64, hi: f64) -> String {
    let names = [
        (20.0, 60.0, "sub"),
        (60.0, 200.0, "bass"),
        (200.0, 500.0, "low mids / mud"),
        (500.0, 2000.0, "mids"),
        (2000.0, 5000.0, "presence / harshness"),
        (5000.0, 10000.0, "brilliance"),
        (10000.0, 20000.0, "air"),
    ];
    let hit: Vec<&str> = names.iter().filter(|(a, b, _)| lo < *b && hi > *a).map(|x| x.2).collect();
    hit.join(", ")
}

/// Stems' share of the power in `[lo, hi)`.
fn stem_shares(stems: &[(String, StereoSpectrum)], lo: f64, hi: f64) -> Vec<StemShare> {
    let p: Vec<(String, f64)> = stems.iter().map(|(n, s)| (n.clone(), s.band(lo, hi).power())).collect();
    let tot: f64 = p.iter().map(|x| x.1).sum();
    if tot <= 1e-30 {
        return vec![];
    }
    let mut v: Vec<StemShare> = p.into_iter().map(|(stem, e)| StemShare { stem, share: (e / tot * 100.0).round() / 100.0 }).filter(|s| s.share >= 0.1).collect();
    v.sort_by(|a, b| b.share.total_cmp(&a.share));
    v.truncate(3);
    v
}

fn stem_text(v: &[StemShare]) -> String {
    if v.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = v.iter().map(|s| format!("{} {:.0} %", s.stem, s.share * 100.0)).collect();
    format!("; carried by {}", parts.join(", "))
}

/// Contiguous runs of 1/3-octave bands whose smoothed difference exceeds 1.5 dB with one sign.
fn balance_suggestions(mix: &Profile, d: &ProfileDiff, stems: &[(String, StereoSpectrum)]) -> Vec<Suggestion> {
    const STEP: f64 = 1.5;
    let mut out = vec![];
    let bands = &d.third_octaves_db;
    let mut i = 0;
    while i < bands.len() {
        let Some(v) = bands[i].1.filter(|v| v.abs() > STEP) else {
            i += 1;
            continue;
        };
        let sign = v.signum();
        let mut j = i;
        while j + 1 < bands.len() && bands[j + 1].1.is_some_and(|w| w.abs() > STEP && w.signum() == sign) {
            j += 1;
        }
        let vals: Vec<f64> = bands[i..=j].iter().filter_map(|b| b.1).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let (lo, _) = third_octave_edges(mix.third_octaves[i].centre_hz);
        let (_, hi) = third_octave_edges(mix.third_octaves[j].centre_hz);
        let octaves = (hi / lo).log2();
        let centre = (lo * hi).sqrt();
        // a wider region is easier to hear: weight by the square root of its width in octaves
        let score = mean.abs() / STEP * octaves.max(1.0 / 3.0).sqrt().max(0.75);
        if score < 1.0 {
            i = j + 1;
            continue;
        }
        let shares = stem_shares(stems, lo, hi);
        let what = if sign > 0.0 { "more" } else { "less" };
        let act = if sign > 0.0 { "cut" } else { "boost" };
        out.push(Suggestion {
            score,
            area: "balance",
            message: format!(
                "{}-{} ({}) has {:.1} dB {what} than the reference: {act} about {:.1} dB around {} with a broad EQ (about {:.1} octaves wide){}",
                hz(lo),
                hz(hi),
                region_name(lo, hi),
                mean.abs(),
                mean.abs(),
                hz(centre),
                octaves,
                stem_text(&shares)
            ),
            range_hz: Some([lo.round(), hi.round()]),
            difference: mean,
            stems: shares,
        });
        i = j + 1;
    }
    out
}

/// All suggestions, highest score first. `whole_song_dynamics` is false when a section is compared with a whole
/// reference song: a section always has less loudness range and a different peak-to-loudness ratio than a whole
/// song, so those comparisons would mislead.
fn suggestions(mix: &Profile, rf: &Profile, d: &ProfileDiff, stems: &[(String, StereoSpectrum)], whole_song_dynamics: bool) -> Vec<Suggestion> {
    let mut out = balance_suggestions(mix, d, stems);
    let mut push = |score: f64, area: &'static str, difference: f64, message: String, range_hz: Option<[f64; 2]>, stems: Vec<StemShare>| {
        if score >= 1.0 {
            out.push(Suggestion { score, area, message, range_hz, difference, stems });
        }
    };
    if let (true, Some(dl), Some(ml), Some(rl)) = (whole_song_dynamics, d.lra_lu, mix.dynamics.lra_lu, rf.dynamics.lra_lu) {
        let msg = if dl < 0.0 {
            format!("loudness range {ml:.1} LU against the reference's {rl:.1}: the mix is flatter; let sections differ more (less glue compression, quieter verses, bigger peaks)")
        } else {
            format!("loudness range {ml:.1} LU against the reference's {rl:.1}: the mix swings more; bring quiet sections up or compress the loud ones")
        };
        push(dl.abs() / 3.0, "dynamics", dl, msg, None, vec![]);
    }
    if let (true, Some(dp), Some(mp), Some(rp)) = (whole_song_dynamics, d.plr_db, mix.dynamics.plr_db, rf.dynamics.plr_db) {
        let msg = if dp < 0.0 {
            format!("peak-to-loudness ratio {mp:.1} dB against {rp:.1}: the mix is more limited and less punchy; lower the master drive or soften the bus compression")
        } else {
            format!("peak-to-loudness ratio {mp:.1} dB against {rp:.1}: the mix peaks more than the reference; tame the loudest hits (clip or compress percussion) if it should sound as dense")
        };
        push(dp.abs() / 2.0, "dynamics", dp, msg, None, vec![]);
    }
    if let Some(dc) = d.transient_contrast_db
        && dc < 0.0
    {
        push(
            -dc / 3.0,
            "transients",
            dc,
            format!(
                "transients stand out {:.1} dB less than in the reference: less reverb or a shorter pre-delay on percussive parts, slower compressor attacks, or transient shaping",
                -dc
            ),
            None,
            vec![],
        );
    }
    if let Some(dt) = d.transient_tail_db
        && dt > 0.0
    {
        push(
            dt / 3.0,
            "transients",
            dt,
            format!("hits ring {dt:.1} dB louder 100-150 ms after the attack than in the reference: shorter reverb decay or gated sustain on percussive parts"),
            None,
            vec![],
        );
    }
    if let Some(r) = d.onset_rate_ratio
        && (r.log2()).abs() >= 1.0
    {
        push(
            r.log2().abs(),
            "transients",
            r,
            format!(
                "{:.1} onsets per second against the reference's {:.1}: the arrangement is rhythmically {} (an arrangement choice, not a mix fault)",
                mix.dynamics.onsets_per_s,
                rf.dynamics.onsets_per_s,
                if r > 1.0 { "busier" } else { "sparser" }
            ),
            None,
            vec![],
        );
    }
    // width per octave above the low end, grouped into runs
    let widths: Vec<(String, f64, f64, Option<f64>)> = mix
        .octaves
        .iter()
        .zip(&d.octave_width)
        .filter_map(|(b, (_, dw))| crate::spectral::OCTAVES.iter().find(|o| o.0 == b.band).map(|o| (b.band.clone(), o.1, o.2, *dw)))
        .filter(|(_, lo, _, _)| *lo >= LOW_END_HZ)
        .collect();
    let mut i = 0;
    while i < widths.len() {
        let Some(v) = widths[i].3.filter(|v| v.abs() > 0.1) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < widths.len() && widths[j + 1].3.is_some_and(|w| w.abs() > 0.1 && w.signum() == v.signum()) {
            j += 1;
        }
        let vals: Vec<f64> = widths[i..=j].iter().filter_map(|w| w.3).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let (lo, hi) = (widths[i].1, widths[j].2);
        let shares = stem_shares(stems, lo, hi);
        push(
            mean.abs() / 0.1,
            "width",
            mean,
            format!(
                "{}-{} is {} than the reference (side share {:+.2}): {}{}",
                hz(lo),
                hz(hi),
                if mean > 0.0 { "wider" } else { "narrower" },
                mean,
                if mean > 0.0 { "narrow the stereo spread or pan these parts closer to the centre" } else { "pan parts apart, use stereo doubles or a wider reverb" },
                stem_text(&shares)
            ),
            Some([lo, hi]),
            shares,
        );
        i = j + 1;
    }
    if let (Some(ds), Some(ms)) = (d.low_end_side_db, mix.mono.low_end_side_db)
        && ds > 0.0
        && ms > -20.0
        && mix.mono.low_end_db.is_some_and(|l| l > LOW_END_MATTERS_DB)
    {
        let shares = stem_shares(stems, 20.0, LOW_END_HZ);
        push(
            ds.min(ms + 20.0) / 6.0,
            "low_end",
            ds,
            format!(
                "the low end is not mono: side {ms:.1} dB under mid below {LOW_END_HZ:.0} Hz ({ds:+.1} dB against the reference); mono the bass, kick and low pads below {LOW_END_HZ:.0} Hz{}",
                stem_text(&shares)
            ),
            Some([20.0, LOW_END_HZ]),
            shares,
        );
    }
    if let Some(df) = d.fold_down_lu
        && df < 0.0
    {
        push(
            -df / 1.5,
            "mono",
            df,
            format!(
                "the mix loses {:.1} LU in mono against the reference's {:.1}: check wide or phase-inverted parts (see the mix check's mono compatibility per band)",
                -mix.mono.fold_down_lu.unwrap_or(0.0),
                -rf.mono.fold_down_lu.unwrap_or(0.0)
            ),
            None,
            vec![],
        );
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    for s in out.iter_mut() {
        s.score = (s.score * 10.0).round() / 10.0;
        s.difference = (s.difference * 100.0).round() / 100.0;
    }
    out
}

/// Compare `mix` with `reference`. `stems` (named, the same length as the mix) let suggestions name the parts
/// that carry a frequency range.
pub fn compare(mix: AudioRef, reference: AudioRef, stems: &[(&str, AudioRef)], o: &ReferenceOptions) -> Result<ReferenceReport> {
    check_sections(&o.mix_sections, mix.duration_s()).context("mix sections")?;
    check_sections(&o.reference_sections, reference.duration_s()).context("reference sections")?;
    for (n, s) in stems {
        ensure!(s.sr == mix.sr, "stem '{n}' is {} Hz, the mix {} Hz", s.sr, mix.sr);
    }
    let (mp, rp) = rayon::join(|| profile(mix).context("profiling the mix"), || profile(reference).context("profiling the reference"));
    let (mp, rp) = (mp?, rp?);
    let stem_spectra = |range: Option<&Section>| -> Vec<(String, StereoSpectrum)> {
        stems
            .par_iter()
            .map(|(n, a)| {
                let a = range.map(|s| a.section(s)).unwrap_or(*a);
                (n.to_string(), StereoSpectrum::new(a, SPECTRUM_N))
            })
            .collect()
    };
    let d = diff(&mp, &rp);
    let whole_stems = stem_spectra(None);
    let sugg = suggestions(&mp, &rp, &d, &whole_stems, true);
    let limit = if o.section_suggestions == 0 { 5 } else { o.section_suggestions };
    let one = |s: &Section| -> Result<Option<SectionComparison>> {
        let ms = mix.section(s);
        if ms.len() < mix.sr as usize / 2 {
            return Ok(None);
        }
        let sp = profile(ms).with_context(|| format!("profiling section '{}'", s.name))?;
        let named = o.reference_sections.iter().find(|r| r.name.eq_ignore_ascii_case(&s.name));
        let (against, rsp) = match named {
            Some(r) => (r.name.clone(), profile(reference.section(r)).with_context(|| format!("profiling reference section '{}'", r.name))?),
            None => ("whole".to_string(), rp.clone()),
        };
        let relative_loudness_lu = named.and_then(|_| {
            let m = sub(sp.dynamics.lufs, mp.dynamics.lufs)?;
            let r = sub(rsp.dynamics.lufs, rp.dynamics.lufs)?;
            Some(((m - r) * 10.0).round() / 10.0)
        });
        let sd = diff(&sp, &rsp);
        let mut ss = suggestions(&sp, &rsp, &sd, &stem_spectra(Some(s)), named.is_some());
        ss.truncate(limit);
        Ok(Some(SectionComparison {
            section: s.name.clone(),
            start_s: s.start_s,
            end_s: s.end_s,
            against,
            relative_loudness_lu,
            diff: sd,
            suggestions: ss,
        }))
    };
    let sections: Vec<SectionComparison> =
        o.mix_sections.par_iter().map(one).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
    Ok(ReferenceReport { whole: Comparison { mix: mp, reference: rp, diff: d, suggestions: sugg }, sections })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::{noise, pink};

    /// A biquad peaking EQ (RBJ), for making a known balance difference.
    fn peaking(x: &[f32], sr: f64, f0: f64, gain_db: f64, q: f64) -> Vec<f32> {
        let a = 10f64.powf(gain_db / 40.0);
        let w = 2.0 * std::f64::consts::PI * f0 / sr;
        let alpha = w.sin() / (2.0 * q);
        let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * w.cos(), 1.0 - alpha * a);
        let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * w.cos(), 1.0 - alpha / a);
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        x.iter()
            .map(|&v| {
                let v = v as f64;
                let y = (b0 * v + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2) / a0;
                x2 = x1;
                x1 = v;
                y2 = y1;
                y1 = y;
                y as f32
            })
            .collect()
    }

    #[test]
    fn finds_a_boosted_band_and_names_its_stem() {
        let sr = 48000u32;
        let n = sr as usize * 12;
        let base = pink(n, 11);
        let other = pink(n, 12);
        // the mix: the reference's balance with +6 dB around 3 kHz in one stem, and at a different level
        let bright = peaking(&base, sr as f64, 3000.0, 9.0, 0.7);
        let mix: Vec<f32> = bright.iter().zip(&other).map(|(a, b)| 0.3 * (a + b)).collect();
        let reference: Vec<f32> = base.iter().zip(&other).map(|(a, b)| 0.8 * (a + b)).collect();
        let s1: Vec<f32> = bright.iter().map(|v| 0.3 * v).collect();
        let s2: Vec<f32> = other.iter().map(|v| 0.3 * v).collect();
        let m = AudioRef::new(sr, &mix, &mix).unwrap();
        let r = AudioRef::new(sr, &reference, &reference).unwrap();
        let stems = [("lead", AudioRef::new(sr, &s1, &s1).unwrap()), ("pad", AudioRef::new(sr, &s2, &s2).unwrap())];
        let o = ReferenceOptions { mix_sections: vec![Section::new("A", 0.0, 6.0), Section::new("B", 6.0, 12.0)], ..Default::default() };
        let rep = compare(m, r, &stems, &o).unwrap();
        let top = &rep.whole.suggestions[0];
        assert_eq!(top.area, "balance", "{:#?}", rep.whole.suggestions);
        assert!(top.difference > 2.0, "{top:?}");
        let [lo, hi] = top.range_hz.unwrap();
        assert!(lo < 3000.0 && hi > 3000.0, "{top:?}");
        assert_eq!(top.stems[0].stem, "lead");
        assert!(top.message.contains("cut"));
        // the mix is 8.5 dB lower before its boost: a loudness difference, not a balance difference
        let dl = rep.whole.diff.lufs.unwrap();
        assert!(dl < -4.0 && dl > -8.5, "{dl}");
        assert!(rep.whole.diff.third_octaves_db.iter().filter(|b| b.0 == "250" || b.0 == "125").all(|b| b.1.unwrap().abs() < 1.0));
        assert_eq!(rep.sections.len(), 2);
        assert_eq!(rep.sections[0].against, "whole");
        assert_eq!(rep.sections[1].suggestions[0].area, "balance");
    }

    #[test]
    fn dynamics_advice_needs_a_comparable_reference() {
        let sr = 48000u32;
        let n = sr as usize * 20;
        // the mix: 10 s 12 dB down, then 10 s full; the reference: steady
        let p = pink(n, 21);
        let mix: Vec<f32> = p.iter().enumerate().map(|(i, v)| if i < n / 2 { v * 0.25 } else { *v }).collect();
        let reference = pink(n, 22);
        let m = AudioRef::new(sr, &mix, &mix).unwrap();
        let r = AudioRef::new(sr, &reference, &reference).unwrap();
        let has_dynamics = |s: &[Suggestion]| s.iter().any(|x| x.area == "dynamics" && x.message.contains("loudness range"));
        // a section spanning the jump, compared with the whole reference: a section of a song is not a song, so no
        // loudness-range advice
        let o = ReferenceOptions { mix_sections: vec![Section::new("Bridge", 5.0, 15.0)], ..Default::default() };
        let rep = compare(m, r, &[], &o).unwrap();
        assert!(has_dynamics(&rep.whole.suggestions), "{:#?}", rep.whole.suggestions);
        assert_eq!(rep.sections[0].against, "whole");
        assert_eq!(rep.sections[0].relative_loudness_lu, None);
        assert!(!has_dynamics(&rep.sections[0].suggestions), "{:#?}", rep.sections[0].suggestions);
        // against the reference's own bridge (matched case-insensitively), it is
        let o = ReferenceOptions { reference_sections: vec![Section::new("bridge", 5.0, 15.0)], ..o };
        let rep = compare(m, r, &[], &o).unwrap();
        assert_eq!(rep.sections[0].against, "bridge");
        assert!(rep.sections[0].relative_loudness_lu.is_some());
        assert!(has_dynamics(&rep.sections[0].suggestions), "{:#?}", rep.sections[0].suggestions);
    }

    #[test]
    fn flags_a_wide_low_end_and_mono_loss() {
        let sr = 48000u32;
        let n = sr as usize * 8;
        let l = pink(n, 1);
        let r = pink(n, 2);
        let mono: Vec<f32> = l.clone();
        let m = AudioRef::new(sr, &l, &r).unwrap();
        let rf = AudioRef::new(sr, &mono, &mono).unwrap();
        let rep = compare(m, rf, &[], &ReferenceOptions::default()).unwrap();
        let areas: Vec<&str> = rep.whole.suggestions.iter().map(|s| s.area).collect();
        assert!(areas.contains(&"low_end") && areas.contains(&"width") && areas.contains(&"mono"), "{areas:?}");
        assert!(rep.sections.is_empty());
        assert!(compare(m, rf, &[], &ReferenceOptions { mix_sections: vec![Section::new("x", 20.0, 30.0)], ..Default::default() }).is_err());
        // white noise has next to no low end (0.5 % of its power under 120 Hz): nothing there worth making mono
        let (wl, wr) = (noise(n, 1), noise(n, 2));
        let rep = compare(AudioRef::new(sr, &wl, &wr).unwrap(), AudioRef::new(sr, &wl, &wl).unwrap(), &[], &ReferenceOptions::default()).unwrap();
        let areas: Vec<&str> = rep.whole.suggestions.iter().map(|s| s.area).collect();
        assert!(!areas.contains(&"low_end") && areas.contains(&"width"), "{areas:?}");
    }
}
