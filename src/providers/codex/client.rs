use super::auth::constants::CODEX_API_ENDPOINT;
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{DefaultCodexAuthStore, StoredAuth, file_store};
use super::client_body::{CodexHttpBody, CodexHttpBodyCache};
use super::client_capture::{
    write_upstream_response_capture, write_upstream_response_headers_capture,
};
pub use super::client_error::*;
pub use super::client_headers::*;
use super::client_proxy::*;
pub use super::events::{CodexLimitWindow, CodexUsageLimit};
use super::translate::reasoning_signature::ReasoningAccount;
use super::translate::request::ResponsesRequest;
use crate::config;
use crate::logging::create_logger;
use crate::provider::{MAX_BUFFERED_RESPONSE_BYTES, RequestContext};
use std::time::{Duration, Instant};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualTransport {
    Http,
    WebSocket,
}
pub struct CodexResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub transport: ActualTransport,
}
pub type CodexHttpEventReceiver =
    tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>;
pub(crate) const MAX_UPSTREAM_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub(crate) struct OwnerAwareCodexResponse {
    response: CodexResponse,
    pub(crate) socket_id: Option<u64>,
    pub(super) hosted_tool_executed: bool,
}
impl OwnerAwareCodexResponse {
    pub(crate) fn new(response: CodexResponse, socket_id: Option<u64>) -> Self {
        Self {
            response,
            socket_id,
            hosted_tool_executed: false,
        }
    }
    pub(super) fn into_response(self) -> CodexResponse {
        self.response
    }
}
impl std::ops::Deref for OwnerAwareCodexResponse {
    type Target = CodexResponse;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}
pub(super) const MAX_BUFFERED_TRANSPORT_RETRIES: u32 = 3;
pub(super) const MAX_BUFFERED_TRANSPORT_ATTEMPTS: u32 = MAX_BUFFERED_TRANSPORT_RETRIES + 1;
const HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS: u64 = 300_000;
pub(super) const IMAGE_HEADER_TIMEOUT_MS: u64 = 300_000;
pub(super) const HTTP_RESPONSE_HEADERS_DETAIL: &str = "http_response_headers";
pub(super) const LOOPBACK_NO_PROXY: &str = "localhost,127.0.0.0/8,::1,::ffff:127.0.0.0/104";
pub struct CodexHttpClient {
    pub(super) client: reqwest::Client,
    pub(super) native_client: reqwest::Client,
    pub(super) websocket_client: reqwest::Client,
    pub(super) websocket_proxy_config: super::websocket::WebSocketProxyConfig,
    pub(super) auto_http_fallback_enabled: bool,
    pub(super) auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
    pub(super) base_url: String,
    pub(super) header_timeout_ms: u64,
    pub(super) body_idle_timeout_ms: u64,
    pub(super) request_compression: config::CodexRequestCompression,
    pub(super) auto_ws_max_bytes: usize,
    #[cfg(test)]
    pub(super) buffered_response_limit: usize,
}
impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
}
impl super::CodexProvider {
    pub async fn current_credential_snapshot(
        &self,
    ) -> anyhow::Result<Option<super::auth::manager::ObservedCredential>> {
        self.client
            .auth_manager()
            .current_credential_snapshot()
            .await
    }
}
impl CodexHttpClient {
    pub fn new() -> Self {
        let timeout_ms = config::codex_header_timeout_ms();
        let proxy_environment = ProxyEnvironment::from_env();
        Self {
            client: codex_http_client(&proxy_environment),
            native_client: codex_http_client(&proxy_environment),
            websocket_client: websocket_http_client(&proxy_environment),
            websocket_proxy_config: proxy_environment.websocket_proxy_config(),
            auto_http_fallback_enabled: true,
            auth_manager: CodexAuthManager::new(file_store()),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            request_compression: config::codex_request_compression(),
            auto_ws_max_bytes: config::codex_auto_ws_max_bytes(),
            #[cfg(test)]
            buffered_response_limit: MAX_BUFFERED_RESPONSE_BYTES,
        }
    }
    pub fn new_with_client(
        client: reqwest::Client,
        auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
        base_url: String,
    ) -> Self {
        let proxy_environment = ProxyEnvironment::from_env();
        let websocket_proxy_config = proxy_environment.websocket_proxy_config();
        let auto_http_fallback_enabled =
            custom_client_auto_http_fallback_enabled(&base_url, &websocket_proxy_config);
        Self {
            native_client: codex_http_client(&proxy_environment),
            websocket_client: websocket_http_client(&proxy_environment),
            websocket_proxy_config,
            auto_http_fallback_enabled,
            client,
            auth_manager,
            base_url,
            header_timeout_ms: config::codex_header_timeout_ms(),
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            request_compression: config::codex_request_compression(),
            auto_ws_max_bytes: config::codex_auto_ws_max_bytes(),
            #[cfg(test)]
            buffered_response_limit: MAX_BUFFERED_RESPONSE_BYTES,
        }
    }
    #[cfg(test)]
    pub fn new_for_test(
        client: reqwest::Client,
        base_url: String,
        header_timeout_ms: u64,
        body_idle_timeout_ms: u64,
    ) -> Self {
        Self {
            native_client: test_native_http_client(),
            websocket_client: test_websocket_http_client(),
            websocket_proxy_config: super::websocket::WebSocketProxyConfig::direct(),
            auto_http_fallback_enabled: true,
            client,
            auth_manager: CodexAuthManager::new(file_store()),
            base_url,
            header_timeout_ms,
            body_idle_timeout_ms,
            request_compression: config::CodexRequestCompression::Off,
            auto_ws_max_bytes: 4 * 1024 * 1024,
            buffered_response_limit: MAX_BUFFERED_RESPONSE_BYTES,
        }
    }
    pub fn auth_manager(&self) -> &CodexAuthManager<DefaultCodexAuthStore> {
        &self.auth_manager
    }
    pub(super) async fn auth_for(
        &self,
        account: &ReasoningAccount,
    ) -> Result<StoredAuth, CodexError> {
        let auth = self.auth_manager.get_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        bind_account(account, auth)
    }
    pub(super) async fn refreshed_auth(
        &self,
        current: &StoredAuth,
    ) -> Result<StoredAuth, CodexError> {
        let account = ReasoningAccount::new(current.account_id.as_deref());
        let auth = self
            .auth_manager
            .force_refresh(&current.access)
            .await
            .map_err(auth_refresh_error)?;
        bind_account(&account, auth)
    }
    pub(super) async fn start_http_event_attempt(
        &self,
        auth: &mut StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        use_responses_lite: bool,
        auth_refresh_attempted: &mut bool,
        tracked: bool,
    ) -> Result<(reqwest::Response, Vec<(String, String)>, Instant), CodexError> {
        loop {
            if tracked {
                ctx.recovery_budget.begin_http_attempt();
            }
            let result = if tracked {
                self.start_post_http_inner(auth, http_body, ctx, use_responses_lite, true)
                    .await
            } else {
                self.start_post_http(auth, http_body, ctx, use_responses_lite)
                    .await
            };
            if tracked
                && result.is_err()
                && ctx.recovery_budget.send() == super::api::SendEvidence::OutcomeUnknown
            {
                ctx.recovery_budget.mark_ambiguous_hosted();
            }
            let (resp, started_at) = result?;
            if tracked && resp.status().is_server_error() {
                ctx.recovery_budget.mark_ambiguous_hosted();
            }

            if resp.status().as_u16() == 401 && !*auth_refresh_attempted {
                if tracked {
                    let mut hosted = false;
                    let response = self
                        .collect_http_response_inner(resp, started_at, ctx, &mut hosted, true)
                        .await
                        .inspect_err(|_| ctx.recovery_budget.mark_ambiguous_hosted())?;
                    if hosted
                        || explicit_permanent_buffered_error(&response.body).is_some()
                        || codex_status_error_message(&response.body).is_some_and(|message| {
                            super::events::is_context_overflow(None, &message)
                        })
                    {
                        ctx.recovery_budget.mark_final_only();
                        let mut error = codex_status_error(response);
                        error.origin = CodexErrorOrigin::Http;
                        return Err(error);
                    }
                }
                *auth_refresh_attempted = true;
                ctx.recovery_budget.start();
                let refreshed = if tracked {
                    self.refreshed_auth_buffered_http(auth, &ctx.recovery_budget)
                        .await
                } else {
                    self.refreshed_auth(auth).await
                };
                *auth = refreshed.inspect_err(|error| {
                    if error.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL) {
                        ctx.recovery_budget.cancel_unspent_start();
                    }
                })?;
                if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }

            if !resp.status().is_success() {
                let mut hosted = false;
                let response = if tracked {
                    self.collect_http_response_inner(resp, started_at, ctx, &mut hosted, true)
                        .await
                        .inspect_err(|_| ctx.recovery_budget.mark_ambiguous_hosted())?
                } else {
                    self.collect_http_response(resp, started_at, ctx, &mut hosted)
                        .await?
                };
                if tracked {
                    if explicit_permanent_buffered_error(&response.body).is_some()
                        || codex_status_error_message(&response.body).is_some_and(|message| {
                            super::events::is_context_overflow(None, &message)
                        })
                    {
                        ctx.recovery_budget.mark_final_only();
                        let mut error = codex_status_error(response);
                        error.origin = CodexErrorOrigin::Http;
                        return Err(error);
                    }
                    if super::events::usage_limit_from_response(&response.body, &response.headers)
                        .is_some()
                    {
                        ctx.recovery_budget
                            .set_kind(super::sources::FailureKind::UsageLimit);
                        if super::recovery::RecoveryBudget::quota_denied_before_execution(
                            response.status,
                            &response.body,
                            hosted,
                        ) {
                            ctx.recovery_budget
                                .set_send(super::api::SendEvidence::DeniedBeforeExecution);
                        } else {
                            ctx.recovery_budget.mark_ambiguous_hosted();
                        }
                    }
                }
                let mut error = codex_status_error(response);
                error.origin = CodexErrorOrigin::Http;
                return Err(error);
            }

            let status = resp.status().as_u16();
            let headers = response_headers(&resp);
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_upstream_response_headers_capture(
                    traffic,
                    status,
                    started_at.elapsed(),
                    &headers,
                );
            }
            return Ok((resp, headers, started_at));
        }
    }

    pub(super) async fn auth_for_buffered_http(
        &self,
        account: &ReasoningAccount,
        budget: &super::recovery::RecoveryBudget,
    ) -> Result<StoredAuth, CodexError> {
        let auth = self.auth_manager.get_auth().await.map_err(|error| {
            trace_auth_failure(budget, &error);
            CodexError {
                status: 401,
                message: "Auth error".to_string(),
                detail: Some(error.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Auth,
            }
        })?;
        let auth = bind_account(account, auth)?;
        budget.set_credential(super::sources::CredentialId::for_chatgpt_auth(&auth));
        Ok(auth)
    }

    pub(super) async fn refreshed_auth_buffered_http(
        &self,
        current: &StoredAuth,
        budget: &super::recovery::RecoveryBudget,
    ) -> Result<StoredAuth, CodexError> {
        let account = ReasoningAccount::new(current.account_id.as_deref());
        let auth = self
            .auth_manager
            .force_refresh(&current.access)
            .await
            .map_err(|error| {
                trace_auth_failure(budget, &error);
                auth_refresh_error(error)
            })?;
        let auth = bind_account(&account, auth)?;
        budget.set_credential(super::sources::CredentialId::for_chatgpt_auth(&auth));
        Ok(auth)
    }

    pub(super) async fn read_buffered_body(
        &self,
        resp: &mut reqwest::Response,
        ctx: &RequestContext,
        label: &str,
        hosted_http: &mut bool,
        terminal: &mut bool,
    ) -> Result<Vec<u8>, CodexError> {
        self.read_buffered_body_inner(resp, ctx, label, hosted_http, terminal, false)
            .await
    }

    pub(super) async fn read_buffered_body_tracked(
        &self,
        resp: &mut reqwest::Response,
        ctx: &RequestContext,
        label: &str,
        hosted_http: &mut bool,
        terminal: &mut bool,
    ) -> Result<Vec<u8>, CodexError> {
        self.read_buffered_body_inner(resp, ctx, label, hosted_http, terminal, true)
            .await
    }

    async fn read_buffered_body_inner(
        &self,
        resp: &mut reqwest::Response,
        ctx: &RequestContext,
        label: &str,
        hosted_http: &mut bool,
        terminal: &mut bool,
        tracked: bool,
    ) -> Result<Vec<u8>, CodexError> {
        let body_error = |message: String| CodexError {
            status: 0,
            message,
            detail: Some("http_response_body".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        };
        let mut body = Vec::new();
        let mut response_started = false;
        let mut evidence = super::client_sse::BufferedHttpSseEvidence::default();
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(self.body_idle_timeout_ms),
                resp.chunk(),
            )
            .await;
            let chunk = match chunk {
                Ok(Ok(chunk)) => Ok(chunk),
                Ok(Err(error)) => Err(body_error(format!(
                    "Transport error reading {label}: {error}"
                ))),
                Err(_) => Err(body_error(format!(
                    "Timed out waiting {}ms for the next {label} chunk",
                    self.body_idle_timeout_ms
                ))),
            };
            if tracked && !matches!(chunk, Ok(Some(_))) {
                evidence.finish_hosted();
                *hosted_http |= evidence.hosted;
                if evidence.hosted {
                    ctx.recovery_budget.mark_hosted();
                }
            }
            let Some(chunk) = chunk? else {
                return Ok(body);
            };
            evidence.push(&chunk);
            *hosted_http |= evidence.hosted;
            if tracked && evidence.hosted {
                ctx.recovery_budget.mark_hosted();
            }
            *terminal |= evidence.terminal;
            #[cfg(test)]
            let body_limit = self.buffered_response_limit;
            #[cfg(not(test))]
            let body_limit = MAX_BUFFERED_RESPONSE_BYTES;
            if body.len().saturating_add(chunk.len()) > body_limit {
                if tracked {
                    evidence.finish_hosted();
                    *hosted_http |= evidence.hosted;
                    if evidence.hosted {
                        ctx.recovery_budget.mark_hosted();
                    }
                    ctx.recovery_budget.mark_final_only();
                }
                return Err(body_error(format!("{label} exceeds the size limit")));
            }
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            body.extend_from_slice(&chunk);
        }
    }

    pub(super) async fn collect_http_response(
        &self,
        resp: reqwest::Response,
        started_at: Instant,
        ctx: &RequestContext,
        hosted_http: &mut bool,
    ) -> Result<CodexResponse, CodexError> {
        self.collect_http_response_inner(resp, started_at, ctx, hosted_http, false)
            .await
    }

    pub(super) async fn collect_http_response_inner(
        &self,
        mut resp: reqwest::Response,
        started_at: Instant,
        ctx: &RequestContext,
        hosted_http: &mut bool,
        tracked: bool,
    ) -> Result<CodexResponse, CodexError> {
        let status = resp.status();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let mut terminal = false;
        let body_result = if tracked {
            self.read_buffered_body_tracked(
                &mut resp,
                ctx,
                "Codex response body",
                hosted_http,
                &mut terminal,
            )
            .await
        } else {
            self.read_buffered_body(
                &mut resp,
                ctx,
                "Codex response body",
                hosted_http,
                &mut terminal,
            )
            .await
        };
        let body_bytes = body_result.map_err(|mut error| {
            if !status.is_success() {
                error.status = status.as_u16();
                error.detail = Some(error.message.clone());
                error.retry_after = headers.iter().find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("retry-after")
                        .then(|| value.clone())
                });
            }
            error
        })?;
        if status.is_success()
            && *hosted_http
            && !terminal
            && !(tracked
                && super::events::usage_limit_from_response(&body_bytes, &headers).is_some())
        {
            return Err(super::client_sse::http_sse_error(
                "Codex SSE stream ended without a terminal event",
            ));
        }
        let status = status.as_u16();

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(
                traffic,
                status,
                started_at.elapsed(),
                &headers,
                &body_bytes,
            );
        }

        Ok(CodexResponse {
            body: body_bytes,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }

    pub fn body_idle_timeout_ms(&self) -> u64 {
        self.body_idle_timeout_ms
    }

    pub async fn post_codex(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<CodexResponse, CodexError> {
        let reservation =
            continuation.map(super::continuation::ContinuationReservation::from_public_candidate);
        self.post_codex_with_transport(
            body,
            ctx,
            reservation.as_ref(),
            crate::config::codex_transport(),
            &CodexHttpBodyCache::new(),
        )
        .await
        .map(OwnerAwareCodexResponse::into_response)
    }

    pub(super) async fn post_codex_for_owner(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        transport: crate::config::CodexTransport,
        http_body: &CodexHttpBodyCache,
    ) -> Result<OwnerAwareCodexResponse, CodexError> {
        self.post_codex_with_transport(body, ctx, continuation, transport, http_body)
            .await
    }
}

pub(super) fn response_headers(resp: &reqwest::Response) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect()
}

pub(super) fn explicit_permanent_buffered_error(
    body: &[u8],
) -> Option<super::events::CodexEventFailure> {
    let direct = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .into_iter();
    let events = crate::anthropic::sse::parse_sse_events(body)
        .into_iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok());
    direct.chain(events).find_map(|mut payload| {
        let error = super::events::event_error(&payload)?;
        let code = error.get("code").and_then(serde_json::Value::as_str);
        let kind = error.get("type").and_then(serde_json::Value::as_str);
        let explicit = matches!(
            code,
            Some(
                "context_length_exceeded"
                    | "invalid_prompt"
                    | "cyber_policy"
                    | "bio_policy"
                    | "insufficient_quota"
                    | "usage_not_included"
            )
        ) || kind == Some("invalid_request_error");
        if !explicit {
            return None;
        }
        if payload.get("type").is_none()
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("type".to_string(), serde_json::json!("error"));
        }
        let mut failure = super::events::classify_event_failure(&payload)?;
        failure.kind = super::events::CodexFailureKind::Permanent;
        failure.status = 400;
        Some(failure)
    })
}

pub(super) fn trace_auth_failure(budget: &super::recovery::RecoveryBudget, error: &anyhow::Error) {
    if let super::auth::manager::AuthFailure::Rejected(id) =
        super::auth::manager::classify_auth_failure(error)
    {
        budget.stamp(
            super::sources::FailureKind::RejectedCredential,
            super::api::SendEvidence::NotSent,
            Some(id),
        );
    }
}

fn bind_account(account: &ReasoningAccount, auth: StoredAuth) -> Result<StoredAuth, CodexError> {
    if *account == ReasoningAccount::new(auth.account_id.as_deref()) {
        return Ok(auth);
    }
    Err(CodexError {
        status: 409,
        message: "The Codex login changed during this request; retry it".to_string(),
        detail: Some(CODEX_ACCOUNT_CHANGED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Auth,
    })
}

pub(super) fn log_buffered_retry(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    failed_attempt: u32,
    delay_ms: u64,
    status: u16,
    origin: &str,
    reason: &str,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("transport".into(), serde_json::json!(transport.as_str()));
    fields.insert("failedAttempt".into(), serde_json::json!(failed_attempt));
    fields.insert("nextAttempt".into(), serde_json::json!(failed_attempt + 1));
    fields.insert(
        "maxAttempts".into(),
        serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
    );
    fields.insert("delayMs".into(), serde_json::json!(delay_ms));
    fields.insert("status".into(), serde_json::json!(status));
    fields.insert("origin".into(), serde_json::json!(origin));
    fields.insert("reason".into(), serde_json::json!(reason));
    create_logger("codex").warn("buffered_transport_retry", Some(fields));
}

pub(super) fn log_buffered_retry_exhausted(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    status: u16,
    origin: &str,
    reason: &str,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("transport".into(), serde_json::json!(transport.as_str()));
    fields.insert(
        "attempts".into(),
        serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
    );
    fields.insert("status".into(), serde_json::json!(status));
    fields.insert("origin".into(), serde_json::json!(origin));
    fields.insert("reason".into(), serde_json::json!(reason));
    create_logger("codex").warn("buffered_transport_retry_exhausted", Some(fields));
}
