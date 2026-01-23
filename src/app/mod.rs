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
use std::sync::{Arc, Mutex};

/// Context passed to runtime operations containing shared state
///
/// This struct provides access to the codebase index, repository path,
/// message channel, and budget tracking for background operations.
pub struct RuntimeContext<'a> {
    /// Reference to the indexed codebase
    pub index: &'a CodebaseIndex,
    /// Path to the repository root
    pub repo_path: &'a PathBuf,
    /// Channel for sending messages to the main thread
    pub tx: &'a mpsc::Sender<messages::BackgroundMessage>,
    /// Budget guard for tracking AI usage costs
    pub budget_guard: BudgetGuard,
}

/// Thread-safe guard for tracking AI usage costs and token budgets
///
/// Shared across background tasks to accumulate costs and enforce limits.
/// Uses interior mutability via `Arc<Mutex<_>>` for safe concurrent access.
#[derive(Clone, Default)]
pub struct BudgetGuard {
    inner: Arc<Mutex<BudgetState>>,
}

#[derive(Default)]
struct BudgetState {
    session_cost: f64,
    session_tokens: u32,
}

impl BudgetGuard {
    /// Create a new budget guard with initial cost and token counts
    pub fn new(session_cost: f64, session_tokens: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BudgetState {
                session_cost,
                session_tokens,
            })),
        }
    }

    /// Record usage from an AI operation
    ///
    /// Adds the specified cost and tokens to the running totals.
    /// Uses saturating addition to prevent overflow.
    pub fn record_usage(&self, cost: f64, tokens: u32) {
        if let Ok(mut state) = self.inner.lock() {
            state.session_cost += cost;
            state.session_tokens = state.session_tokens.saturating_add(tokens);
        }
    }

    /// Get the current session cost in USD
    ///
    /// Recovers gracefully if the lock was poisoned by a panic.
    pub fn session_cost(&self) -> f64 {
        match self.inner.lock() {
            Ok(state) => state.session_cost,
            Err(poisoned) => {
                // Lock was poisoned by a panic; recover the data
                eprintln!("Warning: BudgetGuard lock was poisoned, recovering state");
                poisoned.into_inner().session_cost
            }
        }
    }

    /// Check if AI operations are allowed given current budget constraints
    ///
    /// Returns `Ok(())` if within budget, `Err(message)` if budget exceeded.
    pub fn allow_ai(&self, config: &mut crate::config::Config) -> Result<(), String> {
        let session_cost = self.session_cost();
        config.allow_ai(session_cost)
    }
}
