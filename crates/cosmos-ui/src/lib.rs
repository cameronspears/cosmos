//! Terminal UI layer for Cosmos.

pub mod app;
pub mod ui;

// Compatibility re-exports so preserved UI/input modules can continue using
// historical `crate::...` paths while running on the new workspace crates.
pub mod cache {
    pub use cosmos_adapters::cache::*;
}

pub mod config {
    pub use cosmos_adapters::config::*;
}

pub mod context {
    pub use cosmos_core::context::*;
}

pub mod git_ops {
    pub use cosmos_adapters::git_ops::*;
}

pub mod github {
    pub use cosmos_adapters::github::*;
}

pub mod grouping {
    pub use cosmos_core::grouping::*;
}

pub mod index {
    pub use cosmos_core::index::*;
}

pub mod keyring {
    pub use cosmos_adapters::keyring::*;
}

pub mod onboarding {
    pub use cosmos_adapters::onboarding::*;
}

pub mod suggest {
    pub use cosmos_core::suggest::*;

    pub mod llm {
        pub use cosmos_engine::llm::*;
    }
}

pub mod update {
    pub use cosmos_adapters::update::*;
}

pub mod util {
    pub use cosmos_adapters::util::*;
}

pub use app::run_tui;
