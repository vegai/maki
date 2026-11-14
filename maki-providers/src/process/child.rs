use std::io;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use super::{GroupSignals, within};
#[cfg(unix)]
use super::{pid_of, wait_without_reaping};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Signal, kill_process};
use tokio::process::Child;
use tokio::runtime::Handle;
use tracing::warn;

const REAP_TIMEOUT: Duration = Duration::from_secs(5);
const ALREADY_REAPED: &str = "child already reaped";

pub enum ChildExit {
    Exited,
    WaitFailed,
    TimedOut,
}

pub struct ChildGuard {
    signals: GroupSignals,
    child: Option<Child>,
    signalled: bool,
    runtime: Handle,
}

impl ChildGuard {
    pub fn new(child: Child) -> io::Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, ALREADY_REAPED))?;
        let runtime = Handle::try_current().map_err(io::Error::other)?;
        Ok(Self {
            runtime,
            signals: GroupSignals::new(pid),
            child: Some(child),
            signalled: false,
        })
    }
    pub fn id(&self) -> u32 {
        self.signals.id()
    }
    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }
    pub fn can_signal(&self) -> bool {
        self.signals.is_enabled() && self.child.is_some()
    }
    pub fn was_signalled(&self) -> bool {
        self.signalled
    }

    pub async fn exited_by(&self, deadline: Instant) -> ChildExit {
        if !self.can_signal() {
            return ChildExit::WaitFailed;
        }
        #[cfg(unix)]
        {
            let pid = self.id();
            match within(
                deadline,
                tokio::task::spawn_blocking(move || wait_without_reaping(pid)),
            )
            .await
            {
                Some(Ok(true)) => ChildExit::Exited,
                Some(Ok(false)) | Some(Err(_)) => ChildExit::WaitFailed,
                None => ChildExit::TimedOut,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = deadline;
            ChildExit::WaitFailed
        }
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        let result = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, ALREADY_REAPED))?
            .wait()
            .await;
        self.signals.disable();
        if result.is_ok() {
            self.child.take();
        }
        result
    }

    pub async fn reap(&mut self, deadline: Instant) -> Option<io::Result<ExitStatus>> {
        self.signals.disable();
        let Some(mut child) = self.child.take() else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                ALREADY_REAPED,
            )));
        };
        let result = within(deadline, child.wait()).await;
        if result.is_none() {
            self.reap_later(child);
        }
        result
    }

    // Keep a waiter active: an idle runtime may not drain Tokio's orphan queue promptly.
    fn reap_later(&self, mut child: Child) {
        let pid = self.id();
        self.runtime.spawn(async move {
            if let Err(error) = child.wait().await {
                warn!(pid, %error, "failed to reap a dropped child");
            }
        });
    }

    pub fn kill(&mut self) {
        if self.can_signal() {
            self.signalled = true;
            #[cfg(unix)]
            self.signals.kill();
            #[cfg(not(unix))]
            if let Some(child) = &mut self.child {
                let _ = child.start_kill();
            }
        }
    }

    #[cfg(unix)]
    pub fn terminate(&mut self) {
        if self.can_signal() {
            self.signalled = true;
            if let Some(pid) = pid_of(self.id())
                && let Err(error) = kill_process(pid, Signal::TERM)
                && error != Errno::SRCH
            {
                warn!(pid = self.id(), signal = ?Signal::TERM, %error, "failed to terminate child");
            }
        }
    }

    pub async fn kill_and_reap(&mut self) {
        self.kill();
        let _ = self.reap(Instant::now() + REAP_TIMEOUT).await;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
        self.signals.disable();
        if let Some(child) = self.child.take() {
            self.reap_later(child);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use futures::FutureExt;
    use std::io;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use test_case::test_case;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Child;

    use rustix::process::{
        Signal, WaitId, WaitIdOptions, kill_process, setsid, test_kill_process, waitid,
    };

    use super::{ChildGuard, pid_of};

    const BACKGROUND: &str =
        "exec 3<&0; { printf 'ready\n'; IFS= read -r message <&3; printf '%s\n' \"$message\"; } &";
    const READY: &str = "ready\n";
    const PROBE: &[u8] = b"still running\n";
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn normal_exit_leaves_background_jobs_running() {
        crate::test_support::runtime().block_on(async {
            let work = async {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", BACKGROUND]);
                // SAFETY: setsid is async-signal-safe and takes no borrowed data.
                unsafe {
                    command.pre_exec(|| setsid().map(|_| ()).map_err(io::Error::from));
                }
                let mut child = tokio::process::Command::from(command)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut input = child.stdin.take().unwrap();
                let mut output = BufReader::new(child.stdout.take().unwrap());
                let mut guard = child_guard(child);
                let mut line = String::new();
                output.read_line(&mut line).await.unwrap();
                assert_eq!(line, READY);
                assert!(guard.status().await.unwrap().success());
                drop(guard);
                input.write_all(PROBE).await.unwrap();
                line.clear();
                output.read_line(&mut line).await.unwrap();
                assert_eq!(line.as_bytes(), PROBE);
            };
            async {
                tokio::select! { biased; value = work => value, value = async {
                    tokio::time::sleep(TEST_TIMEOUT).await;
                    panic!("background worker did not answer after the shell exited");
                } => value, }
            }
            .await;
        });
    }

    fn child_guard(child: Child) -> ChildGuard {
        let _runtime = crate::test_support::runtime().enter();
        ChildGuard::new(child).unwrap()
    }

    fn spawn_sleep() -> Child {
        let _runtime = crate::test_support::runtime().enter();
        let mut std_cmd = std::process::Command::new("sleep");
        std_cmd.arg("60");
        unsafe {
            std_cmd.pre_exec(|| {
                let _ = setsid();
                Ok(())
            });
        }
        let mut cmd: tokio::process::Command = std_cmd.into();
        cmd.spawn().expect("failed to spawn sleep")
    }

    fn is_alive(pid: u32) -> bool {
        pid_of(pid).is_some_and(|pid| test_kill_process(pid).is_ok())
    }

    fn wait_for_death(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process {pid} still alive after 2s");
    }

    #[test]
    fn drop_kills_child_process() {
        let child = spawn_sleep();
        let pid = child.id().unwrap();
        assert!(is_alive(pid));
        drop(child_guard(child));
        wait_for_death(pid);
    }

    #[test]
    fn kill_and_reap_kills_process() {
        crate::test_support::runtime().block_on(async {
            let child = spawn_sleep();
            let pid = child.id().unwrap();
            assert!(is_alive(pid));
            let mut guard = child_guard(child);
            guard.kill_and_reap().await;
            wait_for_death(pid);
        });
    }

    #[test_case(false ; "kill")]
    #[test_case(true ; "terminate")]
    fn disabled_signal_authority_never_signals_the_leader(graceful: bool) {
        let child = spawn_sleep();
        let pid = pid_of(child.id().unwrap()).unwrap();
        let mut guard = child_guard(child);
        guard.signals.disable();
        if graceful {
            guard.terminate();
        } else {
            guard.kill();
        }
        kill_process(pid, Signal::USR1).unwrap();
        let status = crate::test_support::runtime()
            .block_on(guard.status())
            .unwrap();
        assert_eq!(status.signal(), Some(Signal::USR1.as_raw()));
    }

    #[test]
    fn reap_deadline_on_a_live_child_is_distinct_from_an_io_error() {
        crate::test_support::runtime().block_on(async {
            let child = spawn_sleep();
            let pid = pid_of(child.id().unwrap()).unwrap();
            let mut guard = child_guard(child);
            assert!(guard.reap(Instant::now()).await.is_none());
            assert!(!guard.can_signal());
            kill_process(pid, Signal::KILL).unwrap();
            wait_for_death(pid.as_raw_pid() as u32);
        });
    }

    #[test]
    fn cancelling_status_keeps_child_killable() {
        crate::test_support::runtime().block_on(async {
            let mut guard = child_guard(spawn_sleep());
            assert!(guard.status().now_or_never().is_none());
            assert!(guard.can_signal());
            guard.kill();
            let status = guard
                .reap(Instant::now() + TEST_TIMEOUT)
                .await
                .unwrap()
                .unwrap();
            assert!(!status.success());
            assert!(guard.was_signalled());
            assert!(!guard.can_signal());
        });
    }

    #[test]
    fn status_after_reap_returns_error() {
        crate::test_support::runtime().block_on(async {
            let child = spawn_sleep();
            let mut guard = child_guard(child);
            guard.kill_and_reap().await;
            assert!(guard.status().await.is_err());
        });
    }

    #[test]
    fn a_failed_wait_disables_signalling() {
        crate::test_support::runtime().block_on(async {
            let mut guard = child_guard(spawn_sleep());
            let pid = pid_of(guard.id()).unwrap();
            kill_process(pid, Signal::KILL).unwrap();
            waitid(WaitId::Pid(pid), WaitIdOptions::EXITED).unwrap();
            let result = guard.status().await;
            let can_signal = guard.can_signal();
            guard.signals.disable();
            assert!(result.is_err());
            assert!(!can_signal);
        });
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::time::{Duration, Instant};

    use futures::FutureExt;
    use tokio::process::Command;

    use super::ChildGuard;

    const SHELL: &str = "powershell.exe";
    const WAIT: &str = "Start-Sleep -Seconds 300";
    const REAP: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn cancelling_status_keeps_child_killable() {
        let child = Command::new(SHELL)
            .args(["-NoProfile", "-Command", WAIT])
            .spawn()
            .unwrap();
        let mut guard = ChildGuard::new(child).unwrap();
        assert!(guard.status().now_or_never().is_none());
        let can_signal = guard.can_signal();
        guard.kill();
        if !can_signal {
            guard.child_mut().unwrap().start_kill().unwrap();
        }
        let status = guard.reap(Instant::now() + REAP).await.unwrap().unwrap();
        assert!(!status.success());
        assert!(can_signal);
        assert!(guard.was_signalled());
    }
}
