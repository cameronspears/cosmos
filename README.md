# CodeCosmos 🌌

A terminal mood dashboard for your codebase. Run it in any git repo to get an instant vibe check — see if your project is **Calm**, **Chaotic**, **Stale**, or in a **Refactor Frenzy**.

```
┌─────────────────────────────────────────────────────────┐
│  ◉ CALM                            my-project @ main   │
│  "Steady progress, no fires"                           │
├─────────────────────────────────────────────────────────┤
│  📁 142 files │ 📝 12 changed │ 📌 8 TODOs │ 🕸️ 3 dusty │
├─────────────────────────────────────────────────────────┤
│  [1] Hotspots  [2] Dusty Files  [3] TODOs & HACKs      │
├─────────────────────────────────────────────────────────┤
│  src/parser.rs .............. 23 changes (14 days)     │
│  src/main.rs ................ 18 changes (14 days)     │
│  lib/analyzer.rs ............ 12 changes (14 days)     │
└─────────────────────────────────────────────────────────┘
```

## Installation

### From source (requires Rust)

```bash
cargo install --path .
```

Or run directly:

```bash
cargo run
```

## Usage

```bash
# Run in current directory
codecosmos

# Run in a specific directory
codecosmos /path/to/repo

# Quick check (no TUI, just print summary)
codecosmos --check

# Customize analysis window
codecosmos --days 7 --stale-days 60
```

### Keyboard Controls

| Key | Action |
|-----|--------|
| `1-3` | Switch panels |
| `Tab` | Next panel |
| `↑/k` | Scroll up |
| `↓/j` | Scroll down |
| `PgUp/PgDn` | Scroll fast |
| `q/Esc` | Quit |

## What It Shows

### Moods

| Mood | What it means |
|------|---------------|
| **◉ Calm** | Balanced activity, decreasing churn, few TODOs |
| **⚡ Chaotic** | High churn across many files, lots of TODOs |
| **◎ Stale** | Low recent activity, many untouched files |
| **🔄 Refactor Frenzy** | Concentrated changes, high delete ratio |

### Panels

1. **Hotspots** — Files with the most changes recently (your "danger zones")
2. **Dusty Files** — Old files nobody has touched in months (potential tech debt)
3. **TODOs & HACKs** — All your TODO, FIXME, HACK, and XXX comments

## Options

```
-d, --days <DAYS>        Days to analyze for churn [default: 14]
-s, --stale-days <DAYS>  Days until a file is "dusty" [default: 90]
-c, --check              Print summary and exit (no TUI)
-h, --help               Print help
-V, --version            Print version
```

## License

MIT


