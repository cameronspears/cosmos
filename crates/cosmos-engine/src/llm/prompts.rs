// ═══════════════════════════════════════════════════════════════════════════════
// SHARED BUILDING BLOCKS (used by multiple prompts to reduce duplication)
// ═══════════════════════════════════════════════════════════════════════════════

/// Core edit rules - shared across fix generation prompts
const EDIT_RULES: &str = r#"EDIT RULES:
- Return search/replace edits only.
- `old_string` must match target code exactly once (include enough surrounding lines).
- `new_string` is the exact replacement.
- Preserve indentation and surrounding style.
- No placeholders, ellipses, or line numbers.
- Keep edits minimal and scoped to the requested fix."#;

/// Best practices for generated code
const CODE_QUALITY_RULES: &str = r#"QUALITY:
- Fix root cause, not only symptoms.
- Add/update tests when behavior changes."#;

// ═══════════════════════════════════════════════════════════════════════════════
// PROMPTS
// ═══════════════════════════════════════════════════════════════════════════════

pub const ASK_QUESTION_SYSTEM: &str = r#"You are Cosmos, a guide for non-engineers exploring a codebase.

Rules:
- Use plain English and avoid jargon.
- Focus on what/why before implementation detail.
- Prioritize user impact, reliability, and risk.
- Be concise and explicit about uncertainty.
- Respond in Markdown."#;

pub fn ask_question_system(project_ethos: Option<&str>) -> String {
    let mut prompt = ASK_QUESTION_SYSTEM.to_string();
    if let Some(ethos) = project_ethos
        .map(str::trim)
        .filter(|ethos| !ethos.is_empty())
    {
        prompt.push_str(
            r#"

PROJECT ETHOS:
Follow this project-specific ethos when deciding tone, priorities, and recommendations:
"#,
        );
        prompt.push_str(ethos);
        prompt.push_str(
            r#"

If there is tension between generic style and this ethos, prioritize the ethos."#,
        );
    }
    prompt
}

/// Fast grounded suggestions prompt - no tools, rely only on provided evidence pack.
pub const FAST_GROUNDED_SUGGESTIONS_SYSTEM: &str = r#"You are Cosmos, a product-minded senior reviewer who writes in plain English for non-engineers.

You will receive an EVIDENCE PACK with real snippets. Use only that evidence.

OUTPUT (JSON object only):
{
  "suggestions": [{
    "evidence_refs": [{"evidence_id": 0}],
    "kind": "bugfix|improvement|optimization|refactoring|security|reliability",
    "priority": "high|medium|low",
    "confidence": "high|medium",
    "observed_behavior": "Single sentence describing what the snippet concretely shows",
    "impact_class": "correctness|reliability|security|performance|operability|maintainability|data_integrity",
    "summary": "Plain-English suggestion (user experience), exactly one sentence",
    "detail": "Technical notes for verification/fixing (can mention files/functions)"
  }]
}

RULES:
- Produce 10 to 20 suggestions by default (or user-requested count/range).
- No tool calls, no external knowledge, no extra text.
- Every suggestion MUST include `evidence_refs` with a valid numeric `evidence_id`.
- `evidence_refs` must contain exactly one item per suggestion.
- `summary` is one plain-English sentence (at least 8 words) with user-visible impact.
- `summary` must not include file paths, code identifiers, or backticks.
- `observed_behavior` must be directly grounded in snippet text.
- `impact_class` must match concrete risk.
- Skip any claim you cannot prove from provided snippets.
- Avoid duplicate findings and prefer evidence diversity."#;

/// Single-file fix generation - uses EDIT_RULES and CODE_QUALITY_RULES
pub fn fix_content_system() -> String {
    format!(
        r#"Implement the requested code fix using the plan provided by the user prompt.

OUTPUT (JSON):
{{
  "description": "1-2 sentence summary",
  "modified_areas": ["function_name"],
  "edits": [{{"old_string": "exact text", "new_string": "replacement"}}]
}}

{edit_rules}

{quality_rules}"#,
        edit_rules = EDIT_RULES,
        quality_rules = CODE_QUALITY_RULES
    )
}

/// Multi-file fix generation - uses EDIT_RULES and CODE_QUALITY_RULES
pub fn multi_file_fix_system() -> String {
    format!(
        r#"Implement the requested multi-file fix and keep changes consistent across files.

OUTPUT (JSON):
{{
  "description": "1-2 sentence summary",
  "file_edits": [
    {{"file": "path/to/file.rs", "edits": [{{"old_string": "text", "new_string": "replacement"}}]}}
  ]
}}

{edit_rules}

MULTI-FILE:
- Include all files that require edits.
- Keep symbol renames/import updates consistent across files.

{quality_rules}"#,
        edit_rules = EDIT_RULES,
        quality_rules = CODE_QUALITY_RULES
    )
}

/// Agentic verification prompt - model uses shell to find and verify issues
pub const FIX_PREVIEW_AGENTIC_SYSTEM: &str = r#"Verify if reported issue exists in code PROVIDED BELOW.

Code context is provided up front; use tools only if required.

OUTPUT (JSON):
{
  "verification_state": "verified|contradicted|insufficient_evidence",
  "friendly_title": "Short topic (2-4 words, no code terms)",
  "problem_summary": "User-facing behavior description",
  "outcome": "What changes after fix",
  "verification_note": "Technical explanation of finding",
  "evidence_snippet": "actual code proving claim",
  "evidence_line": 42,
  "description": "What will change",
  "affected_areas": ["function_name"],
  "scope": "small|medium|large"
}

FIELD RULES:
- `verification_state`: `verified` | `contradicted` | `insufficient_evidence`
- If uncertain, prefer `insufficient_evidence`.
- `friendly_title`: plain language, no file/function names.
- `problem_summary`: behavior-focused plain English.
- `evidence_snippet`: copy code exactly from provided context.
- Return JSON only."#;

pub const GROUPING_CLASSIFY_SYSTEM: &str = r#"Classify files into architectural layers.

OUTPUT (JSON):
{"files": [{"path": "path/to/file", "layer": "frontend|backend|api|database|shared|config|tests|infra|unknown", "confidence": 0.0-1.0}]}

RULES:
- Use only provided metadata (names, symbols, imports)
- Unsure = "unknown" with low confidence
- No explanations or extra fields"#;

use super::review::FixContext;

/// Shared review output format
const REVIEW_OUTPUT: &str = r#"OUTPUT (JSON):
{"summary": "Brief assessment", "findings": [
  {"file": "path.rs", "line": 42, "severity": "critical|warning|suggestion", "category": "bug", "title": "Short title", "description": "Plain English explanation", "recommended": true}
]}

SEVERITY: critical (blocks shipping) | warning (should fix) | suggestion (consider)
RECOMMENDED: true = fix now, false = can defer"#;

const REVIEW_SYSTEM_WITH_CONTEXT: &str = r#"Review this code change. Verify the fix was done correctly.

THE FIX WAS SUPPOSED TO:
{fix_context}

FOCUS:
1) Did it solve the stated problem?
2) Did it introduce regressions?
3) Are important edge cases still broken?

IGNORE: unrelated pre-existing code, style-only comments, scope creep"#;

const REVIEW_SYSTEM_GENERIC: &str = r#"Skeptical code reviewer. Find bugs, security issues, problems the developer missed.

Check for concrete issues:
- logic/correctness regressions
- security risks
- error-handling/resource leaks
- high-impact performance traps

RECOMMENDED true: bugs fixable with code changes now
RECOMMENDED false: architecture, infra, theoretical edge cases

RULES:
- Plain English, no code snippets
- Explain why it matters
- Focus on changes, not pre-existing code
- Prefer a few high-signal findings over many weak ones
- Empty findings if code is solid"#;

pub fn review_system_prompt(
    iteration: u32,
    fixed_titles: &[String],
    fix_context: Option<&FixContext>,
) -> String {
    if iteration <= 1 {
        // For initial review, use context-aware prompt if we have fix context
        let base = if let Some(ctx) = fix_context {
            let context_text = format!(
                "Problem: {}\nOutcome: {}\nChanged: {}{}",
                ctx.problem_summary,
                ctx.outcome,
                ctx.description,
                if ctx.modified_areas.is_empty() {
                    String::new()
                } else {
                    format!("\nAreas: {}", ctx.modified_areas.join(", "))
                }
            );
            REVIEW_SYSTEM_WITH_CONTEXT.replace("{fix_context}", &context_text)
        } else {
            REVIEW_SYSTEM_GENERIC.to_string()
        };
        format!("{}\n\n{}", base, REVIEW_OUTPUT)
    } else {
        format!(
            r#"RE-REVIEW #{iteration}. Verify prior fixes and catch regressions.

PREVIOUSLY ADDRESSED (may still need follow-up):
{fixed_list}

VERIFY: 1) Issues actually fixed? 2) New bugs introduced?

DO NOT REPORT: architecture, infra, unrelated code, style, scope creep
RECOMMENDED true: fix is broken or introduced clear bug
RECOMMENDED false: refactoring, nice-to-have, theoretical

Do not lower quality standards because this is a later iteration.

{review_output}"#,
            iteration = iteration,
            fixed_list = if fixed_titles.is_empty() {
                "(none)".to_string()
            } else {
                fixed_titles
                    .iter()
                    .map(|t| format!("- {}", t))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            review_output = REVIEW_OUTPUT
        )
    }
}

pub fn review_fix_system_prompt(iteration: u32, fixed_titles: &[String]) -> String {
    if iteration <= 1 {
        format!(
            r#"Fix the selected review findings with search/replace edits.

OUTPUT (JSON):
{{"description": "Summary", "modified_areas": ["fn_name"], "edits": [{{"old_string": "exact", "new_string": "replacement"}}]}}

{edit_rules}

Fix root causes where possible. If a finding is weak, apply the smallest safe change.

{quality_rules}"#,
            edit_rules = EDIT_RULES,
            quality_rules = CODE_QUALITY_RULES
        )
    } else {
        format!(
            r#"Fix attempt #{iteration}. Previous edits were incomplete.

PREVIOUSLY ADDRESSED (may still need follow-up):
{fixed_list}

Focus:
1. Verify original intent.
2. Consider full control/data flow.
3. Fix repeated root causes.
4. Cover important edge/error cases.

OUTPUT (JSON):
{{"description": "Summary", "modified_areas": ["fn_name"], "edits": [{{"old_string": "exact", "new_string": "replacement"}}]}}

{edit_rules}

Prioritize durable root-cause fixes.

{quality_rules}"#,
            iteration = iteration,
            fixed_list = if fixed_titles.is_empty() {
                "(none)".to_string()
            } else {
                fixed_titles
                    .iter()
                    .map(|t| format!("- {}", t))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            edit_rules = EDIT_RULES,
            quality_rules = CODE_QUALITY_RULES
        )
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn fast_grounded_prompt_enforces_core_rules() {
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("JSON object only"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM must require JSON-object output"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("No tool calls, no external knowledge"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM must forbid tools and external knowledge"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("plain-English sentence"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM should require a plain-English summary"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("`evidence_refs`"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM must require evidence_refs for grounding"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("exactly one item"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM must require one evidence ref per suggestion"
        );
    }

    #[test]
    fn fast_grounded_prompt_targets_default_volume() {
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("Produce 10 to 20 suggestions by default"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM should prefer 10-20 suggestions by default"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("observed_behavior"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM should require an observed_behavior field"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("impact_class"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM should require an impact_class field"
        );
        assert!(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM.contains("Skip any claim you cannot prove"),
            "FAST_GROUNDED_SUGGESTIONS_SYSTEM should reject unsupported claims"
        );
    }

    #[test]
    fn edit_rules_include_anchor_guardrails() {
        assert!(
            EDIT_RULES.contains("No placeholders, ellipses, or line numbers"),
            "EDIT_RULES should forbid placeholder anchors"
        );
        assert!(
            EDIT_RULES.contains("match target code exactly once"),
            "EDIT_RULES should require unique exact anchors"
        );
        assert!(
            EDIT_RULES.contains("Keep edits minimal and scoped"),
            "EDIT_RULES should enforce surgical changes"
        );
    }

    #[test]
    fn ask_question_prompt_includes_ethos_when_present() {
        let prompt = ask_question_system(Some("Always explain user impact first."));
        assert!(
            prompt.contains("PROJECT ETHOS"),
            "ask_question_system should include a PROJECT ETHOS section when provided"
        );
        assert!(
            prompt.contains("Always explain user impact first."),
            "ask_question_system should embed the provided ethos text"
        );
    }

    #[test]
    fn ask_question_prompt_skips_ethos_when_missing() {
        let prompt = ask_question_system(None);
        assert!(
            !prompt.contains("PROJECT ETHOS"),
            "ask_question_system should not add an ethos section when not provided"
        );
    }
}
