use serde::Deserialize;

// Model pricing per million tokens (estimated, check OpenRouter for current rates)
// Speed: openai/gpt-oss-120b - fast, cheap model for summaries
const SPEED_INPUT_COST: f64 = 0.10; // $0.10 per 1M input tokens
const SPEED_OUTPUT_COST: f64 = 0.30; // $0.30 per 1M output tokens
// Balanced: anthropic/claude-sonnet-4.5 - good reasoning at medium cost
const BALANCED_INPUT_COST: f64 = 3.0; // $3 per 1M input tokens
const BALANCED_OUTPUT_COST: f64 = 15.0; // $15 per 1M output tokens
// Smart: anthropic/claude-opus-4.5 - best reasoning for code generation
const SMART_INPUT_COST: f64 = 15.0; // $15 per 1M input tokens
const SMART_OUTPUT_COST: f64 = 75.0; // $75 per 1M output tokens
// Reviewer: openai/gpt-5.2 - different model family for cognitive diversity
const REVIEWER_INPUT_COST: f64 = 5.0; // $5 per 1M input tokens (estimated)
const REVIEWER_OUTPUT_COST: f64 = 15.0; // $15 per 1M output tokens (estimated)

/// Models available for suggestions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Speed tier - fast, cheap model for summaries and classification (gpt-oss-120b)
    Speed,
    /// Balanced tier - good reasoning at medium cost for questions/previews (claude-sonnet-4.5)
    Balanced,
    /// Smart tier - best reasoning for code generation (claude-opus-4.5)
    Smart,
    /// Reviewer tier - different model family for adversarial bug-finding (gpt-5.2)
    Reviewer,
}

/// Maximum tokens for all model tiers
const MODEL_MAX_TOKENS: u32 = 16384;

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Speed => "openai/gpt-oss-120b:nitro",
            Model::Balanced => "anthropic/claude-sonnet-4.5:nitro",
            Model::Smart => "anthropic/claude-opus-4.5:nitro",
            Model::Reviewer => "openai/gpt-5.2:nitro",
        }
    }

    pub fn max_tokens(&self) -> u32 {
        MODEL_MAX_TOKENS
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Model::Speed => "speed",
            Model::Balanced => "balanced",
            Model::Smart => "smart",
            Model::Reviewer => "reviewer",
        }
    }

    /// Calculate cost in USD based on token usage
    pub fn calculate_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        let (input_rate, output_rate) = match self {
            Model::Speed => (SPEED_INPUT_COST, SPEED_OUTPUT_COST),
            Model::Balanced => (BALANCED_INPUT_COST, BALANCED_OUTPUT_COST),
            Model::Smart => (SMART_INPUT_COST, SMART_OUTPUT_COST),
            Model::Reviewer => (REVIEWER_INPUT_COST, REVIEWER_OUTPUT_COST),
        };

        let input_cost = (prompt_tokens as f64 / 1_000_000.0) * input_rate;
        let output_cost = (completion_tokens as f64 / 1_000_000.0) * output_rate;

        input_cost + output_cost
    }
}

/// API usage information from OpenRouter
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Actual cost in USD as reported by OpenRouter (when available)
    #[serde(default)]
    pub cost: Option<f64>,
}

impl Usage {
    /// Get the cost for this usage.
    /// Prefers the actual cost from OpenRouter when available,
    /// falls back to estimated cost based on hardcoded rates.
    pub fn calculate_cost(&self, model: Model) -> f64 {
        // Prefer actual cost from OpenRouter if available
        if let Some(actual_cost) = self.cost {
            return actual_cost;
        }
        // Fall back to estimate using hardcoded rates
        model.calculate_cost(self.prompt_tokens, self.completion_tokens)
    }
}
