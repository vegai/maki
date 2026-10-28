//! Each request owns its process group and holds tool calls until the reply is complete.
#[path = "run/launch.rs"]
mod launch;
#[path = "run/private_files.rs"]
mod private_files;
#[path = "run/supervision.rs"]
mod supervision;

pub(crate) use self::launch::{Launch, models};
pub use self::launch::{VersionCheck, invalidate_version, version_check};
use self::launch::{base_args, checked_profile, command, probe};
pub(super) use self::private_files::private_dir_async;
use self::private_files::{PrivateDir, private_dir, private_file};
use self::supervision::{
    Group, Next, ReadDeadline, SHUTDOWN_GRACE, send, send_handshake, supervised, unreadable,
};
use super::checks::InitExpect;
use super::error::Error;
use super::mcp::{self, Handoff};
use super::settings::{MCP_CONFIG_FLAG, STARTUP_TIMEOUT_SECS};
use super::stream::{Step, Turn, Wait, user_message};
use super::transcript::{Catalog, SERVER, system_prompt, transcript};
use super::within;
use crate::providers::anthropic::shared::{long_context_window, strip_long_context};
use crate::providers::oauth_loopback::random_token;
use crate::{Message, ProviderEvent, ProviderUsage, StreamResponse};
use flume::{Receiver, Sender};
use futures_lite::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const PROMPT_FILE: &str = "system-prompt.md";
const MCP_FILE: &str = "mcp.json";
const LONG_CONTEXT_NAME: &str = "[1m]";
const STARTUP: Duration = Duration::from_secs(STARTUP_TIMEOUT_SECS);
const MCP_READ: Duration = Duration::from_secs(60);
const HANDOFF: Duration = Duration::from_secs(30);
const EXIT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_ENV: &str = "CLAUDE_CODE_MAX_OUTPUT_TOKENS";
const EFFORT_FLAG: &str = "--effort";
const SYSTEM_PROMPT_FILE_FLAG: &str = "--system-prompt-file";
const THINKING_BUDGET_ENV: &str = "MAX_THINKING_TOKENS";
const NO_THINKING: (&str, &str) = ("CLAUDE_CODE_DISABLE_THINKING", "1");
const SHOWN_THINKING: (&str, &str) = ("--thinking-display", "summarized");
const NO_ATTACHMENTS: (&str, &str) = ("CLAUDE_CODE_DISABLE_ATTACHMENTS", "1");
const NO_AUTO_COMPACT: (&str, &str) = ("DISABLE_AUTO_COMPACT", "1");
const NO_NONSTREAMING_FALLBACK: (&str, &str) = ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1");
// A CLI retry starts a second generation inside one stream. maki retries the whole request.
const NO_CLI_RETRIES: (&str, &str) = ("CLAUDE_CODE_MAX_RETRIES", "0");
const NO_MID_CONVERSATION_SYSTEM: (&str, &str) =
    ("CLAUDE_CODE_MODEL_CAPABILITIES", "-mid_conv_system");
// Held calls must outlive generation, so Claude Code cannot answer them before maki stops it.
const HELD_CALL_TIMEOUT_MS: u64 = i32::MAX as u64;

pub(crate) struct Limits {
    /// Time for Claude Code to start, answer the handshake and send its init
    /// event, and also for each write to its stdin.
    pub startup: Duration,
    /// Time between two events of a reply: maki's stream timeout.
    pub idle: Duration,
    /// Time from the end of a tool-calling reply to its held call.
    pub handoff: Duration,
    pub exit: Duration,
}

impl Limits {
    pub(crate) fn new(idle: Duration) -> Self {
        Self {
            startup: STARTUP,
            idle,
            handoff: HANDOFF,
            exit: EXIT,
        }
    }
}

/// A model Claude Code offers, and the context window it opens for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Listed {
    pub id: String,
    pub window: Option<u32>,
}

pub(crate) struct Exchange<'a> {
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a Value,
    pub events: &'a Sender<ProviderEvent>,
}

pub(crate) struct Request<'a> {
    pub launch: Launch<'a>,
    pub model: &'a str,
    pub exchange: Exchange<'a>,
    pub plan_usage: &'a Mutex<Option<ProviderUsage>>,
    pub max_output: Option<u32>,
    pub thinking: Thinking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinking {
    Default,
    Off,
    /// For a model with adaptive thinking.
    Effort(&'static str),
    Budget(u32),
}

fn token() -> Result<String, Error> {
    random_token().map_err(|error| Error::Io {
        what: "make a handoff token",
        source: io::Error::other(error),
    })
}

/// Recheck current policy and the executable's identity before starting in the project.
pub(crate) async fn request(req: Request<'_>) -> Result<StreamResponse, Error> {
    let started = Instant::now();
    let launch = &req.launch;
    let limits = launch.limits;
    // Before any process starts, so a conversation maki cannot send costs
    // none.
    let (catalog, tools) = Catalog::new(req.exchange.tools)?;
    let conversation = user_message(&transcript(req.exchange.messages)?)?;
    // maki marks the 1M window with a `-1m` id and Claude Code with a `[1m]`
    // name. The generation reports the model without either.
    let asked = strip_long_context(req.model);
    let model_arg = match long_context_window(req.model) {
        Some(_) => format!("{asked}{LONG_CONTEXT_NAME}"),
        None => asked.to_owned(),
    };
    let (handoff_tx, handoffs) = flume::unbounded();
    let server = mcp::serve(token()?, tools, handoff_tx, MCP_READ)
        .await
        .map_err(|source| Error::Io {
            what: "start the handoff server",
            source,
        })?;
    let checks = async {
        let profile = checked_profile(launch).await?;
        let offered = probe(launch, req.plan_usage).await?;
        Ok::<_, Error>((profile, offered))
    };
    let ((profile, offered), (dir, dir_path)) =
        futures::future::try_join(checks, request_files(launch, req.exchange.system, &server))
            .await?;
    // An alias such as `sonnet` runs whatever model the account answer maps
    // it to.
    let runs = offered.resolved_model(asked).unwrap_or(asked);
    let checked = started.elapsed();
    debug!(
        version = %profile,
        checks_ms = millis(checked),
        "claude-code: checks passed"
    );
    // Keep the server outside the request future so process cleanup finishes first. Held
    // calls must retain their connection until Claude Code stops.
    let mut server = Some(server);
    let (args, env) = run_args(
        &req,
        &model_arg,
        &dir_path.join(MCP_FILE),
        &dir_path.join(PROMPT_FILE),
    );
    let tools = catalog.exposed();
    let run = Run {
        turn: Turn::new(
            &catalog,
            InitExpect {
                version: &profile,
                model: runs,
                cwd: launch.project,
                tools: &tools,
            },
            req.plan_usage,
        ),
        conversation: &conversation,
        handoffs,
        events: req.exchange.events,
        limits,
    };
    let ran = Instant::now();
    let result = supervised(
        command(launch.executable, &env, launch.project, &args),
        async |group| converse(group, run, &mut server).await,
    )
    .await;
    dir.close().await;
    let (response, prompted) = result?;
    let usage = &response.usage;
    info!(
        model = req.model,
        version = %profile,
        checks_ms = millis(checked),
        startup_ms = millis(prompted.duration_since(ran)),
        generation_ms = millis(prompted.elapsed()),
        stop_reason = ?response.stop_reason,
        calls = response.message.tool_uses().count(),
        input = usage.input,
        output = usage.output,
        cache_read = usage.cache_read,
        cache_write = usage.cache_creation,
        "claude-code: request done"
    );
    Ok(response)
}

async fn request_files(
    launch: &Launch<'_>,
    system: &str,
    server: &mcp::Server,
) -> Result<(PrivateDir, PathBuf), Error> {
    let mcp_config = json!({ "mcpServers": { SERVER: {
        "type": "http",
        "url": server.url(),
        "headers": { "Authorization": server.authorization() },
        "timeout": HELD_CALL_TIMEOUT_MS,
    } } });
    let temp_dir = launch.temp_dir.to_owned();
    let project = launch.project.to_owned();
    let denied = launch.denied.to_vec();
    let prompt = system_prompt(system);
    smol::unblock(move || {
        let (dir, path) = private_dir(&temp_dir, &project, &denied)?;
        private_file(&path.join(PROMPT_FILE), &prompt)
            .and_then(|()| private_file(&path.join(MCP_FILE), &mcp_config.to_string()))
            .map_err(|source| Error::Io {
                what: "write the request files",
                source,
            })?;
        Ok((dir, path))
    })
    .await
}

fn failed(error: Error, accepted: bool, trace: &str) -> Error {
    warn!(%error, events = trace, "claude-code: the request failed");
    error.after_start(accepted)
}

pub(super) fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// The arguments and environment of a request's run.
fn run_args(
    req: &Request<'_>,
    model_arg: &str,
    mcp_file: &Path,
    prompt_file: &Path,
) -> (Vec<OsString>, Vec<(String, String)>) {
    let mut args: Vec<OsString> = base_args(model_arg)
        .into_iter()
        .map(OsString::from)
        .collect();
    args.extend([
        MCP_CONFIG_FLAG.into(),
        mcp_file.as_os_str().to_owned(),
        SYSTEM_PROMPT_FILE_FLAG.into(),
        prompt_file.as_os_str().to_owned(),
    ]);
    let mut env = req.launch.env.to_vec();
    for (name, value) in [
        NO_ATTACHMENTS,
        NO_AUTO_COMPACT,
        NO_NONSTREAMING_FALLBACK,
        NO_CLI_RETRIES,
        NO_MID_CONVERSATION_SYSTEM,
    ] {
        env.push((name.to_owned(), value.to_owned()));
    }
    if let Some(cap) = req.max_output {
        env.push((MAX_OUTPUT_ENV.to_owned(), cap.to_string()));
    }
    match req.thinking {
        Thinking::Default => {}
        Thinking::Off => env.push((NO_THINKING.0.to_owned(), NO_THINKING.1.to_owned())),
        Thinking::Effort(level) => args.extend([EFFORT_FLAG.into(), level.into()]),
        Thinking::Budget(tokens) => env.push((THINKING_BUDGET_ENV.to_owned(), tokens.to_string())),
    }
    args.extend([SHOWN_THINKING.0.into(), SHOWN_THINKING.1.into()]);
    (args, env)
}

/// What one request's run works on.
struct Run<'a> {
    turn: Turn<'a>,
    conversation: &'a str,
    handoffs: Receiver<Handoff>,
    events: &'a Sender<ProviderEvent>,
    limits: &'a Limits,
}

/// Sends the handshake and the conversation, reads the reply until it is
/// done, and ends the process. `server` closes after the reap. Also returns
/// when the conversation went out, for the log.
async fn converse(
    group: &mut Group,
    mut run: Run<'_>,
    server: &mut Option<mcp::Server>,
) -> Result<(StreamResponse, Instant), Error> {
    let result = exchange_reply(group, &mut run, server).await;
    let accepted = run.turn.accepted();
    let trace = run.turn.trace();
    result
        .and_then(|prompted| Ok((run.turn.response()?, prompted)))
        .map_err(|error| failed(error, accepted, &trace))
}

async fn exchange_reply(
    group: &mut Group,
    run: &mut Run<'_>,
    server: &mut Option<mcp::Server>,
) -> Result<Instant, Error> {
    let Run {
        turn,
        conversation,
        handoffs,
        events,
        limits,
    } = run;
    let mut prompted = Instant::now();
    let (mut stdin, mut lines) = group.pipes()?;
    let mut startup = Instant::now() + limits.startup;
    send_handshake(&mut stdin, startup).await?;
    let mut last_event = Instant::now();
    // Set when the reply is complete, and keep-alives must not move it.
    let mut finish_by = None;
    let mut result_late = false;
    let mut timer = ReadDeadline::new(startup);
    loop {
        let waiting = turn.waiting();
        let deadline = match waiting {
            Wait::Startup => startup,
            Wait::Handoff => finish_deadline(
                &mut finish_by,
                waiting.clone(),
                Instant::now() + limits.handoff,
            ),
            Wait::Result => finish_deadline(
                &mut finish_by,
                waiting.clone(),
                Instant::now() + limits.exit,
            ),
            Wait::Idle => last_event + limits.idle,
        };
        let step = match timer.next(&mut lines, Some(handoffs), deadline).await {
            Next::Line(Some(Ok(line))) => turn.feed(&line),
            Next::Line(Some(Err(source))) => Err(unreadable(source)),
            Next::Line(None)
                if !turn.is_broken() && turn.awaits_result() && turn.has_stream_usage() =>
            {
                Ok(Step::Done)
            }
            Next::Line(None) => {
                let ended = match group.wait(Instant::now() + limits.exit).await {
                    Ok(status) => Error::ExitedEarly(status),
                    Err(Error::ExitLate) => Error::WentQuiet,
                    Err(other) => other,
                };
                let error = turn.take_refusal().unwrap_or(ended);
                return Err(error);
            }
            Next::Handoff(handoff) => turn.park(handoff),
            // The reply and its usage are complete, so the result only
            // repeats them. Its delay is Claude Code's own business.
            Next::Late if turn.is_broken() => Err(turn.take_refusal().unwrap_or(Error::WentQuiet)),
            Next::Late if turn.awaits_result() && turn.has_stream_usage() => {
                warn!(
                    secs = limits.exit.as_secs(),
                    "claude-code: the result is late after a complete reply, so maki uses the reply as it streamed"
                );
                result_late = true;
                Ok(Step::Done)
            }
            Next::Late => Err(match waiting {
                Wait::Startup => Error::StartupLate(limits.startup.as_secs()),
                Wait::Idle => Error::Stalled(limits.idle.as_secs()),
                Wait::Handoff => Error::HandoffLate(limits.handoff.as_secs()),
                Wait::Result => Error::ResultLate(limits.exit.as_secs()),
            }),
        };
        // Keep-alives come from a timer, so they do not restart the clock.
        if !matches!(step, Ok(Step::Alive)) {
            last_event = Instant::now();
        }
        match step? {
            Step::Ready => {
                debug!("claude-code: handshake accepted, sending the conversation");
                // The conversation gets a full limit. Nothing reads stdout
                // during the write, but Claude Code answers nothing before
                // the write ends.
                prompted = Instant::now();
                startup = prompted + limits.startup;
                send(&mut stdin, conversation, startup).await?;
                turn.prompted();
                stdin = None;
            }
            Step::Event(event) => {
                let _ = events.send(event);
            }
            Step::Done => break,
            Step::Nothing | Step::Alive => {}
        }
    }
    stop_and_drain(
        group,
        turn,
        &mut lines,
        handoffs,
        limits,
        server,
        result_late,
    )
    .await?;
    Ok(prompted)
}

async fn stop_and_drain(
    group: &mut Group,
    turn: &mut Turn<'_>,
    lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    handoffs: &Receiver<Handoff>,
    limits: &Limits,
    server: &mut Option<mcp::Server>,
    result_late: bool,
) -> Result<(), Error> {
    // Claude Code would continue after a handoff or a cut reply, so it is
    // stopped here. Every event up to the end of the output is still checked,
    // so nothing after the reply can contradict it unnoticed.
    let stopped = turn.calls_tools() || turn.truncated() || result_late;
    let mut deadline = Instant::now() + limits.exit;
    if turn.truncated() {
        group.kill();
    } else if stopped {
        group.terminate();
    }
    if stopped {
        deadline = deadline.min(Instant::now() + SHUTDOWN_GRACE);
    }
    match drain(turn, lines, handoffs, deadline, stopped).await {
        Err(Error::ExitLate) => {
            group.kill();
            warn!("claude-code: output stayed open after a complete reply; stopping the CLI");
            deadline = Instant::now() + limits.exit.min(SHUTDOWN_GRACE);
            match drain(turn, lines, handoffs, deadline, true).await {
                Err(Error::ExitLate) => {
                    warn!("claude-code: output stayed open after the kill; keeping the reply")
                }
                result => result?,
            }
            deadline = Instant::now() + limits.exit.min(SHUTDOWN_GRACE);
        }
        result => result?,
    }
    let status = match group.wait(deadline).await {
        Ok(status) => Some(status),
        Err(Error::ExitLate) => {
            warn!("claude-code: the CLI did not exit after a complete reply; keeping the reply");
            None
        }
        Err(error) => return Err(error),
    };
    // After the reap, Claude Code cannot make calls. Stop the server to end the queue and
    // validate all remaining calls.
    drop(server.take());
    queued_handoffs(turn, handoffs, Instant::now() + limits.exit).await?;
    if !stopped && let Some(status) = status.filter(|status| !status.success()) {
        warn!(%status, "claude-code: the CLI exited with an error after a validated reply");
    }
    Ok(())
}

fn finish_deadline(
    state: &mut Option<(Wait, Instant)>,
    waiting: Wait,
    deadline: Instant,
) -> Instant {
    let current = state.get_or_insert((waiting.clone(), deadline));
    if current.0 != waiting {
        *current = (waiting, deadline);
    }
    current.1
}

async fn queued_handoffs(
    turn: &mut Turn<'_>,
    handoffs: &Receiver<Handoff>,
    deadline: Instant,
) -> Result<(), Error> {
    loop {
        let handoff = within(deadline, handoffs.recv_async())
            .await
            .ok_or(Error::ServerLate)?
            .ok();
        match handoff {
            Some(handoff) => {
                turn.park(handoff)?;
            }
            None => return Ok(()),
        }
    }
}

/// When Claude Code was `stopped`, the kill can cut its last line, so a
/// non-event line is accepted if the output ends right after it.
async fn drain(
    turn: &mut Turn<'_>,
    lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    handoffs: &Receiver<Handoff>,
    deadline: Instant,
    stopped: bool,
) -> Result<(), Error> {
    let mut timer = ReadDeadline::new(deadline);
    loop {
        let step = match timer.next(lines, Some(handoffs), deadline).await {
            Next::Line(Some(Ok(line))) => if stopped {
                turn.feed_stopping(&line)
            } else {
                turn.feed(&line)
            }
            .map(drop),
            Next::Line(Some(Err(source))) => Err(unreadable(source)),
            Next::Line(None) => return Ok(()),
            Next::Handoff(handoff) => turn.park(handoff).map(drop),
            Next::Late => Err(Error::ExitLate),
        };
        let Err(error) = step else {
            continue;
        };
        let cut = stopped
            && matches!(error, Error::NotAnEvent { .. } | Error::Io { .. })
            && matches!(timer.next(lines, None, deadline).await, Next::Line(None));
        return if cut { Ok(()) } else { Err(error) };
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{self, Stdio};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    use futures_lite::{FutureExt, StreamExt};
    use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process_group};
    use serde_json::{Value, error::Category, json};
    use smol::Timer;
    use tempfile::tempdir;
    use test_case::test_case;

    use super::super::WAIT;
    use super::super::checks::{InitExpect, profile};
    use super::super::error::Error;
    use super::super::fake::{
        ALIAS, EXIT_LIMIT, Fake, HAIKU, IDLE, MARKER, OPUS, POLL, TEMP_BASE, TOOL, VERSION_ERROR,
        wait_until,
    };
    use super::super::mcp::Handoff;
    use super::super::stream::Turn;
    use super::super::transcript::{Catalog, SERVER};
    use super::private_files::{DIR_PREFIX, OWNER_SEPARATOR, owner_namespace, sweep_dead_owners};
    use super::{
        EFFORT_FLAG, Exchange, Group, HANDOFF, HELD_CALL_TIMEOUT_MS, Launch, Limits, Listed,
        MCP_CONFIG_FLAG, NO_AUTO_COMPACT, NO_CLI_RETRIES, NO_MID_CONVERSATION_SYSTEM,
        NO_NONSTREAMING_FALLBACK, NO_THINKING, Request, SHOWN_THINKING, STARTUP,
        SYSTEM_PROMPT_FILE_FLAG, Thinking, Wait, command, finish_deadline, models, probe,
        queued_handoffs, run_args,
    };
    use crate::providers::anthropic::{LABEL_SESSION, LABEL_WEEK_ALL};
    use crate::{ContentBlock, ImageMediaType, ImageSource, Role, StopReason};
    use crate::{Message, ProviderEvent, StreamResponse};

    const SHORT_EXIT: Duration = Duration::from_secs(5);

    const SLEEP: &str = "/bin/sleep";
    const MAKI_ROLE: &str = "MAKI_TEST_PLAYS_MAKI";
    const MEMBER_PID: &str = "member-pid";
    const LONG_SLEEP_SECS: &str = "30";
    /// Much larger than a pipe buffer, so the write blocks when nobody reads.
    const UNREAD_PROMPT_BYTES: usize = 1 << 20;
    /// The run's handshake, generous for a loaded machine.
    const UNREAD_STARTUP: Duration = Duration::from_secs(10);
    const LONG_CONTEXT_ID: &str = "claude-haiku-4-5-1m";
    /// A model the fake never runs.
    const OTHER_MODEL: &str = "claude-sonnet-5";
    const UNKNOWN_TOOL: &str = "unknown";
    /// What the fake's `get_context_usage` reports for its Opus and for every
    /// other model.
    const WIDE_WINDOW: u32 = 1_000_000;
    const STANDARD_WINDOW: u32 = 200_000;
    /// Makes the fake send an error for `get_context_usage`.
    const NO_WINDOWS: &str = "no_windows";
    const LONG_CONTEXT_ARG: &str = "claude-haiku-4-5[1m]";
    const OLDER_VERSION: &str = "2.1.283 (Claude Code)";
    const NEWER_VERSION: &str = "2.1.290 (Claude Code)";
    const SHORT_HANDOFF: Duration = Duration::from_secs(5);
    const SHORT_IDLE: Duration = Duration::from_secs(5);
    const NOT_UTF8_VERSION: &[u8] = b"2.1.284 \xff(Claude Code)";
    /// What the fake records for a variable it did not receive.
    const UNSET: &str = "unset";
    const EFFORT: &str = "high";
    const BUDGET: u32 = 4096;
    const NO_AUTO_COMPACT_SETTING: &str = r#""autoCompactEnabled":false"#;
    /// Far beyond every limit the keep-alive tests set, so only keep-alives
    /// that hold a reply open reach it.
    const KEPT_OPEN: Duration = Duration::from_secs(60);
    /// The output count in the fake's final `message_delta`.
    const FINAL_OUTPUT: u32 = 42;
    const ANY_CWD: &str = "/";
    const REPLY_TEXT: &str = "Reading.";
    /// What the fake's `limits` reports.
    const SESSION_USED: u32 = 45;
    const SESSION_RESET_MS: u64 = 1_790_268_000_000;
    const WEEK_USED: u32 = 15;
    const WEEK_RESET_MS: u64 = 1_790_521_200_000;
    const PRIVATE_MODES: &str = "600\n700\n";
    const MCP_OK: &str = "HTTP/1.1 200 OK";
    const TRACK_SESSIONS: &str = "track_sessions";
    const SESSIONS: &str = "sessions";
    const CLEANED_SESSIONS: &str = "cleaned_sessions";
    const ANSWERED_CALL: &str = "answered_call";
    /// Prints the pid of a `sleep` that closed all its standard streams.
    const LINGERING_MEMBER: &str = "sleep 30 </dev/null >/dev/null 2>&1 & echo $!";

    fn into_cause(error: Error) -> Error {
        match error {
            Error::WithStderr { error, .. } | Error::WithExit { error, .. } => into_cause(*error),
            error => error,
        }
    }

    #[test]
    fn request_file_arguments_preserve_non_utf8_paths() {
        const RAW_DIRECTORY: &[u8] = b"/tmp/private-\xff";
        let root = Path::new(OsStr::from_bytes(RAW_DIRECTORY));
        let mcp = root.join(super::MCP_FILE);
        let prompt = root.join(super::PROMPT_FILE);
        let limits = Limits::new(IDLE);
        let (events, _) = flume::unbounded();
        let req = Request {
            launch: Launch {
                executable: Path::new(SLEEP),
                env: &[],
                project: root,
                denied: &[],
                temp_dir: root,
                limits: &limits,
            },
            model: ALIAS,
            exchange: Exchange {
                system: "",
                messages: &[],
                tools: &Value::Null,
                events: &events,
            },
            plan_usage: &Mutex::new(None),
            max_output: None,
            thinking: Thinking::Default,
        };
        let (args, env) = run_args(&req, ALIAS, &mcp, &prompt);
        let command = command(Path::new(SLEEP), &env, root, &args);
        let args: Vec<_> = command.get_args().collect();
        for (flag, path) in [(MCP_CONFIG_FLAG, &mcp), (SYSTEM_PROMPT_FILE_FLAG, &prompt)] {
            assert_eq!(
                args.windows(2)
                    .find(|pair| pair[0] == flag)
                    .map(|pair| pair[1]),
                Some(path.as_os_str())
            );
        }
    }

    #[test_case(Wait::Handoff, Wait::Handoff, false ; "handoff_events_keep_the_deadline")]
    #[test_case(Wait::Result, Wait::Result, false ; "result_events_keep_the_deadline")]
    #[test_case(Wait::Handoff, Wait::Result, true ; "broken_handoff_gets_a_result_deadline")]
    fn finalization_deadline_tracks_what_is_pending(first: Wait, next: Wait, reset: bool) {
        let original = Instant::now();
        let later = original + EXIT_LIMIT;
        let mut state = None;
        assert_eq!(finish_deadline(&mut state, first, original), original);
        assert_eq!(
            finish_deadline(&mut state, next, later),
            if reset { later } else { original }
        );
    }

    /// The call can reach the server before or after the generation ends. Maki gets
    /// the whole batch and final usage, with no process or private file left behind.
    #[test_case("batch" ; "the_generation_first")]
    #[test_case("cut_line" ; "a_last_line_the_stop_cut_short")]
    #[test_case("rejected_max_turns" ; "a_rejected_call_at_the_turn_limit")]
    fn a_batch_hands_off_whole_and_leaves_nothing(scenario: &str) {
        let fake = Fake::new(scenario);
        let (result, events) = smol::block_on(fake.request());
        let response = result.unwrap();

        let calls: Vec<(String, String)> = response
            .message
            .tool_uses()
            .map(|(id, name, _)| (id.to_owned(), name.to_owned()))
            .collect();
        assert_eq!(
            calls,
            [
                ("toolu_1".to_owned(), TOOL.to_owned()),
                ("toolu_2".to_owned(), TOOL.to_owned())
            ]
        );
        assert!(matches!(response.message.role, Role::Assistant));
        assert!(matches!(
            response.message.content[0],
            ContentBlock::Text { .. }
        ));
        assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(response.usage.output, FINAL_OUTPUT);
        assert_eq!(response.usage.cache_read, 222);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::TextDelta { text } if text == "Reading."))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::ToolUseStart { name, .. } if name == TOOL))
        );

        assert!(fake.group_gone(), "Claude Code continued after its request");
        assert!(
            !Path::new(&fake.log("prompt_file")).exists(),
            "the private directory stayed after the request"
        );
        assert_eq!(fake.log("attachments_disabled"), "1");
        assert_eq!(fake.log("modes"), PRIVATE_MODES);
        assert_eq!(
            fake.log("mcp_status").lines().collect::<Vec<_>>(),
            [MCP_OK, MCP_OK]
        );
        let argv = fake.log("argv");
        assert!(
            !argv.contains(MARKER),
            "the conversation must not be on the command line"
        );
        assert!(
            !argv.contains(&fake.log("token")),
            "the handoff token must not be on the command line"
        );
        assert!(fake.log("stdin_prompt").contains(MARKER));
    }

    #[test_case("graceful_batch", true ; "a_cooperative_leader")]
    #[test_case("ignores_term_batch", false ; "a_leader_that_ignores_sigterm")]
    #[test_case("shutdown_error_batch", true ; "shutdown_reports_its_cancelled_call")]
    #[test_case("shutdown_not_run_batch", true ; "shutdown_reports_a_call_that_did_not_run")]
    fn stopping_a_tool_reply_keeps_calls_parked_during_cleanup(scenario: &str, cleans: bool) {
        let fake = Fake::new(scenario);
        fs::write(fake.dir.path().join(TRACK_SESSIONS), "").unwrap();
        let reply = through_keep_alives(&fake, limits(STARTUP, IDLE, HANDOFF, SHORT_EXIT)).unwrap();
        assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(reply.message.tool_uses().count(), 2);
        assert!(fake.log(ANSWERED_CALL).is_empty());
        assert_eq!(
            fs::read_dir(fake.dir.path().join(SESSIONS))
                .unwrap()
                .next()
                .is_none(),
            cleans
        );
        assert_eq!(
            fake.log(CLEANED_SESSIONS)
                .lines()
                .any(|pid| pid == fake.log("pid").trim()),
            cleans
        );
        assert!(fake.group_gone());
        assert!(fake.leader_reaped());
    }

    #[test]
    fn a_refused_request_allows_the_cli_to_clean_its_session() {
        let fake = Fake::new("graceful_refusal");
        fs::write(fake.dir.path().join(TRACK_SESSIONS), "").unwrap();
        let (result, _) = smol::block_on(fake.request());
        assert!(matches!(result.unwrap_err().cause(), Error::Check(_)));
        assert!(
            fs::read_dir(fake.dir.path().join(SESSIONS))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            fake.log(CLEANED_SESSIONS)
                .lines()
                .any(|pid| pid == fake.log("pid").trim())
        );
        assert!(fake.group_gone());
        assert!(fake.leader_reaped());
    }

    #[test]
    fn a_capped_reply_stops_without_a_cleanup_grace() {
        let fake = Fake::new("graceful_cut");
        fs::write(fake.dir.path().join(TRACK_SESSIONS), "").unwrap();
        let (reply, _) = smol::block_on(fake.request());
        assert_eq!(reply.unwrap().stop_reason, Some(StopReason::MaxTokens));
        assert!(
            !fake
                .log(CLEANED_SESSIONS)
                .lines()
                .any(|pid| pid == fake.log("pid").trim())
        );
        assert!(fake.group_gone());
        assert!(fake.leader_reaped());
    }

    /// Runs a request on a fake that sends keep-alives until it is stopped,
    /// and fails the test if they hold the request open.
    fn through_keep_alives(fake: &Fake, limits: Limits) -> Result<StreamResponse, Error> {
        let request = async {
            Some(
                fake.request_with(&format!("find {MARKER}"), ALIAS, limits)
                    .await
                    .0,
            )
        };
        let held = async {
            Timer::after(KEPT_OPEN).await;
            None
        };
        smol::block_on(request.or(held)).expect("keep-alives kept the reply open")
    }

    /// Once the reply and every usage count have streamed, the result only
    /// repeats them. When it is late, maki keeps the reply and stops Claude
    /// Code, and keep-alives do not hold the request open.
    #[test_case("stalled_result" ; "late_result")]
    #[test_case("complete_output" ; "eof_after_complete_reply")]
    fn a_missing_result_after_a_complete_reply_keeps_the_reply(scenario: &str) {
        let fake = Fake::new(scenario);
        let result = through_keep_alives(&fake, limits(STARTUP, IDLE, SHORT_HANDOFF, SHORT_EXIT));

        let response = result.unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(response.usage.output, FINAL_OUTPUT);
        assert!(fake.group_gone(), "Claude Code continued after the reply");
    }

    /// Events without content can follow the result, like the keep-alive
    /// Claude Code prints on long runs.
    #[test_case("text_then_keep_alive")]
    #[test_case("text_then_fail")]
    fn a_reply_without_calls_ends_at_its_result(scenario: &str) {
        let fake = Fake::new(scenario);
        let (result, _) = smol::block_on(fake.request());
        let response = result.unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(response.message.tool_uses().count(), 0);
        assert!(fake.group_gone());
    }

    #[test_case("api_key" => matches Error::Check(_) ; "an_api_key_at_init")]
    #[test_case("other_arguments" => matches Error::CallDiffers(_) ; "a_call_that_differs_from_the_reply")]
    #[test_case("overloaded" => matches Error::ApiRefused { status: Some(529), .. } ; "an_overloaded_api")]
    #[test_case("cli_retry" => matches Error::CliRetry { status: Some(529), .. } ; "a_retry_claude_code_wanted")]
    #[test_case("no_final_usage" => matches Error::NoFinalOutput ; "a_reply_with_only_the_placeholder_output_count")]
    #[test_case("text_then_api_key" => matches Error::Check(_) ; "a_text_result_then_an_init_that_contradicts_it")]
    #[test_case("text_then_garbage" => matches Error::NotAnEvent { .. } ; "a_text_result_then_a_line_that_is_no_event")]
    fn a_run_maki_cannot_accept_stops_and_is_killed(scenario: &str) -> Error {
        let fake = Fake::new(scenario);
        let (result, _) = smol::block_on(fake.request());
        assert!(fake.leader_reaped(), "the request returned before the reap");
        assert!(
            fake.group_gone(),
            "a stopped run must not continue after its request"
        );
        match into_cause(result.unwrap_err()) {
            Error::Interrupted(error) => *error,
            error => error,
        }
    }

    #[test]
    fn malformed_tool_json_retries_without_running_a_partial_call() {
        let fake = Fake::new("malformed_arguments");
        let (result, _) = smol::block_on(fake.request());
        let error = result.unwrap_err();
        assert!(
            matches!(error.cause(), Error::Interrupted(inner) if matches!(inner.as_ref(), Error::MalformedArguments(_)))
        );
        assert!(super::super::agent_error(error).is_retryable());
        assert!(fake.group_gone());
        assert!(fake.leader_reaped());
        assert!(fake.log("mcp_status").is_empty());
    }

    #[test_case("missing_block_eof", false)]
    #[test_case("missing_block_late", false)]
    #[test_case("status_only_error", true)]
    fn incomplete_generations_have_consistent_retry_policy(scenario: &str, retryable: bool) {
        let fake = Fake::new(scenario);
        let (result, _) = smol::block_on(fake.request_with(
            MARKER,
            ALIAS,
            limits(STARTUP, IDLE, SHORT_HANDOFF, SHORT_EXIT),
        ));
        let error = result.unwrap_err();
        assert_eq!(error.temporary().is_some(), retryable, "{error:?}");
        if !retryable {
            assert!(
                matches!(error.cause(), Error::BlockCount { .. }),
                "{error:?}"
            );
        }
        assert!(fake.group_gone());
        assert!(fake.leader_reaped());
    }

    #[test]
    fn a_truncated_line_before_stop_is_diagnosed_and_retryable() {
        let fake = Fake::new("truncated_line");
        let (result, _) = smol::block_on(fake.request());
        let error = into_cause(result.unwrap_err());
        assert!(error.temporary().is_some());
        let Error::Interrupted(error) = error else {
            panic!("{error:?}")
        };
        let Error::NotAnEvent {
            category,
            bytes,
            source,
            ..
        } = *error
        else {
            panic!("{error:?}")
        };
        assert_eq!(category, Category::Eof);
        assert_eq!(bytes, source.column());
        assert!(fake.leader_reaped());
        assert!(fake.group_gone());
    }

    /// An API error waits for the result that gives its status, but no
    /// longer than the exit limit, and then it is the error.
    #[test]
    fn an_api_error_without_its_result_is_the_error() {
        let fake = Fake::new("refused_then_quiet");
        let (result, _) = smol::block_on(fake.request_with(
            &format!("find {MARKER}"),
            ALIAS,
            limits(STARTUP, IDLE, HANDOFF, SHORT_EXIT),
        ));
        assert!(
            matches!(
                result.as_ref().map_err(Error::cause),
                Err(Error::ApiRefused { .. })
            ),
            "{result:?}"
        );
        assert!(fake.group_gone());
    }

    /// Cancellation can precede spawn completion or channel receipt. An unclaimed process
    /// group must stop in either case.
    #[test_case(true ; "before_the_send")]
    #[test_case(false ; "in_the_channel")]
    fn a_process_nobody_takes_is_stopped(cancelled_before_the_send: bool) {
        let here = env::current_dir().unwrap();
        let args = [LONG_SLEEP_SECS.to_owned()];
        let group = smol::block_on(Group::spawn(command(
            Path::new(SLEEP),
            &Fake::env(),
            &here,
            &args,
        )))
        .unwrap();
        let pid = Pid::from_raw(group.id().try_into().unwrap()).unwrap();
        let (reply, answer) = flume::bounded::<io::Result<Group>>(1);
        let answer = (!cancelled_before_the_send).then_some(answer);
        let _ = reply.send(Ok(group));
        drop((reply, answer));
        assert!(wait_until(|| test_kill_process_group(pid).is_err()));
    }

    /// maki cannot send an image to Claude Code, so a conversation with one
    /// fails before any process starts.
    #[test]
    fn a_conversation_with_an_image_starts_no_process() {
        let fake = Fake::new("text");
        let image = ImageSource::new(ImageMediaType::Png, "AAAA".into());
        let messages = [Message::user_with_images("look".into(), vec![image])];
        let limits = limits(STARTUP, IDLE, HANDOFF, EXIT_LIMIT);
        let (result, _) =
            smol::block_on(fake.send_messages(&messages, ALIAS, Thinking::Default, limits));
        assert!(matches!(result, Err(Error::Image)), "{result:?}");
        assert_eq!(fake.log("versions"), "", "no process must start");
    }

    /// A stderr line that is not UTF-8 must not stop the reader, or Claude
    /// Code blocks once its stderr fills the pipe.
    #[test]
    fn a_stderr_line_that_is_not_utf8_keeps_stderr_flowing() {
        let fake = Fake::new("noisy_stderr");
        let (result, _) = smol::block_on(fake.request());
        assert!(result.is_ok(), "{result:?}");
    }

    /// A reply whose output closed but whose process never exits stops at
    /// the exit limit, and so does the process.
    #[test_case("text_then_linger" ; "closed_stdout")]
    #[test_case("text_then_slow_stdout" ; "open_stdout")]
    #[test_case("text_then_detached_stdout" ; "stdout_open_after_kill")]
    fn a_complete_reply_survives_a_cli_that_does_not_exit(scenario: &str) {
        let fake = Fake::new(scenario);
        let (result, _) = smol::block_on(fake.request_with(
            &format!("find {MARKER}"),
            ALIAS,
            limits(STARTUP, IDLE, HANDOFF, SHORT_EXIT),
        ));
        assert!(
            fake.group_gone(),
            "a stopped run must not continue after its request"
        );
        let response = result.unwrap();
        assert!(
            matches!(response.message.content.as_slice(), [ContentBlock::Text { text }] if text == "Reading.")
        );
        assert_eq!(fake.log("calls").lines().count(), 2);
    }

    /// A process that ignores stdin after its checks must not hold a request indefinitely.
    #[test]
    fn a_prompt_nobody_reads_times_out() {
        let fake = Fake::new("stops_reading");
        let prompt = "x".repeat(UNREAD_PROMPT_BYTES);
        let (result, _) = smol::block_on(fake.request_with(
            &prompt,
            ALIAS,
            limits(UNREAD_STARTUP, IDLE, HANDOFF, EXIT_LIMIT),
        ));
        let err = result.unwrap_err();
        assert!(matches!(err.cause(), Error::InputNotTaken), "{err}");
        assert!(
            fake.group_gone(),
            "a run that hangs must not continue after its request"
        );
    }

    /// Keep-alives and tool progress come from a timer, so they do not show
    /// that work continues. A generation that sends only those stops at the
    /// idle limit. After a reply, only the held call or the result is missing, and
    /// it comes immediately. Only the generation runs with a short idle
    /// limit, so a slow start cannot end the other two early.
    #[test_case("stalled_generation", SHORT_IDLE => matches Error::Stalled(_) ; "a_generation")]
    #[test_case("progress_generation", SHORT_IDLE => matches Error::Stalled(_) ; "a_generation_with_tool_progress")]
    #[test_case("stalled_handoff", IDLE => matches Error::HandoffLate(_) ; "a_handoff")]
    #[test_case("stalled_result_without_usage", IDLE => matches Error::ResultLate(_) ; "a_finished_reply_without_its_final_count")]
    fn keep_alives_cannot_hold_a_reply_open(scenario: &str, idle: Duration) -> Error {
        let fake = Fake::new(scenario);
        let result = through_keep_alives(&fake, limits(STARTUP, idle, SHORT_HANDOFF, SHORT_EXIT));

        assert!(
            fake.group_gone(),
            "a run that hangs must not continue after its request"
        );
        let error = result.unwrap_err();
        match into_cause(error) {
            Error::Interrupted(error) => *error,
            error => error,
        }
    }

    /// maki continues a reply that hit its limit itself. Claude Code would
    /// continue with a prompt of its own, so it is stopped there.
    #[test]
    fn a_reply_at_its_output_cap_comes_back_cut() {
        let fake = Fake::new("truncated");
        let (result, _) = smol::block_on(fake.request());
        let response = result.unwrap();

        assert_eq!(response.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(response.message.first_text_content(), Some(REPLY_TEXT));
        assert!(
            fake.group_gone(),
            "Claude Code must not continue after the limit"
        );
    }

    #[test]
    fn a_context_overflow_omits_makis_own_kill_status() {
        let fake = Fake::new("context_overflow");
        let error = smol::block_on(fake.request()).0.unwrap_err();
        assert!(
            matches!(&error, Error::ContextOverflow)
                || matches!(&error, Error::WithStderr { error, .. } if matches!(**error, Error::ContextOverflow)),
            "{error:?}"
        );
        assert!(fake.group_gone());
    }

    fn limits(startup: Duration, idle: Duration, handoff: Duration, exit: Duration) -> Limits {
        Limits {
            startup,
            idle,
            handoff,
            exit,
        }
    }

    /// A call can reach the server just as the output ends. stdout is read
    /// first, so the call is still queued then.
    #[test]
    fn a_call_still_queued_at_the_end_is_checked() {
        let (catalog, _) =
            Catalog::new(&json!([{ "name": TOOL, "input_schema": { "type": "object" } }])).unwrap();
        let tools = catalog.exposed();
        let profile = profile("2.1.284").unwrap();
        let expect = InitExpect {
            version: &profile,
            model: ALIAS,
            cwd: Path::new(ANY_CWD),
            tools: &tools,
        };
        let plan_usage = Mutex::default();
        let mut turn = Turn::new(&catalog, expect, &plan_usage);
        let (sender, handoffs) = flume::unbounded();
        sender
            .send(Handoff::Parked {
                tool_use_id: Some(MARKER.into()),
                name: UNKNOWN_TOOL.into(),
                arguments: json!({}),
            })
            .unwrap();
        drop(sender);
        let result = smol::block_on(queued_handoffs(&mut turn, &handoffs, Instant::now() + WAIT));
        assert!(
            matches!(result, Err(Error::NotOffered(ref name)) if name == UNKNOWN_TOOL),
            "{result:?}"
        );
    }

    /// Only maki can compact or retry the conversation. Held calls must wait until maki stops
    /// the request. Keep cache marks on the transcript.
    #[test]
    fn claude_code_neither_compacts_nor_retries_on_its_own() {
        let fake = Fake::new("text");
        smol::block_on(fake.request()).0.unwrap();

        assert_eq!(
            fake.log("nonstreaming_fallback"),
            NO_NONSTREAMING_FALLBACK.1
        );
        assert_eq!(fake.log("auto_compact"), NO_AUTO_COMPACT.1);
        assert_eq!(fake.log("max_retries"), NO_CLI_RETRIES.1);
        let mcp: Value = serde_json::from_str(&fake.log("mcp_config")).unwrap();
        assert_eq!(mcp["mcpServers"][SERVER]["timeout"], HELD_CALL_TIMEOUT_MS);
        assert!(
            fake.log("argv").contains(NO_AUTO_COMPACT_SETTING),
            "{}",
            fake.log("argv")
        );
        assert_eq!(fake.log("capabilities"), NO_MID_CONVERSATION_SYSTEM.1);
    }

    /// Every error carries what Claude Code printed on stderr, version check
    /// errors included.
    #[test]
    fn a_failed_version_check_says_what_claude_code_printed() {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join("version_error"), VERSION_ERROR).unwrap();
        let err = smol::block_on(fake.request()).0.unwrap_err();
        assert!(
            matches!(&err, Error::WithStderr { error, stderr } if matches!(**error, Error::VersionFailed(_)) && stderr == VERSION_ERROR.trim_end()),
            "{err:?}"
        );
    }

    #[test]
    fn an_aborted_version_check_names_the_command_and_signal() {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join("version_abort"), "").unwrap();
        let error = smol::block_on(fake.request()).0.unwrap_err();
        assert!(
            matches!(error.cause(), Error::VersionFailed(status) if status.signal() == Some(rustix::process::Signal::ABORT.as_raw()))
        );
        assert!(error.to_string().contains("claude --version"), "{error}");
    }

    /// An unreadable version line and a missing one give different errors.
    #[test]
    fn an_unreadable_version_line_is_named() {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join("version"), NOT_UTF8_VERSION).unwrap();
        let err = smol::block_on(fake.request()).0.unwrap_err();
        let unreadable = |error: &Error| matches!(error, Error::UnreadableVersion(source) if source.kind() == io::ErrorKind::InvalidData);
        assert!(unreadable(err.cause()), "{err:?}");
    }

    /// Output that ends early while the process lingers, and a process that
    /// exited before the reply, give different errors.
    #[test]
    fn output_that_ends_without_an_exit_is_named() {
        let fake = Fake::new("ends_output");
        let (result, _) = smol::block_on(fake.request_with(
            &format!("find {MARKER}"),
            ALIAS,
            limits(STARTUP, IDLE, HANDOFF, SHORT_EXIT),
        ));
        assert!(
            matches!(result.as_ref().map_err(Error::cause), Err(Error::Interrupted(error)) if matches!(**error, Error::WentQuiet)),
            "{result:?}"
        );
    }

    /// Every maki thinking setting reaches Claude Code: an effort level on the
    /// command line, a budget or "off" in its environment. Any thinking comes
    /// back as a summary, so a long turn shows what the model thinks.
    #[test_case(Thinking::Default, None, UNSET, UNSET ; "the_default")]
    #[test_case(Thinking::Effort(EFFORT), Some(EFFORT), UNSET, UNSET ; "an_effort")]
    #[test_case(Thinking::Budget(BUDGET), None, "4096", UNSET ; "a_budget")]
    #[test_case(Thinking::Off, None, UNSET, NO_THINKING.1 ; "off")]
    fn thinking_reaches_claude_code(
        thinking: Thinking,
        effort: Option<&str>,
        budget: &str,
        off: &str,
    ) {
        let fake = Fake::new("text");
        smol::block_on(fake.send_messages(
            &[Message::user(format!("find {MARKER}"))],
            ALIAS,
            thinking,
            limits(STARTUP, IDLE, HANDOFF, EXIT_LIMIT),
        ))
        .0
        .unwrap();

        let argv: Vec<String> = fake.log("argv").lines().map(str::to_owned).collect();
        let value_of = |flag: &str| {
            argv.iter()
                .position(|arg| arg == flag)
                .map(|at| argv[at + 1].as_str())
        };
        assert_eq!(value_of(EFFORT_FLAG), effort);
        assert_eq!(value_of(SHOWN_THINKING.0), Some(SHOWN_THINKING.1));
        assert_eq!(fake.log("thinking_budget"), budget);
        assert_eq!(fake.log("thinking_off"), off);
    }

    /// A cancelled request drops its future, which must stop the process
    /// group and remove the request's private directory.
    #[test]
    fn a_dropped_request_kills_its_process_group() {
        let fake = Fake::new("hang");
        let hanging = fake.dir.path().join("hanging");
        let finished = smol::block_on(async {
            let request = async { Some(fake.request().await) };
            let dropped = async {
                while !hanging.exists() {
                    Timer::after(POLL).await;
                }
                None
            };
            request.or(dropped).await
        });
        assert!(finished.is_none(), "the request must continue to run");
        assert!(
            fake.group_gone(),
            "a drop of the request must kill Claude Code"
        );
        let base = fake.temp_base();
        assert!(
            wait_until(|| fs::read_dir(&base).unwrap().next().is_none()),
            "{:?}",
            fs::read_dir(&base).unwrap().collect::<Vec<_>>()
        );
    }

    /// A signal can kill maki before any destructor runs, so Claude Code must
    /// die with maki rather than with the request. The test binary runs
    /// itself as that maki, with `TMPDIR` in a dir this test owns.
    #[test_case(Signal::TERM ; "sigterm")]
    #[test_case(Signal::KILL ; "sigkill")]
    fn a_killed_maki_takes_claude_code_along(signal: Signal) {
        if env::var_os(MAKI_ROLE).is_some() {
            let _ = smol::block_on(Fake::new("hang").request());
            return;
        }
        let temp = tempdir().unwrap();
        let current = thread::current();
        let mut maki = process::Command::new(env::current_exe().unwrap())
            .args(["--exact", current.name().unwrap()])
            .env(MAKI_ROLE, "1")
            .env("TMPDIR", temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let hanging = || {
            fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|dir| dir.join("hanging").exists())
        };
        assert!(wait_until(|| hanging().is_some()), "Claude Code never ran");
        let fake_dir = hanging().unwrap();
        let pid = fs::read_to_string(fake_dir.join("pid")).unwrap();
        let pid = Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
        let member = fs::read_to_string(fake_dir.join(MEMBER_PID)).unwrap();
        let running = || {
            fs::read(format!("/proc/{}/cmdline", member.trim()))
                .is_ok_and(|bytes| !bytes.is_empty())
        };
        assert!(wait_until(running), "the fake never started its descendant");

        let maki_pid = Pid::from_raw(maki.id().try_into().unwrap()).unwrap();
        kill_process(maki_pid, signal).unwrap();
        maki.wait().unwrap();

        let gone = wait_until(|| test_kill_process_group(pid).is_err());
        let _ = kill_process_group(pid, Signal::KILL);
        assert!(gone, "Claude Code outlived maki");
        assert!(!running(), "a descendant outlived maki");
        let temp_base = fake_dir.join(TEMP_BASE);
        sweep_dead_owners(&temp_base);
        let left: Vec<_> = fs::read_dir(temp_base).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// Only a directory whose maki is gone goes, and only one from this pid
    /// namespace and boot, where its pid names the same maki. A link to a directory
    /// stays, and so does what it points to.
    #[test]
    fn a_dead_makis_private_dir_is_swept() {
        let base = tempdir().unwrap();
        let mut exited = process::Command::new("true").spawn().unwrap();
        exited.wait().unwrap();
        let namespace = owner_namespace().unwrap();
        let named = |namespace: &str, pid: u32, tail: &str| {
            base.path().join(format!(
                "{DIR_PREFIX}{namespace}{OWNER_SEPARATOR}{pid}.{tail}"
            ))
        };
        let dead = named(namespace, exited.id(), "x");
        let alive = named(namespace, process::id(), "x");
        let elsewhere = named(&format!("{namespace}-other"), exited.id(), "x");
        let (boot, pid_namespace) = namespace.rsplit_once(OWNER_SEPARATOR).unwrap();
        let other_boot = named(&format!("{boot}-other-{pid_namespace}"), exited.id(), "x");
        let other = base.path().join("unrelated");
        let target = tempdir().unwrap();
        for dir in [&dead, &alive, &elsewhere, &other_boot, &other] {
            fs::create_dir(dir).unwrap();
            fs::write(dir.join("mcp.json"), "{}").unwrap();
        }
        let link = named(namespace, exited.id(), "link");
        symlink(target.path(), &link).unwrap();

        sweep_dead_owners(base.path());

        assert!(!dead.exists(), "the dead maki's directory stayed");
        for kept in [&alive, &elsewhere, &other_boot, &other, &link] {
            assert!(kept.exists(), "{kept:?} went");
        }
    }

    /// The probe sends no prompt, so a run it starts can only be Claude Code
    /// acting on its own.
    #[test]
    fn a_probe_that_starts_a_run_is_refused() {
        let fake = Fake::new("probe_runs");
        let result = smol::block_on(probe(
            &super::Launch {
                executable: &fake.executable(),
                env: &Fake::env(),
                project: &fake.project(),
                denied: &[],
                temp_dir: &env::temp_dir(),
                limits: &Limits::new(IDLE),
            },
            &fake.plan_usage,
        ));
        assert!(
            fake.log("stdin_prompt").is_empty(),
            "a probe sends no prompt"
        );
        assert!(matches!(
            result.as_ref().map_err(Error::cause),
            Err(Error::UncheckedStart)
        ));
    }

    /// The models come from the account answer, each once, by the id it runs
    /// and with the window Claude Code opens for it. Model discovery sends no prompt,
    /// and the list is still correct without the windows.
    #[test_case(true ; "with_their_windows")]
    #[test_case(false ; "without_windows")]
    fn the_listed_models_are_what_the_account_offers(windows: bool) {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join(TRACK_SESSIONS), "").unwrap();
        if !windows {
            fs::write(fake.dir.path().join(NO_WINDOWS), "").unwrap();
        }
        let listed = smol::block_on(models(
            &Launch {
                executable: &fake.executable(),
                env: &Fake::env(),
                project: &fake.project(),
                denied: &[],
                temp_dir: &env::temp_dir(),
                limits: &Limits::new(IDLE),
            },
            &fake.plan_usage,
        ))
        .unwrap();

        assert!(
            fs::read_dir(fake.dir.path().join(SESSIONS))
                .unwrap()
                .next()
                .is_none()
        );
        let window = |size| windows.then_some(size);
        assert_eq!(
            listed,
            [
                Listed {
                    id: OPUS.to_owned(),
                    window: window(WIDE_WINDOW),
                },
                Listed {
                    id: HAIKU.to_owned(),
                    window: window(STANDARD_WINDOW),
                },
            ]
        );
        assert!(
            fake.log("stdin_prompt").is_empty(),
            "a model list sends no prompt"
        );
    }

    /// maki's `-1m` id asks for the 1M window in Claude Code's name format.
    /// The generation reports the model without either suffix.
    #[test]
    fn a_1m_id_asks_for_the_long_window() {
        let fake = Fake::new("text");
        let (result, _) = smol::block_on(fake.request_with(
            &format!("find {MARKER}"),
            LONG_CONTEXT_ID,
            limits(STARTUP, IDLE, HANDOFF, EXIT_LIMIT),
        ));

        result.unwrap();
        let argv = fake.log("argv");
        let argv: Vec<&str> = argv.lines().collect();
        assert!(
            argv.windows(2).any(|w| w == ["--model", LONG_CONTEXT_ARG]),
            "{argv:?}"
        );
    }

    /// A request runs only the model it names, an alias's model, or a dated
    /// snapshot of either. The other requests run the alias and the bare id.
    #[test]
    fn a_reply_from_another_model_stops_the_request() {
        let fake = Fake::new("text");
        let (result, _) = smol::block_on(fake.request_with(
            &format!("find {MARKER}"),
            OTHER_MODEL,
            limits(STARTUP, IDLE, HANDOFF, EXIT_LIMIT),
        ));

        assert!(
            matches!(
                result.as_ref().map_err(Error::cause),
                Err(Error::OtherModel { .. })
            ),
            "{result:?}"
        );
    }

    /// Policy can turn on a startup hook between two requests. The next probe
    /// sees it run, so Claude Code never starts in the project.
    #[test]
    fn a_hook_policy_turned_on_between_requests_never_reaches_the_project() {
        let fake = Fake::new("text");
        smol::block_on(fake.request()).0.unwrap();
        fs::write(fake.dir.path().join("policy_hook"), "").unwrap();
        let starts = fake.log("calls").lines().count();
        let err = smol::block_on(fake.request()).0.unwrap_err();

        assert!(matches!(err.cause(), Error::Check(_)), "{err}");
        let ran_in = fake.log("hook_ran_in");
        assert!(
            !ran_in.is_empty(),
            "the hook did not run, so this test proves nothing"
        );
        assert!(
            ran_in.lines().all(|dir| Path::new(dir) != fake.project()),
            "the hook ran in the project"
        );
        assert_eq!(
            fake.log("calls").lines().count(),
            starts + 1,
            "only the probe can start"
        );
        assert_eq!(fake.log("versions").lines().count(), 1);
    }

    #[test]
    fn an_unchanged_executable_reuses_its_checked_version() {
        let fake = Fake::new("text");
        for _ in 0..2 {
            smol::block_on(fake.request()).0.unwrap();
        }
        assert_eq!(fake.log("versions").lines().count(), 1);
        assert_eq!(fake.log("calls").lines().count(), 4);
    }

    #[test_case(NEWER_VERSION, true ; "an_upgrade_runs")]
    #[test_case(OLDER_VERSION, false ; "a_downgrade_never_starts")]
    fn a_version_changed_between_requests(version: &str, runs: bool) {
        let fake = Fake::new("text");
        smol::block_on(fake.request()).0.unwrap();
        fs::write(fake.dir.path().join("version"), version).unwrap();
        let script = fs::read(fake.executable()).unwrap();
        let replacement = fake.executable().with_extension("new");
        fs::write(&replacement, script).unwrap();
        fs::set_permissions(
            &replacement,
            fs::metadata(fake.executable()).unwrap().permissions(),
        )
        .unwrap();
        fs::rename(replacement, fake.executable()).unwrap();
        let starts = fake.log("calls").lines().count();
        let result = smol::block_on(fake.request()).0;

        if runs {
            result.unwrap();
        } else {
            let err = result.unwrap_err();
            assert!(matches!(err, Error::TooOld { .. }), "{err}");
            assert_eq!(
                fake.log("calls").lines().count(),
                starts,
                "the version below the minimum started"
            );
        }
    }

    /// Plan usage reaches maki's view even when it arrives after the result,
    /// while maki drains the run.
    #[test]
    fn plan_usage_reaches_the_usage_view() {
        let fake = Fake::new("text_then_limits");
        smol::block_on(fake.request()).0.unwrap();

        let usage = fake
            .plan_usage
            .lock()
            .unwrap()
            .clone()
            .expect("no plan usage");
        let shown: Vec<(&str, Option<u32>, Option<u64>)> = usage
            .limits
            .iter()
            .map(|limit| (limit.label.as_str(), limit.percentage, limit.reset_at))
            .collect();
        assert_eq!(
            shown,
            [
                (LABEL_SESSION, Some(SESSION_USED), Some(SESSION_RESET_MS)),
                (LABEL_WEEK_ALL, Some(WEEK_USED), Some(WEEK_RESET_MS)),
            ]
        );
    }

    /// The unreaped leader reserves the group pid. Kill remaining group members before the reap.
    #[test]
    fn a_leader_that_exits_takes_its_group_along() {
        let here = env::current_dir().unwrap();
        let args = ["-c".to_owned(), LINGERING_MEMBER.to_owned()];
        let shell = command(Path::new("/bin/sh"), &Fake::env(), &here, &args);
        let mut group = smol::block_on(Group::spawn(shell)).unwrap();
        let (_stdin, mut lines) = group.pipes().unwrap();
        let member: String = smol::block_on(lines.next()).unwrap().unwrap();
        let status = smol::block_on(group.wait(Instant::now() + WAIT)).unwrap();
        assert!(status.success());
        let running = || fs::read(format!("/proc/{member}/cmdline")).is_ok_and(|c| !c.is_empty());
        assert!(
            wait_until(|| !running()),
            "the process in the group continued after its leader"
        );
    }
}
