//! Process-local ownership of QQ wire tokens and one-shot interaction attempts.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};

use bot_core::{AdapterId, ButtonInteraction, Context};
use plugin_api::{PluginError, QqButton, QqButtonAccess, QqChat, QqKeyboard};
use serde_json::Value;
use tokio::{
    sync::watch,
    time::{Instant, timeout_at},
};
use uuid::Uuid;

use super::{RegisteredPlugin, now_ms, qq_keyboard};

const MAX_BUTTONS: usize = 4096;
const MAX_BUTTONS_PER_INSTANCE: usize = 256;
const MAX_INTERACTIONS: usize = 4096;
const BUTTON_TTL: Duration = Duration::from_secs(600);
const ACK_TTL_MS: i64 = 5_000;

#[derive(Clone, Default)]
pub(super) struct QqCallbacks(Arc<Mutex<Registry>>);

#[derive(Default)]
struct Registry {
    buttons: HashMap<String, IssuedButton>,
    clicks: HashMap<(AdapterId, String), Click>,
}

#[derive(Clone)]
pub(super) struct IssuedButton {
    pub owner: Weak<RegisteredPlugin>,
    pub generation: u64,
    pub data: String,
    adapter: AdapterId,
    target: QqChat,
    button_id: String,
    access: QqButtonAccess,
    publication: watch::Receiver<Option<Instant>>,
}

struct Click {
    expires: Instant,
    attempted: bool,
}

pub(super) struct PendingButtons {
    callbacks: QqCallbacks,
    tokens: Vec<String>,
    publication: watch::Sender<Option<Instant>>,
}

impl QqCallbacks {
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn prepare_keyboard(
        &self,
        keyboard: QqKeyboard,
        owner: &Arc<RegisteredPlugin>,
        generation: u64,
        adapter: &AdapterId,
        target: &QqChat,
    ) -> Result<(Value, Option<PendingButtons>), PluginError> {
        let count = qq_keyboard::callback_count(&keyboard);
        if count == 0 {
            return Ok((
                qq_keyboard::keyboard(keyboard, |_, _| unreachable!("no callback buttons")),
                None,
            ));
        }
        let (publication, published) = watch::channel(None);
        let mut tokens = Vec::with_capacity(count);
        let mut registry = self.lock();
        let now = Instant::now();
        let owner = Arc::downgrade(owner);
        registry.buttons.retain(|_, button| {
            button.owner.strong_count() > 0
                && (!button.owner.ptr_eq(&owner) || button.generation == generation)
                && button
                    .publication
                    .borrow()
                    .is_none_or(|expiry| expiry > now)
        });
        let instance_count = registry
            .buttons
            .values()
            .filter(|button| button.owner.ptr_eq(&owner))
            .count();
        if registry.buttons.len() + count > MAX_BUTTONS
            || instance_count + count > MAX_BUTTONS_PER_INSTANCE
        {
            return Err(PluginError::ResourceExhausted(
                "QQ callback button capacity exhausted".to_owned(),
            ));
        }
        let wire = qq_keyboard::keyboard(keyboard, |button: &QqButton, data| {
            let token = loop {
                let candidate = format!(
                    "{}{}",
                    ButtonInteraction::MANAGED_TOKEN_PREFIX,
                    Uuid::new_v4().simple()
                );
                if !registry.buttons.contains_key(&candidate) {
                    break candidate;
                }
            };
            registry.buttons.insert(
                token.clone(),
                IssuedButton {
                    owner: owner.clone(),
                    generation,
                    data: data.to_owned(),
                    adapter: adapter.clone(),
                    target: target.clone(),
                    button_id: button.id.clone(),
                    access: button.access.clone(),
                    publication: published.clone(),
                },
            );
            tokens.push(token.clone());
            token
        });
        Ok((
            wire,
            Some(PendingButtons {
                callbacks: self.clone(),
                tokens,
                publication,
            }),
        ))
    }

    pub(super) fn owner(
        &self,
        context: &Context,
        button: &ButtonInteraction,
        target: &QqChat,
    ) -> Option<IssuedButton> {
        let registry = self.lock();
        let issued = registry.buttons.get(&button.token)?;
        if issued.adapter != *context.adapter_id()
            || issued.target != *target
            || issued.button_id != button.button_id
            || issued
                .publication
                .borrow()
                .is_some_and(|expiry| expiry <= Instant::now())
            || matches!(&issued.access, QqButtonAccess::Users { ids } if !ids.contains(&button.actor_id))
        {
            return None;
        }
        Some(issued.clone())
    }

    pub(super) fn admit(
        &self,
        context: &Context,
        button: &ButtonInteraction,
        deadline: Instant,
    ) -> bool {
        let now = Instant::now();
        if deadline <= now {
            return false;
        }
        let mut registry = self.lock();
        // Keep expired IDs for the whole maximum validity window, including permitted clock skew.
        registry
            .clicks
            .retain(|_, click| click.expires + Duration::from_secs(1) > now);
        if registry.clicks.len() >= MAX_INTERACTIONS {
            return false;
        }
        let key = (context.adapter_id().clone(), button.interaction_id.clone());
        if let std::collections::hash_map::Entry::Vacant(entry) = registry.clicks.entry(key) {
            entry.insert(Click {
                expires: deadline,
                attempted: false,
            });
            true
        } else {
            false
        }
    }

    pub(super) fn deadline(&self, context: &Context) -> Option<Instant> {
        let button = context.button_interaction()?;
        self.lock()
            .clicks
            .get(&(context.adapter_id().clone(), button.interaction_id.clone()))
            .map(|click| click.expires)
    }

    pub(super) fn take_ack(
        &self,
        context: &Context,
        button: &ButtonInteraction,
    ) -> Option<Instant> {
        let mut registry = self.lock();
        let click = registry
            .clicks
            .get_mut(&(context.adapter_id().clone(), button.interaction_id.clone()))?;
        if click.attempted || click.expires <= Instant::now() {
            return None;
        }
        click.attempted = true;
        Some(click.expires)
    }
}

pub(super) fn interaction_deadline(context: &Context) -> Option<Instant> {
    let occurred = context.occurred_at_ms()?;
    let age = now_ms().saturating_sub(occurred);
    if !(-1_000..ACK_TTL_MS).contains(&age) {
        return None;
    }
    let remaining_ms = u64::try_from(ACK_TTL_MS - age.max(0)).ok()?;
    Some(Instant::now() + Duration::from_millis(remaining_ms))
}

impl IssuedButton {
    pub(super) async fn wait_until_published(&mut self, deadline: Instant) -> Option<Instant> {
        let published = timeout_at(deadline, self.publication.wait_for(Option::is_some))
            .await
            .ok()?
            .ok()?;
        *published
    }
}

impl PendingButtons {
    pub(super) fn publish(mut self) {
        self.publication
            .send_replace(Some(Instant::now() + BUTTON_TTL));
        self.tokens.clear();
    }
}

impl Drop for PendingButtons {
    fn drop(&mut self) {
        if !self.tokens.is_empty() {
            let mut registry = self.callbacks.lock();
            for token in &self.tokens {
                registry.buttons.remove(token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures_util::poll;

    use super::*;

    fn pending() -> (QqCallbacks, PendingButtons, IssuedButton) {
        let callbacks = QqCallbacks::default();
        let (publication, receiver) = watch::channel(None);
        let issued = IssuedButton {
            owner: Weak::new(),
            generation: 0,
            data: String::new(),
            adapter: AdapterId::new("qq"),
            target: QqChat::Private {
                user_id: "reader".to_owned(),
            },
            button_id: "next".to_owned(),
            access: QqButtonAccess::Everyone {},
            publication: receiver,
        };
        let token = "pending".to_owned();
        callbacks
            .lock()
            .buttons
            .insert(token.clone(), issued.clone());
        let pending = PendingButtons {
            callbacks: callbacks.clone(),
            tokens: vec![token],
            publication,
        };
        (callbacks, pending, issued)
    }

    #[tokio::test(start_paused = true)]
    async fn publication_wakes_a_callback_that_has_started_waiting() {
        let (callbacks, pending, mut issued) = pending();
        let expires = Instant::now() + BUTTON_TTL;
        let mut waiting =
            pin!(issued.wait_until_published(Instant::now() + Duration::from_secs(5)));
        assert!(poll!(waiting.as_mut()).is_pending());
        pending.publish();
        assert_eq!(waiting.await, Some(expires));
        assert!(callbacks.lock().buttons.contains_key("pending"));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wakes_waiters_and_releases_the_button_reservation() {
        let (callbacks, pending, mut issued) = pending();
        let mut waiting =
            pin!(issued.wait_until_published(Instant::now() + Duration::from_secs(5)));
        assert!(poll!(waiting.as_mut()).is_pending());
        drop(pending);
        assert_eq!(waiting.await, None);
        assert!(!callbacks.lock().buttons.contains_key("pending"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_click_stops_waiting_at_its_own_deadline() {
        let (_, pending, mut issued) = pending();
        let mut waiting =
            pin!(issued.wait_until_published(Instant::now() + Duration::from_secs(1)));
        assert!(poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(waiting.await, None);
        pending.publish();
    }
}
