//! Cosmos library crate
//!
//! Exposes core modules so benchmarks and external tooling can exercise
//! internal performance-critical paths without going through CLI startup.

pub mod app;
pub mod cache;
pub mod config;
pub mod context;
pub mod git_ops;
pub mod github;
pub mod grouping;
pub mod index;
pub mod keyring;
pub mod onboarding;
pub mod suggest;
pub mod ui;
pub mod update;
pub mod util;
