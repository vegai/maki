use super::super::MAX_EVENT_BYTES;
use super::super::checks::HANDSHAKE;
use super::super::error::Error;
use super::super::mcp::Handoff;
use super::super::stream::control_request;
#[cfg(target_os = "linux")]
use crate::process::guard;
use crate::process::kill_group;
#[cfg(unix)]
use crate::process::{pid_of, wait_without_reaping};
use flume::Receiver;
use futures_lite::future;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use futures_lite::stream;
use futures_lite::{FutureExt, Stream, StreamExt};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Signal, kill_process};
use serde_json::json;
use smol::process::{Child, ChildStdin, ChildStdout, Command};
use smol::{Task, Timer};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
use std::process::{self, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use thiserror::Error as ThisError;
use tracing::debug;
#[cfg(unix)]
use tracing::warn;

pub(super) const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const STDERR_DRAIN: Duration = Duration::from_secs(1);
const STDERR_TAIL_LINES: usize = 10;
const STDERR_LINE_BYTES: usize = 4096;
const TRUNCATED_LINE: &str = "…";

#[derive(Debug, ThisError)]
#[error("event exceeds the {MAX_EVENT_BYTES}-byte limit")]
struct EventTooLarge;

pub(in crate::providers::claude_code) async fn within<T>(
    deadline: Instant,
    work: impl Future<Output = T>,
) -> Option<T> {
    async { Some(work.await) }
        .or(async {
            Timer::at(deadline).await;
            None
        })
        .await
}

/// Kills the group on drop until reaping starts.
pub(super) struct Group {
    pub(super) child: Child,
    signals_disabled: bool,
    signalled: bool,
    exit_status: Option<ExitStatus>,
    #[cfg(target_os = "linux")]
    _lifetime: UnixStream,
}

enum LeaderExit {
    Exited,
    WaitFailed,
    TimedOut,
}

impl From<Option<bool>> for LeaderExit {
    fn from(exited: Option<bool>) -> Self {
        match exited {
            Some(true) => Self::Exited,
            Some(false) => Self::WaitFailed,
            None => Self::TimedOut,
        }
    }
}

impl Group {
    pub(super) async fn spawn(command: process::Command) -> Result<Self, Error> {
        smol::unblock(move || spawn_piped(command))
            .await
            .map_err(|source| Error::Io {
                what: "start Claude Code",
                source,
            })
    }

    /// Only while the unreaped leader's pid still names the group.
    pub(super) fn kill(&mut self) {
        if !self.signals_disabled {
            self.signalled = true;
            kill_group(self.child.id());
        }
    }

    pub(super) fn terminate(&mut self) {
        if self.signals_disabled {
            return;
        }
        self.signalled = true;
        #[cfg(unix)]
        if let Some(pid) = pid_of(self.child.id())
            && let Err(error) = kill_process(pid, Signal::TERM)
            && error != Errno::SRCH
        {
            warn!(pid = self.child.id(), %error, "could not request Claude Code shutdown");
        }
        #[cfg(not(unix))]
        self.kill();
    }

    /// Kill remaining group members before the leader reap so the pid cannot identify another
    /// group.
    pub(super) async fn wait(&mut self, deadline: Instant) -> Result<ExitStatus, Error> {
        let exited = self.exited_by(deadline).await;
        let natural_exit = matches!(exited, LeaderExit::Exited) && !self.signalled;
        if !matches!(exited, LeaderExit::WaitFailed) {
            self.kill();
        }
        // The status future may reap even if its caller stops waiting. Disable further
        // signals before it starts, while the leader's pid still belongs to the group.
        self.signals_disabled = true;
        let status = within(Instant::now() + SHUTDOWN_GRACE, self.child.status())
            .await
            .ok_or(Error::ReapLate)?;
        self.exit_status = if natural_exit {
            status.as_ref().ok().copied()
        } else {
            None
        };
        if matches!(exited, LeaderExit::TimedOut) {
            return Err(Error::ExitLate);
        }
        status.map_err(|source| Error::Io {
            what: "reap Claude Code",
            source,
        })
    }

    /// Does not reap the leader.
    #[cfg(unix)]
    async fn exited_by(&self, deadline: Instant) -> LeaderExit {
        let pid = self.child.id();
        within(deadline, smol::unblock(move || wait_without_reaping(pid)))
            .await
            .into()
    }

    /// The open process handle keeps the pid for this process.
    #[cfg(not(unix))]
    async fn exited_by(&mut self, deadline: Instant) -> LeaderExit {
        within(deadline, self.child.status())
            .await
            .map(|status| status.is_ok())
            .into()
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_piped(command: process::Command) -> io::Result<Group> {
    #[cfg(target_os = "linux")]
    let mut command = command;
    #[cfg(target_os = "linux")]
    let lifetime = guard::bind(&mut command)?;
    let child = Command::from(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    Ok(Group {
        child,
        signals_disabled: false,
        signalled: false,
        exit_status: None,
        #[cfg(target_os = "linux")]
        _lifetime: lifetime,
    })
}

type StderrTail = Arc<Mutex<VecDeque<String>>>;

struct Stderr {
    tail: StderrTail,
    reader: Option<Task<()>>,
}

impl Stderr {
    async fn explain(self, error: Error) -> Error {
        if let Some(reader) = self.reader {
            reader
                .or(async {
                    Timer::after(STDERR_DRAIN).await;
                })
                .await;
        }
        with_stderr(error, &self.tail)
    }
}

/// Stops the group and bounds cleanup before attaching stderr to an error.
pub(super) async fn supervised<T>(
    command: process::Command,
    work: impl AsyncFnOnce(&mut Group) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut group = Group::spawn(command).await?;
    debug!(pid = group.child.id(), "claude-code: process started");
    let stderr = watch_stderr(&mut group.child);
    let result = work(&mut group).await;
    if !group.signals_disabled {
        drop(group.child.stdin.take());
        group.terminate();
        if matches!(
            group.wait(Instant::now() + SHUTDOWN_GRACE).await,
            Err(Error::ReapLate)
        ) {
            return Err(stderr.explain(Error::ReapLate).await);
        }
    }
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            let error = match group.exit_status {
                Some(status)
                    if !matches!(
                        error.cause(),
                        Error::VersionFailed(_) | Error::ExitedEarly(_) | Error::ExitedInChecks(_)
                    ) =>
                {
                    Error::WithExit {
                        error: Box::new(error),
                        status,
                    }
                }
                _ => error,
            };
            Err(stderr.explain(error).await)
        }
    }
}

fn watch_stderr(child: &mut Child) -> Stderr {
    let tail: StderrTail = Arc::default();
    let reader = child.stderr.take().map(|stderr| {
        let lines = Arc::clone(&tail);
        // A line that is not UTF-8 must not stop the reader, or Claude Code
        // blocks once its stderr fills the pipe.
        smol::spawn(async move {
            let mut reader = BufReader::new(stderr);
            while let Ok(Some(line)) = stderr_line(&mut reader).await {
                let mut lines = lines.lock().unwrap_or_else(PoisonError::into_inner);
                if lines.len() == STDERR_TAIL_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
        })
    });
    Stderr { tail, reader }
}

async fn stderr_line(reader: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if line.is_empty() && !truncated {
                return Ok(None);
            }
            break;
        }
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let length = end.unwrap_or(chunk.len());
        let kept = length.min(STDERR_LINE_BYTES - line.len());
        line.extend_from_slice(&chunk[..kept]);
        truncated |= kept < length;
        reader.consume(length + usize::from(end.is_some()));
        if end.is_some() {
            break;
        }
    }
    let mut text = String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line)).into_owned();
    if truncated {
        text.push_str(TRUNCATED_LINE);
    }
    Ok(Some(text))
}

fn with_stderr(error: Error, tail: &StderrTail) -> Error {
    let lines = tail.lock().unwrap_or_else(PoisonError::into_inner);
    if lines.is_empty() {
        return error;
    }
    let shown: Vec<&str> = lines.iter().map(String::as_str).collect();
    Error::WithStderr {
        error: Box::new(error),
        stderr: shown.join("\n"),
    }
}

pub(super) fn unreadable(source: io::Error) -> Error {
    if source
        .get_ref()
        .is_some_and(|error| error.is::<EventTooLarge>())
    {
        return Error::EventTooLarge {
            limit: MAX_EVENT_BYTES,
        };
    }
    Error::Io {
        what: "read the output of Claude Code",
        source,
    }
}

pub(super) enum Next {
    Line(Option<io::Result<String>>),
    Handoff(Handoff),
    Late,
}

pub(super) async fn next(
    lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    handoffs: Option<&Receiver<Handoff>>,
    deadline: Instant,
) -> Next {
    within(
        deadline,
        async { Next::Line(lines.next().await) }.or(async {
            match handoffs {
                Some(handoffs) => match handoffs.recv_async().await {
                    Ok(handoff) => Next::Handoff(handoff),
                    Err(_) => future::pending().await,
                },
                None => future::pending().await,
            }
        }),
    )
    .await
    .unwrap_or(Next::Late)
}

async fn event_line(reader: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_EVENT_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_EVENT_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, EventTooLarge));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn stdout_lines(
    stdout: Option<ChildStdout>,
) -> Result<impl Stream<Item = io::Result<String>> + Unpin, Error> {
    let reader = BufReader::new(stdout.ok_or(Error::NoStdout)?);
    Ok(Box::pin(stream::unfold(reader, |mut reader| async move {
        event_line(&mut reader)
            .await
            .transpose()
            .map(|line| (line, reader))
    })))
}

/// Sends the check requests, which Claude Code answers before it reads a
/// prompt.
pub(super) async fn send_handshake(
    stdin: &mut Option<ChildStdin>,
    deadline: Instant,
) -> Result<(), Error> {
    for step in HANDSHAKE {
        let request = control_request(step.id, &json!({ "subtype": step.subtype }));
        send(stdin, &request, deadline).await?;
    }
    Ok(())
}

/// A process that ignores stdin can block a write larger than the pipe buffer. Bound the
/// write with a deadline.
pub(super) async fn send(
    stdin: &mut Option<ChildStdin>,
    data: &str,
    deadline: Instant,
) -> Result<(), Error> {
    let pipe = stdin.as_mut().ok_or(Error::NoStdin)?;
    within(deadline, async {
        pipe.write_all(data.as_bytes())
            .await
            .map_err(|source| Error::Io {
                what: "write to Claude Code",
                source,
            })
    })
    .await
    .ok_or(Error::InputNotTaken)?
}

#[cfg(test)]
mod tests {
    use futures_lite::io::Cursor;
    use test_case::test_case;

    use super::{
        Error, MAX_EVENT_BYTES, STDERR_LINE_BYTES, TRUNCATED_LINE, event_line, stderr_line,
        unreadable,
    };

    const NEXT_LINE: &str = "next";

    #[test_case(false ; "unterminated_event")]
    #[test_case(true ; "terminated_event")]
    fn oversized_events_stop_at_the_limit(terminated: bool) {
        let mut input = vec![b'x'; MAX_EVENT_BYTES + 2];
        if terminated {
            input.push(b'\n');
        }
        let mut reader = Cursor::new(input);
        let error = smol::block_on(event_line(&mut reader)).unwrap_err();
        assert_eq!(reader.position(), (MAX_EVENT_BYTES + 1) as u64);
        assert!(
            matches!(unreadable(error), Error::EventTooLarge { limit } if limit == MAX_EVENT_BYTES)
        );
    }

    #[test_case(false ; "unterminated_event")]
    #[test_case(true ; "terminated_event")]
    fn events_at_the_limit_remain_readable(terminated: bool) {
        let mut input = vec![b'x'; MAX_EVENT_BYTES];
        if terminated {
            input.push(b'\n');
        }
        let mut reader = Cursor::new(input);
        let line = smol::block_on(event_line(&mut reader)).unwrap().unwrap();
        assert_eq!(line.len(), MAX_EVENT_BYTES);
        assert!(smol::block_on(event_line(&mut reader)).unwrap().is_none());
    }

    #[test]
    fn capped_stderr_lines_are_drained_before_the_next_line() {
        smol::block_on(async {
            let input = format!("{}\n{NEXT_LINE}\n", "x".repeat(STDERR_LINE_BYTES * 2));
            let mut reader = Cursor::new(input.as_bytes());
            let line = stderr_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(line.len(), STDERR_LINE_BYTES + TRUNCATED_LINE.len());
            assert!(line.ends_with(TRUNCATED_LINE));
            assert_eq!(
                stderr_line(&mut reader).await.unwrap().as_deref(),
                Some(NEXT_LINE)
            );
            assert!(stderr_line(&mut reader).await.unwrap().is_none());
        });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod process_tests {
    use std::env;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::Command;
    use std::time::Instant;

    use futures_lite::io::{AsyncBufReadExt, BufReader};
    use rustix::process::{Pid, Signal, kill_process};
    use test_case::test_case;

    use crate::providers::claude_code::fake::Fake;

    use super::super::launch::command;
    use super::{Error, Group, SHUTDOWN_GRACE, supervised};

    const SLEEP: &str = "/bin/sleep";
    const LONG_SLEEP_SECS: &str = "60";

    const PROCESS_ERROR: &str = "protocol failure";
    const PROCESS_STDERR: &str = "diagnostic stderr";
    const FAIL_COMMAND: &str = "printf 'diagnostic stderr\\n' >&2; exit 7";
    const FAIL_CODE: i32 = 7;

    #[test_case(false, false ; "natural_exit_has_status")]
    #[test_case(true, false ; "requested_shutdown_has_no_status")]
    #[test_case(true, true ; "deadline_kill_has_no_status")]
    fn protocol_errors_keep_only_natural_process_status(shutdown: bool, deadline: bool) {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            if shutdown {
                "printf 'diagnostic stderr\\n' >&2; printf 'ready\\n'; read reply"
            } else {
                FAIL_COMMAND
            },
        ]);
        let error = smol::block_on(supervised::<()>(command, async |group| {
            if shutdown {
                let mut line = String::new();
                BufReader::new(group.child.stdout.as_mut().unwrap())
                    .read_line(&mut line)
                    .await
                    .unwrap();
                if deadline {
                    assert!(group.wait(Instant::now()).await.is_err());
                }
            } else {
                let status = group.wait(Instant::now() + SHUTDOWN_GRACE).await?;
                assert_eq!(status.code(), Some(FAIL_CODE));
            }
            Err(Error::Failed(PROCESS_ERROR.into()))
        }))
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains(PROCESS_ERROR), "{text}");
        assert!(text.contains(PROCESS_STDERR), "{text}");
        if shutdown {
            assert!(
                matches!(&error, Error::WithStderr { error, .. } if matches!(**error, Error::Failed(_)))
            );
        } else {
            assert!(
                matches!(&error, Error::WithStderr { error, .. } if matches!(**error, Error::WithExit { status, .. } if status.code() == Some(FAIL_CODE)))
            );
        }
        assert!(matches!(error.cause(), Error::Failed(message) if message == PROCESS_ERROR));
    }

    #[test_case(false ; "sigkill")]
    #[test_case(true ; "sigterm")]
    fn errors_after_an_explicit_stop_omit_process_status(graceful: bool) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'ready\n'; read reply"]);
        let error = smol::block_on(supervised::<()>(command, async |group| {
            let mut line = String::new();
            BufReader::new(group.child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .await
                .unwrap();
            if graceful {
                group.terminate();
            } else {
                group.kill();
            }
            group.wait(Instant::now() + SHUTDOWN_GRACE).await?;
            Err(Error::Failed(PROCESS_ERROR.into()))
        }))
        .unwrap_err();
        assert!(
            matches!(error, Error::Failed(ref message) if message == PROCESS_ERROR),
            "{error:?}"
        );
    }

    /// Marking the leader reaped must prevent signalling a pid that may identify another group.
    #[test_case(false ; "sigkill")]
    #[test_case(true ; "sigterm")]
    fn a_reaped_group_is_never_signalled(graceful: bool) {
        let here = env::current_dir().unwrap();
        let args = [LONG_SLEEP_SECS.to_owned()];
        let sleep = command(Path::new(SLEEP), &Fake::env(), &here, &args);
        let mut group = smol::block_on(Group::spawn(sleep)).unwrap();
        group.signals_disabled = true;

        if graceful {
            group.terminate();
        } else {
            group.kill();
        }

        let pid = Pid::from_raw(group.child.id().try_into().unwrap()).unwrap();
        kill_process(pid, Signal::USR1).unwrap();
        let status = smol::block_on(group.child.status()).unwrap();
        assert_eq!(status.signal(), Some(Signal::USR1.as_raw()));
    }
}
