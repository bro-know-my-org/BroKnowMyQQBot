//! Event deduplication storage boundary and bounded in-memory implementation.

use std::{
    collections::{HashMap, VecDeque},
    error::Error as StdError,
    sync::Mutex,
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{AdapterId, EventId};

const MAX_DEDUP_KEY_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub adapter: AdapterId,
    pub event: EventId,
}

impl DedupKey {
    pub fn new(adapter: AdapterId, event: EventId) -> Self {
        Self { adapter, event }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct DedupClaim {
    key: DedupKey,
    token: Uuid,
}

impl DedupClaim {
    pub fn new(key: DedupKey) -> Self {
        Self {
            key,
            token: Uuid::new_v4(),
        }
    }

    pub fn key(&self) -> &DedupKey {
        &self.key
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self {
            key: self.key.clone(),
            token: self.token,
        }
    }

    const fn token(&self) -> Uuid {
        self.token
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DedupErrorKind {
    #[error("retryable")]
    Retryable,
    #[error("permanent")]
    Permanent,
}

#[derive(Debug, Error)]
#[error("event deduplication store failed ({kind}): {message}")]
pub struct DedupError {
    kind: DedupErrorKind,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl DedupError {
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            kind: DedupErrorKind::Retryable,
            message: message.into(),
            source: None,
        }
    }

    pub fn retryable_with_source(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: DedupErrorKind::Retryable,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            kind: DedupErrorKind::Permanent,
            message: message.into(),
            source: None,
        }
    }

    pub fn permanent_with_source(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: DedupErrorKind::Permanent,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub const fn kind(&self) -> DedupErrorKind {
        self.kind
    }

    pub const fn is_retryable(&self) -> bool {
        matches!(self.kind, DedupErrorKind::Retryable)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[async_trait]
pub trait DedupStore: Send + Sync + 'static {
    /// Atomically claims an event for processing.
    ///
    /// Implementations must bound concurrent claims. The Runtime tracks every
    /// returned claim, releases claims whose processing did not finish, and
    /// retries commit for successfully processed claims during shutdown.
    /// Durable stores should additionally provide their own crash-recovery or
    /// fencing policy. `Some` means the caller owns the returned claim; `None`
    /// means the event was already committed successfully.
    ///
    /// `claim` must be cancellation-safe: if its future is dropped, it must not
    /// leave ownership that only the cancelled caller could release. A backend
    /// that acquires ownership before returning must fence or recover that
    /// ownership independently.
    async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError>;

    /// Marks this exact claim as successfully processed. Finalization futures
    /// must be cancellation-safe. Repeated finalization of a retained claim
    /// must be idempotent; bounded stores may reject a retry after their
    /// documented retention window has evicted the finalized claim.
    async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError>;

    /// Releases this exact claim so a retry can process the event. Finalization
    /// futures must be cancellation-safe. Repeated finalization of a retained
    /// claim must be idempotent; bounded stores may forget released claims.
    async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError>;
}

#[derive(Debug)]
pub struct MemoryDedupStore {
    capacity: usize,
    state: Mutex<MemoryState>,
}

struct WaiterPin<'a> {
    store: &'a MemoryDedupStore,
    key: DedupKey,
    active: bool,
}

struct PendingClaimGuard<'a> {
    store: &'a MemoryDedupStore,
    active: bool,
}

impl PendingClaimGuard<'_> {
    fn release(mut self, state: &mut MemoryState) {
        state.pending_claims = state.pending_claims.saturating_sub(1);
        self.active = false;
    }
}

impl Drop for PendingClaimGuard<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_claims = state.pending_claims.saturating_sub(1);
    }
}

impl WaiterPin<'_> {
    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for WaiterPin<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unpin_waiter(&mut state, &self.key);
    }
}

#[derive(Debug)]
struct MemoryState {
    entries: HashMap<DedupKey, MemoryEntry>,
    waiters: HashMap<DedupKey, usize>,
    pending_claims: usize,
    evictable: VecDeque<(DedupKey, Uuid)>,
    capacity_changed: watch::Sender<u64>,
}

impl Default for MemoryState {
    fn default() -> Self {
        let (capacity_changed, _) = watch::channel(0);
        Self {
            entries: HashMap::new(),
            waiters: HashMap::new(),
            pending_claims: 0,
            evictable: VecDeque::new(),
            capacity_changed,
        }
    }
}

#[derive(Debug)]
enum MemoryEntry {
    InFlight {
        token: Uuid,
        changed: watch::Sender<u64>,
    },
    Committed {
        token: Uuid,
    },
}

impl MemoryDedupStore {
    pub fn try_new(capacity: usize) -> Result<Self, DedupError> {
        if capacity == 0 {
            return Err(DedupError::permanent(
                "deduplication capacity must be at least one",
            ));
        }
        Ok(Self {
            capacity,
            state: Mutex::new(MemoryState::default()),
        })
    }

    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl DedupStore for MemoryDedupStore {
    async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
        if key.adapter.as_str().len() + key.event.as_str().len() > MAX_DEDUP_KEY_BYTES {
            return Err(DedupError::permanent(format!(
                "deduplication key exceeds {MAX_DEDUP_KEY_BYTES} bytes"
            )));
        }
        let mut waiter_pin: Option<WaiterPin<'_>> = None;
        let mut pending_guard: Option<PendingClaimGuard<'_>> = None;
        loop {
            let wait = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match state.entries.get(&key) {
                    Some(MemoryEntry::Committed { .. }) => {
                        if let Some(guard) = pending_guard.take() {
                            guard.release(&mut state);
                        }
                        if let Some(pin) = waiter_pin.take() {
                            unpin_waiter(&mut state, &key);
                            pin.disarm();
                        }
                        return Ok(None);
                    }
                    Some(MemoryEntry::InFlight { changed, .. }) => {
                        let changed = changed.subscribe();
                        admit_pending_claim(self, &mut state, &mut pending_guard)?;
                        if waiter_pin.is_none() {
                            pin_waiter(&mut state, &key);
                            waiter_pin = Some(WaiterPin {
                                store: self,
                                key: key.clone(),
                                active: true,
                            });
                        }
                        Some(changed)
                    }
                    None => {
                        evict_committed(&mut state, self.capacity);
                        if state.entries.len() >= self.capacity {
                            admit_pending_claim(self, &mut state, &mut pending_guard)?;
                            Some(state.capacity_changed.subscribe())
                        } else {
                            if let Some(guard) = pending_guard.take() {
                                guard.release(&mut state);
                            }
                            if let Some(pin) = waiter_pin.take() {
                                unpin_waiter(&mut state, &key);
                                pin.disarm();
                            }
                            let claim = DedupClaim::new(key.clone());
                            let (changed, _) = watch::channel(0);
                            state.entries.insert(
                                key,
                                MemoryEntry::InFlight {
                                    token: claim.token(),
                                    changed,
                                },
                            );
                            return Ok(Some(claim));
                        }
                    }
                }
            };
            if let Some(mut changed) = wait {
                let _ = changed.changed().await;
            }
        }
    }

    async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = state.entries.get_mut(claim.key()) else {
            return Err(DedupError::permanent(
                "cannot commit an event without a claim",
            ));
        };
        let changed = match entry {
            MemoryEntry::InFlight { token, changed } if *token == claim.token() => changed.clone(),
            MemoryEntry::Committed { token, .. } if *token == claim.token() => return Ok(()),
            MemoryEntry::InFlight { .. } | MemoryEntry::Committed { .. } => {
                return Err(DedupError::permanent("deduplication claim is stale"));
            }
        };
        *entry = MemoryEntry::Committed {
            token: claim.token(),
        };
        if state.waiters.get(claim.key()).copied().unwrap_or(0) == 0 {
            state
                .evictable
                .push_back((claim.key().clone(), claim.token()));
        }
        signal(&changed);
        signal(&state.capacity_changed);
        Ok(())
    }

    async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError> {
        let (changed, capacity_changed) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.entries.get(claim.key()) {
                Some(MemoryEntry::InFlight { token, changed, .. }) if *token == claim.token() => {
                    let changed = changed.clone();
                    let capacity_changed = state.capacity_changed.clone();
                    state.entries.remove(claim.key());
                    (Some(changed), Some(capacity_changed))
                }
                Some(MemoryEntry::Committed { token, .. }) if *token == claim.token() => {
                    (None, None)
                }
                Some(MemoryEntry::InFlight { .. } | MemoryEntry::Committed { .. }) => {
                    return Err(DedupError::permanent("deduplication claim is stale"));
                }
                None => (None, None),
            }
        };
        if let Some(changed) = changed {
            signal(&changed);
        }
        if let Some(capacity_changed) = capacity_changed {
            signal(&capacity_changed);
        }
        Ok(())
    }
}

fn admit_pending_claim<'a>(
    store: &'a MemoryDedupStore,
    state: &mut MemoryState,
    guard: &mut Option<PendingClaimGuard<'a>>,
) -> Result<(), DedupError> {
    if guard.is_some() {
        return Ok(());
    }
    if state.pending_claims >= store.capacity {
        return Err(DedupError::retryable(
            "deduplication claim wait capacity is saturated",
        ));
    }
    state.pending_claims += 1;
    *guard = Some(PendingClaimGuard {
        store,
        active: true,
    });
    Ok(())
}

fn pin_waiter(state: &mut MemoryState, key: &DedupKey) {
    let waiters = state.waiters.entry(key.clone()).or_default();
    *waiters = waiters.saturating_add(1);
}

fn unpin_waiter(state: &mut MemoryState, key: &DedupKey) {
    let remove = state.waiters.get_mut(key).is_some_and(|waiters| {
        *waiters = waiters.saturating_sub(1);
        *waiters == 0
    });
    if remove {
        state.waiters.remove(key);
        if let Some(MemoryEntry::Committed { token }) = state.entries.get(key) {
            state.evictable.push_back((key.clone(), *token));
        }
        signal(&state.capacity_changed);
    }
}

fn evict_committed(state: &mut MemoryState, capacity: usize) {
    while state.entries.len() >= capacity {
        let Some((expired, token)) = state.evictable.pop_front() else {
            break;
        };
        if matches!(
            state.entries.get(&expired),
            Some(MemoryEntry::Committed { token: current }) if *current == token
        ) {
            state.entries.remove(&expired);
        }
    }
}

fn signal(sender: &watch::Sender<u64>) {
    sender.send_modify(|version| *version = version.wrapping_add(1));
}

#[cfg(test)]
mod tests {
    use std::{future::Future as _, future::poll_fn, sync::Arc, task::Poll, time::Duration};

    use tokio::time::timeout;

    use super::{DedupKey, DedupStore, MemoryDedupStore};
    use crate::{AdapterId, EventId};

    fn key(value: &str) -> DedupKey {
        DedupKey::new(AdapterId::new("adapter"), EventId::new(value))
    }

    #[tokio::test]
    async fn memory_store_is_bounded_and_keeps_recent_ids() {
        let store = MemoryDedupStore::try_new(2).unwrap();
        for value in ["one", "two", "three"] {
            let claim = store.claim(key(value)).await.unwrap().unwrap();
            store.commit(&claim).await.unwrap();
        }
        assert!(store.claim(key("two")).await.unwrap().is_none());
        assert!(store.claim(key("three")).await.unwrap().is_none());
        assert_eq!(store.len(), 2);
        let claim = store.claim(key("one")).await.unwrap().unwrap();
        store.release(&claim).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_claim_waits_for_commit_or_release() {
        let store = Arc::new(MemoryDedupStore::try_new(2).unwrap());
        let event = key("shared");
        let claim = store.claim(event.clone()).await.unwrap().unwrap();

        let mut waiting = tokio::spawn({
            let store = Arc::clone(&store);
            let event = event.clone();
            async move { store.claim(event).await }
        });
        assert!(
            timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        store.release(&claim).await.unwrap();
        let replacement = timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        store.commit(&replacement).await.unwrap();
        assert!(store.claim(event).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_flight_claims_are_bounded() {
        let store = Arc::new(MemoryDedupStore::try_new(2).unwrap());
        let first = store.claim(key("one")).await.unwrap().unwrap();
        let second = store.claim(key("two")).await.unwrap().unwrap();
        let mut waiting = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.claim(key("three")).await }
        });
        assert!(
            timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        store.release(&first).await.unwrap();
        let third = timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        store.release(&third).await.unwrap();
        store.release(&second).await.unwrap();
    }

    #[tokio::test]
    async fn pending_claim_waiters_are_bounded_and_cancellation_safe() {
        let store = MemoryDedupStore::try_new(1).unwrap();
        let owner = store.claim(key("owned")).await.unwrap().unwrap();

        let mut waiter = Box::pin(store.claim(key("owned")));
        poll_fn(|context| match waiter.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("first pending claim unexpectedly completed"),
        })
        .await;

        let error = store.claim(key("other")).await.unwrap_err();
        assert_eq!(
            error.message(),
            "deduplication claim wait capacity is saturated"
        );
        drop(waiter);

        let mut replacement = Box::pin(store.claim(key("other")));
        poll_fn(|context| match replacement.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("replacement pending claim unexpectedly completed"),
        })
        .await;
        store.release(&owner).await.unwrap();
        let replacement = timeout(Duration::from_secs(1), replacement)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        store.release(&replacement).await.unwrap();
    }

    #[tokio::test]
    async fn oversized_deduplication_keys_are_rejected() {
        let store = MemoryDedupStore::try_new(1).unwrap();
        assert!(store.claim(key(&"x".repeat(1024))).await.is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn deduplication_errors_retain_classification_and_sources() {
        let error = MemoryDedupStore::try_new(0).unwrap_err();
        assert_eq!(error.kind(), super::DedupErrorKind::Permanent);
        assert_eq!(
            error.message(),
            "deduplication capacity must be at least one"
        );

        let error = super::DedupError::retryable_with_source(
            "database unavailable",
            std::io::Error::other("connection refused"),
        );
        assert!(error.is_retryable());
        assert_eq!(error.kind(), super::DedupErrorKind::Retryable);
        assert_eq!(error.message(), "database unavailable");
        assert!(std::error::Error::source(&error).is_some());

        let error = super::DedupError::permanent("invalid key");
        assert!(!error.is_retryable());
        assert_eq!(error.kind(), super::DedupErrorKind::Permanent);
    }

    #[tokio::test]
    async fn independently_created_claim_cannot_replace_store_ownership() {
        let store = MemoryDedupStore::try_new(1).unwrap();
        let event = key("owned");
        let claim = store.claim(event.clone()).await.unwrap().unwrap();
        let forged = super::DedupClaim::new(event);

        assert_ne!(claim, forged);
        assert!(store.commit(&forged).await.is_err());
        assert!(store.release(&forged).await.is_err());
        store.commit(&claim).await.unwrap();
    }

    #[tokio::test]
    async fn committed_key_remains_pinned_until_waiting_duplicate_observes_it() {
        let store = MemoryDedupStore::try_new(1).unwrap();
        let event = key("shared");
        let claim = store.claim(event.clone()).await.unwrap().unwrap();
        let mut duplicate = Box::pin(store.claim(event));
        poll_fn(|context| match duplicate.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("duplicate claim unexpectedly completed"),
        })
        .await;

        store.commit(&claim).await.unwrap();
        assert_eq!(
            store.claim(key("other")).await.unwrap_err().message(),
            "deduplication claim wait capacity is saturated"
        );

        assert!(duplicate.await.unwrap().is_none());
        let other = store.claim(key("other")).await.unwrap().unwrap();
        store.release(&other).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_duplicate_wait_removes_its_eviction_pin() {
        let store = MemoryDedupStore::try_new(1).unwrap();
        let event = key("shared");
        let claim = store.claim(event.clone()).await.unwrap().unwrap();
        let mut duplicate = Box::pin(store.claim(event));
        poll_fn(|context| match duplicate.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("duplicate claim unexpectedly completed"),
        })
        .await;
        drop(duplicate);

        store.commit(&claim).await.unwrap();
        let other = store.claim(key("other")).await.unwrap().unwrap();
        store.release(&other).await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_pin_survives_release_and_reclaim_generation_change() {
        let store = MemoryDedupStore::try_new(1).unwrap();
        let event = key("shared");
        let original = store.claim(event.clone()).await.unwrap().unwrap();
        let mut duplicate = Box::pin(store.claim(event.clone()));
        poll_fn(|context| match duplicate.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("duplicate claim unexpectedly completed"),
        })
        .await;

        store.release(&original).await.unwrap();
        let replacement = store.claim(event).await.unwrap().unwrap();
        store.commit(&replacement).await.unwrap();

        assert_eq!(
            store.claim(key("other")).await.unwrap_err().message(),
            "deduplication claim wait capacity is saturated"
        );
        assert!(duplicate.await.unwrap().is_none());
        let other = store.claim(key("other")).await.unwrap().unwrap();
        store.release(&other).await.unwrap();
    }
}
