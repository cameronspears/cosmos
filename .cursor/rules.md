# Cursor Agent Rules (Project Defaults)

These rules are guardrails, not shackles. Avoid adding new rigid
constraints unless they clearly improve safety or user outcomes.

## Product intent
- This is an AI maintenance tool for non-engineers and technical users
  who want plain English and low friction.
- Prefer clarity and actionable guidance over technical detail in any
  user-facing text.

## Priorities
- Maintainability and safety first.
- Performance matters, but avoid premature optimization.

## Rust safety and reliability
- Prefer safe Rust. Avoid unsafe unless there is a clear, documented
  need.
- Avoid panics in user-facing flows. Do not use unwrap/expect in
  production paths unless the condition is truly impossible. Prefer
  Result with actionable errors.
- Keep warnings and lints clean. Do not add #[allow(dead_code)] in
  production code; remove dead code or wire it up.

## Error handling and self-healing
- Never leave users with an error they cannot act on.
- When possible, self-heal: retry, use fallbacks, and preserve progress.
- When failure is unavoidable, return a plain-English message with next
  steps.

## Readability and maintainability
- Prefer straightforward, readable code over cleverness.
- Keep functions small and cohesive; use descriptive names.
- Add comments only when logic is non-obvious.

## Dependencies
- Prefer existing dependencies in the repo.
- Add new crates only when they provide clear value; use latest versions.

## Testing
- Add tests for new behavior and bug fixes.
- For refactors without behavior change, rely on existing tests unless
  risk is high.

## UX text
- Plain English, avoid jargon.
- Provide actionable guidance and safe defaults.
