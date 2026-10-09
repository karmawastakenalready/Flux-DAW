//! Note names, pitch classes and MIDI numbers (C4 = 60, the convention of `flux_core::pitch`).
//!
//! Names are a letter `A`-`G` (either case), up to two accidentals (`#`/`♯` sharp, `b`/`♭` flat) and, for
//! notes, an octave from -1 to 9: `C4`, `F#2`, `Bb3`, `C#-1`, `Cb4` (= B3). Pitch classes are 0-11 with C = 0.

use crate::error::NoteError;

/// Pitch-class names spelled with sharps (`flux_core::pitch::name` uses the same spelling).
pub const SHARP_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
/// Pitch-class names spelled with flats.
pub const FLAT_NAMES: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];

/// The name of a MIDI pitch with sharps: 60 -> "C4", 66 -> "F#4".
pub fn note_name(pitch: u8) -> String {
    flux_core::pitch::name(pitch)
}

/// The name of a MIDI pitch, with flats when `flats` is set: 70 -> "Bb4".
pub fn note_name_with(pitch: u8, flats: bool) -> String {
    format!("{}{}", pc_name(pitch % 12, flats), pitch as i32 / 12 - 1)
}

/// The name of a pitch class (taken mod 12).
pub fn pc_name(pc: u8, flats: bool) -> &'static str {
    let names = if flats { &FLAT_NAMES } else { &SHARP_NAMES };
    names[(pc % 12) as usize]
}

/// The seven note letters; a letter index (0-6) counts from C.
pub const LETTERS: [char; 7] = ['C', 'D', 'E', 'F', 'G', 'A', 'B'];
/// Pitch class of each natural letter.
const LETTER_PCS: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];

/// The letter index (0 = C ... 6 = B) of a note letter in either case.
pub(crate) fn letter_index(c: char) -> Option<u8> {
    LETTERS.iter().position(|&l| l == c.to_ascii_uppercase()).map(|i| i as u8)
}

/// The name of pitch class `pc` spelled on letter index `letter` (0 = C): `spell(8, 5)` = "Ab",
/// `spell(8, 4)` = "G#", `spell(9, 6)` = "Bbb". Falls back to the sharp name when the letter is more than
/// two semitones away.
pub fn spell(pc: u8, letter: u8) -> String {
    let letter = (letter % 7) as usize;
    let diff = (pc as i32 - LETTER_PCS[letter] as i32 + 6).rem_euclid(12) - 6;
    match diff {
        -2..=2 => {
            let acc = if diff < 0 { "b" } else { "#" };
            format!("{}{}", LETTERS[letter], acc.repeat(diff.unsigned_abs() as usize))
        }
        _ => pc_name(pc, false).to_string(),
    }
}

/// Reads a note letter and its accidentals from the start of `chars`. Returns the semitone offset from C
/// (unwrapped: "Cb" is -1, "B#" is 12) and the number of characters used.
pub(crate) fn scan_letter(chars: &[char]) -> Option<(i32, usize)> {
    let base = LETTER_PCS[letter_index(*chars.first()?)? as usize] as i32;
    let mut v = base;
    let mut n = 1;
    while n < chars.len() && n <= 2 {
        match chars[n] {
            '#' | '♯' => v += 1,
            'b' | '♭' => v -= 1,
            _ => break,
        }
        n += 1;
    }
    Some((v, n))
}

/// A pitch class from a name without octave: "F#" -> 6, "Bb" -> 10, "Cb" -> 11.
pub fn parse_pitch_class(text: &str) -> Result<u8, NoteError> {
    let err = |m: &str| NoteError { input: text.to_string(), message: m.to_string() };
    let chars: Vec<char> = text.trim().chars().collect();
    let (v, n) = scan_letter(&chars).ok_or_else(|| err("expected a note letter A-G"))?;
    if n != chars.len() {
        return Err(err("unexpected text after the note name (accidentals are #, b, ♯ or ♭, at most two)"));
    }
    Ok(v.rem_euclid(12) as u8)
}

/// A MIDI pitch from a note name ("C4", "F#2", "Bb-1") or a MIDI number written as text ("60").
pub fn parse_note(text: &str) -> Result<u8, NoteError> {
    let err = |m: String| NoteError { input: text.to_string(), message: m };
    let t = text.trim();
    if t.is_empty() {
        return Err(err("empty note name".into()));
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return match t.parse::<u32>() {
            Ok(n) if n <= 127 => Ok(n as u8),
            _ => Err(err("MIDI numbers go from 0 to 127".into())),
        };
    }
    let chars: Vec<char> = t.chars().collect();
    let (v, n) = scan_letter(&chars).ok_or_else(|| err("expected a note letter A-G followed by an octave, e.g. C4 or F#2".into()))?;
    let rest: String = chars[n..].iter().collect();
    if rest.is_empty() {
        return Err(err("missing octave (e.g. C4; middle C is C4 = 60)".into()));
    }
    let digits = rest.strip_prefix('-').unwrap_or(&rest);
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err(format!("bad octave {rest:?} (octaves go from -1 to 9; accidentals are #, b, ♯ or ♭, at most two)")));
    }
    let oct: i32 = rest.parse().map_err(|_| err(format!("bad octave {rest:?}")))?;
    let midi = 12 * (oct + 1) + v;
    if !(0..=127).contains(&midi) {
        let side = if midi < 0 { "below the MIDI range (lowest C-1 = 0)" } else { "above the MIDI range (highest G9 = 127)" };
        return Err(err(side.into()));
    }
    Ok(midi as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_like_flux_core() {
        for name in ["C4", "C#4", "Db4", "B3", "Cb4", "B#3", "F#2", "Bb-1", "C-1", "G9", "e5", "bb3", "Ebb4"] {
            assert_eq!(parse_note(name).ok(), flux_core::pitch::parse(name), "{name}");
        }
        assert_eq!(parse_note("C4"), Ok(60));
        assert_eq!(parse_note("Cb4"), Ok(59));
        assert_eq!(parse_note("B#3"), Ok(60));
        assert_eq!(parse_note("C♯4"), Ok(61));
        assert_eq!(parse_note("D♭4"), Ok(61));
        assert_eq!(parse_note(" 60 "), Ok(60));
        assert_eq!(parse_note("C-1"), Ok(0));
        assert_eq!(parse_note("G9"), Ok(127));
    }

    #[test]
    fn note_errors_say_what_is_wrong() {
        let e = parse_note("H4").unwrap_err();
        assert!(e.message.contains("note letter"), "{e}");
        assert!(parse_note("C").unwrap_err().message.contains("missing octave"));
        assert!(parse_note("G#9").unwrap_err().message.contains("above"));
        assert!(parse_note("Cb-1").unwrap_err().message.contains("below"));
        assert!(parse_note("C4x").unwrap_err().message.contains("bad octave"));
        assert!(parse_note("128").unwrap_err().message.contains("0 to 127"));
        assert!(parse_note("").is_err());
        assert!(parse_note("C###4").is_err());
    }

    #[test]
    fn pitch_classes_and_names() {
        assert_eq!(parse_pitch_class("F#"), Ok(6));
        assert_eq!(parse_pitch_class("Cb"), Ok(11));
        assert_eq!(parse_pitch_class("B#"), Ok(0));
        assert!(parse_pitch_class("F#4").is_err());
        assert_eq!(note_name(61), "C#4");
        assert_eq!(note_name_with(70, true), "Bb4");
        assert_eq!(note_name_with(0, false), "C-1");
        assert_eq!(pc_name(15, false), "D#");
    }

    #[test]
    fn spelling_on_a_letter() {
        assert_eq!(spell(8, 5), "Ab");
        assert_eq!(spell(8, 4), "G#");
        assert_eq!(spell(9, 6), "Bbb");
        assert_eq!(spell(0, 6), "B#");
        assert_eq!(spell(11, 0), "Cb");
        assert_eq!(spell(3, 1), "D#");
        assert_eq!(spell(6, 0), "F#");
        assert_eq!(letter_index('g'), Some(4));
        assert_eq!(letter_index('H'), None);
    }
}
