//! Evidence-backed refactor operations for Weavatrix.
//!
//! This crate is to [`weavatrix_rust`] what a writer is to a reader: it consumes the read-only
//! evidence graph and produces `weavatrix.edit-plan.v1` envelopes, then applies them through
//! [`weavatrix_worktree`]'s crash-recoverable transaction. It owns no protocol; the MCP host
//! (`weavatrix-refactor`) composes this catalog with the read-only one.
//!
//! # The contract is frozen, not re-derived
//!
//! The eleven tool names, their schemas and every result state were recorded from the shipping
//! JavaScript implementation into `contract/refactor-tools.v1.json`. An operation here is
//! conformant when it answers with a status from that file — never a new one, never a renamed
//! one. That is what makes this an incremental replacement rather than a second protocol.
//!
//! # Safety boundary
//!
//! Producing a plan is a read. Applying one requires all three gates: the host exposes the edit
//! capability, `WEAVATRIX_ALLOW_SOURCE_EDITS=1` is set, and the call presents a single-use token
//! bound to that exact plan and repository. Nothing in this crate writes without them.

pub mod contract;
pub mod envelope;
pub mod evidence;
pub mod operations;
pub mod resolve;
#[cfg(test)]
mod test_support;
pub mod token;

/// Version of this crate, reported by the host beside the engine version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the frozen tool contract this crate implements.
pub const CONTRACT_VERSION: u32 = 1;

pub use contract::{ResultState, ToolContract};
pub use operations::{Operation, call, catalog, catalog_names};
