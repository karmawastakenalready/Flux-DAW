//! Audio clips: stretches of audio files placed on an audio track's timeline.
//!
//! A clip starts at a beat but plays its file at the file's own speed (nothing is stretched to the tempo), so its
//! trims and fades are in seconds. Clips that overlap play together: fade one out over the other's fade in for a
//! crossfade.

use crate::time::TimeMap;
use serde::{Deserialize, Serialize};

fn is_zero(x: &f64) -> bool {
    *x == 0.0
}

/// Clips start this close to a beat count as on it.
pub const EPS: f64 = 1e-9;

/// The loudest clip gain (dB); the quietest is [`MIN_GAIN_DB`].
pub const MAX_GAIN_DB: f64 = 24.0;
pub const MIN_GAIN_DB: f64 = -60.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Clip {
    /// Unique in the project (from `Project::alloc_id`, shared with tracks and inserts).
    pub id: u64,
    /// The audio file: WAV, FLAC or Ogg Vorbis, at any sample rate (it is resampled to the project's).
    pub path: String,
    /// Where it starts, in beats from the top.
    pub start: f64,
    /// Where in the file it starts playing (seconds): trims the file's front.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: f64,
    /// How long it plays (seconds); `None`: to the end of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<f64>,
    /// Clip gain (dB), before the track's channel strip.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gain_db: f64,
    /// How long it takes to rise from silence at its start, and to fall to silence at its end (seconds, linear).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_in: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_out: f64,
}

impl Clip {
    /// The whole of a file from `start`.
    pub fn new(id: u64, path: impl Into<String>, start: f64) -> Clip {
        Clip { id, path: path.into(), start, offset: 0.0, length: None, gain_db: 0.0, fade_in: 0.0, fade_out: 0.0 }
    }

    /// The file's name without its folder ("choir.wav").
    pub fn file_name(&self) -> &str {
        let p = self.path.trim_end_matches(['/', '\\']);
        p.rsplit(['/', '\\']).next().unwrap_or(p)
    }

    /// Where it ends (beats), when its length is known: `file_len` is how long its file is (seconds), for a clip that
    /// plays to the end of it.
    pub fn end(&self, tm: &TimeMap, file_len: Option<f64>) -> Option<f64> {
        let len = self.length.or_else(|| file_len.map(|f| (f - self.offset).max(0.0)))?;
        Some(tm.beat(tm.sec(self.start) + len))
    }

    /// The part of it between beats `a` and `b` (either may be infinite) as a clip with the same id, or None when it
    /// has nothing there. An edge that is cut loses its fade.
    pub fn piece(&self, tm: &TimeMap, a: f64, b: f64) -> Option<Clip> {
        let end = self.end(tm, None).unwrap_or(f64::INFINITY);
        let from = a.max(self.start);
        let to = b.min(end);
        if to <= from + EPS {
            return None;
        }
        let mut c = self.clone();
        if from > self.start + EPS {
            let cut = tm.sec(from) - tm.sec(self.start);
            c.start = from;
            c.offset += cut;
            c.length = c.length.map(|l| (l - cut).max(0.0));
            c.fade_in = 0.0;
        }
        if to < end - EPS {
            c.length = Some(tm.sec(to) - tm.sec(c.start));
            c.fade_out = 0.0;
        }
        Some(c)
    }

    /// The fades as they play, in seconds: shortened in proportion when together they are longer than the clip.
    pub fn fades(&self, played: f64) -> (f64, f64) {
        let (i, o) = (self.fade_in.max(0.0), self.fade_out.max(0.0));
        if i + o > played && i + o > 0.0 {
            let k = played.max(0.0) / (i + o);
            (i * k, o * k)
        } else {
            (i, o)
        }
    }

    /// Its gain at `t` seconds into it (`played` long): the clip gain times the fades.
    pub fn gain_at(&self, t: f64, played: f64) -> f64 {
        let (fi, fo) = self.fades(played);
        let mut g = 10f64.powf(self.gain_db / 20.0);
        if fi > 0.0 && t < fi {
            g *= (t / fi).clamp(0.0, 1.0);
        }
        if fo > 0.0 && t > played - fo {
            g *= ((played - t) / fo).clamp(0.0, 1.0);
        }
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Project, TempoPoint};

    #[test]
    fn pieces_keep_the_audio_in_place() {
        // 120 bpm: a beat is half a second
        let p = Project::new("t", 120.0, 8);
        let tm = TimeMap::new(&p);
        let c = Clip { offset: 1.0, length: Some(4.0), fade_in: 0.5, fade_out: 0.25, ..Clip::new(3, "a/b.wav", 4.0) };
        assert_eq!(c.end(&tm, None), Some(12.0));
        assert_eq!(c.file_name(), "b.wav");
        // the middle: both edges cut, so no fades
        let m = c.piece(&tm, 6.0, 8.0).unwrap();
        assert_eq!((m.start, m.offset, m.length, m.fade_in, m.fade_out), (6.0, 2.0, Some(1.0), 0.0, 0.0));
        // the back keeps its fade out; the front its fade in
        let back = c.piece(&tm, 8.0, f64::INFINITY).unwrap();
        assert_eq!((back.start, back.offset, back.length, back.fade_out), (8.0, 3.0, Some(2.0), 0.25));
        let front = c.piece(&tm, f64::NEG_INFINITY, 8.0).unwrap();
        assert_eq!((front.start, front.offset, front.length, front.fade_in), (4.0, 1.0, Some(2.0), 0.5));
        assert!(c.piece(&tm, 12.0, 20.0).is_none() && c.piece(&tm, 0.0, 4.0).is_none());
        // a clip to the end of its file stays so behind the cut
        let open = Clip::new(4, "x.wav", 0.0);
        let rest = open.piece(&tm, 2.0, f64::INFINITY).unwrap();
        assert_eq!((rest.offset, rest.length), (1.0, None));
        assert_eq!(open.end(&tm, Some(3.0)), Some(6.0));
    }

    #[test]
    fn pieces_across_a_tempo_change() {
        let mut p = Project::new("t", 120.0, 8);
        p.tempo.push(TempoPoint { bar: 2, bpm: 60.0 });
        let tm = TimeMap::new(&p);
        // bar 1 lasts 2 s, then a beat is a second
        let c = Clip::new(1, "x.wav", 2.0);
        let back = c.piece(&tm, 6.0, f64::INFINITY).unwrap();
        assert!((back.offset - 3.0).abs() < 1e-12, "{}", back.offset);
    }

    #[test]
    fn gain_and_fades() {
        let c = Clip { gain_db: -6.0, fade_in: 1.0, fade_out: 3.0, ..Clip::new(1, "x.wav", 0.0) };
        // fades longer than the clip share it
        assert_eq!(c.fades(2.0), (0.5, 1.5));
        assert_eq!(c.fades(10.0), (1.0, 3.0));
        let g = 10f64.powf(-6.0 / 20.0);
        assert!((c.gain_at(0.5, 10.0) - g * 0.5).abs() < 1e-12);
        assert!((c.gain_at(5.0, 10.0) - g).abs() < 1e-12);
        assert!((c.gain_at(8.5, 10.0) - g * 0.5).abs() < 1e-12);
        assert_eq!(c.gain_at(10.0, 10.0), 0.0);
    }
}
