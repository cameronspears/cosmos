pub mod background;
pub mod input;
pub mod messages;
pub mod runtime;

#[allow(unused_imports)]
pub use messages::BackgroundMessage;
pub use runtime::run_tui;

use crate::index::CodebaseIndex;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

pub struct RuntimeContext<'a> {
    pub index: &'a CodebaseIndex,
    pub repo_path: &'a PathBuf,
    pub tx: &'a mpsc::Sender<messages::BackgroundMessage>,
    pub budget_guard: BudgetGuard,
}

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
    pub fn new(session_cost: f64, session_tokens: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BudgetState {
                session_cost,
                session_tokens,
            })),
        }
    }

    pub fn record_usage(&self, cost: f64, tokens: u32) {
        if let Ok(mut state) = self.inner.lock() {
            state.session_cost += cost;
            state.session_tokens = state.session_tokens.saturating_add(tokens);
        }
    }

    pub fn session_cost(&self) -> f64 {
        self.inner
            .lock()
            .map(|s| s.session_cost)
            .unwrap_or(0.0)
    }

    pub fn allow_ai(&self, config: &mut crate::config::Config) -> Result<(), String> {
        let session_cost = self.session_cost();
        config.allow_ai(session_cost)
    }
}
