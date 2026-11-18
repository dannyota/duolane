use serde_json::Value;

use super::OpenAiError;
use crate::registry::normalize_incoming_model;

pub fn extract_model(body: &Value) -> Result<(String, String), OpenAiError> {
    let object = body.as_object().ok_or_else(|| {
        OpenAiError::invalid("Request body must be a JSON object", None::<String>)
    })?;
    let requested = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| OpenAiError::invalid("Missing or invalid 'model'", Some("model")))?
        .to_string();
    Ok((requested.clone(), normalize_incoming_model(&requested)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_model_and_preserves_requested_value() {
        let (requested, normalized) =
            extract_model(&json!({"model":"gpt-5.6-sol[1m]","input":"hello"})).unwrap();
        assert_eq!(requested, "gpt-5.6-sol[1m]");
        assert_eq!(normalized, "gpt-5.6-sol");
    }
}
