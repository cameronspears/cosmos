# codecosmos

A terminal health dashboard for your codebase. Get an instant health score (0-100) for any git repo - see complexity hotspots, danger zones, and track improvements over time.

```
┌───────────────────────────────────────────────────────────────┐
│  78/100 (B) [+]                          my-project @ main   │
│  "Good shape"                                                 │
├───────────────────────────────────────────────────────────────┤
│  [142 files]  [3 danger]  [23 changed]  [8 todos]  [12 dusty] │
├───────────────────────────────────────────────────────────────┤
│  [1] Danger Zones  [2] Hotspots  [3] Dusty  [4] TODOs         │
├───────────────────────────────────────────────────────────────┤
│  !!  src/parser.rs                                            │
│        12 changes | complexity 8.2 | high churn + complex     │
│  !   src/analyzer.rs                                          │
│        8 changes | complexity 6.1 | moderate churn + complex  │
└───────────────────────────────────────────────────────────────┘
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
# Run in current directory (launches TUI dashboard)
codecosmos

# Run in a specific directory
codecosmos /path/to/repo

# Quick check (no TUI, just print summary)
codecosmos --check

# Save score to history for trend tracking
codecosmos --check --save

# CI mode: fail if score below threshold
codecosmos --check --threshold 70

# JSON output for pipelines
codecosmos --json

# Customize analysis window
codecosmos --days 7 --stale-days 60
```

### Keyboard Controls

| Key | Action |
|-----|--------|
| `1-4` | Switch panels |
| `Tab` | Next panel |
| `k` / `Up` | Scroll up |
| `j` / `Down` | Scroll down |
| `PgUp/PgDn` | Scroll fast |
| `q` / `Esc` | Quit |

## Health Score

The health score (0-100) is calculated from four weighted components:

| Component | Weight | What it measures |
|-----------|--------|------------------|
| **Churn** | 30% | Ratio of files changed recently (high churn = lower score) |
| **Complexity** | 30% | Code complexity based on LOC, function length |
| **Debt** | 20% | TODO/FIXME/HACK comments per 1000 lines |
| **Freshness** | 20% | Ratio of dusty (untouched) files |

### Grades

| Grade | Score | Description |
|-------|-------|-------------|
| **A** | 90-100 | Excellent health |
| **B** | 75-89 | Good shape |
| **C** | 60-74 | Needs attention |
| **D** | 40-59 | Significant issues |
| **F** | 0-39 | Critical state |

### Trend Indicators

When history is available (use `--save`), the dashboard shows trend:
- **[+]** Improving (score increased by 3+)
- **[-]** Declining (score decreased by 3+)
- **[=]** Stable (within +/-2 points)

## Panels

### 1. Danger Zones
Files that are **both** high-churn AND high-complexity. These are your riskiest files - frequently changed and hard to maintain. Each entry shows:
- Risk level (`!!` critical, `!` high, `.` medium)
- File path
- Change count and complexity score
- **Actionable advice** (e.g., "split into smaller modules", "add test coverage")

### 2. Hotspots
Files with the most changes in the analysis window. High churn often indicates active development or instability.

### 3. Dusty Files
Old files nobody has touched in months. May indicate tech debt, dead code, or stable foundations.

### 4. TODOs
All TODO, FIXME, HACK, and XXX comments found in the codebase, sorted by priority.

## CI Integration

### GitHub Actions

```yaml
- name: Check code health
  run: |
    cargo install --path .
    codecosmos --check --threshold 70 --json > health.json
```

### Exit Codes

- `0` - Score meets or exceeds threshold (or no threshold set)
- `1` - Score below threshold

### JSON Output

Use `--json` for machine-readable output:

```json
{
  "score": 78,
  "grade": "B",
  "components": {
    "churn": 85,
    "complexity": 72,
    "debt": 90,
    "freshness": 65
  },
  "metrics": {
    "total_files": 142,
    "total_loc": 15420,
    "files_changed_recently": 23,
    "todo_count": 5,
    "fixme_count": 2,
    "hack_count": 1,
    "dusty_file_count": 12,
    "danger_zone_count": 3
  },
  "danger_zones": [
    {
      "path": "src/parser.rs",
      "danger_score": 57.2,
      "change_count": 12,
      "complexity_score": 8.2
    }
  ]
}
```

## Options

```
-d, --days <DAYS>        Days to analyze for churn [default: 14]
-s, --stale-days <DAYS>  Days until a file is "dusty" [default: 90]
-c, --check              Print summary and exit (no TUI)
-t, --threshold <SCORE>  Minimum score threshold (exit 1 if below)
    --json               Output results as JSON
    --save               Save score to history for trend tracking
-h, --help               Print help
-V, --version            Print version
```

## History

Scores are stored in `.codecosmos/history.json` in your repo (automatically gitignored).

## License

MIT
