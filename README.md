# Cosmos

Cosmos is a terminal-first AI code review and fix engine for Git repositories.

This repository now uses a big-bang modular architecture that preserves the existing TUI behavior while moving all backend/runtime logic behind crate boundaries.

## What Cosmos does

- Scans repository structure and finds bugs, performance issues, quality risks, and refactor opportunities
- Explains issues in plain language with concrete impact
- Shows preview scope before mutating files
- Applies fixes through a guarded harness (sandbox + safety gates + quick checks)
- Runs adversarial review after apply and supports shipping via commit/push/PR

## Workspace layout

- `crates/cosmos-app` - `cosmos` binary entrypoint and CLI
- `crates/cosmos-ui` - preserved terminal UI and keybinding behavior
- `crates/cosmos-core` - shared domain model and protocol contracts
- `crates/cosmos-engine` - suggestion/preview/apply/review engine implementation
- `crates/cosmos-adapters` - git, auth/config, cache persistence, update adapters

Legacy `src/` backend modules and `cosmos-lab` tooling were removed in this rewrite.

## Core contracts

`cosmos-core` defines:

- Command protocol (`Command`)
- Event protocol (`Event`)
- Engine interface (`Engine` trait)

These contracts are the stable boundary used by UI/runtime orchestration.

## Persistence layout

Cosmos now writes runtime data under `.cosmos/v2`.

On first run after this rewrite:

- Existing legacy `.cosmos/*` top-level files are moved to `.cosmos/v1-archive-<timestamp>/`
- New runtime state is written to `.cosmos/v2/`

## Usage

```bash
# Run in current repository
cargo run -p cosmos-tui -- .

# Setup OpenRouter API key
cargo run -p cosmos-tui -- --setup

# Setup GitHub login
cargo run -p cosmos-tui -- --github-login

# Run suggestions in non-interactive audit mode with detailed trace diagnostics
cargo run -p cosmos-tui -- --suggest-audit --suggest-runs 1 --suggest-trace

# Stream reasoning/thinking deltas live during audit
cargo run -p cosmos-tui -- --suggest-audit --suggest-runs 1 --suggest-trace --suggest-stream-reasoning
```

See `docs/suggestions-observability.md` for the Suggestions pipeline diagram and trace workflow.

## Development

```bash
# Build all crates
cargo build --workspace

# Check all crates
cargo check --workspace

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings
```

## Install (from this repo)

```bash
cargo install --path crates/cosmos-app
```
