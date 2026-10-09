use crate::model::Project;

/// Converts between beats, bars and seconds for a (stepped) tempo map.
#[derive(Clone, Debug)]
pub struct TimeMap {
    /// (start_beat, start_seconds, seconds_per_beat)
    segs: Vec<(f64, f64, f64)>,
    beats_per_bar: f64,
}

impl TimeMap {
    pub fn new(p: &Project) -> TimeMap {
        let bpb = p.beats_per_bar();
        let mut pts = p.tempo.clone();
        pts.sort_by_key(|t| t.bar);
        if pts.is_empty() {
            pts.push(crate::model::TempoPoint { bar: 1, bpm: 120.0 });
        }
        let mut segs: Vec<(f64, f64, f64)> = Vec::new();
        for (i, t) in pts.iter().enumerate() {
            let b0 = (t.bar.max(1) - 1) as f64 * bpb;
            let spb = 60.0 / t.bpm.max(1.0);
            let s0 = if i == 0 {
                0.0
            } else {
                let (pb, ps, pspb) = *segs.last().unwrap();
                ps + (b0 - pb) * pspb
            };
            if i == 0 && b0 > 0.0 {
                segs.push((0.0, 0.0, spb));
            }
            segs.push((b0, s0, spb));
        }
        TimeMap { segs, beats_per_bar: bpb }
    }

    fn seg_beat(&self, beat: f64) -> (f64, f64, f64) {
        let mut s = self.segs[0];
        for x in &self.segs {
            if x.0 <= beat + 1e-9 {
                s = *x;
            }
        }
        s
    }

    pub fn sec(&self, beat: f64) -> f64 {
        let (b0, s0, spb) = self.seg_beat(beat);
        s0 + (beat - b0) * spb
    }

    pub fn beat(&self, sec: f64) -> f64 {
        let mut s = self.segs[0];
        for x in &self.segs {
            if x.1 <= sec + 1e-12 {
                s = *x;
            }
        }
        s.0 + (sec - s.1) / s.2
    }

    pub fn spb_at(&self, beat: f64) -> f64 {
        self.seg_beat(beat).2
    }

    pub fn bpm_at(&self, beat: f64) -> f64 {
        60.0 / self.spb_at(beat)
    }

    pub fn bar_beat(&self, bar: u32) -> f64 {
        (bar.max(1) - 1) as f64 * self.beats_per_bar
    }

    pub fn bar_of(&self, beat: f64) -> u32 {
        (beat / self.beats_per_bar).floor() as u32 + 1
    }

    pub fn beats_per_bar(&self) -> f64 {
        self.beats_per_bar
    }
}
