# Suggestions Observability

This document describes the end-to-end Suggestions pipeline and how to inspect it.

## End-to-end flow

```mermaid
flowchart TD
    A["User opens Cosmos / presses refresh"] --> B["UI requests refresh (background.rs)"]
    B --> C["Build fresh index + context"]
    C --> D["run_fast_grounded_with_gate(...)"]
    D --> E["Attempt loop (time/cost/attempt budget)"]
    E --> F["analyze_codebase_dual_agent_reviewed(...)"]
    F --> G["Build hybrid candidate file pool"]
    G --> H["Spawn dual workers (bug_hunter + security_reviewer)"]
    H --> I["Each worker calls call_llm_agentic_report_back_only(...)"]
    I --> J["Tool rounds (tree/read/search/head/shell/report_back)"]
    J --> K["report_back payload parsed + mapped to Suggestion[]"]
    K --> L["Merge worker findings"]
    L --> M["Create SuggestionDiagnostics + gate snapshot"]
    M --> N["BackgroundMessage::SuggestionsReady"]
    N --> O["UI replaces/sorts suggestions"]
    O --> P["Audit row appended to .cosmos/suggestion_runs.jsonl"]
    O --> Q["Per-suggestion quality telemetry appended as user verifies/applies"]
```

## What is now visible

- Run-level diagnostics in audit mode:
  - `cosmos --suggest-audit --suggest-runs 1 --suggest-trace`
- Live reasoning stream in audit mode:
  - `cosmos --suggest-audit --suggest-runs 1 --suggest-trace --suggest-stream-reasoning`
- Persistent run snapshots:
  - `.cosmos/suggestion_runs.jsonl`
- Per-worker trace notes (stored in diagnostics notes):
  - iterations, tool call count, report_back iteration
  - assistant/reasoning preview snippets (truncated)

## Reasoning visibility

By default, reasoning output is excluded from provider responses.

- Default: reasoning hidden (`include_reasoning=false`)
- Opt-in: set `COSMOS_INCLUDE_REASONING=1`
- Live stream opt-in: set `COSMOS_STREAM_REASONING=1` (or use `--suggest-stream-reasoning`)

Example:

```bash
COSMOS_INCLUDE_REASONING=1 cosmos --suggest-audit --suggest-runs 1 --suggest-trace
COSMOS_STREAM_REASONING=1 COSMOS_INCLUDE_REASONING=1 cosmos --suggest-audit --suggest-runs 1 --suggest-trace
```

Important limitation:
- You can only see provider-returned rationale fields.
- Hidden internal chain-of-thought is not exposed by the API.
