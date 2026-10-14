//! lens: agent code review. This crate holds the plumbing the `/review` skill calls through the
//! `lens` CLI: reading a change, mapping what it touches and picking the reviewer profiles that
//! apply to it, and the ledger of the blocker fix loop.

pub mod config;
pub mod diff;
pub mod fix;
pub mod lang;
pub mod map;
pub mod pr;
pub mod profile;
pub mod select;
