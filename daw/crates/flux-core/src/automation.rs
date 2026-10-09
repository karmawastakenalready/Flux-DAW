//! Automation: a mix setting or a plugin parameter that moves over time.
//!
//! A [`Lane`] holds the points of one [`Target`]. Between two points the value moves the way the first one's
//! [`Shape`] says; before the first point it is the first one's value, after the last the last one's. Two
//! points at the same beat make a jump there. A track's lanes are in [`crate::Track::automation`]; the master's
//! (parameters of the master's and the reverb return's inserts) are in [`crate::Master::automation`].

use serde::{Deserialize, Serialize};

/// A volume lane at or below this is silence.
pub const SILENT_DB: f64 = -90.0;
/// The loudest a volume lane goes.
pub const MAX_VOLUME_DB: f64 = 12.0;
/// Points closer than this (beats) are at the same beat.
const EPS: f64 = 1e-9;
/// The straight lines that stand in for a smooth segment cut in two (half a cosine cut is no longer one).
const SMOOTH_STEPS: usize = 8;

fn is_false(b: &bool) -> bool {
    !*b
}

/// What a lane moves.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum Target {
    /// The track's fader in dB, after its whole strip (inserts and dynamics) and before panning and the
    /// reverb send: 0 dB leaves the track as mixed, [`SILENT_DB`] or lower silences it.
    Volume,
    /// The track's pan, -1 (left) to 1 (right), in place of the mixer's.
    Pan,
    /// The track's reverb send, 0 to 1, in place of the mixer's.
    ReverbSend,
    /// A VST3 parameter (by name or `#id`, as in an insert's `params`) of an insert (`insert`), or of the
    /// track's instrument plugin when there is no `insert`.
    Param {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        insert: Option<u64>,
        param: String,
    },
}

/// The kinds of [`Target`], as commands name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Volume,
    Pan,
    ReverbSend,
    Param,
}

impl Target {
    /// What it moves, for people: "volume", "pan", "reverb send", "Mix (insert #12)", "Cutoff".
    pub fn label(&self) -> String {
        match self {
            Target::Volume => "volume".into(),
            Target::Pan => "pan".into(),
            Target::ReverbSend => "reverb send".into(),
            Target::Param { insert: Some(i), param } => format!("{param} (insert #{i})"),
            Target::Param { insert: None, param } => param.clone(),
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Target::Volume => Kind::Volume,
            Target::Pan => Kind::Pan,
            Target::ReverbSend => Kind::ReverbSend,
            Target::Param { .. } => Kind::Param,
        }
    }

    /// The values it takes (a plugin parameter in its display units has no fixed range).
    pub fn range(&self, normalized: bool) -> Option<(f64, f64)> {
        match self {
            Target::Volume => Some((SILENT_DB, MAX_VOLUME_DB)),
            Target::Pan => Some((-1.0, 1.0)),
            Target::ReverbSend => Some((0.0, 1.0)),
            Target::Param { .. } if normalized => Some((0.0, 1.0)),
            Target::Param { .. } => None,
        }
    }

    /// The insert it belongs to, if any.
    pub fn insert(&self) -> Option<u64> {
        match self {
            Target::Param { insert, .. } => *insert,
            _ => None,
        }
    }

    /// The same target: parameter names match whatever their case.
    pub fn same(&self, o: &Target) -> bool {
        match (self, o) {
            (Target::Param { insert: a, param: p }, Target::Param { insert: b, param: q }) => a == b && p.trim().eq_ignore_ascii_case(q.trim()),
            _ => self == o,
        }
    }
}

/// How the value moves from a point to the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// A straight line.
    #[default]
    Linear,
    /// Eases out of this point and into the next (half a cosine): swells and fades without corners.
    Smooth,
    /// Holds this value until the next point, then jumps.
    Hold,
}

impl Shape {
    fn is_linear(&self) -> bool {
        *self == Shape::Linear
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Beats from the top of the song.
    pub beat: f64,
    pub value: f64,
    /// How the value moves from here to the next point.
    #[serde(default, skip_serializing_if = "Shape::is_linear")]
    pub shape: Shape,
}

/// The value at `beat` on the way from `a` to `b`.
fn between(a: &Point, b: &Point, beat: f64) -> f64 {
    let span = b.beat - a.beat;
    let x = if span > 0.0 { ((beat - a.beat) / span).clamp(0.0, 1.0) } else { 1.0 };
    match a.shape {
        Shape::Linear => a.value + (b.value - a.value) * x,
        Shape::Smooth => a.value + (b.value - a.value) * (0.5 - 0.5 * (std::f64::consts::PI * x).cos()),
        Shape::Hold => a.value,
    }
}

/// The points of one target, in time order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    #[serde(flatten)]
    pub target: Target,
    /// Parameter lanes: the values are the plugin's normalized 0..1 rather than its display units (dB, Hz,
    /// %). Either way the value moves between points in the plugin's own scale, as in other DAWs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub normalized: bool,
    pub points: Vec<Point>,
}

impl Lane {
    pub fn new(target: Target, normalized: bool) -> Lane {
        Lane { target, normalized, points: vec![] }
    }

    /// The value at `beat` (after the jump, where two points at `beat` make one), or `None` for a lane
    /// without points.
    pub fn value_at(&self, beat: f64) -> Option<f64> {
        let p = &self.points;
        let first = p.first()?;
        let i = p.partition_point(|q| q.beat <= beat);
        if i == 0 {
            return Some(first.value);
        }
        Some(match p.get(i) {
            Some(b) => between(&p[i - 1], b, beat),
            None => p[i - 1].value,
        })
    }

    /// The value just before `beat` (before the jump, where two points at `beat` make one).
    pub fn value_before(&self, beat: f64) -> Option<f64> {
        let p = &self.points;
        let first = p.first()?;
        let i = p.partition_point(|q| q.beat < beat);
        if i == 0 {
            return Some(first.value);
        }
        Some(match p.get(i) {
            Some(b) => between(&p[i - 1], b, beat),
            None => p[i - 1].value,
        })
    }

    /// Write `new` over the lane: the points from the first new one's beat to the last's (inclusive) are
    /// replaced. Points at the same beat keep their order (a jump).
    pub fn write(&mut self, mut new: Vec<Point>) {
        if new.is_empty() {
            return;
        }
        new.sort_by(|a, b| a.beat.total_cmp(&b.beat));
        let (a, b) = (new[0].beat, new[new.len() - 1].beat);
        self.points.retain(|q| q.beat < a - EPS || q.beat > b + EPS);
        let at = self.points.partition_point(|q| q.beat < a);
        self.points.splice(at..at, new);
        self.tidy();
    }

    /// Remove the points in `[a, b)` (beats); returns how many. The value then moves straight across.
    pub fn clear(&mut self, a: f64, b: f64) -> usize {
        let before = self.points.len();
        self.points.retain(|q| !(q.beat >= a - EPS && q.beat < b - EPS));
        before - self.points.len()
    }

    /// The curve over `[a, b]` as points moved by `offset` beats: its values at both ends and the points
    /// between, for [`Lane::paste`]. Empty for a lane without points.
    pub fn segment(&self, a: f64, b: f64, offset: f64) -> Vec<Point> {
        if self.points.is_empty() || b <= a {
            return vec![];
        }
        let mut c = self.clone();
        c.split_at(a);
        c.split_at(b);
        let p = &c.points;
        let (Some(s), Some(e)) = (p.iter().rposition(|q| (q.beat - a).abs() < EPS), p.iter().position(|q| (q.beat - b).abs() < EPS))
        else {
            return vec![];
        };
        p[s..=e].iter().map(|q| Point { beat: q.beat + offset, ..*q }).collect()
    }

    /// Lay a [`Lane::segment`] over `[a, b]`: the curve over that span becomes the segment's, while before `a`
    /// and from `b` on it stays as it was (with jumps at the ends where the values differ).
    pub fn paste(&mut self, seg: Vec<Point>, a: f64, b: f64) {
        if seg.is_empty() {
            return;
        }
        if self.points.is_empty() {
            self.points = seg;
            return;
        }
        self.split_at(a);
        self.split_at(b);
        let p = &mut self.points;
        let (Some(s), Some(e)) = (p.iter().position(|q| (q.beat - a).abs() < EPS), p.iter().rposition(|q| (q.beat - b).abs() < EPS))
        else {
            return;
        };
        // the point the curve arrives at `a` with, the segment, and the one it leaves `b` with
        let mut mid = Vec::with_capacity(seg.len() + 2);
        mid.push(p[s]);
        mid.extend(seg);
        mid.push(p[e]);
        p.splice(s..=e, mid);
        self.tidy();
        self.prune();
    }

    /// Drop the points the curve doesn't need: inside a flat stretch or a straight line, and flat ends.
    fn prune(&mut self) {
        let same = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
        let mut out: Vec<Point> = Vec::with_capacity(self.points.len());
        for q in self.points.drain(..) {
            while let [.., p, m] = out[..] {
                let inside = p.beat < m.beat - EPS && m.beat < q.beat - EPS;
                let flat = same(p.value, m.value) && same(m.value, q.value);
                let held = p.shape == Shape::Hold && m.shape == Shape::Hold && same(p.value, m.value);
                let line = p.shape == Shape::Linear && m.shape == Shape::Linear && same(between(&p, &q, m.beat), m.value);
                if !(inside && (flat || held || line)) {
                    break;
                }
                out.pop();
            }
            out.push(q);
        }
        while out.len() >= 2 && out[0].beat < out[1].beat - EPS && same(out[0].value, out[1].value) {
            out.remove(0);
        }
        while let [.., p, l] = out[..]
            && p.beat < l.beat - EPS
            && same(p.value, l.value)
        {
            out.pop();
        }
        self.points = out;
    }

    /// Put a point at `beat` without changing the curve. A smooth segment it cuts becomes short straight lines.
    fn split_at(&mut self, beat: f64) {
        let p = &mut self.points;
        if p.is_empty() || p.iter().any(|q| (q.beat - beat).abs() < EPS) {
            return;
        }
        let i = p.partition_point(|q| q.beat < beat);
        if i == 0 || i == p.len() {
            let value = if i == 0 { p[0].value } else { p[i - 1].value };
            p.insert(i, Point { beat, value, shape: Shape::Linear });
            return;
        }
        let (a, b) = (p[i - 1], p[i]);
        if a.shape != Shape::Smooth {
            p.insert(i, Point { beat, value: between(&a, &b, beat), shape: a.shape });
            return;
        }
        let mut lines: Vec<Point> = (0..SMOOTH_STEPS)
            .map(|k| a.beat + (b.beat - a.beat) * k as f64 / SMOOTH_STEPS as f64)
            .filter(|t| (t - beat).abs() >= EPS)
            .chain([beat])
            .map(|t| Point { beat: t, value: between(&a, &b, t), shape: Shape::Linear })
            .collect();
        lines.sort_by(|x, y| x.beat.total_cmp(&y.beat));
        p.splice(i - 1..i, lines);
    }

    /// At most two points at a beat (a jump), and one where they agree.
    fn tidy(&mut self) {
        let mut out = Vec::with_capacity(self.points.len());
        for g in self.points.chunk_by(|x, y| (y.beat - x.beat).abs() < EPS) {
            let (f, l) = (g[0], g[g.len() - 1]);
            if g.len() > 1 && f.value != l.value {
                out.push(f);
            }
            out.push(l);
        }
        self.points = out;
    }
}

/// Linear gain of a volume lane's value (dB): 0 at [`SILENT_DB`] and below.
pub fn volume_gain(db: f64) -> f64 {
    if db <= SILENT_DB { 0.0 } else { 10f64.powf(db.min(MAX_VOLUME_DB) / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pt(beat: f64, value: f64, shape: Shape) -> Point {
        Point { beat, value, shape }
    }

    fn lane(points: Vec<Point>) -> Lane {
        Lane { target: Target::Volume, normalized: false, points }
    }

    #[test]
    fn values_between_points() {
        let mut l = lane(vec![]);
        assert_eq!(l.value_at(3.0), None);
        l.points = vec![pt(4.0, -12.0, Shape::Linear), pt(8.0, 0.0, Shape::Smooth), pt(12.0, -6.0, Shape::Hold), pt(16.0, 3.0, Shape::Linear)];
        assert_eq!(l.value_at(0.0), Some(-12.0));
        assert_eq!(l.value_at(6.0), Some(-6.0));
        // half a cosine: a quarter of the way is about 15% of the change, half way is half
        assert!((l.value_at(9.0).unwrap() - -0.8787).abs() < 1e-3);
        assert!((l.value_at(10.0).unwrap() - -3.0).abs() < 1e-9);
        assert_eq!(l.value_at(15.99), Some(-6.0));
        assert_eq!(l.value_at(16.0), Some(3.0));
        assert_eq!(l.value_before(16.0), Some(-6.0));
        assert_eq!(l.value_at(100.0), Some(3.0));
        // two points at one beat: a jump
        l.points = vec![pt(0.0, 0.0, Shape::Linear), pt(4.0, 1.0, Shape::Linear), pt(4.0, -1.0, Shape::Linear), pt(8.0, 0.0, Shape::Linear)];
        assert_eq!(l.value_at(3.0), Some(0.75));
        assert_eq!(l.value_before(4.0), Some(1.0));
        assert_eq!(l.value_at(4.0), Some(-1.0));
        assert_eq!(l.value_at(6.0), Some(-0.5));
    }

    #[test]
    fn writing_replaces_the_span() {
        let mut l = lane(vec![]);
        l.write((0..8).map(|b| pt(f64::from(b) * 2.0, 0.0, Shape::Linear)).collect());
        l.write(vec![pt(7.0, 1.0, Shape::Linear), pt(3.0, -1.0, Shape::Hold)]);
        let beats: Vec<f64> = l.points.iter().map(|p| p.beat).collect();
        assert_eq!(beats, [0.0, 2.0, 3.0, 7.0, 8.0, 10.0, 12.0, 14.0]);
        assert_eq!(l.points[2].shape, Shape::Hold);
        assert_eq!(l.clear(8.0, 12.0), 2);
        assert_eq!(l.points.len(), 6);
    }

    /// Copying a span and laying it elsewhere keeps the curve there and leaves the rest alone.
    #[test]
    fn segments_and_paste() {
        // a ramp 0..8 then a hold, copied over a flat stretch further on
        let src = lane(vec![pt(0.0, -20.0, Shape::Linear), pt(8.0, 0.0, Shape::Hold), pt(12.0, -6.0, Shape::Linear), pt(32.0, -6.0, Shape::Linear)]);
        let mut l = src.clone();
        let seg = l.segment(4.0, 12.0, 16.0);
        l.paste(seg, 20.0, 28.0);
        for k in 0..=320 {
            let t = f64::from(k) * 0.1;
            let want = if (20.0..28.0).contains(&t) { src.value_at(t - 16.0) } else { src.value_at(t) };
            assert!((l.value_at(t).unwrap() - want.unwrap()).abs() < 1e-9, "beat {t}: {:?} vs {want:?}", l.value_at(t));
        }
        // a jump back to the old curve at the end
        assert_eq!(l.value_before(28.0), Some(0.0));
        assert_eq!(l.value_at(28.0), Some(-6.0));

        // a smooth swell cut in the middle: short lines that stay close to it
        let swell = lane(vec![pt(0.0, -24.0, Shape::Smooth), pt(16.0, 0.0, Shape::Linear)]);
        let seg = swell.segment(4.0, 12.0, 0.0);
        assert!(seg.len() > 4 && seg.iter().all(|p| p.shape == Shape::Linear));
        let mut copy = lane(vec![pt(0.0, -24.0, Shape::Linear), pt(16.0, -24.0, Shape::Linear)]);
        copy.paste(seg, 4.0, 12.0);
        for k in 40..120 {
            let t = f64::from(k) * 0.1;
            assert!((copy.value_at(t).unwrap() - swell.value_at(t).unwrap()).abs() < 0.3, "beat {t}");
        }
        assert_eq!(copy.value_at(2.0), Some(-24.0));
        assert_eq!(copy.value_at(14.0), Some(-24.0));
        // pasting into a lane without points takes the segment as it is
        let mut empty = lane(vec![]);
        empty.paste(vec![pt(0.0, 1.0, Shape::Linear)], 0.0, 4.0);
        assert_eq!(empty.points.len(), 1);
    }

    #[test]
    fn file_format() {
        let l = Lane {
            target: Target::Param { insert: Some(12), param: "Mix".into() },
            normalized: false,
            points: vec![pt(0.0, 20.0, Shape::Smooth), pt(8.0, 60.0, Shape::Linear)],
        };
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(v, json!({"target": "param", "insert": 12, "param": "Mix", "points": [{"beat": 0.0, "value": 20.0, "shape": "smooth"}, {"beat": 8.0, "value": 60.0}]}));
        assert_eq!(serde_json::from_value::<Lane>(v).unwrap(), l);
        let vol: Lane = serde_json::from_value(json!({"target": "volume", "points": []})).unwrap();
        assert_eq!(vol.target, Target::Volume);
        assert!(Target::Param { insert: None, param: " cutoff".into() }.same(&Target::Param { insert: None, param: "Cutoff".into() }));
        assert_eq!(volume_gain(SILENT_DB), 0.0);
        assert!((volume_gain(-6.0) - 0.501).abs() < 1e-3);
    }
}
