//! Cooperative shutdown primitives shared by the runtime and adapters.

use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct ShutdownHandle {
    sender: watch::Sender<bool>,
}

#[derive(Debug, Clone)]
pub struct ShutdownSignal {
    receiver: watch::Receiver<bool>,
}

pub fn shutdown_channel() -> (ShutdownHandle, ShutdownSignal) {
    let (sender, receiver) = watch::channel(false);
    (ShutdownHandle { sender }, ShutdownSignal { receiver })
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.sender.send_replace(true);
    }

    pub fn is_shutdown(&self) -> bool {
        *self.sender.borrow()
    }
}

impl ShutdownSignal {
    pub fn is_shutdown(&self) -> bool {
        *self.receiver.borrow() || self.receiver.has_changed().is_err()
    }

    pub async fn cancelled(&mut self) {
        if self.is_shutdown() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if self.is_shutdown() {
                return;
            }
        }
    }
}
