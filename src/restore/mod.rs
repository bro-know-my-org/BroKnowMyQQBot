//! Read-only plans and drained, recoverable logical restoration.
pub(crate) mod files;
pub(crate) mod offline;
pub(crate) mod plan;
use crate::{backup::Repository, plugin_management::PluginManager};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
type Expiry = tokio::sync::watch::Sender<(String, Instant)>;

pub(crate) struct Ticket {
    pub prepared: plan::Prepared,
    pub expires: Instant,
}
#[derive(Clone)]
pub(crate) struct State {
    pub manager: PluginManager,
    pub repository: Option<Repository>,
    pub tickets: Arc<Mutex<Option<Ticket>>>,
    pub slots: Arc<Semaphore>,
    pub shutdown: Option<bot_core::ShutdownSignal>,
    expiry: Arc<Mutex<Option<Expiry>>>,
    pub ttl: Duration,
    pub controller: crate::restart::Controller,
}
impl State {
    pub(crate) fn new(manager: PluginManager, shutdown: Option<bot_core::ShutdownSignal>) -> Self {
        Self {
            repository: manager.backup_repository().ok(),
            manager,
            tickets: Arc::default(),
            slots: Arc::new(Semaphore::new(1)),
            shutdown,
            expiry: Arc::default(),
            ttl: Duration::from_secs(900),
            controller: crate::restart::Controller::new(),
        }
    }
    pub(crate) fn inspection(&self, id: &str) -> crate::backup::Result<plan::Inspection> {
        let mut ticket = self.tickets.lock().map_err(crate::backup::invalid)?;
        if ticket.as_ref().is_some_and(|t| t.expires <= Instant::now()) {
            *ticket = None;
        }
        ticket
            .as_ref()
            .filter(|t| t.prepared.inspection.operation_id == id)
            .map(|t| {
                let mut result = t.prepared.inspection.clone();
                result.expires_in_seconds = t
                    .expires
                    .saturating_duration_since(Instant::now())
                    .as_secs();
                result
            })
            .ok_or(crate::plugin_management::ManagementError::ExpiredSelection)
    }
    pub(crate) fn retain(
        &self,
        prepared: plan::Prepared,
    ) -> crate::backup::Result<plan::Inspection> {
        let shutdown = self
            .shutdown
            .clone()
            .filter(|s| !s.is_shutdown())
            .ok_or(crate::plugin_management::ManagementError::Busy)?;
        let mut expiry = self.expiry.lock().map_err(crate::backup::invalid)?;
        let mut ticket = self.tickets.lock().map_err(crate::backup::invalid)?;
        let response = prepared.inspection.clone();
        let deadline = Instant::now() + self.ttl;
        let key = (response.operation_id.clone(), deadline);
        if let Some(sender) = expiry.as_ref() {
            if sender.is_closed() {
                return Err(crate::plugin_management::ManagementError::Busy);
            }
            sender.send_replace(key);
        } else {
            let (sender, receiver) = tokio::sync::watch::channel(key);
            self.manager.supervise_backup(expire(
                receiver,
                Arc::downgrade(&self.tickets),
                shutdown,
            ))?;
            *expiry = Some(sender);
        }
        *ticket = Some(Ticket {
            prepared,
            expires: deadline,
        });
        Ok(response)
    }
}
async fn expire(
    mut changes: tokio::sync::watch::Receiver<(String, Instant)>,
    tickets: std::sync::Weak<Mutex<Option<Ticket>>>,
    mut shutdown: bot_core::ShutdownSignal,
) {
    loop {
        let (id, deadline) = changes.borrow_and_update().clone();
        tokio::select! {biased;
            ()=shutdown.cancelled()=>break,
            result=changes.changed()=>{if result.is_err(){break;} continue;},
            ()=tokio::time::sleep_until(deadline.into())=>()
        }
        if let Some(tickets) = tickets.upgrade() {
            let Ok(mut ticket) = tickets.lock() else {
                tracing::warn!("restore ticket expiry failed");
                return;
            };
            if ticket
                .as_ref()
                .is_some_and(|t| t.prepared.inspection.operation_id == id)
            {
                *ticket = None;
            }
        }
        tokio::select! {biased; ()=shutdown.cancelled()=>break,result=changes.changed()=>{if result.is_err(){break;}}}
    }
    if let Some(tickets) = tickets.upgrade() {
        if let Ok(mut ticket) = tickets.lock() {
            *ticket = None;
        } else {
            tracing::warn!("restore ticket shutdown cleanup failed");
        }
    }
}
pub(crate) fn declaration_revision(
    path: Option<&std::path::Path>,
) -> crate::backup::Result<Option<String>> {
    path.map(|p| {
        crate::backup::read(p, 1024 * 1024).map(|bytes| crate::config::config_revision(&bytes))
    })
    .transpose()
}
