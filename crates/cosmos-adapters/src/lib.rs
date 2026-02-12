//! Runtime adapters for Cosmos (git, config/auth, persistence, updates).

pub mod cache;
pub mod config;
pub mod git_ops;
pub mod github;
pub mod keyring;
pub mod onboarding;
pub mod update;
pub mod util;

// Compatibility re-exports for migrated modules that still expect old crate paths.
pub mod context {
    pub use cosmos_core::context::*;
}

pub mod grouping {
    pub use cosmos_core::grouping::*;
}

pub mod index {
    pub use cosmos_core::index::*;
}

pub mod suggest {
    pub use cosmos_core::suggest::*;
}
