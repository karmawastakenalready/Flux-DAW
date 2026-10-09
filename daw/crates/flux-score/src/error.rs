//! Error types. Every message names the offending input and, for text, where in it the problem is, so
//! the person or agent that wrote it can fix it without guessing.

use thiserror::Error;

/// A bad note name or pitch ("H4", "C#", "G10").
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("note {input:?}: {message}")]
pub struct NoteError {
    pub input: String,
    pub message: String,
}

/// A chord symbol that could not be read. `position` is the 0-based character index of the problem.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("chord {symbol:?}: {message} (at character {})", position + 1)]
pub struct ChordError {
    pub symbol: String,
    pub position: usize,
    pub message: String,
}

/// A key or scale name that could not be read ("E minor", "F# dorian" are fine).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("key {input:?}: {message}")]
pub struct KeyError {
    pub input: String,
    pub message: String,
}

/// A problem in melody notation, chord charts or pattern strings. `position` is the 0-based character
/// index of the offending token in the text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{what}: {message} at character {} ({token:?})", position + 1)]
pub struct TextError {
    /// What was being read: "notation", "chord chart", "pattern".
    pub what: &'static str,
    pub position: usize,
    pub token: String,
    pub message: String,
}

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum Error {
    #[error(transparent)]
    Note(#[from] NoteError),
    #[error(transparent)]
    Chord(#[from] ChordError),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error(transparent)]
    Text(#[from] TextError),
    /// No voicing or bass note satisfies the constraints (range too narrow, too many voices...).
    #[error("{0}")]
    Voicing(String),
    /// An argument is missing, out of range or inconsistent with the project.
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub(crate) fn invalid(msg: impl Into<String>) -> Error {
        Error::Invalid(msg.into())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
