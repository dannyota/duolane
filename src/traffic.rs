use serde_json::{Map, Value};
use std::fs::{self, File, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::config::AliasProvider;
use crate::logging::is_credential_key;
use crate::paths;
use crate::providers::codex::translate::reasoning_signature::is_proxy_reasoning_signature;

#[derive(Debug)]
pub struct TrafficCapture {
    root: PathBuf,
    artifact_counter: Mutex<usize>,
    event_counter: Mutex<usize>,
}

pub const MAX_SSE_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STREAM_CAPTURE_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STREAM_CAPTURE_EVENTS: usize = 1_024;
pub const MAX_STREAM_CAPTURE_FRAME_BYTES: usize = 64 * 1024;
pub const TRAFFIC_CAPTURE_WARNING: &str = "warning: DUOLANE_TRAFFIC_LOG is on. duolane stores request and response content, including conversation text, under its state directory.";

#[derive(Debug)]
pub struct TrafficCaptureOptions {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: Option<String>,
    pub state_dir_override: Option<PathBuf>,
}

pub fn traffic_capture_enabled() -> bool {
    traffic_capture_enabled_for_env(&std::env::vars().collect())
}

pub fn traffic_capture_enabled_for_env(env: &std::collections::HashMap<String, String>) -> bool {
    match env.get("DUOLANE_TRAFFIC_LOG").map(String::as_str) {
        Some(v) => matches!(v, "1" | "true" | "yes"),
        None => false,
    }
}

pub fn create_traffic_capture(opts: TrafficCaptureOptions) -> Option<TrafficCapture> {
    if !traffic_capture_enabled() {
        return None;
    }
    let state_root = opts
        .state_dir_override
        .unwrap_or_else(paths::state_dir)
        .join("traffic")
        .join(sanitize_path_part(
            opts.session_id.as_deref().unwrap_or("no-session"),
        ))
        .join(format!(
            "{:06}-{}-{}",
            opts.session_seq.unwrap_or(0),
            sanitize_path_part(opts.provider.as_deref().unwrap_or("unknown-provider")),
            sanitize_path_part(&opts.req_id),
        ));

    Some(TrafficCapture {
        root: state_root,
        artifact_counter: Mutex::new(0),
        event_counter: Mutex::new(0),
    })
}

impl TrafficCapture {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write_json(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_artifact_path(name, true);
        let _ = write_bytes(path, &payload);
    }

    pub fn write_text(&self, name: &str, text: &str) {
        let file = if name.ends_with(".txt") {
            name.to_string()
        } else {
            format!("{name}.txt")
        };
        let path = self.next_artifact_path(&file, false);
        let _ = write_bytes(path, text.as_bytes());
    }

    pub fn write_bytes(&self, name: &str, value: &[u8]) {
        let path = self.next_artifact_path(name, false);
        let _ = write_bytes(path, value);
    }

    pub fn write_json_event(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_event_path(name, true);
        let _ = write_bytes(path, &payload);
    }

    fn next_artifact_path(&self, name: &str, ensure_ext_json: bool) -> PathBuf {
        let mut counter: MutexGuard<'_, usize> = self
            .artifact_counter
            .lock()
            .unwrap_or_else(|_| self.artifact_counter.lock().unwrap());
        *counter += 1;
        let file = if ensure_ext_json && !name.ends_with(".json") {
            format!("{name}.json")
        } else {
            name.to_string()
        };
        self.root
            .join(format!("{:03}-{}", *counter, sanitize_path_part(&file)))
    }

    fn next_event_path(&self, name: &str, ensure_ext_json: bool) -> PathBuf {
        let mut counter: MutexGuard<'_, usize> = self
            .event_counter
            .lock()
            .unwrap_or_else(|_| self.event_counter.lock().unwrap());
        *counter += 1;
        let file = if ensure_ext_json && !name.ends_with(".json") {
            format!("{name}.json")
        } else {
            name.to_string()
        };
        self.root
            .join("events")
            .join(format!("{:06}-{}", *counter, sanitize_path_part(&file)))
    }
}

#[cfg(test)]
pub(crate) fn test_capture(root: PathBuf) -> TrafficCapture {
    TrafficCapture {
        root,
        artifact_counter: Mutex::new(0),
        event_counter: Mutex::new(0),
    }
}

fn write_bytes(path: PathBuf, value: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
        if let Ok(meta) = fs::metadata(parent) {
            set_mode(parent, 0o700);
            if meta.is_dir() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = meta.permissions();
                    perm.set_mode(0o700);
                    let _ = fs::set_permissions(parent, perm);
                }
            }
        }
    }
    let mut out = File::create(&path)?;
    out.write_all(value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = out.metadata()?.permissions();
        perm.set_mode(0o600);
        let _ = fs::set_permissions(&path, perm);
    }
    Ok(())
}

pub fn sanitize_path_part(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    let truncated = if cleaned.len() > 160 {
        &cleaned[..160]
    } else {
        &cleaned
    };
    if truncated.is_empty() {
        "unknown".to_string()
    } else {
        truncated.to_string()
    }
}

pub fn redact_traffic(value: &Value) -> Value {
    redact_traffic_with_depth(value, 0, false)
}

const IDENTITY_KEYS: [&str; 7] = [
    "email",
    "user_id",
    "account_id",
    "identity",
    "identity_id",
    "subject",
    "sub",
];

fn redact_traffic_with_depth(value: &Value, depth: u16, in_image: bool) -> Value {
    if depth > 100 {
        return Value::String("[depth-limit]".to_string());
    }

    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let normalized = key.to_lowercase();
                let redacted = if normalized == "image_url"
                    || (normalized == "data" && looks_like_image_source(map))
                {
                    redact_image_field(value, depth + 1)
                } else if normalized == "encrypted_content"
                    || (normalized == "signature"
                        && value.as_str().is_some_and(is_proxy_reasoning_signature))
                    || is_credential_key(key)
                    || IDENTITY_KEYS.contains(&normalized.as_str())
                {
                    redact_traffic_value(value)
                } else {
                    redact_traffic_with_depth(value, depth + 1, in_image)
                };
                out.insert(key.clone(), redacted);
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_traffic_with_depth(value, depth + 1, in_image))
                .collect(),
        ),
        Value::String(text) if is_data_url(text) || (in_image && has_data_scheme(text)) => {
            Value::String(format!("[redacted data-url len={}]", text.len()))
        }
        _ => value.clone(),
    }
}

fn redact_image_field(value: &Value, depth: u16) -> Value {
    match value {
        Value::Object(_) | Value::Array(_) => redact_traffic_with_depth(value, depth, true),
        _ => redact_traffic_value(value),
    }
}

fn is_data_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("data:image/") && lower.contains(";base64,")
}

fn has_data_scheme(text: &str) -> bool {
    text.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

fn looks_like_image_source(map: &Map<String, Value>) -> bool {
    if !map
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|source_type| source_type.eq_ignore_ascii_case("base64"))
    {
        return false;
    }
    match map.get("media_type").and_then(Value::as_str) {
        Some(media_type) => media_type.to_ascii_lowercase().starts_with("image/"),
        None => true,
    }
}

fn redact_traffic_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
}

#[allow(dead_code)]
fn _provider_alias(_provider: &str) -> Option<AliasProvider> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_traffic_strips_input_image_data_urls() {
        let value = serde_json::json!({
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "what color?"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"}
                ]}
            ]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "base64 payload leaked: {rendered}"
        );
        assert!(
            !rendered.contains("data:image/png;base64,iVBOR"),
            "data URL payload leaked: {rendered}"
        );
        assert!(rendered.contains("input_image"));
    }

    #[test]
    fn redact_traffic_strips_inline_base64_image_values() {
        let value = serde_json::json!({
            "output": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(!rendered.contains("iVBORw0KGgo"));
    }

    #[test]
    fn redact_traffic_strips_anthropic_image_source_data() {
        let value = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
                    }}
                ]
            }]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "source.data base64 leaked: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        assert!(rendered.contains("image/png"));
    }

    #[test]
    fn redact_traffic_strips_compaction_encrypted_content() {
        let value = serde_json::json!({
            "model": "gpt-5",
            "input": [
                {"type": "compaction", "encrypted_content": "gAAAAABopaque-compacted-transcript"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hello"}
                ]}
            ]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("gAAAAA"),
            "encrypted_content leaked: {rendered}"
        );
        assert_eq!(
            redacted["input"][0]["encrypted_content"],
            "[redacted len=34]"
        );
        assert_eq!(redacted["input"][0]["type"], "compaction");
        assert_eq!(redacted["input"][1]["content"][0]["text"], "hello");
        assert_eq!(redacted["model"], "gpt-5");
    }

    #[test]
    fn redact_traffic_strips_reasoning_encrypted_content_in_events() {
        let value = serde_json::json!({
            "type": "response.output_item.done",
            "item": {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAAAAopaque"}
        });
        let redacted = redact_traffic(&value);
        assert_eq!(redacted["item"]["encrypted_content"], "[redacted len=12]");
        assert_eq!(redacted["item"]["id"], "rs_1");
    }

    #[test]
    fn redact_traffic_strips_proxy_signature_delta_in_downstream_event() {
        let signature = "ccp:codex:v1:cnNfMQ:gAAAAABopaque-reasoning-replay";
        let value = serde_json::json!({
            "event": "content_block_delta",
            "data": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": signature}
            }
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("ccp:codex:v1:"),
            "proxy signature leaked: {rendered}"
        );
        assert!(
            !rendered.contains("gAAAAABopaque"),
            "encrypted payload leaked: {rendered}"
        );
        assert_eq!(
            redacted["data"]["delta"]["signature"],
            format!("[redacted len={}]", signature.len())
        );
        assert_eq!(redacted["data"]["delta"]["type"], "signature_delta");
        assert_eq!(redacted["data"]["index"], 0);
    }

    #[test]
    fn redact_traffic_strips_proxy_signature_in_thinking_blocks() {
        let signature = "ccp:codex:v1:cnNfMQ:gAAAAABopaque-reasoning-replay";
        let value = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "reasoned", "signature": signature},
                {"type": "text", "text": "answer"}
            ]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("ccp:codex:v1:"),
            "proxy signature leaked: {rendered}"
        );
        assert_eq!(
            redacted["content"][0]["signature"],
            format!("[redacted len={}]", signature.len())
        );
        assert_eq!(redacted["content"][0]["thinking"], "reasoned");
        assert_eq!(redacted["content"][1]["text"], "answer");
    }

    #[test]
    fn redact_traffic_keeps_foreign_signatures() {
        let value = serde_json::json!({
            "delta": {"type": "signature_delta", "signature": "ErUBCkYIBRgCIkA-real"},
            "content": [{"type": "thinking", "signature": "another-opaque-signature"}]
        });
        let redacted = redact_traffic(&value);
        assert_eq!(redacted["delta"]["signature"], "ErUBCkYIBRgCIkA-real");
        assert_eq!(
            redacted["content"][0]["signature"],
            "another-opaque-signature"
        );
    }

    #[test]
    fn traffic_capture_redacts_proxy_signatures_at_json_boundary() {
        let temp = tempfile::TempDir::new().unwrap();
        let capture = test_capture(temp.path().join("traffic"));
        let signature = "ccp:codex:v1:cnNfMQ:gAAAAABopaque-reasoning-replay";
        let foreign = "ErUBCkYIBRgCIkA-real-anthropic-signature";

        capture.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": "content_block_delta",
                "data": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": signature}
                }
            }),
        );
        capture.write_json(
            "010-anthropic-request",
            &serde_json::json!({
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "kept", "signature": signature},
                        {"type": "thinking", "thinking": "kept", "signature": foreign}
                    ]
                }]
            }),
        );

        let mut captured = String::new();
        for dir in [capture.root().to_path_buf(), capture.root().join("events")] {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_file() {
                    captured.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }

        assert!(
            !captured.contains("ccp:codex:v1:"),
            "proxy signature leaked to capture: {captured}"
        );
        assert!(
            !captured.contains("gAAAAABopaque"),
            "encrypted payload leaked to capture: {captured}"
        );
        assert!(
            captured.contains(foreign),
            "foreign signature lost: {captured}"
        );
        assert!(captured.contains("kept"));
    }

    #[test]
    fn redact_traffic_keeps_unrelated_data_keys() {
        let value = serde_json::json!({"data": "some-non-image-payload", "count": 3});
        let redacted = redact_traffic(&value);
        assert_eq!(redacted["data"], "some-non-image-payload");
    }

    #[test]
    fn redact_traffic_strips_data_urls_in_array_shaped_tool_output() {
        let value = serde_json::json!({
            "input": [
                {"type": "function_call_output", "call_id": "call_1", "output": [
                    {"type": "input_text", "text": "screenshot"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"}
                ]}
            ]
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "base64 payload leaked: {rendered}"
        );
        assert!(
            !rendered.contains("data:image/png;base64,iVBOR"),
            "data URL payload leaked: {rendered}"
        );
        assert!(rendered.contains("input_image"));
        assert!(rendered.contains("screenshot"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn redact_traffic_strips_uppercase_media_type_image_source() {
        let value = serde_json::json!({
            "source": {"type": "base64", "media_type": "IMAGE/PNG", "data": "QUJDREVGRw=="}
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("QUJDREVGRw"),
            "payload leaked: {rendered}"
        );
        assert!(
            redacted["source"]["data"]
                .as_str()
                .unwrap()
                .contains("redacted")
        );
    }

    #[test]
    fn redact_traffic_strips_uppercase_image_source_type() {
        let value = serde_json::json!({
            "source": {"type": "BASE64", "media_type": "image/png", "data": "QUJDREVGRw=="}
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("QUJDREVGRw"),
            "payload leaked: {rendered}"
        );
    }

    #[test]
    fn redact_traffic_strips_image_source_without_media_type() {
        let value = serde_json::json!({
            "source": {"type": "base64", "data": "QUJDREVGRw=="}
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("QUJDREVGRw"),
            "payload leaked: {rendered}"
        );
    }

    #[test]
    fn redact_traffic_strips_uppercase_data_url_scheme() {
        let value = serde_json::json!({
            "image_url": "DATA:IMAGE/PNG;base64,iVBORw0KGgoAAAANSUhEUg"
        });
        let redacted = redact_traffic(&value);
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "data URL payload leaked: {rendered}"
        );
    }

    #[test]
    fn redact_traffic_keeps_structured_image_url_shape() {
        let value = serde_json::json!({
            "image_url": {"url": "DATA:IMAGE/PNG;BASE64,iVBORw0KGgoAAAANSUhEUg"}
        });
        let redacted = redact_traffic(&value);
        assert!(redacted["image_url"].is_object(), "shape lost: {redacted}");
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("iVBORw0KGgo"),
            "nested payload leaked: {rendered}"
        );
    }

    #[test]
    fn traffic_redacts_proxy_authorization() {
        let redacted = redact_traffic(&serde_json::json!({
            "headers": {
                "proxy-authorization": "Basic dXNlcjpwYXNz",
                "x-safe": "kept"
            }
        }));

        assert_eq!(redacted["headers"]["x-safe"], "kept");
        assert_eq!(
            redacted["headers"]["proxy-authorization"],
            "[redacted len=18]"
        );
    }
}
