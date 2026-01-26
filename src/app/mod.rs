//! Application runtime module for Cosmos
//!
//! This module contains the TUI runtime, background task management,
//! input handling, and message passing infrastructure.

pub mod background;
pub mod input;
pub mod messages;
pub mod runtime;

pub use runtime::run_tui;

use crate::index::CodebaseIndex;
use std::path::PathBuf;
use std::sync::mpsc;

/// Context passed to runtime operations containing shared state
///
/// This struct provides access to the codebase index, repository path,
/// and message channel for background operations.
pub struct RuntimeContext<'a> {
    /// Reference to the indexed codebase
    pub index: &'a CodebaseIndex,
    /// Path to the repository root
    pub repo_path: &'a PathBuf,
    /// Channel for sending messages to the main thread
    pub tx: &'a mpsc::Sender<messages::BackgroundMessage>,
}
