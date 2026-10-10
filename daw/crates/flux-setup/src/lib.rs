//! Installing Flux: the app itself (per user, no administrator rights), its instrument library (packs chosen in
//! Setup or the Library window), the agents (MCP clients) that drive it, and the daily check for a new release.

pub mod agents;
pub mod cli;
pub mod install;
pub mod job;
pub mod seed;
pub mod update;
