//! One initiator only. No cookie/token plaintext, files or browser storage.
use super::{AuthState, SESSION_TTL, session_digest};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq as _;

pub(crate) const ENV: &str = "BKMQB_PRIVATE_RESTART_SESSION";
const WINDOW_MS: u64 = 90_000;
const MAX_BYTES: usize = 512;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    process_id: u32,
    session: String,
    token: String,
    consume_before_ms: u64,
    expires_at_ms: u64,
}

pub(crate) struct SessionHandoff {
    auth: AuthState,
    digest: [u8; 32],
    payload: Payload,
}
impl SessionHandoff {
    pub(crate) fn encode(&self) -> Option<String> {
        // Logout, replacement login and expiration during drain still revoke it.
        let expires = self.auth.sessions.lock().ok()?.get(&self.digest).copied()?;
        if expires <= Instant::now() || self.payload.consume_before_ms <= now_ms()? {
            return None;
        }
        serde_json::to_string(&self.payload).ok()
    }
}
fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

impl AuthState {
    pub(in crate::management) fn restart_session(
        &self,
        headers: &HeaderMap,
    ) -> Option<SessionHandoff> {
        let expected = self.expected?;
        if self.bearer_valid(headers) {
            return None;
        }
        let digest = session_digest(headers)?;
        let expires = self.sessions.lock().ok()?.get(&digest).copied()?;
        let remaining = expires.checked_duration_since(Instant::now())?;
        let wall_now = now_ms()?;
        let expires_at_ms = wall_now.checked_add(remaining.as_millis().try_into().ok()?)?;
        Some(SessionHandoff {
            auth: self.clone(),
            digest,
            payload: Payload {
                process_id: std::process::id(),
                session: hex::encode(digest),
                token: hex::encode(expected),
                consume_before_ms: wall_now.checked_add(WINDOW_MS)?.min(expires_at_ms),
                expires_at_ms,
            },
        })
    }
    pub(in crate::management) fn resume_restart(&self) {
        static CONSUMED: AtomicBool = AtomicBool::new(false);
        if CONSUMED.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(value) = std::env::var(ENV) {
            self.accept_handoff(&value);
        }
    }
    fn accept_handoff(&self, value: &str) {
        let Some(expected) = self.expected else {
            return;
        };
        if value.len() > MAX_BYTES {
            return;
        }
        let Ok(payload) = serde_json::from_str::<Payload>(value) else {
            return;
        };
        if payload.process_id != std::process::id() {
            return;
        }
        let Some(now) = now_ms() else {
            return;
        };
        let Some(window) = payload.consume_before_ms.checked_sub(now) else {
            return;
        };
        let Some(remaining) = payload.expires_at_ms.checked_sub(now) else {
            return;
        };
        if window == 0
            || window > WINDOW_MS
            || window > remaining
            || remaining > SESSION_TTL.as_millis().try_into().unwrap_or(0)
        {
            return;
        }
        let Ok(token) = hex::decode(&payload.token) else {
            return;
        };
        let Ok(session): Result<[u8; 32], _> = hex::decode(&payload.session).and_then(|bytes| {
            bytes
                .try_into()
                .map_err(|_| hex::FromHexError::InvalidStringLength)
        }) else {
            return;
        };
        if !bool::from(expected.as_slice().ct_eq(&token)) {
            return;
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(session, Instant::now() + Duration::from_millis(remaining));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::COOKIE;
    use super::*;
    use axum::http::header;
    use sha2::{Digest as _, Sha256};
    fn fixture() -> (AuthState, HeaderMap, String) {
        let auth: AuthState = [1; 32].into();
        let id = uuid::Uuid::new_v4().to_string();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("{COOKIE}={id}").parse().unwrap());
        auth.sessions.lock().unwrap().insert(
            Sha256::digest(id.as_bytes()).into(),
            Instant::now() + SESSION_TTL,
        );
        (auth, headers, id)
    }
    #[test]
    fn handoff_restores_only_initiator_and_is_bound_to_token_and_revocation() {
        let (auth, headers, id) = fixture();
        let handoff = auth.restart_session(&headers).unwrap();
        let encoded = handoff.encode().unwrap();
        assert!(!encoded.contains(&id));
        let restored: AuthState = [1; 32].into();
        restored.accept_handoff(&encoded);
        assert!(restored.authenticated(&headers));
        let ordinary: AuthState = [1; 32].into();
        assert!(!ordinary.authenticated(&headers));
        let rotated: AuthState = [2; 32].into();
        rotated.accept_handoff(&encoded);
        assert!(!rotated.authenticated(&headers));
        auth.sessions.lock().unwrap().clear();
        assert!(handoff.encode().is_none());
    }
    #[test]
    fn expired_malformed_and_unbounded_handoffs_are_rejected() {
        let (auth, headers, _) = fixture();
        let encoded = auth.restart_session(&headers).unwrap().encode().unwrap();
        for (key, value) in [
            ("consume_before_ms", 0),
            ("consume_before_ms", now_ms().unwrap() + WINDOW_MS * 2),
            ("expires_at_ms", u64::MAX),
        ] {
            let mut payload: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            payload[key] = value.into();
            let restored: AuthState = [1; 32].into();
            restored.accept_handoff(&payload.to_string());
            assert!(!restored.authenticated(&headers));
        }
        let restored: AuthState = [1; 32].into();
        for invalid in ["x".repeat(MAX_BYTES + 1), "{}".to_owned()] {
            restored.accept_handoff(&invalid);
        }
        assert!(!restored.authenticated(&headers));
        let digest = session_digest(&headers).unwrap();
        auth.sessions.lock().unwrap().insert(
            digest,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        );
        assert!(auth.restart_session(&headers).is_none());
    }
}
