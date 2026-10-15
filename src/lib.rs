//! lens: agent code review. `lens review` runs a whole pass: it reads the change, maps what it
//! touches, picks the reviewer profiles that apply, runs each reviewer, verifier and fixer as its
//! own headless Claude session, and keeps the ledger of the blocker fix loop.

pub mod agent;
pub mod config;
pub mod diff;
pub mod fix;
pub mod lang;
pub mod map;
pub mod pr;
pub mod profile;
pub mod review;
pub mod select;
