//! Errors, source locations and parser diagnostics.

use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// A position in a source file: 1-based line and column (the column counts characters, not bytes).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Location {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
}

impl Location {
    pub fn new(file: impl Into<PathBuf>, line: u32, column: u32) -> Location {
        Location { file: file.into(), line, column }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file.display(), self.line, self.column)
    }
}

/// How bad a [`Diagnostic`] is. `Error` means sfizz reports a parse error or drops something the author
/// meant to play; `Warning` is legal but suspicious (sfizz ignores it or it only works by accident).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

/// A problem found while reading an SFZ file. Parsing never stops at the first problem: like sfizz, the
/// parser reports it, skips the rest of the line and carries on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub location: Location,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        write!(f, "{}: {sev}: {}", self.location, self.message)
    }
}

/// Errors returned by this crate. Problems *inside* an SFZ file are [`Diagnostic`]s, not errors: an error
/// means the operation as a whole could not be done (a file could not be read, a set definition is invalid...).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An SFZ file that a tool needs to understand completely has a problem it cannot work around.
    #[error("{location}: {message}")]
    Sfz { location: Location, message: String },
    /// A sample file that could not be read or decoded.
    #[error("{}: {message}", path.display())]
    Audio { path: PathBuf, message: String },
    /// An invalid instrument set definition (sfz/sets/*.toml).
    #[error("{}: {message}", path.display())]
    Set { path: PathBuf, message: String },
    /// A manifest or other JSON/TOML file this crate writes could not be read back.
    #[error("{}: {message}", path.display())]
    Format { path: PathBuf, message: String },
    /// Rendering through sfizz failed (library missing, file rejected...).
    #[error("sfizz: {0}")]
    Render(String),
    /// Bad arguments to a library function.
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Error {
        Error::Io { path: path.to_path_buf(), source }
    }

    pub(crate) fn audio(path: &Path, message: impl Into<String>) -> Error {
        Error::Audio { path: path.to_path_buf(), message: message.into() }
    }

    pub(crate) fn set(path: &Path, message: impl Into<String>) -> Error {
        Error::Set { path: path.to_path_buf(), message: message.into() }
    }
}

/// Read a whole file, with the path in the error.
pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::io(path, e))
}

/// Write a whole file (creating parent directories), with the path in the error.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    std::fs::write(path, bytes).map_err(|e| Error::io(path, e))
}
