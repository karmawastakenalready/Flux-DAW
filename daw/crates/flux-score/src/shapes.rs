//! Controller shapes: ramps, curves through points, and expression swells that follow a melodic line.
//!
//! Points come out with their beat rounded to 4 decimals (and never below 0) and an unrounded value;
//! [`to_cc`] and [`to_bends`] round and clamp the values for MIDI. The ramp and swell arithmetic is
//! score.py's (`ramp`, `curve`, `swell_cc`), operation for operation, so the same input gives the same
//! points.

use crate::util::{beat4, cc_value, round_even};
use flux_core::{BendPoint, CcPoint, Note};
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// A controller point: an absolute beat and a value (CC 0-127, or pitch bend -8192..8191).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub beat: f64,
    pub value: f64,
}

impl Point {
    pub fn new(beat: f64, value: f64) -> Point {
        Point { beat, value }
    }
}

/// How a curve moves between its points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// Straight lines (score.py's `curve`).
    #[default]
    Linear,
    /// Eased (half-cosine) segments: slow at both ends, no corners at the points.
    Smooth,
    /// Holds each value until the next point (a step change at each point).
    Step,
}

fn point(beat: f64, value: f64) -> Point {
    Point { beat: beat4(beat.max(0.0)), value }
}

/// Points every `step` beats from (b0, v0) to (b1, v1), both ends included: score.py's `ramp`.
/// The number of segments is `max(1, round(span / step))` (ties to even).
pub fn ramp(b0: f64, v0: f64, b1: f64, v1: f64, step: f64) -> Vec<Point> {
    shaped_ramp(b0, v0, b1, v1, step, Shape::Linear)
}

fn segments(b0: f64, b1: f64, step: f64) -> usize {
    let k = round_even((b1 - b0) / step);
    if k.is_finite() && k >= 1.0 { k.min(1e6) as usize } else { 1 }
}

fn shaped_ramp(b0: f64, v0: f64, b1: f64, v1: f64, step: f64, shape: Shape) -> Vec<Point> {
    let k = segments(b0, b1, step);
    (0..=k)
        .map(|i| {
            let beat = b0 + (b1 - b0) * i as f64 / k as f64;
            let value = match shape {
                Shape::Linear => v0 + (v1 - v0) * i as f64 / k as f64,
                Shape::Smooth => v0 + (v1 - v0) * (1.0 - (PI * i as f64 / k as f64).cos()) / 2.0,
                Shape::Step => {
                    if i == k {
                        v1
                    } else {
                        v0
                    }
                }
            };
            point(beat, value)
        })
        .collect()
}

/// A curve through `(beat, value)` points (absolute beats, in order), sampled every `step` beats:
/// score.py's `curve` for [`Shape::Linear`]. A step shape holds each value and jumps at the next point.
pub fn curve(points: &[(f64, f64)], step: f64, shape: Shape) -> Vec<Point> {
    let Some(&(lb, lv)) = points.last() else { return vec![] };
    let mut out = Vec::new();
    for w in points.windows(2) {
        let ((ba, va), (bb, vb)) = (w[0], w[1]);
        if shape == Shape::Step {
            out.push(point(ba, va));
            continue;
        }
        let mut seg = shaped_ramp(ba, va, bb, vb, step, shape);
        seg.pop();
        out.extend(seg);
    }
    out.push(point(lb, lv));
    out
}

/// Expression (CC1/CC11) points for a sustained instrument playing `notes`: score.py's `swell_cc`. The
/// level follows the line (higher and louder notes get more), notes of `long` beats or more swell through
/// their first 60% and ease back, and the level sinks to `lo` in rests of `rest` beats or more. Sampled
/// every `step` beats; one point per (rounded) beat, the later one winning.
pub fn swell(notes: &[Note], lo: f64, hi: f64, long: f64, step: f64, rest: f64) -> Vec<Point> {
    let mut xs: Vec<&Note> = notes.iter().collect();
    if xs.is_empty() {
        return vec![];
    }
    xs.sort_by(|a, b| a.start.total_cmp(&b.start).then(b.pitch.cmp(&a.pitch)));
    let p0 = xs.iter().map(|x| x.pitch).min().unwrap_or(0) as f64;
    let p1 = xs.iter().map(|x| x.pitch).max().unwrap_or(0) as f64;
    let v0 = xs.iter().map(|x| x.vel).min().unwrap_or(0) as f64;
    let v1 = xs.iter().map(|x| x.vel).max().unwrap_or(0) as f64;
    let span = hi - lo;
    let norm = |v: f64, a: f64, b: f64| if b > a { (v - a) / (b - a) } else { 0.5 };
    let mut keys: Vec<(f64, f64)> = Vec::new();
    for (i, x) in xs.iter().enumerate() {
        let (t, d) = (x.start, x.dur);
        let level = lo + span * (0.25 + 0.45 * norm(x.pitch as f64, p0, p1) + 0.3 * norm(x.vel as f64, v0, v1));
        if d >= long {
            keys.push((t, level - 0.2 * span));
            keys.push((t + 0.6 * d, hi.min(level + 0.1 * span)));
            keys.push((t + d, level - 0.05 * span));
        } else {
            keys.push((t, level));
            keys.push((t + 0.9 * d, level));
        }
        let next = xs.get(i + 1).map(|n| n.start);
        if next.is_none_or(|nx| nx - t - d >= rest) {
            let gap = next.map_or(1.0, |nx| nx - t - d);
            keys.push((t + d + 0.5f64.min(gap / 2.0), lo));
            if let Some(nx) = next {
                keys.push((nx - 0.25f64.min(gap / 2.0), lo));
            }
        }
    }
    keys.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut pts: Vec<Point> = Vec::new();
    for w in keys.windows(2) {
        let ((b0, a), (b1, b)) = (w[0], w[1]);
        if b1 - b0 > 1e-6 {
            let mut seg = ramp(b0, a, b1, b, step);
            seg.pop();
            pts.extend(seg);
        }
    }
    let (lb, lv) = keys[keys.len() - 1];
    pts.push(point(lb, lv));
    // one point per beat, the later one wins (score.py keeps a dict keyed by the rounded beat)
    let mut out: Vec<Point> = Vec::with_capacity(pts.len());
    let mut index: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for p in pts {
        let key = (p.beat + 0.0).to_bits();
        match index.get(&key) {
            Some(&i) => out[i].value = p.value,
            None => {
                index.insert(key, out.len());
                out.push(p);
            }
        }
    }
    out.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    out
}

/// CC points from shape points: values rounded (ties to even) and clamped to 0-127.
pub fn to_cc(points: &[Point], cc: u8) -> Vec<CcPoint> {
    points.iter().map(|p| CcPoint { beat: p.beat, cc, value: cc_value(p.value) }).collect()
}

/// Pitch-bend points from shape points: values rounded (ties to even) and clamped to -8192..8191.
pub fn to_bends(points: &[Point]) -> Vec<BendPoint> {
    points
        .iter()
        .map(|p| {
            let v = if p.value.is_nan() { 0.0 } else { round_even(p.value).clamp(-8192.0, 8191.0) };
            BendPoint { beat: p.beat, value: v as i16 }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pts: &[Point]) -> Vec<(f64, f64)> {
        pts.iter().map(|p| (p.beat, p.value)).collect()
    }

    #[test]
    fn ramps_like_score_py() {
        assert_eq!(vals(&ramp(0.0, 0.0, 1.0, 100.0, 0.5)), [(0.0, 0.0), (0.5, 50.0), (1.0, 100.0)]);
        // 0.75 / 0.5 = 1.5 rounds to 2 segments (ties to even)
        assert_eq!(ramp(0.0, 0.0, 0.75, 1.0, 0.5).len(), 3);
        // 1.25 / 0.5 = 2.5 rounds to 2
        assert_eq!(ramp(0.0, 0.0, 1.25, 1.0, 0.5).len(), 3);
        // a backwards or empty span is one segment
        assert_eq!(ramp(2.0, 0.0, 1.0, 1.0, 0.5).len(), 2);
        assert_eq!(ramp(-1.0, 0.0, 0.0, 10.0, 1.0)[0].beat, 0.0);
    }

    #[test]
    fn curves() {
        let c = curve(&[(0.0, 0.0), (1.0, 100.0), (2.0, 0.0)], 0.5, Shape::Linear);
        assert_eq!(vals(&c), [(0.0, 0.0), (0.5, 50.0), (1.0, 100.0), (1.5, 50.0), (2.0, 0.0)]);
        let s = curve(&[(0.0, 0.0), (1.0, 100.0)], 0.25, Shape::Smooth);
        assert_eq!(s.len(), 5);
        assert!((s[1].value - 14.6447).abs() < 1e-3 && (s[2].value - 50.0).abs() < 1e-9);
        let st = curve(&[(0.0, 10.0), (1.0, 100.0), (2.0, 5.0)], 0.5, Shape::Step);
        assert_eq!(vals(&st), [(0.0, 10.0), (1.0, 100.0), (2.0, 5.0)]);
        assert!(curve(&[], 0.5, Shape::Linear).is_empty());
        assert_eq!(curve(&[(1.0, 3.0)], 0.5, Shape::Linear).len(), 1);
    }

    #[test]
    fn swell_follows_the_line() {
        let notes = [
            Note { start: 0.0, dur: 2.0, pitch: 60, vel: 80 },
            Note { start: 2.0, dur: 1.0, pitch: 64, vel: 90 },
            Note { start: 5.0, dur: 1.0, pitch: 62, vel: 85 },
        ];
        let pts = swell(&notes, 40.0, 100.0, 1.5, 0.25, 1.0);
        assert!(pts.windows(2).all(|w| w[0].beat < w[1].beat));
        assert_eq!(pts[0].beat, 0.0);
        // sinks to lo in the 2-beat rest
        assert!(pts.iter().any(|p| p.beat == 3.5 && p.value == 40.0));
        assert!(swell(&[], 40.0, 100.0, 1.5, 0.25, 1.0).is_empty());
        let cc = to_cc(&pts, 11);
        assert!(cc.iter().all(|c| c.cc == 11 && (40..=100).contains(&c.value)));
    }

    #[test]
    fn bend_conversion_clamps() {
        let b = to_bends(&[Point::new(0.0, 9000.0), Point::new(1.0, -9000.0), Point::new(2.0, 0.5)]);
        assert_eq!(b.iter().map(|p| p.value).collect::<Vec<_>>(), [8191, -8192, 0]);
    }
}
