//! JSON helpers: an insertion-ordered map and the card file format.
//!
//! `serde_json::Map` sorts its keys unless the workspace-wide `preserve_order` feature is on, and turning that
//! feature on would change every other crate's JSON output. Report cards need a stable, meaningful key order
//! (per-key entries in scan order, flags in severity order), so they use [`OrderedMap`] instead.

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::marker::PhantomData;

/// A JSON object that keeps its keys in insertion order. Inserting an existing key replaces the value in place
/// (the same semantics as a Python dict).
#[derive(Clone, Debug, PartialEq)]
pub struct OrderedMap<V>(Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        OrderedMap(Vec::new())
    }
}

impl<V> OrderedMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace (in place) the value for `key`.
    pub fn insert(&mut self, key: impl Into<String>, value: V) {
        let key = key.into();
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.0.push((key, value));
        }
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.0.iter().map(|(_, v)| v)
    }
}

impl<K: Into<String>, V> FromIterator<(K, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = OrderedMap::new();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V2<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for V2<V> {
            type Value = OrderedMap<V>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut m = OrderedMap::new();
                while let Some((k, v)) = a.next_entry::<String, V>()? {
                    m.insert(k, v);
                }
                Ok(m)
            }
        }
        d.deserialize_map(V2(PhantomData))
    }
}

/// Serialize like Python's `json.dump(obj, f, indent=1, ensure_ascii=False)`, the card file format.
pub fn to_string_indent1<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    let mut out = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
    value.serialize(&mut ser)?;
    // serde_json only ever writes valid UTF-8
    Ok(String::from_utf8(out).unwrap_or_default())
}

/// Python's `repr(float)`: shortest round-trip digits, `.0` on integral values, exponent outside 1e-4..1e16.
pub fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // `{:e}` gives the shortest round-trip digits as d.ddde<exp>
    let e = format!("{x:e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if !(-4..16).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let m = if rest.is_empty() { first.to_string() } else { format!("{first}.{rest}") };
        return format!("{sign}{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let point = exp + 1; // digits before the decimal point
    if point <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{sign}{digits}{}.0", "0".repeat(point as usize - digits.len()))
    } else {
        let (a, b) = digits.split_at(point as usize);
        format!("{sign}{a}.{b}")
    }
}

/// Python's `str()` of a JSON value as printed in an f-string (None, True, ints, float repr, strings raw).
pub fn py_str(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                py_float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_map_keeps_order_and_replaces_in_place() {
        let mut m = OrderedMap::new();
        m.insert("b", 1);
        m.insert("a", 2);
        m.insert("b", 3);
        assert_eq!(m.keys().collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!(m.get("b"), Some(&3));
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"b":3,"a":2}"#);
        let back: OrderedMap<i32> = serde_json::from_str(r#"{"z":1,"y":2,"x":3}"#).unwrap();
        assert_eq!(back.keys().collect::<Vec<_>>(), vec!["z", "y", "x"]);
    }

    #[test]
    fn indent1_matches_python_layout() {
        let v = serde_json::json!({"a": [1, 2], "b": {}, "c": [], "d": "é"});
        assert_eq!(to_string_indent1(&v).unwrap(), "{\n \"a\": [\n  1,\n  2\n ],\n \"b\": {},\n \"c\": [],\n \"d\": \"é\"\n}");
    }

    #[test]
    fn python_float_repr() {
        assert_eq!(py_float(7.0), "7.0");
        assert_eq!(py_float(-22.4), "-22.4");
        assert_eq!(py_float(0.1), "0.1");
        assert_eq!(py_float(1e-5), "1e-05");
        assert_eq!(py_float(0.0001), "0.0001");
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(1234.5), "1234.5");
        assert_eq!(py_float(-0.0), "-0.0");
        assert_eq!(py_float(123456789.0), "123456789.0");
        assert_eq!(py_float(2.5e-7), "2.5e-07");
    }
}
