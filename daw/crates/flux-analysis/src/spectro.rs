//! Log-frequency spectrograms as PNG images (the port of `tools/spectro.py`).
//!
//! Each signal becomes one band of the image (stacked top to bottom): 30 Hz - 16 kHz on a log axis, an 80 dB
//! range in the Flux theme's dark-brown-to-cream palette, a blue grid line every second and at 50, 100, 200,
//! 500 Hz, 1, 2, 5 and 10 kHz, and a white two-pixel separator under each band. The pixels match the Python tool
//! (same window, hop, scaling, row mapping and colour arithmetic); only the PNG compression differs.

use crate::audio::AudioRef;
use crate::num::{hanning, rfft, rfft_bin_hz};
use anyhow::{Context, Result, ensure};
use std::path::Path;

/// Spectrogram geometry and scale. The defaults are the Python tool's.
#[derive(Clone, Debug, PartialEq)]
pub struct SpectroOptions {
    /// Image width in pixels (one FFT per column).
    pub width: usize,
    /// Height of each band in pixels.
    pub height: usize,
    pub fmin_hz: f64,
    pub fmax_hz: f64,
    /// Level range shown, in dB below full scale (0 dBFS sine = top colour).
    pub range_db: f64,
    /// FFT length (the Python tool's `SPECTRO_N`).
    pub fft_size: usize,
}

impl Default for SpectroOptions {
    fn default() -> Self {
        SpectroOptions { width: 1400, height: 220, fmin_hz: 30.0, fmax_hz: 16000.0, range_db: 80.0, fft_size: 4096 }
    }
}

/// An 8-bit RGB image, rows top to bottom.
#[derive(Clone, Debug, PartialEq)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    /// `height * width * 3` bytes.
    pub data: Vec<u8>,
}

impl RgbImage {
    fn new(width: usize, height: usize) -> RgbImage {
        RgbImage { width, height, data: vec![0; width * height * 3] }
    }

    fn px(&mut self, row: usize, col: usize) -> &mut [u8] {
        let i = (row * self.width + col) * 3;
        &mut self.data[i..i + 3]
    }

    /// The pixel at (`row`, `col`).
    pub fn get(&self, row: usize, col: usize) -> [u8; 3] {
        let i = (row * self.width + col) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    /// Stack images of the same width vertically.
    pub fn stack(images: Vec<RgbImage>) -> Result<RgbImage> {
        ensure!(!images.is_empty(), "nothing to stack");
        let width = images[0].width;
        ensure!(images.iter().all(|i| i.width == width), "images of different widths cannot be stacked");
        let height = images.iter().map(|i| i.height).sum();
        let mut data = Vec::with_capacity(width * height * 3);
        for i in images {
            data.extend_from_slice(&i.data);
        }
        Ok(RgbImage { width, height, data })
    }

    /// The image as PNG bytes.
    pub fn encode_png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let w = u32::try_from(self.width).context("image too wide")?;
            let h = u32::try_from(self.height).context("image too tall")?;
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().context("writing the PNG header")?;
            wr.write_image_data(&self.data).context("encoding the PNG")?;
        }
        Ok(out)
    }

    /// Write the image as a PNG file.
    pub fn write_png(&self, path: &Path) -> Result<()> {
        let bytes = self.encode_png()?;
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }
}

const STOPS: [[f64; 3]; 5] = [[24.0, 18.0, 14.0], [92.0, 52.0, 34.0], [184.0, 110.0, 60.0], [236.0, 196.0, 120.0], [252.0, 246.0, 228.0]];
const GRID: [f64; 3] = [90.0, 140.0, 200.0];

/// 0..1 -> dark brown .. cream (numpy's float-to-uint8 cast truncates).
fn colour(v: f64) -> [u8; 3] {
    let x = v.clamp(0.0, 1.0) * (STOPS.len() - 1) as f64;
    let i = (x as usize).min(STOPS.len() - 2);
    let f = x - i as f64;
    let mut c = [0u8; 3];
    for (k, out) in c.iter_mut().enumerate() {
        *out = (STOPS[i][k] * (1.0 - f) + STOPS[i + 1][k] * f) as u8;
    }
    c
}

/// numpy's `geomspace(start, stop, num)` (via `logspace` of a `linspace`, with exact endpoints).
fn geomspace(start: f64, stop: f64, num: usize) -> Vec<f64> {
    if num == 0 {
        return vec![];
    }
    let (ls, le) = (start.log10(), stop.log10());
    let div = (num - 1).max(1) as f64;
    let step = (le - ls) / div;
    let mut v: Vec<f64> = (0..num).map(|i| 10f64.powf(i as f64 * step + ls)).collect();
    v[0] = start;
    if num > 1 {
        v[num - 1] = stop;
    }
    v
}

/// One band: the spectrogram of the mid signal of `a` between `t0` and `t1` seconds.
pub fn spectrogram_band(a: AudioRef, t0: f64, t1: f64, o: &SpectroOptions) -> Result<RgbImage> {
    ensure!(t0.is_finite() && t1.is_finite() && t0 >= 0.0 && t1 > t0, "time range {t0}..{t1} s is not a forward range from 0 on");
    ensure!(o.width > 0 && o.height > 2, "the image must be at least 1 x 3 pixels");
    ensure!(o.fft_size >= 16, "FFT size {} is too small", o.fft_size);
    ensure!(o.fmin_hz > 0.0 && o.fmax_hz > o.fmin_hz, "frequency range {}..{} Hz is not increasing from above 0", o.fmin_hz, o.fmax_hz);
    ensure!(o.range_db > 0.0, "the level range must be positive");
    let sr = a.sr as f64;
    let (w, h, n) = (o.width, o.height, o.fft_size);
    let i0 = ((t0 * sr) as usize).min(a.len());
    let i1 = ((t1 * sr) as usize).min(a.len()).max(i0);
    let m: Vec<f64> = (i0..i1).map(|i| a.mid(i)).collect();
    // Python: max(1, (len(m) - n) // W), with floor division
    let hop = ((m.len() as i64 - n as i64).div_euclid(w as i64)).max(1) as usize;
    let win = hanning(n);
    let rows = geomspace(o.fmax_hz, o.fmin_hz, h);
    let nbins = n / 2 + 1;
    // np.searchsorted(freqs, rows) (left side), clipped to the last bin
    let idx: Vec<usize> = rows
        .iter()
        .map(|&f| (0..nbins).find(|&k| rfft_bin_hz(k, n, sr) >= f).unwrap_or(nbins).min(nbins - 1))
        .collect();
    let mut img = RgbImage::new(w, h);
    let mut seg = vec![0.0; n];
    for c in 0..w {
        let s = (c * hop).min(m.len());
        let e = (c * hop + n).min(m.len());
        for (j, v) in seg.iter_mut().enumerate() {
            *v = if s + j < e { m[s + j] * win[j] } else { 0.0 };
        }
        let spec = rfft(&seg, n);
        for (r, &k) in idx.iter().enumerate() {
            let db = 20.0 * (spec[k].norm() / (n as f64 / 4.0) + 1e-9).log10();
            img.px(r, c).copy_from_slice(&colour((db + o.range_db) / o.range_db));
        }
    }
    // a grid line every whole second
    let mut s = t0.ceil();
    while s < t1 {
        let c = ((s - t0) / (t1 - t0) * w as f64) as i64;
        if (0..w as i64).contains(&c) {
            for r in 0..h {
                let p = img.px(r, c as usize);
                for k in 0..3 {
                    p[k] = (p[k] as f64 * 0.6 + GRID[k] * 0.4) as u8;
                }
            }
        }
        s += 1.0;
    }
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
        let mut r = 0;
        for (i, &v) in rows.iter().enumerate() {
            if (v - f).abs() < (rows[r] - f).abs() {
                r = i;
            }
        }
        for c in (0..w).step_by(4) {
            img.px(r, c).copy_from_slice(&GRID.map(|v| v as u8));
        }
    }
    for r in h - 2..h {
        for c in 0..w {
            img.px(r, c).copy_from_slice(&[255, 255, 255]);
        }
    }
    Ok(img)
}

/// Spectrograms of several signals over the same time range, stacked top to bottom.
pub fn spectrogram(bands: &[AudioRef], t0: f64, t1: f64, o: &SpectroOptions) -> Result<RgbImage> {
    ensure!(!bands.is_empty(), "no signals to draw");
    let imgs = bands
        .iter()
        .enumerate()
        .map(|(i, a)| spectrogram_band(*a, t0, t1, o).with_context(|| format!("band {}", i + 1)))
        .collect::<Result<Vec<_>>>()?;
    RgbImage::stack(imgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_and_geomspace_follow_numpy() {
        assert_eq!(colour(0.0), [24, 18, 14]);
        assert_eq!(colour(1.0), [252, 246, 228]);
        assert_eq!(colour(-3.0), [24, 18, 14]);
        assert_eq!(colour(0.125), [58, 35, 24]);
        let g = geomspace(16000.0, 30.0, 220);
        assert_eq!((g[0], g[219]), (16000.0, 30.0));
        assert!(g.windows(2).all(|w| w[1] < w[0]));
    }

    #[test]
    fn a_sine_lights_its_row() {
        let sr = 44100;
        let x: Vec<f32> = (0..sr * 2).map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / sr as f64).sin() as f32 * 0.5).collect();
        let a = AudioRef::new(sr as u32, &x, &x).unwrap();
        let o = SpectroOptions { width: 64, height: 60, ..Default::default() };
        let img = spectrogram_band(a, 0.0, 2.0, &o).unwrap();
        let rows = geomspace(o.fmax_hz, o.fmin_hz, o.height);
        let r1k = (0..o.height).min_by(|&i, &j| (rows[i] - 1000.0).abs().total_cmp(&(rows[j] - 1000.0).abs())).unwrap();
        let r100 = (0..o.height).min_by(|&i, &j| (rows[i] - 100.0).abs().total_cmp(&(rows[j] - 100.0).abs())).unwrap();
        // column 1 avoids the grid's dotted rows (every 4th column) and the 1-second line (column 32)
        let bright = img.get(r1k, 1);
        let dark = img.get(r100, 1);
        assert!(bright[0] > 230, "{bright:?}");
        assert!(dark[0] < 40, "{dark:?}");
        assert_eq!(img.get(o.height - 1, 5), [255, 255, 255]);
        let png = img.encode_png().unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let both = spectrogram(&[a, a], 0.0, 2.0, &o).unwrap();
        assert_eq!(both.height, 120);
        assert!(spectrogram_band(a, 1.0, 0.5, &o).is_err());
        assert!(spectrogram(&[], 0.0, 1.0, &o).is_err());
    }
}
