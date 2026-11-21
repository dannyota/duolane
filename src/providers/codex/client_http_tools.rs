use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::client::{CodexError, CodexErrorOrigin};
use super::translate::live_stream::BUFFERED_TOOL_MAX_ARGS_BYTES;
use super::translate::live_stream_read_args::repair_whitespace_stalled_read_args;

const MAX_PENDING_TOOL_CALLS: usize = 64;
const MAX_PENDING_TOOL_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMITTED_TOOL_CALLS: usize = 1_024;
const MAX_COMMITTED_TOOL_ID_BYTES: usize = 1024 * 1024;
pub(super) const STALLED_READ_FINISH_FIELD: &str = "_duolane_stalled_read_finish";

#[derive(Default)]
pub(super) struct HttpToolState {
    calls: HashMap<String, PendingCall>,
    bytes: usize,
    committed: CommittedCalls,
}

#[derive(Default)]
struct CommittedCalls {
    ids: HashSet<String>,
    bytes: usize,
}

impl CommittedCalls {
    fn record(&mut self, id: &str) -> Result<(), CodexError> {
        let bytes = self.bytes + id.len();
        if self.ids.len() >= MAX_COMMITTED_TOOL_CALLS || bytes > MAX_COMMITTED_TOOL_ID_BYTES {
            return Err(http_tool_error(
                "Codex HTTP committed tool IDs exceeded safe limits",
            ));
        }
        self.ids.insert(id.to_owned());
        self.bytes = bytes;
        Ok(())
    }
}

struct PendingCall {
    item_id: Option<String>,
    output_index: Option<u64>,
    name: String,
    args: String,
}

impl PendingCall {
    fn matches_arguments_event(&self, call_id: &str, payload: &Value) -> bool {
        let mut matched = false;
        if let Some(id) = string_field(payload, "call_id") {
            if id != call_id {
                return false;
            }
            matched = true;
        }
        if let (Some(held), Some(id)) = (&self.item_id, string_field(payload, "item_id")) {
            if held != id {
                return false;
            }
            matched = true;
        }
        if let (Some(held), Some(index)) = (self.output_index, output_index(payload)) {
            if held != index {
                return false;
            }
            matched = true;
        }
        matched
    }

    fn bytes(&self, call_id: &str) -> usize {
        call_id.len()
            + self.item_id.as_deref().map_or(0, str::len)
            + self.name.len()
            + self.args.len()
    }
}

impl HttpToolState {
    pub(super) fn validate_terminal(&self, payload: &Value) -> Result<(), CodexError> {
        if !self.calls.is_empty() {
            return Err(http_tool_error(
                "upstream stream ended with an open Codex tool call",
            ));
        }
        if let Some(output) = payload
            .pointer("/response/output")
            .and_then(Value::as_array)
        {
            for item in output.iter().filter(|item| item["type"] == "function_call") {
                let id = required_id(item, "call_id")?;
                if !self.committed.ids.contains(id) {
                    return Err(http_tool_error(
                        "Codex HTTP terminal response listed an uncommitted function call",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn observe(&mut self, payload: &mut Value) -> Result<bool, CodexError> {
        let item = &payload["item"];
        match (string_field(payload, "type"), string_field(item, "type")) {
            (Some("response.output_item.added"), Some("function_call")) => {
                self.hold_call(payload, item)?;
                Ok(false)
            }
            (Some("response.function_call_arguments.delta"), _) => {
                if let Some(completed) = self.hold_delta(payload)? {
                    *payload = completed;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            (Some("response.function_call_arguments.done"), _) => {
                match_argument_event(&mut self.calls, payload)?;
                Ok(false)
            }
            (Some("response.output_item.done"), Some("function_call")) => {
                self.complete_call(payload)
            }
            _ => Ok(true),
        }
    }

    fn hold_call(&mut self, payload: &Value, item: &Value) -> Result<(), CodexError> {
        let call_id = required_id(item, "call_id")?;
        let name = required_id(item, "name")?;
        if self.committed.ids.contains(call_id) {
            return Err(http_tool_error(
                "Codex HTTP function call was already committed",
            ));
        }
        if self.calls.contains_key(call_id) {
            return Err(http_tool_error(
                "Codex HTTP function call was already pending",
            ));
        }
        let item_id = string_field(item, "id");
        let args = string_field(item, "arguments").unwrap_or_default();
        check_args_size(args.len())?;
        let bytes = call_id.len() + name.len() + item_id.map_or(0, str::len) + args.len();
        if self.calls.len() >= MAX_PENDING_TOOL_CALLS {
            return Err(http_tool_error(
                "Codex HTTP pending tool calls exceeded safe limits",
            ));
        }
        self.bytes = checked_pending_bytes(self.bytes, bytes)?;
        self.calls.insert(
            call_id.to_owned(),
            PendingCall {
                item_id: item_id.map(str::to_owned),
                output_index: output_index(payload),
                name: name.to_owned(),
                args: args.to_owned(),
            },
        );
        Ok(())
    }

    fn hold_delta(&mut self, payload: &Value) -> Result<Option<Value>, CodexError> {
        let delta = string_field(payload, "delta")
            .ok_or_else(|| http_tool_error("Codex HTTP function call delta is missing a string"))?;
        let sole_call = self.calls.len() == 1;
        let (call_id, call) = match_argument_event(&mut self.calls, payload)?;
        if delta.is_empty() {
            return Ok(None);
        }
        check_args_size(call.args.len() + delta.len())?;
        self.bytes = checked_pending_bytes(self.bytes, delta.len())?;
        call.args.push_str(delta);
        if !sole_call {
            return Ok(None);
        }
        let Some(args) = repair_whitespace_stalled_read_args(&call.name, &call.args) else {
            return Ok(None);
        };
        self.committed.record(call_id)?;
        let mut done = json!({
            "type": "response.output_item.done",
            "item": {"type": "function_call", "call_id": call_id, "name": call.name, "arguments": args}
        });
        done[STALLED_READ_FINISH_FIELD] = Value::Bool(true);
        if let Some(index) = call.output_index {
            done["output_index"] = json!(index);
        }
        self.calls.clear();
        self.bytes = 0;
        Ok(Some(done))
    }

    fn complete_call(&mut self, payload: &mut Value) -> Result<bool, CodexError> {
        let item = &payload["item"];
        if item
            .get("status")
            .is_some_and(|status| status != "completed")
        {
            return Err(http_tool_error(
                "Codex HTTP function call done item was not completed",
            ));
        }
        let call_id = required_id(item, "call_id")?;
        if self.committed.ids.contains(call_id) {
            return Ok(false);
        }
        let pending = self.calls.remove(call_id);
        if let Some(call) = pending.as_ref() {
            self.bytes -= call.bytes(call_id);
        }
        let name = match item.get("name") {
            None => pending
                .as_ref()
                .map(|call| call.name.as_str())
                .ok_or_else(invalid_identity)?,
            Some(_) => required_id(item, "name")?,
        };
        if pending.as_ref().is_some_and(|call| call.name != name) {
            return Err(invalid_identity());
        }
        let args = string_field(item, "arguments").ok_or_else(|| {
            http_tool_error("Codex HTTP function call is missing string arguments")
        })?;
        check_args_size(args.len())?;
        let args = if args.trim().is_empty() {
            "{}"
        } else {
            serde_json::from_str::<serde_json::Map<String, Value>>(args).map_err(|_| {
                http_tool_error("Codex HTTP function call arguments must be a valid JSON object")
            })?;
            args
        };
        let index = pending
            .as_ref()
            .and_then(|call| call.output_index)
            .or_else(|| output_index(payload));
        self.committed.record(call_id)?;
        let mut completed = json!({
            "type": "response.output_item.done",
            "item": {"type": "function_call", "call_id": call_id, "name": name, "arguments": args}
        });
        if let Some(index) = index {
            completed["output_index"] = json!(index);
        }
        *payload = completed;
        Ok(true)
    }
}

fn match_argument_event<'a>(
    calls: &'a mut HashMap<String, PendingCall>,
    payload: &Value,
) -> Result<(&'a String, &'a mut PendingCall), CodexError> {
    let mut matches = calls
        .iter_mut()
        .filter(|(id, call)| call.matches_arguments_event(id, payload));
    let matched = matches.next().ok_or_else(|| {
        http_tool_error("Codex HTTP arguments event had no pending function call")
    })?;
    if matches.next().is_some() {
        return Err(http_tool_error(
            "Codex HTTP arguments event matched multiple function calls",
        ));
    }
    Ok(matched)
}

fn output_index(payload: &Value) -> Option<u64> {
    payload.get("output_index").and_then(Value::as_u64)
}

fn string_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn required_id<'a>(payload: &'a Value, field: &str) -> Result<&'a str, CodexError> {
    string_field(payload, field)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(invalid_identity)
}

fn invalid_identity() -> CodexError {
    http_tool_error("Codex HTTP function call has invalid identity")
}

fn check_args_size(bytes: usize) -> Result<(), CodexError> {
    if bytes > BUFFERED_TOOL_MAX_ARGS_BYTES {
        return Err(http_tool_error(
            "Codex HTTP function call arguments exceeded safe limits",
        ));
    }
    Ok(())
}

fn checked_pending_bytes(current: usize, added: usize) -> Result<usize, CodexError> {
    let bytes = current + added;
    if bytes > MAX_PENDING_TOOL_BYTES {
        return Err(http_tool_error(
            "Codex HTTP pending tool bytes exceeded safe limits",
        ));
    }
    Ok(bytes)
}

pub(super) fn http_tool_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_owned(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}
