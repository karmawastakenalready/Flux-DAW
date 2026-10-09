//! COM objects the host implements for plugins.

pub mod host;
pub mod message;
pub mod process;
pub mod stream;

pub use host::{ComponentHandler, HandlerEvent, HandlerSink, HostApplication, PlugFrame};
pub use message::{AttributeList, Message};
pub use process::{note_off, note_on, EventList, ParamChanges, ParamQueue};
pub use stream::MemStream;
