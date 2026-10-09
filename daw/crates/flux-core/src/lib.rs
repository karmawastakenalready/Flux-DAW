//! Flux core: the project model, the time map, and the command system.
//!
//! Everything that changes a project goes through [`commands::Command`], whether it comes from an
//! agent (MCP) or a human (GUI). That gives both the same powers, a single undo history, and an
//! activity log that records who did what.

pub mod commands;
pub mod model;
pub mod pitch;
pub mod session;
pub mod time;

pub use commands::Command;
pub use model::*;
pub use session::{CommentOp, Session, Source};
pub use time::TimeMap;
