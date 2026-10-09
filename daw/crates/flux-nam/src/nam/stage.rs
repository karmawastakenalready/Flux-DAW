//! The `nam` stage: input gain, resampling to the capture's rate, inference, resampling back, output gain.
//!
//! `rig.py` resamples the whole signal with `scipy.signal.resample_poly` (zero phase) in both directions. The
//! stream computes the same samples (see [`crate::resample`]) and delays the result by a fixed number of host
//! samples ([`NamStage::latency`]) so every output sample exists when it is due. With equal rates there is no
//! resampling and no latency.

use super::{NamModel, NamNet};
use crate::error::{Error, Result};
use crate::resample::{Quality, Resampler};

/// A NAM capture running at a host sample rate.
#[derive(Debug)]
pub struct NamStage {
    net: NamNet,
    input_gain: f32,
    output_gain: f32,
    max_block: usize,
    bridge: Option<Bridge>,
}

/// Host rate -> model rate -> host rate, with the delay line that makes the round trip causal.
#[derive(Debug)]
struct Bridge {
    up: Resampler,
    down: Resampler,
    /// Model-rate samples of one block.
    model: Vec<f32>,
    /// Host-rate samples produced by one block.
    host: Vec<f32>,
    /// Ring buffer of host-rate output waiting to be due.
    fifo: Vec<f32>,
    read: usize,
    count: usize,
    latency: usize,
}

/// Linear gain for a level in dB, rounded to `f32` like `np.float32(10 ** (db / 20))`.
pub(crate) fn db_gain(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

impl NamStage {
    /// The stage for `model` at `sample_rate`, called with blocks of at most `max_block` samples (longer calls
    /// are split). `input_db` / `output_db` are the rig's gains around the model.
    pub fn new(
        model: &NamModel,
        input_db: f64,
        output_db: f64,
        sample_rate: u32,
        max_block: usize,
        quality: Quality,
    ) -> Result<NamStage> {
        if max_block == 0 {
            return Err(Error::stage(model.label(), "max_block must be at least 1"));
        }
        let rate = model.sample_rate();
        let bridge = if rate == sample_rate {
            None
        } else {
            let wrap = |e: Error| match e {
                Error::Stage { message, .. } => Error::stage(model.label(), message),
                other => other,
            };
            let up = Resampler::new(sample_rate, rate, quality, max_block).map_err(wrap)?;
            let model_max = up.max_output(max_block);
            let down = Resampler::new(rate, sample_rate, quality, model_max).map_err(wrap)?;
            let host_max = down.max_output(model_max);
            let (latency, ahead) = round_trip(&up, &down);
            let mut b = Bridge {
                model: vec![0.0; model_max],
                host: vec![0.0; host_max],
                fifo: vec![0.0; latency + max_block.max(host_max) + ahead + 1],
                read: 0,
                count: 0,
                latency,
                up,
                down,
            };
            b.reset();
            Some(b)
        };
        Ok(NamStage { net: model.instance(), input_gain: db_gain(input_db), output_gain: db_gain(output_db), max_block, bridge })
    }

    /// Delay of the stage in host samples (0 when the host runs at the model's rate).
    pub fn latency(&self) -> usize {
        self.bridge.as_ref().map_or(0, |b| b.latency)
    }

    /// Process `buf` in place. Never allocates.
    pub fn process(&mut self, buf: &mut [f32]) {
        for part in buf.chunks_mut(self.max_block) {
            for v in part.iter_mut() {
                *v *= self.input_gain;
            }
            match self.bridge.as_mut() {
                None => self.net.process(part),
                Some(b) => b.process(&mut self.net, part),
            }
            for v in part.iter_mut() {
                *v *= self.output_gain;
            }
        }
    }

    /// Back to the state after silence. Never allocates.
    pub fn reset(&mut self) {
        self.net.reset();
        if let Some(b) = self.bridge.as_mut() {
            b.reset();
        }
    }
}

impl Bridge {
    fn reset(&mut self) {
        self.up.reset();
        self.down.reset();
        self.fifo.fill(0.0);
        self.read = 0;
        // the delay starts as silence
        self.count = self.latency;
    }

    fn process(&mut self, net: &mut NamNet, part: &mut [f32]) {
        let n = self.up.process(part, &mut self.model);
        net.process(&mut self.model[..n]);
        let m = self.down.process(&self.model[..n], &mut self.host);
        let cap = self.fifo.len();
        for &v in &self.host[..m] {
            // cannot overflow: the capacity covers the most the stream can run ahead (see `round_trip`)
            if self.count < cap {
                self.fifo[(self.read + self.count) % cap] = v;
                self.count += 1;
            }
        }
        for v in part.iter_mut() {
            // never empty either: the latency covers the most the stream can lag behind
            if self.count > 0 {
                *v = self.fifo[self.read];
                self.read = (self.read + 1) % cap;
                self.count -= 1;
            } else {
                *v = 0.0;
            }
        }
    }
}

/// The delay that makes host -> model -> host resampling causal, and how far the output can run ahead of the
/// input.
///
/// Host output sample `k` becomes computable when host input sample `g(k) = up.needed_input(down.needed_input(k))`
/// arrives. With a delay of `D = max(0, max_k (g(k) − k))` every output is ready when due. `g(k + P) = g(k) + P`
/// for `P` = the down resampler's up factor (the host side of the reduced ratio), so one period decides both
/// numbers.
fn round_trip(up: &Resampler, down: &Resampler) -> (usize, usize) {
    let period = down.ratio().0 as u64;
    let (mut behind, mut ahead) = (0i64, 0i64);
    for k in 0..period {
        let g = up.needed_input(down.needed_input(k)) as i64;
        behind = behind.max(g - k as i64);
        ahead = ahead.max(k as i64 - g);
    }
    (behind as usize, ahead as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A Linear model (an FIR) at `rate`: easy to compute by hand.
    fn fir(rate: u32, taps: &[f32]) -> NamModel {
        let v = json!({
            "version": "0.5.4", "architecture": "Linear", "sample_rate": rate,
            "config": {"receptive_field": taps.len(), "bias": false},
            "weights": taps,
        });
        NamModel::from_json(&v, "fir.nam", Default::default()).unwrap()
    }

    fn noise(n: usize) -> Vec<f32> {
        let mut s = 99u32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    /// The offline reference: zero-phase resample the whole signal, model, resample back.
    fn offline(model: &NamModel, x: &[f32], host: u32, quality: Quality) -> Vec<f32> {
        let whole = |input: &[f32], from: u32, to: u32| -> Vec<f32> {
            let mut r = Resampler::new(from, to, quality, 1 << 16).unwrap();
            let want = r.max_output(input.len());
            let mut padded = input.to_vec();
            padded.extend(std::iter::repeat_n(0.0, 4096));
            let mut out = vec![0.0; r.max_output(padded.len())];
            let n = r.process(&padded, &mut out);
            assert!(n >= want);
            out.truncate(want);
            out
        };
        let mut y = whole(x, host, model.sample_rate());
        model.instance().process(&mut y);
        let mut back = whole(&y, model.sample_rate(), host);
        back.truncate(x.len());
        back
    }

    #[test]
    fn equal_rates_have_no_latency() {
        let m = fir(48000, &[0.5, 1.0]);
        let mut s = NamStage::new(&m, 6.0, -6.0, 48000, 64, Quality::High).unwrap();
        assert_eq!(s.latency(), 0);
        let mut x = vec![1.0, 0.0, 0.0];
        s.process(&mut x);
        // y = 2 * (x[t] + 0.5 x[t-1]) / 2 with dB gains rounded to f32
        let g = db_gain(6.0) * db_gain(-6.0);
        assert!((x[0] - g).abs() < 1e-6 && (x[1] - 0.5 * g).abs() < 1e-6 && x[2] == 0.0, "{x:?}");
    }

    #[test]
    fn streaming_equals_offline_after_the_latency() {
        let m = fir(48000, &[0.25, -0.5, 1.0]);
        let x = noise(6000);
        for (host, quality) in [(44100, Quality::High), (44100, Quality::Scipy), (96000, Quality::High), (32000, Quality::Scipy)]
        {
            let want = offline(&m, &x, host, quality);
            for block in [1usize, 37, 256] {
                let mut s = NamStage::new(&m, 0.0, 0.0, host, block, quality).unwrap();
                let d = s.latency();
                let mut y = x.clone();
                y.extend(std::iter::repeat_n(0.0, d));
                for part in y.chunks_mut(block) {
                    s.process(part);
                }
                // the last samples differ: offline, the model output stops with the signal
                let check = x.len() - 300;
                let err = (0..check).map(|i| (y[i + d] - want[i]).abs()).fold(0.0f32, f32::max);
                assert!(err < 2e-6, "{host} Hz {quality:?} block {block}: error {err} (latency {d})");
                // the delay line starts silent
                assert!(d == 0 || y[..d].iter().all(|&v| v == 0.0));
                s.reset();
                let mut again = x.clone();
                again.extend(std::iter::repeat_n(0.0, d));
                for part in again.chunks_mut(block) {
                    s.process(part);
                }
                assert_eq!(again, y, "reset");
            }
        }
    }

    #[test]
    fn latency_is_the_smallest_causal_delay() {
        let m = fir(48000, &[1.0]);
        let up = Resampler::new(44100, 48000, Quality::High, 64).unwrap();
        let down = Resampler::new(48000, 44100, Quality::High, 64).unwrap();
        let (d, _) = round_trip(&up, &down);
        let s = NamStage::new(&m, 0.0, 0.0, 44100, 64, Quality::High).unwrap();
        assert_eq!(s.latency(), d);
        // brute force over many samples
        let worst = (0..10_000u64).map(|k| up.needed_input(down.needed_input(k)) as i64 - k as i64).max().unwrap();
        assert_eq!(d as i64, worst.max(0));
        // about 64 input samples of look-ahead in each direction
        assert!((120..=135).contains(&d), "latency {d}");
    }
}
