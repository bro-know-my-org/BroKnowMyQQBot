//! One bounded restart request; only the synchronous entry point can execute it.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::watch;

pub(crate) fn boot_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

/// Child workers must never receive the process-private restart envelope.
pub(crate) fn child_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .env_remove("BKMQB_PRIVATE_RESTART_SESSION")
        .env_remove("BKMQB_INTERNAL_RESTART_LISTEN");
    command
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Restart,
    Stop,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    Stale,
    Unavailable,
    Conflict,
    Signal,
}
impl Rejection {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Stale => "启动标识已变化，请刷新后确认",
            Self::Unavailable => "当前平台或可执行文件不支持直接重启",
            Self::Conflict => "已有其他关闭操作进行中",
            Self::Signal => "无法监听关闭信号，重启未接受",
        }
    }
}

#[derive(Default)]
struct Pending {
    action: Option<Action>,
    stop: Option<StopWatch>,
    session: Option<crate::management::SessionHandoff>,
}

#[derive(Clone)]
pub(crate) struct Controller {
    executable: Option<Arc<PathBuf>>,
    requested: watch::Sender<Option<Action>>,
    state: Arc<Mutex<Pending>>,
    listener: Arc<Mutex<Option<std::net::SocketAddr>>>,
    environment: Arc<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

impl Controller {
    pub(crate) fn new() -> Self {
        Self {
            executable: executable().ok().map(Arc::new),
            state: Arc::default(),
            listener: Arc::default(),
            environment: Arc::new(
                std::env::vars_os()
                    .filter(|(key, _)| {
                        key != "BKMQB_PRIVATE_RESTART_SESSION"
                            && key != "BKMQB_INTERNAL_RESTART_LISTEN"
                    })
                    .collect(),
            ),
            requested: watch::channel(None).0,
        }
    }

    pub(crate) fn record_listener(&self, address: std::net::SocketAddr) {
        *self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(address);
    }

    pub(crate) fn available(&self) -> bool {
        self.executable.is_some()
    }

    pub(crate) fn request(&self, expected: &str, action: Action) -> Result<(), Rejection> {
        self.request_with_session(expected, action, None)
    }

    pub(crate) fn request_with_session(
        &self,
        expected: &str,
        action: Action,
        session: Option<crate::management::SessionHandoff>,
    ) -> Result<(), Rejection> {
        if expected != boot_id() {
            return Err(Rejection::Stale);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = state.action {
            return if previous == action {
                Ok(())
            } else {
                Err(Rejection::Conflict)
            };
        }
        if action == Action::Restart {
            if !self.available() {
                return Err(Rejection::Unavailable);
            }
            let watch = StopWatch::new();
            if !watch.registered() {
                return Err(Rejection::Signal);
            }
            state.stop = Some(watch);
            state.session = session;
        }
        state.action = Some(action);
        self.requested.send_replace(Some(action));
        Ok(())
    }

    pub(crate) async fn cancelled(&self) -> Action {
        let mut receiver = self.requested.subscribe();
        let _ = receiver.wait_for(Option::is_some).await;
        receiver
            .borrow()
            .expect("controller retains the selected shutdown action")
    }

    /// Called after all async state, the runtime and logging guards have dropped.
    pub(crate) fn exec(&self) -> std::process::ExitCode {
        // All resources have been released. From here a signal exits immediately,
        // closing the flag-check/exec gap without unsafe signal-mask manipulation.
        let stop = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(watch) = stop.stop.as_ref() {
            watch.finished.store(true, Ordering::SeqCst);
            if watch.stopped.load(Ordering::SeqCst) {
                return std::process::ExitCode::SUCCESS;
            }
        }
        let handoff = stop
            .session
            .as_ref()
            .and_then(crate::management::SessionHandoff::encode);
        drop(stop);
        if let Some(executable) = &self.executable {
            #[cfg(target_os = "linux")]
            use std::os::unix::process::CommandExt as _;
            let mut command = std::process::Command::new(executable.as_ref());
            command
                .args(std::env::args_os().skip(1))
                .env_clear()
                .envs(self.environment.iter().cloned());
            if let Some(address) = *self
                .listener
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                command.env("BKMQB_INTERNAL_RESTART_LISTEN", address.to_string());
            }
            if let Some(handoff) = handoff {
                command.env("BKMQB_PRIVATE_RESTART_SESSION", handoff);
            }
            #[cfg(target_os = "linux")]
            {
                let error = command.exec();
                eprintln!("BroKnowMyQQBot restart failed: {error}");
            }
        }
        std::process::ExitCode::FAILURE
    }
}

// Independent of Tokio: remains active while logging and the runtime are dropped.
struct StopWatch {
    stopped: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
}
impl StopWatch {
    fn new() -> Self {
        let stopped = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let mut registrations = Vec::new();
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            match signal_hook::flag::register_conditional_shutdown(signal, 0, finished.clone()) {
                Ok(id) => registrations.push(id),
                Err(_) => break,
            }
            match signal_hook::flag::register(signal, stopped.clone()) {
                Ok(id) => registrations.push(id),
                Err(_) => break,
            }
        }
        Self {
            stopped,
            finished,
            registrations,
        }
    }
    fn registered(&self) -> bool {
        self.registrations.len() == 4
    }
}
impl Drop for StopWatch {
    fn drop(&mut self) {
        for id in &self.registrations {
            signal_hook::low_level::unregister(*id);
        }
    }
}

/// Signals win ties; cancellation never drops the drain future.
pub(crate) async fn drain<T>(
    mut signal: std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
    work: impl std::future::Future<Output = T>,
) -> (T, std::io::Result<bool>) {
    tokio::pin!(work);
    tokio::select! {
        biased;
        result = &mut signal => (work.await, result.map(|()| false)),
        result = &mut work => (result, Ok(true)),
    }
}

fn executable() -> std::io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = std::env::current_exe()?;
        let metadata = path.metadata()?;
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Ok(path);
        }
    }
    Err(std::io::Error::other("self exec is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn workers_cannot_inherit_private_restart_environment() {
        const CHILD: &str = "BKMQB_PRIVATE_ENV_TEST";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let output = runtime
                .block_on(async { child_command("/usr/bin/env").output().await })
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(!text.contains("BKMQB_PRIVATE_RESTART_SESSION"));
            assert!(!text.contains("BKMQB_INTERNAL_RESTART_LISTEN"));
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "restart::tests::workers_cannot_inherit_private_restart_environment",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("BKMQB_PRIVATE_RESTART_SESSION", "private-fixture")
            .env("BKMQB_INTERNAL_RESTART_LISTEN", "127.0.0.1:12345")
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn stop_conflict_takes_priority_when_restart_is_unavailable() {
        let mut controller = Controller::new();
        controller.executable = None;
        controller.request(boot_id(), Action::Stop).unwrap();
        controller.request(boot_id(), Action::Stop).unwrap();
        assert_eq!(
            controller.request(boot_id(), Action::Restart),
            Err(Rejection::Conflict)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn simultaneous_stop_restart_selects_only_one_action() {
        let controller = Controller::new();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = [Action::Stop, Action::Restart]
                .into_iter()
                .map(|action| {
                    let controller = &controller;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        (action, controller.request(boot_id(), action).is_ok())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|(_, accepted)| *accepted).count(), 1);
        for (action, accepted) in results {
            assert_eq!(controller.request(boot_id(), action).is_ok(), accepted);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signal_after_final_teardown_cannot_cross_exec_handoff() {
        const CHILD: &str = "BKMQB_RESTART_SIGNAL_TEST";
        if std::env::var_os(CHILD).is_some() {
            let watch = StopWatch::new();
            assert!(watch.registered());
            watch.finished.store(true, Ordering::SeqCst);
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
                .unwrap();
            panic!("final teardown signal must terminate before exec");
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "restart::tests::signal_after_final_teardown_cannot_cross_exec_handoff",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn signal_wins_ties_and_drain_is_still_completed() {
        let signal = async { Ok(()) };
        tokio::pin!(signal);
        let (done, restart) = drain(signal.as_mut(), async { 42 }).await;
        assert_eq!(done, 42);
        assert!(!restart.unwrap());
        let signal = async { Err(std::io::Error::other("signal failed")) };
        tokio::pin!(signal);
        let (done, restart) = drain(signal.as_mut(), async { 42 }).await;
        assert_eq!(done, 42);
        assert!(restart.is_err());
        let signal = std::future::pending();
        tokio::pin!(signal);
        let ((), restart) = drain(signal.as_mut(), async {}).await;
        assert!(restart.unwrap());
    }

    #[tokio::test]
    async fn signal_during_pending_drain_suppresses_restart_without_cancelling_work() {
        let (send, recv) = tokio::sync::oneshot::channel();
        let signal = async {
            recv.await.unwrap();
            Ok(())
        };
        tokio::pin!(signal);
        let (finished, restart) = drain(signal.as_mut(), async {
            send.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            42
        })
        .await;
        assert_eq!(finished, 42);
        assert!(!restart.unwrap());
    }

    #[tokio::test]
    async fn request_is_bounded_deduplicated_and_boot_checked() {
        let controller = Controller::new();
        assert!(controller.request("stale-boot", Action::Restart).is_err());
        assert!(controller.requested.borrow().is_none());
        if cfg!(target_os = "linux") {
            controller.request(boot_id(), Action::Restart).unwrap();
            controller.request(boot_id(), Action::Restart).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), controller.cancelled())
                .await
                .unwrap();
        }
        assert!(uuid::Uuid::parse_str(boot_id()).is_ok());
    }
}
