//! Key and CC names an SFZ declares (`sw_label`, `label_key`, `label_ccN`), read from sfizz's MIDNAM export.

use crate::json::OrderedMap;
use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

/// The SFZ's MIDNAM XML as sfizz exports it.
pub fn export_midnam(sfz: &Path) -> Result<String> {
    super::sfizz::export_midnam(sfz)
}

/// `(key labels, CC labels)` from a MIDNAM document (CC 7, 10 and 11 are left out: sfizz names them itself).
pub fn parse_labels(xml: &str) -> (OrderedMap<String>, OrderedMap<String>) {
    static NOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<Note Number="(\d+)" Name="([^"]*)""#).unwrap());
    static CTRL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"<Control Type="\w+" Number="(\d+)" Name="([^"]*)""#).unwrap());
    let keys = NOTE.captures_iter(xml).map(|m| (m[1].to_string(), unescape(&m[2]))).collect();
    let ccs = CTRL
        .captures_iter(xml)
        .filter(|m| !matches!(&m[1], "7" | "10" | "11"))
        .map(|m| (m[1].to_string(), unescape(&m[2])))
        .collect();
    (keys, ccs)
}

/// XML entity decoding (the five named entities and numeric references).
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 12);
        let decoded = end.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Key and CC labels of an SFZ.
pub fn labels(sfz: &Path) -> Result<(OrderedMap<String>, OrderedMap<String>)> {
    Ok(parse_labels(&export_midnam(sfz)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_entities() {
        let xml = r#"<NoteNameList><Note Number="24" Name="Sus &amp; legato"/><Note Number="25" Name="Pizz &#233;"/></NoteNameList>
<ControlNameList><Control Type="7bit" Number="1" Name="Vibrato"/><Control Type="7bit" Number="7" Name="Volume"/></ControlNameList>"#;
        let (k, c) = parse_labels(xml);
        assert_eq!(k.get("24").unwrap(), "Sus & legato");
        assert_eq!(k.get("25").unwrap(), "Pizz é");
        assert_eq!(c.keys().collect::<Vec<_>>(), vec!["1"]);
        assert_eq!(unescape("a &bogus; b &lt;"), "a &bogus; b <");
    }
}
