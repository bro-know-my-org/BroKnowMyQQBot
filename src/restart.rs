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

#[derive(Clone)]
pub(crate) struct Controller {
    executable: Option<Arc<PathBuf>>,
    requested: watch::Sender<bool>,
    stop: Arc<Mutex<Option<StopWatch>>>,
    environment: Arc<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

impl Controller {
    pub(crate) fn new() -> Self {
        Self {
            executable: executable().ok().map(Arc::new),
            stop: Arc::default(),
            environment: Arc::new(std::env::vars_os().collect()),
            requested: watch::channel(false).0,
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.executable.is_some()
    }

    pub(crate) fn request(&self, expected: &str) -> Result<(), &'static str> {
        if expected != boot_id() {
            return Err("启动标识已变化，请刷新后确认");
        }
        if !self.available() {
            return Err("当前平台或可执行文件不支持直接重启");
        }
        let mut stop = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stop.is_none() {
            let watch = StopWatch::new();
            if !watch.registered() {
                return Err("无法监听关闭信号，重启未接受");
            }
            *stop = Some(watch);
        }
        self.requested.send_replace(true);
        Ok(())
    }

    pub(crate) async fn cancelled(&self) {
        let mut receiver = self.requested.subscribe();
        let _ = receiver.wait_for(|requested| *requested).await;
    }

    /// Called after all async state, the runtime and logging guards have dropped.
    pub(crate) fn exec(&self) -> std::process::ExitCode {
        // All resources have been released. From here a signal exits immediately,
        // closing the flag-check/exec gap without unsafe signal-mask manipulation.
        let stop = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(watch) = stop.as_ref() {
            watch.finished.store(true, Ordering::SeqCst);
            if watch.stopped.load(Ordering::SeqCst) {
                return std::process::ExitCode::SUCCESS;
            }
        }
        drop(stop);
        #[cfg(target_os = "linux")]
        if let Some(executable) = &self.executable {
            use std::os::unix::process::CommandExt as _;
            let error = std::process::Command::new(executable.as_ref())
                .args(std::env::args_os().skip(1))
                .env_clear()
                .envs(self.environment.iter().cloned())
                .exec();
            eprintln!("BroKnowMyQQBot restart failed: {error}");
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
        assert!(controller.request("stale-boot").is_err());
        assert!(!*controller.requested.borrow());
        if cfg!(target_os = "linux") {
            controller.request(boot_id()).unwrap();
            controller.request(boot_id()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), controller.cancelled())
                .await
                .unwrap();
        }
        assert!(uuid::Uuid::parse_str(boot_id()).is_ok());
    }
}
