//! Text encoding of SFZ files.
//!
//! SFZ has no declared encoding. Almost every file is ASCII or UTF-8 (sfizz treats paths as UTF-8); a few
//! old libraries contain Latin-1 bytes in comments. Files are decoded as UTF-8 (a byte order mark is
//! remembered) and fall back to Latin-1, which maps every byte to one character, so any file can be read
//! and written back byte for byte.

use serde::Serialize;

/// How a file's bytes were turned into text; [`encode`] reverses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Utf8,
    /// UTF-8 with a byte order mark (EF BB BF) in front.
    Utf8Bom,
    /// Not valid UTF-8: every byte is one character U+0000..U+00FF.
    Latin1,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Decode the bytes of an SFZ file.
pub fn decode(bytes: &[u8]) -> (String, Encoding) {
    let (body, bom) = match bytes.strip_prefix(BOM) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    match std::str::from_utf8(body) {
        Ok(s) => (s.to_owned(), if bom { Encoding::Utf8Bom } else { Encoding::Utf8 }),
        Err(_) => (bytes.iter().map(|&b| b as char).collect(), Encoding::Latin1),
    }
}

/// Encode text the way it was decoded. Characters a Latin-1 file cannot hold become `?`.
pub fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => [BOM, text.as_bytes()].concat(),
        Encoding::Latin1 => text.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_round_trip() {
        let (s, e) = decode("sample=é.wav".as_bytes());
        assert_eq!((s.as_str(), e), ("sample=é.wav", Encoding::Utf8));
        assert_eq!(encode(&s, e), "sample=é.wav".as_bytes());
    }

    #[test]
    fn bom_is_remembered() {
        let bytes = b"\xEF\xBB\xBF<region>";
        let (s, e) = decode(bytes);
        assert_eq!((s.as_str(), e), ("<region>", Encoding::Utf8Bom));
        assert_eq!(encode(&s, e), bytes);
    }

    #[test]
    fn latin1_fallback_is_lossless() {
        let bytes = b"// caf\xE9\n<region> sample=a.wav";
        let (s, e) = decode(bytes);
        assert_eq!(e, Encoding::Latin1);
        assert!(s.starts_with("// café"));
        assert_eq!(encode(&s, e), bytes);
    }
}
