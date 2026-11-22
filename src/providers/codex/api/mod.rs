mod catalog;
mod error;
pub mod messages;
#[cfg(test)]
mod messages_tests;
mod sanitize;
#[cfg(test)]
mod tests;

use anyhow::Result;
pub use error::{ApiFailure, ApiFailureKind, SendEvidence};
use reqwest::{
    Response,
    header::{AUTHORIZATION, HeaderValue},
};
use serde_json::Value;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::time::{Instant, timeout};
use url::Url;

use super::{
    auth::api_key::OpenAiKey,
    client_proxy::{ProxyEnvironment, codex_http_client},
    client_sse::{DecodedHttpSseEvent, HttpSseDecoder},
    events,
    sources::CredentialId,
    translate::reasoning_signature::ReasoningAccount,
};
use crate::config::codex_auth::AuthSource;
use catalog::{Catalog, REFRESH_INTERVAL};

const MAX_BODY: usize = 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ApiClient {
    client: reqwest::Client,
    base: Url,
    authorization: HeaderValue,
    credential: CredentialId,
    reasoning_account: ReasoningAccount,
    catalog: Catalog,
    header_timeout: Duration,
}

impl ApiClient {
    pub fn new(base: Url, key: OpenAiKey) -> Result<Self> {
        let credential = CredentialId::new(AuthSource::ApiKey, key.for_hashing());
        let reasoning_account = ReasoningAccount::for_api_key(key.for_hashing());
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", key.for_sending()))
            .map_err(|_| anyhow::anyhow!("Invalid API credential"))?;
        authorization.set_sensitive(true);
        let client = codex_http_client(&ProxyEnvironment::from_env());
        Ok(Self {
            client,
            base,
            authorization,
            credential,
            reasoning_account,
            catalog: Catalog::default(),
            header_timeout: Duration::from_millis(crate::config::codex_header_timeout_ms()),
        })
    }

    pub fn credential_id(&self) -> CredentialId {
        self.credential
    }

    pub fn reasoning_account(&self) -> &ReasoningAccount {
        &self.reasoning_account
    }

    fn endpoint(&self, endpoint: &str) -> Result<Url, ApiFailure> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| ApiFailure::local(ApiFailureKind::Other, SendEvidence::NotSent, false))?
            .pop_if_empty()
            .push(endpoint);
        Ok(url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Response, ApiFailure> {
        let request = request
            .header(AUTHORIZATION, self.authorization.clone())
            .build()
            .map_err(|_| ApiFailure::local(ApiFailureKind::Other, SendEvidence::NotSent, false))?;
        match timeout(self.header_timeout, self.client.execute(request)).await {
            Err(_) => Err(ApiFailure::local(
                ApiFailureKind::HeaderTimeout,
                SendEvidence::OutcomeUnknown,
                false,
            )),
            Ok(Err(error)) => Err(ApiFailure::local(
                ApiFailureKind::Transient,
                if error.is_connect() {
                    SendEvidence::NotSent
                } else {
                    SendEvidence::OutcomeUnknown
                },
                false,
            )),
            Ok(Ok(response)) => Ok(response),
        }
    }

    async fn models(&self) -> Result<Vec<u8>, ApiFailure> {
        let response = self.send(self.client.get(self.endpoint("models")?)).await?;
        let status = response.status().as_u16();
        let retry_after = retry_hint(&response);
        let bytes = read_bounded(response).await;
        if status != 200 {
            let value = bytes
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let mut failure = ApiFailure::upstream(status, value.as_ref(), false, false);
            if matches!(
                failure.kind,
                ApiFailureKind::Quota | ApiFailureKind::Transient
            ) {
                failure.error.retry_after = retry_after;
            }
            return Err(failure);
        }
        bytes
    }

    pub async fn verify(&self) -> Result<(), ApiFailure> {
        self.models().await.map(|_| ())
    }

    pub async fn refresh_models(&self) {
        let mut attempt = self.catalog.attempt.lock().await;
        let now = Instant::now();
        if attempt.is_some_and(|last| now.duration_since(last) < REFRESH_INTERVAL) {
            return;
        }
        *attempt = Some(now);
        if let Ok(bytes) = self.models().await {
            self.catalog.replace(&bytes);
        }
    }

    pub fn catalog(&self) -> Option<Arc<[String]>> {
        self.catalog.snapshot()
    }

    pub async fn responses(&self, body: &Value) -> Result<ApiEvents, ApiFailure> {
        let response = self
            .send(self.client.post(self.endpoint("responses")?).json(body))
            .await?;
        let status = response.status().as_u16();
        if status != 200 {
            let retry_after = retry_hint(&response);
            let value = read_bounded(response)
                .await
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let mut failure = ApiFailure::upstream(status, value.as_ref(), false, false);
            if matches!(
                failure.kind,
                ApiFailureKind::Quota | ApiFailureKind::Transient
            ) {
                failure.error.retry_after = retry_after;
            }
            return Err(failure);
        }
        Ok(ApiEvents {
            response,
            decoder: HttpSseDecoder::default(),
            pending: VecDeque::new(),
            chunk: None,
            hosted: false,
            terminal: false,
            ended: false,
        })
    }
}

fn retry_hint(response: &Response) -> Option<String> {
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u32>()
        .ok()?;
    (seconds <= 86_400).then(|| seconds.to_string())
}

async fn read_bounded(mut response: Response) -> Result<Vec<u8>, ApiFailure> {
    timeout(BODY_TIMEOUT, async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            ApiFailure::local(ApiFailureKind::Other, SendEvidence::OutcomeUnknown, false)
        })? {
            if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
                return Err(ApiFailure::local(
                    ApiFailureKind::Other,
                    SendEvidence::OutcomeUnknown,
                    false,
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ApiFailure::local(ApiFailureKind::Other, SendEvidence::OutcomeUnknown, false))?
}

pub struct ApiEvents {
    response: Response,
    decoder: HttpSseDecoder,
    pending: VecDeque<DecodedHttpSseEvent>,
    chunk: Option<bytes::Bytes>,
    hosted: bool,
    terminal: bool,
    ended: bool,
}

impl ApiEvents {
    pub async fn next(&mut self) -> Option<Result<Value, ApiFailure>> {
        use bytes::Buf;
        loop {
            if self.ended {
                return None;
            }
            if let Some(event) = self.pending.pop_front() {
                let Some(payload) = event.payload else {
                    if matches!(
                        event.event.as_deref(),
                        Some("error" | "response.error" | "response.failed")
                    ) {
                        self.ended = true;
                        return Some(Err(ApiFailure::local(
                            ApiFailureKind::Other,
                            SendEvidence::OutcomeUnknown,
                            self.hosted,
                        )));
                    }
                    continue;
                };
                self.hosted |= events::event_confirms_hosted_tool_execution(&payload)
                    || payload
                        .pointer("/response/output")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item.get("type").and_then(Value::as_str) == Some("web_search_call")
                            })
                        });
                let event_type = payload
                    .get("type")
                    .and_then(Value::as_str)
                    .or(event.event.as_deref());
                if matches!(
                    event_type,
                    Some("error" | "response.error" | "response.failed")
                ) || matches!(
                    event.event.as_deref(),
                    Some("error" | "response.error" | "response.failed")
                ) || payload.pointer("/response/status").and_then(Value::as_str)
                    == Some("failed")
                    || events::event_error(&payload).is_some()
                {
                    self.ended = true;
                    let status = events::numeric_status(&payload)
                        .or_else(|| {
                            events::event_error(&payload)
                                .and_then(|error| error.get("status"))
                                .and_then(Value::as_u64)
                        })
                        .and_then(|s| u16::try_from(s).ok())
                        .unwrap_or(0);
                    return Some(Err(ApiFailure::upstream(
                        status,
                        Some(&payload),
                        self.hosted,
                        true,
                    )));
                }
                if events::response_is_incomplete_terminal(&payload)
                    && !events::is_standard_max_output_tokens_incomplete(&payload)
                {
                    self.ended = true;
                    return Some(Err(ApiFailure::local(
                        ApiFailureKind::Transient,
                        SendEvidence::OutcomeUnknown,
                        self.hosted,
                    )));
                }
                let payload = match sanitize::event(&payload) {
                    Ok(Some(payload)) => payload,
                    Ok(None) => continue,
                    Err(()) => {
                        self.ended = true;
                        return Some(Err(ApiFailure::local(
                            ApiFailureKind::Other,
                            SendEvidence::OutcomeUnknown,
                            self.hosted,
                        )));
                    }
                };
                self.terminal |= events::event_is_success_terminal(&payload);
                return Some(Ok(payload));
            }
            if let Some(chunk) = &mut self.chunk {
                if !chunk.is_empty() {
                    let n = chunk[..chunk.len().min(8192)]
                        .iter()
                        .position(|byte| matches!(byte, b'\n' | b'\r'))
                        .map_or(chunk.len().min(8192), |index| (index + 1).min(8192));
                    let mut decoded = Vec::new();
                    let result = self.decoder.push(&chunk[..n], &mut decoded);
                    chunk.advance(n);
                    self.pending.extend(decoded);
                    if result.is_err() {
                        self.ended = true;
                        return Some(Err(ApiFailure::local(
                            ApiFailureKind::Other,
                            SendEvidence::OutcomeUnknown,
                            self.hosted,
                        )));
                    }
                    continue;
                }
                self.chunk = None;
            }
            match timeout(BODY_TIMEOUT, self.response.chunk()).await {
                Ok(Ok(Some(chunk))) => self.chunk = Some(chunk),
                Ok(Ok(None)) => {
                    self.ended = true;
                    if self.decoder.finish().is_err() || !self.terminal {
                        return Some(Err(ApiFailure::local(
                            ApiFailureKind::Transient,
                            SendEvidence::OutcomeUnknown,
                            self.hosted,
                        )));
                    }
                    return None;
                }
                _ => {
                    self.ended = true;
                    return Some(Err(ApiFailure::local(
                        ApiFailureKind::Transient,
                        SendEvidence::OutcomeUnknown,
                        self.hosted,
                    )));
                }
            }
        }
    }
}
