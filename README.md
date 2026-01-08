# *c o s m o s*

A **terminal-first codebase steward** for solo developers.

Cosmos lives *outside* your editing loop. It reads git context, surfaces high-leverage improvements, and ships fixes as branches + PRs — all from the terminal.

**Monochromatic. Minimal. Meaningful.**

![Cosmos Main Interface](assets/image-1cd94e09-8039-41ea-bce0-95a5767cdb9b.png)

## What It Does

- **Indexes your codebase** using tree-sitter (Rust, TypeScript, JavaScript, Python, Go)
- **Suggests improvements** via AI with smart tiering (Opus 4.5 for depth, fast models for speed)
- **Ships fixes safely** — preview changes, apply to a branch, create PRs
- **Remembers context** — caches summaries, tracks your decisions

![Suggestions View](assets/image-591aa864-7d98-4437-9128-02a3744679bc.png)

## Installation

```bash
# From source
cargo install --path .

# Or run directly
cargo run --release
```

## Quick Start

```bash
# Launch the TUI
cosmos

# Point at a specific project
cosmos /path/to/project

# Time-boxed improvement session (ritual mode)
cosmos ritual --minutes 15

# Show stats without TUI
cosmos --stats

# Set up AI features (BYOK mode)
cosmos --setup
```

## Keyboard Controls

| Key | Action |
|-----|--------|
| `↑/k` `↓/j` | Navigate |
| `Tab` | Switch panels |
| `Enter` | View detail |
| `/` | Search |
| `?` | Toggle help |

### Actions

| Key | Action |
|-----|--------|
| `a` | Apply suggestion (generates preview first) |
| `d` | Dismiss suggestion |
| `i` | Inquiry — ask AI a question about your code |
| `u` | Undo last applied change |
| `r` | Refresh context |

### Git & Shipping

| Key | Action |
|-----|--------|
| `s` | Ship — commit + push + create PR |
| `c` | Git status (stage/unstage files) |
| `m` | Switch to main branch |
| `b` | Branch workflow |

### Modes & Views

| Key | Action |
|-----|--------|
| `R` | Ritual mode — curated time-boxed session |
| `g` | Toggle flat/grouped view |
| `S` | Cycle sort mode |
| `M` | Repo memory (store decisions) |
| `1-8` | Jump to architectural layer |

![Ship Workflow](assets/image-ceb802d7-fbe0-429c-ad69-5d867da46bb6.png)

## Workflows

### Apply Flow

When you press `a` on a suggestion:

1. **Preview** — AI verifies the issue and shows a human-readable plan
2. **Confirm** — Press `y` to apply, `n` to cancel, `m` to modify
3. **Apply** — Creates a fix branch, applies the change, runs safety checks
4. **Ship** — Press `s` to commit, push, and create a PR in one motion

### Ritual Mode

A focused, time-boxed improvement session:

```bash
cosmos ritual --minutes 10
```

Presents a curated queue of suggestions. Work through them one by one. Mark as done, skip, or dismiss.

### Ship Workflow

After applying changes, press `s` to:
1. Stage modified files
2. Commit with auto-generated message
3. Push to remote
4. Create a PR via GitHub CLI

## Smart Caching

Cosmos caches aggressively to minimize LLM costs:

- **File summaries** — Cached by content hash, regenerated only when files change
- **Suggestions** — Persisted between sessions
- **Decisions** — Repo memory stores your preferences

On subsequent runs, unchanged files load instantly from cache.

![Codebase View](assets/image-d056b5fc-6545-4aac-a0a3-e5eaaeb3b83a.png)

## Suggestion Types

| Icon | Priority |
|------|----------|
| ● | High — significant improvement |
| ◐ | Medium — worth considering |
| ○ | Low — minor enhancement |

**Categories:** Improvement, BugFix, Optimization, Quality, Feature

## AI Setup (BYOK Mode)

```bash
cosmos --setup
```

Guides you through getting an [OpenRouter API key](https://openrouter.ai/keys). Your key is saved locally.

## License Management

```bash
# Activate a license
cosmos --activate <KEY>

# Check status
cosmos --status

# View usage
cosmos --usage

# Deactivate
cosmos --deactivate
```

**Tiers:**
- **Free** — BYOK mode, billed to your OpenRouter account
- **Pro** — Bundled tokens, monthly allowance
- **Team** — Higher limits, priority support

## Configuration

Press `O` to see your config file location. Config options:

- **Privacy preview** (`P`) — Preview what gets sent to AI before sending
- **Summarize changed only** (`T`) — Only summarize modified files and their dependencies

## Project Structure

```
cosmos/
├── src/
│   ├── main.rs          # Entry point, CLI, event loop
│   ├── cache/           # Smart caching (summaries, suggestions)
│   ├── config.rs        # User configuration
│   ├── context/         # Git-aware work context
│   ├── git_ops.rs       # Git operations (branch, commit, PR)
│   ├── grouping/        # Architectural layer detection
│   ├── history.rs       # Suggestion history (SQLite)
│   ├── index/           # AST-based codebase indexing
│   ├── license.rs       # License management
│   ├── onboarding.rs    # First-run experience
│   ├── safe_apply.rs    # Safety checks before applying
│   ├── suggest/         # Suggestion engine (LLM + static rules)
│   └── ui/              # TUI components (ratatui)
└── assets/              # Screenshots
```

## Design Philosophy

**Contemplative pace:**
- Suggestions, not demands
- Preview before applying
- Undo always available

**Cost-conscious:**
- Free static analysis first
- Smart caching eliminates redundant LLM calls
- Tiered models (fast for summaries, powerful for suggestions)

**Git-native:**
- Knows what you're working on from uncommitted changes
- Ships fixes as proper branches with PRs
- One-key undo to restore backups

## CLI Reference

```
Usage: cosmos [OPTIONS] [PATH]
       cosmos ritual [PATH] --minutes <N>

Arguments:
  [PATH]  Path to the repository [default: .]

Options:
      --setup             Set up OpenRouter API key
      --stats             Show stats and exit (no TUI)
      --activate <KEY>    Activate a Cosmos Pro license
      --deactivate        Deactivate current license
      --status            Show license and usage status
      --usage             Show token usage statistics
  -h, --help              Print help
  -V, --version           Print version

Ritual Mode:
      --minutes <N>       Session length in minutes [default: 10]
```

## License

MIT

---

*"A contemplative companion for your codebase"*
