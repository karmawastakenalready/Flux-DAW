//! Chord symbols: "Am", "F#m7b5", "C/E", "Dsus4", "G7(b9,#11)", "Bbmaj9", "E7alt", "N.C.".
//!
//! # Grammar
//!
//! ```text
//! symbol  = "N.C." | root quality? number? element* ("/" bass)?
//! root    = A-G (either case) + up to two of # b ♯ ♭        bass = the same
//! quality = maj Maj M ma major Δ ∆ ^   (major; with a number: major seventh)
//!         | m min mi minor -            (minor; may be followed by a major marker: mM7, m(maj7), -Δ7)
//!         | dim ° o                     (diminished; dim7 = diminished seventh)
//!         | ø Ø                         (half-diminished = m7b5)
//!         | aug +                       (augmented)
//! number  = 5 (power chord, alone) | 6 | 6/9 | 69 | 7 | 9 | 11 | 13
//! element = sus | sus2 | sus4 | sus24             (sus replaces the third)
//!         | add9 add2 add11 add4 add13 add6 addb9 add#11 ...
//!         | b5 #5 b9 #9 #11 b13 (also -5 +5 -9 +9 +11 -13, b6 = b13, #4 = #11)
//!         | 9 11 13 (a tension added in parentheses: "C7(9,13)")
//!         | alt (7 with b9 #9 #11 b13, no natural fifth)
//!         | no3 no5 omit3 omit5
//!         | maj7 M7 Δ7 ... (major seventh, for "m(maj7)")
//! ```
//!
//! Parentheses, commas, spaces and slashes inside the quality are separators. A slash followed by a note
//! name is the bass ("C/E"); "6/9" is a quality.
//!
//! Stacked numbers include the tones under them: 9 = 7 + 9, 11 = 7 + 9 + 11, 13 = 7 + 9 + 13 (the 11 is
//! left out of 13 chords, as in most charts and in score.py). Dominant and major 11 chords drop the major
//! third (it clashes with the 11, which then stands in for it like a sus4); minor 11 chords keep it.
//! Alterations replace the natural tension they alter (13b9 has no natural 9).

use crate::error::ChordError;
use crate::theory::note::{letter_index, pc_name, scan_letter, spell};
use serde::Serialize;
use std::fmt;
use std::str::FromStr;

/// What a chord tone does in the chord. Voicing uses it to decide what to keep in thin voicings and what
/// to double.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Root,
    Third,
    /// A suspended 4th or 2nd standing in for the third (also the 11 of a dominant 11 chord).
    Sus,
    Fifth,
    Seventh,
    /// 6ths, 9ths, 11ths, 13ths (natural or altered) and added tones.
    Extension,
}

/// One tone of a chord.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChordTone {
    /// Semitones above the root (0-11).
    pub interval: u8,
    /// Pitch class (0-11, C = 0).
    pub pc: u8,
    pub role: Role,
    /// Chord-degree label: "1", "b3", "3", "4", "2", "5", "b5", "#5", "6", "bb7", "b7", "7", "b9", "9",
    /// "#9", "11", "#11", "b13", "13".
    pub degree: &'static str,
    /// Letter index of the tone's spelling (0 = C ... 6 = B): the root letter plus the degree, so the
    /// third of E is spelled G# and the seventh of Bdim7 is Ab.
    pub letter: u8,
}

impl ChordTone {
    /// The tone's name spelled from its degree ("G#", "Ab", "Bbb").
    pub fn name(&self) -> String {
        spell(self.pc, self.letter)
    }
}

/// A parsed chord symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    /// The symbol as written (trimmed).
    pub symbol: String,
    /// Root pitch class.
    pub root: u8,
    /// Letter index of the root as written (0 = C ... 6 = B).
    pub root_letter: u8,
    /// Bass pitch class: the slash bass, else the root.
    pub bass: u8,
    /// Letter index of the bass as written.
    pub bass_letter: u8,
    /// Chord tones, root first, then third/sus, fifth, seventh, sixth, extensions by degree. Each pitch
    /// class appears once. The slash bass is not included unless it is also a chord tone.
    pub tones: Vec<ChordTone>,
}

/// A chord-track entry: a chord or "no chord".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChordSymbol {
    /// "N.C.": no harmony (silence or an unharmonised line).
    NoChord,
    Chord(Chord),
}

impl ChordSymbol {
    /// Parses a chord symbol, accepting "N.C." (also "NC", "N.C", "n.c.", "N/C") for no chord.
    pub fn parse(text: &str) -> Result<ChordSymbol, ChordError> {
        let compact: String = text.chars().filter(|c| !matches!(c, '.' | ' ')).collect::<String>().to_ascii_uppercase();
        if compact == "NC" || compact == "N/C" {
            return Ok(ChordSymbol::NoChord);
        }
        parse_chord(text).map(ChordSymbol::Chord)
    }

    pub fn chord(&self) -> Option<&Chord> {
        match self {
            ChordSymbol::Chord(c) => Some(c),
            ChordSymbol::NoChord => None,
        }
    }
}

impl fmt::Display for ChordSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChordSymbol::NoChord => f.write_str("N.C."),
            ChordSymbol::Chord(c) => f.write_str(&c.symbol),
        }
    }
}

impl FromStr for Chord {
    type Err = ChordError;
    fn from_str(s: &str) -> Result<Chord, ChordError> {
        parse_chord(s)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.symbol)
    }
}

impl Chord {
    /// Parses a chord symbol ("N.C." is an error here: use [`ChordSymbol::parse`] for chord tracks).
    pub fn parse(text: &str) -> Result<Chord, ChordError> {
        parse_chord(text)
    }

    /// A chord from pitch classes, the first one being the root (score.py's `chord([7, 11, 2])`). Roles are
    /// inferred from the intervals: a 3 or 4 is the third, a 7 (else a b5 over a minor third, a #5 over a
    /// major third) the fifth, a 10 or 11 (or the bb7 of a diminished triad) the seventh, a 5 or 2 without a
    /// third the sus; everything else is an extension. The symbol is the tone list in braces, "{G B D}".
    pub fn from_pitch_classes(pcs: &[u8]) -> Result<Chord, ChordError> {
        let Some(&first) = pcs.first() else {
            return Err(ChordError { symbol: "{}".into(), position: 0, message: "a chord needs at least one pitch class".into() });
        };
        let root = first % 12;
        let mut ivs: Vec<u8> = pcs.iter().map(|&p| (p % 12 + 12 - root) % 12).collect();
        ivs.sort_unstable();
        ivs.dedup();
        let has = |i: u8| ivs.contains(&i);
        let third = if has(4) {
            Some(4)
        } else if has(3) {
            Some(3)
        } else {
            None
        };
        let fifth = if has(7) {
            Some(7)
        } else if third == Some(3) && has(6) {
            Some(6)
        } else if third == Some(4) && has(8) {
            Some(8)
        } else {
            None
        };
        let seventh = if has(10) {
            Some(10)
        } else if has(11) {
            Some(11)
        } else if third == Some(3) && fifth == Some(6) && has(9) {
            Some(9)
        } else {
            None
        };
        let sus = match third {
            Some(_) => None,
            None if has(5) => Some(5),
            None if has(2) => Some(2),
            None => None,
        };
        let root_letter = letter_of_pc(root);
        let tones = ivs
            .iter()
            .map(|&i| {
                let role = if i == 0 {
                    Role::Root
                } else if Some(i) == third {
                    Role::Third
                } else if Some(i) == fifth {
                    Role::Fifth
                } else if Some(i) == seventh {
                    Role::Seventh
                } else if Some(i) == sus {
                    Role::Sus
                } else {
                    Role::Extension
                };
                let degree = inferred_degree(i, role);
                ChordTone { interval: i, pc: (root + i) % 12, role, degree, letter: (root_letter + letter_offset(degree)) % 7 }
            })
            .collect::<Vec<_>>();
        let names: Vec<&str> = pcs.iter().map(|&p| pc_name(p, false)).collect();
        Ok(Chord { symbol: format!("{{{}}}", names.join(" ")), root, root_letter, bass: root, bass_letter: root_letter, tones })
    }

    /// Pitch classes of the chord tones, in tone order.
    pub fn pcs(&self) -> impl Iterator<Item = u8> + '_ {
        self.tones.iter().map(|t| t.pc)
    }

    /// The tone with this pitch class, if it is a chord tone.
    pub fn tone(&self, pc: u8) -> Option<&ChordTone> {
        self.tones.iter().find(|t| t.pc == pc % 12)
    }

    pub fn role_of(&self, pc: u8) -> Option<Role> {
        self.tone(pc).map(|t| t.role)
    }

    /// Whether `pc` is a chord tone or the slash bass.
    pub fn contains(&self, pc: u8) -> bool {
        self.tone(pc).is_some() || self.bass == pc % 12
    }

    /// Pitch class of the third, if the chord has one.
    pub fn third(&self) -> Option<u8> {
        self.tones.iter().find(|t| t.role == Role::Third).map(|t| t.pc)
    }

    /// A chord with a major third.
    pub fn is_major(&self) -> bool {
        self.tones.iter().any(|t| t.role == Role::Third && t.interval == 4)
    }

    /// A chord with a minor third.
    pub fn is_minor(&self) -> bool {
        self.tones.iter().any(|t| t.role == Role::Third && t.interval == 3)
    }

    /// A dominant-function seventh chord: a minor seventh over a major third or a sus.
    pub fn is_dominant(&self) -> bool {
        let minor_seventh = self.tones.iter().any(|t| t.role == Role::Seventh && t.interval == 10);
        let upper = self.is_major() || self.tones.iter().any(|t| t.role == Role::Sus);
        minor_seventh && upper
    }

    /// Whether the root or slash bass was written with a flat (pitch names then prefer flats).
    pub fn prefers_flats(&self) -> bool {
        let flat = |pc: u8, letter: u8| spell(pc, letter).contains('b');
        flat(self.root, self.root_letter) || flat(self.bass, self.bass_letter)
    }

    /// The slash bass spelled as written ("E" in C/E), or the root.
    pub fn bass_name(&self) -> String {
        spell(self.bass, self.bass_letter)
    }

    /// Chord tones spelled from their degrees, root first ("C E G Bb"). A slash bass that is not a chord
    /// tone comes first ("G F A C" for F/G).
    pub fn tone_names(&self) -> String {
        let mut names: Vec<String> = self.tones.iter().map(ChordTone::name).collect();
        if self.tone(self.bass).is_none() {
            names.insert(0, self.bass_name());
        }
        names.join(" ")
    }
}

/// Letter offset of a degree label: "1" 0, "b9"/"9"/"2" 1, "b3"/"3" 2, "4"/"11"/"#11" 3, "5"/"b5"/"#5" 4,
/// "6"/"13"/"b13" 5, "7"/"b7"/"bb7" 6.
fn letter_offset(degree: &str) -> u8 {
    match degree.trim_start_matches(['b', '#']) {
        "2" | "9" => 1,
        "3" => 2,
        "4" | "11" => 3,
        "5" => 4,
        "6" | "13" => 5,
        "7" => 6,
        _ => 0,
    }
}

/// The letter of a pitch class's sharp spelling.
fn letter_of_pc(pc: u8) -> u8 {
    [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6][(pc % 12) as usize]
}

fn inferred_degree(interval: u8, role: Role) -> &'static str {
    match (interval, role) {
        (0, _) => "1",
        (1, _) => "b9",
        (2, Role::Sus) => "2",
        (2, _) => "9",
        (3, Role::Third) => "b3",
        (3, _) => "#9",
        (4, _) => "3",
        (5, Role::Sus) => "4",
        (5, _) => "11",
        (6, Role::Fifth) => "b5",
        (6, _) => "#11",
        (7, _) => "5",
        (8, Role::Fifth) => "#5",
        (8, _) => "b13",
        (9, Role::Seventh) => "bb7",
        (9, _) => "6",
        (10, _) => "b7",
        _ => "7",
    }
}

// ---------------------------------------------------------------------------------------------
// parsing

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Triad {
    Major,
    Minor,
    Diminished,
    HalfDiminished,
    Augmented,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seventh {
    Minor,
    Major,
    Diminished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sus {
    Two,
    Four,
    Both,
}

/// An altered or added degree: (accidental -1/0/+1, degree 2..13).
type Degree = (i8, u8);

#[derive(Debug, Default)]
struct Spec {
    triad: Option<Triad>,
    seventh: Option<Seventh>,
    /// Highest stacked number: 7, 9, 11 or 13.
    stack: Option<u8>,
    sixth: bool,
    sus: Option<Sus>,
    adds: Vec<Degree>,
    alters: Vec<Degree>,
    omit3: bool,
    omit5: bool,
    power: bool,
    alt: bool,
}

struct Scan<'a> {
    chars: &'a [char],
    pos: usize,
    /// Character offset of `chars[0]` in the whole symbol (for error positions).
    base: usize,
    symbol: &'a str,
}

impl Scan<'_> {
    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.chars.get(self.pos + k).copied()
    }

    fn looking_at(&self, lit: &str) -> bool {
        lit.chars().enumerate().all(|(k, c)| self.chars.get(self.pos + k) == Some(&c))
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.looking_at(lit) {
            self.pos += lit.chars().count();
            true
        } else {
            false
        }
    }

    fn eat_any(&mut self, lits: &[&str]) -> Option<usize> {
        let i = lits.iter().position(|l| self.looking_at(l))?;
        self.pos += lits[i].chars().count();
        Some(i)
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        self.chars[start..self.pos].iter().collect::<String>().parse().ok()
    }

    fn err(&self, at: usize, message: impl Into<String>) -> ChordError {
        ChordError { symbol: self.symbol.to_string(), position: self.base + at, message: message.into() }
    }

    fn rest_text(&self) -> String {
        self.chars[self.pos..].iter().collect()
    }
}

fn parse_chord(text: &str) -> Result<Chord, ChordError> {
    let symbol = text.trim();
    let chars: Vec<char> = symbol.chars().collect();
    let err = |at: usize, m: &str| ChordError { symbol: symbol.to_string(), position: at, message: m.to_string() };
    if chars.is_empty() {
        return Err(err(0, "empty chord symbol"));
    }
    let (root_v, n) = scan_letter(&chars)
        .ok_or_else(|| err(0, "a chord symbol starts with its root, a note letter A-G (e.g. Am, F#m7b5, C/E, Dsus4)"))?;
    let root = root_v.rem_euclid(12) as u8;
    let root_letter = letter_index(chars[0]).unwrap_or(0);

    // the slash bass: the last '/' followed by exactly a note name
    let mut end = chars.len();
    let mut bass = root;
    let mut bass_letter = root_letter;
    if let Some(j) = chars.iter().rposition(|&c| c == '/')
        && j >= n
    {
        let after = &chars[j + 1..];
        match scan_letter(after) {
            Some((v, k)) if k == after.len() => {
                bass = v.rem_euclid(12) as u8;
                bass_letter = letter_index(after[0]).unwrap_or(0);
                end = j;
            }
            Some(_) if after.first().is_some_and(|c| c.is_ascii_uppercase()) => {
                return Err(err(j + 1, "the bass after '/' must be a note name only (e.g. C/E, Bb/D)"));
            }
            _ if after.is_empty() => return Err(err(j, "missing bass note after '/'")),
            _ => {}
        }
    }

    let mut sc = Scan { chars: &chars[n..end], pos: 0, base: n, symbol };
    let spec = parse_quality(&mut sc)?;
    let tones = build_tones(&spec);
    Ok(Chord {
        symbol: symbol.to_string(),
        root,
        root_letter,
        bass,
        bass_letter,
        tones: tones
            .into_iter()
            .map(|(interval, role, degree)| ChordTone {
                interval,
                pc: (root + interval) % 12,
                role,
                degree,
                letter: (root_letter + letter_offset(degree)) % 7,
            })
            .collect(),
    })
}

fn parse_quality(sc: &mut Scan) -> Result<Spec, ChordError> {
    let mut spec = Spec::default();
    if sc.chars == ['5'] {
        spec.power = true;
        return Ok(spec);
    }

    // quality head
    let mut maj_marker = false;
    let mut delta = false;
    let head_at = sc.pos;
    if let Some(i) = sc.eat_any(&["major", "Major", "maj", "Maj", "MAJ", "Δ", "∆", "^"]) {
        spec.triad = Some(Triad::Major);
        maj_marker = true;
        delta = i >= 5;
    } else if sc.looking_at("ma") && sc.peek_at(2).is_none_or(|c| c.is_ascii_digit() || c == '(') {
        sc.pos += 2;
        spec.triad = Some(Triad::Major);
        maj_marker = true;
    } else if sc.eat("M") {
        spec.triad = Some(Triad::Major);
        maj_marker = true;
    } else if sc.eat_any(&["minor", "min", "mi", "m", "-"]).is_some() {
        spec.triad = Some(Triad::Minor);
    } else if sc.eat_any(&["dim", "°"]).is_some() {
        spec.triad = Some(Triad::Diminished);
    } else if sc.looking_at("o") && sc.peek_at(1).is_none_or(|c| c.is_ascii_digit() || c == '(') {
        sc.pos += 1;
        spec.triad = Some(Triad::Diminished);
    } else if sc.eat_any(&["ø", "Ø"]).is_some() {
        spec.triad = Some(Triad::HalfDiminished);
    } else if sc.eat_any(&["aug", "+"]).is_some() {
        spec.triad = Some(Triad::Augmented);
    }
    // a major-seventh marker after minor / diminished / augmented: mM7, m(maj7) is handled as an element
    if matches!(spec.triad, Some(Triad::Minor | Triad::Diminished | Triad::Augmented))
        && let Some(i) = sc.eat_any(&["maj", "Maj", "Δ", "∆", "^", "M"])
    {
        maj_marker = true;
        delta = (2..=4).contains(&i);
    }

    // the number
    let num_at = sc.pos;
    let number = if sc.eat("6/9") || sc.eat("69") {
        Some(69)
    } else if sc.peek().is_some_and(|c| c.is_ascii_digit()) {
        sc.number()
    } else {
        None
    };
    match number {
        None => {
            if maj_marker && (delta || spec.triad != Some(Triad::Major)) {
                spec.seventh = Some(Seventh::Major);
                spec.stack = Some(7);
            }
        }
        Some(69) => {
            spec.sixth = true;
            spec.adds.push((0, 9));
        }
        Some(6) => spec.sixth = true,
        Some(n @ (7 | 9 | 11 | 13)) => {
            spec.stack = Some(n as u8);
            spec.seventh = Some(if maj_marker {
                Seventh::Major
            } else if spec.triad == Some(Triad::Diminished) {
                Seventh::Diminished
            } else {
                Seventh::Minor
            });
        }
        Some(5) => return Err(sc.err(num_at, "a power chord is just the number 5 after the root (e.g. E5)")),
        Some(2) => return Err(sc.err(num_at, "ambiguous '2': write add9 (or add2) for an added second, sus2 to replace the third")),
        Some(4) => return Err(sc.err(num_at, "ambiguous '4': write sus4 to replace the third, add11 (or add4) to add a fourth")),
        Some(n) => return Err(sc.err(num_at, format!("unsupported chord number {n} (use 5, 6, 6/9, 7, 9, 11 or 13)"))),
    }
    if spec.triad == Some(Triad::HalfDiminished) && spec.seventh.is_none() {
        spec.seventh = Some(Seventh::Minor);
        spec.stack = Some(7);
    }
    if spec.triad == Some(Triad::HalfDiminished) && spec.seventh == Some(Seventh::Major) {
        return Err(sc.err(head_at, "a half-diminished chord has a minor seventh (use dim(maj7) for a major one)"));
    }

    // elements
    while !sc.at_end() {
        let at = sc.pos;
        if sc.eat_any(&["(", ")", ",", " ", "/"]).is_some() {
            continue;
        }
        if sc.eat("sus") {
            if spec.sus.is_some() {
                return Err(sc.err(at, "sus appears twice"));
            }
            spec.sus = Some(if sc.eat("24") || sc.eat("42") || sc.eat("2sus4") {
                Sus::Both
            } else if sc.eat("2") {
                Sus::Two
            } else {
                sc.eat("4");
                Sus::Four
            });
            continue;
        }
        if sc.eat("add") {
            let acc = accidental(sc);
            let deg_at = sc.pos;
            let d = sc.number().ok_or_else(|| sc.err(deg_at, "add needs a degree, e.g. add9, add11, add13"))?;
            let d = match d {
                2 | 4 | 6 | 9 | 11 | 13 => d as u8,
                _ => return Err(sc.err(deg_at, format!("can't add a {d} (use add9, add11, add13, add2, add4 or add6)"))),
            };
            spec.adds.push((acc.unwrap_or(0), d));
            continue;
        }
        if sc.eat("alt") {
            spec.alt = true;
            continue;
        }
        if sc.eat("no") || sc.eat("omit") {
            let d_at = sc.pos;
            match sc.number() {
                Some(3) => spec.omit3 = true,
                Some(5) => spec.omit5 = true,
                _ => return Err(sc.err(d_at, "only the 3rd or the 5th can be omitted (no3, no5)")),
            }
            continue;
        }
        if sc.eat_any(&["maj", "Maj", "Δ", "∆", "^", "M"]).is_some() {
            let n = sc.number();
            if spec.seventh.is_some_and(|s| s != Seventh::Major) {
                return Err(sc.err(at, "the seventh is given twice"));
            }
            match n {
                None | Some(7) => {}
                Some(k @ (9 | 11 | 13)) => spec.stack = Some(k as u8),
                Some(k) => return Err(sc.err(at, format!("unsupported major-seventh extension {k}"))),
            }
            spec.seventh = Some(Seventh::Major);
            spec.stack = Some(spec.stack.unwrap_or(7).max(7));
            continue;
        }
        if let Some(acc) = accidental(sc) {
            let d_at = sc.pos;
            match sc.number() {
                Some(d) => {
                    let deg: Degree = match (acc, d) {
                        (_, 5 | 9) | (1, 11) | (-1, 13) => (acc, d as u8),
                        (-1, 6) => (-1, 13),
                        (1, 4) => (1, 11),
                        _ => {
                            let written: String = sc.chars[at..sc.pos].iter().collect();
                            return Err(sc.err(at, format!("unsupported alteration {written} (use b5 #5 b9 #9 #11 b13)")));
                        }
                    };
                    if spec.alters.contains(&deg) {
                        return Err(sc.err(at, "alteration appears twice"));
                    }
                    spec.alters.push(deg);
                }
                None if acc > 0 && sc.chars[at] == '+' => spec.alters.push((1, 5)),
                None => return Err(sc.err(d_at, "an alteration needs a degree, e.g. b9 or #11")),
            }
            continue;
        }
        if sc.peek().is_some_and(|c| c.is_ascii_digit()) {
            match sc.number() {
                Some(d @ (9 | 11 | 13)) => spec.adds.push((0, d as u8)),
                Some(6) => spec.sixth = true,
                Some(7) if spec.seventh.is_none() => {
                    spec.seventh = Some(Seventh::Minor);
                    spec.stack = Some(7);
                }
                _ => return Err(sc.err(at, format!("unexpected number in {:?}", sc.rest_text()))),
            }
            continue;
        }
        let word: String = sc.chars[at..].iter().take_while(|c| !matches!(c, '(' | ')' | ',' | ' ' | '/')).collect();
        return Err(sc.err(at, format!("unknown chord quality {word:?} (see the chord grammar: m, maj7, m7b5, dim7, sus4, add9, 7b9, 6/9, ...)")));
    }

    if spec.sus.is_some() && matches!(spec.triad, Some(Triad::Minor | Triad::Diminished | Triad::HalfDiminished | Triad::Augmented)) {
        return Err(sc.err(0, "sus replaces the third, so it can't be combined with m, dim, ø or aug"));
    }
    if spec.alt {
        if spec.triad.is_some_and(|t| t != Triad::Major) {
            return Err(sc.err(0, "alt is a dominant chord (write e.g. G7alt)"));
        }
        if spec.seventh.is_none() {
            spec.seventh = Some(Seventh::Minor);
            spec.stack = Some(7);
        }
    }
    Ok(spec)
}

fn accidental(sc: &mut Scan) -> Option<i8> {
    match sc.peek()? {
        'b' | '♭' | '-' => {
            sc.pos += 1;
            Some(-1)
        }
        '#' | '♯' | '+' => {
            sc.pos += 1;
            Some(1)
        }
        _ => None,
    }
}

/// Sort key of an extension among the tensions: 9ths, then 11ths, then 13ths (adds of 2/4/6 sort with
/// 9/11/13), the flat before the natural before the sharp.
fn ext_key(acc: i8, degree: u8) -> (u8, i8) {
    let d = match degree {
        2 => 9,
        4 => 11,
        6 => 13,
        d => d,
    };
    (d, acc)
}

fn ext_tone(acc: i8, degree: u8) -> (u8, &'static str) {
    match (acc, degree) {
        (0, 2) => (2, "2"),
        (0, 4) => (5, "4"),
        (0, 6) => (9, "6"),
        (-1, 9) => (1, "b9"),
        (0, 9) => (2, "9"),
        (1, 9) => (3, "#9"),
        (0, 11) => (5, "11"),
        (1, 11) => (6, "#11"),
        (-1, 11) => (4, "b11"),
        (-1, 13) => (8, "b13"),
        (0, 13) => (9, "13"),
        (1, 13) => (10, "#13"),
        (-1, 2) => (1, "b9"),
        (1, 2) => (3, "#9"),
        (1, 4) => (6, "#11"),
        (-1, 4) => (4, "b11"),
        (-1, 6) => (8, "b13"),
        (1, 6) => (10, "#13"),
        _ => (2, "9"),
    }
}

fn build_tones(spec: &Spec) -> Vec<(u8, Role, &'static str)> {
    let mut tones: Vec<(u8, Role, &'static str)> = vec![(0, Role::Root, "1")];
    if spec.power {
        tones.push((7, Role::Fifth, "5"));
        return tones;
    }
    let triad = spec.triad.unwrap_or(Triad::Major);
    let has = |d: Degree| spec.alters.contains(&d);
    let natural_11_in_stack = spec.stack == Some(11) && !has((1, 11));

    // third or sus
    let mut third = match triad {
        Triad::Major | Triad::Augmented => Some(4),
        _ => Some(3),
    };
    if spec.sus.is_some() || spec.omit3 {
        third = None;
    }
    let eleven_is_sus = natural_11_in_stack && third == Some(4) && !spec.alt;
    if eleven_is_sus {
        third = None;
    }
    match third {
        Some(4) => tones.push((4, Role::Third, "3")),
        Some(_) => tones.push((3, Role::Third, "b3")),
        None => {}
    }
    match spec.sus {
        Some(Sus::Four) => tones.push((5, Role::Sus, "4")),
        Some(Sus::Two) => tones.push((2, Role::Sus, "2")),
        Some(Sus::Both) => tones.push((5, Role::Sus, "4")),
        None => {}
    }

    // fifth
    let mut extra_fifth = None;
    if !spec.alt && !spec.omit5 {
        let (flat, sharp) = (has((-1, 5)), has((1, 5)));
        let fifth = if flat {
            if sharp {
                extra_fifth = Some((8, Role::Extension, "#5"));
            }
            (6, "b5")
        } else if sharp {
            (8, "#5")
        } else {
            match triad {
                Triad::Diminished | Triad::HalfDiminished => (6, "b5"),
                Triad::Augmented => (8, "#5"),
                _ => (7, "5"),
            }
        };
        tones.push((fifth.0, Role::Fifth, fifth.1));
    }

    // seventh, sixth
    match spec.seventh {
        Some(Seventh::Minor) => tones.push((10, Role::Seventh, "b7")),
        Some(Seventh::Major) => tones.push((11, Role::Seventh, "7")),
        Some(Seventh::Diminished) => tones.push((9, Role::Seventh, "bb7")),
        None => {}
    }
    if spec.sixth {
        tones.push((9, Role::Extension, "6"));
    }

    // extensions: stacked naturals (unless altered), alterations, adds, alt
    let mut ext: Vec<(i8, u8)> = vec![];
    if let Some(top) = spec.stack {
        if top >= 9 && !has((-1, 9)) && !has((1, 9)) {
            ext.push((0, 9));
        }
        if top == 11 && !has((1, 11)) {
            ext.push((0, 11));
        }
        if top == 13 && !has((-1, 13)) {
            ext.push((0, 13));
        }
    }
    if spec.sus == Some(Sus::Both) {
        ext.push((0, 2));
    }
    ext.extend(spec.alters.iter().copied().filter(|&(_, d)| d != 5));
    ext.extend(spec.adds.iter().copied());
    if spec.alt {
        ext.extend([(-1, 9), (1, 9), (1, 11), (-1, 13)]);
    }
    ext.sort_by_key(|&(a, d)| ext_key(a, d));
    let mut ext_tones: Vec<(u8, Role, &'static str)> = ext
        .into_iter()
        .map(|(a, d)| {
            let (iv, label) = ext_tone(a, d);
            let role = if eleven_is_sus && (a, d) == (0, 11) { Role::Sus } else { Role::Extension };
            (iv, role, label)
        })
        .collect();
    if let Some(t) = extra_fifth {
        ext_tones.push(t);
    }
    for t in ext_tones {
        tones.push(t);
    }

    // each pitch class once, the first (most important) role wins
    let mut seen = [false; 12];
    tones.retain(|t| !std::mem::replace(&mut seen[t.0 as usize], true));
    tones
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivs(s: &str) -> Vec<u8> {
        Chord::parse(s).unwrap_or_else(|e| panic!("{e}")).tones.iter().map(|t| t.interval).collect()
    }

    fn roles(s: &str) -> Vec<(u8, Role)> {
        Chord::parse(s).unwrap().tones.iter().map(|t| (t.interval, t.role)).collect()
    }

    #[test]
    fn triads_and_sevenths() {
        assert_eq!(ivs("C"), [0, 4, 7]);
        assert_eq!(ivs("Am"), [0, 3, 7]);
        assert_eq!(ivs("Bdim"), [0, 3, 6]);
        assert_eq!(ivs("Caug"), [0, 4, 8]);
        assert_eq!(ivs("C+"), [0, 4, 8]);
        assert_eq!(ivs("E5"), [0, 7]);
        assert_eq!(ivs("G7"), [0, 4, 7, 10]);
        assert_eq!(ivs("Cmaj7"), [0, 4, 7, 11]);
        assert_eq!(ivs("CM7"), [0, 4, 7, 11]);
        assert_eq!(ivs("CΔ"), [0, 4, 7, 11]);
        assert_eq!(ivs("C^7"), [0, 4, 7, 11]);
        assert_eq!(ivs("Cma7"), [0, 4, 7, 11]);
        assert_eq!(ivs("Cmaj"), [0, 4, 7]);
        assert_eq!(ivs("CM"), [0, 4, 7]);
        assert_eq!(ivs("Dm7"), [0, 3, 7, 10]);
        assert_eq!(ivs("D-7"), [0, 3, 7, 10]);
        assert_eq!(ivs("Dmin7"), [0, 3, 7, 10]);
        assert_eq!(ivs("Cm(maj7)"), [0, 3, 7, 11]);
        assert_eq!(ivs("CmM7"), [0, 3, 7, 11]);
        assert_eq!(ivs("Cmmaj7"), [0, 3, 7, 11]);
        assert_eq!(ivs("C-Δ7"), [0, 3, 7, 11]);
        assert_eq!(ivs("F#m7b5"), [0, 3, 6, 10]);
        assert_eq!(ivs("F#ø"), [0, 3, 6, 10]);
        assert_eq!(ivs("F#ø7"), [0, 3, 6, 10]);
        assert_eq!(ivs("Bdim7"), [0, 3, 6, 9]);
        assert_eq!(ivs("B°7"), [0, 3, 6, 9]);
        assert_eq!(ivs("Bo7"), [0, 3, 6, 9]);
        assert_eq!(ivs("C+7"), [0, 4, 8, 10]);
        assert_eq!(ivs("C7#5"), [0, 4, 8, 10]);
        assert_eq!(ivs("C7+"), [0, 4, 8, 10]);
        assert_eq!(ivs("Caugmaj7"), [0, 4, 8, 11]);
    }

    #[test]
    fn sixths_sus_add() {
        assert_eq!(ivs("C6"), [0, 4, 7, 9]);
        assert_eq!(ivs("Cm6"), [0, 3, 7, 9]);
        assert_eq!(ivs("C6/9"), [0, 4, 7, 9, 2]);
        assert_eq!(ivs("C69"), [0, 4, 7, 9, 2]);
        assert_eq!(ivs("Cm6/9"), [0, 3, 7, 9, 2]);
        assert_eq!(ivs("Csus"), [0, 5, 7]);
        assert_eq!(ivs("Csus4"), [0, 5, 7]);
        assert_eq!(ivs("Csus2"), [0, 2, 7]);
        assert_eq!(ivs("C7sus4"), [0, 5, 7, 10]);
        assert_eq!(ivs("C9sus4"), [0, 5, 7, 10, 2]);
        assert_eq!(ivs("Cadd9"), [0, 4, 7, 2]);
        assert_eq!(ivs("Cadd2"), [0, 4, 7, 2]);
        assert_eq!(ivs("Cmadd9"), [0, 3, 7, 2]);
        assert_eq!(ivs("C(add9)"), [0, 4, 7, 2]);
        assert_eq!(ivs("Cadd11"), [0, 4, 7, 5]);
        assert_eq!(ivs("Csus24"), [0, 5, 7, 2]);
    }

    #[test]
    fn extensions_and_alterations() {
        assert_eq!(ivs("C9"), [0, 4, 7, 10, 2]);
        assert_eq!(ivs("Cmaj9"), [0, 4, 7, 11, 2]);
        assert_eq!(ivs("Cm9"), [0, 3, 7, 10, 2]);
        assert_eq!(ivs("C11"), [0, 7, 10, 2, 5]);
        assert_eq!(ivs("Cm11"), [0, 3, 7, 10, 2, 5]);
        assert_eq!(ivs("C13"), [0, 4, 7, 10, 2, 9]);
        assert_eq!(ivs("Cmaj13"), [0, 4, 7, 11, 2, 9]);
        assert_eq!(ivs("Cm13"), [0, 3, 7, 10, 2, 9]);
        assert_eq!(ivs("C7b9"), [0, 4, 7, 10, 1]);
        assert_eq!(ivs("C7#9"), [0, 4, 7, 10, 3]);
        assert_eq!(ivs("C7(b9,#11)"), [0, 4, 7, 10, 1, 6]);
        assert_eq!(ivs("C7(b9 #11)"), [0, 4, 7, 10, 1, 6]);
        assert_eq!(ivs("C13b9"), [0, 4, 7, 10, 1, 9]);
        assert_eq!(ivs("C9#11"), [0, 4, 7, 10, 2, 6]);
        assert_eq!(ivs("C7b13"), [0, 4, 7, 10, 8]);
        assert_eq!(ivs("C7b5"), [0, 4, 6, 10]);
        assert_eq!(ivs("C7alt"), [0, 4, 10, 1, 3, 6, 8]);
        assert_eq!(ivs("C7(9,13)"), [0, 4, 7, 10, 2, 9]);
        assert_eq!(ivs("C7-9"), [0, 4, 7, 10, 1]);
        assert_eq!(ivs("C(no3)"), [0, 7]);
        assert_eq!(ivs("C7no5"), [0, 4, 10]);
    }

    #[test]
    fn roles_follow_function() {
        assert_eq!(roles("C11"), [(0, Role::Root), (7, Role::Fifth), (10, Role::Seventh), (2, Role::Extension), (5, Role::Sus)]);
        assert_eq!(roles("Cm11")[5], (5, Role::Extension));
        assert_eq!(roles("Bdim7")[3], (9, Role::Seventh));
        assert_eq!(roles("C6")[3], (9, Role::Extension));
        assert_eq!(roles("C7b5")[2], (6, Role::Fifth));
        assert_eq!(roles("Csus2")[1], (2, Role::Sus));
    }

    #[test]
    fn roots_basses_and_spelling() {
        let c = Chord::parse("C/E").unwrap();
        assert_eq!((c.root, c.bass), (0, 4));
        let c = Chord::parse("F/G").unwrap();
        assert_eq!((c.root, c.bass), (5, 7));
        assert!(c.contains(7) && c.role_of(7).is_none());
        let c = Chord::parse("C#m7b5/E").unwrap();
        assert_eq!((c.root, c.bass), (1, 4));
        let c = Chord::parse("C6/9/E").unwrap();
        assert_eq!(c.bass, 4);
        assert_eq!(c.tones.len(), 5);
        let c = Chord::parse("Bb/D").unwrap();
        assert_eq!((c.root, c.bass), (10, 2));
        assert!(c.prefers_flats());
        assert_eq!(c.tone_names(), "Bb D F");
        assert_eq!(Chord::parse("E").unwrap().tone_names(), "E G# B");
        assert_eq!(Chord::parse("Bdim7").unwrap().tone_names(), "B D F Ab");
        assert_eq!(Chord::parse("C7#9").unwrap().tone_names(), "C E G Bb D#");
        assert_eq!(Chord::parse("F/G").unwrap().tone_names(), "G F A C");
        assert_eq!(Chord::parse("Gb/Bb").unwrap().tone_names(), "Gb Bb Db");
        assert!(!Chord::parse("C/E").unwrap().prefers_flats());
        assert_eq!(Chord::parse("Cm/maj7").unwrap().tones.len(), 4);
        assert_eq!(Chord::parse(" am ").unwrap().root, 9);
        assert!(Chord::parse("Am").unwrap().is_minor());
        assert!(Chord::parse("A").unwrap().is_major());
    }

    #[test]
    fn from_pitch_classes_infers_roles_like_score_py() {
        let c = Chord::from_pitch_classes(&[7, 11, 2]).unwrap();
        assert_eq!(c.symbol, "{G B D}");
        assert_eq!(c.tones.iter().map(|t| (t.interval, t.role)).collect::<Vec<_>>(), [(0, Role::Root), (4, Role::Third), (7, Role::Fifth)]);
        let c = Chord::from_pitch_classes(&[11, 2, 5, 8]).unwrap();
        assert_eq!(c.role_of(8), Some(Role::Seventh));
        let c = Chord::from_pitch_classes(&[2, 7, 9]).unwrap();
        assert_eq!(c.role_of(7), Some(Role::Sus));
        assert!(Chord::from_pitch_classes(&[]).is_err());
    }

    #[test]
    fn dominants() {
        assert!(Chord::parse("G7").unwrap().is_dominant());
        assert!(Chord::parse("G7sus4").unwrap().is_dominant());
        assert!(Chord::parse("G13b9").unwrap().is_dominant());
        assert!(!Chord::parse("Gm7").unwrap().is_dominant());
        assert!(!Chord::parse("Gmaj7").unwrap().is_dominant());
    }

    #[test]
    fn no_chord() {
        for s in ["N.C.", "NC", "N.C", "n.c.", "N/C"] {
            assert_eq!(ChordSymbol::parse(s), Ok(ChordSymbol::NoChord), "{s}");
        }
        assert!(Chord::parse("N.C.").is_err());
    }

    #[test]
    fn errors_point_at_the_problem() {
        let e = Chord::parse("Cxyz").unwrap_err();
        assert_eq!(e.position, 1);
        assert!(e.message.contains("unknown chord quality \"xyz\""), "{e}");
        let e = Chord::parse("H7").unwrap_err();
        assert_eq!(e.position, 0);
        assert!(Chord::parse("").is_err());
        assert!(Chord::parse("C2").unwrap_err().message.contains("ambiguous"));
        assert!(Chord::parse("C4").unwrap_err().message.contains("ambiguous"));
        assert!(Chord::parse("Cmsus4").unwrap_err().message.contains("sus replaces the third"));
        assert!(Chord::parse("C/").unwrap_err().message.contains("missing bass"));
        assert!(Chord::parse("C/E7").unwrap_err().message.contains("bass"));
        assert!(Chord::parse("Cadd").unwrap_err().message.contains("add needs a degree"));
        assert!(Chord::parse("C7b9b9").unwrap_err().message.contains("twice"));
        assert!(Chord::parse("Cm7alt").is_err());
        assert!(Chord::parse("C15").is_err());
        assert!(Chord::parse("Csussus").is_err());
        let e = Chord::parse("Cmaj7(b10)").unwrap_err();
        assert!(e.message.contains("alteration"), "{e}");
        assert_eq!(e.to_string(), "chord \"Cmaj7(b10)\": unsupported alteration b10 (use b5 #5 b9 #9 #11 b13) (at character 7)");
    }
}
