//! Stopping a long operation (a download, a clone, an install) from another thread, such as Setup's Cancel button.
//!
//! Cancelling never leaves a half-installed asset: installs are staged and only moved into place at the end, a
//! partial download is kept to be resumed next time, and a running `git` is stopped.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A flag shared between the operation and whoever may stop it. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the operation to stop at its next check.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Clear the flag so the next operation can run.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(Cancelled)` once cancelled.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() { Err(Cancelled) } else { Ok(()) }
    }
}

/// The error an operation stops with when it was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Whether an error (anywhere in its chain) is a cancellation.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Cancelled>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn clones_share_the_flag() {
        let c = Cancel::new();
        let other = c.clone();
        assert!(c.check().is_ok());
        other.cancel();
        assert!(c.is_cancelled());
        let e = Err::<(), _>(c.check().unwrap_err()).context("downloading x").unwrap_err();
        assert!(is_cancelled(&e));
        assert!(!is_cancelled(&anyhow::anyhow!("network down")));
        c.reset();
        assert!(!other.is_cancelled());
    }
}
