use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::AuthStorage;
use crate::config::codex_auth::AuthSource;
use crate::providers::codex::sources::{CredentialId, CredentialObservation};
use crate::providers::codex::translate::reasoning_signature::ReasoningAccount;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    Rejected(CredentialId),
    Transient,
    Other,
}

struct AuthFailureContext {
    kind: AuthFailure,
    message: String,
}

impl std::fmt::Display for AuthFailureContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::fmt::Debug for AuthFailureContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthFailureContext").finish_non_exhaustive()
    }
}

fn classified(error: anyhow::Error, kind: AuthFailure) -> anyhow::Error {
    let message = error.to_string();
    error.context(AuthFailureContext { kind, message })
}

#[cfg(test)]
pub(crate) fn test_transient_error() -> anyhow::Error {
    classified(anyhow::anyhow!("transient"), AuthFailure::Transient)
}

pub fn classify_auth_failure(error: &anyhow::Error) -> AuthFailure {
    error
        .downcast_ref::<AuthFailureContext>()
        .map_or(AuthFailure::Other, |context| context.kind)
}

const MAX_REFRESH_RESPONSE_BYTES: usize = 64 * 1024;
const SAVE_ATTEMPTS: u32 = 3;
const SAVE_RETRY_PAUSE: Duration = Duration::from_millis(100);

static CODEX_REFRESH_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));
static SNAPSHOT_SEQUENCE: Mutex<u64> = Mutex::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ObservedCredential {
    pub id: CredentialId,
    pub observation: CredentialObservation,
}

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: Arc<CodexTokenStore<S>>,
    #[cfg(test)]
    test_auth: Arc<Mutex<Option<StoredAuth>>>,
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_client: reqwest::Client,
    token_endpoint: String,
}

impl<S: AuthStorage<StoredAuth> + 'static> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self::new_with_token_endpoint(store, format!("{ISSUER}/oauth/token"))
    }

    fn new_with_token_endpoint(store: CodexTokenStore<S>, token_endpoint: String) -> Self {
        Self {
            store: Arc::new(store),
            #[cfg(test)]
            test_auth: Arc::new(Mutex::new(None)),
            refresh_lock: CODEX_REFRESH_LOCK.clone(),
            refresh_client: crate::tls::client_builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    pub async fn current_credential_snapshot(
        &self,
    ) -> Result<Option<ObservedCredential>, anyhow::Error> {
        let store = self.store.clone();
        #[cfg(test)]
        let test_auth = self.test_auth.clone();
        tokio::task::spawn_blocking(move || {
            let mut sequence = SNAPSHOT_SEQUENCE
                .lock()
                .map_err(|_| anyhow::anyhow!("Codex snapshot lock poisoned"))?;
            #[cfg(test)]
            let auth = match test_auth
                .lock()
                .map_err(|_| anyhow::anyhow!("Codex test auth lock poisoned"))?
                .clone()
            {
                Some(auth) => Some(auth),
                None => store.load_auth()?,
            };
            #[cfg(not(test))]
            let auth = store.load_auth()?;
            let Some(auth) = auth.filter(|auth| {
                !auth.refresh.is_empty()
                    || (!auth.access.is_empty()
                        && auth.expires > now_ms().saturating_add(REFRESH_MARGIN_MS))
            }) else {
                return Ok(None);
            };
            *sequence = sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Codex snapshot sequence exhausted"))?;
            Ok(Some(ObservedCredential {
                id: CredentialId::for_chatgpt_auth(&auth),
                observation: CredentialObservation(*sequence),
            }))
        })
        .await
        .map_err(|e| anyhow::anyhow!("Codex snapshot task failed: {e}"))?
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = self.load_auth().await?.ok_or_else(|| {
            anyhow::anyhow!(
                "No Codex credentials. Log in with the Codex CLI (`codex login`) to create ~/.codex/auth.json"
            )
        })?;

        if stored.expires > now_ms() + REFRESH_MARGIN_MS {
            return Ok(stored);
        }

        self.refresh(false, &stored.access).await
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, anyhow::Error> {
        self.refresh(true, rejected_access).await
    }

    pub async fn reasoning_account(&self) -> ReasoningAccount {
        let auth = self.load_auth().await.ok().flatten();
        ReasoningAccount::new(auth.as_ref().and_then(|auth| auth.account_id.as_deref()))
    }

    async fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(auth));
        }

        with_store(&self.store, |store| store.load_auth()).await
    }

    async fn refresh(&self, force: bool, stale_access: &str) -> Result<StoredAuth, anyhow::Error> {
        let refresh_guard = self.refresh_lock.clone().lock_owned().await;
        let file_lock = match with_store(&self.store, |store| store.lock_for_refresh()).await {
            Ok(file_lock) => file_lock,
            Err(err) if force => return Err(classified(err, AuthFailure::Transient)),
            Err(err) => {
                return match self.load_auth().await {
                    Ok(Some(current)) if current.expires > now_ms() => Ok(current),
                    _ => Err(classified(err, AuthFailure::Transient)),
                };
            }
        };

        let current = self
            .load_auth()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;

        if current.access != stale_access
            || (!force && current.expires > now_ms() + REFRESH_MARGIN_MS)
        {
            return Ok(current);
        }

        let exchange = exchange_refresh_token(
            self.refresh_client.clone(),
            self.token_endpoint.clone(),
            self.store.clone(),
            current,
        );
        tokio::spawn(async move {
            let _held = (refresh_guard, file_lock);
            exchange.await
        })
        .await
        .map_err(|e| anyhow::anyhow!("Codex token refresh task failed: {e}"))?
    }

    #[cfg(test)]
    pub fn set_test_auth(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.test_auth.lock() {
            *guard = Some(auth);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn with_store<S: AuthStorage<StoredAuth> + 'static, T: Send + 'static>(
    store: &Arc<CodexTokenStore<S>>,
    task: impl FnOnce(&CodexTokenStore<S>) -> Result<T, anyhow::Error> + Send + 'static,
) -> Result<T, anyhow::Error> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || task(&store))
        .await
        .map_err(|e| anyhow::anyhow!("Codex auth file task failed: {e}"))?
}

async fn exchange_refresh_token<S: AuthStorage<StoredAuth> + 'static>(
    client: reqwest::Client,
    token_endpoint: String,
    store: Arc<CodexTokenStore<S>>,
    current: StoredAuth,
) -> Result<StoredAuth, anyhow::Error> {
    if current.refresh.is_empty() {
        anyhow::bail!("No refresh token stored; run `codex login` to sign in again");
    }

    let form = [
        ("client_id", CLIENT_ID.to_string()),
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", current.refresh.clone()),
    ];

    let resp = client
        .post(&token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| {
            classified(
                anyhow::anyhow!("refresh network error: {e}"),
                AuthFailure::Transient,
            )
        })?;

    let status = resp.status().as_u16();
    if resp.status().is_client_error() {
        drop(resp);
        if let Some(latest) = with_store(&store, |store| store.load_auth())
            .await
            .map_err(|error| {
                if matches!(status, 408 | 429) {
                    classified(error, AuthFailure::Transient)
                } else {
                    error
                }
            })?
            && latest.refresh != current.refresh
        {
            return Ok(latest);
        }
        if matches!(status, 408 | 429) {
            return Err(classified(
                anyhow::anyhow!("Token refresh failed: {status}"),
                AuthFailure::Transient,
            ));
        }
        let id = CredentialId::new(AuthSource::ChatGpt, current.refresh.as_bytes());
        return Err(classified(
            anyhow::anyhow!(
                "Codex rejected the stored refresh token ({status}); run `codex login` to sign in again"
            ),
            AuthFailure::Rejected(id),
        ));
    }

    if !resp.status().is_success() {
        let error = anyhow::anyhow!("Token refresh failed: {status}");
        return Err(if resp.status().is_server_error() {
            classified(error, AuthFailure::Transient)
        } else {
            error
        });
    }

    let body = read_refresh_body(resp).await?;
    let tokens: TokenResponse = serde_json::from_slice(&body)
        .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
    validate_token_response(&tokens)?;
    let account_id = extract_account_id(&tokens).or_else(|| current.account_id.clone());
    let expires = now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
    let next = StoredAuth {
        access: tokens.access_token,
        refresh: tokens.refresh_token,
        expires,
        account_id,
    };
    with_store(&store, move |store| {
        persist_refreshed(store, &current.refresh, next)
    })
    .await
}

fn persist_refreshed<S: AuthStorage<StoredAuth>>(
    store: &CodexTokenStore<S>,
    spent_refresh: &str,
    next: StoredAuth,
) -> Result<StoredAuth, anyhow::Error> {
    let mut attempt = 1;
    loop {
        let saved = match store.load_auth() {
            Ok(Some(latest)) if latest.refresh != spent_refresh => return Ok(latest),
            Ok(Some(_)) => store.save_auth(next.clone()),
            Ok(None) => anyhow::bail!(
                "the Codex login was removed during a token refresh; run `codex login` to sign in again"
            ),
            Err(err) => Err(err),
        };
        match saved {
            Ok(()) => return Ok(next),
            Err(err) if attempt >= SAVE_ATTEMPTS => return Err(err),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(SAVE_RETRY_PAUSE);
            }
        }
    }
}

async fn read_refresh_body(mut resp: reqwest::Response) -> Result<Vec<u8>, anyhow::Error> {
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_REFRESH_RESPONSE_BYTES as u64)
    {
        anyhow::bail!("token refresh response exceeds {MAX_REFRESH_RESPONSE_BYTES} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        classified(
            anyhow::anyhow!("refresh network error: {e}"),
            AuthFailure::Transient,
        )
    })? {
        if body.len().saturating_add(chunk.len()) > MAX_REFRESH_RESPONSE_BYTES {
            anyhow::bail!("token refresh response exceeds {MAX_REFRESH_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::super::token_store::CodexCliAuthStore;
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn test_store() -> CodexTokenStore<InMemoryAuthStore<StoredAuth>> {
        CodexTokenStore::new(InMemoryAuthStore::new())
    }

    #[tokio::test]
    async fn snapshot_observes_only_usable_access_only_credentials() {
        let manager = CodexAuthManager::new(test_store());
        let mut auth = StoredAuth {
            access: "toy-access-one".into(),
            refresh: String::new(),
            expires: now_ms().saturating_add(REFRESH_MARGIN_MS + 60_000),
            account_id: None,
        };
        manager.store.save_auth(auth.clone()).unwrap();
        let first = manager
            .current_credential_snapshot()
            .await
            .unwrap()
            .unwrap();
        auth.access = "toy-access-two".into();
        manager.store.save_auth(auth.clone()).unwrap();
        let second = manager
            .current_credential_snapshot()
            .await
            .unwrap()
            .unwrap();
        assert!(first.id != second.id);
        assert!(first.id != CredentialId::new(AuthSource::ChatGpt, b"toy-access-one"));
        auth.expires = now_ms();
        manager.store.save_auth(auth.clone()).unwrap();
        assert!(
            manager
                .current_credential_snapshot()
                .await
                .unwrap()
                .is_none()
        );
        auth.expires = u64::MAX;
        auth.access.clear();
        manager.store.save_auth(auth).unwrap();
        assert!(
            manager
                .current_credential_snapshot()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn snapshot_is_passive_and_orders_rotations() {
        use crate::config::codex_auth::load_order;
        use crate::providers::codex::api::SendEvidence;
        use crate::providers::codex::client::CodexError;
        use crate::providers::codex::sources::{
            CodexSources, FailureKind, SkipMark, SourceFailure,
        };
        use std::path::Path;
        use tokio::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            test_store(),
            format!("http://{}/oauth/token", listener.local_addr().unwrap()),
        );
        assert!(
            manager
                .current_credential_snapshot()
                .await
                .unwrap()
                .is_none()
        );
        let mut auth = StoredAuth {
            access: "toy-access".into(),
            refresh: "toy-old".into(),
            expires: 0,
            account_id: None,
        };
        manager.store.save_auth(auth.clone()).unwrap();
        let old = manager
            .current_credential_snapshot()
            .await
            .unwrap()
            .unwrap();
        assert!(old.id == CredentialId::new(AuthSource::ChatGpt, b"toy-old"));
        auth.refresh = "toy-new".into();
        manager.store.save_auth(auth.clone()).unwrap();
        let new = manager
            .current_credential_snapshot()
            .await
            .unwrap()
            .unwrap();
        assert!(new.id != old.id && new.observation > old.observation);
        let sources = CodexSources::new(load_order(Path::new("unused"), None).unwrap());
        let now = Instant::now();
        assert!(sources.register(AuthSource::ChatGpt, new.id, new.observation));
        sources.mark(
            AuthSource::ChatGpt,
            &SourceFailure {
                kind: FailureKind::RejectedCredential,
                send: SendEvidence::DeniedBeforeExecution,
                error: CodexError::new(401, String::new()),
                credential: Some(new.id),
            },
            now,
        );
        assert!(!sources.register(AuthSource::ChatGpt, old.id, old.observation));
        assert_eq!(
            sources.skip(AuthSource::ChatGpt, new.id, now),
            Some(SkipMark::RejectedCredential)
        );
        auth.refresh.clear();
        manager.store.save_auth(auth).unwrap();
        assert!(
            manager
                .current_credential_snapshot()
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
        manager.set_test_auth(StoredAuth {
            access: "toy-access".into(),
            refresh: "toy-mock".into(),
            expires: 0,
            account_id: None,
        });
        let mock = manager
            .current_credential_snapshot()
            .await
            .unwrap()
            .unwrap();
        assert!(mock.id == CredentialId::new(AuthSource::ChatGpt, b"toy-mock"));
    }

    struct FailingReadStore;

    impl AuthStorage<StoredAuth> for FailingReadStore {
        fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
            anyhow::bail!("toy read failure")
        }
        fn save(&self, _: StoredAuth) -> anyhow::Result<()> {
            Ok(())
        }
        fn clear(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn path(&self) -> String {
            "toy".into()
        }
    }

    #[tokio::test]
    async fn transient_status_keeps_classification_when_reread_fails() {
        for status in [408, 429, 400] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                write!(
                    stream,
                    "HTTP/1.1 {status} Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
                .unwrap();
            });
            let result = exchange_refresh_token(
                crate::tls::client_builder().build().unwrap(),
                format!("http://{addr}/oauth/token"),
                Arc::new(CodexTokenStore::new(FailingReadStore)),
                StoredAuth {
                    access: "toy-access".into(),
                    refresh: "toy-refresh".into(),
                    expires: 0,
                    account_id: None,
                },
            )
            .await;
            server.join().unwrap();
            let Err(error) = result else {
                panic!("expected refresh failure")
            };
            assert_eq!(error.to_string(), "toy read failure");
            let expected = if status == 400 {
                AuthFailure::Other
            } else {
                AuthFailure::Transient
            };
            assert!(classify_auth_failure(&error) == expected);
        }
    }

    #[tokio::test]
    async fn refresh_failures_are_classified_at_the_origin() {
        for status in [Some(400), Some(401), Some(408), Some(429), Some(500), None] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                if let Some(status) = status {
                    write!(
                        stream,
                        "HTTP/1.1 {status} Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            });
            let store = test_store();
            store
                .save_auth(StoredAuth {
                    access: "toy-access".into(),
                    refresh: "toy-attempt".into(),
                    expires: 0,
                    account_id: None,
                })
                .unwrap();
            let manager = CodexAuthManager::new_with_token_endpoint(
                store,
                format!("http://{addr}/oauth/token"),
            );
            let result = manager.get_auth().await;
            server.join().unwrap();
            let Err(error) = result else {
                panic!("expected refresh failure");
            };
            let kind = classify_auth_failure(&error);
            if matches!(status, Some(400 | 401)) {
                assert!(
                    kind == AuthFailure::Rejected(CredentialId::new(
                        AuthSource::ChatGpt,
                        b"toy-attempt"
                    ))
                );
                assert!(
                    kind != AuthFailure::Rejected(CredentialId::new(
                        AuthSource::ChatGpt,
                        b"toy-replacement"
                    ))
                );
            } else {
                assert!(kind == AuthFailure::Transient);
            }
        }
        let result = CodexAuthManager::new(test_store()).get_auth().await;
        let Err(error) = result else {
            panic!("expected missing auth failure");
        };
        assert!(classify_auth_failure(&error) == AuthFailure::Other);
    }

    #[tokio::test]
    async fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        let result = manager.get_auth().await.unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.account_id.as_deref(), Some("acct_1"));
    }

    #[tokio::test]
    async fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = CodexAuthManager::new(store);
        assert!(manager.get_auth().await.is_err());
        assert!(
            manager
                .get_auth()
                .await
                .unwrap_err()
                .to_string()
                .contains("No Codex credentials")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_expired_auth_refreshes_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let server_refreshes = refreshes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("refresh_token=stale"));
            server_refreshes.fetch_add(1, Ordering::SeqCst);

            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "expired".into(),
                refresh: "stale".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            store,
            format!("http://{addr}/oauth/token"),
        ));
        let (first, second) = tokio::join!(manager.get_auth(), manager.get_auth());
        let results = [first.unwrap(), second.unwrap()];
        server.join().unwrap();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|auth| auth.access == "rotated"));
        assert!(results.iter().all(|auth| auth.refresh == "rotated-refresh"));
    }

    #[tokio::test]
    async fn stale_401_reuses_already_rotated_auth() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            "http://127.0.0.1:1/should-not-be-called".into(),
        );

        let auth = manager.force_refresh("rejected").await.unwrap();
        assert_eq!(auth.access, "rotated");
        assert_eq!(auth.refresh, "rotated-refresh");
    }

    #[tokio::test]
    async fn rejected_refresh_uses_changed_tokens_and_otherwise_errors_without_clearing() {
        for codex_cli_rotated in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let backing = InMemoryAuthStore::new();
            let server_backing = backing.clone();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                if codex_cli_rotated {
                    server_backing
                        .save(StoredAuth {
                            access: "same-access".into(),
                            refresh: "replacement-refresh".into(),
                            expires: u64::MAX,
                            account_id: Some("acct_1".into()),
                        })
                        .unwrap();
                }
                let body = b"upstream rejection detail";
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            });

            let rejected = StoredAuth {
                access: "same-access".into(),
                refresh: "rejected-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            };
            let store = CodexTokenStore::new(backing);
            store.save_auth(rejected.clone()).unwrap();
            let manager = CodexAuthManager::new_with_token_endpoint(
                store,
                format!("http://{addr}/oauth/token"),
            );

            let result = manager.get_auth().await;
            server.join().unwrap();
            if codex_cli_rotated {
                let auth = result.unwrap();
                assert_eq!(auth.access, "same-access");
                assert_eq!(auth.refresh, "replacement-refresh");
                assert_eq!(manager.store.load_auth().unwrap(), Some(auth));
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("run `codex login`"), "{error}");
                assert!(!error.contains("upstream rejection detail"), "{error}");
                assert_eq!(manager.store.load_auth().unwrap(), Some(rejected));
            }
        }
    }

    #[tokio::test]
    async fn refresh_keeps_a_login_the_codex_cli_rotated_during_the_exchange() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let backing = InMemoryAuthStore::new();
        let server_backing = backing.clone();
        let rotated_by_cli = StoredAuth {
            access: "cli-access".into(),
            refresh: "cli-refresh".into(),
            expires: u64::MAX,
            account_id: Some("acct_1".into()),
        };
        let server_rotated = rotated_by_cli.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            server_backing.save(server_rotated).unwrap();
            let body = br#"{"access_token":"duolane-access","refresh_token":"duolane-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = CodexTokenStore::new(backing);
        store
            .save_auth(StoredAuth {
                access: "stale-access".into(),
                refresh: "spent-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager =
            CodexAuthManager::new_with_token_endpoint(store, format!("http://{addr}/oauth/token"));

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth, rotated_by_cli);
        assert_eq!(manager.store.load_auth().unwrap(), Some(rotated_by_cli));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_lock_times_out_and_rereads_the_file_once_acquired() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let write_access = |access: &str| {
            let doc = serde_json::json!({"tokens": {
                "access_token": access,
                "refresh_token": "toy-refresh",
                "account_id": "acct_1"
            }});
            std::fs::write(&path, doc.to_string()).unwrap();
        };
        write_access("stale-access");
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join("auth.json.duolane.lock"))
            .unwrap();
        held.lock().unwrap();
        let unreachable = "http://127.0.0.1:1/oauth/token".to_string();

        let impatient = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(
                CodexCliAuthStore::new(path.clone()).with_lock_timeout(Duration::from_millis(200)),
            ),
            unreachable.clone(),
        );
        let error = impatient.force_refresh("stale-access").await.unwrap_err();
        assert!(classify_auth_failure(&error) == AuthFailure::Transient);
        assert!(error.to_string().contains("timed out"));

        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(CodexCliAuthStore::new(path.clone())),
            unreachable,
        ));
        let waiting = tokio::spawn({
            let manager = manager.clone();
            async move { manager.force_refresh("stale-access").await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished());
        write_access("rotated-by-another-process");
        drop(held);

        let auth = waiting.await.unwrap().unwrap();
        assert_eq!(auth.access, "rotated-by-another-process");
        assert_eq!(auth.refresh, "toy-refresh");
    }

    #[tokio::test]
    async fn durable_rotation_and_logout_are_observed_by_shared_manager() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "first".into(),
                refresh: "first-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);
        assert_eq!(manager.get_auth().await.unwrap().access, "first");

        manager
            .store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_2".into()),
            })
            .unwrap();
        let rotated = manager.get_auth().await.unwrap();
        assert_eq!(rotated.access, "rotated");
        assert_eq!(rotated.account_id.as_deref(), Some("acct_2"));

        manager.store.clear_auth().unwrap();
        assert!(manager.get_auth().await.is_err());
    }
}
