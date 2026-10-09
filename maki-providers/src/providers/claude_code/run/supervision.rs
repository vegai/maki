use super::super::checks::HANDSHAKE;
use super::super::error::Error;
use super::super::mcp::Handoff;
use super::super::stream::control_request;
use super::EXIT;
#[cfg(target_os = "linux")]
use crate::process::guard;
use crate::process::kill_group;
#[cfg(unix)]
use crate::process::wait_without_reaping;
use flume::Receiver;
use futures_lite::future;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use futures_lite::{FutureExt, Stream, StreamExt};
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
use tracing::debug;

const STDERR_DRAIN: Duration = Duration::from_secs(1);
const STDERR_TAIL_LINES: usize = 10;
const STDERR_LINE_BYTES: usize = 4096;
const TRUNCATED_LINE: &str = "…";

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

/// Kills the group on drop unless the leader is already reaped.
pub(super) struct Group {
    pub(super) child: Child,
    pub(super) reaped: bool,
    #[cfg(target_os = "linux")]
    _lifetime: UnixStream,
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
    pub(super) fn kill(&self) {
        if !self.reaped {
            kill_group(self.child.id());
        }
    }

    /// Kill remaining group members before the leader reap so the pid cannot identify another
    /// group.
    pub(super) async fn wait(&mut self, deadline: Instant) -> Result<ExitStatus, Error> {
        let exited = self.exited_by(deadline).await;
        if exited == Some(false) {
            self.reaped = true;
        } else {
            self.kill();
        }
        let status = self.child.status().await;
        self.reaped = true;
        if exited.is_none() {
            return Err(Error::ExitLate);
        }
        status.map_err(|source| Error::Io {
            what: "reap Claude Code",
            source,
        })
    }

    /// Does not reap the leader.
    #[cfg(unix)]
    async fn exited_by(&self, deadline: Instant) -> Option<bool> {
        let pid = self.child.id();
        within(deadline, smol::unblock(move || wait_without_reaping(pid))).await
    }

    /// The open process handle keeps the pid for this process.
    #[cfg(not(unix))]
    async fn exited_by(&mut self, deadline: Instant) -> Option<bool> {
        within(deadline, self.child.status())
            .await
            .map(|status| status.is_ok())
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

pub(super) fn spawn_piped(command: process::Command) -> io::Result<Group> {
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
        reaped: false,
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
    /// Kills `group` first so its stderr ends, and every line gets read, even
    /// from a process that exited before its reader started.
    async fn explain(self, error: Error, group: &mut Group) -> Error {
        group.kill();
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

/// Each error carries what Claude Code printed on stderr. After an error the
/// group is reaped, so none of it still runs when the caller moves on.
pub(super) async fn supervised<T>(
    command: process::Command,
    work: impl AsyncFnOnce(&mut Group) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut group = Group::spawn(command).await?;
    debug!(pid = group.child.id(), "claude-code: process started");
    let stderr = watch_stderr(&mut group.child);
    match work(&mut group).await {
        Ok(value) => Ok(value),
        Err(error) => {
            let error = stderr.explain(error, &mut group).await;
            if !group.reaped {
                let _ = group.wait(Instant::now() + EXIT).await;
            }
            Err(error)
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

pub(super) fn stdout_lines(
    stdout: Option<ChildStdout>,
) -> Result<Lines<BufReader<ChildStdout>>, Error> {
    stdout
        .map(|out| BufReader::new(out).lines())
        .ok_or(Error::NoStdout)
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

    use super::{STDERR_LINE_BYTES, TRUNCATED_LINE, stderr_line};

    const NEXT_LINE: &str = "next";

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
