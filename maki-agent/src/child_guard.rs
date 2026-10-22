use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use maki_providers::process::GroupSignals;
#[cfg(unix)]
use maki_providers::process::{pid_of, wait_without_reaping};
#[cfg(unix)]
use rustix::process::{WaitOptions, waitpid};
use smol::process::Child;

const REAP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ChildGuard {
    signals: GroupSignals,
    child: Option<Child>,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self {
            signals: GroupSignals::new(child.id()),
            child: Some(child),
        }
    }

    pub fn id(&self) -> u32 {
        self.signals.id()
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        match self.child.as_mut() {
            Some(child) => {
                #[cfg(unix)]
                {
                    let pid = child.id();
                    let _ = smol::unblock(move || wait_without_reaping(pid)).await;
                }
                self.signals.disable();
                let result = child.status().await;
                if result.is_ok() {
                    self.child = None;
                }
                result
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child already reaped",
            )),
        }
    }

    pub async fn kill_and_reap(&mut self) {
        self.signal_kill();
        if let Some(mut child) = self.child.take() {
            self.signals.disable();
            futures_lite::future::or(
                async {
                    let _ = child.status().await;
                },
                async {
                    smol::Timer::after(REAP_TIMEOUT).await;
                },
            )
            .await;
        }
    }

    #[cfg(unix)]
    fn signal_kill(&self) {
        if self.child.is_some() {
            self.signals.kill();
        }
    }

    #[cfg(not(unix))]
    fn signal_kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }

    // Best-effort non-blocking reap after SIGKILL to prevent zombie accumulation.
    // All normal paths call .status() or .kill_and_reap() first, so this only
    // fires on unexpected drops (panics, early returns). Uses WNOHANG to never
    // block the async executor.
    #[cfg(unix)]
    fn reap_nonblocking(&mut self) {
        self.signals.disable();
        if self.child.take().is_some()
            && let Some(pid) = pid_of(self.signals.id())
        {
            let _ = waitpid(Some(pid), WaitOptions::NOHANG);
        }
    }

    #[cfg(not(unix))]
    fn reap_nonblocking(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.try_status();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.signal_kill();
        }
        self.reap_nonblocking();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use futures_lite::FutureExt;
    use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use smol::process::Child;

    use rustix::process::{setsid, test_kill_process};

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
