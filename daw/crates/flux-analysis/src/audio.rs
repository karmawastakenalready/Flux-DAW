//! Audio input: decoding WAV and FLAC files, and a borrowed stereo view that every analysis takes.
//!
//! [`AudioRef`] borrows planar left/right buffers, so a [`flux_engine::RenderOutput`] (its master or a layer
//! stem) is analysed in place without copying. Files decode into an owned [`Audio`].

use anyhow::{Context, Result, anyhow, bail, ensure};
use flux_engine::RenderOutput;
use std::path::Path;

/// Decoded audio: planar stereo `f32` (mono files are duplicated to both channels).
#[derive(Clone, Debug)]
pub struct Audio {
    pub sr: u32,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Channels in the source (only the first two are kept).
    pub source_channels: u16,
    /// Bits per sample in the source (32 for float WAV).
    pub source_bits: u16,
}

impl Audio {
    /// Stereo audio from planar buffers (lengths must match, sample rate must be non-zero).
    pub fn new(sr: u32, left: Vec<f32>, right: Vec<f32>) -> Result<Audio> {
        ensure!(sr > 0, "sample rate must be positive");
        ensure!(left.len() == right.len(), "left has {} samples, right {}", left.len(), right.len());
        Ok(Audio { sr, left, right, source_channels: 2, source_bits: 32 })
    }

    /// Mono audio (copied to both channels).
    pub fn mono(sr: u32, x: Vec<f32>) -> Result<Audio> {
        let mut a = Audio::new(sr, x.clone(), x)?;
        a.source_channels = 1;
        Ok(a)
    }

    /// Decode a WAV (PCM 8-32 bit or float) or FLAC file, chosen by its header, not its extension.
    pub fn read(path: &Path) -> Result<Audio> {
        let mut magic = [0u8; 4];
        {
            use std::io::Read;
            let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            f.read_exact(&mut magic).with_context(|| format!("reading {} (too short to be audio)", path.display()))?;
        }
        match &magic {
            b"fLaC" => read_flac(path),
            b"RIFF" | b"RF64" => read_wav(path),
            _ => bail!("{} is neither WAV nor FLAC (header {:?})", path.display(), String::from_utf8_lossy(&magic)),
        }
        .with_context(|| format!("decoding {}", path.display()))
    }

    pub fn view(&self) -> AudioRef<'_> {
        AudioRef { sr: self.sr, left: &self.left, right: &self.right }
    }

    pub fn len(&self) -> usize {
        self.left.len()
    }

    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }
}

fn read_wav(path: &Path) -> Result<Audio> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    ensure!(spec.channels > 0, "no channels");
    ensure!(spec.sample_rate > 0, "sample rate 0");
    let ch = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => {
            ensure!(spec.bits_per_sample == 32, "{}-bit float WAV is not supported", spec.bits_per_sample);
            r.samples::<f32>().collect::<std::result::Result<_, _>>()?
        }
        hound::SampleFormat::Int => {
            ensure!((8..=32).contains(&spec.bits_per_sample), "{}-bit PCM is not supported", spec.bits_per_sample);
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f64;
            r.samples::<i32>().map(|s| s.map(|v| (v as f64 * scale) as f32)).collect::<std::result::Result<_, _>>()?
        }
    };
    let (left, right) = deinterleave(&samples, ch);
    Ok(Audio { sr: spec.sample_rate, left, right, source_channels: spec.channels, source_bits: spec.bits_per_sample })
}

fn read_flac(path: &Path) -> Result<Audio> {
    let mut r = claxon::FlacReader::open(path).map_err(|e| anyhow!("{e}"))?;
    let info = r.streaminfo();
    ensure!(info.channels > 0, "no channels");
    ensure!(info.sample_rate > 0, "sample rate 0");
    let ch = info.channels as usize;
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f64;
    let cap = info.samples.unwrap_or(0) as usize;
    let mut left = Vec::with_capacity(cap);
    let mut right = Vec::with_capacity(cap);
    let mut blocks = r.blocks();
    let mut buf = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(buf).map_err(|e| anyhow!("{e}"))? {
        let a = block.channel(0);
        let b = if ch > 1 { block.channel(1) } else { a };
        left.extend(a.iter().map(|&v| (v as f64 * scale) as f32));
        right.extend(b.iter().map(|&v| (v as f64 * scale) as f32));
        buf = block.into_buffer();
    }
    Ok(Audio { sr: info.sample_rate, left, right, source_channels: info.channels as u16, source_bits: info.bits_per_sample as u16 })
}

fn deinterleave(samples: &[f32], ch: usize) -> (Vec<f32>, Vec<f32>) {
    let frames = samples.len() / ch;
    let mut l = Vec::with_capacity(frames);
    let mut r = Vec::with_capacity(frames);
    for f in 0..frames {
        l.push(samples[f * ch]);
        r.push(if ch > 1 { samples[f * ch + 1] } else { samples[f * ch] });
    }
    (l, r)
}

/// A borrowed stereo signal (planar, equal lengths).
#[derive(Clone, Copy, Debug)]
pub struct AudioRef<'a> {
    pub sr: u32,
    pub left: &'a [f32],
    pub right: &'a [f32],
}

impl<'a> AudioRef<'a> {
    /// A view of planar buffers; the lengths must match and the sample rate must be non-zero.
    pub fn new(sr: u32, left: &'a [f32], right: &'a [f32]) -> Result<AudioRef<'a>> {
        ensure!(sr > 0, "sample rate must be positive");
        ensure!(left.len() == right.len(), "left has {} samples, right {}", left.len(), right.len());
        Ok(AudioRef { sr, left, right })
    }

    /// The master of a render.
    pub fn master(out: &'a RenderOutput) -> AudioRef<'a> {
        let n = out.left.len().min(out.right.len());
        AudioRef { sr: out.sr, left: &out.left[..n], right: &out.right[..n] }
    }

    /// The layer stems of a render (`RenderOptions::layers`), named.
    pub fn layers(out: &'a RenderOutput) -> Vec<(&'a str, AudioRef<'a>)> {
        out.layers
            .iter()
            .map(|(name, (l, r))| {
                let n = l.len().min(r.len());
                (name.as_str(), AudioRef { sr: out.sr, left: &l[..n], right: &r[..n] })
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.left.len()
    }

    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }

    pub fn duration_s(&self) -> f64 {
        self.left.len() as f64 / self.sr as f64
    }

    /// Samples `a..b` (clamped to the signal).
    pub fn slice(&self, a: usize, b: usize) -> AudioRef<'a> {
        let b = b.min(self.len());
        let a = a.min(b);
        AudioRef { sr: self.sr, left: &self.left[a..b], right: &self.right[a..b] }
    }

    /// The samples of a section (clamped; times truncated to samples like the Python tools).
    pub fn section(&self, s: &crate::meta::Section) -> AudioRef<'a> {
        let (a, b) = s.samples(self.sr, self.len());
        self.slice(a, b)
    }

    /// The time range `t0..t1` in seconds (clamped).
    pub fn slice_s(&self, t0: f64, t1: f64) -> AudioRef<'a> {
        let sr = self.sr as f64;
        self.slice((t0.max(0.0) * sr).round() as usize, (t1.max(0.0) * sr).round() as usize)
    }

    /// Mid (mono) sample `(l + r) / 2`.
    #[inline]
    pub fn mid(&self, i: usize) -> f64 {
        (self.left[i] as f64 + self.right[i] as f64) * 0.5
    }

    /// Side sample `(l - r) / 2`.
    #[inline]
    pub fn side(&self, i: usize) -> f64 {
        (self.left[i] as f64 - self.right[i] as f64) * 0.5
    }

    /// The mid signal as f64.
    pub fn mid_vec(&self) -> Vec<f64> {
        (0..self.len()).map(|i| self.mid(i)).collect()
    }

    /// Largest absolute sample over both channels.
    pub fn sample_peak(&self) -> f64 {
        self.left.iter().chain(self.right.iter()).fold(0f32, |m, x| m.max(x.abs())) as f64
    }

    /// RMS of both channels together (power average of L and R).
    pub fn rms(&self) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let s: f64 = self.left.iter().chain(self.right.iter()).map(|&x| (x as f64) * (x as f64)).sum();
        (s / (2 * self.len()) as f64).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_checks_lengths() {
        assert!(AudioRef::new(44100, &[0.0; 3], &[0.0; 2]).is_err());
        assert!(AudioRef::new(0, &[0.0; 3], &[0.0; 3]).is_err());
        let a = AudioRef::new(10, &[1.0, -1.0, 0.5, 0.0], &[1.0, 1.0, 0.5, 0.0]).unwrap();
        assert_eq!(a.slice(1, 99).len(), 3);
        assert_eq!(a.slice_s(0.1, 0.3).len(), 2);
        assert_eq!(a.mid(1), 0.0);
        assert_eq!(a.side(1), -1.0);
        assert_eq!(a.sample_peak(), 1.0);
    }

    #[test]
    fn wav_round_trip_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 8000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        for v in [0i16, 16384, -16384, 32767] {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        let a = Audio::read(&p).unwrap();
        assert_eq!(a.sr, 8000);
        assert_eq!(a.source_channels, 1);
        assert_eq!(a.left, a.right);
        assert_eq!(a.left[1], 0.5);
        let bad = dir.path().join("bad.wav");
        std::fs::write(&bad, b"hello world").unwrap();
        let e = Audio::read(&bad).unwrap_err();
        assert!(format!("{e:#}").contains("neither WAV nor FLAC"));
        assert!(Audio::read(&dir.path().join("missing.flac")).is_err());
    }
}
