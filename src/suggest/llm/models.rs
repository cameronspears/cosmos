use serde::Deserialize;

/// Models available for suggestions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Speed tier - fast, cheap model for summaries and classification (gpt-oss-120b)
    Speed,
    /// Balanced tier - deep reasoning at medium cost for questions/previews (gpt-5.2-codex)
    Balanced,
    /// Smart tier - best reasoning for code generation (gpt-5.2)
    Smart,
}

/// Maximum tokens for all model tiers
const MODEL_MAX_TOKENS: u32 = 16384;

/// Models we allow to use JSON formatting / structured outputs.
const JSON_FORMAT_MODELS: [&str; 3] = [
    "openai/gpt-oss-120b:exacto",
    "openai/gpt-5.2:nitro",
    "openai/gpt-5.2-codex:nitro",
];

fn supports_json_format(model_id: &str) -> bool {
    JSON_FORMAT_MODELS.iter().any(|id| *id == model_id)
}

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Speed => "openai/gpt-oss-120b:exacto",
            Model::Balanced => "openai/gpt-5.2-codex:nitro",
            Model::Smart => "openai/gpt-5.2:nitro",
        }
    }

    pub fn max_tokens(&self) -> u32 {
        MODEL_MAX_TOKENS
    }

    /// Whether this model supports JSON response formatting.
    pub fn supports_json_mode(&self) -> bool {
        supports_json_format(self.id())
    }

    /// Whether this model supports structured outputs with JSON schema.
    pub fn supports_structured_outputs(&self) -> bool {
        supports_json_format(self.id())
    }

    /// Reasoning effort level to use, if supported.
    pub fn reasoning_effort(&self) -> Option<&'static str> {
        match self {
            Model::Speed | Model::Balanced => Some("high"),
            Model::Smart => Some("xhigh"),
        }
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

/// Merge two optional `Usage` values, summing their token counts and costs.
pub(crate) fn merge_usage(primary: Option<Usage>, secondary: Option<Usage>) -> Option<Usage> {
    match (primary, secondary) {
        (Some(p), Some(s)) => Some(Usage {
            prompt_tokens: p.prompt_tokens + s.prompt_tokens,
            completion_tokens: p.completion_tokens + s.completion_tokens,
            total_tokens: p.total_tokens + s.total_tokens,
            cost: match (p.cost, s.cost) {
                (Some(pc), Some(sc)) => Some(pc + sc),
                (Some(pc), None) => Some(pc),
                (None, Some(sc)) => Some(sc),
                (None, None) => None,
            },
        }),
        (Some(p), None) => Some(p),
        (None, Some(s)) => Some(s),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_ids() {
        assert!(Model::Speed.id().contains("gpt"));
        assert!(Model::Balanced.id().contains("gpt-5.2-codex"));
        assert!(Model::Smart.id().contains("gpt-5.2"));
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
    fn test_supports_json_format_allowlist() {
        assert!(supports_json_format("openai/gpt-oss-120b:exacto"));
        assert!(supports_json_format("openai/gpt-5.2:nitro"));
        assert!(supports_json_format("openai/gpt-5.2-codex:nitro"));
        assert!(!supports_json_format("openai/gpt-4o"));
    }

    #[test]
    fn test_reasoning_effort_by_model() {
        assert_eq!(Model::Speed.reasoning_effort(), Some("high"));
        assert_eq!(Model::Balanced.reasoning_effort(), Some("high"));
        assert_eq!(Model::Smart.reasoning_effort(), Some("xhigh"));
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
