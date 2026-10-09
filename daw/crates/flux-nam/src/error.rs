//! The crate's error type. Every message names the file and, where it applies, the JSON location of the problem,
//! so a broken rig or model can be fixed from the message alone.

use std::path::{Path, PathBuf};

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong loading rigs and models or building a chain.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file could not be read.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A file is not valid JSON.
    #[error("{}: invalid JSON: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A rig file, or a parameter override, is invalid. `at` locates the problem (file and JSON path).
    #[error("{at}: {message}")]
    Rig { at: String, message: String },
    /// A `.nam` model is invalid or uses a feature this crate does not implement.
    #[error("{at}: {message}")]
    Model { at: String, message: String },
    /// An impulse-response file could not be decoded or is unusable.
    #[error("{}: {message}", path.display())]
    Audio { path: PathBuf, message: String },
    /// A chain stage cannot run with the given processing settings (sample rate, block size).
    #[error("{at}: {message}")]
    Stage { at: String, message: String },
    /// The host-supplied processor factory (see [`crate::StageHooks`]) failed for a stage.
    #[error("{at}: {source:#}")]
    Hook {
        at: String,
        #[source]
        source: anyhow::Error,
    },
}

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Error {
        Error::Io { path: path.to_path_buf(), source }
    }

    pub(crate) fn rig(at: impl Into<String>, message: impl Into<String>) -> Error {
        Error::Rig { at: at.into(), message: message.into() }
    }

    pub(crate) fn model(at: impl Into<String>, message: impl Into<String>) -> Error {
        Error::Model { at: at.into(), message: message.into() }
    }

    pub(crate) fn audio(path: &Path, message: impl Into<String>) -> Error {
        Error::Audio { path: path.to_path_buf(), message: message.into() }
    }

    pub(crate) fn stage(at: impl Into<String>, message: impl Into<String>) -> Error {
        Error::Stage { at: at.into(), message: message.into() }
    }
}

/// Read and parse a JSON file.
pub(crate) fn read_json(path: &Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json { path: path.to_path_buf(), source })
}
