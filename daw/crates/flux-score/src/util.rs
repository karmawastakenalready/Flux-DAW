//! Small shared helpers: Python-compatible rounding, an insertion-ordered map, a seeded generator and
//! number formatting.

use std::collections::HashMap;
use std::hash::Hash;

/// Python's `round(x, digits)` for floats: the exact binary value rounded to `digits` decimals (ties to
/// even). Rust's fixed-precision float formatting is exact in the same way, so formatting and parsing
/// back reproduces Python bit for bit.
pub(crate) fn py_round(x: f64, digits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// Python's `round(x)` to an integer: ties go to the even neighbour (`round(2.5) == 2`).
pub(crate) fn round_even(x: f64) -> f64 {
    x.round_ties_even()
}

/// A note velocity from a computed value: rounded (ties to even) and clamped to 1..=127.
pub(crate) fn velocity(v: f64) -> u8 {
    if v.is_nan() {
        return 1;
    }
    round_even(v).clamp(1.0, 127.0) as u8
}

/// A CC value from a computed value: rounded (ties to even) and clamped to 0..=127.
pub(crate) fn cc_value(v: f64) -> u8 {
    if v.is_nan() {
        return 0;
    }
    round_even(v).clamp(0.0, 127.0) as u8
}

/// A beat position or length for output: Python's 4-decimal rounding (what score.py's `n()`/`cc()` did),
/// which also removes float noise such as 0.30000000000000004.
pub(crate) fn beat4(x: f64) -> f64 {
    py_round(x, 4)
}

/// `1.5` -> "1.5", `2.0` -> "2", `0.3333333` -> "0.333".
pub(crate) fn fmt_num(x: f64) -> String {
    let s = format!("{:.3}", x + 0.0);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

/// A map that remembers insertion order (findings are reported in the order they were first seen).
#[derive(Debug, Clone)]
pub(crate) struct OrderedMap<K, V> {
    index: HashMap<K, usize>,
    entries: Vec<(K, V)>,
}

impl<K: Eq + Hash + Clone, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        OrderedMap { index: HashMap::new(), entries: Vec::new() }
    }
}

impl<K: Eq + Hash + Clone, V> OrderedMap<K, V> {
    pub(crate) fn entry_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        let i = match self.index.get(&key) {
            Some(&i) => i,
            None => {
                self.entries.push((key.clone(), make()));
                self.index.insert(key, self.entries.len() - 1);
                self.entries.len() - 1
            }
        };
        &mut self.entries[i].1
    }

    pub(crate) fn into_entries(self) -> Vec<(K, V)> {
        self.entries
    }
}

/// splitmix64: a tiny, well-mixed deterministic generator, so seeded variation is reproducible on every
/// platform (the same family flux-core's `humanize` uses).
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> SplitMix64 {
        SplitMix64(seed ^ 0x6A09_E667_F3BC_C908)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [a, b).
    pub(crate) fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.unit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_round_matches_python() {
        // values checked against CPython's round()
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(0.375, 2), 0.38);
        assert_eq!(py_round(2.675, 2), 2.67); // 2.675 is really 2.67499999...
        assert_eq!(py_round(1.00005, 4), 1.0001); // 1.00005 is really 1.0000500000000001
        assert_eq!(py_round(0.1 + 0.2, 4), 0.3);
        assert_eq!(py_round(-0.33335, 4), -0.3333);
        assert_eq!(py_round(2.5, 0), 2.0);
        assert_eq!(py_round(3.5, 0), 4.0);
    }

    #[test]
    fn rounding_helpers() {
        assert_eq!(round_even(64.5), 64.0);
        assert_eq!(round_even(65.5), 66.0);
        assert_eq!(velocity(200.0), 127);
        assert_eq!(velocity(-3.0), 1);
        assert_eq!(velocity(f64::NAN), 1);
        assert_eq!(cc_value(-0.4), 0);
        assert_eq!(cc_value(127.6), 127);
        assert_eq!(fmt_num(1.5), "1.5");
        assert_eq!(fmt_num(2.0), "2");
        assert_eq!(fmt_num(-0.0001), "0");
    }

    #[test]
    fn ordered_map_keeps_first_insertion_order() {
        let mut m: OrderedMap<&str, Vec<u32>> = OrderedMap::default();
        m.entry_or_insert_with("b", Vec::new).push(1);
        m.entry_or_insert_with("a", Vec::new).push(2);
        m.entry_or_insert_with("b", Vec::new).push(3);
        let e = m.into_entries();
        assert_eq!(e, vec![("b", vec![1, 3]), ("a", vec![2])]);
    }

    #[test]
    fn generator_is_deterministic_and_bounded() {
        let mut a = SplitMix64::new(7);
        let mut b = SplitMix64::new(7);
        for _ in 0..1000 {
            let x = a.uniform(-2.0, 2.0);
            assert_eq!(x, b.uniform(-2.0, 2.0));
            assert!((-2.0..2.0).contains(&x));
        }
        assert_ne!(SplitMix64::new(1).next_u64(), SplitMix64::new(2).next_u64());
    }
}
