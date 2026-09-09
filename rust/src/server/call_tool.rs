//! `LeanCtxServer::call_tool_guarded` — the guarded tool-dispatch path — and
//! root resolution. Split out of `server/mod.rs` to keep that module focused on
//! wiring. `use super::*` re-imports the parent aliases and sibling submodules.

#[allow(unused_imports, clippy::wildcard_imports)]
use super::*;

mod error_telemetry;
mod guarded;
mod outcome;
mod pipeline;
mod policy;

pub(super) use outcome::*;
pub(super) use pipeline::*;

#[cfg(test)]
mod tests;
