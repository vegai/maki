//! SIGINT and SIGTERM in the modes without a terminal UI. By default a signal
//! kills maki on the spot: the processes its tools started keep running as
//! orphans, and no result is printed. Here the first signal stops the
//! session instead, which stops that work, and maki exits as the signal would
//! have once it is torn down. A second signal exits at once, in case the stop
//! wedges. A signal the parent started maki with ignored stays ignored, as
//! with `maki -p ... &` in a script.

use std::process;
use std::sync::{Arc, OnceLock};

use futures::future::select_all;
use maki_agent::{CancelToken, CancelTrigger};
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
#[cfg(not(unix))]
use tokio::signal::windows::{CtrlC, ctrl_c};
use tracing::{info, warn};

#[cfg(unix)]
const WATCHED: [SignalKind; 2] = [SignalKind::interrupt(), SignalKind::terminate()];
/// Windows has no signal numbers; a Ctrl-C exits as SIGINT does elsewhere.
#[cfg(not(unix))]
const CTRL_C_NUMBER: i32 = 2;
/// A shell reports a process a signal killed as 128 plus the signal number.
const SIGNAL_EXIT_BASE: i32 = 128;
const FORCED_EXIT: &str = "maki: signalled again, exiting without stopping background work";

#[cfg(unix)]
type SignalStream = Signal;
#[cfg(not(unix))]
type SignalStream = CtrlC;

pub struct Interrupt {
    stop: CancelToken,
    _unwatched: Option<CancelTrigger>,
    /// The number of the signal caught first.
    caught: Arc<OnceLock<i32>>,
}

impl Interrupt {
    pub fn install() -> Self {
        Self::with_signals(watched())
    }

    fn with_signals(signals: Vec<(i32, SignalStream)>) -> Self {
        let (trigger, stop) = CancelToken::new();
        let caught = Arc::new(OnceLock::new());
        let unwatched = if signals.is_empty() {
            Some(trigger)
        } else {
            tokio::spawn(watch(signals, trigger, Arc::clone(&caught)));
            None
        };
        Self {
            stop,
            caught,
            _unwatched: unwatched,
        }
    }

    /// Fires on the first signal.
    pub fn stop(&self) -> CancelToken {
        self.stop.clone()
    }

    /// Call once everything is torn down: exits as the caught signal would
    /// have, and returns when none was caught.
    pub fn exit_if_caught(&self) {
        if let Some(&signal) = self.caught.get() {
            maki_otel::shutdown(crate::TELEMETRY_SHUTDOWN_TIMEOUT);
            process::exit(exit_code(signal));
        }
    }
}

/// The signal streams to watch, each with its number for the exit code.
#[cfg(unix)]
fn watched() -> Vec<(i32, SignalStream)> {
    WATCHED
        .into_iter()
        .filter(|&kind| !inherited_ignore(kind))
        .filter_map(|kind| match signal(kind) {
            Ok(stream) => Some((kind.as_raw_value(), stream)),
            Err(error) => {
                warn!(%error, signal = kind.as_raw_value(), "cannot catch the signal, it kills maki at once");
                None
            }
        })
        .collect()
}

#[cfg(not(unix))]
fn watched() -> Vec<(i32, SignalStream)> {
    match ctrl_c() {
        Ok(stream) => vec![(CTRL_C_NUMBER, stream)],
        Err(error) => {
            warn!(%error, "cannot catch Ctrl-C, it kills maki at once");
            Vec::new()
        }
    }
}

async fn watch(
    mut signals: Vec<(i32, SignalStream)>,
    trigger: CancelTrigger,
    caught: Arc<OnceLock<i32>>,
) {
    let Some(first) = next_signal(&mut signals).await else {
        return;
    };
    info!(signal = first, "signal caught, stopping the session");
    let _ = caught.set(first);
    trigger.cancel();
    if let Some(second) = next_signal(&mut signals).await {
        eprintln!("{FORCED_EXIT}");
        process::exit(exit_code(second));
    }
}

/// `None` once a stream closes, which ends the watch.
async fn next_signal(signals: &mut [(i32, SignalStream)]) -> Option<i32> {
    let waits = signals
        .iter_mut()
        .map(|(number, stream)| Box::pin(async move { stream.recv().await.map(|()| *number) }));
    let (caught, _, _) = select_all(waits).await;
    caught
}

/// Non-interactive shells start background jobs with SIGINT ignored, and
/// installing a handler would replace that.
#[cfg(unix)]
fn inherited_ignore(kind: SignalKind) -> bool {
    // SAFETY: a null `act` only reads the current action into `current`.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(kind.as_raw_value(), std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

fn exit_code(signal: i32) -> i32 {
    SIGNAL_EXIT_BASE + signal
}

#[cfg(all(test, unix))]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(SignalKind::interrupt(), 130 ; "sigint")]
    #[test_case(SignalKind::terminate(), 143 ; "sigterm")]
    fn exits_as_a_shell_reports_the_signal(kind: SignalKind, expected: i32) {
        assert_eq!(exit_code(kind.as_raw_value()), expected);
    }

    /// One test, so no other thread flips the disposition between a set and
    /// its read.
    #[test]
    fn reads_whether_the_parent_ignored_the_signal() {
        for (action, ignored) in [(libc::SIG_IGN, true), (libc::SIG_DFL, false)] {
            // SAFETY: SIGUSR1 is not watched or handled anywhere else in maki.
            unsafe { libc::signal(SignalKind::user_defined1().as_raw_value(), action) };

            assert_eq!(inherited_ignore(SignalKind::user_defined1()), ignored);
        }
    }
}

#[cfg(test)]
mod portability_tests {
    use test_case::test_case;

    use super::{Interrupt, watched};

    #[test_case(false ; "all_signals_ignored")]
    #[test_case(true ; "signals_watched")]
    #[tokio::test]
    async fn installing_the_watcher_keeps_the_run_active(watch: bool) {
        let interrupt = Interrupt::with_signals(if watch { watched() } else { Vec::new() });
        assert!(!interrupt.stop().is_cancelled());
    }
}

#[cfg(all(test, unix))]
mod signal_tests {
    use std::env;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use rustix::process::{Pid, Signal, kill_process};
    use test_case::test_case;
    use wait_timeout::ChildExt;

    use super::{Interrupt, SIGNAL_EXIT_BASE};

    const READY: &str = "MAKI_SIGNAL_READY";
    const CANCELLED: &str = "MAKI_SIGNAL_CANCELLED";
    const CHILD_TEST: &str = "interrupt::signal_tests::signal_child";
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "subprocess fixture run by second_signal_forces_exit"]
    async fn signal_child() {
        let interrupt = Interrupt::install();
        let token = interrupt.stop();
        println!("\n{READY}");
        std::io::stdout().flush().unwrap();
        token.cancelled().await;
        println!("{CANCELLED}");
        std::io::stdout().flush().unwrap();
        // Simulate a session write blocking the executor during shutdown.
        loop {
            thread::park();
        }
    }

    #[test_case(Signal::TERM ; "sigterm")]
    #[test_case(Signal::INT ; "sigint")]
    fn second_signal_forces_exit(signal: Signal) {
        let child = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", CHILD_TEST, "--nocapture"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Running(child);
        let output = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        while rx.recv_timeout(TEST_TIMEOUT).unwrap() != READY {}
        let pid = Pid::from_raw(child.0.id().try_into().unwrap()).unwrap();
        kill_process(pid, signal).unwrap();
        while rx.recv_timeout(TEST_TIMEOUT).unwrap() != CANCELLED {}
        assert!(child.0.try_wait().unwrap().is_none());
        kill_process(pid, signal).unwrap();
        let status = child
            .0
            .wait_timeout(TEST_TIMEOUT)
            .unwrap()
            .expect("second signal did not terminate maki");
        assert_eq!(status.code(), Some(SIGNAL_EXIT_BASE + signal.as_raw()));
    }
}
