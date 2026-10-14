//! lens: agent code review. This crate holds the plumbing the `/review` skill calls through the
//! `lens` CLI: reading a change and picking the reviewer profiles that apply to it.

pub mod diff;
pub mod lang;
pub mod profile;
pub mod select;
