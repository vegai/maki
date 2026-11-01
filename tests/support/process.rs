use flume::Receiver;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(300);
const RESULT: &str = "result";

pub fn ndjson(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).unwrap_or_else(|error| {
                panic!(
                    "invalid protocol event: {error}: {}",
                    String::from_utf8_lossy(line)
                )
            })
        })
        .collect()
}

pub struct Process {
    pub child: Child,
    events: Receiver<Value>,
}

impl Process {
    pub fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let (tx, events) = flume::unbounded();
        thread::spawn(move || {
            for line in output.lines() {
                let line = line.expect("cannot read a protocol event from maki");
                let event = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("invalid protocol event: {error}: {line}"));
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self { child, events }
    }

    pub fn send(&mut self, message: Value) {
        let input = self.child.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &message).unwrap();
        writeln!(input).unwrap();
        input.flush().unwrap();
    }

    pub fn until(&self, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if matches(&event) {
                return event;
            }
        }
    }

    pub fn response(&self, id: u64) -> Value {
        let response = self.until(|event| event["id"] == id);
        assert!(response.get("error").is_none(), "{response}");
        response[RESULT].clone()
    }

    pub fn finish(&mut self) {
        self.finish_status(true);
    }

    pub fn finish_status(&mut self, success: bool) {
        self.child.stdin.take();
        let status = self
            .child
            .wait_timeout(PROCESS_TIMEOUT)
            .unwrap()
            .unwrap_or_else(|| panic!("maki did not exit in {PROCESS_TIMEOUT:?}"));
        assert_eq!(status.success(), success, "{status}");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read both output streams concurrently so stderr cannot fill its pipe and block maki.
pub fn finish(command: &mut Command) -> Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let status = child
        .wait_timeout(PROCESS_TIMEOUT)
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("maki did not complete in {PROCESS_TIMEOUT:?}");
        });
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap().into_bytes(),
    }
}
