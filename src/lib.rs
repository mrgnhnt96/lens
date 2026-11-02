//! lens: agent code review. `lens review` runs a whole pass: it reads the change, maps what it
//! touches, picks the reviewer profiles that apply, runs each reviewer and verifier as its
//! own headless Claude session.

pub mod agent;
pub mod assets;
pub mod backoff;
pub mod config;
pub mod cost;
pub mod diff;
pub mod forge;
pub mod lang;
pub mod map;
pub mod outcome;
pub mod pr;
pub mod profile;
pub mod reply;
pub mod review;
pub mod select;
pub mod usage;
