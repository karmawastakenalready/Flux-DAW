//! Parsing `.nam` files into network descriptions with their weights.
//!
//! Supported, following the `neural-amp-modeler` Python package (v0.13, `nam.models._from_nam`):
//!
//! * `WaveNet` in the A2 export format (version 0.7: per-layer `kernel_sizes`, `head` rechannel objects,
//!   `bottleneck`, `layer1x1` / `head1x1`, grouped convolutions, gating / blending, the eight FiLM positions,
//!   and an optional output `head`), and in the A1 format (versions 0.5.x: `head_size`, `gated`, `head_bias`),
//!   which is mapped onto the same structure;
//! * `LSTM` (`input_size` 1, any number of layers);
//! * `Linear` (an FIR filter);
//! * `SlimmableContainer` (A2): one of its submodels, chosen by [`SubmodelChoice`].
//!
//! Not supported (clear errors): `condition_dsp` (the Python loader ignores it too, so there is no reference to
//! verify against), `packing`, `ConvNet` and other architectures.

use crate::error::{Error, Result};
use crate::json::{Domain, Obj, describe};
use crate::nam::activation::Activation;
use serde_json::Value;

/// Which submodel of an A2 `SlimmableContainer` to use.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SubmodelChoice {
    /// The submodel with the largest `max_value` (the first of equals), as `rig.py` does: the full model.
    #[default]
    Largest,
    /// The submodel at this index in the file.
    Index(usize),
    /// The smallest submodel whose `max_value` is at least this (0..=1); the largest if none is.
    Size(f64),
}

/// A 1-D convolution's shape and weights in PyTorch layout.
#[derive(Clone, Debug)]
pub(crate) struct ConvW {
    pub(crate) out: usize,
    pub(crate) inp: usize,
    pub(crate) kernel: usize,
    pub(crate) dilation: usize,
    pub(crate) groups: usize,
    pub(crate) weight: Vec<f32>,
    pub(crate) bias: Option<Vec<f32>>,
}

impl ConvW {
    fn shape(out: usize, inp: usize, kernel: usize, dilation: usize, groups: usize, bias: bool) -> ConvW {
        ConvW { out, inp, kernel, dilation, groups, weight: vec![0.0; out * (inp / groups) * kernel], bias: bias.then(|| vec![0.0; out]) }
    }

    fn read(&mut self, w: &mut Weights) -> Result<()> {
        let n = self.weight.len();
        self.weight.copy_from_slice(w.take(n)?);
        if let Some(b) = self.bias.as_mut() {
            let n = b.len();
            b.copy_from_slice(w.take(n)?);
        }
        Ok(())
    }
}

/// FiLM: `x · scale(c) + shift(c)` with `[scale; shift] = conv1x1(c)`.
#[derive(Clone, Debug)]
pub(crate) struct FilmW {
    pub(crate) conv: ConvW,
    pub(crate) shift: bool,
    pub(crate) dim: usize,
}

/// FiLM positions of a layer, in weight order.
pub(crate) const FILM_KEYS: [&str; 8] = [
    "conv_pre_film",
    "conv_post_film",
    "input_mixin_pre_film",
    "input_mixin_post_film",
    "activation_pre_film",
    "activation_post_film",
    "layer1x1_post_film",
    "head1x1_post_film",
];

/// The activation of a layer: single, or paired over the two halves of the convolution output.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LayerAct {
    Single(Activation),
    /// `primary(x1) · secondary(x2)`.
    Gated(Activation, Activation),
    /// `s · primary(x1) + (1 − s) · x1` with `s = secondary(x2)`.
    Blended(Activation, Activation),
}

impl LayerAct {
    pub(crate) fn paired(&self) -> bool {
        !matches!(self, LayerAct::Single(_))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LayerW {
    pub(crate) conv: ConvW,
    pub(crate) mixer: ConvW,
    pub(crate) act: LayerAct,
    pub(crate) layer1x1: Option<ConvW>,
    pub(crate) head1x1: Option<ConvW>,
    /// Indexed like [`FILM_KEYS`].
    pub(crate) films: [Option<FilmW>; 8],
}

#[derive(Clone, Debug)]
pub(crate) struct ArrayW {
    pub(crate) input_size: usize,
    pub(crate) condition_size: usize,
    pub(crate) channels: usize,
    pub(crate) bottleneck: usize,
    pub(crate) rechannel: ConvW,
    pub(crate) layers: Vec<LayerW>,
    pub(crate) head_rechannel: ConvW,
}

impl ArrayW {
    /// Channels each layer adds to the head path.
    pub(crate) fn head_term_channels(&self) -> usize {
        self.layers[0].head1x1.as_ref().map_or(self.bottleneck, |h| h.out)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WaveNetW {
    pub(crate) arrays: Vec<ArrayW>,
    /// Output head blocks: activation, then convolution.
    pub(crate) head: Vec<(Activation, ConvW)>,
    pub(crate) head_scale: f32,
}

impl WaveNetW {
    pub(crate) fn receptive_field(&self) -> usize {
        let mut rf = 1;
        for a in &self.arrays {
            rf += a.layers.iter().map(|l| (l.conv.kernel - 1) * l.conv.dilation).sum::<usize>();
            rf += a.head_rechannel.kernel - 1;
        }
        rf + self.head.iter().map(|(_, c)| c.kernel - 1).sum::<usize>()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LstmLayerW {
    pub(crate) input: usize,
    pub(crate) hidden: usize,
    /// `4H × (input + H)`, row-major, gates in PyTorch order i, f, g, o.
    pub(crate) weight: Vec<f32>,
    pub(crate) bias: Vec<f32>,
    pub(crate) h0: Vec<f32>,
    pub(crate) c0: Vec<f32>,
}

#[derive(Clone, Debug)]
pub(crate) struct LstmW {
    pub(crate) layers: Vec<LstmLayerW>,
    pub(crate) head_weight: Vec<f32>,
    pub(crate) head_bias: f32,
}

#[derive(Clone, Debug)]
pub(crate) enum Arch {
    WaveNet(WaveNetW),
    Lstm(LstmW),
    Linear(ConvW),
}

/// What was loaded from a `.nam` file.
#[derive(Clone, Debug)]
pub(crate) struct Parsed {
    pub(crate) net: Arch,
    pub(crate) sample_rate: u32,
    /// For containers: (chosen index, its max_value, all max_values).
    pub(crate) submodel: Option<(usize, f64, Vec<f64>)>,
    pub(crate) version: Option<String>,
}

/// A cursor over the flat weight list.
struct Weights<'a> {
    data: Vec<f32>,
    i: usize,
    at: &'a str,
}

impl Weights<'_> {
    fn take(&mut self, n: usize) -> Result<&[f32]> {
        if self.i + n > self.data.len() {
            return Err(Error::model(
                format!("{}.weights", self.at),
                format!("has {} values, fewer than the configuration needs", self.data.len()),
            ));
        }
        let s = &self.data[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }

    fn finish(&self) -> Result<()> {
        if self.i != self.data.len() {
            return Err(Error::model(
                format!("{}.weights", self.at),
                format!("has {} values, but the configuration uses {}", self.data.len(), self.i),
            ));
        }
        Ok(())
    }
}

fn read_weights<'a>(o: &mut Obj, at: &'a str) -> Result<Weights<'a>> {
    let arr = o.array("weights")?;
    let mut data = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        match v.as_f64() {
            Some(x) if x.is_finite() => data.push(x as f32),
            _ => return Err(o.field_error(&format!("weights[{i}]"), format!("expected a finite number, got {}", describe(v)))),
        }
    }
    Ok(Weights { data, i: 0, at })
}

/// Parse a whole `.nam` document. `at` names it in errors.
pub(crate) fn parse(v: &Value, at: &str, choice: SubmodelChoice) -> Result<Parsed> {
    let mut o = Obj::new(v, at, Domain::Model)?;
    let arch = o.string("architecture")?;
    if arch == "SlimmableContainer" {
        let mut cfg = o.child("config")?;
        let subs = cfg.array("submodels")?;
        cfg.finish("unsupported .nam feature")?;
        if subs.is_empty() {
            return Err(o.field_error("config.submodels", "is empty"));
        }
        let mut values = vec![];
        for (i, s) in subs.iter().enumerate() {
            let mut so = Obj::new(s, format!("{at}.config.submodels[{i}]"), Domain::Model)?;
            values.push(so.num("max_value")?);
        }
        let index = match choice {
            // max() in Python keeps the first of equal maxima
            SubmodelChoice::Largest => values.iter().enumerate().fold(0, |b, (i, &v)| if v > values[b] { i } else { b }),
            SubmodelChoice::Index(i) if i < values.len() => i,
            SubmodelChoice::Index(i) => {
                return Err(o.field_error("config.submodels", format!("has {} submodels; index {i} does not exist", values.len())));
            }
            SubmodelChoice::Size(x) => {
                let mut best: Option<usize> = None;
                for (i, &v) in values.iter().enumerate() {
                    if v >= x && best.is_none_or(|b| v < values[b]) {
                        best = Some(i);
                    }
                }
                best.unwrap_or_else(|| values.iter().enumerate().fold(0, |b, (i, &v)| if v > values[b] { i } else { b }))
            }
        };
        let sub_at = format!("{at}.config.submodels[{index}].model");
        let mut so = Obj::new(&subs[index], format!("{at}.config.submodels[{index}]"), Domain::Model)?;
        let model = so.required("model")?;
        so.ignore(&["max_value"]);
        so.finish("unsupported .nam feature")?;
        // the container's own metadata, weights and rate are not used (rig.py reads the submodel's)
        let mut p = parse_model(model, &sub_at)?;
        p.submodel = Some((index, values[index], values));
        return Ok(p);
    }
    parse_model(v, at)
}

fn parse_model(v: &Value, at: &str) -> Result<Parsed> {
    let mut o = Obj::new(v, at, Domain::Model)?;
    let arch = o.string("architecture")?;
    let version = o.opt_string("version")?.map(str::to_owned);
    let sample_rate = match o.opt_num("sample_rate")? {
        // rig.py: int(d.get("sample_rate") or 48000)
        None => 48000,
        Some(0.0) => 48000,
        Some(r) if (1.0..=1.0e6).contains(&r) => r as u32,
        Some(r) => return Err(o.field_error("sample_rate", format!("{r} is not a usable sample rate"))),
    };
    let mut weights = read_weights(&mut o, at)?;
    let mut cfg = o.child("config")?;
    let net = match arch {
        "WaveNet" => Arch::WaveNet(parse_wavenet(&mut cfg, &mut weights)?),
        "LSTM" => Arch::Lstm(parse_lstm(&mut cfg, &mut weights)?),
        "Linear" => Arch::Linear(parse_linear(&mut cfg, &mut weights)?),
        other => {
            return Err(o.field_error(
                "architecture",
                format!("unsupported architecture {other:?} (supported: WaveNet, LSTM, Linear, SlimmableContainer)"),
            ));
        }
    };
    cfg.finish("unsupported .nam feature")?;
    weights.finish()?;
    // metadata and anything else at the top level do not affect processing
    Ok(Parsed { net, sample_rate, submodel: None, version })
}

fn positive(o: &mut Obj, key: &str) -> Result<usize> {
    let v = o.usize(key)?;
    if v == 0 {
        return Err(o.field_error(key, "must be at least 1"));
    }
    Ok(v)
}

fn parse_linear(cfg: &mut Obj, w: &mut Weights) -> Result<ConvW> {
    let rf = positive(cfg, "receptive_field")?;
    let bias = cfg.bool_or("bias", false)?;
    let mut c = ConvW::shape(1, 1, rf, 1, 1, bias);
    c.read(w)?;
    Ok(c)
}

fn parse_lstm(cfg: &mut Obj, w: &mut Weights) -> Result<LstmW> {
    let input = cfg.usize_or("input_size", 1)?;
    if input != 1 {
        return Err(cfg.field_error("input_size", format!("only mono models are supported (input_size 1), got {input}")));
    }
    let hidden = positive(cfg, "hidden_size")?;
    let num_layers = cfg.usize_or("num_layers", 1)?;
    if num_layers == 0 {
        return Err(cfg.field_error("num_layers", "must be at least 1"));
    }
    let mut layers = vec![];
    for l in 0..num_layers {
        let inp = if l == 0 { input } else { hidden };
        let weight = w.take(4 * hidden * (inp + hidden))?.to_vec();
        let bias = w.take(4 * hidden)?.to_vec();
        let h0 = w.take(hidden)?.to_vec();
        let c0 = w.take(hidden)?.to_vec();
        layers.push(LstmLayerW { input: inp, hidden, weight, bias, h0, c0 });
    }
    let head_weight = w.take(hidden)?.to_vec();
    let head_bias = w.take(1)?[0];
    Ok(LstmW { layers, head_weight, head_bias })
}

fn parse_wavenet(cfg: &mut Obj, w: &mut Weights) -> Result<WaveNetW> {
    let layer_vals = cfg.array("layers")?;
    if layer_vals.is_empty() {
        return Err(cfg.field_error("layers", "needs at least one layer array"));
    }
    if cfg.get("condition_dsp").is_some() {
        return Err(cfg.field_error("condition_dsp", "unsupported .nam feature (a conditioning network)"));
    }
    let head_scale_cfg = cfg.num("head_scale")?;
    let mut arrays = vec![];
    for (i, lv) in layer_vals.iter().enumerate() {
        let mut lo = Obj::new(lv, cfg.at_key(&format!("layers[{i}]")), Domain::Model)?;
        let a = if lo.has("head_size") || lo.has("gated") { parse_array_a1(&mut lo)? } else { parse_array_a2(&mut lo)? };
        lo.finish("unsupported .nam feature")?;
        arrays.push(a);
    }
    // consistency between arrays, and with the mono input / output of the wrapper
    for (i, a) in arrays.iter().enumerate() {
        let at = cfg.at_key(&format!("layers[{i}]"));
        let want_input = if i == 0 { 1 } else { arrays[i - 1].channels };
        if a.input_size != want_input {
            return Err(Error::model(
                format!("{at}.input_size"),
                format!("is {}, expected {want_input} ({})", a.input_size, if i == 0 { "mono input" } else { "previous array's channels" }),
            ));
        }
        if a.condition_size != 1 {
            return Err(Error::model(format!("{at}.condition_size"), format!("is {}, expected 1 (the mono input)", a.condition_size)));
        }
        if i > 0 && arrays[i - 1].head_rechannel.out != a.head_term_channels() {
            return Err(Error::model(
                at,
                format!(
                    "head channels do not chain: the previous array outputs {}, this one adds {}",
                    arrays[i - 1].head_rechannel.out,
                    a.head_term_channels()
                ),
            ));
        }
    }
    let mut head = vec![];
    if let Some(mut ho) = cfg.opt_child("head")? {
        let in_ch = arrays.last().map_or(1, |a| a.head_rechannel.out);
        head = parse_head(&mut ho, in_ch)?;
        ho.finish("unsupported .nam feature")?;
    }
    let out_ch = head.last().map_or_else(|| arrays.last().map_or(1, |a| a.head_rechannel.out), |(_, c)| c.out);
    if out_ch != 1 {
        return Err(cfg.error(format!("the network has {out_ch} output channels; only mono output is supported")));
    }
    // weights: arrays, head, then head_scale
    for a in &mut arrays {
        a.rechannel.read(w)?;
        for l in &mut a.layers {
            l.conv.read(w)?;
            l.mixer.read(w)?;
            if let Some(c) = l.layer1x1.as_mut() {
                c.read(w)?;
            }
            if let Some(c) = l.head1x1.as_mut() {
                c.read(w)?;
            }
            for f in l.films.iter_mut().flatten() {
                f.conv.read(w)?;
            }
        }
        a.head_rechannel.read(w)?;
    }
    for (_, c) in &mut head {
        c.read(w)?;
    }
    // nam keeps the config value unless the stored tail differs by more than 1e-5
    let tail = w.take(1)?[0] as f64;
    let head_scale = if (tail - head_scale_cfg).abs() > 1e-5 { tail } else { head_scale_cfg };
    Ok(WaveNetW { arrays, head, head_scale: head_scale as f32 })
}

/// Per-layer list or one value for all layers.
fn per_layer<'a>(o: &mut Obj<'a>, key: &str, n: usize) -> Result<Vec<&'a Value>> {
    let v = o.required(key)?;
    match v {
        Value::Array(a) => {
            if a.len() != n {
                return Err(o.field_error(key, format!("has {} entries for {n} layers", a.len())));
            }
            Ok(a.iter().collect())
        }
        other => Ok(vec![other; n]),
    }
}

fn dilations(o: &mut Obj) -> Result<Vec<usize>> {
    let arr = o.array("dilations")?;
    if arr.is_empty() {
        return Err(o.field_error("dilations", "needs at least one layer"));
    }
    arr.iter()
        .map(|v| match crate::json::as_int(v) {
            Some(d) if (1..=1 << 20).contains(&d) => Ok(d as usize),
            _ => Err(o.field_error("dilations", format!("expected positive integers, got {}", describe(v)))),
        })
        .collect()
}

fn kernel_sizes(o: &mut Obj, n: usize) -> Result<Vec<usize>> {
    let key = if o.has("kernel_sizes") { "kernel_sizes" } else { "kernel_size" };
    per_layer(o, key, n)?
        .into_iter()
        .map(|v| match crate::json::as_int(v) {
            Some(k) if (1..=4096).contains(&k) => Ok(k as usize),
            _ => Err(o.field_error(key, format!("expected positive integers, got {}", describe(v)))),
        })
        .collect()
}

/// A1 layer array: `{input_size, condition_size, head_size, channels, kernel_size, dilations, activation, gated,
/// head_bias}`.
fn parse_array_a1(o: &mut Obj) -> Result<ArrayW> {
    let input_size = positive(o, "input_size")?;
    let condition_size = positive(o, "condition_size")?;
    let head_size = positive(o, "head_size")?;
    let channels = positive(o, "channels")?;
    let dil = dilations(o)?;
    let ks = kernel_sizes(o, dil.len())?;
    let act_v = o.required("activation")?;
    let act = Activation::parse(act_v, o, "activation")?;
    let gated = o.bool_or("gated", false)?;
    let head_bias = o.bool_or("head_bias", false)?;
    let mid = if gated { 2 * channels } else { channels };
    let layers = ks
        .iter()
        .zip(&dil)
        .map(|(&k, &d)| LayerW {
            conv: ConvW::shape(mid, channels, k, d, 1, true),
            mixer: ConvW::shape(mid, condition_size, 1, 1, 1, false),
            act: if gated { LayerAct::Gated(act.clone(), Activation::Sigmoid) } else { LayerAct::Single(act.clone()) },
            layer1x1: Some(ConvW::shape(channels, channels, 1, 1, 1, true)),
            head1x1: None,
            films: Default::default(),
        })
        .collect();
    act.check_channels(channels).map_err(|m| o.field_error("activation", m))?;
    Ok(ArrayW {
        input_size,
        condition_size,
        channels,
        bottleneck: channels,
        rechannel: ConvW::shape(channels, input_size, 1, 1, 1, false),
        layers,
        head_rechannel: ConvW::shape(head_size, channels, 1, 1, 1, head_bias),
    })
}

fn groups_ok(o: &Obj, key: &str, groups: usize, inp: usize, out: usize) -> Result<()> {
    if groups == 0 || !inp.is_multiple_of(groups) || !out.is_multiple_of(groups) {
        return Err(o.field_error(key, format!("{groups} groups do not divide {inp} input and {out} output channels")));
    }
    Ok(())
}

/// A2 layer array.
fn parse_array_a2(o: &mut Obj) -> Result<ArrayW> {
    let input_size = positive(o, "input_size")?;
    let condition_size = positive(o, "condition_size")?;
    let channels = positive(o, "channels")?;
    let bottleneck = if o.has("bottleneck") { positive(o, "bottleneck")? } else { channels };
    let dil = dilations(o)?;
    let n = dil.len();
    let ks = kernel_sizes(o, n)?;

    let mut head = o.child("head")?;
    let head_out = positive(&mut head, "out_channels")?;
    let head_kernel = positive(&mut head, "kernel_size")?;
    let head_bias = head.bool_or("bias", false)?;
    head.finish("unsupported .nam feature")?;

    let (l1_active, l1_groups) = match o.opt_child("layer1x1")? {
        None => (true, 1),
        Some(mut c) => {
            let a = c.bool_or("active", true)?;
            let g = c.usize_or("groups", 1)?;
            c.finish("unsupported .nam feature")?;
            (a, g)
        }
    };
    let (h1_active, h1_out, h1_groups) = match o.opt_child("head1x1")? {
        None => (false, 1, 1),
        Some(mut c) => {
            let a = c.bool_or("active", false)?;
            let out = c.usize_or("out_channels", 1)?;
            let g = c.usize_or("groups", 1)?;
            c.finish("unsupported .nam feature")?;
            (a, out, g)
        }
    };
    let groups_input = o.usize_or("groups_input", 1)?;
    let groups_mixin = o.usize_or("groups_input_mixin", 1)?;
    let mut films: [Option<(bool, usize)>; 8] = Default::default();
    for (slot, key) in films.iter_mut().zip(FILM_KEYS) {
        if let Some(mut f) = o.opt_child(key)? {
            let active = f.bool_or("active", false)?;
            let shift = f.bool_or("shift", true)?;
            let groups = f.usize_or("groups", 1)?;
            f.finish("unsupported .nam feature")?;
            if active {
                *slot = Some((shift, groups));
            }
        }
    }
    // slimmable submodels are exported at their final width; the slimming settings do not change inference
    let _ = o.get("slimmable");
    if o.get("packing").is_some() {
        return Err(o.field_error("packing", "unsupported .nam feature"));
    }

    // activations, with optional pairing
    let acts = per_layer(o, "activation", n)?;
    let gating: Vec<Option<&Value>> = match o.get("gating_mode") {
        None => vec![None; n],
        Some(Value::Array(a)) if a.len() == n => a.iter().map(Some).collect(),
        Some(v) => return Err(o.field_error("gating_mode", format!("expected a list of {n} modes, got {}", describe(v)))),
    };
    let secondary: Vec<Option<&Value>> = match o.get("secondary_activation") {
        None => vec![None; n],
        Some(Value::Array(a)) if a.len() == n => a.iter().map(|v| (!v.is_null()).then_some(v)).collect(),
        Some(v) => {
            return Err(o.field_error("secondary_activation", format!("expected a list of {n} entries, got {}", describe(v))));
        }
    };
    let mut layer_acts = vec![];
    for i in 0..n {
        let prim = Activation::parse(acts[i], o, &format!("activation[{i}]"))?;
        let mode = match gating[i] {
            None => "none",
            Some(Value::String(s)) => s.as_str(),
            Some(v) => return Err(o.field_error(&format!("gating_mode[{i}]"), format!("expected a string, got {}", describe(v)))),
        };
        let sec = |o: &Obj| -> Result<Activation> {
            let v = secondary[i]
                .ok_or_else(|| o.field_error(&format!("secondary_activation[{i}]"), format!("is required for gating mode {mode:?}")))?;
            Activation::parse(v, o, &format!("secondary_activation[{i}]"))
        };
        layer_acts.push(match mode {
            "none" => LayerAct::Single(prim),
            "gated" => LayerAct::Gated(prim, sec(o)?),
            "blended" => LayerAct::Blended(prim, sec(o)?),
            other => {
                return Err(o.field_error(&format!("gating_mode[{i}]"), format!("unknown mode {other:?} (none, gated, blended)")));
            }
        });
    }

    if !l1_active && bottleneck != channels {
        return Err(o.error(format!(
            "without layer1x1 the bottleneck ({bottleneck}) must equal the channels ({channels}) for the residual connection"
        )));
    }
    if films[6].is_some() && !l1_active {
        return Err(o.field_error("layer1x1_post_film", "cannot be active when layer1x1 is not"));
    }
    if films[7].is_some() && !h1_active {
        return Err(o.field_error("head1x1_post_film", "cannot be active when head1x1 is not"));
    }
    let head_in = if h1_active { h1_out } else { bottleneck };
    let mut layers = vec![];
    for i in 0..n {
        let act = layer_acts[i].clone();
        let mid = if act.paired() { 2 * bottleneck } else { bottleneck };
        match &act {
            LayerAct::Single(a) => a.check_channels(bottleneck),
            LayerAct::Gated(a, b) | LayerAct::Blended(a, b) => a.check_channels(bottleneck).and(b.check_channels(bottleneck)),
        }
        .map_err(|m| o.field_error(&format!("activation[{i}]"), m))?;
        groups_ok(o, "groups_input", groups_input, channels, mid)?;
        groups_ok(o, "groups_input_mixin", groups_mixin, condition_size, mid)?;
        if l1_active {
            groups_ok(o, "layer1x1", l1_groups, bottleneck, channels)?;
        }
        if h1_active {
            groups_ok(o, "head1x1", h1_groups, bottleneck, h1_out)?;
        }
        // FiLM input dimensions per position (nam: conv_pre on channels, conv_post / mixin_post / act_pre on the
        // convolution output, mixin_pre on the condition, act_post on the bottleneck, layer1x1_post on channels,
        // head1x1_post on head1x1 outputs)
        let dims = [channels, mid, condition_size, mid, mid, bottleneck, channels, h1_out];
        let mut fw: [Option<FilmW>; 8] = Default::default();
        for (j, f) in films.iter().enumerate() {
            if let Some((shift, groups)) = *f {
                let out = if shift { 2 * dims[j] } else { dims[j] };
                groups_ok(o, FILM_KEYS[j], groups, condition_size, out)?;
                fw[j] = Some(FilmW { conv: ConvW::shape(out, condition_size, 1, 1, groups, true), shift, dim: dims[j] });
            }
        }
        layers.push(LayerW {
            conv: ConvW::shape(mid, channels, ks[i], dil[i], groups_input, true),
            mixer: ConvW::shape(mid, condition_size, 1, 1, groups_mixin, false),
            act,
            layer1x1: l1_active.then(|| ConvW::shape(channels, bottleneck, 1, 1, l1_groups, true)),
            head1x1: h1_active.then(|| ConvW::shape(h1_out, bottleneck, 1, 1, h1_groups, true)),
            films: fw,
        });
    }
    Ok(ArrayW {
        input_size,
        condition_size,
        channels,
        bottleneck,
        rechannel: ConvW::shape(channels, input_size, 1, 1, 1, false),
        layers,
        head_rechannel: ConvW::shape(head_out, head_in, head_kernel, 1, 1, head_bias),
    })
}

/// The output head: A2 `{channels, activation, out_channels, kernel_sizes}` or A1 `{channels, activation,
/// num_layers, out_channels}` (1x1 convolutions).
fn parse_head(o: &mut Obj, in_channels: usize) -> Result<Vec<(Activation, ConvW)>> {
    let channels = positive(o, "channels")?;
    let out = positive(o, "out_channels")?;
    let act_v = o.required("activation")?;
    let act = Activation::parse(act_v, o, "activation")?;
    let kernels: Vec<usize> = if o.has("kernel_sizes") {
        o.array("kernel_sizes")?
            .iter()
            .map(|v| match crate::json::as_int(v) {
                Some(k) if (1..=4096).contains(&k) => Ok(k as usize),
                _ => Err(o.field_error("kernel_sizes", format!("expected positive integers, got {}", describe(v)))),
            })
            .collect::<Result<_>>()?
    } else {
        vec![1; positive(o, "num_layers")?]
    };
    if kernels.is_empty() {
        return Err(o.field_error("kernel_sizes", "must not be empty"));
    }
    if let Some(legacy) = o.opt_int("in_channels")?
        && legacy as usize != in_channels
    {
        return Err(o.field_error("in_channels", format!("is {legacy}, but the last layer array outputs {in_channels}")));
    }
    let mut blocks = vec![];
    let mut cin = in_channels;
    for (i, &k) in kernels.iter().enumerate() {
        let cout = if i + 1 == kernels.len() { out } else { channels };
        act.check_channels(cin).map_err(|m| o.field_error("activation", m))?;
        blocks.push((act.clone(), ConvW::shape(cout, cin, k, 1, 1, true)));
        cin = channels;
    }
    Ok(blocks)
}
