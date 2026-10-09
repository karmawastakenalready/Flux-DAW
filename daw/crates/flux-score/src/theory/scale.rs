//! Scales, modes and keys ("E minor", "F# dorian", "Bb", "G mixolydian"), and what a pitch class is over a
//! chord in a key: a chord tone, an available tension, an avoid note or outside.

use crate::error::KeyError;
use crate::theory::chord::{Chord, Role};
use crate::theory::note::{letter_index, pc_name, scan_letter, spell};
use serde::Serialize;
use std::fmt;
use std::str::FromStr;

// ---------------------------------------------------------------------------------------------
// pitch-class sets

/// A set of pitch classes (0-11), as a bit mask.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PcSet(u16);

impl PcSet {
    pub const EMPTY: PcSet = PcSet(0);
    pub const ALL: PcSet = PcSet(0x0FFF);

    pub fn contains(self, pc: u8) -> bool {
        self.0 & (1 << (pc % 12)) != 0
    }

    pub fn insert(&mut self, pc: u8) {
        self.0 |= 1 << (pc % 12);
    }

    pub fn remove(&mut self, pc: u8) {
        self.0 &= !(1 << (pc % 12));
    }

    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn union(self, other: PcSet) -> PcSet {
        PcSet(self.0 | other.0)
    }

    /// The pitch classes in ascending order from C.
    pub fn iter(self) -> impl Iterator<Item = u8> {
        (0..12u8).filter(move |&pc| self.contains(pc))
    }
}

impl FromIterator<u8> for PcSet {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> PcSet {
        let mut s = PcSet::EMPTY;
        for pc in iter {
            s.insert(pc);
        }
        s
    }
}

impl fmt::Debug for PcSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter().map(|pc| pc_name(pc, false))).finish()
    }
}

// ---------------------------------------------------------------------------------------------
// modes

/// A scale type. The seven-note ones are spelled one letter per degree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Major,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Minor,
    Locrian,
    HarmonicMinor,
    MelodicMinor,
    PhrygianDominant,
    MajorPentatonic,
    MinorPentatonic,
    Blues,
    WholeTone,
    /// Half-whole diminished (the dominant diminished scale: 1 b2 #2 3 #4 5 6 b7).
    DiminishedHalfWhole,
    /// Whole-half diminished (1 2 b3 4 b5 b6 6 7).
    DiminishedWholeHalf,
    Chromatic,
}

struct ModeInfo {
    mode: Mode,
    name: &'static str,
    intervals: &'static [u8],
    /// Letter offset of the tonic above its parent major key's tonic (Dorian 1: D dorian is in C), so the
    /// key signature (and the sharp/flat spelling) follows from the tonic.
    parent_letter: u8,
    /// Semitones of the tonic above the parent major tonic.
    parent_semis: u8,
    aliases: &'static [&'static str],
}

const MODES: &[ModeInfo] = &[
    ModeInfo { mode: Mode::Major, name: "major", intervals: &[0, 2, 4, 5, 7, 9, 11], parent_letter: 0, parent_semis: 0, aliases: &["major", "maj", "ionian"] },
    ModeInfo { mode: Mode::Dorian, name: "dorian", intervals: &[0, 2, 3, 5, 7, 9, 10], parent_letter: 1, parent_semis: 2, aliases: &["dorian"] },
    ModeInfo { mode: Mode::Phrygian, name: "phrygian", intervals: &[0, 1, 3, 5, 7, 8, 10], parent_letter: 2, parent_semis: 4, aliases: &["phrygian"] },
    ModeInfo { mode: Mode::Lydian, name: "lydian", intervals: &[0, 2, 4, 6, 7, 9, 11], parent_letter: 3, parent_semis: 5, aliases: &["lydian"] },
    ModeInfo { mode: Mode::Mixolydian, name: "mixolydian", intervals: &[0, 2, 4, 5, 7, 9, 10], parent_letter: 4, parent_semis: 7, aliases: &["mixolydian", "mixo"] },
    ModeInfo { mode: Mode::Minor, name: "minor", intervals: &[0, 2, 3, 5, 7, 8, 10], parent_letter: 5, parent_semis: 9, aliases: &["minor", "min", "aeolian", "natural minor"] },
    ModeInfo { mode: Mode::Locrian, name: "locrian", intervals: &[0, 1, 3, 5, 6, 8, 10], parent_letter: 6, parent_semis: 11, aliases: &["locrian"] },
    ModeInfo { mode: Mode::HarmonicMinor, name: "harmonic minor", intervals: &[0, 2, 3, 5, 7, 8, 11], parent_letter: 5, parent_semis: 9, aliases: &["harmonic minor", "harm minor"] },
    ModeInfo { mode: Mode::MelodicMinor, name: "melodic minor", intervals: &[0, 2, 3, 5, 7, 9, 11], parent_letter: 5, parent_semis: 9, aliases: &["melodic minor", "jazz minor"] },
    ModeInfo { mode: Mode::PhrygianDominant, name: "phrygian dominant", intervals: &[0, 1, 4, 5, 7, 8, 10], parent_letter: 2, parent_semis: 4, aliases: &["phrygian dominant", "phrygian major", "spanish phrygian"] },
    ModeInfo { mode: Mode::MajorPentatonic, name: "major pentatonic", intervals: &[0, 2, 4, 7, 9], parent_letter: 0, parent_semis: 0, aliases: &["major pentatonic", "pentatonic"] },
    ModeInfo { mode: Mode::MinorPentatonic, name: "minor pentatonic", intervals: &[0, 3, 5, 7, 10], parent_letter: 5, parent_semis: 9, aliases: &["minor pentatonic"] },
    ModeInfo { mode: Mode::Blues, name: "blues", intervals: &[0, 3, 5, 6, 7, 10], parent_letter: 5, parent_semis: 9, aliases: &["blues", "minor blues"] },
    ModeInfo { mode: Mode::WholeTone, name: "whole tone", intervals: &[0, 2, 4, 6, 8, 10], parent_letter: 0, parent_semis: 0, aliases: &["whole tone", "wholetone"] },
    ModeInfo { mode: Mode::DiminishedHalfWhole, name: "half-whole diminished", intervals: &[0, 1, 3, 4, 6, 7, 9, 10], parent_letter: 0, parent_semis: 0, aliases: &["half whole diminished", "half whole", "dominant diminished"] },
    ModeInfo { mode: Mode::DiminishedWholeHalf, name: "whole-half diminished", intervals: &[0, 2, 3, 5, 6, 8, 9, 11], parent_letter: 0, parent_semis: 0, aliases: &["whole half diminished", "whole half", "diminished", "octatonic"] },
    ModeInfo { mode: Mode::Chromatic, name: "chromatic", intervals: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], parent_letter: 0, parent_semis: 0, aliases: &["chromatic"] },
];

impl Mode {
    fn info(self) -> &'static ModeInfo {
        MODES.iter().find(|m| m.mode == self).expect("every mode has an entry in MODES")
    }

    /// Semitones above the tonic, ascending from 0.
    pub fn intervals(self) -> &'static [u8] {
        self.info().intervals
    }

    /// The name used in key strings and messages: "minor", "dorian", "harmonic minor".
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// Seven-note modes are spelled with one letter per degree.
    pub fn is_heptatonic(self) -> bool {
        self.intervals().len() == 7
    }

    /// Every mode with its accepted names (for error messages and tool descriptions).
    pub fn all() -> impl Iterator<Item = Mode> {
        MODES.iter().map(|m| m.mode)
    }

    /// A mode from its name: "minor", "Dorian", "harmonic-minor", "m", "M", "" (major), "aeolian",
    /// "mixo"... Case, hyphens, underscores and a trailing "mode"/"scale" are ignored (except "m"/"M").
    pub fn parse(text: &str) -> Option<Mode> {
        let t = text.trim();
        match t {
            "" | "M" => return Some(Mode::Major),
            "m" | "-" => return Some(Mode::Minor),
            _ => {}
        }
        let lower = t.to_lowercase().replace(['-', '_'], " ");
        let mut words: Vec<&str> = lower.split_whitespace().collect();
        if words.len() > 1 && matches!(words.last(), Some(&("mode" | "scale" | "key"))) {
            words.pop();
        }
        let norm = words.join(" ");
        MODES.iter().find(|m| m.aliases.contains(&norm.as_str())).map(|m| m.mode)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------------------------
// scales and keys

/// A scale on a tonic: a key ("E minor") or a mode ("F# dorian").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Scale {
    /// Tonic pitch class.
    pub root: u8,
    /// Letter index of the tonic's spelling (0 = C ... 6 = B), which decides how the scale is spelled.
    pub root_letter: u8,
    pub mode: Mode,
}

/// The usual spelling of each major-key tonic: Db, Eb, F#, Ab, Bb.
const MAJOR_TONIC_LETTER: [u8; 12] = [0, 1, 1, 2, 2, 3, 3, 4, 5, 5, 6, 6];

impl Scale {
    /// A scale on `root` with the conventional spelling (the one whose key signature has the fewest
    /// accidentals: Bb major, F# minor, Eb dorian).
    pub fn new(root: u8, mode: Mode) -> Scale {
        let root = root % 12;
        let info = mode.info();
        let parent = (root + 12 - info.parent_semis) % 12;
        let root_letter = (MAJOR_TONIC_LETTER[parent as usize] + info.parent_letter) % 7;
        Scale { root, root_letter, mode }
    }

    /// Parses a key: a tonic ("E", "F#", "Bb", "E flat") and a mode name ("minor", "m", "dorian",
    /// "harmonic minor"; none = major). Text in parentheses is ignored, so a project key such as
    /// "F# Phrygian (piano in D major)" reads as F# phrygian.
    pub fn parse(text: &str) -> Result<Scale, KeyError> {
        let err = |m: String| KeyError { input: text.to_string(), message: m };
        let main = text.split('(').next().unwrap_or("").trim();
        let chars: Vec<char> = main.chars().collect();
        let (mut v, mut n) = scan_letter(&chars)
            .ok_or_else(|| err("a key starts with its tonic, a note letter A-G (e.g. \"E minor\", \"F# dorian\", \"Bb\")".into()))?;
        let root_letter = letter_index(chars[0]).unwrap_or(0);
        let mut rest: String = chars[n..].iter().collect();
        if n == 1 {
            // "E flat minor", "F-sharp dorian"
            let lower = rest.to_ascii_lowercase();
            let trimmed = lower.trim_start_matches([' ', '-']);
            for (word, d) in [("flat", -1), ("sharp", 1)] {
                if let Some(after) = trimmed.strip_prefix(word)
                    && after.chars().next().is_none_or(|c| !c.is_alphabetic())
                {
                    v += d;
                    n = rest.len() - after.len();
                    rest = rest[n..].to_string();
                    break;
                }
            }
        }
        let mode = Mode::parse(&rest).ok_or_else(|| {
            let names: Vec<&str> = MODES.iter().map(|m| m.name).collect();
            err(format!("unknown mode {:?} (use one of: {}; \"m\" = minor, nothing = major)", rest.trim(), names.join(", ")))
        })?;
        Ok(Scale { root: v.rem_euclid(12) as u8, root_letter, mode })
    }

    /// The scale's pitch classes, from the tonic up.
    pub fn pcs(&self) -> Vec<u8> {
        self.mode.intervals().iter().map(|i| (self.root + i) % 12).collect()
    }

    pub fn pc_set(&self) -> PcSet {
        self.pcs().into_iter().collect()
    }

    pub fn contains(&self, pc: u8) -> bool {
        self.mode.intervals().contains(&((pc % 12 + 12 - self.root) % 12))
    }

    /// The 0-based scale degree of a pitch class (0 = tonic), if it is in the scale.
    pub fn degree_of(&self, pc: u8) -> Option<usize> {
        let iv = (pc % 12 + 12 - self.root) % 12;
        self.mode.intervals().iter().position(|&i| i == iv)
    }

    /// Whether the key signature uses flats (D minor, Bb major, C dorian) rather than sharps.
    pub fn prefers_flats(&self) -> bool {
        let info = self.mode.info();
        let parent_letter = (self.root_letter + 7 - info.parent_letter) % 7;
        let parent = (self.root + 12 - info.parent_semis) % 12;
        let name = spell(parent, parent_letter);
        name.contains('b') || name == "F"
    }

    /// The name of a pitch class in this key: scale tones by their degree's letter, other pitch classes
    /// with the key's accidental (sharps or flats).
    pub fn spell(&self, pc: u8) -> String {
        if self.mode.is_heptatonic()
            && let Some(d) = self.degree_of(pc)
        {
            return spell(pc, self.root_letter + d as u8);
        }
        pc_name(pc, self.prefers_flats()).to_string()
    }

    /// The name of a MIDI pitch in this key ("Bb3" in F major, "A#3" in B major). The octave follows the
    /// letter, so B#3 is MIDI 60.
    pub fn note_name(&self, pitch: u8) -> String {
        let name = self.spell(pitch % 12);
        let letter_pc = [0i32, 2, 4, 5, 7, 9, 11][letter_index(name.chars().next().unwrap_or('C')).unwrap_or(0) as usize];
        // the octave of the letter: Cb4 sounds as B3 (pitch 59), B#3 as C4 (pitch 60)
        let shift: i32 = name.chars().skip(1).map(|c| if c == '#' { 1 } else { -1 }).sum();
        let octave = (pitch as i32 - shift - letter_pc).div_euclid(12) - 1;
        format!("{name}{octave}")
    }

    /// The nearest pitch in the scale (a tie goes down).
    pub fn snap(&self, pitch: u8) -> u8 {
        for d in 0..12i32 {
            for p in [pitch as i32 - d, pitch as i32 + d] {
                if (0..=127).contains(&p) && self.contains(p as u8) {
                    return p as u8;
                }
            }
        }
        pitch
    }

    /// The pitch `steps` scale steps from `pitch` (snapped into the scale first), or None past the MIDI
    /// range.
    pub fn step(&self, pitch: u8, steps: i32) -> Option<u8> {
        let mut p = self.snap(pitch) as i32;
        let dir = steps.signum();
        for _ in 0..steps.unsigned_abs() {
            loop {
                p += dir;
                if !(0..=127).contains(&p) {
                    return None;
                }
                if self.contains(p as u8) {
                    break;
                }
            }
        }
        Some(p as u8)
    }
}

impl FromStr for Scale {
    type Err = KeyError;
    fn from_str(s: &str) -> Result<Scale, KeyError> {
        Scale::parse(s)
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", spell(self.root, self.root_letter), self.mode.name())
    }
}

// ---------------------------------------------------------------------------------------------
// tones over a chord

/// What a pitch class is over a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToneClass {
    /// A chord tone (or the slash bass).
    ChordTone,
    /// In the chord scale and safe to hold over the chord (9ths, 11ths over minor chords, 13ths...).
    Tension,
    /// In the chord scale but a semitone above a chord tone (the 4th over a major chord, b9 over a minor
    /// chord, b6 over a minor triad) or the major third over a sus4: fine passing, rough when held.
    Avoid,
    /// Not in the chord scale: chromatic over this chord in this key.
    Outside,
}

/// The chord scale: the key's scale with the chord's tones replacing the scale tones of the same letter
/// (an E chord in A minor turns G into G#: A harmonic minor's colour). Without a seven-note key, the
/// scale comes from the chord itself: major chords Ionian, dominant and sus-seventh chords Mixolydian,
/// minor-seventh and minor-sixth chords Dorian, minor triads and power chords Aeolian, minor-major
/// sevenths melodic minor, half-diminished and diminished chords Locrian; again with the chord's own
/// tones replacing the scale tones of the same letter.
pub fn chord_scale(chord: &Chord, key: Option<&Scale>) -> PcSet {
    let base = match key {
        Some(k) if k.mode.is_heptatonic() => *k,
        _ => default_scale(chord),
    };
    // pc of each letter in the base scale
    let mut by_letter: [Option<u8>; 7] = [None; 7];
    for (d, pc) in base.pcs().into_iter().enumerate() {
        by_letter[((base.root_letter as usize) + d) % 7] = Some(pc);
    }
    let mut taken = [false; 7];
    let mut set = PcSet::EMPTY;
    for t in &chord.tones {
        taken[t.letter as usize] = true;
        set.insert(t.pc);
    }
    taken[chord.bass_letter as usize] = true;
    set.insert(chord.bass);
    for (letter, pc) in by_letter.iter().enumerate() {
        if let Some(pc) = pc
            && !taken[letter]
        {
            set.insert(*pc);
        }
    }
    set
}

fn default_scale(chord: &Chord) -> Scale {
    let has = |iv: u8, role: Role| chord.tones.iter().any(|t| t.interval == iv && t.role == role);
    let mode = if chord.is_major() {
        if has(10, Role::Seventh) { Mode::Mixolydian } else { Mode::Major }
    } else if chord.is_minor() {
        if has(6, Role::Fifth) {
            Mode::Locrian
        } else if has(11, Role::Seventh) {
            Mode::MelodicMinor
        } else if has(10, Role::Seventh) || chord.tones.iter().any(|t| t.degree == "6") {
            Mode::Dorian
        } else {
            Mode::Minor
        }
    } else if chord.tones.iter().any(|t| t.role == Role::Sus) {
        if has(10, Role::Seventh) { Mode::Mixolydian } else { Mode::Major }
    } else {
        Mode::Minor
    };
    let mode_root_letter = chord.root_letter;
    Scale { root: chord.root, root_letter: mode_root_letter, mode }
}

/// Classifies pitch class `pc` over `chord` in `key` (see [`ToneClass`] and [`chord_scale`]). Over
/// dominant chords the b9 and b13 are available tensions, not avoid notes.
pub fn classify(pc: u8, chord: &Chord, key: Option<&Scale>) -> ToneClass {
    let pc = pc % 12;
    if chord.contains(pc) {
        return ToneClass::ChordTone;
    }
    if !chord_scale(chord, key).contains(pc) {
        return ToneClass::Outside;
    }
    let iv = (pc + 12 - chord.root) % 12;
    let below_is_chord_tone = chord.tone((pc + 11) % 12).is_some();
    let dominant_tension = chord.is_dominant() && matches!(iv, 1 | 8);
    let third_under_sus4 = iv == 4 && chord.tones.iter().any(|t| t.role == Role::Sus && t.interval == 5);
    if (below_is_chord_tone && !dominant_tension) || third_under_sus4 {
        ToneClass::Avoid
    } else {
        ToneClass::Tension
    }
}

/// The degree of `pc` above the chord's root as a chord-symbol label: a chord tone's own label ("b3",
/// "#5", "bb7"), else the tension name ("b9", "9", "#9", "11", "#11", "b13", "13") or, for the remaining
/// intervals, "3", "5", "b7", "7".
pub fn degree_name(pc: u8, chord: &Chord) -> &'static str {
    if let Some(t) = chord.tone(pc) {
        return t.degree;
    }
    match (pc % 12 + 12 - chord.root) % 12 {
        0 => "1",
        1 => "b9",
        2 => "9",
        3 if chord.is_major() => "#9",
        3 => "b3",
        4 => "3",
        5 => "11",
        6 => "#11",
        7 => "5",
        8 => "b13",
        9 => "13",
        10 => "b7",
        _ => "7",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Scale {
        Scale::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn names(s: &Scale) -> String {
        s.pcs().into_iter().map(|pc| s.spell(pc)).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn parses_keys() {
        assert_eq!(key("E minor"), Scale { root: 4, root_letter: 2, mode: Mode::Minor });
        assert_eq!(key("F# dorian").mode, Mode::Dorian);
        assert_eq!(key("F# Phrygian (piano in D major)"), Scale { root: 6, root_letter: 3, mode: Mode::Phrygian });
        assert_eq!(key("G mixolydian").to_string(), "G mixolydian");
        assert_eq!(key("Am").mode, Mode::Minor);
        assert_eq!(key("Bb").to_string(), "Bb major");
        assert_eq!(key("Ebm").to_string(), "Eb minor");
        assert_eq!(key("C harmonic minor").mode, Mode::HarmonicMinor);
        assert_eq!(key("c Harmonic-Minor scale").mode, Mode::HarmonicMinor);
        assert_eq!(key("E flat major").to_string(), "Eb major");
        assert_eq!(key("F-sharp minor").to_string(), "F# minor");
        assert_eq!(key("A aeolian").mode, Mode::Minor);
        assert_eq!(key("D"), Scale::new(2, Mode::Major));
        let e = Scale::parse("E mixolidian").unwrap_err();
        assert!(e.message.contains("unknown mode \"mixolidian\""), "{e}");
        assert!(Scale::parse("H minor").is_err());
        assert!(Scale::parse("").is_err());
    }

    #[test]
    fn spells_scales_by_letter() {
        assert_eq!(names(&key("E minor")), "E F# G A B C D");
        assert_eq!(names(&key("F major")), "F G A Bb C D E");
        assert_eq!(names(&key("D minor")), "D E F G A Bb C");
        assert_eq!(names(&key("F# phrygian")), "F# G A B C# D E");
        assert_eq!(names(&key("C# major")), "C# D# E# F# G# A# B#");
        assert_eq!(names(&key("A harmonic minor")), "A B C D E F G#");
        assert_eq!(names(&Scale::new(10, Mode::Major)), "Bb C D Eb F G A");
        assert_eq!(names(&Scale::new(6, Mode::Minor)), "F# G# A B C# D E");
        assert_eq!(names(&Scale::new(3, Mode::Dorian)), "Eb F Gb Ab Bb C Db");
        assert!(key("D minor").prefers_flats());
        assert!(key("C minor").prefers_flats());
        assert!(!key("E minor").prefers_flats());
        assert!(!key("G mixolydian").prefers_flats());
        assert_eq!(key("F major").note_name(70), "Bb4");
        assert_eq!(key("C# major").note_name(60), "B#3");
        assert_eq!(key("Gb major").note_name(59), "Cb4");
        assert_eq!(key("E minor").note_name(61), "C#4");
        assert_eq!(key("F major").note_name(61), "Db4");
    }

    #[test]
    fn snaps_and_steps() {
        let c = key("C major");
        assert_eq!(c.snap(61), 60);
        assert_eq!(c.snap(66), 65);
        assert_eq!(c.snap(64), 64);
        assert_eq!(c.step(60, 1), Some(62));
        assert_eq!(c.step(60, 7), Some(72));
        assert_eq!(c.step(60, -1), Some(59));
        assert_eq!(c.step(127, 1), None);
        assert_eq!(c.degree_of(7), Some(4));
        assert_eq!(c.degree_of(6), None);
    }

    #[test]
    fn tone_classes_in_a_key() {
        let em = key("E minor");
        let am = Chord::parse("Am").unwrap();
        let b = Chord::parse("B").unwrap();
        let c = Chord::parse("C").unwrap();
        assert_eq!(classify(9, &am, Some(&em)), ToneClass::ChordTone);
        assert_eq!(classify(11, &am, Some(&em)), ToneClass::Tension); // 9
        assert_eq!(classify(2, &am, Some(&em)), ToneClass::Tension); // 11 over minor
        assert_eq!(classify(5, &am, Some(&em)), ToneClass::Outside); // F not in E minor
        assert_eq!(classify(5, &c, Some(&em)), ToneClass::Outside);
        assert_eq!(classify(6, &c, Some(&em)), ToneClass::Tension); // #11 over C in E minor
        assert_eq!(classify(2, &c, Some(&em)), ToneClass::Tension); // 9
        // B major in E minor: D# replaces D
        assert_eq!(classify(3, &b, Some(&em)), ToneClass::ChordTone);
        assert_eq!(classify(2, &b, Some(&em)), ToneClass::Outside);
        assert_eq!(classify(4, &b, Some(&em)), ToneClass::Avoid); // the 4th over a major chord
        assert_eq!(classify(0, &b, Some(&em)), ToneClass::Avoid); // b9 over a triad (not a dominant 7)
        let b7 = Chord::parse("B7").unwrap();
        assert_eq!(classify(0, &b7, Some(&em)), ToneClass::Tension); // b9 over a dominant
        assert_eq!(classify(7, &b7, Some(&em)), ToneClass::Tension); // b13 over a dominant
        let dsus = Chord::parse("Dsus4").unwrap();
        assert_eq!(classify(6, &dsus, Some(&key("G major"))), ToneClass::Avoid); // the 3rd under a sus4
    }

    #[test]
    fn tone_classes_without_a_key() {
        let g7 = Chord::parse("G7").unwrap();
        assert_eq!(chord_scale(&g7, None), [7, 9, 11, 0, 2, 4, 5].into_iter().collect());
        assert_eq!(classify(0, &g7, None), ToneClass::Avoid); // the 11 over a dominant with its third
        assert_eq!(classify(4, &g7, None), ToneClass::Tension); // 13
        let dm7 = Chord::parse("Dm7").unwrap();
        assert_eq!(classify(11, &dm7, None), ToneClass::Tension); // dorian 13
        assert_eq!(classify(3, &dm7, None), ToneClass::Outside);
        let f = Chord::parse("Fmaj7#11").unwrap_or_else(|_| Chord::parse("Fmaj7(#11)").unwrap());
        assert_eq!(classify(11, &f, None), ToneClass::ChordTone);
        assert_eq!(classify(10, &f, None), ToneClass::Outside); // the #11 replaced Bb
        assert_eq!(degree_name(2, &Chord::parse("C").unwrap()), "9");
        assert_eq!(degree_name(3, &Chord::parse("C").unwrap()), "#9");
        assert_eq!(degree_name(8, &Chord::parse("C+").unwrap()), "#5");
    }

    #[test]
    fn pc_sets() {
        let mut s: PcSet = [0, 4, 7].into_iter().collect();
        assert!(s.contains(16));
        assert_eq!(s.len(), 3);
        s.remove(4);
        assert_eq!(s.iter().collect::<Vec<_>>(), [0, 7]);
        assert_eq!(format!("{s:?}"), "{\"C\", \"G\"}");
        assert!(PcSet::EMPTY.is_empty());
        assert_eq!(PcSet::ALL.len(), 12);
    }
}
