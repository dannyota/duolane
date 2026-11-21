use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;

use super::auth::constants::CODEX_API_ENDPOINT;
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{DefaultCodexAuthStore, StoredAuth, file_store};
use super::client_body::CodexHttpBodyCache;
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

#[cfg(unix)]
struct ProfileManager {
    device: u64,
    inode: u64,
    manager: std::sync::Arc<CodexAuthManager<DefaultCodexAuthStore>>,
}

pub struct CodexHttpClient {
    pub(super) client: reqwest::Client,
    pub(super) native_client: reqwest::Client,
    pub(super) websocket_client: reqwest::Client,
    pub(super) websocket_proxy_config: super::websocket::WebSocketProxyConfig,
    pub(super) auto_http_fallback_enabled: bool,
    pub(super) auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
    #[cfg(unix)]
    profile_managers: std::sync::Mutex<std::collections::HashMap<String, ProfileManager>>,
    pub(super) base_url: String,
    pub(super) header_timeout_ms: u64,
    pub(super) body_idle_timeout_ms: u64,
    pub(super) request_compression: config::CodexRequestCompression,
    pub(super) auto_ws_max_bytes: usize,
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
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
            #[cfg(unix)]
            profile_managers: Default::default(),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            request_compression: config::codex_request_compression(),
            auto_ws_max_bytes: config::codex_auto_ws_max_bytes(),
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
            #[cfg(unix)]
            profile_managers: Default::default(),
            base_url,
            header_timeout_ms: config::codex_header_timeout_ms(),
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            request_compression: config::codex_request_compression(),
            auto_ws_max_bytes: config::codex_auto_ws_max_bytes(),
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
            #[cfg(unix)]
            profile_managers: Default::default(),
            base_url,
            header_timeout_ms,
            body_idle_timeout_ms,
            request_compression: config::CodexRequestCompression::Off,
            auto_ws_max_bytes: 4 * 1024 * 1024,
        }
    }

    pub fn manager_for_source(
        &self,
        source: super::profiles::AuthSource,
    ) -> anyhow::Result<Option<std::sync::Arc<CodexAuthManager<DefaultCodexAuthStore>>>> {
        match source {
            super::profiles::AuthSource::Default { .. } => Ok(None),
            #[cfg(unix)]
            super::profiles::AuthSource::Named { home, path, .. } => {
                use super::auth::token_store::{CodexAuthStore, CodexTokenStore};
                use std::os::unix::ffi::OsStrExt;
                use std::os::unix::fs::MetadataExt;
                let metadata = home.metadata()?;
                let digest =
                    ring::digest::digest(&ring::digest::SHA256, path.as_os_str().as_bytes());
                let key: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
                let mut managers = self
                    .profile_managers
                    .lock()
                    .map_err(|_| anyhow::anyhow!("profile manager lock poisoned"))?;
                if let Some(entry) = managers.get(&key)
                    && entry.device == metadata.dev()
                    && entry.inode == metadata.ino()
                {
                    return Ok(Some(entry.manager.clone()));
                }
                if managers.len() >= 16
                    && let Some(old) = managers.keys().next().cloned()
                {
                    managers.remove(&old);
                }
                let store = super::auth::named_home::NamedHomeAuthStore::new(
                    std::sync::Arc::new(home),
                    path,
                );
                let manager = std::sync::Arc::new(CodexAuthManager::new_isolated(
                    CodexTokenStore::new(CodexAuthStore::Home(store)),
                ));
                managers.insert(
                    key,
                    ProfileManager {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        manager: manager.clone(),
                    },
                );
                Ok(Some(manager))
            }
        }
    }

    pub fn auth_manager(&self) -> &CodexAuthManager<DefaultCodexAuthStore> {
        &self.auth_manager
    }

    pub(super) fn auth_manager_for<'a>(
        &'a self,
        ctx: &'a RequestContext,
    ) -> &'a CodexAuthManager<DefaultCodexAuthStore> {
        ctx.codex_profile
            .as_ref()
            .map_or(&self.auth_manager, |binding| &binding.manager)
    }

    pub(super) async fn auth_for(
        &self,
        account: &ReasoningAccount,
        ctx: &RequestContext,
    ) -> Result<StoredAuth, CodexError> {
        let auth = self
            .auth_manager_for(ctx)
            .get_auth()
            .await
            .map_err(|e| CodexError {
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
        ctx: &RequestContext,
    ) -> Result<StoredAuth, CodexError> {
        let account = ReasoningAccount::new(current.account_id.as_deref());
        let auth = self
            .auth_manager_for(ctx)
            .force_refresh(&current.access)
            .await
            .map_err(auth_refresh_error)?;
        bind_account(&account, auth)
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

#[cfg(all(test, unix))]
mod profile_manager_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn auth_uses_manager_bound_to_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = CodexHttpClient::new();
        let source = super::super::profiles::AuthSource::Named {
            home: std::fs::File::open(dir.path()).unwrap(),
            auth_name: "auth.json",
            path: dir.path().join("auth.json"),
        };
        let manager = client.manager_for_source(source).unwrap().unwrap();
        let mut auth = super::super::client_test_support::http_test_auth();
        auth.account_id = Some("named".into());
        manager.set_test_auth(auth);
        let mut ctx = super::super::client_test_support::http_test_context();
        ctx.codex_profile = Some(Arc::new(super::super::CodexProfileBinding {
            name: "named".into(),
            fingerprint: "test".into(),
            manager,
        }));
        assert!(
            client
                .auth_for(&ReasoningAccount::new(Some("named")), &ctx)
                .await
                .is_ok()
        );
    }

    #[test]
    fn managers_are_reused_only_for_the_same_fingerprint() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        let source = |dir: &tempfile::TempDir| super::super::profiles::AuthSource::Named {
            home: std::fs::File::open(dir.path()).unwrap(),
            auth_name: "auth.json",
            path: dir.path().join("auth.json"),
        };
        let client = CodexHttpClient::new();
        let a = client.manager_for_source(source(&first)).unwrap().unwrap();
        let b = client.manager_for_source(source(&first)).unwrap().unwrap();
        let c = client.manager_for_source(source(&second)).unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        let moved = first.path().with_extension("moved");
        std::fs::rename(first.path(), &moved).unwrap();
        std::fs::create_dir(first.path()).unwrap();
        let replacement = client.manager_for_source(source(&first)).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&a, &replacement));
        std::fs::remove_dir_all(moved).unwrap();
    }
}
