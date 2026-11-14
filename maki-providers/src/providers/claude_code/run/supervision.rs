use super::super::MAX_EVENT_BYTES;
use super::super::error::Error;
use super::super::mcp::Handoff;
use crate::process::guard;
use crate::process::{ChildExit, ChildGuard};
use crate::process::{STDERR_LINE_BYTES, STDERR_TAIL_LINES, cap_stderr_line, within};
use futures::future;
use futures::stream;
use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::io;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::process::{self, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use thiserror::Error as ThisError;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Sleep, sleep, sleep_until};
use tokio_util::task::AbortOnDropHandle;
use tracing::debug;

pub(super) const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const STDERR_DRAIN: Duration = Duration::from_secs(1);

#[derive(Debug, ThisError)]
#[error("event exceeds the {MAX_EVENT_BYTES}-byte limit")]
struct EventTooLarge;

/// Kills the group on drop until reaping starts.
pub(in crate::providers::claude_code) struct Group {
    child: ChildGuard,
    exit_status: Option<ExitStatus>,
    _lifetime: UnixStream,
}

impl Group {
    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn pipes(
        &mut self,
    ) -> Result<
        (
            Option<ChildStdin>,
            impl Stream<Item = io::Result<String>> + Unpin + use<>,
        ),
        Error,
    > {
        let child = self.child.child_mut().ok_or(Error::NoStdout)?;
        let lines = stdout_lines(child.stdout.take())?;
        Ok((child.stdin.take(), lines))
    }

    pub(super) async fn spawn(command: process::Command) -> Result<Self, Error> {
        tokio::task::spawn_blocking(move || spawn_piped(command))
            .await?
            .map_err(Error::io("start Claude Code"))
    }

    pub(super) fn kill(&mut self) {
        self.child.kill();
    }
    pub(super) fn terminate(&mut self) {
        self.child.terminate();
    }

    /// Kill remaining group members before the leader reap so the pid cannot identify another
    /// group.
    pub(super) async fn wait(&mut self, deadline: Instant) -> Result<ExitStatus, Error> {
        let exited = self.child.exited_by(deadline).await;
        let natural_exit = matches!(exited, ChildExit::Exited) && !self.child.was_signalled();
        if !matches!(exited, ChildExit::WaitFailed) {
            self.kill();
        }
        let status = wait_result(
            exited,
            self.child.reap(Instant::now() + SHUTDOWN_GRACE).await,
        )?;
        self.exit_status = natural_exit.then_some(status);
        Ok(status)
    }
}

fn wait_result(
    exited: ChildExit,
    reaped: Option<io::Result<ExitStatus>>,
) -> Result<ExitStatus, Error> {
    let status = reaped
        .ok_or(Error::ReapLate)?
        .map_err(Error::io("reap Claude Code"))?;
    if matches!(exited, ChildExit::TimedOut) {
        Err(Error::ExitLate)
    } else {
        Ok(status)
    }
}

fn spawn_piped(command: process::Command) -> io::Result<Group> {
    let mut command = command;
    let lifetime = guard::bind(&mut command)?;
    let child = Command::from(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    Ok(Group {
        child: ChildGuard::new(child)?,
        exit_status: None,
        _lifetime: lifetime,
    })
}

type StderrTail = Arc<Mutex<VecDeque<String>>>;

struct Stderr {
    tail: StderrTail,
    reader: Option<AbortOnDropHandle<()>>,
}

impl Stderr {
    async fn explain(self, error: Error) -> Error {
        if let Some(mut reader) = self.reader {
            tokio::select! {
                _ = &mut reader => {},
                () = sleep(STDERR_DRAIN) => {
                    reader.abort();
                    let _ = reader.await;
                },
            }
        }
        with_stderr(error, &self.tail)
    }
}

/// Stops the group and bounds cleanup before attaching stderr to an error.
pub(in crate::providers::claude_code) async fn supervised<T>(
    command: process::Command,
    work: impl AsyncFnOnce(&mut Group) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut group = Group::spawn(command).await?;
    debug!(pid = group.id(), "claude-code: process started");
    let stderr = watch_stderr(
        group
            .child
            .child_mut()
            .and_then(|child| child.stderr.take()),
    );
    let result = work(&mut group).await;
    if group.child.can_signal() {
        drop(group.child.child_mut().and_then(|child| child.stdin.take()));
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
                Some(status) if !error.has_exit_status() => error.with_exit(status),
                _ => error,
            };
            Err(stderr.explain(error).await)
        }
    }
}

fn watch_stderr(stderr: Option<ChildStderr>) -> Stderr {
    let tail: StderrTail = Arc::default();
    let reader = stderr.map(|stderr| {
        let lines = Arc::clone(&tail);
        // A line that is not UTF-8 must not stop the reader, or Claude Code
        // blocks once its stderr fills the pipe.
        AbortOnDropHandle::new(tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            while let Ok(Some(line)) = stderr_line(&mut reader).await {
                let mut lines = lines.lock().unwrap_or_else(PoisonError::into_inner);
                if lines.len() == STDERR_TAIL_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
        }))
    });
    Stderr { tail, reader }
}

async fn stderr_line(reader: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let length = end.unwrap_or(chunk.len());
        let kept = length.min(STDERR_LINE_BYTES + 1 - line.len());
        line.extend_from_slice(&chunk[..kept]);
        reader.consume(length + usize::from(end.is_some()));
        if end.is_some() {
            break;
        }
    }
    Ok(Some(cap_stderr_line(&String::from_utf8_lossy(
        line.strip_suffix(b"\r").unwrap_or(&line),
    ))))
}

fn with_stderr(error: Error, tail: &StderrTail) -> Error {
    let lines = tail.lock().unwrap_or_else(PoisonError::into_inner);
    if lines.is_empty() {
        return error;
    }
    let shown: Vec<&str> = lines.iter().map(String::as_str).collect();
    error.with_stderr(shown.join("\n"))
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

pub(super) struct ReadDeadline {
    timer: Pin<Box<Sleep>>,
    at: Option<Instant>,
}

impl ReadDeadline {
    pub(super) fn new(at: Instant) -> Self {
        Self {
            timer: Box::pin(sleep_until(at.into())),
            at: Some(at),
        }
    }

    pub(super) async fn next(
        &mut self,
        lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
        mut handoffs: Option<&mut UnboundedReceiver<Handoff>>,
        deadline: Instant,
    ) -> Next {
        if self.at.is_none_or(|at| deadline < at) {
            self.timer.as_mut().reset(deadline.into());
            self.at = Some(deadline);
        }
        loop {
            let next = tokio::select! {
                biased;
                line = lines.next() => Next::Line(line),
                handoff = async {
                    match handoffs.as_deref_mut() {
                        Some(handoffs) => match handoffs.recv().await {
                            Some(handoff) => handoff,
                            None => future::pending().await,
                        },
                        None => future::pending().await,
                    }
                } => Next::Handoff(handoff),
                () = async {
                    if Instant::now() < deadline {
                        (&mut self.timer).await;
                    }
                } => Next::Late,
            };
            if !matches!(next, Next::Late) {
                return next;
            }
            self.at = None;
            if Instant::now() >= deadline {
                return next;
            }
            // Progress extends idle time without registering another alarm on each line.
            self.timer.as_mut().reset(deadline.into());
            self.at = Some(deadline);
        }
    }
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
    Ok(Box::pin(
        stream::unfold(reader, |mut reader| async move {
            event_line(&mut reader)
                .await
                .transpose()
                .map(|line| (line, reader))
        })
        .fuse(),
    ))
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
            .map_err(Error::io("write to Claude Code"))
    })
    .await
    .ok_or(Error::InputNotTaken)?
}

#[cfg(test)]
mod tests {
    use crate::process::ChildExit;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant};

    use futures::stream;
    use std::io::Cursor;
    use test_case::test_case;

    use super::{
        Error, MAX_EVENT_BYTES, Next, ReadDeadline, STDERR_LINE_BYTES, event_line, stderr_line,
        unreadable, wait_result,
    };
    use crate::process::STDERR_TRUNCATED;

    const NEXT_LINE: &str = "next";

    #[test_case(ChildExit::TimedOut ; "exit_deadline_also_passed")]
    #[test_case(ChildExit::Exited ; "leader_exited_on_time")]
    fn reap_deadline_always_prevents_a_successful_cleanup(exited: ChildExit) {
        let error = wait_result(exited, None).unwrap_err();
        assert!(matches!(error, Error::ReapLate));
    }

    #[test]
    fn a_reap_error_takes_precedence_over_the_exit_deadline() {
        const REAP_ERROR: &str = "wait failed";
        let error =
            wait_result(ChildExit::TimedOut, Some(Err(io::Error::other(REAP_ERROR)))).unwrap_err();
        assert!(error.to_string().contains(REAP_ERROR));
        assert!(matches!(
            wait_result(ChildExit::TimedOut, Some(Ok(ExitStatus::from_raw(0)))),
            Err(Error::ExitLate)
        ));
    }

    #[test]
    fn a_read_deadline_can_be_reused_after_it_expires() {
        crate::test_support::runtime().block_on(async {
            let later = Instant::now();
            let earlier = later - Duration::from_secs(1);
            let mut timer = ReadDeadline::new(earlier);
            let mut lines = stream::pending();
            assert!(matches!(
                timer.next(&mut lines, None, earlier).await,
                Next::Late
            ));
            assert!(matches!(
                futures::FutureExt::now_or_never(timer.next(&mut lines, None, later)),
                Some(Next::Late)
            ));
        });
    }

    #[test_case(false ; "unterminated_event")]
    #[test_case(true ; "terminated_event")]
    fn oversized_events_stop_at_the_limit(terminated: bool) {
        let mut input = vec![b'x'; MAX_EVENT_BYTES + 2];
        if terminated {
            input.push(b'\n');
        }
        let mut reader = Cursor::new(input);
        let error = crate::test_support::runtime()
            .block_on(event_line(&mut reader))
            .unwrap_err();
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
        let line = crate::test_support::runtime()
            .block_on(event_line(&mut reader))
            .unwrap()
            .unwrap();
        assert_eq!(line.len(), MAX_EVENT_BYTES);
        assert!(
            crate::test_support::runtime()
                .block_on(event_line(&mut reader))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn capped_stderr_lines_are_drained_before_the_next_line() {
        crate::test_support::runtime().block_on(async {
            let input = format!("{}\n{NEXT_LINE}\n", "x".repeat(STDERR_LINE_BYTES * 2));
            let mut reader = Cursor::new(input.as_bytes());
            let line = stderr_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(line.len(), STDERR_LINE_BYTES + STDERR_TRUNCATED.len());
            assert!(line.ends_with(STDERR_TRUNCATED));
            assert_eq!(
                stderr_line(&mut reader).await.unwrap().as_deref(),
                Some(NEXT_LINE)
            );
            assert!(stderr_line(&mut reader).await.unwrap().is_none());
        });
    }
}

#[cfg(test)]
mod process_tests {
    use std::process::Command;
    use std::time::Instant;

    use tokio::io::{AsyncBufReadExt, BufReader};

    use test_case::test_case;

    use super::{Error, SHUTDOWN_GRACE, supervised};

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
        let error = crate::test_support::runtime()
            .block_on(supervised::<()>(command, async |group| {
                if shutdown {
                    let mut line = String::new();
                    BufReader::new(group.child.child_mut().unwrap().stdout.as_mut().unwrap())
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
        let Error::Context(failure) = &error else {
            panic!("{error:?}")
        };
        assert_eq!(failure.stderr, PROCESS_STDERR);
        assert_eq!(
            failure.exit.map(|status| status.code()),
            (!shutdown).then_some(Some(FAIL_CODE))
        );
        assert!(matches!(error.cause(), Error::Failed(message) if message == PROCESS_ERROR));
    }

    #[test]
    fn an_interrupted_exit_reports_the_process_status_once() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", FAIL_COMMAND]);
        let error = crate::test_support::runtime()
            .block_on(supervised::<()>(command, async |group| {
                let status = group.wait(Instant::now() + SHUTDOWN_GRACE).await?;
                Err(Error::ExitedEarly(status).after_start(true))
            }))
            .unwrap_err();
        assert!(
            matches!(&error, Error::Context(failure) if failure.interrupted && matches!(*failure.cause, Error::ExitedEarly(_))),
            "{error:?}"
        );
        assert!(error.temporary().is_some());
    }

    #[test_case(false ; "sigkill")]
    #[test_case(true ; "sigterm")]
    fn errors_after_an_explicit_stop_omit_process_status(graceful: bool) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'ready\n'; read reply"]);
        let error = crate::test_support::runtime()
            .block_on(supervised::<()>(command, async |group| {
                let mut line = String::new();
                BufReader::new(group.child.child_mut().unwrap().stdout.as_mut().unwrap())
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
}
