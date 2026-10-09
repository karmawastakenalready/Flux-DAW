//! Streaming WaveNet inference.
//!
//! The input is processed in chunks of up to [`CHUNK`] samples. Each layer keeps the history of its input that
//! its dilated convolution reaches back to, so streaming gives exactly the result of running the whole signal
//! at once. The network starts (and [`WaveNet::reset`] returns) in the state reached after `receptive_field − 1`
//! samples of silence, which is what PyTorch's `pad_start=True` computes.

use crate::nam::activation::Activation;
use crate::nam::file::{ArrayW, ConvW, FilmW, LayerAct, WaveNetW};
use crate::nam::kernel::{Conv, Init, MulAdd, NR, Plane};

/// Samples per internal chunk (a multiple of [`NR`]).
pub(crate) const CHUNK: usize = 128;

fn conv(w: &ConvW) -> Conv {
    Conv::new(w.out, w.inp, w.kernel, w.dilation, w.groups, &w.weight, w.bias.as_deref())
}

struct Film {
    conv: Conv,
    shift: bool,
    dim: usize,
}

impl Film {
    fn new(w: &FilmW) -> Film {
        Film { conv: conv(&w.conv), shift: w.shift, dim: w.dim }
    }

    /// `dst[r] = scale[r] · src[r] + shift[r]` with `[scale; shift]` computed from the condition into `tmp`.
    /// `src` may be `None` to transform `dst` in place.
    #[inline(always)]
    fn apply<M: MulAdd>(&self, cond: &Plane, src: Option<&Plane>, dst: &mut Plane, tmp: &mut Plane, cols: usize) {
        self.conv.run::<M>(cond, tmp, cols, Init::Bias);
        for r in 0..self.dim {
            let scale = tmp.cols(r, 0, cols);
            let shift = if self.shift { Some(tmp.cols(self.dim + r, 0, cols)) } else { None };
            // copy the source row first when it is another plane
            if let Some(s) = src {
                let row: &[f32] = s.cols(r, 0, cols);
                dst.cols_mut(r, 0, cols).copy_from_slice(row);
            }
            let d = dst.cols_mut(r, 0, cols);
            match shift {
                Some(sh) => {
                    for ((x, &a), &b) in d.iter_mut().zip(scale).zip(sh) {
                        *x = a * *x + b;
                    }
                }
                None => {
                    for (x, &a) in d.iter_mut().zip(scale) {
                        *x *= a;
                    }
                }
            }
        }
    }
}

struct Layer {
    conv: Conv,
    mixer: Conv,
    act: LayerAct,
    bottleneck: usize,
    layer1x1: Option<Conv>,
    head1x1: Option<Conv>,
    /// Indexed like `file::FILM_KEYS`.
    films: [Option<Film>; 8],
    /// Whether the residual output is used (not for the last layer of the last array).
    residual: bool,
}

struct Array {
    rechannel: Conv,
    layers: Vec<Layer>,
    /// Input of each layer (the residual stream), with the history its convolution needs unless the layer
    /// convolves a FiLM-modulated copy (`prefilm`).
    inputs: Vec<Plane>,
    prefilm: Vec<Option<Plane>>,
    head_acc: Plane,
    head_rechannel: Conv,
    head_out: Plane,
    /// Residual output of the last layer, input of the next array.
    out_x: Option<Plane>,
}

struct HeadBlock {
    act: Activation,
    input: Plane,
    conv: Conv,
    out: Plane,
}

/// Work buffers shared by all layers.
struct Work {
    cond: Plane,
    cond2: Plane,
    z: Plane,
    a: Plane,
    tmp: Plane,
    film: Plane,
}

/// A WaveNet ready to stream.
pub(crate) struct WaveNet {
    arrays: Vec<Array>,
    head: Vec<HeadBlock>,
    head_scale: f32,
    work: Work,
    receptive_field: usize,
    fma: bool,
    /// State after the silent lead-in, for [`WaveNet::reset`].
    rest: Vec<f32>,
    rest_pos: Vec<usize>,
}

impl WaveNet {
    pub(crate) fn new(w: &WaveNetW) -> WaveNet {
        let n_arrays = w.arrays.len();
        let mut max_rows = 1;
        let mut arrays = vec![];
        for (ai, aw) in w.arrays.iter().enumerate() {
            let last_array = ai + 1 == n_arrays;
            arrays.push(build_array(aw, last_array, &mut max_rows));
        }
        let mut head = vec![];
        for (act, cw) in &w.head {
            let c = conv(cw);
            max_rows = max_rows.max(c.out).max(c.inp);
            head.push(HeadBlock { act: act.clone(), input: Plane::new(c.inp, c.history(), CHUNK), out: Plane::new(c.out, 0, CHUNK), conv: c });
        }
        let work = Work {
            cond: Plane::new(1, 0, CHUNK),
            cond2: Plane::new(1, 0, CHUNK),
            z: Plane::new(max_rows, 0, CHUNK),
            a: Plane::new(max_rows, 0, CHUNK),
            tmp: Plane::new(max_rows, 0, CHUNK),
            film: Plane::new(2 * max_rows, 0, CHUNK),
        };
        let mut net = WaveNet {
            arrays,
            head,
            head_scale: w.head_scale,
            work,
            receptive_field: w.receptive_field(),
            fma: fma_available(),
            rest: vec![],
            rest_pos: vec![],
        };
        // the silent lead-in (PyTorch pads receptive_field − 1 zeros), then remember that state
        let mut silence = vec![0.0f32; CHUNK];
        let mut left = net.receptive_field - 1;
        while left > 0 {
            let n = left.min(CHUNK);
            net.process(&mut silence[..n]);
            silence.fill(0.0);
            left -= n;
        }
        let (mut rest, mut rest_pos) = (vec![], vec![]);
        net.for_each_plane(|p| {
            rest.extend_from_slice(&p.data);
            rest_pos.push(p.pos);
        });
        net.rest = rest;
        net.rest_pos = rest_pos;
        net
    }

    pub(crate) fn receptive_field(&self) -> usize {
        self.receptive_field
    }

    /// Back to the state after silence. Never allocates.
    pub(crate) fn reset(&mut self) {
        let rest = std::mem::take(&mut self.rest);
        let rest_pos = std::mem::take(&mut self.rest_pos);
        let (mut off, mut i) = (0, 0);
        self.for_each_plane(|p| {
            let len = p.data.len();
            p.data.copy_from_slice(&rest[off..off + len]);
            off += len;
            p.pos = rest_pos[i];
            i += 1;
        });
        self.rest = rest;
        self.rest_pos = rest_pos;
    }

    fn for_each_plane(&mut self, mut f: impl FnMut(&mut Plane)) {
        for a in &mut self.arrays {
            a.inputs.iter_mut().for_each(&mut f);
            a.prefilm.iter_mut().flatten().for_each(&mut f);
            f(&mut a.head_acc);
            f(&mut a.head_out);
            if let Some(p) = a.out_x.as_mut() {
                f(p);
            }
        }
        for h in &mut self.head {
            f(&mut h.input);
            f(&mut h.out);
        }
    }

    /// Process `buf` in place (input samples in, output samples out). Never allocates.
    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        for chunk in buf.chunks_mut(CHUNK) {
            #[cfg(target_arch = "x86_64")]
            if self.fma {
                // SAFETY: `fma` is only true when the CPU supports AVX2 and FMA (checked at construction).
                unsafe { self.chunk_avx2(chunk) };
                continue;
            }
            self.chunk::<crate::nam::kernel::Separate>(chunk);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn chunk_avx2(&mut self, x: &mut [f32]) {
        self.chunk::<crate::nam::kernel::Fused>(x)
    }

    #[inline(always)]
    fn chunk<M: MulAdd>(&mut self, x: &mut [f32]) {
        let n = x.len();
        let cols = n.div_ceil(NR) * NR;
        let WaveNet { arrays, head, head_scale, work, .. } = self;
        let cond_row = work.cond.cols_mut(0, 0, cols);
        cond_row[..n].copy_from_slice(x);
        cond_row[n..].fill(0.0);
        for ai in 0..arrays.len() {
            let (prev, rest) = arrays.split_at_mut(ai);
            let arr = &mut rest[0];
            let prev = prev.last();
            // layer-array input: the model input, or the previous array's residual output
            let src = match prev {
                None => &work.cond,
                Some(p) => p.out_x.as_ref().unwrap_or(&work.cond),
            };
            arr.inputs[0].begin(cols);
            arr.rechannel.run::<M>(src, &mut arr.inputs[0], cols, Init::Bias);
            arr.head_acc.begin(cols);
            for r in 0..arr.head_acc.rows {
                let dst = arr.head_acc.cols_mut(r, 0, cols);
                match prev {
                    None => dst.fill(0.0),
                    Some(p) => dst.copy_from_slice(p.head_out.cols(r, 0, cols)),
                }
            }
            let n_layers = arr.layers.len();
            for li in 0..n_layers {
                run_layer::<M>(arr, li, work, cols);
            }
            arr.head_rechannel.run::<M>(&arr.head_acc, &mut arr.head_out, cols, Init::Bias);
            for p in &mut arr.inputs {
                p.end(n);
            }
            for p in arr.prefilm.iter_mut().flatten() {
                p.end(n);
            }
            arr.head_acc.end(n);
        }
        let last = arrays.last().map(|a| &a.head_out);
        let Some(last) = last else { return };
        if head.is_empty() {
            for (y, &h) in x.iter_mut().zip(last.cols(0, 0, n)) {
                *y = *head_scale * h;
            }
            return;
        }
        for bi in 0..head.len() {
            let (done, rest) = head.split_at_mut(bi);
            let blk = &mut rest[0];
            blk.input.begin(cols);
            for r in 0..blk.input.rows {
                let dst = blk.input.cols_mut(r, 0, cols);
                match done.last() {
                    None => {
                        for (d, &s) in dst.iter_mut().zip(last.cols(r, 0, cols)) {
                            *d = *head_scale * s;
                        }
                    }
                    Some(p) => dst.copy_from_slice(p.out.cols(r, 0, cols)),
                }
                blk.act.apply(dst, r);
            }
            blk.conv.run::<M>(&blk.input, &mut blk.out, cols, Init::Bias);
            blk.input.end(n);
        }
        if let Some(h) = head.last() {
            x.copy_from_slice(h.out.cols(0, 0, n));
        }
    }
}

fn build_array(aw: &ArrayW, last_array: bool, max_rows: &mut usize) -> Array {
    let n = aw.layers.len();
    let mut layers = vec![];
    let mut inputs = vec![];
    let mut prefilm = vec![];
    for (li, lw) in aw.layers.iter().enumerate() {
        let c = conv(&lw.conv);
        let films: [Option<Film>; 8] = std::array::from_fn(|j| lw.films[j].as_ref().map(Film::new));
        let pre = films[0].is_some();
        inputs.push(Plane::new(aw.channels, if pre { 0 } else { c.history() }, CHUNK));
        prefilm.push(pre.then(|| Plane::new(aw.channels, c.history(), CHUNK)));
        *max_rows = (*max_rows).max(c.out).max(aw.channels).max(aw.condition_size);
        if let Some(h) = &lw.head1x1 {
            *max_rows = (*max_rows).max(h.out);
        }
        layers.push(Layer {
            conv: c,
            mixer: conv(&lw.mixer),
            act: lw.act.clone(),
            bottleneck: aw.bottleneck,
            layer1x1: lw.layer1x1.as_ref().map(conv),
            head1x1: lw.head1x1.as_ref().map(conv),
            films,
            residual: !(last_array && li + 1 == n),
        });
    }
    let head_rechannel = conv(&aw.head_rechannel);
    *max_rows = (*max_rows).max(head_rechannel.out).max(head_rechannel.inp);
    Array {
        rechannel: conv(&aw.rechannel),
        layers,
        inputs,
        prefilm,
        head_acc: Plane::new(head_rechannel.inp, head_rechannel.history(), CHUNK),
        head_out: Plane::new(head_rechannel.out, 0, CHUNK),
        out_x: (!last_array).then(|| Plane::new(aw.channels, 0, CHUNK)),
        head_rechannel,
    }
}

/// One layer for one chunk: convolution, input mixing, activation, head contribution and residual output.
#[inline(always)]
fn run_layer<M: MulAdd>(arr: &mut Array, li: usize, w: &mut Work, cols: usize) {
    let Array { layers, inputs, prefilm, head_acc, out_x, .. } = arr;
    let layer = &layers[li];
    let (cur, next) = inputs.split_at_mut(li + 1);
    let x = &cur[li];
    let [conv_pre, conv_post, mix_pre, mix_post, act_pre, act_post, l1_post, h1_post] = &layer.films;

    // dilated convolution, of the input or of its FiLM-modulated copy
    let src: &Plane = match (conv_pre, prefilm[li].as_mut()) {
        (Some(f), Some(p)) => {
            p.begin(cols);
            f.apply::<M>(&w.cond, Some(x), p, &mut w.film, cols);
            p
        }
        _ => x,
    };
    layer.conv.run::<M>(src, &mut w.z, cols, Init::Bias);
    if let Some(f) = conv_post {
        f.apply::<M>(&w.cond, None, &mut w.z, &mut w.film, cols);
    }

    // input mixer (1x1 from the condition)
    if mix_pre.is_none() && mix_post.is_none() {
        layer.mixer.run::<M>(&w.cond, &mut w.z, cols, Init::Accumulate);
    } else {
        let csrc = match mix_pre {
            Some(f) => {
                f.apply::<M>(&w.cond, Some(&w.cond), &mut w.cond2, &mut w.film, cols);
                &w.cond2
            }
            None => &w.cond,
        };
        layer.mixer.run::<M>(csrc, &mut w.tmp, cols, Init::Bias);
        if let Some(f) = mix_post {
            f.apply::<M>(&w.cond, None, &mut w.tmp, &mut w.film, cols);
        }
        for r in 0..layer.mixer.out {
            let add = w.tmp.cols(r, 0, cols);
            for (z, &m) in w.z.cols_mut(r, 0, cols).iter_mut().zip(add) {
                *z += m;
            }
        }
    }
    if let Some(f) = act_pre {
        f.apply::<M>(&w.cond, None, &mut w.z, &mut w.film, cols);
    }

    // activation: z (bottleneck, or twice that when paired) -> a (bottleneck)
    let b = layer.bottleneck;
    for r in 0..b {
        match &layer.act {
            LayerAct::Single(act) => {
                let src = w.z.cols(r, 0, cols);
                let dst = w.a.cols_mut(r, 0, cols);
                dst.copy_from_slice(src);
                act.apply(dst, r);
            }
            LayerAct::Gated(p, s) => {
                p.apply(w.z.cols_mut(r, 0, cols), r);
                s.apply(w.z.cols_mut(b + r, 0, cols), r);
                let (x1, x2) = (w.z.cols(r, 0, cols), w.z.cols(b + r, 0, cols));
                for ((d, &pv), &sv) in w.a.cols_mut(r, 0, cols).iter_mut().zip(x1).zip(x2) {
                    *d = pv * sv;
                }
            }
            LayerAct::Blended(p, s) => {
                w.a.cols_mut(r, 0, cols).copy_from_slice(w.z.cols(r, 0, cols));
                p.apply(w.a.cols_mut(r, 0, cols), r);
                s.apply(w.z.cols_mut(b + r, 0, cols), r);
                let (x1, x2) = (w.z.cols(r, 0, cols), w.z.cols(b + r, 0, cols));
                for ((d, &xv), &sv) in w.a.cols_mut(r, 0, cols).iter_mut().zip(x1).zip(x2) {
                    *d = sv * *d + (1.0 - sv) * xv;
                }
            }
        }
    }
    if let Some(f) = act_post {
        f.apply::<M>(&w.cond, None, &mut w.a, &mut w.film, cols);
    }

    // head path
    match &layer.head1x1 {
        Some(h) => {
            h.run::<M>(&w.a, &mut w.tmp, cols, Init::Bias);
            if let Some(f) = h1_post {
                f.apply::<M>(&w.cond, None, &mut w.tmp, &mut w.film, cols);
            }
            add_rows(head_acc, &w.tmp, h.out, cols);
        }
        None => add_rows(head_acc, &w.a, b, cols),
    }

    // residual output: next layer's input, or the array output
    if !layer.residual {
        return;
    }
    let dst: &mut Plane = match next.first_mut() {
        Some(p) => p,
        None => match out_x.as_mut() {
            Some(p) => p,
            None => return,
        },
    };
    dst.begin(cols);
    match &layer.layer1x1 {
        Some(l1) if l1_post.is_none() => l1.run::<M>(&w.a, dst, cols, Init::BiasPlus(x)),
        Some(l1) => {
            l1.run::<M>(&w.a, &mut w.tmp, cols, Init::Bias);
            if let Some(f) = l1_post {
                f.apply::<M>(&w.cond, None, &mut w.tmp, &mut w.film, cols);
            }
            for r in 0..dst.rows {
                let add = w.tmp.cols(r, 0, cols);
                let xr = x.cols(r, 0, cols);
                for ((d, &a), &xv) in dst.cols_mut(r, 0, cols).iter_mut().zip(add).zip(xr) {
                    *d = xv + a;
                }
            }
        }
        None => {
            for r in 0..dst.rows {
                let xr = x.cols(r, 0, cols);
                let ar = w.a.cols(r, 0, cols);
                for ((d, &a), &xv) in dst.cols_mut(r, 0, cols).iter_mut().zip(ar).zip(xr) {
                    *d = xv + a;
                }
            }
        }
    }
}

/// `dst[r] += src[r]` for rows `0..rows`.
#[inline(always)]
fn add_rows(dst: &mut Plane, src: &Plane, rows: usize, cols: usize) {
    for r in 0..rows {
        let s = src.cols(r, 0, cols);
        for (d, &v) in dst.cols_mut(r, 0, cols).iter_mut().zip(s) {
            *d += v;
        }
    }
}

/// Whether the fused AVX2 path can run on this CPU.
pub(crate) fn fma_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}
