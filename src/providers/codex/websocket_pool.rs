#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
#[cfg(test)]
use http::HeaderMap;
use std::collections::HashMap;
#[cfg(test)]
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::task::{Context, Poll};
use std::time::Duration;
#[cfg(test)]
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex as AsyncMutex;
#[cfg(test)]
use tokio::sync::mpsc;
#[cfg(test)]
use tokio_tungstenite::WebSocketStream;
#[cfg(test)]
use tokio_tungstenite::tungstenite::Message;
#[cfg(test)]
use tokio_tungstenite::tungstenite::protocol::Role;

#[cfg(test)]
use crate::provider::RequestContext;
use crate::request_identity::ConversationIdentity;

use super::client::CodexError;
use super::continuation::ContinuationReservation;
use super::translate::reasoning_signature::ReasoningAccount;
use super::websocket::*;
#[cfg(test)]
use super::websocket_stream::*;

const POOL_IDLE_TTL_MS: u64 = 30 * 60 * 1000;

const MAX_POOL_ENTRIES: usize = 10_000;

const POOL_CONNECT_CLEANUP_THRESHOLD: usize = 50;

const POOL_CONNECT_CLEANUP_TARGET: usize = 40;

pub(super) struct PoolEntry {
    pub(super) ws: Arc<AsyncMutex<CodexWebSocketStream>>,
    pub(super) socket_id: u64,
    created_at: u64,
    last_activity: AtomicU64,
}

impl PoolEntry {
    pub(super) fn new(ws: CodexWebSocketStream) -> Self {
        Self {
            ws: Arc::new(AsyncMutex::new(ws)),
            socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
            created_at: now_ms(),
            last_activity: AtomicU64::new(next_pool_activity()),
        }
    }

    fn touch(&self) {
        self.last_activity
            .fetch_max(next_pool_activity(), Ordering::Relaxed);
    }
}

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

pub(super) static POOLED_VALIDATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

static POOL_ACTIVITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) type PoolKey = (ConversationIdentity, ReasoningAccount);

static WS_POOL: once_cell::sync::Lazy<Mutex<HashMap<PoolKey, Arc<PoolEntry>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static WS_POOL_TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

pub(super) static WS_CONNECT_GATE: once_cell::sync::Lazy<WebSocketConnectGate> =
    once_cell::sync::Lazy::new(|| WebSocketConnectGate::new(WEBSOCKET_CONNECT_START_SPACING));

pub(super) fn next_monotonic_nonzero(sequence: &AtomicU64, label: &str) -> u64 {
    let previous = sequence
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{label} sequence exhausted"));
    previous + 1
}

fn next_pool_activity() -> u64 {
    POOL_ACTIVITY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn clear_codex_websocket_pool_for_tests() {
    let mut guard = WS_POOL.lock().unwrap();
    guard.clear();
}

#[cfg(test)]
pub(crate) async fn lock_codex_websocket_pool_for_tests() -> tokio::sync::MutexGuard<'static, ()> {
    WS_POOL_TEST_LOCK.lock().await
}

#[cfg(test)]
pub(crate) fn pooled_socket_id_for_tests(owner: &ConversationIdentity) -> Option<u64> {
    WS_POOL
        .lock()
        .unwrap()
        .iter()
        .find(|((pooled_owner, _), _)| pooled_owner == owner)
        .map(|(_, entry)| entry.socket_id)
}

pub fn invalidate_codex_websocket_pool_owner(owner: &ConversationIdentity) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.retain(|(pooled_owner, _), _| pooled_owner != owner);
}

#[deprecated(note = "use typed conversation ownership internally")]
pub fn invalidate_codex_websocket_pool_key(session_id: &str) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.retain(|(owner, _), _| match owner {
        ConversationIdentity::Main(owner_session_id)
        | ConversationIdentity::Agent(owner_session_id, _) => owner_session_id != session_id,
    });
}

pub(crate) fn invalidate_codex_websocket_pool_turn_for_owner(
    owner: &ConversationIdentity,
    turn_id: Option<u64>,
) {
    let reservation = ContinuationReservation::for_owner_turn(Some(owner), turn_id);
    super::continuation::with_current_turn_for_owner(&reservation, || {
        invalidate_codex_websocket_pool_owner(owner)
    });
}

#[deprecated(note = "use typed conversation ownership internally")]
pub fn invalidate_codex_websocket_pool_turn(session_id: &str, turn_id: Option<u64>) {
    let owner = ConversationIdentity::Main(session_id.to_owned());
    invalidate_codex_websocket_pool_turn_for_owner(&owner, turn_id);
}

fn invalidate_pool_entry(owner: &PoolKey, entry: &Arc<PoolEntry>) {
    let mut guard = WS_POOL.lock().unwrap();
    if guard
        .get(owner)
        .is_some_and(|pooled| Arc::ptr_eq(pooled, entry))
    {
        guard.remove(owner);
    }
}

pub(super) fn invalidate_pool_owner(owner: Option<&PoolKey>, entry: Option<&Arc<PoolEntry>>) {
    let Some(owner) = owner else {
        return;
    };
    match entry {
        Some(entry) => invalidate_pool_entry(owner, entry),
        None => drop(WS_POOL.lock().unwrap().remove(owner)),
    }
}

pub(super) fn reservation_pool_owner(
    reservation: Option<&ContinuationReservation>,
) -> Option<PoolKey> {
    let reservation = reservation?;
    if reservation.candidate().disabled_reason.as_deref() == Some("disabled") {
        return None;
    }
    Some((reservation.owner()?.clone(), reservation.account().clone()))
}

fn pool_take_for_turn(reservation: &ContinuationReservation) -> Option<Arc<PoolEntry>> {
    let owner = (reservation.owner()?.clone(), reservation.account().clone());
    super::continuation::if_current_turn_for_owner(reservation, || {
        WS_POOL.lock().ok()?.remove(&owner)
    })
    .flatten()
}

pub(super) fn take_pool_entry_for_request(
    reservation: Option<&ContinuationReservation>,
) -> Result<Option<Arc<PoolEntry>>, CodexError> {
    let candidate = reservation.map(ContinuationReservation::candidate);
    let requires_origin = candidate
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let expected_socket_id = reservation.and_then(ContinuationReservation::origin_socket_id);
    let pool_owner = reservation_pool_owner(reservation);
    let pooled = reservation
        .filter(|_| pool_owner.is_some())
        .and_then(pool_take_for_turn);

    if requires_origin
        && (pool_owner.is_none()
            || expected_socket_id.is_none()
            || pooled
                .as_ref()
                .is_none_or(|entry| Some(entry.socket_id) != expected_socket_id))
    {
        if let (Some(owner), Some(entry)) = (pool_owner, pooled.as_ref()) {
            pool_insert_if_vacant_or_same(owner, entry.clone());
        }
        return Err(continuation_socket_missing_error());
    }

    Ok(pooled)
}

pub(super) fn pool_insert_for_turn(
    reservation: &ContinuationReservation,
    entry: Arc<PoolEntry>,
) -> bool {
    let Some(owner) = reservation_pool_owner(Some(reservation)) else {
        return false;
    };
    super::continuation::if_current_turn_for_owner(reservation, || {
        pool_insert_if_vacant_or_same(owner, entry)
    })
    .unwrap_or(false)
}

pub(super) fn pool_remove_entry(owner: &PoolKey, entry: &Arc<PoolEntry>) {
    invalidate_pool_entry(owner, entry);
}

pub(super) fn invalidate_codex_websocket_pool_socket(
    reservation: &ContinuationReservation,
    socket_id: Option<u64>,
) {
    let Some(owner) = reservation_pool_owner(Some(reservation)) else {
        return;
    };
    let Some(socket_id) = socket_id else {
        return;
    };
    let entry = WS_POOL.lock().ok().and_then(|pool| {
        pool.get(&owner)
            .filter(|entry| entry.socket_id == socket_id)
            .cloned()
    });
    if let Some(entry) = entry {
        pool_remove_entry(&owner, &entry);
    }
}

fn pool_insert_if_vacant_or_same(owner: PoolKey, entry: Arc<PoolEntry>) -> bool {
    entry.touch();
    let mut guard = WS_POOL.lock().unwrap();
    if let Some(existing) = guard.get(&owner) {
        return Arc::ptr_eq(existing, &entry);
    }
    if guard.len() >= MAX_POOL_ENTRIES
        && let Some(oldest_owner) = guard.keys().next().cloned()
    {
        guard.remove(&oldest_owner);
    }
    let now = now_ms();
    guard.retain(|_, pooled| now.saturating_sub(pooled.created_at) < POOL_IDLE_TTL_MS);
    guard.insert(owner, entry);
    true
}

#[cfg(test)]
fn pool_insert(owner: ConversationIdentity, entry: Arc<PoolEntry>) {
    entry.touch();
    let mut guard = WS_POOL.lock().unwrap();
    if guard.len() >= MAX_POOL_ENTRIES
        && let Some(oldest_owner) = guard.keys().next().cloned()
    {
        guard.remove(&oldest_owner);
    }
    let now = now_ms();
    guard.retain(|_, entry| now.saturating_sub(entry.created_at) < POOL_IDLE_TTL_MS);
    guard.insert((owner, ReasoningAccount::default()), entry);
}

pub(super) fn cleanup_pool_before_connect() {
    let removed = {
        let mut guard = WS_POOL.lock().unwrap();
        if guard.len() <= POOL_CONNECT_CLEANUP_THRESHOLD {
            return;
        }

        let remove_count = guard.len() - POOL_CONNECT_CLEANUP_TARGET;
        let mut candidates: Vec<_> = guard
            .iter()
            .filter(|(_, entry)| Arc::strong_count(entry) == 1)
            .map(|(owner, entry)| (owner.clone(), entry.last_activity.load(Ordering::Relaxed)))
            .collect();
        candidates.sort_unstable_by_key(|(_, activity)| *activity);
        candidates
            .into_iter()
            .take(remove_count)
            .filter_map(|(owner, _)| guard.remove(&owner))
            .collect::<Vec<_>>()
    };
    drop(removed);
}

pub(super) struct WebSocketConnectGate {
    last_start: AsyncMutex<Option<tokio::time::Instant>>,
    start_spacing: Duration,
}

impl WebSocketConnectGate {
    pub(super) fn new(start_spacing: Duration) -> Self {
        Self {
            last_start: AsyncMutex::new(None),
            start_spacing,
        }
    }

    pub(super) async fn wait_to_start(&self, before_start: impl std::future::Future<Output = ()>) {
        let mut last_start = self.last_start.lock().await;
        if let Some(previous) = *last_start {
            tokio::time::sleep_until(previous + self.start_spacing).await;
        }
        before_start.await;
        *last_start = Some(tokio::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::super::websocket_test_support::*;
    use super::*;

    fn pool_key_for(owner: &ConversationIdentity) -> PoolKey {
        (owner.clone(), ReasoningAccount::default())
    }

    fn main_owner(session_id: &str) -> ConversationIdentity {
        ConversationIdentity::Main(session_id.to_string())
    }

    fn continuation_request() -> super::super::translate::request::ResponsesRequest {
        super::super::translate::request::ResponsesRequest {
            model: "gpt-5.6-sol".to_string(),
            instructions: None,
            input: vec![],
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: None,
                format: None,
            },
            reasoning: None,
            reasoning_account: Default::default(),
        }
    }

    struct DropProbeIo {
        probe: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
    }

    impl Drop for DropProbeIo {
        fn drop(&mut self) {
            if let Some((dropped, pool_was_unlocked)) = &self.probe {
                pool_was_unlocked.store(WS_POOL.try_lock().is_ok(), Ordering::SeqCst);
                dropped.store(true, Ordering::SeqCst);
            }
        }
    }

    impl AsyncRead for DropProbeIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for DropProbeIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn raw_test_stream(
        probe: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
    ) -> CodexWebSocketStream {
        let io: BoxedWebSocketIo = Box::new(DropProbeIo { probe });
        WebSocketStream::from_raw_socket(io, Role::Client, None).await
    }

    fn shared_pool_entry(ws: &Arc<AsyncMutex<CodexWebSocketStream>>) -> Arc<PoolEntry> {
        Arc::new(PoolEntry {
            ws: ws.clone(),
            socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
            created_at: now_ms(),
            last_activity: AtomicU64::new(next_pool_activity()),
        })
    }

    #[tokio::test]
    async fn connect_cleanup_uses_threshold_target_lru_and_skips_leases() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let ws = Arc::new(AsyncMutex::new(raw_test_stream(None).await));
        {
            let mut guard = WS_POOL.lock().unwrap();
            for index in 0..POOL_CONNECT_CLEANUP_THRESHOLD {
                guard.insert(
                    pool_key_for(&main_owner(&format!("entry-{index:02}"))),
                    shared_pool_entry(&ws),
                );
            }
        }

        cleanup_pool_before_connect();
        assert_eq!(
            WS_POOL.lock().unwrap().len(),
            POOL_CONNECT_CLEANUP_THRESHOLD
        );

        WS_POOL.lock().unwrap().insert(
            pool_key_for(&main_owner("entry-50")),
            shared_pool_entry(&ws),
        );
        WS_POOL
            .lock()
            .unwrap()
            .get(&pool_key_for(&main_owner("entry-00")))
            .unwrap()
            .touch();
        let leased = WS_POOL
            .lock()
            .unwrap()
            .get(&pool_key_for(&main_owner("entry-01")))
            .unwrap()
            .clone();

        cleanup_pool_before_connect();

        let guard = WS_POOL.lock().unwrap();
        assert_eq!(guard.len(), POOL_CONNECT_CLEANUP_TARGET);
        assert!(guard.contains_key(&pool_key_for(&main_owner("entry-00"))));
        assert!(guard.contains_key(&pool_key_for(&main_owner("entry-01"))));
        for index in 2..=12 {
            assert!(!guard.contains_key(&pool_key_for(&main_owner(&format!("entry-{index:02}")))));
        }
        assert!(guard.contains_key(&pool_key_for(&main_owner("entry-13"))));
        assert!(guard.contains_key(&pool_key_for(&main_owner("entry-50"))));
        drop(guard);
        drop(leased);
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn connect_cleanup_drops_final_socket_owners_after_unlocking_pool() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let dropped = Arc::new(AtomicBool::new(false));
        let pool_was_unlocked = Arc::new(AtomicBool::new(false));
        let shared_ws = Arc::new(AsyncMutex::new(raw_test_stream(None).await));
        let probe_stream =
            raw_test_stream(Some((dropped.clone(), pool_was_unlocked.clone()))).await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                pool_key_for(&main_owner("entry-00")),
                Arc::new(PoolEntry::new(probe_stream)),
            );
            for index in 1..=POOL_CONNECT_CLEANUP_THRESHOLD {
                guard.insert(
                    pool_key_for(&main_owner(&format!("entry-{index:02}"))),
                    shared_pool_entry(&shared_ws),
                );
            }
        }

        cleanup_pool_before_connect();

        assert!(dropped.load(Ordering::SeqCst));
        assert!(pool_was_unlocked.load(Ordering::SeqCst));
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test(start_paused = true)]
    async fn connect_gate_spaces_starts_without_serializing_handshakes() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::from_secs(1)));
        let preflight_started = Arc::new(tokio::sync::Notify::new());
        let release_first = Arc::new(tokio::sync::Notify::new());
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();

        let first_gate = gate.clone();
        let first_preflight = preflight_started.clone();
        let first_release = release_first.clone();
        let first_tx = started_tx.clone();
        let first = tokio::spawn(async move {
            first_gate
                .wait_to_start(async move {
                    first_preflight.notify_one();
                    tokio::time::sleep(Duration::from_secs(2)).await;
                })
                .await;
            first_tx.send(tokio::time::Instant::now()).unwrap();
            first_release.notified().await;
        });
        preflight_started.notified().await;

        let second_gate = gate.clone();
        let second = tokio::spawn(async move {
            second_gate.wait_to_start(async {}).await;
            started_tx.send(tokio::time::Instant::now()).unwrap();
        });
        let first_started = started_rx.recv().await.unwrap();
        let second_started = started_rx.recv().await.unwrap();

        assert!(second_started.duration_since(first_started) >= Duration::from_secs(1));
        assert!(!first.is_finished());
        second.await.unwrap();
        release_first.notify_one();
        first.await.unwrap();
    }

    #[tokio::test]
    async fn pool_checkout_is_exclusive_and_removal_is_identity_safe() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let first = Arc::new(PoolEntry::new(create_dummy_stream_async().await));
        let owner = main_owner("exclusive");
        pool_insert(owner.clone(), first.clone());
        assert!(Arc::ptr_eq(
            &WS_POOL
                .lock()
                .unwrap()
                .remove(&pool_key_for(&owner))
                .unwrap(),
            &first
        ));
        assert!(
            WS_POOL
                .lock()
                .unwrap()
                .remove(&pool_key_for(&owner))
                .is_none()
        );

        let replacement = Arc::new(PoolEntry::new(create_dummy_stream_async().await));
        pool_insert(owner.clone(), replacement.clone());
        pool_remove_entry(&pool_key_for(&owner), &first);
        let reservation = test_continuation(Some(owner.clone()), None, None, None);
        invalidate_codex_websocket_pool_socket(&reservation, Some(first.socket_id));
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&pool_key_for(&owner)).unwrap(),
            &replacement
        ));
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn pool_entries_receive_monotonic_nonzero_socket_ids() {
        let first = PoolEntry::new(raw_test_stream(None).await);
        let second = PoolEntry::new(raw_test_stream(None).await);

        assert_ne!(first.socket_id, 0);
        assert!(second.socket_id > first.socket_id);
    }

    #[tokio::test]
    async fn continuation_rejects_and_preserves_same_owner_replacement() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = agent_owner("replacement-session", "replacement-agent");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let replacement = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        pool_insert(owner.clone(), replacement.clone());
        let continuation = test_continuation(
            Some(owner.clone()),
            reserved.turn_id(),
            Some("resp_origin"),
            Some(replacement.socket_id.checked_add(1).unwrap()),
        );

        let error = match prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            "ws://127.0.0.1:9/responses",
            &HeaderMap::new(),
            None,
            Some(&continuation),
            50,
            50,
        )
        .await
        {
            Ok(_) => panic!("replacement socket must not satisfy continuation provenance"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
        );
        assert!(!error.message.contains("replacement-session"));
        assert!(!error.message.contains("replacement-agent"));
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&pool_key_for(&owner)).unwrap(),
            &replacement
        ));

        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn dead_exact_origin_removes_only_that_arc_and_preserves_replacement() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("dead-origin-session");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let exact = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        let replacement = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        pool_insert(owner.clone(), exact.clone());
        let continuation = test_continuation(
            Some(owner.clone()),
            reserved.turn_id(),
            Some("resp_exact"),
            Some(exact.socket_id),
        );

        let replacement_owner = owner.clone();
        let replacement_for_task = replacement.clone();
        let mut insert_replacement = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(1), async move {
                loop {
                    if !WS_POOL
                        .lock()
                        .unwrap()
                        .contains_key(&pool_key_for(&replacement_owner))
                    {
                        pool_insert(replacement_owner, replacement_for_task);
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
        });
        let error = match prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            "ws://127.0.0.1:9/responses",
            &HeaderMap::new(),
            None,
            Some(&continuation),
            50,
            50,
        )
        .await
        {
            Ok(_) => panic!("dead continuation origin must be rejected before request send"),
            Err(error) => error,
        };
        match tokio::time::timeout(Duration::from_secs(2), &mut insert_replacement).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => panic!(
                "replacement polling timed out; currently pooled socket ID: {:?}",
                pooled_socket_id_for_tests(&owner)
            ),
            Ok(Err(error)) => panic!(
                "replacement polling task failed ({error}); currently pooled socket ID: {:?}",
                pooled_socket_id_for_tests(&owner)
            ),
            Err(_) => {
                insert_replacement.abort();
                let abort_result = insert_replacement.await;
                panic!(
                    "replacement polling join timed out ({abort_result:?}); currently pooled socket ID: {:?}",
                    pooled_socket_id_for_tests(&owner)
                );
            }
        }

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
        );
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&pool_key_for(&owner)).unwrap(),
            &replacement
        ));
        assert!(!Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&pool_key_for(&owner)).unwrap(),
            &exact
        ));

        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn completed_terminal_is_published_after_origin_returns_to_pool() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = agent_owner("terminal-order-session", "terminal-order-agent");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_response_tx, release_response_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = websocket.next().await {
                match message {
                    Message::Ping(payload) => {
                        websocket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Text(_) => {
                        release_response_rx.await.unwrap();
                        websocket
                            .send(Message::text(
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {"id": "resp_terminal_order"}
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });
        let context = RequestContext {
            req_id: "terminal-order-request".to_string(),
            session_id: Some("header-session-is-not-pool-owner".to_string()),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let mut events = codex_websocket_event_stream(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &serde_json::json!({"type":"response.create","input":[]}),
            &context,
            None,
            1_000,
            1_000,
            Some(&continuation),
        )
        .await
        .unwrap();
        assert_eq!(events.socket_id(), None);
        release_response_tx.send(()).unwrap();
        let terminal = events.recv().await.unwrap().unwrap();

        assert_eq!(
            terminal.get("type").and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
        let pooled = WS_POOL
            .lock()
            .unwrap()
            .get(&pool_key_for(&owner))
            .cloned()
            .expect("origin must be reusable before terminal publication");
        assert_eq!(events.socket_id(), Some(pooled.socket_id));
        server.await.unwrap();

        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&continuation);
    }

    #[tokio::test]
    async fn dropped_receiver_removes_completed_origin_after_terminal_publication_fails() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("dropped-terminal-receiver-session");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (release_response_tx, release_response_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = websocket.next().await {
                match message {
                    Message::Ping(payload) => {
                        websocket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Text(_) => {
                        request_seen_tx.send(()).unwrap();
                        release_response_rx.await.unwrap();
                        websocket
                            .send(Message::text(
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {"id": "resp_dropped_receiver"}
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });
        let headers = HeaderMap::new();
        let body = serde_json::json!({"type":"response.create","input":[]});
        let ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &headers,
            None,
            Some(&continuation),
            1_000,
            1_000,
        )
        .await
        .unwrap();
        let exact = ready.entry.clone();
        let events = start_codex_websocket_events(
            ready,
            &body,
            serde_json::to_string(&body).unwrap(),
            &headers,
            Some(&continuation),
        );

        request_seen_rx.await.unwrap();
        drop(events);
        release_response_tx.send(()).unwrap();
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&exact) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed terminal publication must release the exact completed pool entry");

        assert!(
            WS_POOL
                .lock()
                .unwrap()
                .get(&pool_key_for(&owner))
                .is_none_or(|pooled| !Arc::ptr_eq(pooled, &exact)),
            "the exact completed socket must not remain pooled"
        );
        super::super::continuation::abort_continuation_for_owner(&continuation);
    }

    #[tokio::test]
    async fn pool_invalidation() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let first_stream = create_dummy_stream_async().await;
        let second_stream = create_dummy_stream_async().await;
        let first_owner = agent_owner("test-session", "first-agent");
        let sibling_owner = agent_owner("test-session", "sibling-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                pool_key_for(&first_owner),
                Arc::new(PoolEntry::new(first_stream)),
            );
            guard.insert(
                pool_key_for(&sibling_owner),
                Arc::new(PoolEntry::new(second_stream)),
            );
        }
        assert!(
            WS_POOL
                .lock()
                .unwrap()
                .contains_key(&pool_key_for(&first_owner))
        );
        assert!(
            WS_POOL
                .lock()
                .unwrap()
                .contains_key(&pool_key_for(&sibling_owner))
        );

        invalidate_codex_websocket_pool_owner(&first_owner);
        assert!(
            !WS_POOL
                .lock()
                .unwrap()
                .contains_key(&pool_key_for(&first_owner))
        );
        assert!(
            WS_POOL
                .lock()
                .unwrap()
                .contains_key(&pool_key_for(&sibling_owner))
        );
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn session_key_invalidation_removes_main_and_agents_only_for_exact_session() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let session_id = "session-key";
        let main = main_owner(session_id);
        let first_agent = agent_owner(session_id, "first-agent");
        let second_agent = agent_owner(session_id, "second-agent");
        let other_session = main_owner("session-key-other");
        for owner in [
            main.clone(),
            first_agent.clone(),
            second_agent.clone(),
            other_session.clone(),
        ] {
            pool_insert(
                owner,
                Arc::new(PoolEntry::new(create_dummy_stream_async().await)),
            );
        }

        invalidate_codex_websocket_pool_key(session_id);

        let pool = WS_POOL.lock().unwrap();
        assert!(!pool.contains_key(&pool_key_for(&main)));
        assert!(!pool.contains_key(&pool_key_for(&first_agent)));
        assert!(!pool.contains_key(&pool_key_for(&second_agent)));
        assert!(pool.contains_key(&pool_key_for(&other_session)));
        drop(pool);
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn missing_owner_or_turn_cannot_mutate_pool_state() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("missing-owner-turn-pool");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        clear_codex_websocket_pool_for_tests();
        let request = continuation_request();
        let current = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let pooled = Arc::new(PoolEntry::new(create_dummy_stream_async().await));
        pool_insert(owner.clone(), pooled.clone());
        let missing_owner = test_continuation(None, current.turn_id(), None, None);
        let missing_turn = test_continuation(Some(owner.clone()), None, None, None);

        assert!(pool_take_for_turn(&missing_owner).is_none());
        assert!(pool_take_for_turn(&missing_turn).is_none());
        assert!(!pool_insert_for_turn(
            &missing_owner,
            Arc::new(PoolEntry::new(create_dummy_stream_async().await)),
        ));
        assert!(!pool_insert_for_turn(
            &missing_turn,
            Arc::new(PoolEntry::new(create_dummy_stream_async().await)),
        ));
        invalidate_codex_websocket_pool_turn_for_owner(&owner, None);
        invalidate_codex_websocket_pool_socket(&missing_owner, Some(pooled.socket_id));

        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&pool_key_for(&owner)).unwrap(),
            &pooled
        ));
        clear_codex_websocket_pool_for_tests();
        super::super::continuation::abort_continuation_for_owner(&current);
    }

    #[tokio::test]
    async fn binary_frame_invalidates_pool_owner() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = agent_owner("binary-session", "binary-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                pool_key_for(&owner),
                Arc::new(PoolEntry::new(pooled_stream)),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Binary(vec![1, 2, 3].into()))
                .await
                .unwrap();
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err = match collect_ws_events(&mut ws, 1_000, Some(&pool_key_for(&owner)), None, None)
            .await
        {
            Ok(_) => panic!("expected binary frame to fail"),
            Err(err) => err,
        };

        assert!(err.message.contains("binary frames"));
        assert!(!WS_POOL.lock().unwrap().contains_key(&pool_key_for(&owner)));
    }

    #[tokio::test]
    async fn response_start_timeout_ignores_rate_limits_and_pings() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = main_owner("start-timeout-session");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                pool_key_for(&owner),
                Arc::new(PoolEntry::new(pooled_stream)),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::text(
                r#"{"type":"codex.rate_limits","rate_limits":{"allowed":true}}"#,
            ))
            .await
            .unwrap();
            loop {
                if ws.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err =
            match collect_ws_events(&mut ws, 50, Some(&pool_key_for(&owner)), None, None).await {
                Ok(_) => panic!("expected response start timeout"),
                Err(err) => err,
            };

        assert_eq!(
            err.detail.as_deref(),
            Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
        );
        assert!(!WS_POOL.lock().unwrap().contains_key(&pool_key_for(&owner)));
    }

    #[tokio::test]
    async fn response_idle_timeout_ignores_pings_after_response_event() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = main_owner("response-idle-session");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                pool_key_for(&owner),
                Arc::new(PoolEntry::new(pooled_stream)),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            ))
            .await
            .unwrap();
            loop {
                if ws.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err =
            match collect_ws_events(&mut ws, 50, Some(&pool_key_for(&owner)), None, None).await {
                Ok(_) => panic!("expected response idle timeout"),
                Err(err) => err,
            };

        assert!(err.message.contains("idle timeout"));
        assert_eq!(err.detail, None);
        assert!(!WS_POOL.lock().unwrap().contains_key(&pool_key_for(&owner)));
    }

    async fn create_dummy_stream_async() -> CodexWebSocketStream {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = tokio_tungstenite::accept_async(socket).await;
            futures_util::future::pending::<()>().await;
        });
        let url = format!("ws://{addr}/");
        let client = test_websocket_client();
        connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            1_000,
        )
        .await
        .unwrap()
    }
}
