//! Phrasing: shape a melodic line like a player would. A port of score.py's `phrase`.
//!
//! The line is split into phrases (at rests of at least `rest` beats, after notes of at least `long` beats
//! and before the bars in `breaks`); then every note gets
//!
//! * `accent` velocity points for the metre (`accent * (metric_weight - 0.3)`: strong on 1 and the other
//!   strong beat, a touch softer off the beat),
//! * up to `arc` points rising to the phrase's high point (the highest, then longest, then earliest note) and
//!   falling away after it,
//! * minus `soften` points on the phrase's last note (a third of that on the one before),
//! * and `vary` points of seeded random variation.
//!
//! With `rit`, the time into each phrase's last two notes stretches by rit/2 and rit (at most 0.12 beat in
//! all); legato notes stretch to meet them and the last note grows by `rit` (without reaching the next
//! phrase). With `breath`, each phrase ends `breath` beats before the next one starts.
//!
//! Everything matches score.py exactly except the random variation, which uses splitmix64 instead of
//! Python's Mersenne Twister: with `vary = 0` the output is identical; otherwise it is the same shape with
//! different (still deterministic, seed-stable) jitter.

use crate::meter::Meter;
use crate::util::{SplitMix64, beat4, velocity};
use flux_core::Note;

/// Options for [`phrase`]. The defaults are score.py's.
#[derive(Clone, Debug, PartialEq)]
pub struct PhraseOptions {
    pub accent: f64,
    pub arc: f64,
    pub soften: f64,
    pub rit: f64,
    pub breath: f64,
    pub vary: f64,
    pub seed: u64,
    /// A gap of at least this many beats ends a phrase.
    pub rest: f64,
    /// A note of at least this many beats ends a phrase (default: one bar).
    pub long: Option<f64>,
    /// Bars (1-based) that start a new phrase.
    pub breaks: Vec<u32>,
}

impl Default for PhraseOptions {
    fn default() -> PhraseOptions {
        PhraseOptions { accent: 6.0, arc: 8.0, soften: 8.0, rit: 0.08, breath: 0.0, vary: 2.0, seed: 0, rest: 0.5, long: None, breaks: vec![] }
    }
}

#[derive(Clone, Debug)]
struct Work {
    beat: f64,
    dur: f64,
    pitch: u8,
    vel: f64,
}

/// Splits sorted notes into phrases (indices into `xs`).
fn phrases(xs: &[Work], bar: f64, rest: f64, long: f64, breaks: &[u32]) -> Vec<Vec<usize>> {
    let starts: Vec<f64> = breaks.iter().map(|&b| (b as f64 - 1.0) * bar).collect();
    let mut groups = vec![vec![0usize]];
    for i in 1..xs.len() {
        let (prev, x) = (&xs[i - 1], &xs[i]);
        let gap = x.beat - prev.beat - prev.dur;
        let boundary = starts.iter().any(|&s| prev.beat < s - 1e-6 && s - 1e-6 <= x.beat);
        if gap >= rest - 1e-6 || prev.dur >= long - 1e-6 || boundary {
            groups.push(vec![]);
        }
        groups.last_mut().expect("groups starts non-empty").push(i);
    }
    groups
}

/// Shapes a melodic line (absolute beats) in `meter`; returns new notes sorted by (start, pitch), rounded
/// like score.py (4-decimal beats and lengths, integer velocities 1-127).
pub fn phrase(notes: &[Note], meter: Meter, opts: &PhraseOptions) -> Vec<Note> {
    if notes.is_empty() {
        return vec![];
    }
    let bar = meter.bar_beats();
    let mut xs: Vec<Work> = notes.iter().map(|n| Work { beat: n.start, dur: n.dur, pitch: n.pitch, vel: n.vel as f64 }).collect();
    xs.sort_by(|a, b| a.beat.total_cmp(&b.beat).then(a.pitch.cmp(&b.pitch)));
    let groups = phrases(&xs, bar, opts.rest, opts.long.unwrap_or(bar), &opts.breaks);
    let mut rng = SplitMix64::new(opts.seed);
    for (gi, g) in groups.iter().enumerate() {
        let (t0, t1) = (xs[g[0]].beat, xs[g[g.len() - 1]].beat);
        // the high point: highest pitch, then longest, then earliest
        let mut hp = 0;
        for k in 1..g.len() {
            let (a, b) = (&xs[g[k]], &xs[g[hp]]);
            if (a.pitch, a.dur) > (b.pitch, b.dur) {
                hp = k;
            }
        }
        let tp = xs[g[hp]].beat;
        let n = g.len();
        for (i, &xi) in g.iter().enumerate() {
            let t = xs[xi].beat;
            let f = if n == 1 {
                0.5
            } else if t <= tp {
                0.3 + 0.7 * if tp > t0 { (t - t0) / (tp - t0) } else { 1.0 }
            } else {
                (t1 - t) / (t1 - tp)
            };
            let x = &mut xs[xi];
            x.vel += opts.accent * (meter.weight(t) - 0.3) + opts.arc * f;
            if n > 1 && i >= n - 2 {
                x.vel -= opts.soften * if i == n - 1 { 1.0 } else { 0.33 };
            }
            x.vel += if opts.vary != 0.0 { rng.uniform(-opts.vary, opts.vary) } else { 0.0 };
        }
        if opts.rit != 0.0 && n > 1 {
            let last = g[n - 1];
            let end = xs[last].beat + xs[last].dur;
            let next = groups.get(gi + 1).map(|h| xs[h[0]].beat);
            let cap = match next {
                Some(nx) if end >= nx => end,
                Some(nx) => nx - 0.02,
                None => f64::INFINITY,
            };
            // the time into the last two notes stretches by rit/2 and rit: each note lands a little later
            let tail: Vec<usize> = g[n.saturating_sub(3)..].to_vec();
            let ioi: Vec<f64> = tail.windows(2).map(|w| xs[w[1]].beat - xs[w[0]].beat).collect();
            let ks = [opts.rit / 2.0, opts.rit];
            let k = &ks[ks.len() - ioi.len()..];
            let mut shift = vec![0.0];
            for (i_, s) in ioi.iter().zip(k) {
                let prev = shift[shift.len() - 1];
                shift.push(prev + s * i_);
            }
            let total = shift[shift.len() - 1];
            let scale = if total > 0.0 { 1.0f64.min(0.12 / total) } else { 1.0 };
            for (w, s) in tail.windows(2).zip(shift.windows(2)) {
                let (xi, yi, s0, s1) = (w[0], w[1], s[0], s[1]);
                if xs[xi].beat + xs[xi].dur >= xs[yi].beat - 0.02 {
                    xs[xi].dur += (s1 - s0) * scale; // legato notes keep meeting the next
                }
                xs[yi].beat += s1 * scale;
            }
            let x = &mut xs[last];
            x.dur = 0.05f64.max((x.dur * (1.0 + opts.rit)).min(cap - x.beat));
        }
    }
    if opts.breath != 0.0 {
        for w in groups.windows(2) {
            let (xi, h0) = (w[0][w[0].len() - 1], xs[w[1][0]].beat);
            let x = &mut xs[xi];
            if x.beat + x.dur > h0 - opts.breath {
                x.dur = (0.4 * x.dur).max(h0 - opts.breath - x.beat);
            }
        }
    }
    let mut out: Vec<Note> = groups
        .iter()
        .flatten()
        .map(|&i| {
            let x = &xs[i];
            Note { start: beat4(x.beat), dur: beat4(x.dur.max(0.02)), pitch: x.pitch, vel: velocity(x.vel) }
        })
        .collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.pitch.cmp(&b.pitch)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(spec: &[(f64, f64, u8)]) -> Vec<Note> {
        spec.iter().map(|&(start, dur, pitch)| Note { start, dur, pitch, vel: 80 }).collect()
    }

    #[test]
    fn accents_arc_and_soft_ending() {
        let m = Meter::new(4, 4).unwrap();
        let notes = line(&[(0.0, 1.0, 60), (1.0, 1.0, 64), (2.0, 1.0, 67), (3.0, 1.0, 64), (4.0, 2.0, 60)]);
        let o = PhraseOptions { vary: 0.0, rit: 0.0, ..Default::default() };
        let out = phrase(&notes, m, &o);
        let v: Vec<u8> = out.iter().map(|n| n.vel).collect();
        // 80 + 6*(w-0.3) + 8*f, last two softened
        // beat 0: w=1, f=0.3 -> 80+4.2+2.4 = 86.6 -> 87
        // beat 2: w=0.6, f=1 (high point) -> 80+1.8+8 = 89.8 -> 90
        assert_eq!(v[0], 87);
        assert_eq!(v[2], 90);
        assert!(v[4] < v[2]);
        assert_eq!(out.iter().map(|n| n.start).collect::<Vec<_>>(), [0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn rit_stretches_the_end_and_breath_clears_space() {
        let m = Meter::new(4, 4).unwrap();
        let notes = line(&[(0.0, 1.0, 60), (1.0, 1.0, 62), (2.0, 1.0, 64), (4.0, 1.0, 65), (5.0, 1.0, 67)]);
        let out = phrase(&notes, m, &PhraseOptions { vary: 0.0, breath: 0.0, ..Default::default() });
        // phrase 1 = first three notes (a 1-beat rest follows): 1.0 + 0.04 and 2.0 + 0.12
        assert_eq!(out[1].start, 1.04);
        assert_eq!(out[2].start, 2.12);
        assert!(out[2].start + out[2].dur <= 4.0 - 0.02 + 1e-9);
        let legato = line(&[(0.0, 1.0, 60), (1.0, 2.0, 62), (3.0, 1.0, 64)]);
        let out = phrase(&legato, m, &PhraseOptions { vary: 0.0, rit: 0.0, breath: 0.25, ..Default::default() });
        // the 2-beat note ends a phrase (long = 1 bar? no: 2 < 4) -> one phrase; nothing to breathe
        assert_eq!(out[1].dur, 2.0);
        let out = phrase(&legato, m, &PhraseOptions { vary: 0.0, rit: 0.0, breath: 0.25, long: Some(2.0), ..Default::default() });
        assert_eq!(out[1].dur, 1.75);
    }

    #[test]
    fn variation_is_seeded() {
        let m = Meter::new(6, 8).unwrap();
        let notes = line(&[(0.0, 0.5, 60), (0.5, 0.5, 62), (1.0, 0.5, 64), (1.5, 1.5, 65)]);
        let a = phrase(&notes, m, &PhraseOptions { seed: 3, ..Default::default() });
        let b = phrase(&notes, m, &PhraseOptions { seed: 3, ..Default::default() });
        assert_eq!(a, b);
        let base = phrase(&notes, m, &PhraseOptions { vary: 0.0, ..Default::default() });
        assert!(a.iter().zip(&base).all(|(x, y)| (x.vel as i32 - y.vel as i32).abs() <= 2));
        assert!(phrase(&[], m, &PhraseOptions::default()).is_empty());
    }

    #[test]
    fn breaks_split_phrases() {
        let m = Meter::new(4, 4).unwrap();
        let notes = line(&[(2.0, 1.0, 60), (3.0, 1.0, 62), (4.0, 1.0, 64), (5.0, 1.0, 65)]);
        let one = phrase(&notes, m, &PhraseOptions { vary: 0.0, rit: 0.0, ..Default::default() });
        let two = phrase(&notes, m, &PhraseOptions { vary: 0.0, rit: 0.0, breaks: vec![2], ..Default::default() });
        // with a break at bar 2, the note before it ends a phrase and is softened
        assert!(two[1].vel < one[1].vel);
    }
}
