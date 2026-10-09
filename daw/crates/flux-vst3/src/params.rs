//! Finding parameters by name and turning human values ("Sine", -3.0 dB, 900 Hz) into the
//! normalized values VST3 uses.
//!
//! Names match, in order: `#<id>`, the exact title, the title case-insensitively, the snake-case title
//! (`a_osc_1_type`, how pedalboard named them, so older projects keep working), the short title.
//!
//! Values:
//! * a string is parsed by the plugin itself (`getParamValueByString`), so units work ("250 ms",
//!   "-6 dB"); for list parameters it is matched against every step's display text.
//! * a number is a value in the parameter's display units (dB, Hz, %...). It is passed to the plugin's
//!   parser and the result checked by reading the display back; if the plugin's parser disagrees the
//!   display is searched (bisection over the normalized range) for the value. A plugin whose display only
//!   shows its current value (Valhalla Supermassive) is searched by moving the controller's value and reading
//!   the display back, then putting the value back.
//! * `{"normalized": x}` is used as is.

use crate::plugin::ParamInfo;
use crate::util::snake;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;

/// What the resolver needs from a plugin (implemented by [`crate::Plugin`]; mocked in tests).
pub trait ParamAccess {
    fn param_text(&self, id: u32, normalized: f64) -> Option<String>;
    fn param_from_text(&self, id: u32, text: &str) -> Option<f64>;
    /// The parameter's current normalized value.
    fn current(&self, id: u32) -> Option<f64> {
        let _ = id;
        None
    }
    /// Move the controller's value (not the processor's) so a display that only shows the current value can be
    /// read at `normalized`; false if the plugin can't be probed this way.
    fn probe(&self, id: u32, normalized: f64) -> bool {
        let _ = (id, normalized);
        false
    }
}

impl ParamAccess for crate::Plugin {
    fn param_text(&self, id: u32, normalized: f64) -> Option<String> {
        crate::Plugin::param_text(self, id, normalized)
    }
    fn param_from_text(&self, id: u32, text: &str) -> Option<f64> {
        crate::Plugin::param_from_text(self, id, text)
    }
    fn current(&self, id: u32) -> Option<f64> {
        crate::Plugin::param(self, id)
    }
    fn probe(&self, id: u32, normalized: f64) -> bool {
        self.set_controller_param(id, normalized)
    }
}

/// Find a parameter by `#id`, title, snake-case title or short title.
pub fn find<'a>(params: &'a [ParamInfo], name: &str) -> Result<&'a ParamInfo> {
    let name = name.trim();
    if let Some(id) = name.strip_prefix('#').and_then(|s| s.parse::<u32>().ok()) {
        return params.iter().find(|p| p.id == id).ok_or_else(|| anyhow!("no parameter with id {id}"));
    }
    let lower = name.to_lowercase();
    let sn = snake(name);
    params
        .iter()
        .find(|p| p.title == name)
        .or_else(|| params.iter().find(|p| p.title.to_lowercase() == lower))
        .or_else(|| params.iter().find(|p| snake(&p.title) == sn))
        .or_else(|| params.iter().find(|p| !p.short_title.is_empty() && (p.short_title.to_lowercase() == lower || snake(&p.short_title) == sn)))
        .ok_or_else(|| {
            let close = suggestions(params, name);
            if close.is_empty() {
                anyhow!("no parameter named '{name}'")
            } else {
                anyhow!("no parameter named '{name}'; did you mean {}?", close.join(", "))
            }
        })
}

/// Up to five titles that contain every word of `name`.
pub fn suggestions(params: &[ParamInfo], name: &str) -> Vec<String> {
    let words: Vec<String> = snake(name).split('_').filter(|w| !w.is_empty()).map(String::from).collect();
    if words.is_empty() {
        return vec![];
    }
    params
        .iter()
        .filter(|p| {
            let t = snake(&p.title);
            words.iter().all(|w| t.split('_').any(|x| x == w || x.starts_with(w.as_str())))
        })
        .take(5)
        .map(|p| format!("'{}'", p.title))
        .collect()
}

/// The first number in a display string, scaled by an SI prefix on its unit ("1.2 kHz" -> 1200).
pub fn display_number(text: &str) -> Option<f64> {
    let t = text.trim();
    let bytes = t.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        let next_digit = bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit() || *c == b'.');
        if b.is_ascii_digit() || ((b == b'-' || b == b'+' || b == b'.') && next_digit) {
            start = Some(i);
            break;
        }
    }
    let s = start?;
    let mut end = s + 1;
    let mut seen_e = false;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_digit() || c == b'.' {
            end += 1;
        } else if (c == b'e' || c == b'E') && !seen_e && bytes.get(end + 1).is_some_and(|d| d.is_ascii_digit() || *d == b'-' || *d == b'+') {
            seen_e = true;
            end += 2;
        } else {
            break;
        }
    }
    let mut v: f64 = t[s..end].parse().ok()?;
    let unit = t[end..].trim_start();
    if unit.starts_with("kHz") || unit.starts_with("k ") || unit == "k" {
        v *= 1000.0;
    } else if unit.starts_with("MHz") {
        v *= 1e6;
    }
    if t[..s].trim_end().ends_with('-') && v > 0.0 {
        v = -v;
    }
    Some(v)
}

/// Normalized value for a JSON value (see the module docs).
pub fn normalized(access: &dyn ParamAccess, p: &ParamInfo, value: &Value) -> Result<f64> {
    match value {
        Value::Object(m) => {
            let v = m
                .get("normalized")
                .and_then(Value::as_f64)
                .ok_or_else(|| anyhow!("parameter '{}': an object value must be {{\"normalized\": 0..1}}", p.title))?;
            if !(0.0..=1.0).contains(&v) {
                bail!("parameter '{}': normalized value {v} is outside 0..1", p.title);
            }
            Ok(v)
        }
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => from_text(access, p, s),
        Value::Number(n) => from_number(access, p, n.as_f64().ok_or_else(|| anyhow!("bad number"))?),
        _ => bail!("parameter '{}': value must be a number (display units), text, or {{\"normalized\": x}}", p.title),
    }
}

fn steps(p: &ParamInfo) -> Option<usize> {
    (p.step_count > 0 && p.step_count <= 4096).then_some(p.step_count as usize)
}

fn step_value(i: usize, n: usize) -> f64 {
    if n == 0 { 0.0 } else { i as f64 / n as f64 }
}

fn from_text(access: &dyn ParamAccess, p: &ParamInfo, text: &str) -> Result<f64> {
    let want = text.trim();
    // list parameters: match a step's display text first (the plugin's parser can be lax with these)
    if let Some(n) = steps(p) {
        let mut options = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let v = step_value(i, n);
            if let Some(t) = access.param_text(p.id, v) {
                if t.trim().eq_ignore_ascii_case(want) {
                    return Ok(v);
                }
                options.push(t);
            }
        }
        if let Some(v) = access.param_from_text(p.id, want) {
            return Ok(v);
        }
        let list = options.iter().take(24).map(|o| format!("'{}'", o.trim())).collect::<Vec<_>>().join(", ");
        bail!("parameter '{}' has no option '{want}'; options: {list}{}", p.title, if options.len() > 24 { ", ..." } else { "" });
    }
    if let Some(v) = access.param_from_text(p.id, want) {
        return Ok(v);
    }
    // continuous parameter whose display is text at some positions ("Off"): scan for it
    for i in 0..=128 {
        let v = i as f64 / 128.0;
        if access.param_text(p.id, v).is_some_and(|t| t.trim().eq_ignore_ascii_case(want)) {
            return Ok(v);
        }
    }
    if let Some(x) = display_number(want) {
        return from_number(access, p, x);
    }
    bail!("parameter '{}' doesn't accept '{want}'", p.title)
}

/// Display value `a` matches the wanted `b` (relative tolerance; displays round to a few digits).
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6_f64.max(b.abs() * 1e-4)
}

fn from_number(access: &dyn ParamAccess, p: &ParamInfo, x: f64) -> Result<f64> {
    if !x.is_finite() {
        bail!("parameter '{}': value {x} is not finite", p.title);
    }
    let shown = |v: f64| access.param_text(p.id, v).and_then(|t| display_number(&t));
    if let Some(n) = steps(p) {
        let mut best: Option<(f64, f64)> = None;
        for i in 0..=n {
            let v = step_value(i, n);
            if let Some(d) = shown(v) {
                let err = (d - x).abs();
                if best.is_none_or(|b| err < b.1) {
                    best = Some((v, err));
                }
            }
        }
        if let Some((v, err)) = best
            && err <= 1e-6_f64.max(x.abs() * 1e-3)
        {
            return Ok(v);
        }
    }
    // the plugin's own parser, checked by reading the display back
    for text in [format!("{x}"), format!("{x:.6}")] {
        if let Some(v) = access.param_from_text(p.id, &text)
            && shown(v).is_some_and(|d| close(d, x))
        {
            return Ok(v);
        }
    }
    let frozen = |samples: &[(f64, f64)]| samples.len() > 2 && samples.iter().all(|s| s.1 == samples[0].1);
    let samples = sample(&shown);
    if !frozen(&samples) {
        return search(access, p, &shown, samples, x);
    }
    // the same number whatever value it is asked to show: the plugin shows its current value (Valhalla
    // Supermassive does). Move the controller's value and read the display back instead, then put it back.
    if let Some(now) = access.current(p.id)
        && access.probe(p.id, now)
    {
        let at = |v: f64| {
            access.probe(p.id, v);
            access.param_text(p.id, v).and_then(|t| display_number(&t))
        };
        let samples = sample(&at);
        let found = if frozen(&samples) { None } else { Some(search(access, p, &at, samples, x)) };
        access.probe(p.id, now);
        if let Some(found) = found {
            return found;
        }
    }
    if close(samples[0].1, x)
        && let Some(now) = access.current(p.id)
    {
        return Ok(now);
    }
    // nothing to search: the plugin's own parser is all there is to go by
    for text in [format!("{x}"), format!("{x:.6}")] {
        if let Some(v) = access.param_from_text(p.id, &text) {
            return Ok(v);
        }
    }
    let shows = access.param_text(p.id, 0.0).unwrap_or_default();
    bail!(
        "parameter '{}' shows '{}' for every value (the plugin only displays its current one), so {x} can't be looked up; give {{\"normalized\": x}}",
        p.title,
        shows.trim()
    )
}

/// The display's number at evenly spaced normalized values.
fn sample(shown: &dyn Fn(f64) -> Option<f64>) -> Vec<(f64, f64)> {
    const N: usize = 256;
    (0..=N).filter_map(|i| {
        let v = i as f64 / N as f64;
        shown(v).map(|d| (v, d))
    }).collect()
}

/// Find `x` on the display: a bracketing pair of samples, then bisection.
fn search(access: &dyn ParamAccess, p: &ParamInfo, shown: &dyn Fn(f64) -> Option<f64>, samples: Vec<(f64, f64)>, x: f64) -> Result<f64> {
    if samples.len() < 2 {
        bail!("parameter '{}' shows no numbers; give its text or {{\"normalized\": x}}", p.title);
    }
    for w in samples.windows(2) {
        let ((v0, d0), (v1, d1)) = (w[0], w[1]);
        if close(d0, x) {
            return Ok(v0);
        }
        if (d0 - x) * (d1 - x) < 0.0 {
            let (mut a, mut b, mut da) = (v0, v1, d0);
            for _ in 0..60 {
                let m = 0.5 * (a + b);
                let Some(dm) = shown(m) else { break };
                if close(dm, x) {
                    return Ok(m);
                }
                if (da - x) * (dm - x) <= 0.0 {
                    b = m;
                } else {
                    a = m;
                    da = dm;
                }
            }
            return Ok(0.5 * (a + b));
        }
    }
    if let Some(&(v, d)) = samples.last()
        && close(d, x)
    {
        return Ok(v);
    }
    let (mn, mx) = samples.iter().fold((f64::MAX, f64::MIN), |(a, b), s| (a.min(s.1), b.max(s.1)));
    let unit = access.param_text(p.id, 1.0).map(|t| t.trim_start_matches(|c: char| c.is_ascii_digit() || "+-. ".contains(c)).trim().to_string()).unwrap_or_default();
    bail!("parameter '{}': {x} is outside its range {mn}..{mx} {unit}", p.title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(id: u32, title: &str, steps: i32) -> ParamInfo {
        ParamInfo { id, title: title.into(), short_title: String::new(), units: String::new(), step_count: steps, default: 0.0, unit_id: 0, flags: 1 }
    }

    /// 1: volume -48..+12 dB linear; 2: type list Sine/Saw/Square; 3: cutoff 20 Hz..20 kHz exponential, kHz above 1000;
    /// 4: a parser that ignores units (returns garbage), 0..100 %.
    struct Mock;
    impl ParamAccess for Mock {
        fn param_text(&self, id: u32, v: f64) -> Option<String> {
            match id {
                1 => Some(format!("{:.2} dB", -48.0 + 60.0 * v)),
                2 => Some(["Sine", "Saw", "Square"][(v * 2.0).round() as usize].into()),
                3 => {
                    let hz = 20.0 * 1000f64.powf(v);
                    Some(if hz >= 1000.0 { format!("{:.3} kHz", hz / 1000.0) } else { format!("{hz:.2} Hz") })
                }
                4 => Some(format!("{:.1} %", 100.0 * v)),
                _ => None,
            }
        }
        fn param_from_text(&self, id: u32, t: &str) -> Option<f64> {
            match id {
                1 => display_number(t).map(|x| ((x + 48.0) / 60.0).clamp(0.0, 1.0)),
                4 => Some(0.123),
                _ => None,
            }
        }
    }

    #[test]
    fn find_by_names() {
        let ps = vec![info(1, "A Volume", 0), info(2, "A Osc 1 Type", 2), info(3, "A Filter 1 Cutoff", 0)];
        assert_eq!(find(&ps, "a_osc_1_type").unwrap().id, 2);
        assert_eq!(find(&ps, "A Volume").unwrap().id, 1);
        assert_eq!(find(&ps, "a volume").unwrap().id, 1);
        assert_eq!(find(&ps, "#3").unwrap().id, 3);
        let e = find(&ps, "filter_cutoff").unwrap_err().to_string();
        assert!(e.contains("A Filter 1 Cutoff"), "{e}");
    }

    #[test]
    fn display_numbers() {
        assert_eq!(display_number("-3.00 dB"), Some(-3.0));
        assert_eq!(display_number("1.2 kHz"), Some(1200.0));
        assert_eq!(display_number("50 %"), Some(50.0));
        assert_eq!(display_number("Sine"), None);
        assert_eq!(display_number("1e-3 s"), Some(0.001));
        assert_eq!(display_number("- 6 dB"), Some(-6.0));
    }

    #[test]
    fn values() {
        let m = Mock;
        let vol = info(1, "Vol", 0);
        assert!((normalized(&m, &vol, &json!(-3.0)).unwrap() - 0.75).abs() < 1e-9);
        assert!((normalized(&m, &vol, &json!("-3 dB")).unwrap() - 0.75).abs() < 1e-9);
        assert!(normalized(&m, &vol, &json!(40.0)).is_err());
        let ty = info(2, "Type", 2);
        assert_eq!(normalized(&m, &ty, &json!("square")).unwrap(), 1.0);
        assert_eq!(normalized(&m, &ty, &json!("Saw")).unwrap(), 0.5);
        let e = normalized(&m, &ty, &json!("Noise")).unwrap_err().to_string();
        assert!(e.contains("'Sine', 'Saw', 'Square'"), "{e}");
        let cut = info(3, "Cutoff", 0);
        let v = normalized(&m, &cut, &json!(900.0)).unwrap();
        assert!((20.0 * 1000f64.powf(v) - 900.0).abs() < 0.5, "{v}");
        let v = normalized(&m, &cut, &json!(1200.0)).unwrap();
        assert!((20.0 * 1000f64.powf(v) - 1200.0).abs() < 1.0, "{v}");
        // a parser that returns the wrong value is caught by the read-back and the search takes over
        let pct = info(4, "Mix", 0);
        assert!((normalized(&m, &pct, &json!(50.0)).unwrap() - 0.5).abs() < 1e-3);
        assert_eq!(normalized(&m, &pct, &json!({"normalized": 0.25})).unwrap(), 0.25);
        assert!(normalized(&m, &pct, &json!({"normalized": 2})).is_err());
    }

    /// A rate 0..10 Hz whose display shows its current value (0.27 Hz to start) whatever it is asked about,
    /// like Valhalla Supermassive; it may take the controller's value being moved, and may have a parser.
    struct Stuck {
        now: std::cell::Cell<f64>,
        probe: bool,
        parser: bool,
    }
    impl Stuck {
        fn new(probe: bool, parser: bool) -> Self {
            Stuck { now: std::cell::Cell::new(0.027), probe, parser }
        }
    }
    impl ParamAccess for Stuck {
        fn param_text(&self, _: u32, _: f64) -> Option<String> {
            Some(format!("{:.2} Hz", 10.0 * self.now.get()))
        }
        fn param_from_text(&self, _: u32, t: &str) -> Option<f64> {
            if self.parser { display_number(t).map(|x| (x / 10.0).clamp(0.0, 1.0)) } else { None }
        }
        fn current(&self, _: u32) -> Option<f64> {
            Some(self.now.get())
        }
        fn probe(&self, _: u32, v: f64) -> bool {
            if self.probe {
                self.now.set(v);
            }
            self.probe
        }
    }

    #[test]
    fn a_display_stuck_on_the_current_value_is_read_by_moving_the_value() {
        let rate = info(5, "ModRate", 0);
        // moving the controller's value makes the display searchable; the value is put back afterwards
        let m = Stuck::new(true, false);
        let v = normalized(&m, &rate, &json!(2.75)).unwrap();
        assert!((10.0 * v - 2.75).abs() < 0.006, "{v}");
        assert_eq!(m.now.get(), 0.027);
        assert!((normalized(&m, &rate, &json!("0.5 Hz")).unwrap() - 0.05).abs() < 1e-3);
        assert!(normalized(&m, &rate, &json!(12.0)).unwrap_err().to_string().contains("outside its range 0..10"));
        assert_eq!(m.now.get(), 0.027);
        // without that, the plugin's parser
        let m = Stuck::new(false, true);
        assert!((normalized(&m, &rate, &json!("0.275 Hz")).unwrap() - 0.0275).abs() < 1e-9);
        assert!((normalized(&m, &rate, &json!(2.0)).unwrap() - 0.2).abs() < 1e-9);
        // and without either, only the current value can be named
        let m = Stuck::new(false, false);
        assert_eq!(normalized(&m, &rate, &json!(0.27)).unwrap(), 0.027);
        let e = normalized(&m, &rate, &json!(0.5)).unwrap_err().to_string();
        assert!(e.contains("shows '0.27 Hz' for every value") && e.contains("normalized"), "{e}");
    }
}
