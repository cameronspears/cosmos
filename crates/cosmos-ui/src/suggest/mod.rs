//! Minimal suggestion models and in-memory list for UI shell mode.

pub mod llm;

use crate::index::CodebaseIndex;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

const MAX_SUGGESTIONS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionSource {
    Static,
    Cached,
    LlmFast,
    LlmDeep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SuggestionKind {
    Improvement,
    BugFix,
    Feature,
    Optimization,
    Quality,
    Documentation,
    Testing,
    Refactoring,
}

impl SuggestionKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Improvement => "Improve",
            Self::BugFix => "Fix",
            Self::Feature => "Feature",
            Self::Optimization => "Speed",
            Self::Quality => "Stability",
            Self::Documentation => "Guidance",
            Self::Testing => "Safety",
            Self::Refactoring => "Cleanup",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum Confidence {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    #[default]
    Unverified,
    Verified,
    Contradicted,
    InsufficientEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionValidationState {
    #[default]
    Pending,
    Validated,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionEvidenceRef {
    pub snippet_id: usize,
    pub file: PathBuf,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: Uuid,
    pub kind: SuggestionKind,
    pub priority: Priority,
    #[serde(default)]
    pub confidence: Confidence,
    pub file: PathBuf,
    #[serde(default)]
    pub additional_files: Vec<PathBuf>,
    pub line: Option<usize>,
    pub summary: String,
    pub detail: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub evidence_refs: Vec<SuggestionEvidenceRef>,
    #[serde(default)]
    pub verification_state: VerificationState,
    #[serde(default)]
    pub validation_state: SuggestionValidationState,
    #[serde(default)]
    pub implementation_readiness_score: Option<f32>,
    #[serde(default)]
    pub implementation_risk_flags: Vec<String>,
    #[serde(default)]
    pub implementation_sketch: Option<String>,
    pub source: SuggestionSource,
    pub created_at: DateTime<Utc>,
    pub dismissed: bool,
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
            source,
            created_at: Utc::now(),
            dismissed: false,
            applied: false,
        }
    }

    pub fn affected_files(&self) -> Vec<&PathBuf> {
        std::iter::once(&self.file)
            .chain(self.additional_files.iter())
            .collect()
    }

    pub fn is_multi_file(&self) -> bool {
        !self.additional_files.is_empty()
    }

    pub fn file_count(&self) -> usize {
        1 + self.additional_files.len()
    }
}

pub struct SuggestionEngine {
    pub suggestions: Vec<Suggestion>,
    pub index: CodebaseIndex,
}

impl SuggestionEngine {
    pub fn new(index: CodebaseIndex) -> Self {
        Self {
            suggestions: Vec::new(),
            index,
        }
    }

    pub fn active_suggestions(&self) -> Vec<&Suggestion> {
        self.suggestions
            .iter()
            .filter(|s| !s.dismissed && !s.applied)
            .take(MAX_SUGGESTIONS)
            .collect()
    }

    pub fn mark_applied(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.applied = true;
        }
    }

    pub fn mark_dismissed(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.dismissed = true;
        }
    }

    pub fn unmark_applied(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.applied = false;
        }
    }
}
