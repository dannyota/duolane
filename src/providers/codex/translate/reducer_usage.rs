#[derive(Debug, Clone, Default)]
pub struct CodexUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub input_tokens_details_cached: Option<u64>,
    pub output_tokens_details_reasoning: Option<u64>,
    pub rate_limits: Option<Box<CodexRateLimits>>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodexRateLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<CodexRateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<CodexRateLimitWindow>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodexRateLimitWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
}

pub fn parse_codex_rate_limits(payload: &serde_json::Value) -> Option<CodexRateLimits> {
    let limits = payload.get("rate_limits")?.as_object()?;
    let window = |name: &str| -> Option<CodexRateLimitWindow> {
        let w = limits.get(name)?.as_object()?;
        let window = CodexRateLimitWindow {
            used_percent: w.get("used_percent").and_then(|v| v.as_f64()),
            window_minutes: w.get("window_minutes").and_then(|v| v.as_u64()),
            reset_at: w.get("reset_at").and_then(|v| v.as_u64()),
        };
        (window != CodexRateLimitWindow::default()).then_some(window)
    };
    let snapshot = CodexRateLimits {
        plan_type: payload
            .get("plan_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        limit_reached: limits.get("limit_reached").and_then(|v| v.as_bool()),
        primary: window("primary"),
        secondary: window("secondary"),
    };
    (snapshot != CodexRateLimits::default()).then_some(snapshot)
}

pub(super) fn parse_codex_usage(response: &serde_json::Value) -> CodexUsage {
    let usage = match response.get("usage") {
        Some(u) => u,
        None => return CodexUsage::default(),
    };
    CodexUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        input_tokens_details_cached: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
        rate_limits: None,
    }
}

pub fn map_codex_usage_to_anthropic(
    u: &Option<CodexUsage>,
    web_search_requests: Option<usize>,
) -> AnthropicUsage {
    let usage = match u {
        Some(u) => u,
        None => return AnthropicUsage::default(),
    };
    let cached = usage.input_tokens_details_cached.unwrap_or(0);
    let total_input = usage.input_tokens.unwrap_or(0);
    let input_tokens = total_input.saturating_sub(cached);

    let mut result = AnthropicUsage {
        input_tokens,
        output_tokens: usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        server_tool_use: None,
        output_tokens_details: usage
            .output_tokens_details_reasoning
            .map(|reasoning_tokens| OutputTokensDetails {
                reasoning_tokens,
                thinking_tokens: reasoning_tokens,
            }),
        codex_rate_limits: usage.rate_limits.as_deref().cloned(),
    };

    if let Some(requests) = web_search_requests
        && requests > 0
    {
        result.server_tool_use = Some(WebSearchUsage {
            web_search_requests: requests,
        });
    }

    result
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<WebSearchUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rate_limits: Option<CodexRateLimits>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u64,
    pub thinking_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebSearchUsage {
    pub web_search_requests: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_usage_reports_cached_prompt_tokens() {
        let usage = CodexUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            input_tokens_details_cached: Some(20),
            output_tokens_details_reasoning: None,
            rate_limits: None,
        };
        let mapped = map_codex_usage_to_anthropic(&Some(usage), None);
        assert_eq!(mapped.input_tokens, 80);
        assert_eq!(mapped.output_tokens, 50);
        assert_eq!(mapped.cache_read_input_tokens, 20);
        assert!(mapped.output_tokens_details.is_none());
        assert!(mapped.codex_rate_limits.is_none());
    }

    #[test]
    fn map_usage_reports_reasoning_and_rate_limits() {
        let usage = CodexUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            input_tokens_details_cached: None,
            output_tokens_details_reasoning: Some(30),
            rate_limits: Some(Box::new(CodexRateLimits {
                plan_type: Some("prolite".into()),
                limit_reached: Some(false),
                primary: Some(CodexRateLimitWindow {
                    used_percent: Some(8.0),
                    window_minutes: Some(10080),
                    reset_at: Some(1_789_765_755),
                }),
                secondary: None,
            })),
        };
        let mapped = map_codex_usage_to_anthropic(&Some(usage), None);
        let details = mapped
            .output_tokens_details
            .as_ref()
            .expect("reasoning details");
        assert_eq!(details.reasoning_tokens, 30);
        assert_eq!(details.thinking_tokens, 30);
        let json = serde_json::to_value(&mapped).unwrap();
        assert_eq!(json["codex_rate_limits"]["primary"]["used_percent"], 8.0);
        assert_eq!(json["codex_rate_limits"]["plan_type"], "prolite");
        assert!(json["codex_rate_limits"].get("secondary").is_none());
    }
}
