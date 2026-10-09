//! Reading opcode values the way sfizz 1.2.3 does.
//!
//! * Numbers are read from the front of the value and the rest is ignored: `60.7` read as an integer is 60,
//!   `-3dB` read as a float is -3.
//! * Key opcodes (`lokey`, `pitch_keycenter`, `sw_last`...) also accept note names: a letter, an optional `#`/`♯`
//!   or `b`/`♭`, and an octave, with C4 = 60 (so C-1 = 0). `e#` and `b#`, `cb` and `fb` are rejected.
//! * A key outside 0..=127 is invalid; sfizz then uses the opcode's default (`lokey=200` gives lokey 0).

const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// Note name of a MIDI key with sharps and sfizz's octave numbering (60 = "C4", 0 = "C-1").
pub fn note_name(key: u8) -> String {
    format!("{}{}", NAMES[usize::from(key % 12)], i32::from(key) / 12 - 1)
}

/// `"C4 (60)"`: name and number, the form used in reports.
pub fn note_label(key: u8) -> String {
    format!("{} ({key})", note_name(key))
}

/// The leading integer of a value (`[+-]?[0-9]+`), as sfizz reads integer opcodes.
pub fn leading_int(value: &str) -> Option<i64> {
    let b = value.trim().as_bytes();
    let mut end = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits = b[end..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    std::str::from_utf8(&b[..end + digits]).ok()?.parse().ok()
}

/// The leading decimal number of a value (`[+-]?[0-9]*(\.[0-9]*)?`, at least one digit), as sfizz reads
/// float opcodes.
pub fn leading_float(value: &str) -> Option<f64> {
    let b = value.trim().as_bytes();
    let mut end = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let int_digits = b[end..].iter().take_while(|c| c.is_ascii_digit()).count();
    end += int_digits;
    let mut frac_digits = 0;
    if b.get(end) == Some(&b'.') {
        frac_digits = b[end + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        end += 1 + frac_digits;
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    std::str::from_utf8(&b[..end]).ok()?.parse().ok()
}

/// A note name (`c4`, `C#4`, `db4`, `c♯4`, `e♭4`, `C-1`) as a MIDI key, or `None` if it is not a valid name or
/// lies outside 0..=127.
pub fn parse_note_name(value: &str) -> Option<u8> {
    let mut chars = value.chars();
    let letter = chars.next()?.to_ascii_lowercase();
    let mut note: i64 = match letter {
        'c' => 0,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => return None,
    };
    let mut rest = chars.as_str();
    for (prefix, delta) in [("#", 1), ("♯", 1), ("b", -1), ("♭", -1)] {
        if let Some(r) = rest.strip_prefix(prefix) {
            if (delta == 1 && matches!(letter, 'b' | 'e')) || (delta == -1 && matches!(letter, 'c' | 'f')) {
                return None;
            }
            note += delta;
            rest = r;
            break;
        }
    }
    // absl::SimpleAtoi: surrounding whitespace allowed, the rest must be an integer
    let octave: i64 = rest.trim().parse().ok()?;
    let key = note + (octave + 1) * 12;
    u8::try_from(key).ok().filter(|k| *k <= 127)
}

/// A key opcode value: a number (leading integer) or a note name. `None` if invalid or outside 0..=127.
pub fn parse_key(value: &str) -> Option<u8> {
    let v = value.trim();
    let starts_numeric = matches!(v.as_bytes().first(), Some(b'0'..=b'9' | b'+' | b'-'));
    if starts_numeric {
        leading_int(v).and_then(|n| u8::try_from(n).ok()).filter(|k| *k <= 127)
    } else {
        parse_note_name(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names_follow_sfizz() {
        assert_eq!(parse_key("c4"), Some(60));
        assert_eq!(parse_key("C4"), Some(60));
        assert_eq!(parse_key("c#4"), Some(61));
        assert_eq!(parse_key("db4"), Some(61));
        assert_eq!(parse_key("Bb3"), Some(58));
        assert_eq!(parse_key("c♯4"), Some(61));
        assert_eq!(parse_key("e♭4"), Some(63));
        assert_eq!(parse_key("C-1"), Some(0));
        assert_eq!(parse_key("g9"), Some(127));
        assert_eq!(parse_key("g#9"), None);
        assert_eq!(parse_key("e#4"), None);
        assert_eq!(parse_key("cb4"), None);
        assert_eq!(parse_key("c 4"), Some(60));
        assert_eq!(parse_key("h4"), None);
        assert_eq!(parse_key(""), None);
    }

    #[test]
    fn numbers_are_leading_prefixes() {
        assert_eq!(parse_key("60.7"), Some(60));
        assert_eq!(parse_key("60 = 5"), Some(60));
        assert_eq!(parse_key("130"), None);
        assert_eq!(parse_key("-5"), None);
        assert_eq!(leading_int("6EY2"), Some(6));
        assert_eq!(leading_int("+12"), Some(12));
        assert_eq!(leading_int("-"), None);
        assert_eq!(leading_float("-3.5dB"), Some(-3.5));
        assert_eq!(leading_float(".5"), Some(0.5));
        assert_eq!(leading_float("7."), Some(7.0));
        assert_eq!(leading_float("x"), None);
    }

    #[test]
    fn names_round_trip() {
        for k in 0..=127u8 {
            assert_eq!(parse_key(&note_name(k)), Some(k), "{}", note_name(k));
        }
        assert_eq!(note_label(61), "C#4 (61)");
    }
}
