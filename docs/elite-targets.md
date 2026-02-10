# Elite Targets for Cosmos

Cosmos should feel dependable to non-engineers and to strong engineers.
"Elite" means a professional engineer can review Cosmos output and think:
"This is safe, correct, and well-executed."

This document defines the concrete targets Cosmos must meet to earn that trust.
It complements `ETHOS.md` by turning values into measurable standards.

## Scope of This Document

These targets apply to:

1. The **implementation pipeline** (turning validated suggestions into code changes).
2. The **apply experience** (safe, reversible, transparent).
3. The **lab + CI proof** that Cosmos works on real codebases, not just one repo.

Non-goal: changing suggestion generation/validation behavior. The suggestion engine is treated as upstream truth.

## Elite Means: Safety, Then Speed

Cosmos must be safe and honest even when the fastest path would be riskier.

Elite behavior:

1. **No hidden changes.** Cosmos never silently edits files.
2. **Scope is sacred.** Cosmos does not "fix the repo" outside the validated suggestion scope.
3. **Fail clearly.** If a gate cannot be met, Cosmos stops and explains why in plain language.
4. **Proof over vibes.** Cosmos shows evidence and diagnostics for what it did.

## Two Contexts: Interactive vs Lab/CI

Cosmos operates in two policy contexts with different strictness:

### Interactive ("apply" in the app)

Goal: keep the user moving while staying safe.

Rules:

1. If quick checks are unavailable, Cosmos may continue only with an explicit **reduced-confidence** warning.
2. Safety gates and review gates still block apply.
3. The user always remains in control with a clear review step.

### Lab/CI ("prove it works across repos")

Goal: produce proof that Cosmos is dependable.

Rules:

1. Quick checks must be **detectable** and must **pass**.
2. Every run must emit reports/telemetry.
3. Release gates must fail when elite bars are missed.

## Core Invariants (Non-Negotiable)

These must be true for every apply attempt and every lab run.

1. **No mutation on failure:** if an attempt fails, the real repo must have zero tracked-file mutations and zero unintended staging.
2. **Deterministic safety gates:** path traversal blocked, symlink writes blocked, binary writes blocked, out-of-scope file edits blocked.
3. **Syntax safety:** any "passed" attempt must leave all changed files parseable by Cosmos parsers (and must not introduce parse failures after review).
4. **Blocking review residuals are zero:** a "passed" attempt must have zero remaining blocking review findings under the configured severity policy.
5. **Transparency artifacts exist:** every run produces a report that explains what happened, with stable gate ids and reason codes.

## Elite Quality Bars (Measured)

These are the quantitative bars Cosmos must meet on a real corpus.

### Required Rates (Lab/CI)

1. **Blocking residual rate:** `0`
2. **Syntax-failure-after-pass rate:** `0`
3. **Mutation-on-failure rate:** `0`
4. **Overall pass rate:** `>= 0.90`
5. **First-attempt pass rate:** `>= 0.70`

### Required Budgets (Lab/CI)

1. **Average total cost per implemented suggestion:** `<= $0.015`
2. **Average total wall time per implemented suggestion:** `<= 35s`

Notes:

1. These bars are evaluated on a multi-repo corpus, not a single repo.
2. A run is only "elite pass" if both the aggregate bars and every repo-level gate passes.

## Definitions (What We Measure)

Cosmos reports these metrics for implement runs:

1. **pass_rate:** fraction of attempted implementations that fully passed all gates.
2. **first_attempt_pass_rate:** fraction of cases that pass on attempt 1 (no retries).
3. **avg_total_cost_usd:** average OpenRouter-reported cost per case.
4. **avg_total_ms:** average wall time per case.
5. **residual_blocking_rate:** fraction of cases with any blocking review findings remaining at the end.
6. **syntax_failure_after_pass_rate:** fraction of cases where syntax/parse fails after a "pass" decision (should be impossible).
7. **mutation_on_failure_rate:** fraction of failed cases that still left tracked mutations or staging in the primary checkout (should be impossible).

## Provider Reliability Targets (Latency and Rate Limits)

Users should not experience "hung" or unpredictably slow behavior due to a single upstream provider.

Targets:

1. **Primary provider preference:** Cerebras fp16 should serve the Speed tier in normal conditions.
2. **Fast fallback:** when Cerebras is slow/unavailable, Cosmos must fail over quickly to trusted providers.
3. **Circuit breaking:** repeated timeouts or rate limits should temporarily stop selecting a degraded provider during a run.
4. **Bounded retries:** no single LLM call may consume the full attempt budget without a clear, logged timeout reason.

Provider policy (Speed tier model `openai/gpt-oss-120b`):

1. `cerebras/fp16`
2. `deepinfra/turbo`
3. `groq`

## Implementation Harness Targets (Correctness and Cost)

The implementation harness is considered elite when:

1. It is **transactional**: attempts run in a sandbox, and the real repo is only written during an explicit finalization step.
2. It is **budget-aware inside attempts**: budgets are enforced before and after every LLM call, and the harness stops immediately once exhausted.
3. It is **repair-capable**: common failures (like quick check errors) are repaired in-attempt when in scope, without expanding scope.
4. It is **reviewed adversarially**: changes are verified with a bounded adversarial review, then auto-fixed within limits, then re-reviewed.
5. It produces **plain-language explanations** suitable for non-engineers.

## Transparency Targets (User Trust)

Every harness run must generate:

1. A JSON report with:
   1. run id
   2. attempt-level gate snapshots
   3. commands run and outcomes
   4. timing and cost
   5. finalization outcome (applied, rolled back, failed before finalize)
2. A JSONL telemetry row for longitudinal tracking with a schema version field.

User-facing messaging must always include:

1. a plain-language reason for failure
2. the report path when present
3. whether confidence was reduced (and why)

## Release Gate Targets (Proving It, Then Enforcing It)

Elite targets must be enforced in stages:

1. **Shadow gate:** run in CI on a schedule and report results without blocking merges.
2. **Enforced gate:** once stable, fail CI when elite bars are missed.

Exit criteria for "elite":

1. Two consecutive weeks of scheduled runs meeting elite bars with no safety regressions.
2. Regular spot-checks of reports confirm ETHOS-level plain language and honest confidence.

## How to Run the Proof Locally

The main operator command is the lab corpus run:

```bash
cargo run --bin cosmos-lab -- implement --corpus-manifest corpus/oss-corpus.toml --sync --sample-size 10
```

The output report is written under `.cosmos/lab/` and includes:

1. aggregate metrics vs elite bars
2. per-repo metrics
3. top failure clusters by `(gate, reason_code)` so iteration stays fast

