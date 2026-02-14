//! Terminal UI layer for Cosmos.

pub mod app;
pub mod cache;
pub mod config;
pub mod context;
pub mod git_ops;
pub mod index;
pub mod suggest;
pub mod ui;
pub mod update;
pub mod util;

pub use app::run_tui;
