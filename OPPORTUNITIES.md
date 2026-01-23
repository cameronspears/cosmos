# Cosmos Repository - Top 10 Prioritized Opportunities

This document outlines the most important improvement opportunities for the Cosmos codebase, sorted by priority.

---

## 1. 🔴 CRITICAL: Incomplete Reset Feature Causes Compilation Failure

**Priority:** Critical  
**Files:** `src/app/input/overlay.rs`, `src/ui/types.rs`, `src/app/messages.rs`, `src/cache/mod.rs`  
**Impact:** The codebase does not compile

**Problem:**
The "Reset Cosmos" overlay feature in `overlay.rs` references code that doesn't exist:
- `LoadingState::Resetting` - variant not defined in `types.rs`
- `BackgroundMessage::ResetComplete` - variant not defined in `messages.rs`
- `crate::cache::reset_cosmos()` - async function not defined in `cache/mod.rs`

**Evidence (overlay.rs:88-102):**
```rust
app.loading = LoadingState::Resetting;  // Does not exist
// ...
match crate::cache::reset_cosmos(&selected).await {  // Does not exist
    Ok(_) => {
        let _ = tx_reset.send(BackgroundMessage::ResetComplete { options: selected });  // Does not exist
    }
```

**Resolution:**
1. Add `Resetting` variant to `LoadingState` enum in `types.rs`
2. Add `ResetComplete { options: Vec<ResetOption> }` variant to `BackgroundMessage` in `messages.rs`
3. Implement `pub async fn reset_cosmos(options: &[ResetOption]) -> anyhow::Result<()>` in `cache/mod.rs`
4. Add message handling in `background.rs` for `ResetComplete`

---

## 2. 🔴 HIGH: Large UI Module Approaching Size Limit

**Priority:** High  
**File:** `src/ui/mod.rs` (1,335 lines)  
**Impact:** Maintainability, readability

**Problem:**
Per project guidelines, files should not exceed ~1,500 lines. The UI module at 1,335 lines is approaching this limit and contains mixed concerns:
- App state struct definition and all its methods
- Workflow navigation logic
- PR content generation
- Overlay management
- Search/filter logic

**Resolution:**
Split into focused modules:
- `src/ui/app.rs` - Core App struct and basic state management
- `src/ui/workflow.rs` - Workflow step navigation (Suggestions → Verify → Review → Ship)
- `src/ui/overlays.rs` - Overlay state management
- `src/ui/search.rs` - Search and filter logic
- `src/ui/commits.rs` - Commit message and PR content generation

---

## 3. 🔴 HIGH: Overly Complex Input Handler with Deep Nesting

**Priority:** High  
**File:** `src/app/input/normal.rs` (909 lines)  
**Impact:** Maintainability, testability, readability

**Problem:**
The `handle_normal_mode` function contains deeply nested async closures, especially for the Apply Fix workflow (lines 264-690). The nesting reaches 7+ levels in some places, making it extremely difficult to:
- Understand the control flow
- Add proper error handling
- Write unit tests
- Debug issues

**Evidence (lines 290-550):**
```rust
background::spawn_background(ctx.tx.clone(), "apply_fix", async move {
    // 200+ lines of nested async code with multiple match arms
    // Error handling duplicated across multi-file and single-file paths
});
```

**Resolution:**
1. Extract fix application logic into dedicated functions in a separate module (e.g., `src/app/fix_workflow.rs`)
2. Create helper functions for common patterns:
   - `apply_single_file_fix()`
   - `apply_multi_file_fix()`
   - `backup_and_restore()`
3. Use early returns and guard clauses to reduce nesting

---

## 4. 🟡 MEDIUM: Duplicate Code for Review Fix Handling

**Priority:** Medium  
**File:** `src/app/input/normal.rs`  
**Impact:** DRY principle violation, maintenance burden

**Problem:**
The logic for fixing review findings is duplicated in two places:
1. `KeyCode::Char('f')` handler (lines 84-143)
2. `KeyCode::Enter` handler in `WorkflowStep::Review` (lines 693-755)

Both implementations are nearly identical, spawning the same background task with the same parameters.

**Resolution:**
Extract into a shared helper function:
```rust
fn spawn_review_fix_task(app: &mut App, ctx: &RuntimeContext) -> Result<()> {
    // Common implementation
}
```

---

## 5. 🟡 MEDIUM: Unused Import Warning

**Priority:** Medium  
**File:** `src/app/input/normal.rs:12`  
**Impact:** Code cleanliness, compiler warnings

**Problem:**
```rust
use std::collections::HashMap;  // Unused
```

**Resolution:**
Remove the unused import to eliminate compiler warnings.

---

## 6. 🟡 MEDIUM: Limited Test Coverage

**Priority:** Medium  
**Files:** Various  
**Impact:** Reliability, regression prevention

**Problem:**
Test coverage is minimal:
- `src/suggest/mod.rs` - 2 basic tests
- `src/index/mod.rs` - 5 tests
- `src/config.rs` - 1 test
- `src/git_ops.rs` - 4 tests
- `src/util.rs` - 2 tests

Key untested areas:
- LLM client retry logic
- Cache operations (file locking, atomic writes)
- Background message handling
- Workflow state transitions

**Resolution:**
Add tests for:
1. `client.rs` - Mock HTTP responses for retry logic
2. `cache/mod.rs` - File locking edge cases
3. `background.rs` - Message handling state transitions
4. UI state machine transitions

---

## 7. 🟡 MEDIUM: Legacy Migration Code with Scheduled Removal

**Priority:** Medium  
**File:** `src/config.rs:142-160`  
**Impact:** Technical debt

**Problem:**
```rust
// TODO: Remove this migration code after 2026-06-01 (6 months from keychain release)
if let Some(key) = self.openrouter_api_key.clone() {
    eprintln!("  Migrating API key from config file to system keychain...");
    // ... migration logic
```

The current date is January 2026 - this migration code is approaching its scheduled removal date.

**Resolution:**
Track this for removal in June 2026 or confirm all users have migrated.

---

## 8. 🟢 LOW: Inconsistent Error Handling with unwrap()

**Priority:** Low  
**Files:** Multiple (10 occurrences)  
**Impact:** Potential panics in edge cases

**Problem:**
While the codebase is generally good about using `Result`, there are 10 remaining `unwrap()`/`expect()` calls:
- `src/suggest/llm/parse.rs` - 2 occurrences
- `src/index/parser.rs` - 2 occurrences
- `src/index/mod.rs` - 3 occurrences
- `src/git_ops.rs` - 2 occurrences
- `src/context/mod.rs` - 1 occurrence

Most are in test code or truly impossible conditions, but should be audited.

**Resolution:**
Audit each occurrence and:
- Keep if truly impossible (document why)
- Replace with `.ok_or_else()` for potentially failing conditions

---

## 9. 🟢 LOW: Missing Feature - Ritual Mode

**Priority:** Low  
**File:** README.md mentions it, but implementation not found  
**Impact:** Feature gap

**Problem:**
The README documents a "ritual mode" feature:
```bash
cosmos ritual --minutes 10
```

But the CLI args in `main.rs` only have:
- `path`
- `--setup`
- `--stats`

The ritual mode feature appears to be documented but not implemented.

**Resolution:**
Either implement the ritual mode or remove it from documentation.

---

## 10. 🟢 LOW: Background Task Naming Inconsistency

**Priority:** Low  
**File:** `src/app/runtime.rs`, `src/app/input/normal.rs`  
**Impact:** Debugging, observability

**Problem:**
Background tasks are spawned with names like:
- `"summary_generation"`
- `"suggestions_generation"`
- `"preview_generation"`
- `"apply_fix"`
- `"ship_confirm"`
- `"verification_fix"`

These names are used for logging but have no consistent naming convention.

**Resolution:**
Establish a naming convention (e.g., `{module}_{action}`) and add documentation.

---

## Summary by Priority

| Priority | Count | Description |
|----------|-------|-------------|
| 🔴 Critical | 1 | Compilation failure - must fix |
| 🔴 High | 2 | Maintainability blockers |
| 🟡 Medium | 4 | Code quality issues |
| 🟢 Low | 3 | Nice-to-have improvements |

## Recommended Action Order

1. **Immediate:** Fix compilation errors (#1)
2. **This sprint:** Address high-priority items (#2, #3)
3. **Next sprint:** Clean up medium-priority items (#4, #5, #6, #7)
4. **Backlog:** Low-priority improvements (#8, #9, #10)
