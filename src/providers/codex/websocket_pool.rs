#[path = "websocket_pool_bound.rs"]
mod bound;
#[path = "websocket_pool_freshness.rs"]
mod freshness;
use bound::IDLE_POOL_BOUND;
#[cfg(test)]
use bound::idle_pool_bound;
#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
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

#[derive(Clone, PartialEq, Eq)]
pub(super) struct HandshakeIdentity {
    credential: super::sources::CredentialId,
    headers: [u8; 32],
}

pub(super) fn handshake_identity(
    credential: super::sources::CredentialId,
    headers: &HeaderMap,
) -> HandshakeIdentity {
    let mut fields: Vec<_> = headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            !matches!(
                name,
                "authorization" | "host" | "connection" | "upgrade" | "proxy-authorization"
            ) && !name.starts_with("sec-websocket-")
        })
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect();
    fields.sort_unstable();
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for (name, value) in fields {
        digest.update(&(name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update(&(value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    let mut hash = [0; 32];
    hash.copy_from_slice(digest.finish().as_ref());
    HandshakeIdentity {
        credential,
        headers: hash,
    }
}

pub(super) struct PoolEntry {
    pub(super) ws: Arc<AsyncMutex<CodexWebSocketStream>>,
    pub(super) socket_id: u64,
    identity: Option<HandshakeIdentity>,
    created_at: u64,
    last_activity: AtomicU64,
    returned_at: Mutex<Option<tokio::time::Instant>>,
    return_id: AtomicU64,
    completed_ids: Mutex<std::collections::HashSet<[u8; 32]>>,
}

impl PoolEntry {
    pub(super) fn new(ws: CodexWebSocketStream) -> Self {
        Self {
            ws: Arc::new(AsyncMutex::new(ws)),
            socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
            identity: None,
            created_at: now_ms(),
            last_activity: AtomicU64::new(next_pool_activity()),
            returned_at: Mutex::new(None),
            return_id: AtomicU64::new(0),
            completed_ids: Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub(super) fn with_identity(ws: CodexWebSocketStream, identity: HandshakeIdentity) -> Self {
        let mut entry = Self::new(ws);
        entry.identity = Some(identity);
        entry
    }

    pub(super) fn has_completed(&self, id: &str) -> bool {
        let digest = ring::digest::digest(&ring::digest::SHA256, id.as_bytes());
        let mut hash = [0; 32];
        hash.copy_from_slice(digest.as_ref());
        self.completed_ids.lock().unwrap().contains(&hash)
    }

    pub(super) fn record_completed(&self, id: &str) -> bool {
        let digest = ring::digest::digest(&ring::digest::SHA256, id.as_bytes());
        let mut hash = [0; 32];
        hash.copy_from_slice(digest.as_ref());
        let mut ids = self.completed_ids.lock().unwrap();
        if ids.len() >= 1024 {
            return false;
        }
        ids.insert(hash);
        true
    }

    fn touch(&self) {
        self.last_activity
            .fetch_max(next_pool_activity(), Ordering::Relaxed);
    }
}

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

pub(super) static POOLED_VALIDATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

static POOL_ACTIVITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static POOL_RETURN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) type PoolKey = (ConversationIdentity, ReasoningAccount, bool);

static WS_POOL: once_cell::sync::Lazy<Mutex<HashMap<PoolKey, Arc<PoolEntry>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static WS_POOL_TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

pub(super) static ORIGIN_403_COUNT: AtomicU64 = AtomicU64::new(0);
pub(super) static HTTP_FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);
static WEBSOCKET_REUSED_COUNT: AtomicU64 = AtomicU64::new(0);
static WEBSOCKET_NEW_COUNT: AtomicU64 = AtomicU64::new(0);

pub(super) fn record_websocket_send(reused: bool) {
    let counter = if reused {
        &WEBSOCKET_REUSED_COUNT
    } else {
        &WEBSOCKET_NEW_COUNT
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn codex_connection_counters() -> crate::monitor::snapshot::CodexConnections {
    crate::monitor::snapshot::CodexConnections {
        queued_attempts: WS_CONNECT_GATE.queued.load(Ordering::Relaxed),
        handshakes_in_flight: WS_CONNECT_GATE.in_flight.load(Ordering::Relaxed),
        handshake_spacing_ms: WS_CONNECT_GATE.spacing_ms.load(Ordering::Relaxed),
        websocket_reused: WEBSOCKET_REUSED_COUNT.load(Ordering::Relaxed),
        websocket_new: WEBSOCKET_NEW_COUNT.load(Ordering::Relaxed),
        http_fallbacks: HTTP_FALLBACK_COUNT.load(Ordering::Relaxed),
        origin_403: ORIGIN_403_COUNT.load(Ordering::Relaxed),
    }
}

pub fn queued_codex_handshake_request_ids() -> Vec<String> {
    WS_CONNECT_GATE.queued_request_ids()
}

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

pub fn reset_codex_websocket_connect_gate_for_tests() {
    let mut state = WS_CONNECT_GATE.last_start.lock().unwrap();
    state.last_start = None;
    state.spacing = WS_CONNECT_GATE.floor;
    WS_CONNECT_GATE.spacing_ms.store(
        WS_CONNECT_GATE.floor.as_millis().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    state.cooldown_until = None;
    drop(state);
    WS_CONNECT_GATE.changed.notify_waiters();
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
        .find(|((pooled_owner, _, _), _)| pooled_owner == owner)
        .map(|(_, entry)| entry.socket_id)
}

pub(super) fn snapshot_codex_websocket_pool(owner: &ConversationIdentity) -> Vec<u64> {
    WS_POOL
        .lock()
        .unwrap()
        .iter()
        .filter(|((pooled_owner, _, _), _)| pooled_owner == owner)
        .map(|(_, entry)| entry.return_id.load(Ordering::Relaxed))
        .collect()
}

pub(super) fn invalidate_codex_websocket_pool_snapshot(
    owner: &ConversationIdentity,
    return_ids: &[u64],
) {
    WS_POOL
        .lock()
        .unwrap()
        .retain(|(pooled_owner, _, _), entry| {
            pooled_owner != owner || !return_ids.contains(&entry.return_id.load(Ordering::Relaxed))
        });
}

pub fn invalidate_codex_websocket_pool_owner(owner: &ConversationIdentity) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.retain(|(pooled_owner, _, _), _| pooled_owner != owner);
}

#[deprecated(note = "use typed conversation ownership internally")]
pub fn invalidate_codex_websocket_pool_key(session_id: &str) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.retain(|(owner, _, _), _| match owner {
        ConversationIdentity::Main(owner_session_id)
        | ConversationIdentity::Agent(owner_session_id, _) => owner_session_id != session_id,
    });
}

pub(super) fn invalidate_codex_websocket_pool_reservation(reservation: &ContinuationReservation) {
    let Some(key) = reservation_pool_owner(Some(reservation)) else {
        return;
    };
    let remove = || {
        WS_POOL
            .lock()
            .unwrap()
            .retain(|(owner, _, enabled), _| owner != &key.0 || *enabled != key.2);
    };
    if key.2 {
        super::continuation::with_current_turn_for_owner(reservation, remove);
    } else {
        remove();
    }
}

pub(crate) fn invalidate_codex_websocket_pool_turn_for_owner(
    owner: &ConversationIdentity,
    turn_id: Option<u64>,
) {
    let reservation = ContinuationReservation::for_owner_turn(Some(owner), turn_id);
    super::continuation::with_current_turn_for_owner(&reservation, || {
        WS_POOL
            .lock()
            .unwrap()
            .retain(|(pooled_owner, _, enabled), _| pooled_owner != owner || !enabled);
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
    let enabled = reservation.candidate().disabled_reason.as_deref() != Some("disabled");
    Some((
        reservation.owner()?.clone(),
        reservation.account().clone(),
        enabled,
    ))
}

fn pool_take_for_turn(reservation: &ContinuationReservation) -> Option<Arc<PoolEntry>> {
    let owner = reservation_pool_owner(Some(reservation))?;
    if !owner.2 {
        return WS_POOL.lock().ok()?.remove(&owner);
    }
    super::continuation::if_current_turn_for_owner(reservation, || {
        WS_POOL.lock().ok()?.remove(&owner)
    })
    .flatten()
}

pub(super) fn take_pool_entry_for_request(
    reservation: Option<&ContinuationReservation>,
    identity: Option<&HandshakeIdentity>,
) -> Result<Option<Arc<PoolEntry>>, CodexError> {
    let candidate = reservation.map(ContinuationReservation::candidate);
    let requires_origin = candidate
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let expected_socket_id = reservation.and_then(ContinuationReservation::origin_socket_id);
    let pool_owner = reservation_pool_owner(reservation);
    let pooled = reservation
        .filter(|_| pool_owner.is_some())
        .and_then(pool_take_for_turn)
        .filter(|entry| now_ms().saturating_sub(entry.created_at) <= POOL_IDLE_TTL_MS)
        .filter(|entry| identity.is_none_or(|identity| entry.identity.as_ref() == Some(identity)));

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
    pool_insert_for_turn_token(reservation, entry).is_some()
}

pub(super) fn pool_insert_for_turn_token(
    reservation: &ContinuationReservation,
    entry: Arc<PoolEntry>,
) -> Option<u64> {
    let owner = reservation_pool_owner(Some(reservation))?;
    if !owner.2 {
        return pool_insert_token(owner, entry, true);
    }
    super::continuation::if_current_turn_for_owner(reservation, || {
        pool_insert_token(owner, entry, true)
    })
    .flatten()
}

pub(super) fn pool_remove_entry(owner: &PoolKey, entry: &Arc<PoolEntry>) {
    invalidate_pool_entry(owner, entry);
}

#[cfg(test)]
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

pub(super) fn invalidate_codex_websocket_pool_return(
    reservation: &ContinuationReservation,
    return_id: Option<u64>,
) {
    let (Some(owner), Some(return_id)) = (reservation_pool_owner(Some(reservation)), return_id)
    else {
        return;
    };
    if let Ok(mut pool) = WS_POOL.lock()
        && pool
            .get(&owner)
            .is_some_and(|entry| entry.return_id.load(Ordering::Relaxed) == return_id)
    {
        pool.remove(&owner);
    }
}

fn pool_insert_if_vacant_or_same(owner: PoolKey, entry: Arc<PoolEntry>) -> bool {
    pool_insert_token(owner, entry, false).is_some()
}

fn pool_insert_token(owner: PoolKey, entry: Arc<PoolEntry>, returned: bool) -> Option<u64> {
    let mut guard = WS_POOL.lock().unwrap();
    let now = now_ms();
    let expired_keys: Vec<_> = guard
        .iter()
        .filter(|(_, pooled)| now.saturating_sub(pooled.created_at) > POOL_IDLE_TTL_MS)
        .map(|(key, _)| key.clone())
        .collect();
    let expired: Vec<_> = expired_keys
        .iter()
        .filter_map(|key| guard.remove(key))
        .collect();
    if let Some(existing) = guard.get(&owner) {
        let same = Arc::ptr_eq(existing, &entry);
        drop(guard);
        drop(expired);
        return same.then(|| entry.return_id.load(Ordering::Relaxed));
    }
    if *IDLE_POOL_BOUND == 0 {
        drop(guard);
        drop(expired);
        return None;
    }
    let evicted = if guard.len() >= *IDLE_POOL_BOUND {
        guard
            .iter()
            .min_by_key(|(_, entry)| entry.last_activity.load(Ordering::Relaxed))
            .map(|(key, _)| key.clone())
            .and_then(|key| guard.remove(&key))
    } else {
        None
    };
    let return_id = next_monotonic_nonzero(&POOL_RETURN_SEQUENCE, "WebSocket return ID");
    entry.return_id.store(return_id, Ordering::Relaxed);
    entry.touch();
    if returned {
        *entry.returned_at.lock().unwrap() = Some(tokio::time::Instant::now());
    }
    guard.insert(owner, entry);
    drop(guard);
    drop(evicted);
    drop(expired);
    Some(return_id)
}

#[cfg(test)]
fn pool_insert(owner: ConversationIdentity, entry: Arc<PoolEntry>) {
    entry.touch();
    let mut guard = WS_POOL.lock().unwrap();
    guard.insert((owner, ReasoningAccount::default(), true), entry);
}

struct ConnectGateState {
    last_start: Option<tokio::time::Instant>,
    spacing: Duration,
    cooldown_until: Option<tokio::time::Instant>,
    queue: std::collections::VecDeque<u64>,
    waiting: std::collections::VecDeque<(u64, String)>,
    next_id: u64,
}

pub(super) struct WebSocketConnectGate {
    last_start: Mutex<ConnectGateState>,
    floor: Duration,
    changed: tokio::sync::Notify,
    handshakes: tokio::sync::Semaphore,
    queued: AtomicU64,
    in_flight: AtomicU64,
    spacing_ms: AtomicU64,
}

pub(super) struct HandshakePermit<'a> {
    gate: &'a WebSocketConnectGate,
    _permit: tokio::sync::SemaphorePermit<'a>,
}

impl Drop for HandshakePermit<'_> {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

struct ConnectWaiter<'a> {
    gate: &'a WebSocketConnectGate,
    id: u64,
}

impl Drop for ConnectWaiter<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.last_start.lock().unwrap();
        state.queue.retain(|id| *id != self.id);
        state.waiting.retain(|(id, _)| *id != self.id);
        self.gate.queued.fetch_sub(1, Ordering::Relaxed);
        drop(state);
        self.gate.changed.notify_waiters();
    }
}

impl WebSocketConnectGate {
    fn queued_request_ids(&self) -> Vec<String> {
        self.last_start
            .lock()
            .unwrap()
            .waiting
            .iter()
            .map(|(_, id)| id.clone())
            .collect()
    }

    pub(super) fn new(start_spacing: Duration) -> Self {
        Self {
            last_start: Mutex::new(ConnectGateState {
                last_start: None,
                spacing: start_spacing,
                cooldown_until: None,
                queue: Default::default(),
                waiting: Default::default(),
                next_id: 0,
            }),
            floor: start_spacing,
            changed: tokio::sync::Notify::new(),
            handshakes: tokio::sync::Semaphore::new(4),
            queued: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            spacing_ms: AtomicU64::new(start_spacing.as_millis().min(u64::MAX as u128) as u64),
        }
    }

    #[cfg(test)]
    pub(super) async fn wait_to_start(
        &self,
        before_start: impl std::future::Future<Output = ()>,
    ) -> HandshakePermit<'_> {
        self.wait_to_start_until(
            "",
            before_start,
            tokio::time::Instant::now() + Duration::from_secs(3600),
        )
        .await
        .unwrap()
    }

    pub(super) async fn wait_to_start_until(
        &self,
        request_id: &str,
        before_start: impl std::future::Future<Output = ()>,
        queue_deadline: tokio::time::Instant,
    ) -> Result<HandshakePermit<'_>, ()> {
        before_start.await;
        let waiter = {
            let mut state = self.last_start.lock().unwrap();
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1);
            state.waiting.push_back((id, request_id.to_owned()));
            state.queue.push_back(id);
            self.queued.fetch_add(1, Ordering::Relaxed);
            ConnectWaiter { gate: self, id }
        };
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.last_start.lock().unwrap().queue.front() == Some(&waiter.id) {
                break;
            }
            tokio::select! { _ = &mut notified => {}, _ = tokio::time::sleep_until(queue_deadline) => return Err(()) }
        }
        let permit = tokio::time::timeout_at(queue_deadline, self.handshakes.acquire())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let deadline = {
                let mut state = self.last_start.lock().unwrap();
                let now = tokio::time::Instant::now();
                if now >= queue_deadline {
                    drop(state);
                    return Err(());
                }
                if state.queue.front() == Some(&waiter.id) {
                    let deadline = state
                        .last_start
                        .map(|last| last + state.spacing)
                        .unwrap_or(now)
                        .max(state.cooldown_until.unwrap_or(now));
                    if deadline <= now {
                        state.last_start = Some(now);
                        state.queue.pop_front();
                        drop(state);
                        drop(waiter);
                        self.in_flight.fetch_add(1, Ordering::Relaxed);
                        return Ok(HandshakePermit {
                            gate: self,
                            _permit: permit,
                        });
                    }
                    Some(deadline)
                } else {
                    None
                }
            };
            if let Some(deadline) = deadline {
                tokio::select! { _ = tokio::time::sleep_until(deadline) => {}, _ = &mut notified => {} }
            } else {
                notified.await;
            }
        }
    }

    pub(super) fn record_forbidden(&self, cooldown: Duration) {
        let mut state = self.last_start.lock().unwrap();
        let now = tokio::time::Instant::now();
        if state.cooldown_until.is_none_or(|until| until <= now) {
            state.spacing = state.spacing.saturating_mul(2).min(Duration::from_secs(8));
            state.cooldown_until = Some(now + cooldown);
        }
        self.spacing_ms.store(
            state.spacing.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        drop(state);
        self.changed.notify_waiters();
    }

    #[cfg(test)]
    pub(super) fn last_start_for_tests(&self) -> Option<tokio::time::Instant> {
        self.last_start.lock().unwrap().last_start
    }

    #[cfg(test)]
    pub(super) fn spacing_for_tests(&self) -> Duration {
        self.last_start.lock().unwrap().spacing
    }

    pub(super) fn record_success(&self) {
        let mut state = self.last_start.lock().unwrap();
        state.spacing = (state.spacing / 2).max(self.floor);
        self.spacing_ms.store(
            state.spacing.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        drop(state);
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::super::websocket_test_support::*;
    use super::*;

    fn pool_key_for(owner: &ConversationIdentity) -> PoolKey {
        (owner.clone(), ReasoningAccount::default(), true)
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

    #[tokio::test]
    async fn queued_attempts_keep_arrival_order_and_remove_cancelled() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::ZERO));
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(gate.wait_to_start(async {}).await);
        }
        let first_gate = gate.clone();
        let first = tokio::spawn(async move {
            drop(
                first_gate
                    .wait_to_start_until(
                        "first",
                        async {},
                        tokio::time::Instant::now() + Duration::from_secs(60),
                    )
                    .await,
            );
        });
        tokio::task::yield_now().await;
        let second_gate = gate.clone();
        let second = tokio::spawn(async move {
            drop(
                second_gate
                    .wait_to_start_until(
                        "second",
                        async {},
                        tokio::time::Instant::now() + Duration::from_secs(60),
                    )
                    .await,
            );
        });
        tokio::task::yield_now().await;
        assert_eq!(gate.queued_request_ids(), vec!["first", "second"]);
        let (queue, waiting) = {
            let state = gate.last_start.lock().unwrap();
            (
                state.queue.iter().copied().collect::<Vec<_>>(),
                state.waiting.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            )
        };
        assert_eq!(queue, waiting);
        first.abort();
        let _ = first.await;
        assert_eq!(gate.queued_request_ids(), vec!["second"]);
        drop(permits.pop());
        second.await.unwrap();
        assert!(gate.queued_request_ids().is_empty());
    }

    #[tokio::test]
    async fn snapshot_reports_queue_places_and_updates_after_cancel() {
        let _lock = lock_codex_websocket_pool_for_tests().await;
        let permits = WS_CONNECT_GATE.handshakes.acquire_many(4).await.unwrap();
        let monitor = crate::monitor::MonitorHandle::default();
        for id in ["first", "second", "http"] {
            monitor.request_started(id, None, None, crate::monitor::EndpointKind::Messages);
        }
        let first = tokio::spawn(async {
            drop(
                WS_CONNECT_GATE
                    .wait_to_start_until(
                        "first",
                        async {},
                        tokio::time::Instant::now() + Duration::from_secs(60),
                    )
                    .await,
            );
        });
        tokio::task::yield_now().await;
        let second = tokio::spawn(async {
            drop(
                WS_CONNECT_GATE
                    .wait_to_start_until(
                        "second",
                        async {},
                        tokio::time::Instant::now() + Duration::from_secs(60),
                    )
                    .await,
            );
        });
        tokio::task::yield_now().await;
        let snapshot = monitor.snapshot();
        assert_eq!(codex_connection_counters().queued_attempts, 2);
        assert_eq!(
            snapshot.codex_connections.as_ref().unwrap().queued_attempts,
            2
        );
        let place = |id: &str| {
            snapshot
                .active
                .iter()
                .find(|r| r.request_id == id)
                .unwrap()
                .queue_place
        };
        assert_eq!(place("first"), Some(1));
        assert_eq!(place("second"), Some(2));
        assert_eq!(place("http"), None);
        first.abort();
        let _ = first.await;
        let snapshot = monitor.snapshot();
        assert_eq!(
            snapshot
                .active
                .iter()
                .find(|r| r.request_id == "second")
                .unwrap()
                .queue_place,
            Some(1)
        );
        second.abort();
        let _ = second.await;
        drop(permits);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_start_does_not_hold_gate_lock() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::from_secs(1)));
        drop(gate.wait_to_start(async {}).await);
        let waiting_gate = gate.clone();
        let waiting = tokio::spawn(async move { drop(waiting_gate.wait_to_start(async {}).await) });
        tokio::task::yield_now().await;
        assert!(gate.last_start.try_lock().is_ok());
        waiting.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn burst_slots_and_cancelled_waiter() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::from_secs(1)));
        let start = tokio::time::Instant::now();
        drop(gate.wait_to_start(async {}).await);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tasks = Vec::new();
        for id in 0..4 {
            let gate = gate.clone();
            let tx = tx.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = gate.wait_to_start(async {}).await;
                tx.send((id, tokio::time::Instant::now())).unwrap();
            }));
            tokio::task::yield_now().await;
        }
        tasks[1].abort();
        for (second, expected_id) in [0, 2, 3].into_iter().enumerate() {
            let (id, started) = rx.recv().await.unwrap();
            assert_eq!(id, expected_id);
            assert_eq!(
                started.duration_since(start),
                Duration::from_secs(second as u64 + 1)
            );
        }
        for (id, task) in tasks.into_iter().enumerate() {
            if id == 1 {
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                task.await.unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_does_not_start_handshake() {
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let deadline = tokio::time::Instant::now();
        assert!(
            gate.wait_to_start_until("", async {}, deadline)
                .await
                .is_err()
        );
        assert!(gate.last_start.lock().unwrap().last_start.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn permit_ready_at_deadline_cannot_start() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::ZERO));
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(gate.wait_to_start(async {}).await);
        }
        let previous = gate.last_start.lock().unwrap().last_start;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            waiter_gate
                .wait_to_start_until("", async {}, deadline)
                .await
                .is_err()
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        permits.pop();
        assert!(waiter.await.unwrap());
        assert_eq!(gate.last_start.lock().unwrap().last_start, previous);
    }

    #[tokio::test(start_paused = true)]
    async fn forbidden_cooldown_bounds_growth_and_success_decays() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        gate.record_forbidden(Duration::from_secs(3));
        gate.record_forbidden(Duration::from_secs(3));
        assert_eq!(
            gate.last_start.lock().unwrap().spacing,
            Duration::from_secs(2)
        );
        assert_eq!(gate.spacing_ms.load(Ordering::Relaxed), 2000);
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(3)).await;
            gate.record_forbidden(Duration::from_secs(3));
        }
        assert_eq!(
            gate.last_start.lock().unwrap().spacing,
            Duration::from_secs(8)
        );
        for _ in 0..4 {
            gate.record_success();
        }
        assert_eq!(
            gate.last_start.lock().unwrap().spacing,
            Duration::from_secs(1)
        );
        assert_eq!(gate.spacing_ms.load(Ordering::Relaxed), 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn handshakes_are_bounded_and_cancellation_releases_permits() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::ZERO));
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(gate.wait_to_start(async {}).await);
        }
        assert_eq!(gate.in_flight.load(Ordering::Relaxed), 4);
        let other = gate.clone();
        let task = tokio::spawn(async move { drop(other.wait_to_start(async {}).await) });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert_eq!(gate.last_start.lock().unwrap().waiting.len(), 1);
        permits.pop();
        task.await.unwrap();
        assert_eq!(gate.in_flight.load(Ordering::Relaxed), 3);
        drop(permits);
        assert_eq!(gate.in_flight.load(Ordering::Relaxed), 0);
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
            let _permit = first_gate
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
            let _permit = second_gate.wait_to_start(async {}).await;
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
    async fn expired_socket_is_not_checked_out() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let owner = main_owner("expired-socket");
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &continuation_request(),
            true,
        );
        let mut entry = PoolEntry::new(raw_test_stream(None).await);
        entry.created_at = now_ms().saturating_sub(POOL_IDLE_TTL_MS + 1);
        assert!(pool_insert_for_turn(&reservation, Arc::new(entry)));
        assert!(
            take_pool_entry_for_request(Some(&reservation), None)
                .unwrap()
                .is_none()
        );
        super::super::continuation::abort_continuation_for_owner(&reservation);
    }

    #[tokio::test]
    async fn reused_socket_rejects_completed_and_missing_created_ids() {
        let entry = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        assert!(entry.record_completed("old"));
        for created in [
            serde_json::json!({"type":"response.created", "response":{"id":"old"}}),
            serde_json::json!({"type":"response.created"}),
        ] {
            let (client_io, server_io) = tokio::io::duplex(2048);
            let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            server
                .send(Message::text(created.to_string()))
                .await
                .unwrap();
            let (tx, mut rx) = mpsc::channel(2);
            let (_, terminal) = stream_ws_events(
                &mut client,
                1_000,
                None,
                &tx,
                &Default::default(),
                true,
                Some(&entry),
            )
            .await;
            assert!(matches!(
                terminal,
                Some(Err(CodexError {
                    origin: super::super::client::CodexErrorOrigin::WebSocketTransient,
                    ..
                }))
            ));
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn returning_socket_sweeps_expired_idle_entries() {
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let old_owner = main_owner("sweep-old");
        let mut old = PoolEntry::new(raw_test_stream(None).await);
        old.created_at = now_ms().saturating_sub(POOL_IDLE_TTL_MS + 1);
        let old = Arc::new(old);
        let weak = Arc::downgrade(&old);
        pool_insert(old_owner.clone(), old);
        let new = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        assert!(pool_insert_if_vacant_or_same(
            (main_owner("sweep-new"), ReasoningAccount::default(), false),
            new,
        ));
        assert!(pooled_socket_id_for_tests(&old_owner).is_none());
        assert!(weak.upgrade().is_none());
        clear_codex_websocket_pool_for_tests();
    }

    #[test]
    fn idle_pool_bound_follows_descriptor_limit() {
        assert_eq!(idle_pool_bound(Some(256)), 25);
        assert_eq!(idle_pool_bound(Some(4096)), 1441);
        assert_eq!(idle_pool_bound(Some(16384)), 2048);
        assert_eq!(idle_pool_bound(None), 2048);
    }

    #[test]
    fn handshake_identity_distinguishes_credentials_and_application_headers() {
        use super::super::sources::CredentialId;
        use crate::config::codex_auth::AuthSource;
        let first = CredentialId::new(AuthSource::ChatGpt, b"synthetic-first");
        let second = CredentialId::new(AuthSource::ChatGpt, b"synthetic-second");
        let mut headers = HeaderMap::new();
        headers.insert("x-lane", "lite".parse().unwrap());
        let identity = handshake_identity(first, &headers);
        assert!(identity == handshake_identity(first, &headers));
        assert!(identity != handshake_identity(second, &headers));
        headers.insert("x-lane", "full".parse().unwrap());
        assert!(identity != handshake_identity(first, &headers));
    }

    #[tokio::test]
    async fn checkout_rejects_changed_identity_and_separates_continuation_slots() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let owner = main_owner("identity-slots");
        let request = continuation_request();
        let enabled = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let disabled = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let first = super::super::sources::CredentialId::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            b"synthetic-first",
        );
        let second = super::super::sources::CredentialId::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            b"synthetic-second",
        );
        let headers = HeaderMap::new();
        let identity = handshake_identity(first, &headers);
        let entry = Arc::new(PoolEntry::with_identity(
            raw_test_stream(None).await,
            identity.clone(),
        ));
        let weak = Arc::downgrade(&entry);
        assert!(pool_insert_for_turn(&enabled, entry));
        assert!(
            take_pool_entry_for_request(Some(&disabled), Some(&identity))
                .unwrap()
                .is_none()
        );
        assert!(
            take_pool_entry_for_request(
                Some(&enabled),
                Some(&handshake_identity(second, &headers))
            )
            .unwrap()
            .is_none()
        );
        assert!(
            take_pool_entry_for_request(Some(&enabled), Some(&identity))
                .unwrap()
                .is_none()
        );
        assert!(weak.upgrade().is_none());
        let entry = Arc::new(PoolEntry::with_identity(
            raw_test_stream(None).await,
            identity,
        ));
        let weak = Arc::downgrade(&entry);
        assert!(pool_insert_for_turn(&enabled, entry));
        let mut changed = headers.clone();
        changed.insert("x-lane", "full".parse().unwrap());
        assert!(
            take_pool_entry_for_request(Some(&enabled), Some(&handshake_identity(first, &changed)))
                .unwrap()
                .is_none()
        );
        assert!(weak.upgrade().is_none());
        super::super::continuation::abort_continuation_for_owner(&enabled);
    }

    #[tokio::test]
    async fn full_pool_evicts_least_recently_used_socket() {
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        if *IDLE_POOL_BOUND < 2 {
            return;
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let unlocked = Arc::new(AtomicBool::new(false));
        let oldest = Arc::new(PoolEntry::new(
            raw_test_stream(Some((dropped.clone(), unlocked.clone()))).await,
        ));
        assert!(pool_insert_if_vacant_or_same(
            pool_key_for(&main_owner("oldest")),
            oldest
        ));
        let shared = Arc::new(AsyncMutex::new(raw_test_stream(None).await));
        for index in 1..*IDLE_POOL_BOUND {
            let entry = Arc::new(PoolEntry {
                ws: shared.clone(),
                socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
                identity: None,
                created_at: now_ms(),
                last_activity: AtomicU64::new(next_pool_activity()),
                returned_at: Mutex::new(None),
                return_id: AtomicU64::new(0),
                completed_ids: Mutex::new(std::collections::HashSet::new()),
            });
            assert!(pool_insert_if_vacant_or_same(
                pool_key_for(&main_owner(&format!("idle-{index}"))),
                entry
            ));
        }
        let newest = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        assert!(pool_insert_if_vacant_or_same(
            pool_key_for(&main_owner("newest")),
            newest
        ));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(unlocked.load(Ordering::SeqCst));
        assert!(
            !WS_POOL
                .lock()
                .unwrap()
                .contains_key(&pool_key_for(&main_owner("oldest")))
        );
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn full_context_retry_discards_pooled_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("full-context-empty-retry");
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &continuation_request(),
            true,
        );
        let entry = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        assert!(pool_insert_for_turn(&reservation, entry));
        let mut retry = Some(reservation.clone());
        assert!(!super::super::live_completion::drop_live_continuation_for_retry(&mut retry));
        assert!(super::super::client_live::should_reset_websocket_pool(
            retry.as_ref()
        ));
        super::super::client_live::invalidate_live_continuation_pool(retry.as_ref());
        assert!(pooled_socket_id_for_tests(&owner).is_none());
        super::super::continuation::abort_continuation_for_owner(&reservation);
    }

    #[tokio::test]
    async fn http_transition_removes_continuation_off_socket() {
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("http-continuation-off");
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &continuation_request(),
            false,
        );
        assert!(pool_insert_for_turn(
            &reservation,
            Arc::new(PoolEntry::new(raw_test_stream(None).await))
        ));
        super::super::client_live::invalidate_live_continuation_pool(Some(&reservation));
        assert!(pooled_socket_id_for_tests(&owner).is_none());
    }

    #[tokio::test]
    async fn continuation_off_uses_an_exclusive_pool_slot() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let owner = main_owner("continuation-off");
        let request = continuation_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let retry = reservation.full_context_retry();
        assert!(!reservation_pool_owner(Some(&retry)).unwrap().2);
        let entry = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        assert!(pool_insert_for_turn(&reservation, entry.clone()));
        let checkout = take_pool_entry_for_request(Some(&reservation), None)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&checkout, &entry));
        assert!(
            take_pool_entry_for_request(Some(&reservation), None)
                .unwrap()
                .is_none()
        );
        clear_codex_websocket_pool_for_tests();
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
            "",
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            "ws://127.0.0.1:9/responses",
            &HeaderMap::new(),
            None,
            None,
            Some(&continuation),
            50,
            50,
            &super::super::recovery::RecoveryBudget::default(),
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
            "",
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            "ws://127.0.0.1:9/responses",
            &HeaderMap::new(),
            None,
            None,
            Some(&continuation),
            50,
            50,
            &super::super::recovery::RecoveryBudget::default(),
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
                        websocket.send(Message::text(serde_json::json!({"type":"response.created","response":{"id":"resp_terminal_order"}}).to_string())).await.unwrap();
                        websocket
                            .send(Message::text(
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {"id": "resp_terminal_order", "status": "completed"}
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
            recovery_budget: Default::default(),
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
            None,
            &serde_json::json!({"type":"response.create","input":[]}),
            &context,
            None,
            1_000,
            1_000,
            Some(&continuation),
            Arc::default(),
        )
        .await
        .unwrap();
        assert_eq!(events.socket_id(), None);
        release_response_tx.send(()).unwrap();
        let created = events.recv().await.unwrap().unwrap();
        assert_eq!(created["type"], "response.created");
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
                        websocket.send(Message::text(serde_json::json!({"type":"response.created","response":{"id":"resp_dropped_receiver"}}).to_string())).await.unwrap();
                        request_seen_tx.send(()).unwrap();
                        release_response_rx.await.unwrap();
                        websocket
                            .send(Message::text(
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {"id": "resp_dropped_receiver", "status": "completed"}
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
            "",
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &headers,
            None,
            None,
            Some(&continuation),
            1_000,
            1_000,
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        .unwrap();
        let exact = ready.entry.clone();
        let mut events = start_codex_websocket_events(
            ready,
            &body,
            serde_json::to_string(&body).unwrap(),
            &headers,
            Some(&continuation),
            Arc::default(),
            None,
        );

        request_seen_rx.await.unwrap();
        assert_eq!(
            events.recv().await.unwrap().unwrap()["type"],
            "response.created"
        );
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
            "",
            &client,
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            1_000,
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        .unwrap()
    }
}
