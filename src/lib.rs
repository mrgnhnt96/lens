//! lens: agent code review. This crate holds the plumbing the `/review` skill calls through the
//! `lens` CLI: reading a change, mapping what it touches and picking the reviewer profiles that
//! apply to it.

pub mod config;
pub mod diff;
pub mod lang;
pub mod map;
pub mod profile;
pub mod select;
