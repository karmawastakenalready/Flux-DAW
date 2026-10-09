//! The card sections computed from rendered notes: key map, per-key pitch and level, pitch summary, level
//! evenness, timbre, sustain and release, velocity and CC response, plus the additive sections (round robin,
//! velocity curve, CC curves, level holes, pitch slides).

use super::measure::{FRAME, PaddedSpectrum, SR, YinFrame, argmax, at, envelope, note_level, refine, subharmonic, timbre, yin_frames, yin_of};
use super::model::*;
use super::play::Play;
use super::sfztext::note_name;
use crate::json::OrderedMap;
use crate::num::{db_floor, median, percentile, rms, round_py, std_pop};

/// Flag thresholds (documented in the catalog README).
pub const HARSH: f64 = -35.0;
pub const SHARP: f64 = 12.0;
pub const UNEVEN: f64 = 6.0;
pub const DETUNED: f64 = 15.0;
pub const SHORT: f64 = 3.0;
pub const LATE: f64 = 25.0;
/// Additive thresholds: a key this much quieter than its neighbours is a level hole; a pitch drift this large is a
/// slide.
pub const HOLE_DB: f64 = 10.0;
pub const SLIDE_CENTS: f64 = 20.0;

fn r1(x: f64) -> f64 {
    round_py(x, 1)
}

// ------------------------------------------------------------------------------------------------ key map

/// Runs of consecutive keys, bridging holes of up to `bridge` keys.
pub fn runs(keys: &[i64], bridge: i64) -> Vec<[i64; 2]> {
    let mut out: Vec<[i64; 2]> = vec![];
    for &k in keys {
        match out.last_mut() {
            Some(last) if k - last[1] <= bridge + 1 => last[1] = k,
            _ => out.push([k, k]),
        }
    }
    out
}

/// The playable keys: the longest range (bridging 1-2 key holes), its gaps, and the keys outside it.
pub fn keymap(sounding: &[i64]) -> KeyMap {
    let ranges = runs(sounding, 0);
    let bridged = runs(sounding, 2);
    let mut main = bridged[0];
    for r in &bridged[1..] {
        if r[1] - r[0] > main[1] - main[0] {
            main = *r;
        }
    }
    let inside = |k: i64| main[0] <= k && k <= main[1];
    KeyMap {
        main,
        main_names: format!("{}-{}", note_name(main[0]), note_name(main[1])),
        count: sounding.len() as i64,
        span: [sounding[0], sounding[sounding.len() - 1]],
        gaps: (main[0]..=main[1]).filter(|k| !sounding.contains(k)).collect(),
        other_ranges: ranges.iter().filter(|a| a[1] - a[0] >= 2 && !inside(a[0])).copied().collect(),
        isolated: ranges.iter().filter(|a| a[1] - a[0] < 2 && !inside(a[0])).flat_map(|a| a[0]..=a[1]).collect(),
        ranges,
    }
}

// ------------------------------------------------------------------------------------------------ per key

/// Pitch and level of one key (velocity 90, held 0.7 s). With `drift`, also the pitch movement during the note.
pub fn key_detail(y: &[f64], k: i64, drift: bool) -> KeyDetail {
    let fk = 440.0 * 2f64.powf((k - 69) as f64 / 12.0);
    let frames = yin_frames(y, (fk / 8.0).max(25.0), (fk * 8.0).min(5000.0));
    let (mut f, ap) = yin_of(&frames);
    let spec = (f > 0.0).then(|| PaddedSpectrum::new(y));
    if let Some(s) = &spec {
        f = subharmonic(s, f);
        if f > 400.0 {
            f = refine(s, f);
        }
    }
    let mut d = KeyDetail { level_db: r1(note_level(y)), ap: round_py(ap, 2), ..Default::default() };
    if f > 0.0 {
        let midi = 69.0 + 12.0 * (f / 440.0).log2();
        let near = midi.round_ties_even();
        d.f0 = Some(round_py(f, 2));
        d.note = Some(near as i64);
        d.cents = Some(r1(100.0 * (midi - near)));
        if drift {
            d.drift_cents = drift_cents(&frames, f);
        }
    }
    d
}

/// How far the pitch moves during the note: the median pitch at 0.3-0.5 s minus the median at 0.04-0.2 s, in
/// cents, from the clearly pitched YIN frames within 20 dB of the note's loudest frame (a breath or bow noise
/// lead-in far under the note is inaudible as pitch, and YIN often reads it at a subharmonic). Each frame is
/// taken relative to the note's pitch `f0` and folded into the nearest octave first, so a frame read an octave
/// off counts as the pitch it belongs to instead of dragging a median halfway between two octaves.
fn drift_cents(frames: &[YinFrame], f0: f64) -> Option<f64> {
    let loud = frames.iter().map(|f| f.rms).fold(0.0f64, f64::max);
    let gate = loud * 0.1;
    let pick = |a: f64, b: f64| -> Vec<f64> {
        frames
            .iter()
            .filter(|f| f.start >= at(a) && f.start < at(b) && f.ap < 0.25 && f.hz > 0.0 && f.rms >= gate)
            .map(|f| {
                let c = 1200.0 * (f.hz / f0).log2();
                c - 1200.0 * (c / 1200.0).round()
            })
            .collect()
    };
    let (early, late) = (pick(0.04, 0.2), pick(0.3, 0.5));
    if early.len() < 2 || late.len() < 2 || f0 <= 0.0 {
        return None;
    }
    let c = median(&late) - median(&early);
    c.is_finite().then(|| r1(c))
}

// ------------------------------------------------------------------------------------------------ pitch

/// kind: pitched (one key-to-sounding offset), mixed (pitched, but the offset changes across the range, e.g. an
/// organ with several footages) or unpitched (drum / FX map).
pub fn pitch_section(per: &[(i64, KeyDetail)], slides: bool) -> Pitch {
    let mut sure: Vec<(i64, KeyDetail)> = per
        .iter()
        .filter(|(_, d)| d.note.is_some() && d.ap < 0.25 && d.level_db > -90.0)
        .map(|(k, d)| (*k, d.clone()))
        .collect();
    let mut out = Pitch { kind: "unpitched".into(), confident_keys: sure.len() as i64, ..Default::default() };
    if (sure.len() as f64) < 3f64.max(0.5 * per.len() as f64) {
        return out;
    }
    let off_of = |k: i64, d: &KeyDetail| d.note.unwrap_or(0) - k;
    // Counter.most_common(4): by count, ties in first-seen order
    let mut counts: Vec<(i64, i64)> = vec![];
    for (k, d) in &sure {
        let o = off_of(*k, d);
        match counts.iter_mut().find(|(x, _)| *x == o) {
            Some(c) => c.1 += 1,
            None => counts.push((o, 1)),
        }
    }
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    counts.truncate(4);
    let (off, n) = counts[0];
    let offsets_map = || counts.iter().map(|(o, c)| (o.to_string(), *c)).collect::<OrderedMap<i64>>();
    if n as f64 >= 0.5 * sure.len() as f64 {
        out.kind = "pitched".into();
        out.offset = Some(off);
        out.offset_share = Some(round_py(n as f64 / sure.len() as f64, 2));
        out.offset_outliers =
            Some(sure.iter().filter(|(k, d)| off_of(*k, d) != off).map(|(k, d)| (k.to_string(), off_of(*k, d))).collect());
        // a key a semitone off is badly tuned (or scoops up into its note), not mapped to another octave
        sure = sure
            .into_iter()
            .filter(|(k, d)| (off_of(*k, d) - off).abs() <= 1)
            .map(|(k, mut d)| {
                d.cents = Some(r1(100.0 * (off_of(k, &d) - off) as f64 + d.cents.unwrap_or(0.0)));
                (k, d)
            })
            .collect();
    } else if counts.len() > 1 && counts[1].1 >= 2 && (n + counts[1].1) as f64 >= 0.6 * sure.len() as f64 {
        out.kind = "mixed".into();
        out.offsets = Some(offsets_map());
    } else {
        // pitched-sounding drums whose pitch doesn't follow the keys
        out.offsets = Some(offsets_map());
        return out;
    }
    let cents = |d: &KeyDetail| d.cents.unwrap_or(0.0);
    let mut worst = 0;
    for (i, (_, d)) in sure.iter().enumerate() {
        if cents(d).abs() > cents(&sure[worst].1).abs() {
            worst = i;
        }
    }
    out.worst_cents = Some(cents(&sure[worst].1));
    out.worst_key = Some(sure[worst].0);
    out.median_abs_cents = Some(r1(median(&sure.iter().map(|(_, d)| cents(d).abs()).collect::<Vec<_>>())));
    out.out_of_tune =
        Some(sure.iter().filter(|(_, d)| cents(d).abs() > DETUNED).map(|(k, d)| (k.to_string(), cents(d))).collect());
    if slides {
        out.slides = Some(
            sure.iter()
                .filter_map(|(k, d)| d.drift_cents.filter(|c| c.abs() > SLIDE_CENTS).map(|c| (k.to_string(), c)))
                .collect(),
        );
    }
    out
}

// ------------------------------------------------------------------------------------------------ level

/// Level spread over the main range and the biggest jump between neighbouring keys.
pub fn level_section(per: &[(i64, KeyDetail)], main_keys: &[i64], holes: bool) -> Option<Level> {
    let lv: Vec<f64> =
        main_keys.iter().map(|k| per.iter().find(|(x, _)| x == k).map(|(_, d)| d.level_db)).collect::<Option<_>>()?;
    if lv.is_empty() {
        return None;
    }
    let mut out = Level {
        median_db: r1(median(&lv)),
        std_db: r1(std_pop(&lv)),
        min_db: r1(lv.iter().copied().fold(f64::INFINITY, f64::min)),
        max_db: r1(lv.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
        ..Default::default()
    };
    if lv.len() > 1 {
        let jumps: Vec<f64> = lv.windows(2).map(|w| w[1] - w[0]).collect();
        let abs: Vec<f64> = jumps.iter().map(|j| j.abs()).collect();
        let i = argmax(&abs);
        out.max_jump_db = Some(r1(abs[i]));
        out.max_jump_keys = Some([main_keys[i], main_keys[i + 1]]);
        out.jumps_over_6db = Some(abs.iter().filter(|j| **j > UNEVEN).count() as i64);
    }
    if holes {
        out.holes = Some(level_holes(main_keys, &lv));
    }
    Some(out)
}

/// Keys at least [`HOLE_DB`] under the median of the keys within 4 semitones (two or more on each side).
fn level_holes(keys: &[i64], lv: &[f64]) -> OrderedMap<f64> {
    let mut out = OrderedMap::new();
    for (i, &k) in keys.iter().enumerate() {
        let left: Vec<f64> = (0..i).filter(|&j| k - keys[j] <= 4).map(|j| lv[j]).collect();
        let right: Vec<f64> = (i + 1..keys.len()).filter(|&j| keys[j] - k <= 4).map(|j| lv[j]).collect();
        if left.len() < 2 || right.len() < 2 {
            continue;
        }
        let around = median(&[left, right].concat());
        if lv[i] < around - HOLE_DB {
            out.insert(k.to_string(), r1(around - lv[i]));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------ timbre

/// A few representative keys per octave (the sounding keys nearest to C, E and G#).
pub fn reps(keys: &[i64]) -> Vec<i64> {
    let mut octs: Vec<i64> = keys.iter().map(|k| k.div_euclid(12)).collect();
    octs.sort_unstable();
    octs.dedup();
    let mut out = vec![];
    for o in octs {
        let ks: Vec<i64> = keys.iter().copied().filter(|k| k.div_euclid(12) == o).collect();
        for t in [12 * o, 12 * o + 4, 12 * o + 8] {
            let mut best = ks[0];
            for &k in &ks[1..] {
                if (k - t).abs() < (best - t).abs() {
                    best = k;
                }
            }
            if !out.contains(&best) {
                out.push(best);
            }
        }
    }
    out
}

pub fn timbre_plays(keys: &[i64]) -> Vec<Play> {
    reps(keys).into_iter().map(|k| Play::new(k as u8, 70, 1.05)).collect()
}

fn med_of(ns: &[&TimbreNote]) -> TimbreMedian {
    let m = |get: &dyn Fn(&Timbre) -> Option<f64>, nd: usize| -> Option<f64> {
        let v: Vec<f64> = ns.iter().filter_map(|n| get(&n.timbre)).collect();
        let x = median(&v);
        x.is_finite().then(|| round_py(x, nd))
    };
    TimbreMedian {
        hi3k_db: m(&|t| Some(t.hi3k_db), 1),
        centroid_hz: m(&|t| Some(t.centroid_hz), 0),
        attack_db: m(&|t| Some(t.attack_db), 1),
        onset_ms: m(&|t| t.onset_ms, 1),
    }
}

/// Brightness and attack at velocity 70: per note, median over the core range, overall and per octave.
pub fn timbre_section(keys_played: &[i64], renders: &[Vec<f64>], core: [i64; 2]) -> TimbreSection {
    let notes: Vec<TimbreNote> = keys_played
        .iter()
        .zip(renders)
        .filter(|(_, y)| y.iter().fold(0f64, |m, v| m.max(v.abs())) > 1e-6)
        .map(|(&k, y)| TimbreNote { key: k, timbre: timbre(y) })
        .collect();
    let all: Vec<&TimbreNote> = notes.iter().collect();
    let med = |ns: &[&TimbreNote]| (!ns.is_empty()).then(|| med_of(ns));
    let mut octs: OrderedMap<Vec<&TimbreNote>> = OrderedMap::new();
    for n in &notes {
        let o = note_name(n.key.div_euclid(12) * 12);
        let mut v = octs.get(&o).cloned().unwrap_or_default();
        v.push(n);
        octs.insert(o, v);
    }
    let mut in_core: Vec<&TimbreNote> = notes.iter().filter(|n| core[0] <= n.key && n.key <= core[1]).collect();
    if in_core.is_empty() {
        in_core = all.clone();
    }
    TimbreSection {
        velocity: 70,
        main_keys: core,
        main: med(&in_core),
        median: med(&all),
        octaves: octs.iter().map(|(o, ns)| (o.to_string(), med_of(ns))).collect(),
        notes,
    }
}

// ------------------------------------------------------------------------------------------------ sustain

pub fn sustain_plays(key: i64) -> Vec<Play> {
    let mut release = Play::new(key as u8, 90, 1.0);
    release.tail = 10.0;
    release.stop = true;
    vec![Play::new(key as u8, 90, 6.0), release]
}

/// How long a held note lasts and how long its release rings (`renders` from [`sustain_plays`]).
pub fn sustain_section(key: i64, held: &[f64], release: &[f64]) -> Sustain {
    let mut rel = envelope(&held[..held.len().min(6 * SR as usize)]);
    let top = rel.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    rel.iter_mut().for_each(|v| *v -= top);
    let ip = argmax(&rel);
    let span = |t0: f64, t1: f64| {
        let (a, b) = ((t0 / FRAME) as usize, (t1 / FRAME) as usize);
        median(&rel[a.min(rel.len())..b.min(rel.len())])
    };
    let down = rel[ip..].iter().position(|v| *v < -40.0);
    let t40 = down.map(|d| round_py((ip + d) as f64 * FRAME, 2));
    let at2s = span(1.5, 2.5);
    let (kind, usable) = if at2s > -10.0 {
        // the recording has run out once the (100 ms smoothed) level stays 10 dB under its sustain level for good
        let sm = smooth5(&rel);
        let lvl = span(0.5, 2.5) - 10.0;
        let up = sm.iter().rposition(|v| *v >= lvl);
        let end = up.filter(|&u| u + 5 < rel.len()).map(|u| round_py((u + 1) as f64 * FRAME, 2));
        match end {
            None => ("sustained", None),
            Some(e) => ("sustained, sample ends", Some(e)),
        }
    } else {
        ("decaying", t40)
    };
    // release: hold 1 s, then the time until the tail is 60 dB under its level at the note-off
    let off = at(1.0);
    let y = release;
    let refl = db_floor(rms(&y[(off - at(0.05)).min(y.len())..off.min(y.len())]));
    let pk = db_floor(y[..off.min(y.len())].iter().fold(0f64, |m, v| m.max(v.abs())));
    let release_s = (refl > pk - 60.0).then(|| {
        let env = envelope(&y[off.min(y.len())..]);
        let above = env.iter().rposition(|v| *v > refl - 60.0);
        round_py(above.map_or(0.0, |a| (a + 1) as f64 * FRAME), 2)
    });
    Sustain {
        key,
        kind: kind.into(),
        peak_at_s: round_py(ip as f64 * FRAME, 2),
        t40_s: t40,
        level_at_2s_db: r1(at2s),
        usable_max_note_s: usable,
        release_s,
        note: "times from note-on; t40/usable None = still sounding after 6 s; release_s 10.0 = still sounding 10 s after \
               note-off, None = already silent at note-off"
            .into(),
    }
}

/// `np.convolve(x, np.ones(5) / 5, mode="same")`.
fn smooth5(x: &[f64]) -> Vec<f64> {
    let n = x.len() as isize;
    (0..n)
        .map(|i| {
            let mut s = 0.0;
            for j in i - 2..=i + 2 {
                if (0..n).contains(&j) {
                    s += x[j as usize] * 0.2;
                }
            }
            s
        })
        .collect()
}

// ------------------------------------------------------------------------------------------------ velocity / CC

pub const VELOCITIES: [u8; 3] = [30, 70, 110];

pub fn velocity_plays(key: i64) -> Vec<Play> {
    VELOCITIES.iter().map(|&v| Play::new(key as u8, v, 1.05)).collect()
}

/// Level and brightness at velocities 30, 70 and 110.
pub fn velocity_section(key: i64, renders: &[Vec<f64>]) -> Velocity {
    let mut levels = OrderedMap::new();
    for (v, y) in VELOCITIES.iter().zip(renders) {
        levels.insert(v.to_string(), VelocityLevel { rms_db: r1(note_level(y)), timbre: timbre(y) });
    }
    let (lo, hi) = (levels.get("30").cloned().unwrap_or_default(), levels.get("110").cloned().unwrap_or_default());
    let rng = hi.rms_db - lo.rms_db;
    let dh = hi.timbre.hi3k_db - lo.timbre.hi3k_db;
    let dc = hi.timbre.centroid_hz / lo.timbre.centroid_hz.max(1.0);
    let kind = if rng.abs() < 1.5 {
        "no velocity response"
    } else if dh.abs() >= 3.0 || !(0.83 < dc && dc < 1.2) {
        "level and timbre (layers or filter)"
    } else {
        "level only"
    };
    Velocity { key, levels, range_db: r1(rng), hi3k_change_db: r1(dh), kind: kind.into() }
}

pub fn cc_plays(key: i64) -> Vec<Play> {
    let mut out = vec![];
    for cc in [1u8, 11] {
        for v in [0u8, 127] {
            out.push(Play::new(key as u8, 90, 0.65).with_cc(cc, v));
        }
    }
    out
}

/// Level and brightness change from CC value 0 to 127, for CC1 and CC11 (`renders` from [`cc_plays`]).
pub fn cc_section(renders: &[Vec<f64>]) -> OrderedMap<CcResponse> {
    let mut out = OrderedMap::new();
    for (i, cc) in [1, 11].iter().enumerate() {
        let (y0, y1) = (&renders[2 * i], &renders[2 * i + 1]);
        let (l0, l1) = (note_level(y0), note_level(y1));
        if l0 < -90.0 {
            out.insert(
                cc.to_string(),
                CcResponse { level_change_db: None, hi3k_change_db: None, silent_at_0: true, changes_level: true, changes_timbre: None },
            );
            continue;
        }
        let dl = l1 - l0;
        let dh = timbre(y1).hi3k_db - timbre(y0).hi3k_db;
        out.insert(
            cc.to_string(),
            CcResponse {
                level_change_db: Some(r1(dl)),
                hi3k_change_db: Some(r1(dh)),
                silent_at_0: false,
                changes_level: dl.abs() > 1.0,
                changes_timbre: Some(dh.abs() > 2.0),
            },
        );
    }
    out
}

// ------------------------------------------------------------------------------------------------ additive sections

pub const RR_REPEATS: usize = 6;

pub fn round_robin_plays(key: i64) -> Vec<Play> {
    (0..RR_REPEATS).map(|_| Play::new(key as u8, 90, 0.7)).collect()
}

/// Normalised correlation of two renders over their first 0.5 s (two silent renders count as identical).
fn similarity(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len()).min(at(0.5));
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        ab += a[i] * b[i];
        aa += a[i] * a[i];
        bb += b[i] * b[i];
    }
    if aa < 1e-18 && bb < 1e-18 {
        return 1.0;
    }
    ab / (aa * bb).sqrt().max(1e-30)
}

/// The same note repeated: how many different recordings it cycles through, and how much they differ.
pub fn round_robin_section(key: i64, renders: &[Vec<f64>], random: bool) -> RoundRobin {
    const SAME: f64 = 0.999;
    let levels: Vec<f64> = renders.iter().map(|y| r1(note_level(y))).collect();
    let onsets: Vec<Option<f64>> = renders.iter().map(|y| timbre(y).onset_ms).collect();
    let spread = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max) - v.iter().copied().fold(f64::INFINITY, f64::min);
    let on: Vec<f64> = onsets.iter().flatten().copied().collect();
    let mut reps: Vec<usize> = vec![];
    for (i, y) in renders.iter().enumerate() {
        if !reps.iter().any(|&j| similarity(&renders[j], y) >= SAME) {
            reps.push(i);
        }
    }
    let n = renders.len();
    let cycle = (1..n).find(|&p| (0..n - p).all(|i| similarity(&renders[i], &renders[i + p]) >= SAME)).map(|p| p as i64);
    RoundRobin {
        key,
        velocity: 90,
        level_spread_db: r1(spread(&levels)),
        levels_db: levels,
        onset_spread_ms: (!on.is_empty()).then(|| r1(spread(&on))),
        onsets_ms: onsets,
        distinct: reps.len() as i64,
        cycle,
        random,
    }
}

pub const CURVE_VELOCITIES: [u8; 9] = [1, 16, 32, 48, 64, 80, 96, 112, 127];

pub fn velocity_curve_plays(key: i64) -> Vec<Play> {
    CURVE_VELOCITIES.iter().map(|&v| Play::new(key as u8, v, 0.65)).collect()
}

fn level_and_hi3k(y: &[f64]) -> (f64, Option<f64>) {
    let l = note_level(y);
    (r1(l), (l > -90.0).then(|| timbre(y).hi3k_db))
}

pub fn velocity_curve_section(key: i64, renders: &[Vec<f64>]) -> VelocityCurve {
    let (rms_db, hi3k_db): (Vec<f64>, Vec<Option<f64>>) = renders.iter().map(|y| level_and_hi3k(y)).unzip();
    let monotonic = rms_db.windows(2).all(|w| w[1] >= w[0] - 0.5);
    let range_db = r1(rms_db.last().copied().unwrap_or(0.0) - rms_db.first().copied().unwrap_or(0.0));
    VelocityCurve { key, velocities: CURVE_VELOCITIES.iter().map(|&v| v as i64).collect(), rms_db, hi3k_db, monotonic, range_db }
}

/// CC values sampled by the CC curves: 0, 8, ..., 120, 127 plus 70 (the default) and 100.
pub fn cc_curve_values() -> Vec<u8> {
    let mut v: Vec<u8> = (0..=120).step_by(8).collect();
    v.extend([70, 100, 127]);
    v.sort_unstable();
    v.dedup();
    v
}

pub const CURVE_CCS: [u8; 2] = [1, 11];

pub fn cc_curve_plays(key: i64) -> Vec<Play> {
    let mut out = vec![];
    for cc in CURVE_CCS {
        for v in cc_curve_values() {
            out.push(Play::new(key as u8, 90, 0.65).with_cc(cc, v));
        }
    }
    out
}

pub fn cc_curves_section(key: i64, renders: &[Vec<f64>]) -> OrderedMap<CcCurve> {
    let values = cc_curve_values();
    let mut out = OrderedMap::new();
    for (i, cc) in CURVE_CCS.iter().enumerate() {
        let part = &renders[i * values.len()..(i + 1) * values.len()];
        let (rms_db, hi3k_db): (Vec<f64>, Vec<Option<f64>>) = part.iter().map(|y| level_and_hi3k(y)).unzip();
        let top = rms_db.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let silent = |l: f64| l < -90.0 || l < top - 40.0;
        let audible = |l: f64| l > -90.0 && l >= top - 30.0;
        let silent_at: Vec<i64> = values.iter().zip(&rms_db).filter(|(_, l)| silent(**l)).map(|(v, _)| *v as i64).collect();
        let dropout = (0..rms_db.len()).any(|a| audible(rms_db[a]) && rms_db[a + 1..].iter().any(|l| silent(*l)));
        out.insert(
            cc.to_string(),
            CcCurve { key, values: values.iter().map(|&v| v as i64).collect(), rms_db, hi3k_db, silent_at, dropout },
        );
    }
    out
}

// ------------------------------------------------------------------------------------------------ test note choice

/// The test note of a tonal instrument: the middle of the middle half of the main range. Returns `(core, mid)`.
pub fn core_and_mid(main_keys: &[i64]) -> ([i64; 2], i64) {
    let f: Vec<f64> = main_keys.iter().map(|&k| k as f64).collect();
    let (q1, q3) = (percentile(&f, 25.0), percentile(&f, 75.0));
    let mut core: Vec<i64> = main_keys.iter().copied().filter(|&k| q1 <= k as f64 && k as f64 <= q3).collect();
    if core.is_empty() {
        core = main_keys.to_vec();
    }
    let m = median(&core.iter().map(|&k| k as f64).collect::<Vec<_>>());
    let mut mid = core[0];
    for &k in &core[1..] {
        if (k as f64 - m).abs() < (mid as f64 - m).abs() {
            mid = k;
        }
    }
    ([core[0], core[core.len() - 1]], mid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_ignores_octave_errors_and_finds_slides() {
        let fr = |t: f64, hz: f64| YinFrame { start: at(t), hz, ap: 0.05, rms: 0.1 };
        // breathy attack read an octave low, then a steady note slightly sharp later on
        let frames = [fr(0.05, 393.5), fr(0.08, 393.5), fr(0.11, 787.0), fr(0.15, 787.0), fr(0.35, 800.0), fr(0.4, 800.0), fr(0.45, 800.0)];
        let d = drift_cents(&frames, 787.0).unwrap();
        assert!((d - 1200.0 * (800f64 / 787.0).log2()).abs() < 0.2, "{d}");
        assert_eq!(drift_cents(&frames[..3], 787.0), None, "needs two frames in each window");
        // a quiet lead-in read at a third of the pitch (a fifth off after folding) is ignored
        let quiet = |t: f64| YinFrame { rms: 0.005, ..fr(t, 787.0 / 3.0) };
        let frames = [quiet(0.05), quiet(0.08), quiet(0.11), fr(0.14, 787.0), fr(0.17, 787.0), fr(0.35, 787.0), fr(0.4, 787.0)];
        assert_eq!(drift_cents(&frames, 787.0), Some(0.0));
        // a sine gliding from 440 to 466.16 Hz (one semitone) between 0.2 and 0.3 s
        let n = at(0.7);
        let mut phase = 0.0;
        let y: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / SR;
                let f = 440.0 * 2f64.powf(((t - 0.2) / 0.1).clamp(0.0, 1.0) / 12.0);
                phase += 2.0 * std::f64::consts::PI * f / SR;
                0.3 * phase.sin()
            })
            .collect();
        let k = key_detail(&y, 69, true);
        let d = k.drift_cents.unwrap();
        assert!((d - 100.0).abs() < 5.0, "{d}");
        assert_eq!(key_detail(&y, 69, false).drift_cents, None);
    }

    #[test]
    fn keymap_bridges_small_holes() {
        let mut s: Vec<i64> = (40..=60).filter(|k| *k != 50 && *k != 51).collect();
        s.extend([24, 25, 26, 30, 90]);
        s.sort();
        let m = keymap(&s);
        assert_eq!(m.main, [40, 60]);
        assert_eq!(m.main_names, "E2-C4");
        assert_eq!(m.gaps, vec![50, 51]);
        assert_eq!(m.other_ranges, vec![[24, 26]]);
        assert_eq!(m.isolated, vec![30, 90]);
        assert_eq!(m.count, s.len() as i64);
    }

    #[test]
    fn representative_keys() {
        let keys: Vec<i64> = (40..=64).collect();
        assert_eq!(reps(&keys), vec![40, 44, 48, 52, 56, 60, 64]);
    }

    #[test]
    fn core_range_and_test_note() {
        let keys: Vec<i64> = (40..=86).collect();
        let (core, mid) = core_and_mid(&keys);
        assert_eq!(core, [52, 74]);
        assert_eq!(mid, 63);
    }

    fn kd(level: f64, note: Option<i64>, cents: f64, ap: f64) -> KeyDetail {
        KeyDetail { level_db: level, ap, note, cents: note.map(|_| cents), f0: note.map(|_| 1.0), drift_cents: None }
    }

    #[test]
    fn pitch_offsets_and_tuning() {
        // an instrument an octave up, with one detuned key and one octave outlier
        let mut per: Vec<(i64, KeyDetail)> = (48..=60).map(|k| (k, kd(-30.0, Some(k + 12), 2.0, 0.05))).collect();
        per[3].1.cents = Some(-30.0);
        per[5].1.note = Some(per[5].0 + 24);
        per[6].1.note = Some(per[6].0 + 11); // a semitone flat: badly tuned, folded into the offset
        per[6].1.cents = Some(40.0);
        let p = pitch_section(&per, false);
        assert_eq!(p.kind, "pitched");
        assert_eq!(p.offset, Some(12));
        assert_eq!(p.offset_outliers.as_ref().unwrap().len(), 2);
        assert_eq!(p.worst_cents, Some(-60.0));
        assert_eq!(p.worst_key, Some(54));
        assert_eq!(p.out_of_tune.as_ref().unwrap().len(), 2);
        let drums: Vec<(i64, KeyDetail)> = (36..=40).map(|k| (k, kd(-20.0, None, 0.0, 0.9))).collect();
        assert_eq!(pitch_section(&drums, false).kind, "unpitched");
    }

    #[test]
    fn level_jumps_and_holes() {
        let keys: Vec<i64> = (28..=40).collect();
        let mut per: Vec<(i64, KeyDetail)> = keys.iter().map(|&k| (k, kd(-50.0, None, 0.0, 1.0))).collect();
        for i in 2..5 {
            per[i].1.level_db = -71.0;
        }
        let l = level_section(&per, &keys, true).unwrap();
        assert_eq!(l.max_jump_db, Some(21.0));
        assert_eq!(l.jumps_over_6db, Some(2));
        let holes = l.holes.unwrap();
        assert_eq!(holes.keys().collect::<Vec<_>>(), vec!["30", "31", "32"]);
        assert_eq!(holes.get("31"), Some(&21.0));
    }

    #[test]
    fn smoothing_matches_numpy_same_mode() {
        let x = [5.0, 0.0, 0.0, 0.0, 0.0, 10.0];
        let s = smooth5(&x);
        assert_eq!(s.len(), 6);
        assert!((s[0] - 1.0).abs() < 1e-12 && (s[2] - 1.0).abs() < 1e-12 && (s[3] - 2.0).abs() < 1e-12 && (s[5] - 2.0).abs() < 1e-12);
    }

    #[test]
    fn cc_curve_dropout() {
        let values = cc_curve_values();
        assert_eq!(values.len(), 19);
        let n = values.len();
        let mut renders = vec![];
        for cc in 0..2 {
            for (i, v) in values.iter().enumerate() {
                let amp = if cc == 0 && *v >= 100 { 0.0 } else if cc == 1 { i as f64 / n as f64 * 0.3 } else { 0.3 };
                renders.push(vec![amp; at(0.7)]);
            }
        }
        let c = cc_curves_section(60, &renders);
        let c1 = c.get("1").unwrap();
        assert!(c1.dropout);
        assert_eq!(c1.silent_at, vec![100, 104, 112, 120, 127]);
        let c11 = c.get("11").unwrap();
        assert!(!c11.dropout, "{c11:?}");
        assert_eq!(c11.silent_at, vec![0]);
    }

    #[test]
    fn round_robin_cycles() {
        let a: Vec<f64> = (0..30000).map(|i| (i as f64 * 0.01).sin()).collect();
        let b: Vec<f64> = (0..30000).map(|i| (i as f64 * 0.013).sin()).collect();
        let renders = vec![a.clone(), b.clone(), a.clone(), b.clone(), a, b];
        let rr = round_robin_section(60, &renders, false);
        assert_eq!(rr.distinct, 2);
        assert_eq!(rr.cycle, Some(2));
        let same = vec![vec![0.1; 30000]; 6];
        let rr = round_robin_section(60, &same, false);
        assert_eq!((rr.distinct, rr.cycle), (1, Some(1)));
    }
}
