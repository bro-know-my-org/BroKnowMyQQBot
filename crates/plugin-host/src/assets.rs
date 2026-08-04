//! Short-lived, instance-scoped binary assets.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use plugin_api::AssetReference;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use uuid::Uuid;

const ASSET_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;
const MAX_INSTANCE_ASSETS: usize = 32;
const MAX_INSTANCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_STORE_ASSETS: usize = 256;
const MAX_STORE_BYTES: usize = 256 * 1024 * 1024;
const MAX_MIME_TYPE_BYTES: usize = 127;
const MAX_INSTANCE_ID_BYTES: usize = 256;

#[derive(Debug, Clone)]
pub struct StoredAsset {
    pub mime_type: String,
    pub data: Arc<[u8]>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AssetDigest([u8; 32]);

impl AssetDigest {
    pub(crate) fn from_data(data: &[u8]) -> Self {
        Self(Sha256::digest(data).into())
    }
}

#[derive(Debug)]
struct AssetRecord {
    instance_id: String,
    asset: StoredAsset,
    accounted_bytes: usize,
    deadline: Instant,
}

#[derive(Debug, Clone, Copy, Default)]
struct AssetUsage {
    count: usize,
    bytes: usize,
}

#[derive(Debug, Default)]
struct AssetState {
    records: HashMap<String, AssetRecord>,
    instance_usage: HashMap<String, AssetUsage>,
    total_bytes: usize,
}

#[derive(Debug, Clone)]
struct AssetLimits {
    ttl: Duration,
    max_asset_bytes: usize,
    max_instance_assets: usize,
    max_instance_bytes: usize,
    max_store_assets: usize,
    max_store_bytes: usize,
}

impl Default for AssetLimits {
    fn default() -> Self {
        Self {
            ttl: ASSET_TTL,
            max_asset_bytes: MAX_ASSET_BYTES,
            max_instance_assets: MAX_INSTANCE_ASSETS,
            max_instance_bytes: MAX_INSTANCE_BYTES,
            max_store_assets: MAX_STORE_ASSETS,
            max_store_bytes: MAX_STORE_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AssetStore {
    inner: Arc<Mutex<AssetState>>,
    limits: Arc<AssetLimits>,
}

impl Default for AssetStore {
    fn default() -> Self {
        Self::build(AssetLimits::default())
    }
}

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("asset exceeds the 8 MiB limit")]
    TooLarge,
    #[error("plugin asset quota is exhausted")]
    QuotaExceeded,
    #[error("asset MIME type is invalid")]
    InvalidMimeType,
    #[error("plugin instance ID is invalid")]
    InvalidInstanceId,
    #[error("asset is missing, expired, or belongs to another plugin instance")]
    NotFound,
    #[error("asset store lock is poisoned")]
    Poisoned,
}

impl AssetStore {
    pub fn insert(
        &self,
        instance_id: &str,
        mime_type: String,
        data: Vec<u8>,
    ) -> Result<AssetReference, AssetError> {
        self.preflight(instance_id, &mime_type, &data)?;
        let digest = AssetDigest::from_data(&data);
        self.insert_prehashed(instance_id, mime_type, data, digest)
    }

    pub(crate) fn preflight(
        &self,
        instance_id: &str,
        mime_type: &str,
        data: &[u8],
    ) -> Result<(), AssetError> {
        let accounted_bytes =
            validate_insert_request(self.limits.as_ref(), instance_id, mime_type, data)?;
        let now = Instant::now();
        let mut state = self.inner.lock().map_err(|_| AssetError::Poisoned)?;
        purge_expired(&mut state, now);
        if !has_capacity(&state, &self.limits, instance_id, accounted_bytes) {
            return Err(AssetError::QuotaExceeded);
        }
        Ok(())
    }

    pub(crate) fn insert_prehashed(
        &self,
        instance_id: &str,
        mime_type: String,
        data: Vec<u8>,
        digest: AssetDigest,
    ) -> Result<AssetReference, AssetError> {
        let accounted_bytes =
            validate_insert_request(self.limits.as_ref(), instance_id, &mime_type, &data)?;
        let now = Instant::now();
        let now_wall = now_ms();
        let size_bytes = data.len() as u64;
        let data: Arc<[u8]> = data.into();
        let mut state = self.inner.lock().map_err(|_| AssetError::Poisoned)?;
        purge_expired(&mut state, now);
        if !has_capacity(&state, &self.limits, instance_id, accounted_bytes) {
            return Err(AssetError::QuotaExceeded);
        }
        let asset_id = Uuid::new_v4().to_string();
        let ttl_ms = i64::try_from(self.limits.ttl.as_millis()).unwrap_or(i64::MAX);
        let expires_at_ms = now_wall.saturating_add(ttl_ms);
        state.records.insert(
            asset_id.clone(),
            AssetRecord {
                instance_id: instance_id.to_owned(),
                asset: StoredAsset {
                    mime_type: mime_type.clone(),
                    data,
                },
                accounted_bytes,
                deadline: now + self.limits.ttl,
            },
        );
        state.total_bytes = state.total_bytes.saturating_add(accounted_bytes);
        let usage = state
            .instance_usage
            .entry(instance_id.to_owned())
            .or_default();
        usage.count = usage.count.saturating_add(1);
        usage.bytes = usage.bytes.saturating_add(accounted_bytes);
        let reference = AssetReference {
            asset_id: asset_id.clone(),
            mime_type,
            size_bytes,
            sha256: hex::encode(digest.0),
            expires_at_ms,
        };
        drop(state);
        Ok(reference)
    }

    pub fn get(&self, instance_id: &str, asset_id: &str) -> Result<StoredAsset, AssetError> {
        let mut state = self.inner.lock().map_err(|_| AssetError::Poisoned)?;
        purge_expired(&mut state, Instant::now());
        state
            .records
            .get(asset_id)
            .filter(|record| record.instance_id == instance_id)
            .map(|record| record.asset.clone())
            .ok_or(AssetError::NotFound)
    }

    pub fn remove(&self, instance_id: &str, asset_id: &str) -> Result<(), AssetError> {
        let mut state = self.inner.lock().map_err(|_| AssetError::Poisoned)?;
        if state
            .records
            .get(asset_id)
            .is_some_and(|record| record.instance_id == instance_id)
        {
            remove_record(&mut state, asset_id);
            Ok(())
        } else {
            Err(AssetError::NotFound)
        }
    }

    pub fn remove_instance(&self, instance_id: &str) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let asset_ids = state
            .records
            .iter()
            .filter(|(_, record)| record.instance_id == instance_id)
            .map(|(asset_id, _)| asset_id.clone())
            .collect::<Vec<_>>();
        for asset_id in asset_ids {
            remove_record(&mut state, &asset_id);
        }
    }

    #[cfg(test)]
    fn with_limits(limits: AssetLimits) -> Self {
        Self::build(limits)
    }

    fn build(limits: AssetLimits) -> Self {
        let inner = Arc::new(Mutex::new(AssetState::default()));
        spawn_expiration_sweeper(&inner, limits.ttl);
        Self {
            inner,
            limits: Arc::new(limits),
        }
    }
}

fn validate_insert_request(
    limits: &AssetLimits,
    instance_id: &str,
    mime_type: &str,
    data: &[u8],
) -> Result<usize, AssetError> {
    if data.len() > limits.max_asset_bytes {
        return Err(AssetError::TooLarge);
    }
    if instance_id.is_empty()
        || instance_id.len() > MAX_INSTANCE_ID_BYTES
        || instance_id.chars().any(char::is_control)
    {
        return Err(AssetError::InvalidInstanceId);
    }
    validate_mime_type(mime_type)?;
    Ok(data
        .len()
        .saturating_add(mime_type.len())
        .saturating_add(instance_id.len()))
}

fn spawn_expiration_sweeper(inner: &Arc<Mutex<AssetState>>, ttl: Duration) {
    let inner: Weak<Mutex<AssetState>> = Arc::downgrade(inner);
    let interval = ttl
        .min(Duration::from_secs(1))
        .max(Duration::from_millis(1));
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            let Some(inner) = inner.upgrade() else {
                break;
            };
            {
                let mut state = inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                purge_expired(&mut state, Instant::now());
            }
        }
    });
}

fn validate_mime_type(mime_type: &str) -> Result<(), AssetError> {
    let Some((kind, subtype)) = mime_type.split_once('/') else {
        return Err(AssetError::InvalidMimeType);
    };
    if mime_type.is_empty()
        || mime_type.len() > MAX_MIME_TYPE_BYTES
        || subtype.contains('/')
        || kind.is_empty()
        || subtype.is_empty()
        || !kind.bytes().all(is_mime_token_byte)
        || !subtype.bytes().all(is_mime_token_byte)
    {
        return Err(AssetError::InvalidMimeType);
    }
    Ok(())
}

const fn is_mime_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn has_capacity(
    state: &AssetState,
    limits: &AssetLimits,
    instance_id: &str,
    accounted_bytes: usize,
) -> bool {
    let usage = state
        .instance_usage
        .get(instance_id)
        .copied()
        .unwrap_or_default();
    usage.count < limits.max_instance_assets
        && usage.bytes.saturating_add(accounted_bytes) <= limits.max_instance_bytes
        && state.records.len() < limits.max_store_assets
        && state.total_bytes.saturating_add(accounted_bytes) <= limits.max_store_bytes
}

fn purge_expired(state: &mut AssetState, now: Instant) {
    let expired = state
        .records
        .iter()
        .filter(|(_, record)| record.deadline <= now)
        .map(|(asset_id, _)| asset_id.clone())
        .collect::<Vec<_>>();
    for asset_id in expired {
        remove_record(state, &asset_id);
    }
}

fn remove_record(state: &mut AssetState, asset_id: &str) {
    let Some(record) = state.records.remove(asset_id) else {
        return;
    };
    state.total_bytes = state.total_bytes.saturating_sub(record.accounted_bytes);
    if let Some(usage) = state.instance_usage.get_mut(&record.instance_id) {
        usage.count = usage.count.saturating_sub(1);
        usage.bytes = usage.bytes.saturating_sub(record.accounted_bytes);
        if usage.count == 0 {
            state.instance_usage.remove(&record.instance_id);
        }
    }
}

fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128);
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_scoped_and_consumable() {
        let store = AssetStore::default();
        let reference = store
            .insert("plugin/a", "image/png".to_owned(), b"png".to_vec())
            .unwrap();
        assert_eq!(reference.size_bytes, 3);
        assert!(store.get("plugin/b", &reference.asset_id).is_err());
        assert_eq!(
            store
                .get("plugin/a", &reference.asset_id)
                .unwrap()
                .data
                .as_ref(),
            b"png"
        );
        store.remove("plugin/a", &reference.asset_id).unwrap();
        assert!(store.get("plugin/a", &reference.asset_id).is_err());
    }

    #[test]
    fn oversized_assets_are_rejected() {
        let store = AssetStore::default();
        assert!(matches!(
            store.insert(
                "plugin/a",
                "application/octet-stream".to_owned(),
                vec![0; MAX_ASSET_BYTES + 1]
            ),
            Err(AssetError::TooLarge)
        ));
    }

    #[test]
    fn quotas_expiry_mime_and_instance_purge_are_enforced() {
        let limits = AssetLimits {
            ttl: Duration::ZERO,
            max_asset_bytes: 8,
            max_instance_assets: 1,
            max_instance_bytes: 64,
            max_store_assets: 2,
            max_store_bytes: 128,
        };
        let store = AssetStore::with_limits(limits.clone());
        let expired = store
            .insert("plugin/a", "image/png".to_owned(), vec![1])
            .unwrap();
        assert!(store.get("plugin/a", &expired.asset_id).is_err());

        let store = AssetStore::with_limits(AssetLimits {
            ttl: Duration::from_secs(60),
            ..limits
        });
        assert!(matches!(
            store.insert("plugin/a", "invalid".to_owned(), vec![1]),
            Err(AssetError::InvalidMimeType)
        ));
        assert!(matches!(
            store.insert("", "image/png".to_owned(), vec![1]),
            Err(AssetError::InvalidInstanceId)
        ));
        let first = store
            .insert("plugin/a", "image/png".to_owned(), vec![1])
            .unwrap();
        assert!(matches!(
            store.insert("plugin/a", "image/png".to_owned(), vec![2]),
            Err(AssetError::QuotaExceeded)
        ));
        store.remove_instance("plugin/a");
        assert!(store.get("plugin/a", &first.asset_id).is_err());

        store
            .insert("plugin/a", "image/png".to_owned(), vec![1])
            .unwrap();
        store
            .insert("plugin/b", "image/png".to_owned(), vec![1])
            .unwrap();
        assert!(matches!(
            store.insert("plugin/c", "image/png".to_owned(), vec![1]),
            Err(AssetError::QuotaExceeded)
        ));
    }

    #[tokio::test]
    async fn expiration_sweeper_releases_idle_assets() {
        let store = AssetStore::with_limits(AssetLimits {
            ttl: Duration::from_millis(10),
            max_asset_bytes: 64,
            max_instance_assets: 2,
            max_instance_bytes: 128,
            max_store_assets: 2,
            max_store_bytes: 128,
        });
        let reference = store
            .insert("plugin/a", "image/png".to_owned(), vec![1])
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let state = store.inner.lock().unwrap();
        assert!(!state.records.contains_key(&reference.asset_id));
        assert_eq!(state.total_bytes, 0);
        assert!(!state.instance_usage.contains_key("plugin/a"));
    }
}
