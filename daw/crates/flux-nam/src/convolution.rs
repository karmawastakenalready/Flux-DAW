//! The `ir` stage: impulse-response convolution with pedalboard's (`juce::dsp::Convolution`) conventions, by
//! zero-latency uniformly partitioned FFT convolution.
//!
//! Matching pedalboard (verified in `build/flux-nam_check`: `stage_check.py`, `probe_ir.py`):
//!
//! * the IR is used whole (no trimming of leading or trailing silence);
//! * an IR at another sample rate is first resampled to the processing rate as [`IrResampling`] selects: with
//!   this crate's high-quality zero-phase resampler (default), or exactly like JUCE, whose `ResamplingAudioSource`
//!   interpolates linearly and low-passes with a second-order Butterworth filter at the lower Nyquist frequency
//!   (the IR loses several dB in its top octave and gains a little phase shift there);
//! * it is then normalised to `0.125 / sqrt(E)`, where `E` is the largest energy (sum of squares, accumulated in
//!   `f32` in order, as JUCE does) of its first two channels; a mono signal is convolved with the first channel;
//! * `mix` blends linearly: `out = wet · mix + dry · (1 − mix)`, without latency.
//!
//! The convolution has no latency: every call transforms the partially filled current input block, so output
//! sample `i` only waits for input sample `i`. Cost per call is one real FFT pair of twice the partition size,
//! plus one complex multiply-add per partition per block.

use crate::error::{Error, Result};
use crate::resample::{Quality, Resampler};
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::path::Path;
use std::sync::Arc;

/// Longest impulse response accepted, in seconds at the processing rate.
pub const MAX_IR_SECONDS: f64 = 60.0;

/// Read a WAV impulse response: every channel as `f32` (integer formats scaled to ±1) and the file's rate.
pub fn read_wav(path: &Path) -> Result<(Vec<Vec<f32>>, u32)> {
    let reader = hound::WavReader::open(path).map_err(|e| match e {
        hound::Error::IoError(io) => Error::io(path, io),
        other => Error::audio(path, format!("not a readable WAV file: {other}")),
    })?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    if channels == 0 {
        return Err(Error::audio(path, "WAV file has no channels"));
    }
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => {
            if spec.bits_per_sample != 32 {
                return Err(Error::audio(path, format!("{}-bit float WAV is not supported", spec.bits_per_sample)));
            }
            reader.into_samples::<f32>().collect::<Result<_, _>>()
        }
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f64;
            reader.into_samples::<i32>().map(|s| s.map(|v| (v as f64 * scale) as f32)).collect::<Result<_, _>>()
        }
    }
    .map_err(|e| Error::audio(path, format!("cannot decode samples: {e}")))?;
    let frames = interleaved.len() / channels;
    let mut out = vec![Vec::with_capacity(frames); channels];
    for frame in interleaved.chunks_exact(channels) {
        for (c, &v) in frame.iter().enumerate() {
            out[c].push(v);
        }
    }
    Ok((out, spec.sample_rate))
}

/// How an impulse response recorded at another sample rate is brought to the processing rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IrResampling {
    /// This crate's [`Quality::High`] resampler: zero phase, flat to 20 kHz.
    #[default]
    High,
    /// JUCE's `ResamplingAudioSource`, as `juce::dsp::Convolution` (pedalboard, `rig.py`) uses it: the same
    /// result, including its loss of high frequencies and its aliasing.
    Juce,
}

/// Zero-phase offline resampling of a whole signal (output length `⌈len · to / from⌉`).
fn resample_offline(x: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    let mut r = Resampler::new(from, to, Quality::High, 4096)?;
    let n_out = r.max_output(x.len());
    let mut out = vec![0.0; r.max_output(x.len()) + 2];
    let mut written = r.process(x, &mut out);
    // flush the look-ahead with silence until every output sample of the signal exists
    let zeros = vec![0.0f32; 256];
    let mut tmp = vec![0.0; r.max_output(zeros.len())];
    while written < n_out {
        let n = r.process(&zeros, &mut tmp);
        let take = n.min(n_out - written);
        if out.len() < written + take {
            out.resize(written + take, 0.0);
        }
        out[written..written + take].copy_from_slice(&tmp[..take]);
        written += take;
    }
    out.truncate(n_out);
    Ok(out)
}

/// JUCE's `roundToInt` (round half to even, through the 2^52 + 2^51 trick).
fn round_to_int(v: f64) -> usize {
    v.round_ties_even().max(0.0) as usize
}

/// `juce::ResamplingAudioSource`'s anti-aliasing filter: a second-order Butterworth low-pass at the lower Nyquist
/// frequency, computed in `f64`, with outputs of magnitude at most 1e-8 snapped to zero (as JUCE does on x86).
struct JuceLowPass {
    c: [f64; 5],
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl JuceLowPass {
    /// `createLowPass(ratio)` and `setFilterCoefficients` (whose normalising `a0` is 1).
    fn new(ratio: f64) -> JuceLowPass {
        use std::f64::consts::{PI, SQRT_2};
        let rate = if ratio > 1.0 { 0.5 / ratio } else { 0.5 * ratio };
        let n = 1.0 / (PI * rate.max(0.001)).tan();
        let n2 = n * n;
        let c1 = 1.0 / (1.0 + SQRT_2 * n + n2);
        let c = [c1, c1 * 2.0, c1, c1 * 2.0 * (1.0 - n2), c1 * (1.0 - SQRT_2 * n + n2)];
        JuceLowPass { c, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    /// `applyFilter`.
    fn run(&mut self, buf: &mut [f32]) {
        let [c0, c1, c2, c4, c5] = self.c;
        for v in buf {
            let input = *v as f64;
            let mut out = c0 * input + c1 * self.x1 + c2 * self.x2 - c4 * self.y1 - c5 * self.y2;
            if !(out < -1.0e-8 || out > 1.0e-8) {
                out = 0.0;
            }
            (self.x2, self.x1, self.y2, self.y1) = (self.x1, input, self.y1, out);
            *v = out as f32;
        }
    }
}

/// `juce::dsp::Convolution`'s `resampleImpulseResponse` on one channel: one `ResamplingAudioSource` block of
/// `roundToInt(len · to / from)` samples, reading the IR followed by silence.
fn resample_juce(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    let ratio = from as f64 / to as f64;
    let len = round_to_int((x.len() as f64 / ratio).max(1.0));
    // the source block JUCE reads (zeros past the end of the IR); `pos + 1` below stays inside it
    let needed = round_to_int(len as f64 * ratio) + 3;
    let mut src = vec![0.0f32; needed];
    let n = needed.min(x.len());
    src[..n].copy_from_slice(&x[..n]);
    let mut lp = JuceLowPass::new(ratio);
    if ratio > 1.0001 {
        // downsampling: filter before interpolating
        lp.run(&mut src);
    }
    let mut out = Vec::with_capacity(len);
    let (mut pos, mut offset) = (0usize, 0.0f64);
    for _ in 0..len {
        let alpha = offset as f32;
        out.push(src[pos] + alpha * (src[pos + 1] - src[pos]));
        offset += ratio;
        while offset >= 1.0 {
            pos += 1;
            offset -= 1.0;
        }
    }
    if ratio < 0.9999 {
        // upsampling: filter after interpolating
        lp.run(&mut out);
    }
    out
}

/// Load an impulse response for processing at `sample_rate`: resampled if needed (see [`IrResampling`]),
/// normalised like JUCE, first channel only. `at` locates the stage for error messages.
pub fn load_ir(path: &Path, sample_rate: u32, resampling: IrResampling, at: &str) -> Result<Vec<f32>> {
    let (mut channels, file_rate) = read_wav(path)?;
    channels.truncate(2);
    if channels[0].is_empty() {
        return Err(Error::audio(path, "impulse response is empty"));
    }
    if file_rate == 0 {
        return Err(Error::audio(path, "WAV header has a sample rate of 0"));
    }
    if file_rate != sample_rate {
        for ch in &mut channels {
            *ch = match resampling {
                IrResampling::High => {
                    resample_offline(ch, file_rate, sample_rate).map_err(|e| Error::stage(at, e.to_string()))?
                }
                IrResampling::Juce => resample_juce(ch, file_rate, sample_rate),
            };
        }
    }
    let max_len = (MAX_IR_SECONDS * sample_rate as f64) as usize;
    if channels[0].len() > max_len {
        return Err(Error::audio(
            path,
            format!("impulse response is {:.1} s long; at most {MAX_IR_SECONDS} s is supported", channels[0].len() as f64 / sample_rate as f64),
        ));
    }
    // JUCE's normaliseImpulseResponse: the largest channel energy, summed in order in f32
    let energy = channels.iter().map(|ch| ch.iter().fold(0.0f32, |e, &v| e + v * v)).fold(0.0f32, f32::max);
    if !(energy > 0.0 && energy.is_finite()) {
        return Err(Error::audio(path, "impulse response is silent, too loud or not finite"));
    }
    let factor = 0.125f32 / energy.sqrt();
    let mut ir = std::mem::take(&mut channels[0]);
    for v in &mut ir {
        *v *= factor;
    }
    Ok(ir)
}

/// Zero-latency uniformly partitioned convolution of a mono signal.
pub struct Convolver {
    /// Partition (block) size; FFT size is twice that.
    block: usize,
    bins: usize,
    parts: usize,
    /// Spectra of the IR partitions (scaled by 1/FFT size), `parts × bins`.
    filters: Vec<Complex<f32>>,
    /// Spectra of the last `parts` complete input blocks (ring), `parts × bins`.
    history: Vec<Complex<f32>>,
    newest: usize,
    /// Contribution of the previous input blocks to the current output block.
    acc: Vec<Complex<f32>>,
    input: Vec<f32>,
    pos: usize,
    overlap: Vec<f32>,
    time: Vec<f32>,
    spec: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
}

impl std::fmt::Debug for Convolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Convolver").field("block", &self.block).field("parts", &self.parts).finish_non_exhaustive()
    }
}

impl Convolver {
    /// A convolver for `ir`, sized for calls of up to `max_block` samples (any length works; this only picks
    /// the partition size).
    pub fn new(ir: &[f32], max_block: usize) -> Convolver {
        let block = max_block.clamp(32, 8192).next_power_of_two();
        let n = 2 * block;
        let bins = block + 1;
        let parts = ir.len().div_ceil(block).max(1);
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let scratch_len = fwd.get_scratch_len().max(inv.get_scratch_len());
        let mut scratch = vec![Complex::default(); scratch_len];
        let mut filters = vec![Complex::default(); parts * bins];
        let mut time = vec![0.0f32; n];
        let scale = 1.0 / n as f32;
        for (p, dst) in filters.chunks_exact_mut(bins).enumerate() {
            time.fill(0.0);
            let seg = &ir[(p * block).min(ir.len())..((p + 1) * block).min(ir.len())];
            for (t, &v) in time.iter_mut().zip(seg) {
                *t = v * scale;
            }
            // lengths are right by construction, so the transform cannot fail
            let _ = fwd.process_with_scratch(&mut time, dst, &mut scratch);
        }
        Convolver {
            block,
            bins,
            parts,
            filters,
            history: vec![Complex::default(); parts * bins],
            newest: 0,
            acc: vec![Complex::default(); bins],
            input: vec![0.0; block],
            pos: 0,
            overlap: vec![0.0; block],
            time,
            spec: vec![Complex::default(); bins],
            scratch,
            fwd,
            inv,
        }
    }

    /// Partition size.
    pub fn block(&self) -> usize {
        self.block
    }

    pub fn reset(&mut self) {
        self.history.fill(Complex::default());
        self.acc.fill(Complex::default());
        self.input.fill(0.0);
        self.overlap.fill(0.0);
        self.pos = 0;
        self.newest = 0;
    }

    /// Convolve `buf` in place. Never allocates.
    pub fn process(&mut self, buf: &mut [f32]) {
        let mut done = 0;
        while done < buf.len() {
            let n = (self.block - self.pos).min(buf.len() - done);
            self.step(&mut buf[done..done + n]);
            done += n;
        }
    }

    /// Process samples within one partition.
    fn step(&mut self, buf: &mut [f32]) {
        let (pos, block, bins) = (self.pos, self.block, self.bins);
        self.input[pos..pos + buf.len()].copy_from_slice(buf);
        self.time[..block].copy_from_slice(&self.input);
        self.time[block..].fill(0.0);
        let _ = self.fwd.process_with_scratch(&mut self.time, &mut self.spec, &mut self.scratch);
        let full = pos + buf.len() == block;
        if full {
            // keep the complete block's spectrum for the following blocks
            self.newest = (self.newest + 1) % self.parts;
            self.history[self.newest * bins..(self.newest + 1) * bins].copy_from_slice(&self.spec);
        }
        let h0 = &self.filters[..bins];
        for ((s, &a), &h) in self.spec.iter_mut().zip(&self.acc).zip(h0) {
            *s = a + *s * h;
        }
        self.spec[0].im = 0.0;
        self.spec[bins - 1].im = 0.0;
        let _ = self.inv.process_with_scratch(&mut self.spec, &mut self.time, &mut self.scratch);
        for (i, v) in buf.iter_mut().enumerate() {
            *v = self.time[pos + i] + self.overlap[pos + i];
        }
        self.pos += buf.len();
        if full {
            self.overlap.copy_from_slice(&self.time[block..]);
            self.input.fill(0.0);
            self.pos = 0;
            // previous blocks' contribution to the next block: block b-p+1 meets partition p
            self.acc.fill(Complex::default());
            for p in 1..self.parts {
                let slot = (self.newest + self.parts - (p - 1)) % self.parts;
                let x = &self.history[slot * bins..(slot + 1) * bins];
                let h = &self.filters[p * bins..(p + 1) * bins];
                for ((a, &xv), &hv) in self.acc.iter_mut().zip(x).zip(h) {
                    *a += xv * hv;
                }
            }
        }
    }
}

/// The `ir` stage: convolution plus dry/wet mix.
#[derive(Debug)]
pub struct IrStage {
    conv: Convolver,
    dry_gain: f32,
    wet_gain: f32,
    dry: Vec<f32>,
}

impl IrStage {
    pub fn new(ir: &[f32], mix: f64, max_block: usize) -> IrStage {
        let wet_gain = mix as f32;
        let dry_gain = (1.0 - mix) as f32;
        let conv = Convolver::new(ir, max_block);
        let dry = if dry_gain != 0.0 { vec![0.0; conv.block()] } else { vec![] };
        IrStage { conv, dry_gain, wet_gain, dry }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        if self.dry_gain == 0.0 {
            self.conv.process(buf);
            if self.wet_gain != 1.0 {
                for v in buf.iter_mut() {
                    *v *= self.wet_gain;
                }
            }
            return;
        }
        let chunk = self.dry.len();
        for part in buf.chunks_mut(chunk) {
            let dry = &mut self.dry[..part.len()];
            dry.copy_from_slice(part);
            self.conv.process(part);
            for (w, &d) in part.iter_mut().zip(dry.iter()) {
                *w = *w * self.wet_gain + d * self.dry_gain;
            }
        }
    }

    pub fn reset(&mut self) {
        self.conv.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(x: &[f32], h: &[f32]) -> Vec<f64> {
        (0..x.len()).map(|i| (0..=i.min(h.len() - 1)).map(|k| h[k] as f64 * x[i - k] as f64).sum()).collect()
    }

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn run(c: &mut Convolver, x: &[f32], calls: &[usize]) -> Vec<f32> {
        let mut y = x.to_vec();
        let (mut i, mut k) = (0, 0);
        while i < y.len() {
            let n = calls[k % calls.len()].min(y.len() - i);
            c.process(&mut y[i..i + n]);
            i += n;
            k += 1;
        }
        y
    }

    #[test]
    fn matches_direct_convolution_for_any_call_pattern() {
        let h: Vec<f32> = noise(700, 1).iter().enumerate().map(|(i, v)| v * (-(i as f32) / 150.0).exp()).collect();
        let x = noise(5000, 2);
        let want = direct(&x, &h);
        for (max_block, calls) in [(64, vec![64]), (128, vec![1, 7, 128, 33, 100]), (256, vec![256]), (100, vec![300])] {
            let mut c = Convolver::new(&h, max_block);
            let y = run(&mut c, &x, &calls);
            let err = want.iter().zip(&y).map(|(a, b)| (a - *b as f64).abs()).fold(0.0, f64::max);
            assert!(err < 2e-5, "max_block {max_block} calls {calls:?}: error {err}");
            // after a reset the same calls give exactly the same output
            c.reset();
            assert_eq!(run(&mut c, &x, &calls), y, "after reset");
        }
    }

    fn read_f32(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
    }

    /// Against `build/flux-nam_check/probe_ir.py --fixture`, whose emulation reproduces pedalboard's
    /// `Convolution` with IRs at other rates to -134 dB or better.
    #[test]
    fn juce_resampling_matches_the_reference() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("ir_juce.json")).expect("ir_juce.json")).expect("manifest");
        let x = read_f32(&dir.join(manifest["input"].as_str().expect("input")));
        let cases = manifest["cases"].as_array().expect("cases");
        assert_eq!(cases.len(), 4);
        for case in cases {
            let rate = |k: &str| case[k].as_u64().expect("rate") as u32;
            let (from, to) = (rate("from"), rate("to"));
            let want = read_f32(&dir.join(case["file"].as_str().expect("file")));
            let got = resample_juce(&x, from, to);
            assert_eq!(got.len(), want.len(), "{from} -> {to}");
            let err = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(err == 0.0, "{from} -> {to}: max difference {err}");
        }
    }

    #[test]
    fn juce_rounding_and_lengths() {
        assert_eq!(round_to_int(2.5), 2);
        assert_eq!(round_to_int(3.5), 4);
        assert_eq!(resample_juce(&[1.0; 5], 96000, 48000).len(), 2);
        assert_eq!(resample_juce(&[1.0], 96000, 8000).len(), 1);
        assert_eq!(resample_juce(&[1.0; 441], 44100, 48000).len(), 480);
    }

    #[test]
    fn load_normalises_and_resamples() {
        let dir = std::env::temp_dir().join(format!("flux-nam-ir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write = |name: &str, rate: u32, frames: &[[f32; 2]]| {
            let p = dir.join(name);
            let spec =
                hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
            let mut w = hound::WavWriter::create(&p, spec).expect("wav");
            for f in frames {
                w.write_sample(f[0]).expect("sample");
                w.write_sample(f[1]).expect("sample");
            }
            w.finalize().expect("finalize");
            p
        };
        // the louder second channel sets the normalisation; the first is the one used
        let p = write("stereo.wav", 48000, &[[0.5, 0.0], [0.0, 2.0]]);
        assert_eq!(load_ir(&p, 48000, IrResampling::High, "t").expect("load"), vec![0.5 * 0.125 / 2.0, 0.0]);
        for mode in [IrResampling::High, IrResampling::Juce] {
            let ir = load_ir(&p, 44100, mode, "t").expect("load");
            assert!(ir.len() == 2 && ir.iter().all(|v| v.is_finite()) && ir[0] != 0.0, "{mode:?}: {ir:?}");
        }
        let silent = write("silent.wav", 48000, &[[0.0, 0.0]; 4]);
        let e = load_ir(&silent, 48000, IrResampling::High, "t").unwrap_err().to_string();
        assert!(e.contains("silent.wav") && e.contains("silent,"), "{e}");
        let e = load_ir(&dir.join("missing.wav"), 48000, IrResampling::High, "t").unwrap_err().to_string();
        assert!(e.contains("missing.wav"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rounding: against `f64` direct convolution of a 0.2 s IR at 48 kHz, for small and large partitions.
    #[test]
    fn precision_against_direct_f64() {
        let h: Vec<f32> = noise(9600, 17).iter().enumerate().map(|(i, v)| v * (-(i as f32) / 1440.0).exp()).collect();
        let x = noise(20000, 2);
        let want = direct(&x, &h);
        let power = |v: &mut dyn Iterator<Item = f64>| v.map(|e| e * e).sum::<f64>() / x.len() as f64;
        let rms = power(&mut want.iter().copied()).sqrt();
        for max_block in [64usize, 512, 4096] {
            let mut c = Convolver::new(&h, max_block);
            let mut y = x.clone();
            c.process(&mut y);
            let err_db = 10.0 * (power(&mut want.iter().zip(&y).map(|(a, b)| a - *b as f64)) / (rms * rms)).log10();
            let max = want.iter().zip(&y).map(|(a, b)| (a - *b as f64).abs()).fold(0.0, f64::max);
            // measured: about -130 dB rms, -116 to -120 dB peak
            assert!(err_db < -125.0 && max < 1e-5 * rms, "max_block {max_block}: {err_db:.1} dB rms, max {max:e}");
        }
    }

    #[test]
    fn single_partition_and_mix() {
        let h = [0.5f32, 0.25];
        let mut s = IrStage::new(&h, 0.25, 64);
        let mut x = vec![1.0f32, 0.0, 0.0, 2.0];
        s.process(&mut x);
        // wet = [0.5, 0.25, 0, 1.0]; out = 0.25 wet + 0.75 dry
        let want = [0.25 * 0.5 + 0.75, 0.25 * 0.25, 0.0, 0.25 * 1.0 + 1.5];
        for (a, b) in x.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{x:?}");
        }
    }
}
