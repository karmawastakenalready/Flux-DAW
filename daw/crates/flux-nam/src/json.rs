//! A small strict reader over `serde_json` objects: typed field access with errors that name the location, and
//! detection of unknown keys (typos in a rig file, or `.nam` features this crate does not implement).

use crate::error::Error;
use serde_json::{Map, Value};

/// Which error variant a reader reports through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Domain {
    Rig,
    Model,
}

impl Domain {
    pub(crate) fn error(self, at: impl Into<String>, message: impl Into<String>) -> Error {
        match self {
            Domain::Rig => Error::rig(at, message),
            Domain::Model => Error::model(at, message),
        }
    }
}

/// Short description of a JSON value for error messages.
pub(crate) fn describe(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("the boolean {b}"),
        Value::Number(n) => format!("the number {n}"),
        Value::String(s) => format!("the string {s:?}"),
        Value::Array(a) => format!("an array of {} items", a.len()),
        Value::Object(_) => "an object".into(),
    }
}

/// A JSON number as f64, if it is one and finite.
pub(crate) fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64().filter(|x| x.is_finite())
}

/// A JSON number with an integral value (`3` or `3.0`) as i64.
pub(crate) fn as_int(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    let f = v.as_f64()?;
    (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
}

/// The keys of the object at `source.init_cc` in file order. `serde_json::Value` objects sort their keys, but
/// Python dicts (and so `rig.py`) keep file order, and the order is the order controllers are sent. Returns
/// `None` when the text has no such object (the strict parse reports any problem).
pub(crate) fn init_cc_order(text: &str) -> Option<Vec<String>> {
    use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};

    struct Keys(Vec<String>);
    impl<'de> serde::Deserialize<'de> for Keys {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Keys, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Keys;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Keys, A::Error> {
                    let mut keys: Vec<String> = vec![];
                    while let Some(k) = m.next_key::<String>()? {
                        m.next_value::<IgnoredAny>()?;
                        // a repeated key keeps its first position, as in a Python dict
                        if !keys.contains(&k) {
                            keys.push(k);
                        }
                    }
                    Ok(Keys(keys))
                }
            }
            d.deserialize_map(V)
        }
    }
    #[derive(serde::Deserialize)]
    struct Src {
        init_cc: Option<Keys>,
    }
    #[derive(serde::Deserialize)]
    struct Top {
        source: Option<Src>,
    }
    let top: Top = serde_json::from_str(text).ok()?;
    top.source?.init_cc.map(|k| k.0)
}

/// Strict reader over one JSON object.
pub(crate) struct Obj<'a> {
    map: &'a Map<String, Value>,
    at: String,
    domain: Domain,
    used: Vec<&'a str>,
}

impl<'a> Obj<'a> {
    pub(crate) fn new(v: &'a Value, at: impl Into<String>, domain: Domain) -> Result<Obj<'a>, Error> {
        let at = at.into();
        match v {
            Value::Object(map) => Ok(Obj { map, at, domain, used: Vec::new() }),
            other => Err(domain.error(at, format!("expected an object, got {}", describe(other)))),
        }
    }

    /// Location of one of this object's fields.
    pub(crate) fn at_key(&self, key: &str) -> String {
        format!("{}.{key}", self.at)
    }

    pub(crate) fn error(&self, message: impl Into<String>) -> Error {
        self.domain.error(self.at.clone(), message)
    }

    pub(crate) fn field_error(&self, key: &str, message: impl Into<String>) -> Error {
        self.domain.error(self.at_key(key), message)
    }

    /// The raw value of `key` (marked as used). `null` counts as absent, as `dict.get` does in the Python tools.
    pub(crate) fn get(&mut self, key: &str) -> Option<&'a Value> {
        let (k, v) = self.map.get_key_value(key)?;
        self.used.push(k.as_str());
        (!v.is_null()).then_some(v)
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.map.get(key).is_some_and(|v| !v.is_null())
    }

    pub(crate) fn required(&mut self, key: &str) -> Result<&'a Value, Error> {
        self.get(key).ok_or_else(|| self.field_error(key, "missing required field"))
    }

    pub(crate) fn opt_num(&mut self, key: &str) -> Result<Option<f64>, Error> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => as_f64(v)
                .map(Some)
                .ok_or_else(|| self.field_error(key, format!("expected a finite number, got {}", describe(v)))),
        }
    }

    pub(crate) fn num(&mut self, key: &str) -> Result<f64, Error> {
        self.opt_num(key)?.ok_or_else(|| self.field_error(key, "missing required number"))
    }

    pub(crate) fn num_or(&mut self, key: &str, default: f64) -> Result<f64, Error> {
        Ok(self.opt_num(key)?.unwrap_or(default))
    }

    pub(crate) fn opt_int(&mut self, key: &str) -> Result<Option<i64>, Error> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => as_int(v)
                .map(Some)
                .ok_or_else(|| self.field_error(key, format!("expected an integer, got {}", describe(v)))),
        }
    }

    /// A non-negative integer, required.
    pub(crate) fn usize(&mut self, key: &str) -> Result<usize, Error> {
        let v = self.opt_int(key)?.ok_or_else(|| self.field_error(key, "missing required integer"))?;
        usize::try_from(v).map_err(|_| self.field_error(key, format!("expected a non-negative integer, got {v}")))
    }

    pub(crate) fn usize_or(&mut self, key: &str, default: usize) -> Result<usize, Error> {
        if self.has(key) { self.usize(key) } else { Ok(default) }
    }

    pub(crate) fn string(&mut self, key: &str) -> Result<&'a str, Error> {
        let v = self.required(key)?;
        v.as_str().ok_or_else(|| self.field_error(key, format!("expected a string, got {}", describe(v))))
    }

    pub(crate) fn opt_string(&mut self, key: &str) -> Result<Option<&'a str>, Error> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => {
                v.as_str().map(Some).ok_or_else(|| self.field_error(key, format!("expected a string, got {}", describe(v))))
            }
        }
    }

    pub(crate) fn bool_or(&mut self, key: &str, default: bool) -> Result<bool, Error> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| self.field_error(key, format!("expected true or false, got {}", describe(v)))),
        }
    }

    pub(crate) fn opt_array(&mut self, key: &str) -> Result<Option<&'a [Value]>, Error> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_array()
                .map(|a| Some(a.as_slice()))
                .ok_or_else(|| self.field_error(key, format!("expected an array, got {}", describe(v)))),
        }
    }

    pub(crate) fn array(&mut self, key: &str) -> Result<&'a [Value], Error> {
        self.opt_array(key)?.ok_or_else(|| self.field_error(key, "missing required array"))
    }

    /// A nested object field.
    pub(crate) fn child(&mut self, key: &str) -> Result<Obj<'a>, Error> {
        let at = self.at_key(key);
        let v = self.required(key)?;
        Obj::new(v, at, self.domain)
    }

    pub(crate) fn opt_child(&mut self, key: &str) -> Result<Option<Obj<'a>>, Error> {
        let at = self.at_key(key);
        match self.get(key) {
            None => Ok(None),
            Some(v) => Obj::new(v, at, self.domain).map(Some),
        }
    }

    /// Mark keys as read without interpreting them (fields that carry no meaning for processing).
    pub(crate) fn ignore(&mut self, keys: &[&str]) {
        for k in keys {
            let _ = self.get(k);
        }
    }

    /// Fail if the object has keys nobody read. `what` explains the consequence ("unknown field" or
    /// "unsupported .nam feature").
    pub(crate) fn finish(self, what: &str) -> Result<(), Error> {
        let mut unknown: Vec<&str> = self.map.keys().map(String::as_str).filter(|k| !self.used.contains(k)).collect();
        if unknown.is_empty() {
            return Ok(());
        }
        unknown.sort_unstable();
        let list = unknown.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>().join(", ");
        Err(self.domain.error(self.at, format!("{what}: {list}")))
    }
}
