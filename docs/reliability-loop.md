# Cosmos→Gielinor-Gains Reliability Loop

This runbook defines the maintainer workflow for using Cosmos against
`/Users/cam/WebstormProjects/gielinor-gains` to improve suggestion reliability.

## Goal and Scope

- Goal: Improve Cosmos suggestion quality using repeated real-codebase validation.
- Scope: Maintainer-only loop using `cosmos-lab` sandboxed runs.
- Primary target repo: `/Users/cam/WebstormProjects/gielinor-gains`.

## Proof We Are Already Doing This

- Default target in CLI: `src/bin/cosmos-lab.rs` sets
  `DEFAULT_TARGET_REPO = "/Users/cam/WebstormProjects/gielinor-gains"`.
- Real passing run exists at:
  - `.cosmos/lab/manual-validate-fast-real.json`
  - Includes non-null `reliability_metrics` and `reliability_failure_kind: null`.

## Safety Model

`cosmos-lab` runs in isolated git worktrees and is designed for no-surprise execution:

- Sandboxes created under `$TMPDIR/cosmos-sandbox/<run_id>/`.
- Git prompts disabled:
  - `GIT_TERMINAL_PROMPT=0`
  - `GIT_ASKPASS=/bin/true`
- Push disabled:
  - `COSMOS_DISABLE_PUSH=1`
- Main checkouts should remain unchanged after runs.

## Preconditions

- Cosmos repo exists at `/Users/cam/WebstormProjects/cosmos`.
- Target repo exists at `/Users/cam/WebstormProjects/gielinor-gains`.
- Tooling available:
  - `cargo`
  - `pnpm`
  - `git`
- Network/API access available for reliability model calls.
- If running `--mode full`, target build environment variables are set (strict blocking gate).

## First-Time Bootstrap

Run once to establish baseline behavior and warm caches/indexes:

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode fast \
  --verify-sample 4

cargo run --bin cosmos-lab -- reliability \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --trials 3 \
  --verify-sample 4
```

Optional explicit output paths for audit trails:

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode fast \
  --verify-sample 4 \
  --output .cosmos/lab/validate-fast-bootstrap.json

cargo run --bin cosmos-lab -- reliability \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --trials 5 \
  --verify-sample 4 \
  --output .cosmos/lab/reliability-bootstrap.json
```

## Standard Operating Cadence

- Fast loop: every iteration.
- Full loop: every 3 successful fast loops, or before major merges.
- Reliability-only trials: when tuning suggestion precision/grounding behavior.

### Fast Loop Command

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode fast \
  --verify-sample 4
```

### Full Loop Command

```bash
cargo run --bin cosmos-lab -- validate \
  --cosmos-repo /Users/cam/WebstormProjects/cosmos \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --mode full \
  --verify-sample 4
```

### Reliability Trials Command

```bash
cargo run --bin cosmos-lab -- reliability \
  --target-repo /Users/cam/WebstormProjects/gielinor-gains \
  --trials 3 \
  --verify-sample 4
```

## Reports and Telemetry

Primary files:

- Run reports: `.cosmos/lab/*.json`
- Run summary telemetry: `.cosmos/self_iteration_runs.jsonl`
- Suggestion quality telemetry: `.cosmos/suggestion_quality.jsonl`

Use these to compare run-over-run quality and failure modes.

## Metric Interpretation Guide

Core report fields and how to read them:

- `validated_ratio`: proportion of provisional suggestions that survived validation.
- `rejected_ratio`: proportion filtered out by validator/regeneration logic.
- `preview_precision`: `verified / (verified + contradicted)` over sampled previews.
- `evidence_line1_ratio`: proportion of pack anchors at line 1 (lower is usually better).
- `reliability_failure_kind`: machine-classified cause when reliability run fails.

Interpretation shortcuts:

- High `preview_precision` + lower `rejected_ratio` = healthier reliability loop.
- Rising `evidence_line1_ratio` suggests weaker grounding anchors.
- Non-null `reliability_failure_kind` means reliability metrics may be unavailable for that run.

## Maintainer Reliability Targets

Use these targets to keep tuning decisions objective:

- Rolling preview precision target: `>= 0.90`.
- Contradictions guardrail: keep `preview_contradicted_count` low and non-trending.
- Evidence anchor quality: keep `evidence_line1_ratio <= 0.25`.
- Evidence diversity: no single source family should dominate without rationale.

## Failure Classification Playbook

### `IndexEmpty`

Symptoms:

- `reliability_failure_kind = "IndexEmpty"`
- Reliability notes mention empty index or no supported files.

Actions:

1. Confirm target repo path points to expected checkout.
2. Confirm target contains supported source files.
3. Re-run fast validate once to warm index/cache.

### `InsufficientEvidencePack`

Symptoms:

- `reliability_failure_kind = "InsufficientEvidencePack"`
- Notes mention not enough grounded evidence.

Actions:

1. Re-run after indexing completes.
2. Increase trials/sample to gather more signal.
3. Inspect evidence quality telemetry before changing heuristics.

### `LlmUnavailable`

Symptoms:

- `reliability_failure_kind = "LlmUnavailable"`
- Notes mention API key, auth, rate limits, connectivity, or provider errors.

Actions:

1. Verify API key and network availability.
2. Retry after transient outage/rate limiting.
3. Treat as infrastructure signal, not product-quality regression.

### `Other`

Symptoms:

- `reliability_failure_kind = "Other"`
- Unclassified error text in notes.

Actions:

1. Capture report path and failing note text.
2. Reproduce with explicit `--output`.
3. Add classification mapping if recurrence is observed.

## Troubleshooting Examples

- Index-empty style note:
  - `Reliability preflight failed: codebase index is empty for ...`
- LLM unavailable style note:
  - API key missing/invalid, OpenRouter timeout, network/connect errors.
- Full-mode strict build failure:
  - `target:build` fails when required app env vars are missing.

## ETHOS-Aligned Iteration Template

For each loop, record:

1. What changed.
2. Why it matters.
3. What failed.
4. Assumptions and unknowns.
5. Next action.

Keep claims evidence-backed and linked to report/telemetry fields.

## Definition of Done for a Reliability Iteration

A loop is done when all are true:

- Fast or full run completed and produced a report in `.cosmos/lab/`.
- Safety expectations held (no primary checkout mutations, no pushes).
- Reliability outcome is interpretable:
  - Either valid metrics are present, or failure is classified.
- Next action is documented from observed evidence.
