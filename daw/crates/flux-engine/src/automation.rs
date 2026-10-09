//! Automation in the renderers. A track's volume, pan and reverb send lanes become per-sample curves over the
//! song; VST3 parameter lanes become the points a plugin is sent block by block (see `vst`).

use flux_core::automation::{Lane, Target, volume_gain};
use flux_core::{TimeMap, Track};

/// Jumps in a mix curve (a hold point, two points at one beat, a loop whose end and start differ) become ramps
/// this long (s) instead of clicks.
pub const SMOOTH_S: f64 = 0.005;
/// A lane is evaluated every this many samples and drawn straight in between.
const STEP: usize = 16;

/// A track's mix lanes as values per song sample.
#[derive(Default)]
pub struct MixCurves {
    /// Linear gain of the volume lane.
    gain: Option<Vec<f32>>,
    pan: Option<Vec<f32>>,
    send: Option<Vec<f32>>,
}

impl MixCurves {
    /// The curves of `t`'s lanes over `frames` song samples; a looped song's wrap around (its last sample leads
    /// to its first).
    pub fn new(t: &Track, tm: &TimeMap, sr: f64, frames: usize, looped: bool) -> MixCurves {
        let mut c = MixCurves::default();
        for lane in t.automation.iter().filter(|l| !l.points.is_empty()) {
            let curve = |f: fn(f64) -> f64| Some(lane_curve(lane, tm, sr, frames, looped, f));
            match lane.target {
                Target::Volume => c.gain = curve(volume_gain),
                Target::Pan => c.pan = curve(|v| v.clamp(-1.0, 1.0)),
                Target::ReverbSend => c.send = curve(|v| v.clamp(0.0, 1.0)),
                Target::Param { .. } => {}
            }
        }
        c
    }

    pub fn has_gain(&self) -> bool {
        self.gain.is_some()
    }

    pub fn has_pan(&self) -> bool {
        self.pan.is_some()
    }

    pub fn has_send(&self) -> bool {
        self.send.is_some()
    }

    /// The volume lane's gain at song sample `s` (1 without one).
    pub fn gain(&self, s: usize) -> f32 {
        self.gain.as_deref().map_or(1.0, |c| at(c, s))
    }

    /// The pan at song sample `s` (`mixer` without a lane).
    pub fn pan(&self, s: usize, mixer: f64) -> f64 {
        self.pan.as_deref().map_or(mixer, |c| at(c, s) as f64)
    }

    /// The reverb send at song sample `s` (`mixer` without a lane).
    pub fn send(&self, s: usize, mixer: f32) -> f32 {
        self.send.as_deref().map_or(mixer, |c| at(c, s))
    }

    /// Whether the track sends to the reverb anywhere.
    pub fn sends(&self, mixer: f32) -> bool {
        match &self.send {
            Some(c) => c.iter().any(|&x| x > 0.0),
            None => mixer > 0.0,
        }
    }
}

fn at(c: &[f32], s: usize) -> f32 {
    c[s.min(c.len().saturating_sub(1))]
}

/// `lane` over `frames` song samples through `f` (dB to gain, a clamp), smoothed over [`SMOOTH_S`].
pub fn lane_curve(lane: &Lane, tm: &TimeMap, sr: f64, frames: usize, looped: bool, f: fn(f64) -> f64) -> Vec<f32> {
    if frames == 0 {
        return vec![];
    }
    let steps = frames.div_ceil(STEP);
    let ctl: Vec<f64> = (0..=steps).map(|k| f(lane.value_at(tm.beat((k * STEP) as f64 / sr)).unwrap_or(0.0))).collect();
    let mut v = vec![0f64; frames];
    for (s, x) in v.iter_mut().enumerate() {
        let (k, r) = (s / STEP, (s % STEP) as f64 / STEP as f64);
        *x = ctl[k] + (ctl[k + 1] - ctl[k]) * r;
    }
    smooth(&v, ((SMOOTH_S * sr) as usize) | 1, looped).into_iter().map(|x| x as f32).collect()
}

/// A centered moving average `w` (odd) samples wide: around the ends of a loop it wraps, otherwise the ends
/// repeat.
fn smooth(v: &[f64], w: usize, wrap: bool) -> Vec<f64> {
    let n = v.len();
    if w <= 1 || n < 2 {
        return v.to_vec();
    }
    let h = (w / 2) as isize;
    let get = |i: isize| -> f64 {
        if wrap {
            v[i.rem_euclid(n as isize) as usize]
        } else {
            v[i.clamp(0, n as isize - 1) as usize]
        }
    };
    let mut sum: f64 = (-h..=h).map(get).sum();
    let mut out = Vec::with_capacity(n);
    for i in 0..n as isize {
        out.push(sum / w as f64);
        sum += get(i + h + 1) - get(i - h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::automation::{Point, Shape};
    use flux_core::Project;

    fn lane(target: Target, points: &[(f64, f64, Shape)]) -> Lane {
        Lane { target, normalized: false, points: points.iter().map(|&(beat, value, shape)| Point { beat, value, shape }).collect() }
    }

    #[test]
    fn curves() {
        // 120 bpm: a beat is half a second
        let p = Project::new("t", 120.0, 4);
        let tm = TimeMap::new(&p);
        let sr = 1000.0;
        let ramp = lane(Target::Pan, &[(0.0, -1.0, Shape::Linear), (4.0, 1.0, Shape::Hold), (6.0, 0.0, Shape::Linear)]);
        let c = lane_curve(&ramp, &tm, sr, 8000, true, |v| v);
        assert!((c[500] - -0.5).abs() < 1e-3);
        assert!((c[1000] - 0.0).abs() < 1e-3);
        assert!((c[2500] - 1.0).abs() < 1e-6);
        // the jump at beat 6 is a ramp a few samples long, centered on it
        assert!((c[3000] - 0.5).abs() < 0.05);
        assert!((c[3010] - 0.0).abs() < 1e-6);
        assert!((c[2990] - 1.0).abs() < 1e-6);
        // looped, the wrap from 0 back to -1 is smoothed across the loop point too
        assert!(c[0] > -1.0 && c[0] < -0.4, "{}", c[0]);
        assert!(c[7999] < 0.0 && c[7999] > -0.6, "{}", c[7999]);
        let one_shot = lane_curve(&ramp, &tm, sr, 8000, false, |v| v);
        assert!((one_shot[0] - -1.0).abs() < 1e-2 && (one_shot[7999] - 0.0).abs() < 1e-6);

        let mut q = Project::new("t", 120.0, 4);
        flux_core::commands::apply(&mut q, &serde_json::from_value(serde_json::json!({"op": "add_track", "name": "a"})).unwrap()).unwrap();
        let mut t = q.tracks.remove(0);
        t.automation = vec![lane(Target::Volume, &[(0.0, -90.0, Shape::Linear), (2.0, 0.0, Shape::Linear)])];
        let m = MixCurves::new(&t, &tm, sr, 8000, false);
        assert!(m.has_gain() && !m.has_pan() && !m.has_send());
        assert!(m.gain(0) < 1e-4);
        assert!((m.gain(5000) - 1.0).abs() < 1e-6);
        assert_eq!(m.pan(10, 0.25), 0.25);
        assert!(!m.sends(0.0) && m.sends(0.1));
    }
}
