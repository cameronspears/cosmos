# Sandboxed Self-Iteration

`cosmos-lab` is a maintainer tool for running autonomous validation loops in isolated git worktrees.
For the full operating playbook (cadence, metrics, troubleshooting), see
[`docs/reliability-loop.md`](reliability-loop.md).

## Safety Guarantees

- Every run creates detached worktrees under `$TMPDIR/cosmos-sandbox/<run_id>/`.
- Commands run with:
  - `GIT_TERMINAL_PROMPT=0`
  - `GIT_ASKPASS=/bin/true`
  - `COSMOS_DISABLE_PUSH=1`
- Push is hard-blocked while sandbox mode is active.
- Main checkouts are not mutated by worktree execution.

## Commands

### Fast Validation Loop

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode fast \
  --verify-sample 4
```

Optional rolling quality gate:

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode fast \
  --verify-sample 4 \
  --enforce-quality-gate \
  --gate-window 10 \
  --gate-min-displayed-validity 0.95 \
  --gate-min-final-count 10 \
  --gate-max-suggest-ms 30000 \
  --gate-max-suggest-cost-usd 0.01 \
  --gate-source both
```

Runs:
- Cosmos: `cargo test --locked`
- Target: `pnpm test:once`, `pnpm type-check`
- Reliability trial with grounding + refinement + preview sampling

### Full Validation Loop

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode full \
  --verify-sample 4
```

Adds:
- Cosmos perf gate: `scripts/perf/gate.sh`
- Target build: `pnpm build`
- Target lint: baseline-delta policy from `.cosmos/lab/lint-baseline.json`

Full mode remains strict: `target:build` is a blocking gate. If the target app requires
environment variables (for example, a Next.js build env), missing values will fail the run.

### Reliability Trials Only

```bash
cargo run --bin cosmos-lab -- reliability \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --trials 3 \
  --verify-sample 4
```

Optional rolling quality gate:

```bash
cargo run --bin cosmos-lab -- reliability \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --trials 3 \
  --verify-sample 4 \
  --enforce-quality-gate \
  --gate-window 10 \
  --gate-min-displayed-validity 0.95 \
  --gate-min-final-count 10 \
  --gate-max-suggest-ms 30000 \
  --gate-max-suggest-cost-usd 0.01 \
  --gate-source both
```

## When To Use Which Mode

| Mode | Use when | Key output |
| --- | --- | --- |
| `validate --mode fast` | Default every-iteration loop | Gate results + one reliability sample |
| `validate --mode full` | Every 3 successful fast loops, or before major merges | Adds perf/build/lint checks (strict build gate) |
| `reliability` | Tuning suggestion quality and measuring variability | Aggregated reliability metrics across trials |

## Reports and Telemetry

- JSON report files are written to `.cosmos/lab/` by default.
- Run telemetry is appended to `.cosmos/self_iteration_runs.jsonl`.
- Suggestion validation telemetry remains in `.cosmos/suggestion_quality.jsonl`.
- Primary pre-verify quality metric is `displayed_valid_ratio`.
- Gate on count/speed/cost too: `final_count`, `suggest_total_ms`, and `suggest_total_cost_usd`.
- Final refined output should keep `pending_count == 0` (no pending backfill).

## ETHOS-Aligned Run Summary Template

Use this structure when reviewing each run:

1. What changed:
   - Which reliability or validation behavior changed this run.
2. Why it matters:
   - User-facing reliability impact (precision, contradictions, trust).
3. What failed:
   - Exact failed gates, command names, and key diagnostics.
4. Assumptions and unknowns:
   - Unverified hypotheses or missing evidence.
5. Next action:
   - Smallest concrete follow-up iteration.
