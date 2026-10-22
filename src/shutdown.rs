use std::io;
#[cfg(unix)]
use std::process;
use std::thread;

#[cfg(unix)]
use async_signal::{Signal, Signals};
use maki_agent::CancelTrigger;
#[cfg(unix)]
use smol::stream::StreamExt;

#[cfg(unix)]
const SIGNAL_EXIT_OFFSET: i32 = 128;

pub fn watch(cancel: CancelTrigger) -> io::Result<()> {
    #[cfg(unix)]
    let mut signals = Signals::new([Signal::Int, Signal::Term])?;
    thread::Builder::new()
        .name("shutdown-signals".into())
        .spawn(move || {
            #[cfg(unix)]
            smol::block_on(async move {
                let _ = signals.next().await;
                cancel.cancel();
                if let Some(Ok(signal)) = signals.next().await {
                    process::exit(SIGNAL_EXIT_OFFSET + signal as i32);
                }
            });
            #[cfg(not(unix))]
            {
                let _cancel = cancel;
                loop {
                    thread::park();
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod portability_tests {
    use maki_agent::CancelToken;

    use super::watch;

    #[test]
    fn installing_the_watcher_keeps_the_run_active() {
        let (trigger, token) = CancelToken::new();
        watch(trigger).unwrap();
        assert!(!token.is_cancelled());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::env;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use maki_agent::CancelToken;
    use rustix::process::{Pid, Signal, kill_process};
    use test_case::test_case;
    use wait_timeout::ChildExt;

    use super::{SIGNAL_EXIT_OFFSET, watch};

    const READY: &str = "MAKI_SIGNAL_READY";
    const CANCELLED: &str = "MAKI_SIGNAL_CANCELLED";
    const CHILD_TEST: &str = "shutdown::tests::signal_child";
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "subprocess fixture run by second_signal_forces_exit"]
    fn signal_child() {
        let (trigger, token) = CancelToken::new();
        watch(trigger).unwrap();
        println!("\n{READY}");
        std::io::stdout().flush().unwrap();
        smol::block_on(smol::spawn(async move {
            token.cancelled().await;
            println!("{CANCELLED}");
            std::io::stdout().flush().unwrap();
            // Simulate a session write blocking the executor during shutdown.
            loop {
                thread::park();
            }
        }));
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
        let (tx, rx) = flume::unbounded();
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
        assert_eq!(status.code(), Some(SIGNAL_EXIT_OFFSET + signal.as_raw()));
    }
}
