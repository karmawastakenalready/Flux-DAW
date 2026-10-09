//! Convolution kernels on planar (channel-major) buffers.
//!
//! Every NAM layer is a 1-D convolution `y[o][t] = b[o] + Σ_k Σ_i W[o][i][k] · x[i][t − (K−1−k)·d]` (PyTorch
//! `Conv1d` weight layout, "valid" alignment so the newest tap meets the current sample). Signals are stored
//! one channel per row, so the time axis is contiguous and the kernels compute tiles of `MR` output channels ×
//! 16 consecutive samples in registers, broadcasting one weight against 16 samples per multiply-add. Columns are
//! always computed in groups of 16; buffers leave room for the extra columns, whose values are never used.
//!
//! The arithmetic is generic over [`MulAdd`] so the same code is compiled twice: with fused multiply-add under
//! `#[target_feature(enable = "avx2,fma")]` (selected at run time) and with separate multiply and add for any
//! other CPU.

/// Columns per tile.
pub(crate) const NR: usize = 16;

/// How a multiply-add is evaluated.
pub(crate) trait MulAdd: Copy {
    /// Whether the convolution kernels may use AVX2 and FMA instructions. Only true for types used exclusively
    /// inside code compiled for, and run on, CPUs with those features.
    const AVX2: bool = false;

    fn mul_add(a: f32, b: f32, c: f32) -> f32;
}

/// `a * b + c` with one rounding. Only used inside functions compiled with the `fma` target feature (otherwise
/// it would become a slow library call).
#[derive(Clone, Copy)]
pub(crate) struct Fused;

impl MulAdd for Fused {
    const AVX2: bool = true;

    #[inline(always)]
    fn mul_add(a: f32, b: f32, c: f32) -> f32 {
        a.mul_add(b, c)
    }
}

/// `a * b + c` with two roundings.
#[derive(Clone, Copy)]
pub(crate) struct Separate;

impl MulAdd for Separate {
    #[inline(always)]
    fn mul_add(a: f32, b: f32, c: f32) -> f32 {
        a * b + c
    }
}

/// A buffer of `rows` channels. Each row holds `hist` columns of history before the current position, then
/// room for the current chunk (padded to whole tiles).
#[derive(Clone, Debug)]
pub(crate) struct Plane {
    pub(crate) data: Vec<f32>,
    pub(crate) rows: usize,
    pub(crate) stride: usize,
    pub(crate) hist: usize,
    /// Column of the current chunk's first sample.
    pub(crate) pos: usize,
}

impl Plane {
    /// A plane for chunks of up to `chunk` columns (a multiple of [`NR`]) with `hist` columns of history.
    pub(crate) fn new(rows: usize, hist: usize, chunk: usize) -> Plane {
        // room for several chunks before the history has to be moved back to the front
        let stride = hist + chunk + hist.max(4 * chunk);
        Plane { data: vec![0.0; rows * stride], rows, stride, hist, pos: hist }
    }

    /// Make room for a chunk of `cols` (padded) columns at the current position.
    #[inline(always)]
    pub(crate) fn begin(&mut self, cols: usize) {
        if self.pos + cols > self.stride {
            let (from, hist) = (self.pos - self.hist, self.hist);
            for r in 0..self.rows {
                let row = &mut self.data[r * self.stride..(r + 1) * self.stride];
                row.copy_within(from..from + hist, 0);
            }
            self.pos = hist;
        }
    }

    /// Advance past `n` valid columns.
    #[inline(always)]
    pub(crate) fn end(&mut self, n: usize) {
        self.pos += n;
    }

    /// Columns `[pos + start, pos + start + len)` of `row`.
    #[inline(always)]
    pub(crate) fn cols(&self, row: usize, start: usize, len: usize) -> &[f32] {
        let s = row * self.stride + self.pos + start;
        &self.data[s..s + len]
    }

    #[inline(always)]
    pub(crate) fn cols_mut(&mut self, row: usize, start: usize, len: usize) -> &mut [f32] {
        let s = row * self.stride + self.pos + start;
        &mut self.data[s..s + len]
    }
}

/// How a convolution's output tile starts before the products are added.
#[derive(Clone, Copy)]
pub(crate) enum Init<'a> {
    /// From the bias (zero without bias).
    Bias,
    /// From the current contents of the output.
    Accumulate,
    /// From bias plus the same rows of another plane at its current position (a residual connection).
    BiasPlus(&'a Plane),
}

/// One tile row group of a packed convolution.
#[derive(Clone, Debug)]
struct Tile {
    row: usize,
    mr: usize,
    offset: usize,
}

/// Convolution weights packed for the kernels.
#[derive(Clone, Debug)]
pub(crate) struct Conv {
    pub(crate) out: usize,
    pub(crate) inp: usize,
    pub(crate) kernel: usize,
    pub(crate) dilation: usize,
    bias: Vec<f32>,
    packed: Vec<f32>,
    tiles: Vec<Tile>,
}

impl Conv {
    /// From PyTorch layout `weight[o][i][k]` (`inp / groups` inputs per output for grouped convolutions, which
    /// are expanded to dense with zeros) and an optional bias.
    pub(crate) fn new(
        out: usize,
        inp: usize,
        kernel: usize,
        dilation: usize,
        groups: usize,
        weight: &[f32],
        bias: Option<&[f32]>,
    ) -> Conv {
        let ipg = inp / groups;
        let opg = out / groups;
        debug_assert_eq!(weight.len(), out * ipg * kernel);
        let dense = |o: usize, i: usize, k: usize| -> f32 {
            let g = o / opg;
            if i / ipg == g { weight[(o * ipg + (i - g * ipg)) * kernel + k] } else { 0.0 }
        };
        let mut tiles = vec![];
        let mut packed = vec![];
        let mut row = 0;
        while row < out {
            let mr = match out - row {
                n if n >= 4 => 4,
                n if n >= 2 => 2,
                _ => 1,
            };
            tiles.push(Tile { row, mr, offset: packed.len() });
            for k in 0..kernel {
                for i in 0..inp {
                    for r in 0..mr {
                        packed.push(dense(row + r, i, k));
                    }
                }
            }
            row += mr;
        }
        Conv {
            out,
            inp,
            kernel,
            dilation,
            bias: bias.map(<[f32]>::to_vec).unwrap_or_else(|| vec![0.0; out]),
            packed,
            tiles,
        }
    }

    /// Columns of history the input needs.
    pub(crate) fn history(&self) -> usize {
        (self.kernel - 1) * self.dilation
    }

    /// Compute `cols` columns (a multiple of [`NR`]) of `dst` rows `0..out` at its position from `src` rows
    /// `0..inp` at its position.
    #[inline(always)]
    pub(crate) fn run<M: MulAdd>(&self, src: &Plane, dst: &mut Plane, cols: usize, init: Init) {
        debug_assert!(cols.is_multiple_of(NR) && src.rows >= self.inp && dst.rows >= self.out);
        debug_assert!(src.pos >= self.history());
        #[cfg(target_arch = "x86_64")]
        if M::AVX2 {
            for tile in &self.tiles {
                // SAFETY: `M::AVX2` is only true for `Fused`, which is only used after `fma_available()`
                // confirmed AVX2 and FMA.
                unsafe {
                    match tile.mr {
                        4 => self.tile_avx2::<4>(tile, src, dst, cols, init),
                        2 => self.tile_avx2::<2>(tile, src, dst, cols, init),
                        _ => self.tile_avx2::<1>(tile, src, dst, cols, init),
                    }
                }
            }
            return;
        }
        for tile in &self.tiles {
            match tile.mr {
                4 => self.tile::<M, 4>(tile, src, dst, cols, init),
                2 => self.tile::<M, 2>(tile, src, dst, cols, init),
                _ => self.tile::<M, 1>(tile, src, dst, cols, init),
            }
        }
    }

    #[inline(always)]
    fn tile<M: MulAdd, const MR: usize>(&self, tile: &Tile, src: &Plane, dst: &mut Plane, cols: usize, init: Init) {
        let w = &self.packed[tile.offset..tile.offset + self.kernel * self.inp * MR];
        let span = (self.kernel - 1) * self.dilation;
        let mut col = 0;
        while col < cols {
            let mut acc = [[0.0f32; NR]; MR];
            for (r, a) in acc.iter_mut().enumerate() {
                let o = tile.row + r;
                match init {
                    Init::Bias => *a = [self.bias[o]; NR],
                    Init::Accumulate => a.copy_from_slice(dst.cols(o, col, NR)),
                    Init::BiasPlus(p) => {
                        let b = self.bias[o];
                        for (x, &y) in a.iter_mut().zip(p.cols(o, col, NR)) {
                            *x = y + b;
                        }
                    }
                }
            }
            let mut wi = 0;
            for k in 0..self.kernel {
                // start column of tap k for this tile: pos + col − (K−1−k)·d
                let start = src.pos + col + k * self.dilation - span;
                for i in 0..self.inp {
                    let base = i * src.stride + start;
                    let x: &[f32; NR] = src.data[base..base + NR].try_into().unwrap_or(&[0.0; NR]);
                    let wr: &[f32; MR] = w[wi..wi + MR].try_into().unwrap_or(&[0.0; MR]);
                    wi += MR;
                    for r in 0..MR {
                        let wv = wr[r];
                        for j in 0..NR {
                            acc[r][j] = M::mul_add(x[j], wv, acc[r][j]);
                        }
                    }
                }
            }
            for (r, a) in acc.iter().enumerate() {
                dst.cols_mut(tile.row + r, col, NR).copy_from_slice(a);
            }
            col += NR;
        }
    }
}

impl Conv {
    /// [`Conv::tile`] with explicit AVX2 / FMA intrinsics: `MR` rows × 16 columns in `2·MR` registers. Written
    /// out so that it stays fast in unoptimised-std builds too, where iterator-based code keeps its checks.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn tile_avx2<const MR: usize>(&self, tile: &Tile, src: &Plane, dst: &mut Plane, cols: usize, init: Init) {
        use std::arch::x86_64::{__m256, _mm256_fmadd_ps, _mm256_loadu_ps, _mm256_set1_ps, _mm256_storeu_ps};
        let w = &self.packed[tile.offset..tile.offset + self.kernel * self.inp * MR];
        let span = (self.kernel - 1) * self.dilation;
        // SAFETY (all loads and stores below): every pointer comes from a bounds-checked slice of at least the
        // 16 (or 8 after the offset) floats accessed.
        let load = |s: &[f32]| -> (__m256, __m256) {
            let s = &s[..NR];
            unsafe { (_mm256_loadu_ps(s.as_ptr()), _mm256_loadu_ps(s[8..].as_ptr())) }
        };
        let mut col = 0;
        while col < cols {
            let mut lo = [_mm256_set1_ps(0.0); MR];
            let mut hi = [_mm256_set1_ps(0.0); MR];
            for r in 0..MR {
                let o = tile.row + r;
                match init {
                    Init::Bias => {
                        lo[r] = _mm256_set1_ps(self.bias[o]);
                        hi[r] = lo[r];
                    }
                    Init::Accumulate => (lo[r], hi[r]) = load(dst.cols(o, col, NR)),
                    Init::BiasPlus(p) => {
                        let b = _mm256_set1_ps(self.bias[o]);
                        let (a, c) = load(p.cols(o, col, NR));
                        lo[r] = std::arch::x86_64::_mm256_add_ps(a, b);
                        hi[r] = std::arch::x86_64::_mm256_add_ps(c, b);
                    }
                }
            }
            let mut wi = 0;
            for k in 0..self.kernel {
                let start = src.pos + col + k * self.dilation - span;
                for i in 0..self.inp {
                    let base = i * src.stride + start;
                    let (x0, x1) = load(&src.data[base..base + NR]);
                    let wr = &w[wi..wi + MR];
                    for r in 0..MR {
                        let wv = _mm256_set1_ps(wr[r]);
                        lo[r] = _mm256_fmadd_ps(x0, wv, lo[r]);
                        hi[r] = _mm256_fmadd_ps(x1, wv, hi[r]);
                    }
                    wi += MR;
                }
            }
            for r in 0..MR {
                let d = dst.cols_mut(tile.row + r, col, NR);
                unsafe {
                    _mm256_storeu_ps(d.as_mut_ptr(), lo[r]);
                    _mm256_storeu_ps(d[8..].as_mut_ptr(), hi[r]);
                }
            }
            col += NR;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(w: &[f32], b: &[f32], out: usize, inp: usize, k: usize, d: usize, groups: usize, x: &[Vec<f32>]) -> Vec<Vec<f64>> {
        let n = x[0].len();
        let (ipg, opg) = (inp / groups, out / groups);
        (0..out)
            .map(|o| {
                (0..n)
                    .map(|t| {
                        let g = o / opg;
                        let mut s = b[o] as f64;
                        for j in 0..ipg {
                            for kk in 0..k {
                                let back = (k - 1 - kk) * d;
                                if t >= back {
                                    s += w[(o * ipg + j) * k + kk] as f64 * x[g * ipg + j][t - back] as f64;
                                }
                            }
                        }
                        s
                    })
                    .collect()
            })
            .collect()
    }

    fn run_case<M: MulAdd>(out: usize, inp: usize, k: usize, d: usize, groups: usize) {
        let n = 300;
        let mut s = 7u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let w: Vec<f32> = (0..out * (inp / groups) * k).map(|_| rnd()).collect();
        let b: Vec<f32> = (0..out).map(|_| rnd()).collect();
        let x: Vec<Vec<f32>> = (0..inp).map(|_| (0..n).map(|_| rnd()).collect()).collect();
        let want = reference(&w, &b, out, inp, k, d, groups, &x);
        let conv = Conv::new(out, inp, k, d, groups, &w, Some(&b));
        let chunk = 64;
        let mut src = Plane::new(inp, conv.history(), chunk);
        let mut dst = Plane::new(out, 0, chunk);
        let mut got = vec![vec![]; out];
        let mut t = 0;
        // uneven chunks, the last tile of each padded
        for &len in [5usize, 64, 17, 1, 40, 64, 64, 45].iter().cycle() {
            if t >= n {
                break;
            }
            let len = len.min(n - t);
            let cols = len.div_ceil(NR) * NR;
            src.begin(cols);
            for (i, xi) in x.iter().enumerate() {
                src.cols_mut(i, 0, len).copy_from_slice(&xi[t..t + len]);
            }
            conv.run::<M>(&src, &mut dst, cols, Init::Bias);
            for (o, g) in got.iter_mut().enumerate() {
                g.extend_from_slice(dst.cols(o, 0, len));
            }
            src.end(len);
            t += len;
        }
        for o in 0..out {
            for t in 0..n {
                assert!((got[o][t] as f64 - want[o][t]).abs() < 1e-5, "out {o} t {t}: {} vs {}", got[o][t], want[o][t]);
            }
        }
    }

    #[test]
    fn matches_direct_convolution() {
        for &(out, inp, k, d, g) in &[(8, 8, 6, 7, 1), (3, 3, 15, 13, 1), (1, 8, 16, 1, 1), (16, 1, 1, 1, 1), (6, 4, 3, 2, 2), (7, 5, 2, 239, 1)] {
            run_case::<Separate>(out, inp, k, d, g);
            #[cfg(target_arch = "x86_64")]
            if crate::nam::wavenet::fma_available() {
                // `Fused` selects the AVX2 kernel; it is only valid on CPUs with AVX2 and FMA
                run_case::<Fused>(out, inp, k, d, g);
            }
        }
    }
}
