use std::collections::HashMap;

use base64::Engine;
use ring::digest;
use serde_json::Value;

use super::request::ResponsesInputItem;

const FAMILY_PREFIX: &str = "duolane:";
const CODEX_FAMILY_PREFIX: &str = "duolane:codex:";
const PREFIX: &str = "duolane:codex:v1:";
const ACCOUNT_TAG_BYTES: usize = 8;
const MAX_ID_BYTES: usize = 4 * 1024;
const MAX_ENCRYPTED_CONTENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn is_proxy_reasoning_signature(signature: &str) -> bool {
    signature.starts_with(FAMILY_PREFIX)
}

pub(crate) fn is_codex_signature(signature: &str) -> bool {
    signature.starts_with(CODEX_FAMILY_PREFIX)
}

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct ReasoningAccount {
    tag: Option<String>,
}

impl std::fmt::Debug for ReasoningAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReasoningAccount([REDACTED])")
    }
}

impl ReasoningAccount {
    pub fn new(account_id: Option<&str>) -> Self {
        Self {
            tag: account_id.filter(|id| !id.is_empty()).map(account_tag),
        }
    }

    pub fn for_api_key(key: &[u8]) -> Self {
        let mut context = digest::Context::new(&digest::SHA256);
        context.update(b"openai-api-key:");
        context.update(key);
        let tag = context
            .finish()
            .as_ref()
            .iter()
            .take(ACCOUNT_TAG_BYTES)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self { tag: Some(tag) }
    }

    pub(crate) fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    pub fn sign(&self, replay: &ReasoningReplay) -> Option<String> {
        encode_reasoning_signature(self.tag.as_deref()?, replay)
    }

    pub fn replay(&self, signature: Option<&str>) -> Option<ReasoningReplay> {
        decode_reasoning_signature(self.tag.as_deref()?, signature?)
    }
}

fn account_tag(account_id: &str) -> String {
    digest::digest(&digest::SHA256, account_id.as_bytes())
        .as_ref()
        .iter()
        .take(ACCOUNT_TAG_BYTES)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningReplay {
    pub id: String,
    pub encrypted_content: String,
}

impl ReasoningReplay {
    pub fn into_input_item(self) -> ResponsesInputItem {
        ResponsesInputItem::Reasoning {
            id: self.id,
            summary: Vec::new(),
            encrypted_content: self.encrypted_content,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PendingReasoning {
    id: Option<String>,
    encrypted_content: Option<String>,
}

impl PendingReasoning {
    pub fn capture(&mut self, item: &Value) {
        if let Some(id) = non_empty_string(item.get("id")) {
            self.id = Some(id.to_string());
        }
        if let Some(encrypted_content) = non_empty_string(item.get("encrypted_content")) {
            self.encrypted_content = Some(encrypted_content.to_string());
        }
    }

    pub fn replay(&self) -> Option<ReasoningReplay> {
        Some(ReasoningReplay {
            id: self.id.clone()?,
            encrypted_content: self.encrypted_content.clone()?,
        })
    }
}

#[derive(Debug, Default)]
pub struct ReasoningSigner {
    account: ReasoningAccount,
    pending: HashMap<usize, PendingReasoning>,
}

impl ReasoningSigner {
    pub fn new(account: ReasoningAccount) -> Self {
        Self {
            account,
            pending: HashMap::new(),
        }
    }

    pub fn capture(&mut self, output_index: usize, item: &Value) {
        self.pending.entry(output_index).or_default().capture(item);
    }

    pub fn take(&mut self, output_index: usize) -> Option<(ReasoningReplay, String)> {
        let replay = self.pending.remove(&output_index)?.replay()?;
        let signature = self.account.sign(&replay)?;
        Some((replay, signature))
    }
}

pub fn encode_reasoning_signature(account_tag: &str, replay: &ReasoningReplay) -> Option<String> {
    if replay.id.is_empty()
        || replay.id.len() > MAX_ID_BYTES
        || replay.encrypted_content.is_empty()
        || replay.encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let encoded_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(replay.id.as_bytes());
    Some(format!(
        "{PREFIX}{account_tag}:{encoded_id}:{}",
        replay.encrypted_content
    ))
}

pub fn decode_reasoning_signature(account_tag: &str, signature: &str) -> Option<ReasoningReplay> {
    let payload = signature
        .strip_prefix(PREFIX)?
        .strip_prefix(account_tag)?
        .strip_prefix(':')?;
    if payload.is_empty() || payload.len() > max_payload_len() {
        return None;
    }
    let (encoded_id, encrypted_content) = payload.split_once(':')?;
    if encoded_id.is_empty()
        || encoded_id.len() > encoded_id_len_limit()
        || encrypted_content.is_empty()
        || encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded_id)
        .ok()?;
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return None;
    }
    Some(ReasoningReplay {
        id: String::from_utf8(id).ok()?,
        encrypted_content: encrypted_content.to_string(),
    })
}

fn encoded_id_len_limit() -> usize {
    MAX_ID_BYTES.div_ceil(3) * 4
}

fn max_payload_len() -> usize {
    encoded_id_len_limit() + 1 + MAX_ENCRYPTED_CONTENT_BYTES
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn replay() -> ReasoningReplay {
        ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "gAAAAopaque".to_string(),
        }
    }

    #[test]
    fn signature_carries_the_account_tag_and_replays_only_for_that_account() {
        let account = ReasoningAccount::new(Some("acct"));
        let signature = account.sign(&replay()).unwrap();
        assert_eq!(
            signature,
            "duolane:codex:v1:def0b17f603285ef:cnNfMQ:gAAAAopaque"
        );
        assert_eq!(account.replay(Some(&signature)), Some(replay()));
        assert_eq!(
            ReasoningAccount::new(Some("acct_b")).replay(Some(&signature)),
            None
        );
        assert_eq!(ReasoningAccount::default().replay(Some(&signature)), None);
    }

    #[test]
    fn unknown_account_issues_no_signature() {
        assert_eq!(ReasoningAccount::new(None).sign(&replay()), None);
        assert_eq!(ReasoningAccount::new(Some("")).sign(&replay()), None);
    }

    #[test]
    fn foreign_and_malformed_signatures_are_ignored() {
        let account = ReasoningAccount::new(Some("acct"));
        for signature in [
            "anthropic-signature",
            "ccp:codex:v1:cnNfMQ:gAAAAopaque",
            "duolane:codex:v1:def0b17f603285ef:not-base64!:x",
            "duolane:codex:v1:def0b17f603285ef:cnNfMQ:",
            "duolane:codex:v1:def0b17f603285efcnNfMQ:gAAAAopaque",
        ] {
            assert_eq!(account.replay(Some(signature)), None, "{signature}");
        }
        assert!(is_proxy_reasoning_signature("duolane:codex:v2:x"));
        assert!(!is_proxy_reasoning_signature("ccp:codex:v1:x"));
    }

    #[test]
    fn reasoning_account_debug_redacts_tags() {
        for account in [
            ReasoningAccount::for_api_key(b"toy-key"),
            ReasoningAccount::new(Some("acct")),
        ] {
            let formatted = format!("{account:?}");
            assert!(!formatted.contains(account.tag().unwrap()));
        }
    }

    #[test]
    fn api_key_reasoning_is_scoped_to_key_and_source() {
        let account = ReasoningAccount::for_api_key(b"toy-key");
        let signature = account.sign(&replay()).unwrap();
        assert_eq!(account.replay(Some(&signature)), Some(replay()));
        assert_eq!(
            ReasoningAccount::for_api_key(b"other-key").replay(Some(&signature)),
            None
        );
        assert_eq!(
            ReasoningAccount::new(Some("toy-key")).replay(Some(&signature)),
            None
        );
        assert!(account.tag() == Some("6426a6369ead0707"));
    }

    #[test]
    fn pending_reasoning_keeps_early_metadata_when_done_omits_it() {
        let mut pending = PendingReasoning::default();
        pending.capture(&json!({
            "id": "rs_1",
            "encrypted_content": "early"
        }));
        pending.capture(&json!({"id": "rs_1"}));
        assert_eq!(
            pending.replay(),
            Some(ReasoningReplay {
                id: "rs_1".to_string(),
                encrypted_content: "early".to_string(),
            })
        );
    }

    #[test]
    fn oversized_signature_is_ignored_without_decoding() {
        let signature = format!(
            "{PREFIX}def0b17f603285ef:cnNfMQ:{}",
            "A".repeat(MAX_ENCRYPTED_CONTENT_BYTES + 1)
        );
        assert_eq!(
            decode_reasoning_signature("def0b17f603285ef", &signature),
            None
        );
    }
}
