use std::io;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use super::{GroupSignals, within};
#[cfg(unix)]
use super::{pid_of, wait_without_reaping};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Signal, WaitOptions, kill_process, waitpid};
use smol::process::Child;
#[cfg(unix)]
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
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self {
            signals: GroupSignals::new(child.id()),
            child: Some(child),
            signalled: false,
        }
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
        #[cfg(unix)]
        {
            let pid = self.id();
            match within(deadline, smol::unblock(move || wait_without_reaping(pid))).await {
                Some(true) => ChildExit::Exited,
                Some(false) => ChildExit::WaitFailed,
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
        #[cfg(unix)]
        if self.child.is_some() {
            let pid = self.id();
            let _ = smol::unblock(move || wait_without_reaping(pid)).await;
        }
        self.signals.disable();
        let result = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, ALREADY_REAPED))?
            .status()
            .await;
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
        within(deadline, child.status()).await
    }

    pub fn kill(&mut self) {
        if self.can_signal() {
            self.signalled = true;
            #[cfg(unix)]
            self.signals.kill();
            #[cfg(not(unix))]
            if let Some(child) = &mut self.child {
                let _ = child.kill();
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
        #[cfg(unix)]
        if self.child.take().is_some()
            && let Some(pid) = pid_of(self.id())
        {
            let _ = waitpid(Some(pid), WaitOptions::NOHANG);
        }
        #[cfg(not(unix))]
        if let Some(mut child) = self.child.take() {
            let _ = child.try_status();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use test_case::test_case;

    use futures_lite::FutureExt;
    use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use smol::process::Child;

    use rustix::process::{Signal, kill_process, setsid, test_kill_process};

    use super::{ChildGuard, pid_of};

    const BACKGROUND: &str =
        "exec 3<&0; { printf 'ready\n'; IFS= read -r message <&3; printf '%s\n' \"$message\"; } &";
    const READY: &str = "ready\n";
    const PROBE: &[u8] = b"still running\n";
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn normal_exit_leaves_background_jobs_running() {
        smol::block_on(async {
            let work = async {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", BACKGROUND]);
                // SAFETY: setsid is async-signal-safe and takes no borrowed data.
                unsafe {
                    command.pre_exec(|| setsid().map(|_| ()).map_err(io::Error::from));
                }
                let mut child = smol::process::Command::from(command)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut input = child.stdin.take().unwrap();
                let mut output = BufReader::new(child.stdout.take().unwrap());
                let mut guard = ChildGuard::new(child);
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
            work.or(async {
                smol::Timer::after(TEST_TIMEOUT).await;
                panic!("background worker did not answer after the shell exited");
            })
            .await;
        });
    }

    fn spawn_sleep() -> Child {
        let mut std_cmd = std::process::Command::new("sleep");
        std_cmd.arg("60");
        unsafe {
            std_cmd.pre_exec(|| {
                let _ = setsid();
                Ok(())
            });
        }
        let mut cmd: smol::process::Command = std_cmd.into();
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
        let pid = child.id();
        assert!(is_alive(pid));
        drop(ChildGuard::new(child));
        wait_for_death(pid);
    }

    #[test]
    fn kill_and_reap_kills_process() {
        smol::block_on(async {
            let child = spawn_sleep();
            let pid = child.id();
            assert!(is_alive(pid));
            let mut guard = ChildGuard::new(child);
            guard.kill_and_reap().await;
            wait_for_death(pid);
        });
    }

    #[test_case(false ; "kill")]
    #[test_case(true ; "terminate")]
    fn disabled_signal_authority_never_signals_the_leader(graceful: bool) {
        let child = spawn_sleep();
        let pid = pid_of(child.id()).unwrap();
        let mut guard = ChildGuard::new(child);
        guard.signals.disable();
        if graceful {
            guard.terminate();
        } else {
            guard.kill();
        }
        kill_process(pid, Signal::USR1).unwrap();
        let status = smol::block_on(guard.status()).unwrap();
        assert_eq!(status.signal(), Some(Signal::USR1.as_raw()));
    }

    #[test]
    fn reap_deadline_on_a_live_child_is_distinct_from_an_io_error() {
        smol::block_on(async {
            let child = spawn_sleep();
            let pid = pid_of(child.id()).unwrap();
            let mut guard = ChildGuard::new(child);
            assert!(guard.reap(Instant::now()).await.is_none());
            assert!(!guard.can_signal());
            kill_process(pid, Signal::KILL).unwrap();
            wait_for_death(pid.as_raw_pid() as u32);
        });
    }

    #[test]
    fn status_after_reap_returns_error() {
        smol::block_on(async {
            let child = spawn_sleep();
            let mut guard = ChildGuard::new(child);
            guard.kill_and_reap().await;
            assert!(guard.status().await.is_err());
        });
    }
}
