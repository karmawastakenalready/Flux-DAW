//! Music theory: note names, chord symbols, scales, modes and keys, and how a pitch relates to a chord in
//! a key (chord tone, tension, avoid note or outside).

pub mod chord;
pub mod note;
pub mod scale;

pub use chord::{Chord, ChordSymbol, ChordTone, Role};
pub use note::{FLAT_NAMES, LETTERS, SHARP_NAMES, note_name, note_name_with, parse_note, parse_pitch_class, pc_name, spell};
pub use scale::{Mode, PcSet, Scale, ToneClass, chord_scale, classify, degree_name};
