use serde::Deserialize;

/// Models available for suggestions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Speed tier - fast, cheap model for summaries and classification (gpt-oss-120b)
    Speed,
    /// Balanced tier - good reasoning at medium cost for questions/previews (claude-sonnet-4.5)
    Balanced,
    /// Smart tier - best reasoning for code generation (claude-opus-4.5)
    Smart,
}

/// Maximum tokens for all model tiers
const MODEL_MAX_TOKENS: u32 = 16384;

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Speed => "openai/gpt-oss-120b:nitro",
            Model::Balanced => "anthropic/claude-sonnet-4.5:nitro",
            Model::Smart => "anthropic/claude-opus-4.5:nitro",
        }
    }

    pub fn max_tokens(&self) -> u32 {
        MODEL_MAX_TOKENS
    }

    /// Whether this model supports JSON response formatting.
    pub fn supports_json_mode(&self) -> bool {
        matches!(self, Model::Speed | Model::Balanced | Model::Smart)
    }
}

/// API usage information from OpenRouter
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// Actual cost in USD as reported by OpenRouter.
    /// OpenRouter returns this as `total_cost` in the usage object.
    #[serde(default, alias = "total_cost")]
    pub cost: Option<f64>,
}

impl Usage {
    /// Get the actual cost for this usage from OpenRouter.
    /// Returns the cost reported by OpenRouter, or 0.0 if not available.
    /// We don't estimate costs - hardcoded rates are always wrong.
    pub fn cost(&self) -> f64 {
        self.cost.unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_ids() {
        assert!(Model::Speed.id().contains("gpt"));
        assert!(Model::Balanced.id().contains("claude"));
        assert!(Model::Smart.id().contains("claude"));
    }

    #[test]
    fn test_model_max_tokens() {
        assert_eq!(Model::Speed.max_tokens(), MODEL_MAX_TOKENS);
        assert_eq!(Model::Smart.max_tokens(), MODEL_MAX_TOKENS);
    }

    #[test]
    fn test_model_supports_json_mode() {
        assert!(Model::Speed.supports_json_mode());
        assert!(Model::Balanced.supports_json_mode());
        assert!(Model::Smart.supports_json_mode());
    }

    #[test]
    fn test_usage_returns_actual_cost() {
        let usage = Usage {
            prompt_tokens: 1000,
            completion_tokens: 1000,
            total_tokens: 2000,
            cost: Some(0.05),
        };
        assert_eq!(usage.cost(), 0.05);
    }

    #[test]
    fn test_usage_returns_zero_when_no_cost() {
        let usage = Usage {
            prompt_tokens: 1000,
            completion_tokens: 1000,
            total_tokens: 2000,
            cost: None,
        };
        // Returns 0.0 when no cost is available (we don't estimate)
        assert_eq!(usage.cost(), 0.0);
    }

    #[test]
    fn test_usage_deserialize_with_total_cost() {
        // OpenRouter returns "total_cost" in the usage object
        let json = r#"{"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150, "total_cost": 0.0025}"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cost(), 0.0025);
    }
}
