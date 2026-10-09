//! Streaming LSTM inference (PyTorch `nn.LSTM` cell equations, gates in the order i, f, g, o) with a linear head.
//! The state starts at, and resets to, the initial hidden and cell states stored in the file.

use crate::nam::activation::{sigmoid, tanh};
use crate::nam::file::LstmW;

struct Layer {
    input: usize,
    hidden: usize,
    /// Column-major `4H × (input + H)`: column `j` holds the weights of input element `j` for all gates.
    cols: Vec<f32>,
    bias: Vec<f32>,
    h0: Vec<f32>,
    c0: Vec<f32>,
    h: Vec<f32>,
    c: Vec<f32>,
    gates: Vec<f32>,
}

pub(crate) struct Lstm {
    layers: Vec<Layer>,
    head_weight: Vec<f32>,
    head_bias: f32,
}

impl Lstm {
    pub(crate) fn new(w: &LstmW) -> Lstm {
        let layers = w
            .layers
            .iter()
            .map(|l| {
                let (rows, ncols) = (4 * l.hidden, l.input + l.hidden);
                let mut cols = vec![0.0; rows * ncols];
                for r in 0..rows {
                    for j in 0..ncols {
                        cols[j * rows + r] = l.weight[r * ncols + j];
                    }
                }
                Layer {
                    input: l.input,
                    hidden: l.hidden,
                    cols,
                    bias: l.bias.clone(),
                    h0: l.h0.clone(),
                    c0: l.c0.clone(),
                    h: l.h0.clone(),
                    c: l.c0.clone(),
                    gates: vec![0.0; rows],
                }
            })
            .collect();
        Lstm { layers, head_weight: w.head_weight.clone(), head_bias: w.head_bias }
    }

    pub(crate) fn reset(&mut self) {
        for l in &mut self.layers {
            l.h.copy_from_slice(&l.h0);
            l.c.copy_from_slice(&l.c0);
        }
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        for v in buf.iter_mut() {
            *v = self.step(*v);
        }
    }

    fn step(&mut self, x: f32) -> f32 {
        for li in 0..self.layers.len() {
            let (done, rest) = self.layers.split_at_mut(li);
            let l = &mut rest[0];
            let hdim = l.hidden;
            let rows = 4 * hdim;
            l.gates.copy_from_slice(&l.bias);
            // input part: the sample (first layer) or the previous layer's new hidden state
            let input: &[f32] = match done.last() {
                None => std::slice::from_ref(&x),
                Some(p) => &p.h,
            };
            debug_assert_eq!(input.len(), l.input);
            for (j, &v) in input.iter().enumerate() {
                let col = &l.cols[j * rows..(j + 1) * rows];
                for (g, &w) in l.gates.iter_mut().zip(col) {
                    *g += w * v;
                }
            }
            for j in 0..hdim {
                let v = l.h[j];
                let col = &l.cols[(l.input + j) * rows..(l.input + j + 1) * rows];
                for (g, &w) in l.gates.iter_mut().zip(col) {
                    *g += w * v;
                }
            }
            let (i_f, g_o) = l.gates.split_at(2 * hdim);
            let (ig, fg) = i_f.split_at(hdim);
            let (gg, og) = g_o.split_at(hdim);
            for k in 0..hdim {
                let c = sigmoid(fg[k]) * l.c[k] + sigmoid(ig[k]) * tanh(gg[k]);
                l.c[k] = c;
                l.h[k] = sigmoid(og[k]) * tanh(c);
            }
        }
        let h = &self.layers[self.layers.len() - 1].h;
        let mut y = self.head_bias;
        for (&w, &v) in self.head_weight.iter().zip(h) {
            y += w * v;
        }
        y
    }
}
