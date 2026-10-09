//! Activation functions of NAM models, evaluated elementwise in `f32` with code the compiler vectorises.
//!
//! `exp`, `tanh` and `sigmoid` use range reduction and short polynomials accurate to a few ulp, close to
//! PyTorch's own vectorised versions (the reference NAM implementation), and much faster than the libm calls.

use crate::json::{Obj, as_f64, describe};
use serde_json::Value;

/// One activation, with its parameters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Activation {
    Identity,
    Tanh,
    Hardtanh { min: f32, max: f32 },
    ReLU,
    LeakyReLU { slope: f32 },
    /// One slope, or one per channel.
    PReLU { slopes: Vec<f32> },
    Sigmoid,
    SiLU,
    Hardswish,
    Softsign,
    Softsigmoid,
    LeakyHardtanh { min: f32, max: f32, min_slope: f32, max_slope: f32 },
}

const NAMES: &str = "Tanh, Hardtanh, ReLU, LeakyReLU, PReLU, Sigmoid, SiLU, Hardswish, Softsign, Softsigmoid, LeakyHardtanh, Identity";

impl Activation {
    /// Parse an activation in either of the forms `.nam` files use: a bare name (`"Tanh"`), the export form
    /// (`{"type": "LeakyReLU", "negative_slope": 0.01}`) or the init form (`{"name": "LeakyReLU", ...}`).
    pub(crate) fn parse(v: &Value, o: &Obj, key: &str) -> crate::Result<Activation> {
        let at = o.at_key(key);
        let err = |m: String| o.field_error(key, m);
        let (name, params) = match v {
            Value::String(s) => (s.as_str(), None),
            Value::Object(m) => {
                let name = m
                    .get("type")
                    .or_else(|| m.get("name"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| err("activation object has no \"type\" or \"name\"".into()))?;
                (name, Some(v))
            }
            other => return Err(err(format!("expected an activation name or object, got {}", describe(other)))),
        };
        let mut p = match params {
            Some(v) => Some(Obj::new(v, at.clone(), crate::json::Domain::Model)?),
            None => None,
        };
        if let Some(p) = p.as_mut() {
            p.ignore(&["type", "name"]);
        }
        fn num(p: &mut Option<Obj>, k: &str, default: f64) -> crate::Result<f32> {
            match p.as_mut() {
                Some(p) => Ok(p.num_or(k, default)? as f32),
                None => Ok(default as f32),
            }
        }
        let act = match name {
            "Identity" => Activation::Identity,
            "Tanh" => Activation::Tanh,
            "Hardtanh" => Activation::Hardtanh { min: num(&mut p, "min_val", -1.0)?, max: num(&mut p, "max_val", 1.0)? },
            "ReLU" => Activation::ReLU,
            "LeakyReLU" => Activation::LeakyReLU { slope: num(&mut p, "negative_slope", 0.01)? },
            "PReLU" => {
                let slopes = match p.as_mut() {
                    Some(p) if p.has("negative_slopes") => p
                        .array("negative_slopes")?
                        .iter()
                        .map(|x| as_f64(x).map(|x| x as f32))
                        .collect::<Option<Vec<f32>>>()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| err("PReLU negative_slopes must be a non-empty list of numbers".into()))?,
                    Some(p) => vec![p.num_or("negative_slope", 0.25)? as f32],
                    None => vec![0.25],
                };
                Activation::PReLU { slopes }
            }
            "Sigmoid" => Activation::Sigmoid,
            "SiLU" => Activation::SiLU,
            "Hardswish" => Activation::Hardswish,
            "Softsign" => Activation::Softsign,
            "Softsigmoid" => Activation::Softsigmoid,
            "LeakyHardtanh" => Activation::LeakyHardtanh {
                min: num(&mut p, "min_val", -1.0)?,
                max: num(&mut p, "max_val", 1.0)?,
                min_slope: num(&mut p, "min_slope", 0.01)?,
                max_slope: num(&mut p, "max_slope", 0.01)?,
            },
            other => return Err(err(format!("unsupported activation {other:?} (supported: {NAMES})"))),
        };
        if let Some(p) = p {
            p.finish("unsupported activation parameter")?;
        }
        Ok(act)
    }

    /// Check per-channel parameters against the number of channels the activation sees.
    pub(crate) fn check_channels(&self, channels: usize) -> Result<(), String> {
        match self {
            Activation::PReLU { slopes } if slopes.len() != 1 && slopes.len() != channels => {
                Err(format!("PReLU has {} slopes for {channels} channels", slopes.len()))
            }
            _ => Ok(()),
        }
    }

    /// Apply in place to one channel (`row` selects per-channel parameters).
    #[inline(always)]
    pub(crate) fn apply(&self, x: &mut [f32], row: usize) {
        match *self {
            Activation::Identity => {}
            Activation::Tanh => x.iter_mut().for_each(|v| *v = tanh(*v)),
            Activation::Hardtanh { min, max } => x.iter_mut().for_each(|v| *v = hardtanh(*v, min, max)),
            Activation::ReLU => x.iter_mut().for_each(|v| *v = if *v > 0.0 { *v } else { 0.0 }),
            Activation::LeakyReLU { slope } => x.iter_mut().for_each(|v| *v = leaky(*v, slope)),
            Activation::PReLU { ref slopes } => {
                let s = if slopes.len() == 1 { slopes[0] } else { slopes[row] };
                x.iter_mut().for_each(|v| *v = leaky(*v, s))
            }
            Activation::Sigmoid => x.iter_mut().for_each(|v| *v = sigmoid(*v)),
            Activation::SiLU => x.iter_mut().for_each(|v| *v *= sigmoid(*v)),
            Activation::Hardswish => x.iter_mut().for_each(|v| *v = *v * (*v + 3.0).clamp(0.0, 6.0) / 6.0),
            Activation::Softsign => x.iter_mut().for_each(|v| *v /= 1.0 + v.abs()),
            Activation::Softsigmoid => x.iter_mut().for_each(|v| *v = 0.5 * (1.0 + *v / (1.0 + v.abs()))),
            Activation::LeakyHardtanh { min, max, min_slope, max_slope } => x.iter_mut().for_each(|v| {
                let x = *v;
                *v = if x < min {
                    (x - min) * min_slope + min
                } else if x > max {
                    (x - max) * max_slope + max
                } else {
                    x
                }
            }),
        }
    }
}

#[inline(always)]
fn leaky(x: f32, slope: f32) -> f32 {
    if x > 0.0 { x } else { x * slope }
}

#[inline(always)]
fn hardtanh(x: f32, min: f32, max: f32) -> f32 {
    if x < min {
        min
    } else if x > max {
        max
    } else {
        x
    }
}

/// Round to the nearest integer (ties to even) for |x| < 2^22, vectorisable without SSE4.1.
#[inline(always)]
fn round_magic(x: f32) -> f32 {
    const MAGIC: f32 = 12_582_912.0; // 1.5 * 2^23
    (x + MAGIC) - MAGIC
}

/// 2^k for integral k in -126..=127.
#[inline(always)]
fn pow2i(k: f32) -> f32 {
    f32::from_bits(((k as i32 + 127) as u32) << 23)
}

const LN2_HI: f32 = 0.693_145_75;
const LN2_LO: f32 = 1.428_606_8e-6;

/// `exp(x)` for x in [-87, 0].
#[inline(always)]
fn exp_neg(x: f32) -> f32 {
    let x = x.max(-87.0);
    let k = round_magic(x * std::f32::consts::LOG2_E);
    let r = (x - k * LN2_HI) - k * LN2_LO;
    // Taylor series of e^r on |r| <= ln2/2, truncation error < 1e-8 relative
    let p = 1.0 / 5040.0;
    let p = p * r + 1.0 / 720.0;
    let p = p * r + 1.0 / 120.0;
    let p = p * r + 1.0 / 24.0;
    let p = p * r + 1.0 / 6.0;
    let p = p * r + 0.5;
    let p = p * r + 1.0;
    let p = p * r + 1.0;
    p * pow2i(k)
}

/// `exp(x) - 1` for x in [-18, 0], accurate near 0.
#[inline(always)]
fn expm1_neg(x: f32) -> f32 {
    let k = round_magic(x * std::f32::consts::LOG2_E);
    let r = (x - k * LN2_HI) - k * LN2_LO;
    // expm1(r) = r + r^2/2 + ... + r^8/8!
    let p = 1.0 / 40320.0;
    let p = p * r + 1.0 / 5040.0;
    let p = p * r + 1.0 / 720.0;
    let p = p * r + 1.0 / 120.0;
    let p = p * r + 1.0 / 24.0;
    let p = p * r + 1.0 / 6.0;
    let p = p * r + 0.5;
    let em1 = (p * r) * r + r;
    let s = pow2i(k);
    s * em1 + (s - 1.0)
}

/// tanh(x) = sign(x) · (−u / (2 + u)) with u = expm1(−2|x|).
#[inline(always)]
pub(crate) fn tanh(x: f32) -> f32 {
    let a = x.abs().min(9.0);
    let u = expm1_neg(-2.0 * a);
    let t = -u / (2.0 + u);
    if x < 0.0 { -t } else { t }
}

/// 1 / (1 + exp(−x)), without overflow for large |x|.
#[inline(always)]
pub(crate) fn sigmoid(x: f32) -> f32 {
    let t = exp_neg(-x.abs());
    let num = if x >= 0.0 { 1.0 } else { t };
    num / (1.0 + t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ulps(a: f32, b: f64) -> f64 {
        let b32 = b as f32;
        let ulp = (f32::from_bits(b32.abs().to_bits() + 1) - b32.abs()) as f64;
        (a as f64 - b).abs() / ulp.max(f32::MIN_POSITIVE as f64)
    }

    #[test]
    fn tanh_sigmoid_exp_are_accurate() {
        let mut worst = (0.0f64, 0.0f64, 0.0f64);
        let mut x = -30.0f32;
        while x < 30.0 {
            let xd = x as f64;
            worst.0 = worst.0.max(ulps(tanh(x), xd.tanh()));
            worst.1 = worst.1.max(ulps(sigmoid(x), 1.0 / (1.0 + (-xd).exp())));
            if x <= 0.0 {
                worst.2 = worst.2.max(ulps(exp_neg(x), xd.exp()));
            }
            x += 0.000_731;
        }
        // tiny arguments
        for &x in &[1e-30f32, 1e-12, 1e-7, -3e-5, 2e-3] {
            worst.0 = worst.0.max(ulps(tanh(x), (x as f64).tanh()));
        }
        assert!(worst.0 <= 4.0 && worst.1 <= 4.0 && worst.2 <= 3.0, "ulps (tanh, sigmoid, exp): {worst:?}");
        assert_eq!(tanh(100.0), 1.0);
        assert_eq!(tanh(-100.0), -1.0);
        assert!(sigmoid(-200.0).max(0.0) < 1e-37);
        assert_eq!(sigmoid(200.0), 1.0);
    }

    #[test]
    fn piecewise_activations() {
        let mut x = [-3.0f32, -0.5, 0.0, 0.5, 3.0];
        Activation::LeakyHardtanh { min: -1.0, max: 1.0, min_slope: 0.1, max_slope: 0.2 }.apply(&mut x, 0);
        assert_eq!(x, [-1.2, -0.5, 0.0, 0.5, 1.4]);
        let mut x = [-4.0f32, -1.0, 0.0, 1.0, 4.0];
        Activation::Hardswish.apply(&mut x, 0);
        assert_eq!(x, [0.0, -2.0 / 6.0, 0.0, 4.0 / 6.0, 4.0]);
        let mut x = [-2.0f32, 2.0];
        Activation::PReLU { slopes: vec![0.1, 0.5] }.apply(&mut x, 1);
        assert_eq!(x, [-1.0, 2.0]);
    }
}
