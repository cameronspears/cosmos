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
  --gate-max-suggest-ms 26000 \
  --gate-max-suggest-cost-usd 0.016 \
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
  --gate-max-suggest-ms 26000 \
  --gate-max-suggest-cost-usd 0.016 \
  --gate-source both
```

## Production Runtime Contract

The real app (`cosmos <repo>`) uses the shared gated suggestion orchestrator and
does not rely on a separate pre-apply verify stage.

- Display contract:
  - validated-only suggestions
  - `pending_count == 0`
  - target count `10..=15`
  - target displayed validity `1.0`
- Budget contract:
  - suggest + refine cost target `< $0.015`
  - end-to-end suggestion budget `< 35s`
- Retry/fallback contract:
  - up to 2 hidden attempts
  - stop early on pass
  - on miss, show best validated set with gate-failure reasons (best-effort warning)
- Rewrite contract:
  - overclaiming impact language is conservatively rewritten and revalidated once before final rejection.

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
- Refinement uses a two-stage count policy: hard target `10`, stretch target `15` only when budget permits.
- Validation includes a bounded transport retry lane and tracks `validation_transport_retry_count` plus `validation_transport_recovered_count`.
- Regeneration stops early when validation budget is exhausted (`regen_stopped_validation_budget=true`) to avoid low-value extra spend.
- Overclaim rejects are rewritten to conservative grounded language and revalidated once before final rejection.

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
