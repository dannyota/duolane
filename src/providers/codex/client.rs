use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;

use super::auth::constants::CODEX_API_ENDPOINT;
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{CodexTokenStore, DefaultCodexAuthStore, StoredAuth, file_store};
pub use super::client_error::*;
pub use super::client_headers::*;
use super::client_proxy::*;
pub use super::events::{CodexLimitWindow, CodexUsageLimit};
use super::translate::reasoning_signature::ReasoningAccount;
use super::translate::request::ResponsesRequest;

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
}

impl OwnerAwareCodexResponse {
    pub(crate) fn new(response: CodexResponse, socket_id: Option<u64>) -> Self {
        Self {
            response,
            socket_id,
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
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHttpClient {
    pub fn new() -> Self {
        Self::with_store(file_store(), None)
    }

    pub(crate) fn with_store(
        store: CodexTokenStore<DefaultCodexAuthStore>,
        profile: Option<ring::digest::Digest>,
    ) -> Self {
        let timeout_ms = config::codex_header_timeout_ms();
        let proxy_environment = ProxyEnvironment::from_env();
        Self {
            client: codex_http_client(&proxy_environment),
            native_client: codex_http_client(&proxy_environment),
            websocket_client: websocket_http_client(&proxy_environment),
            websocket_proxy_config: proxy_environment.websocket_proxy_config(),
            auto_http_fallback_enabled: true,
            auth_manager: CodexAuthManager::with_profile(store, profile),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
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
        self.bind_account(account, auth)
    }

    pub(super) async fn refreshed_auth(
        &self,
        current: &StoredAuth,
    ) -> Result<StoredAuth, CodexError> {
        let account = self
            .auth_manager
            .account_scope(current.account_id.as_deref());
        let auth = self
            .auth_manager
            .force_refresh(&current.access)
            .await
            .map_err(auth_refresh_error)?;
        self.bind_account(&account, auth)
    }

    fn bind_account(
        &self,
        account: &ReasoningAccount,
        auth: StoredAuth,
    ) -> Result<StoredAuth, CodexError> {
        if *account == self.auth_manager.account_scope(auth.account_id.as_deref()) {
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
        )
        .await
        .map(OwnerAwareCodexResponse::into_response)
    }

    pub(crate) async fn post_codex_for_owner(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<OwnerAwareCodexResponse, CodexError> {
        self.post_codex_with_transport(body, ctx, continuation, crate::config::codex_transport())
            .await
    }
}

pub(super) fn response_headers(resp: &reqwest::Response) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect()
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
