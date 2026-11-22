use anyhow::Result;
use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::providers::codex::translate::{
    model_allowlist::ResolvedModel,
    reasoning_signature::ReasoningAccount,
    request::{ServiceTier, TranslateOptions, translate_request},
};

pub fn build_request(
    body: &MessagesRequest,
    resolved: &ResolvedModel,
    session_id: Option<String>,
    reasoning: &ReasoningAccount,
) -> Result<Value> {
    let mut translated = translate_request(
        body,
        TranslateOptions {
            session_id,
            service_tier: resolved.service_tier.clone(),
            model: resolved.model.clone(),
            use_responses_lite: false,
            reasoning_account: reasoning.clone(),
        },
    )?;
    if translated.service_tier == Some(ServiceTier::Ultrafast) {
        translated.service_tier = Some(ServiceTier::Priority);
    }
    let mut value = serde_json::to_value(translated)?;
    if let Some(max_tokens) = body.max_tokens {
        value["max_output_tokens"] = json!(max_tokens);
    }
    Ok(value)
}
