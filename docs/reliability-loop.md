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

### Fast Loop With Rolling Quality Gate

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

### Reliability Trials With Rolling Quality Gate

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

## Production Runtime Gate Contract

Real app runs (`cosmos <repo>`) now use the same gated suggestion cycle as lab via
`run_fast_grounded_with_gate(...)`, with hidden retries and validated-only output.

- Gate targets:
  - `final_count` in `10..=15`
  - `displayed_valid_ratio == 1.0`
  - `pending_count == 0`
  - `suggest_total_cost_usd <= 0.015` (suggest + refine only)
  - `suggest_total_ms <= 35_000`
- Retry policy:
  - up to `2` hidden attempts
  - stop early on first pass
  - second attempt only when remaining budget allows
- Fallback policy:
  - if gate still misses, show best validated set plus explicit gate-failure reasons
  - never inject pending suggestions
- Overclaim handling:
  - impact/assumption-heavy rejects are rewritten to conservative grounded wording once, then revalidated.

## Reports and Telemetry

Primary files:

- Run reports: `.cosmos/lab/*.json`
- Run summary telemetry: `.cosmos/self_iteration_runs.jsonl`
- Suggestion quality telemetry: `.cosmos/suggestion_quality.jsonl`

Use these to compare run-over-run quality and failure modes.

## Metric Interpretation Guide

Core report fields and how to read them:

- `displayed_valid_ratio`: `validated_count / final_count` for suggestions shown after refinement (primary quality signal).
- `final_count`: number of suggestions shown to the user after refinement.
- `validated_ratio`: proportion of provisional suggestions that survived validation.
- `rejected_ratio`: proportion filtered out by validator/regeneration logic.
- `pending_count`: number of non-validated suggestions left in final output (target is always zero).
- `suggest_total_ms`: end-to-end suggest + refine latency for the run.
- `suggest_total_cost_usd`: LLM spend for suggest + refine in USD.
- `preview_precision`: `verified / (verified + contradicted)` over sampled previews.
- `evidence_line1_ratio`: proportion of pack anchors at line 1 (lower is usually better).
- `validation_transport_retry_count`: number of transport/deadline validator failures that were retried once.
- `validation_transport_recovered_count`: retried transport failures that eventually validated.
- `regen_stopped_validation_budget`: regeneration halted because validation budget/deadline made further refinement unlikely to succeed.
- `reliability_failure_kind`: machine-classified cause when reliability run fails.

Interpretation shortcuts:

- High `displayed_valid_ratio` with `pending_count = 0` is required, but not sufficient alone.
- Also watch `final_count`, `suggest_total_ms`, and `suggest_total_cost_usd` to avoid high-accuracy/low-throughput regressions.
- Validation now includes a bounded transport retry lane; rising transport retries with low recovery indicates provider instability, not rubric weakness.
- Refinement behavior is target-staged: hard fill target `10`, opportunistic stretch target `15` when budget allows.
- Production uses hidden retry attempts and surfaces a best-effort validated set with gate miss reasons when retries are exhausted.
- Overclaim rewrite/revalidate is enabled before final rejection for assumption-heavy impact claims.
- High `preview_precision` + lower `rejected_ratio` = healthier end-to-end reliability loop.
- Rising `evidence_line1_ratio` suggests weaker grounding anchors.
- Non-null `reliability_failure_kind` means reliability metrics may be unavailable for that run.

## Maintainer Reliability Targets

Use these targets to keep tuning decisions objective:

- Rolling displayed validity target: `displayed_valid_ratio >= 0.95`.
- Rolling final count target: `final_count >= 10`.
- Pending guardrail: `pending_count == 0` in every gated run.
- Rolling speed target: `suggest_total_ms <= 26000`.
- Rolling cost target: `suggest_total_cost_usd <= 0.016`.
- Rolling preview precision target: keep `preview_precision` healthy and non-trending down.
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
