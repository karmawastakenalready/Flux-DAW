//! Pitch names <-> MIDI numbers (C4 = 60).

const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

pub fn name(p: u8) -> String {
    format!("{}{}", NAMES[(p % 12) as usize], p as i32 / 12 - 1)
}

pub fn parse(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return (0..=127).contains(&n).then_some(n as u8);
    }
    let mut chars = s.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    let mut v: i32 = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let mut i = 0;
    let bytes = rest.as_bytes();
    while i < bytes.len() && (bytes[i] == b'#' || bytes[i] == b'b') {
        v += if bytes[i] == b'#' { 1 } else { -1 };
        i += 1;
    }
    let oct: i32 = rest[i..].parse().ok()?;
    let m = 12 * (oct + 1) + v;
    (0..=127).contains(&m).then_some(m as u8)
}

/// A pitch given either as a MIDI number or a name like "F#2".
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PitchSpec {
    Num(u8),
    Name(String),
}

impl PitchSpec {
    pub fn resolve(&self) -> anyhow::Result<u8> {
        match self {
            PitchSpec::Num(n) => Ok(*n),
            PitchSpec::Name(s) => parse(s).ok_or_else(|| anyhow::anyhow!("bad pitch '{s}' (use e.g. 60 or \"C4\")")),
        }
    }
}
