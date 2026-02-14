use crate::suggest::{Suggestion, VerificationState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cost: Option<f64>,
}

impl Usage {
    pub fn cost(&self) -> f64 {
        self.cost.unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuggestionQualityGateConfig {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuggestionGateSnapshot {
    pub final_count: usize,
    pub pending_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuggestionDiagnostics {
    pub run_id: String,
    pub model: String,
    pub attempt_index: usize,
    pub attempt_count: usize,
    pub attempt_ms: u64,
    pub attempt_cost_usd: f64,
    pub refinement_complete: bool,
    pub provisional_count: usize,
    pub validated_count: usize,
    pub final_count: usize,
    pub rejected_count: usize,
    pub raw_count: usize,
    pub deduped_count: usize,
    pub grounding_filtered: usize,
    pub low_confidence_filtered: usize,
    pub truncated_count: usize,
    pub regeneration_attempts: usize,
    pub tool_calls: usize,
    pub tool_names: Vec<String>,
    pub iterations: usize,
    pub llm_ms: u64,
    pub evidence_pack_ms: u64,
    pub tool_exec_ms: u64,
    pub batch_verify_ms: u64,
    pub batch_verify_attempted: usize,
    pub batch_verify_verified: usize,
    pub batch_verify_not_found: usize,
    pub batch_verify_errors: usize,
    pub pack_pattern_count: usize,
    pub pack_hotspot_count: usize,
    pub pack_core_count: usize,
    pub pack_line1_ratio: f64,
    pub sent_snippet_count: usize,
    pub sent_bytes: usize,
    pub response_chars: usize,
    pub parse_strategy: String,
    pub parse_stripped_markdown: bool,
    pub parse_used_sanitized_fix: bool,
    pub parse_used_json_fix: bool,
    pub parse_used_individual_parse: bool,
    pub forced_final: bool,
    pub formatting_pass: bool,
    pub response_format: bool,
    pub response_healing: bool,
    pub gate_passed: bool,
    pub gate_fail_reasons: Vec<String>,
    pub response_preview: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FixScope {
    Small,
    #[default]
    Medium,
    Large,
}

impl FixScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixPreview {
    pub verification_state: VerificationState,
    pub friendly_title: String,
    pub problem_summary: String,
    pub outcome: String,
    pub verification_note: String,
    pub description: String,
    pub affected_areas: Vec<String>,
    pub scope: FixScope,
    pub evidence_snippet: Option<String>,
    pub evidence_line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub file: String,
    pub line: Option<u32>,
    pub severity: String,
    pub category: String,
    pub title: String,
    pub description: String,
    pub recommended: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FixContext {
    pub problem_summary: String,
    pub outcome: String,
    pub description: String,
    pub modified_areas: Vec<String>,
}

pub fn build_fix_preview_from_validated_suggestion(suggestion: &Suggestion) -> FixPreview {
    FixPreview {
        verification_state: suggestion.verification_state,
        friendly_title: suggestion.kind.label().to_string(),
        problem_summary: suggestion.summary.clone(),
        outcome: "Implementation is disabled in UI shell mode.".to_string(),
        verification_note: "Verification is disabled in UI shell mode.".to_string(),
        description: suggestion.detail.clone().unwrap_or_default(),
        affected_areas: suggestion
            .affected_files()
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        scope: FixScope::Medium,
        evidence_snippet: suggestion.evidence.clone(),
        evidence_line: suggestion.line.map(|line| line as u32),
    }
}

pub fn is_available() -> bool {
    false
}

pub async fn fetch_account_balance() -> anyhow::Result<f64> {
    Err(anyhow::anyhow!(
        "Wallet balance is unavailable in UI shell mode"
    ))
}
