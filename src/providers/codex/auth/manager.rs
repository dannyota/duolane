use std::sync::Arc;
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::AuthStorage;
use crate::providers::codex::translate::reasoning_signature::ReasoningAccount;

const MAX_REFRESH_RESPONSE_BYTES: usize = 64 * 1024;

static CODEX_REFRESH_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));

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
            refresh_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = self.load_auth().await?.ok_or_else(|| {
            anyhow::anyhow!(
                "No Codex credentials. Log in with the Codex CLI (`codex login`) to create ~/.codex/auth.json"
            )
        })?;

        if stored.expires > Self::now_ms() + REFRESH_MARGIN_MS {
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

        self.with_store(|store| store.load_auth()).await
    }

    async fn with_store<T: Send + 'static>(
        &self,
        task: impl FnOnce(&CodexTokenStore<S>) -> Result<T, anyhow::Error> + Send + 'static,
    ) -> Result<T, anyhow::Error> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || task(&store))
            .await
            .map_err(|e| anyhow::anyhow!("Codex auth file task failed: {e}"))?
    }

    async fn refresh(&self, force: bool, stale_access: &str) -> Result<StoredAuth, anyhow::Error> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let _file_lock = self.with_store(|store| store.lock_for_refresh()).await?;

        let current = self
            .load_auth()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;

        if current.access != stale_access
            || (!force && current.expires > Self::now_ms() + REFRESH_MARGIN_MS)
        {
            return Ok(current);
        }

        self.refresh_now(&current).await
    }

    async fn refresh_now(&self, current: &StoredAuth) -> Result<StoredAuth, anyhow::Error> {
        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; run `codex login` to sign in again");
        }

        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        let resp = self
            .refresh_client
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("refresh network error: {e}"))?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            drop(resp);
            if let Some(latest) = self.load_auth().await?
                && latest.refresh != current.refresh
            {
                return Ok(latest);
            }
            anyhow::bail!(
                "Codex rejected the stored refresh token ({status}); run `codex login` to sign in again"
            );
        }

        if !resp.status().is_success() {
            anyhow::bail!("Token refresh failed: {status}");
        }

        let body = read_refresh_body(resp).await?;
        let tokens: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
        validate_token_response(&tokens)?;
        let account_id = extract_account_id(&tokens).or_else(|| current.account_id.clone());
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let next = StoredAuth {
            access: tokens.access_token,
            refresh: tokens.refresh_token,
            expires,
            account_id,
        };
        let saved = next.clone();
        self.with_store(move |store| store.save_auth(saved)).await?;
        Ok(next)
    }

    #[cfg(test)]
    pub fn set_test_auth(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.test_auth.lock() {
            *guard = Some(auth);
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
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("refresh network error: {e}"))?
    {
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
        let error = impatient
            .force_refresh("stale-access")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");

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
