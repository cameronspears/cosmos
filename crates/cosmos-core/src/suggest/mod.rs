//! Suggestion engine for Cosmos
//!
//! LLM-driven suggestions with a hard cap to avoid overwhelming users.
//! Suggestions are generated on-demand via `analyze_codebase()`.

/// Maximum suggestions to display to avoid overwhelming users
const MAX_SUGGESTIONS: usize = 30;

use crate::index::CodebaseIndex;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Source of a suggestion
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionSource {
    /// Pattern matching, no LLM cost
    Static,
    /// Previously generated, loaded from cache
    Cached,
    /// Grok Fast for quick categorization
    LlmFast,
    /// LLM for detailed analysis
    LlmDeep,
}

/// Kind of suggestion
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SuggestionKind {
    /// Code improvement/refactoring
    Improvement,
    /// Potential bug fix
    BugFix,
    /// New feature suggestion
    Feature,
    /// Performance optimization
    Optimization,
    /// Code quality/maintainability
    Quality,
    /// Documentation improvement
    Documentation,
    /// Test coverage
    Testing,
    /// Code refactoring (extract, rename, restructure)
    Refactoring,
}

impl SuggestionKind {
    pub fn label(&self) -> &'static str {
        match self {
            SuggestionKind::Improvement => "Improve",
            SuggestionKind::BugFix => "Fix",
            SuggestionKind::Feature => "Feature",
            SuggestionKind::Optimization => "Speed",
            SuggestionKind::Quality => "Stability",
            SuggestionKind::Documentation => "Guidance",
            SuggestionKind::Testing => "Safety",
            SuggestionKind::Refactoring => "Cleanup",
        }
    }
}

/// Priority level
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
}

/// Confidence level for suggestions
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum Confidence {
    Low,
    #[default]
    Medium,
    High,
}

/// Verification state for a suggestion (explicit verify contract)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    #[default]
    Unverified,
    Verified,
    Contradicted,
    InsufficientEvidence,
}

/// Validation lifecycle state for suggestion quality refinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionValidationState {
    #[default]
    Pending,
    Validated,
    Rejected,
}

/// Deterministic metadata captured from evidence sampling for validation context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SuggestionValidationMetadata {
    #[serde(default)]
    pub why_interesting: Option<String>,
    #[serde(default)]
    pub file_loc: Option<usize>,
    #[serde(default)]
    pub file_complexity: Option<f64>,
    #[serde(default)]
    pub anchor_context: Option<String>,
    #[serde(default)]
    pub evidence_quality_score: Option<f64>,
    #[serde(default)]
    pub snippet_comment_ratio: Option<f64>,
    #[serde(default)]
    pub snippet_top_comment_ratio: Option<f64>,
    #[serde(default)]
    pub claim_observed_behavior: Option<String>,
    #[serde(default)]
    pub claim_impact_class: Option<String>,
}

/// A concrete evidence reference backing a suggestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionEvidenceRef {
    /// Stable snippet/evidence item ID from the evidence pack
    pub snippet_id: usize,
    /// Repo-relative file path for the evidence
    pub file: PathBuf,
    /// 1-based line where the evidence is anchored
    pub line: usize,
}

/// A suggestion for improvement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: Uuid,
    pub kind: SuggestionKind,
    pub priority: Priority,
    /// Confidence level (high = verified by reading code, medium = likely, low = uncertain)
    #[serde(default)]
    pub confidence: Confidence,
    /// Primary file (used for display/grouping)
    pub file: PathBuf,
    /// Additional files affected by this suggestion (for multi-file refactors)
    #[serde(default)]
    pub additional_files: Vec<PathBuf>,
    pub line: Option<usize>,
    pub summary: String,
    pub detail: Option<String>,
    /// Raw code snippet proving the issue (used for grounding and UI citations).
    #[serde(default)]
    pub evidence: Option<String>,
    /// Structured evidence references tied to real file/line/snippet IDs.
    #[serde(default)]
    pub evidence_refs: Vec<SuggestionEvidenceRef>,
    /// Explicit verification contract state.
    #[serde(default)]
    pub verification_state: VerificationState,
    /// Suggestion validation state used by two-stage refinement.
    #[serde(default)]
    pub validation_state: SuggestionValidationState,
    /// Deterministic implementation-readiness score (0.0-1.0) used by harness gating.
    #[serde(default)]
    pub implementation_readiness_score: Option<f32>,
    /// Deterministic risk flags explaining readiness penalties.
    #[serde(default)]
    pub implementation_risk_flags: Vec<String>,
    /// Plain-language implementation sketch consumed by harness prompts.
    #[serde(default)]
    pub implementation_sketch: Option<String>,
    /// Deterministic evidence metadata carried into validation prompts.
    #[serde(default)]
    pub validation_metadata: SuggestionValidationMetadata,
    pub source: SuggestionSource,
    pub created_at: DateTime<Utc>,
    /// Whether the suggestion has been applied
    pub applied: bool,
}

impl Suggestion {
    pub fn new(
        kind: SuggestionKind,
        priority: Priority,
        file: PathBuf,
        summary: String,
        source: SuggestionSource,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            priority,
            confidence: Confidence::default(),
            file,
            additional_files: Vec::new(),
            line: None,
            summary,
            detail: None,
            evidence: None,
            evidence_refs: Vec::new(),
            verification_state: VerificationState::Unverified,
            validation_state: SuggestionValidationState::Pending,
            implementation_readiness_score: None,
            implementation_risk_flags: Vec::new(),
            implementation_sketch: None,
            validation_metadata: SuggestionValidationMetadata::default(),
            source,
            created_at: Utc::now(),
            applied: false,
        }
    }

    pub fn with_confidence(mut self, confidence: Confidence) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn with_detail(mut self, detail: String) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn with_evidence(mut self, evidence: String) -> Self {
        self.evidence = Some(evidence);
        self
    }

    pub fn with_evidence_refs(mut self, evidence_refs: Vec<SuggestionEvidenceRef>) -> Self {
        self.evidence_refs = evidence_refs;
        self
    }

    pub fn with_validation_state(mut self, validation_state: SuggestionValidationState) -> Self {
        self.validation_state = validation_state;
        self
    }

    pub fn with_verification_state(mut self, verification_state: VerificationState) -> Self {
        self.verification_state = verification_state;
        self
    }

    pub fn with_implementation_readiness_score(mut self, score: f32) -> Self {
        self.implementation_readiness_score = Some(score.clamp(0.0, 1.0));
        self
    }

    pub fn with_implementation_risk_flags(mut self, flags: Vec<String>) -> Self {
        self.implementation_risk_flags = flags;
        self
    }

    pub fn with_implementation_sketch(mut self, sketch: String) -> Self {
        self.implementation_sketch = Some(sketch);
        self
    }

    pub fn with_validation_metadata(mut self, metadata: SuggestionValidationMetadata) -> Self {
        self.validation_metadata = metadata;
        self
    }

    /// Get all files affected by this suggestion (primary + additional)
    pub fn affected_files(&self) -> Vec<&PathBuf> {
        std::iter::once(&self.file)
            .chain(self.additional_files.iter())
            .collect()
    }

    /// Check if this is a multi-file suggestion
    pub fn is_multi_file(&self) -> bool {
        !self.additional_files.is_empty()
    }

    /// Get the total number of files affected
    pub fn file_count(&self) -> usize {
        1 + self.additional_files.len()
    }
}

/// The suggestion engine
pub struct SuggestionEngine {
    pub suggestions: Vec<Suggestion>,
    pub index: CodebaseIndex,
}

impl SuggestionEngine {
    fn update_suggestion<F>(&mut self, id: Uuid, mut update: F)
    where
        F: FnMut(&mut Suggestion),
    {
        if let Some(suggestion) = self.suggestions.iter_mut().find(|s| s.id == id) {
            update(suggestion);
        }
    }

    fn sort_by_priority_desc(&mut self) {
        self.suggestions.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    fn kind_weight(kind: SuggestionKind) -> i64 {
        match kind {
            SuggestionKind::BugFix => 40,
            SuggestionKind::Refactoring => 30,
            SuggestionKind::Optimization => 25,
            SuggestionKind::Testing => 20,
            SuggestionKind::Quality => 15,
            SuggestionKind::Documentation => 10,
            SuggestionKind::Improvement => 10,
            SuggestionKind::Feature => 0,
        }
    }

    fn evidence_penalty(
        suggestion: &Suggestion,
        contradicted_evidence_counts: Option<&std::collections::HashMap<usize, usize>>,
    ) -> usize {
        let evidence_id = suggestion.evidence_refs.first().map(|r| r.snippet_id);
        evidence_id
            .and_then(|id| contradicted_evidence_counts.and_then(|m| m.get(&id).copied()))
            .unwrap_or(0)
    }

    /// Create a new suggestion engine from a codebase index
    ///
    /// Starts empty - LLM suggestions are generated separately.
    pub fn new(index: CodebaseIndex) -> Self {
        Self {
            suggestions: Vec::new(),
            index,
        }
    }

    /// Get all active suggestions (not yet applied), capped at MAX_SUGGESTIONS.
    pub fn active_suggestions(&self) -> Vec<&Suggestion> {
        self.active_suggestions_with_limit(MAX_SUGGESTIONS)
    }

    /// Get active suggestions (not yet applied), capped by caller limit and MAX_SUGGESTIONS.
    pub fn active_suggestions_with_limit(&self, limit: usize) -> Vec<&Suggestion> {
        if limit == 0 {
            return Vec::new();
        }
        let cap = limit.min(MAX_SUGGESTIONS);
        self.suggestions
            .iter()
            .filter(|s| !s.applied)
            .take(cap)
            .collect()
    }

    /// Mark a suggestion as applied
    pub fn mark_applied(&mut self, id: Uuid) {
        self.update_suggestion(id, |s| s.applied = true);
    }

    /// Mark a suggestion as not applied (used for undo).
    pub fn unmark_applied(&mut self, id: Uuid) {
        self.update_suggestion(id, |s| s.applied = false);
    }

    /// Add a suggestion from LLM
    pub fn add_llm_suggestion(&mut self, suggestion: Suggestion) {
        self.suggestions.push(suggestion);
        self.sort_by_priority_desc();
    }

    /// Replace provisional LLM suggestions with refined suggestions.
    ///
    /// Keeps non-LLM suggestions and already-applied suggestions intact.
    pub fn replace_llm_suggestions(&mut self, mut suggestions: Vec<Suggestion>) {
        self.suggestions
            .retain(|s| s.source != SuggestionSource::LlmDeep || s.applied);
        self.suggestions.append(&mut suggestions);
        self.sort_by_priority_desc();
    }

    /// Sort suggestions by priority first, then confidence and contradiction history,
    /// then git context (changed files, blast radius).
    pub fn sort_with_context(
        &mut self,
        context: &crate::context::WorkContext,
        contradicted_evidence_counts: Option<&std::collections::HashMap<usize, usize>>,
    ) {
        let changed: std::collections::HashSet<PathBuf> =
            context.all_changed_files().into_iter().cloned().collect();

        // “Blast radius” = files that import changed files (and direct deps of changed files).
        let mut blast: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for path in &changed {
            if let Some(file_index) = self.index.files.get(path) {
                for u in &file_index.summary.used_by {
                    blast.insert(u.clone());
                }
                for d in &file_index.summary.depends_on {
                    blast.insert(d.clone());
                }
            }
        }
        for c in &changed {
            blast.remove(c);
        }

        self.suggestions.sort_by(|a, b| {
            // Priority is the primary sort criterion
            let pri = b.priority.cmp(&a.priority);
            if pri != std::cmp::Ordering::Equal {
                return pri;
            }

            // Higher confidence suggestions should surface first.
            let conf = b.confidence.cmp(&a.confidence);
            if conf != std::cmp::Ordering::Equal {
                return conf;
            }

            // Suggestions tied to recently contradicted evidence are demoted.
            let a_penalty = Self::evidence_penalty(a, contradicted_evidence_counts);
            let b_penalty = Self::evidence_penalty(b, contradicted_evidence_counts);
            if a_penalty != b_penalty {
                return a_penalty.cmp(&b_penalty);
            }

            // Then kind weight
            let kw = Self::kind_weight(b.kind).cmp(&Self::kind_weight(a.kind));
            if kw != std::cmp::Ordering::Equal {
                return kw;
            }

            // Git context is a *weak* tie-breaker: it helps relevance, but shouldn't
            // dominate results when users want broader codebase improvements.
            let a_changed = changed.contains(&a.file);
            let b_changed = changed.contains(&b.file);
            if a_changed != b_changed {
                return b_changed.cmp(&a_changed);
            }

            let a_blast = blast.contains(&a.file);
            let b_blast = blast.contains(&b.file);
            if a_blast != b_blast {
                return b_blast.cmp(&a_blast);
            }

            // Finally: newest first
            b.created_at.cmp(&a.created_at)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::High > Priority::Medium);
        assert!(Priority::Medium > Priority::Low);
    }

    #[test]
    fn test_suggestion_creation() {
        let suggestion = Suggestion::new(
            SuggestionKind::Improvement,
            Priority::High,
            PathBuf::from("test.rs"),
            "Test suggestion".to_string(),
            SuggestionSource::Static,
        );

        assert!(!suggestion.applied);
    }

    #[test]
    fn test_suggestion_deserialize_without_evidence_is_backward_compatible() {
        let suggestion = Suggestion::new(
            SuggestionKind::BugFix,
            Priority::High,
            PathBuf::from("src/lib.rs"),
            "Example".to_string(),
            SuggestionSource::LlmDeep,
        )
        .with_line(3)
        .with_detail("Details".to_string());

        let mut value = serde_json::to_value(&suggestion).unwrap();
        if let Value::Object(map) = &mut value {
            map.remove("evidence");
            map.remove("evidence_refs");
            map.remove("verification_state");
            map.remove("validation_state");
            map.remove("validation_metadata");
        }

        let round: Suggestion = serde_json::from_value(value).unwrap();
        assert!(round.evidence.is_none());
        assert!(round.evidence_refs.is_empty());
        assert_eq!(round.verification_state, VerificationState::Unverified);
        assert_eq!(round.validation_state, SuggestionValidationState::Pending);
        assert!(round.validation_metadata.why_interesting.is_none());
    }

    #[test]
    fn test_active_suggestions_with_limit_caps_and_keeps_wrapper_compatibility() {
        let index = CodebaseIndex {
            root: PathBuf::from("."),
            files: std::collections::HashMap::new(),
            index_errors: Vec::new(),
            git_head: None,
        };
        let mut engine = SuggestionEngine::new(index);
        for i in 0..40 {
            engine.add_llm_suggestion(Suggestion::new(
                SuggestionKind::Improvement,
                Priority::Medium,
                PathBuf::from(format!("src/file_{}.rs", i)),
                format!("Suggestion {}", i),
                SuggestionSource::LlmDeep,
            ));
        }

        let capped_30 = engine.active_suggestions_with_limit(30);
        assert_eq!(capped_30.len(), 30);

        let capped_overflow = engine.active_suggestions_with_limit(99);
        assert_eq!(capped_overflow.len(), 30);

        let wrapper = engine.active_suggestions();
        assert_eq!(wrapper.len(), 30);
    }

    #[test]
    fn test_active_suggestions_with_limit_zero_returns_empty() {
        let index = CodebaseIndex {
            root: PathBuf::from("."),
            files: std::collections::HashMap::new(),
            index_errors: Vec::new(),
            git_head: None,
        };
        let mut engine = SuggestionEngine::new(index);
        engine.add_llm_suggestion(Suggestion::new(
            SuggestionKind::Improvement,
            Priority::Medium,
            PathBuf::from("src/lib.rs"),
            "Suggestion".to_string(),
            SuggestionSource::LlmDeep,
        ));
        assert!(engine.active_suggestions_with_limit(0).is_empty());
    }

    #[test]
    fn test_sort_with_context_prefers_kind_over_changed() {
        let index = CodebaseIndex {
            root: PathBuf::from("."),
            files: std::collections::HashMap::new(),
            index_errors: Vec::new(),
            git_head: None,
        };

        let mut engine = SuggestionEngine::new(index);

        // Same priority; one is in a changed file but lower kind weight.
        let changed_doc = Suggestion::new(
            SuggestionKind::Documentation,
            Priority::High,
            PathBuf::from("src/changed.rs"),
            "Docs".to_string(),
            SuggestionSource::Static,
        );
        let unchanged_bug = Suggestion::new(
            SuggestionKind::BugFix,
            Priority::High,
            PathBuf::from("src/other.rs"),
            "Bug".to_string(),
            SuggestionSource::Static,
        );

        engine.suggestions = vec![changed_doc, unchanged_bug];

        let context = crate::context::WorkContext {
            branch: "test".to_string(),
            uncommitted_files: vec![PathBuf::from("src/changed.rs")],
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 1,
            repo_root: PathBuf::from("."),
        };

        engine.sort_with_context(&context, None);
        assert_eq!(engine.suggestions[0].kind, SuggestionKind::BugFix);
    }

    #[test]
    fn test_sort_with_context_prefers_higher_confidence_after_priority() {
        let index = CodebaseIndex {
            root: PathBuf::from("."),
            files: std::collections::HashMap::new(),
            index_errors: Vec::new(),
            git_head: None,
        };
        let mut engine = SuggestionEngine::new(index);
        let high = Suggestion::new(
            SuggestionKind::Improvement,
            Priority::High,
            PathBuf::from("src/high.rs"),
            "High confidence".to_string(),
            SuggestionSource::LlmDeep,
        )
        .with_confidence(Confidence::High);
        let medium = Suggestion::new(
            SuggestionKind::Improvement,
            Priority::High,
            PathBuf::from("src/medium.rs"),
            "Medium confidence".to_string(),
            SuggestionSource::LlmDeep,
        )
        .with_confidence(Confidence::Medium);
        engine.suggestions = vec![medium, high];
        let context = crate::context::WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: PathBuf::from("."),
        };
        engine.sort_with_context(&context, None);
        assert_eq!(engine.suggestions[0].summary, "High confidence");
    }

    #[test]
    fn test_sort_with_context_demotes_contradicted_evidence() {
        let index = CodebaseIndex {
            root: PathBuf::from("."),
            files: std::collections::HashMap::new(),
            index_errors: Vec::new(),
            git_head: None,
        };
        let mut engine = SuggestionEngine::new(index);
        let contradicted = Suggestion::new(
            SuggestionKind::Improvement,
            Priority::High,
            PathBuf::from("src/contradicted.rs"),
            "Contradicted evidence".to_string(),
            SuggestionSource::LlmDeep,
        )
        .with_confidence(Confidence::High)
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 7,
            file: PathBuf::from("src/contradicted.rs"),
            line: 10,
        }]);
        let clean = Suggestion::new(
            SuggestionKind::Improvement,
            Priority::High,
            PathBuf::from("src/clean.rs"),
            "Clean evidence".to_string(),
            SuggestionSource::LlmDeep,
        )
        .with_confidence(Confidence::High)
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 9,
            file: PathBuf::from("src/clean.rs"),
            line: 12,
        }]);
        engine.suggestions = vec![contradicted, clean];
        let contradicted_counts =
            std::collections::HashMap::from([(7usize, 3usize), (9usize, 0usize)]);
        let context = crate::context::WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: PathBuf::from("."),
        };
        engine.sort_with_context(&context, Some(&contradicted_counts));
        assert_eq!(engine.suggestions[0].summary, "Clean evidence");
    }

    #[test]
    fn test_kind_labels_are_plain_language() {
        assert_eq!(SuggestionKind::Refactoring.label(), "Cleanup");
        assert_eq!(SuggestionKind::Optimization.label(), "Speed");
        assert_eq!(SuggestionKind::Quality.label(), "Stability");
        assert_eq!(SuggestionKind::Testing.label(), "Safety");
        assert_eq!(SuggestionKind::Documentation.label(), "Guidance");
    }
}
