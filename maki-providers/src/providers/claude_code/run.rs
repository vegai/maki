//! Each request owns a Claude Code process group. Stop the group before the handoff server so
//! held calls retain their connection during cleanup.

use std::collections::VecDeque;
use std::env;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(unix)]
use std::fs::Permissions;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use flume::{Receiver, Sender};
use futures::future::join_all;
use futures_lite::future;
use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use futures_lite::{FutureExt, Stream, StreamExt};
#[cfg(target_os = "linux")]
use rustix::io::Errno;
#[cfg(target_os = "linux")]
use rustix::process::{Pid, getuid, test_kill_process};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::process::{Child, ChildStdin, ChildStdout, Command};
use smol::{Task, Timer};
use tempfile::TempDir;
use tracing::{debug, info, warn};

use super::checks::{self, InitExpect, Profile, RULES};
use super::error::Error;
#[cfg(target_os = "linux")]
use super::guard;
use super::mcp::{self, Handoff};
use super::stream::{
    CONTROL_RESPONSE, INITIALIZE, Offered, SUCCESS, Step, Turn, control_request, user_message,
};
use super::transcript::{Catalog, SERVER, system_prompt, transcript};
use crate::process::kill_group;
#[cfg(unix)]
use crate::process::wait_without_reaping;
use crate::providers::anthropic::shared::{long_context_window, strip_long_context};
use crate::{Message, ProviderEvent, ProviderUsage, StreamResponse};

const DIR_PREFIX: &str = "maki-claude-code.";
/// Between the pid namespace and the pid in a private directory's name.
#[cfg(target_os = "linux")]
const OWNER_SEPARATOR: char = '-';
#[cfg(target_os = "linux")]
const PID_NAMESPACE: &str = "/proc/self/ns/pid";
const PROMPT_FILE: &str = "system-prompt.md";
const MCP_FILE: &str = "mcp.json";
const TOKEN_BYTES: usize = 32;
/// How long a killed process's last stderr lines get to reach the error.
const STDERR_DRAIN: Duration = Duration::from_secs(1);
const STDERR_TAIL_LINES: usize = 10;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
const WINDOW_PROCESSES: usize = 4;
const SET_MODEL: &str = "set_model";
const CONTEXT_USAGE: &str = "get_context_usage";
/// What Claude Code appends to a model name for its 1M window.
const LONG_CONTEXT_NAME: &str = "[1m]";
/// The probe sends no prompt, so its model never runs.
const PROBE_MODEL: &str = "haiku";
/// Generous, because Claude Code starts slowly.
const STARTUP: Duration = Duration::from_secs(60);
/// The held call arrives right after a completed reply, or not at all.
const HANDOFF: Duration = Duration::from_secs(30);
const EXIT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_ENV: &str = "CLAUDE_CODE_MAX_OUTPUT_TOKENS";
const EFFORT_FLAG: &str = "--effort";
const MCP_CONFIG_FLAG: &str = "--mcp-config";
const SYSTEM_PROMPT_FILE_FLAG: &str = "--system-prompt-file";
const VERSION_FLAG: &str = "--version";
const THINKING_BUDGET_ENV: &str = "MAX_THINKING_TOKENS";
const NO_THINKING: (&str, &str) = ("CLAUDE_CODE_DISABLE_THINKING", "1");
/// Otherwise the thinking of a reply reaches maki empty, and a long turn shows
/// nothing until its text. A model that always thinks does so even with
/// thinking off, so the flag goes on every run.
const SHOWN_THINKING: (&str, &str) = ("--thinking-display", "summarized");
/// maki owns the history and compacts it itself.
const NO_AUTO_COMPACT: (&str, &str) = ("DISABLE_AUTO_COMPACT", "1");
/// Otherwise Claude Code can resend a failed request without stream events.
const NO_NONSTREAMING_FALLBACK: (&str, &str) = ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1");
/// maki retries a temporary API error itself. Claude Code's own retry would
/// start a second generation inside one stream, which maki cannot tell from a
/// broken stream.
const NO_CLI_RETRIES: (&str, &str) = ("CLAUDE_CODE_MAX_RETRIES", "0");
/// On Claude 5 models, Claude Code puts the cache mark on its final system message. Disable
/// that message so later requests reuse transcript blocks.
const NO_MID_CONVERSATION_SYSTEM: (&str, &str) =
    ("CLAUDE_CODE_MODEL_CAPABILITIES", "-mid_conv_system");
/// Claude Code loads nothing on its own, and only maki's handoff server can
/// receive tool calls.
const SETTINGS: &str = r#"{"disableAllHooks":true,"autoMemoryEnabled":false,"autoCompactEnabled":false,"claudeMdExcludes":["**"],"disableClaudeAiConnectors":true,"permissions":{"allow":["mcp__maki"]}}"#;
const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;
/// Claude Code starts tool calls before the reply ends. maki holds them until the complete
/// reply arrives.
///
/// The HTTP MCP client has short call and server timeouts. Use the largest accepted server
/// timeout so the client cannot answer held calls.
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

/// Turns off every Claude Code tool, sessions, settings files, and every MCP
/// server except maki's.
fn base_args(model: &str) -> Vec<String> {
    [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
        "--restricted",
        "--tools",
        "",
        "--max-turns",
        "1",
        "--permission-mode",
        "default",
        "--permission-prompts",
        "none",
        "--setting-sources",
        "",
        "--settings",
        SETTINGS,
        "--strict-mcp-config",
        "--model",
        model,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// A model Claude Code offers, and the context window it opens for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Listed {
    pub id: String,
    pub window: Option<u32>,
}

pub(crate) struct Request<'a> {
    pub executable: &'a Path,
    pub env: &'a [(String, String)],
    pub model: &'a str,
    pub cwd: &'a Path,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a Value,
    pub events: &'a Sender<ProviderEvent>,
    pub plan_usage: &'a Mutex<Option<ProviderUsage>>,
    /// `$TMPDIR` as maki sees it.
    pub temp_dir: &'a Path,
    pub max_output: Option<u32>,
    pub thinking: Thinking,
    pub limits: &'a Limits,
    /// How long the request waited for a free slot, for the log.
    pub slot_wait: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinking {
    Default,
    Off,
    /// For a model with adaptive thinking.
    Effort(&'static str),
    Budget(u32),
}

/// Kills the group on drop unless the leader is already reaped.
struct Group {
    child: Child,
    reaped: bool,
    #[cfg(target_os = "linux")]
    _lifetime: UnixStream,
}

impl Group {
    async fn spawn(command: process::Command) -> Result<Self, Error> {
        smol::unblock(move || spawn_piped(command))
            .await
            .map_err(|source| Error::Io {
                what: "start Claude Code",
                source,
            })
    }

    /// Only while the unreaped leader's pid still names the group.
    fn kill(&self) {
        if !self.reaped {
            kill_group(self.child.id());
        }
    }

    /// Kill remaining group members before the leader reap so the pid cannot identify another
    /// group.
    async fn wait(&mut self, deadline: Instant) -> Result<ExitStatus, Error> {
        let exited = self.exited_by(deadline).await;
        self.kill();
        let status = self.child.status().await;
        self.reaped = true;
        if !exited {
            return Err(Error::ExitLate);
        }
        status.map_err(|source| Error::Io {
            what: "reap Claude Code",
            source,
        })
    }

    /// Does not reap the leader.
    #[cfg(unix)]
    async fn exited_by(&self, deadline: Instant) -> bool {
        let pid = self.child.id();
        smol::unblock(move || wait_without_reaping(pid))
            .or(async {
                Timer::at(deadline).await;
                false
            })
            .await
    }

    /// The open process handle keeps the pid for this process.
    #[cfg(not(unix))]
    async fn exited_by(&mut self, deadline: Instant) -> bool {
        async { self.child.status().await.is_ok() }
            .or(async {
                Timer::at(deadline).await;
                false
            })
            .await
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
        reaped: false,
        #[cfg(target_os = "linux")]
        _lifetime: lifetime,
    })
}

fn command(
    executable: &Path,
    env: &[(String, String)],
    cwd: &Path,
    args: &[String],
) -> process::Command {
    let mut command = process::Command::new(executable);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)));
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Creates a file that only the user can read and write, from the moment it
/// exists.
fn private_file(path: &Path, content: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(PRIVATE_FILE_MODE);
    let mut file: File = options.open(path)?;
    file.write_all(content.as_bytes())
}

/// Private to the user from the moment it exists. Returns the resolved path,
/// because a link on the way to `base` could make a directory inside
/// `project` look like it is outside.
fn private_dir(base: &Path, project: &Path) -> Result<(TempDir, PathBuf), Error> {
    #[cfg(target_os = "linux")]
    sweep_once(base);
    let prefix = dir_prefix();
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix);
    #[cfg(unix)]
    builder.permissions(Permissions::from_mode(PRIVATE_DIR_MODE));
    let dir = builder.tempdir_in(base).map_err(|source| Error::Path {
        what: "make a private directory in",
        path: base.to_owned(),
        source,
    })?;
    let path = dir.path().canonicalize().map_err(|source| Error::Path {
        what: "resolve",
        path: dir.path().to_owned(),
        source,
    })?;
    if path.starts_with(project) {
        return Err(Error::TempInProject(path));
    }
    Ok((dir, path))
}

/// Include the pid namespace in the directory name. A later maki process can then identify
/// dead owners with pids scoped to their own namespace.
fn dir_prefix() -> String {
    #[cfg(target_os = "linux")]
    if let Some(namespace) = pid_namespace() {
        return format!("{DIR_PREFIX}{namespace}{OWNER_SEPARATOR}{}.", process::id());
    }
    DIR_PREFIX.to_owned()
}

#[cfg(target_os = "linux")]
fn pid_namespace() -> Option<u64> {
    fs::metadata(PID_NAMESPACE).ok().map(|meta| meta.ino())
}

/// Artifact cleanup can take a long time. Run it outside the async threads and scan each
/// base once.
#[cfg(target_os = "linux")]
fn sweep_once(base: &Path) {
    static SWEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut swept = SWEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if swept.iter().any(|dir| dir == base) {
        return;
    }
    swept.push(base.to_owned());
    let base = base.to_owned();
    smol::unblock(move || sweep_dead_owners(&base)).detach();
}

/// Removes the private directories in `base` whose maki is gone. A signal
/// can kill maki before the directory's destructor runs. Only the user's own
/// directories from this pid namespace go, and a link is never followed.
#[cfg(target_os = "linux")]
fn sweep_dead_owners(base: &Path) {
    let (Ok(entries), Some(namespace)) = (fs::read_dir(base), pid_namespace()) else {
        return;
    };
    let uid = getuid().as_raw();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(owner) = name
            .to_str()
            .and_then(|name| name.strip_prefix(DIR_PREFIX))
            .and_then(|rest| rest.split_once('.'))
            .and_then(|(owner, _)| owner.split_once(OWNER_SEPARATOR))
            .filter(|(owner_namespace, _)| owner_namespace.parse() == Ok(namespace))
            .and_then(|(_, pid)| pid.parse().ok())
            .and_then(Pid::from_raw)
        else {
            continue;
        };
        let path = entry.path();
        let owned =
            fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && meta.uid() == uid);
        if !owned || test_kill_process(owner) != Err(Errno::SRCH) {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => debug!(
                path = %path.display(),
                owner = owner.as_raw_nonzero(),
                "claude-code: removed a dead maki's private directory"
            ),
            Err(error) => warn!(
                path = %path.display(),
                %error,
                "claude-code: could not remove a dead maki's private directory"
            ),
        }
    }
}

fn token() -> Result<String, Error> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(Error::Token)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
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
async fn supervised<T>(
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
            let mut reader = BufReader::new(stderr).split(b'\n');
            while let Some(Ok(line)) = reader.next().await {
                let line =
                    String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line)).into_owned();
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

fn unreadable(source: io::Error) -> Error {
    Error::Io {
        what: "read the output of Claude Code",
        source,
    }
}

enum Next {
    Line(Option<io::Result<String>>),
    Handoff(Handoff),
    Late,
}

async fn next(
    lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    handoffs: Option<&Receiver<Handoff>>,
    deadline: Instant,
) -> Next {
    async { Next::Line(lines.next().await) }
        .or(async {
            match handoffs {
                Some(handoffs) => match handoffs.recv_async().await {
                    Ok(handoff) => Next::Handoff(handoff),
                    Err(_) => future::pending().await,
                },
                None => future::pending().await,
            }
        })
        .or(async {
            Timer::at(deadline).await;
            Next::Late
        })
        .await
}

fn stdout_lines(stdout: Option<ChildStdout>) -> Result<Lines<BufReader<ChildStdout>>, Error> {
    stdout
        .map(|out| BufReader::new(out).lines())
        .ok_or(Error::NoStdout)
}

/// Sends the check requests, which Claude Code answers before it reads a
/// prompt.
async fn send_handshake(stdin: &mut Option<ChildStdin>, deadline: Instant) -> Result<(), Error> {
    for step in &RULES.handshake {
        let request = control_request(&step.id, &json!({ "subtype": step.subtype }));
        send(stdin, &request, deadline).await?;
    }
    Ok(())
}

/// A process that ignores stdin can block a write larger than the pipe buffer. Bound the
/// write with a deadline.
async fn send(stdin: &mut Option<ChildStdin>, data: &str, deadline: Instant) -> Result<(), Error> {
    let pipe = stdin.as_mut().ok_or(Error::NoStdin)?;
    async {
        pipe.write_all(data.as_bytes())
            .await
            .map_err(|source| Error::Io {
                what: "write to Claude Code",
                source,
            })
    }
    .or(async {
        Timer::at(deadline).await;
        Err(Error::InputNotTaken)
    })
    .await
}

/// Versions and policy can change between requests. Validate both before a process starts in
/// the project.
///
/// Retry only temporary API errors and stalled replies. Other failures can occur after Claude
/// Code sends the request.
pub(crate) async fn request(req: Request<'_>) -> Result<StreamResponse, Error> {
    let started = Instant::now();
    let limits = req.limits;
    // Before any process starts, so a conversation maki cannot send costs
    // none.
    let catalog = Catalog::new(req.tools)?;
    let conversation = user_message(&transcript(req.messages)?);
    let profile = checked_profile(
        req.executable,
        req.env,
        req.cwd,
        req.temp_dir,
        limits.startup,
    )
    .await?;
    let offered = probe(
        req.executable,
        &profile,
        req.env,
        req.cwd,
        req.temp_dir,
        req.plan_usage,
        limits.startup,
    )
    .await?;
    // maki marks the 1M window with a `-1m` id and Claude Code with a `[1m]`
    // name. The generation reports the model without either.
    let asked = strip_long_context(req.model);
    let model_arg = match long_context_window(req.model) {
        Some(_) => format!("{asked}{LONG_CONTEXT_NAME}"),
        None => asked.to_owned(),
    };
    // An alias such as `sonnet` runs whatever model the account answer maps
    // it to.
    let runs = offered
        .iter()
        .find(|offer| offer.name == asked)
        .map_or(asked, |offer| offer.model.as_str());
    let checked = started.elapsed();
    debug!(
        version = %profile.version,
        checks_ms = millis(checked),
        "claude-code: checks passed"
    );
    let (_dir, dir_path) = private_dir(req.temp_dir, req.cwd)?;
    let (handoff_tx, handoffs) = flume::unbounded();
    let server = mcp::serve(token()?, catalog.tools.clone(), handoff_tx, limits.startup)
        .await
        .map_err(|source| Error::Io {
            what: "start the handoff server",
            source,
        })?;
    let prompt_file = dir_path.join(PROMPT_FILE);
    let mcp_file = dir_path.join(MCP_FILE);
    let mcp_config = json!({ "mcpServers": { SERVER: {
        "type": "http",
        "url": server.url(),
        "headers": { "Authorization": server.authorization() },
        "timeout": HELD_CALL_TIMEOUT_MS,
    } } });
    private_file(&prompt_file, &system_prompt(req.system))
        .and_then(|()| private_file(&mcp_file, &mcp_config.to_string()))
        .map_err(|source| Error::Io {
            what: "write the request files",
            source,
        })?;
    // Keep the server outside the request future so process cleanup finishes first. Held
    // calls must retain their connection until Claude Code stops.
    let mut server = Some(server);
    let (args, env) = run_args(&req, &model_arg, &mcp_file, &prompt_file);
    let tools = catalog.exposed();
    let run = Run {
        turn: Turn::new(
            &catalog,
            InitExpect {
                profile: &profile,
                model: runs,
                cwd: req.cwd,
                server: SERVER,
                tools: &tools,
            },
            req.plan_usage,
        ),
        conversation: &conversation,
        handoffs,
        events: req.events,
        limits,
    };
    let ran = Instant::now();
    let (response, prompted) = supervised(
        command(req.executable, &env, req.cwd, &args),
        async |group| converse(group, run, &mut server).await,
    )
    .await?;
    let usage = &response.usage;
    info!(
        model = req.model,
        version = %profile.version,
        slot_wait_ms = millis(req.slot_wait),
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

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// The arguments and environment of a request's run.
fn run_args(
    req: &Request<'_>,
    model_arg: &str,
    mcp_file: &Path,
    prompt_file: &Path,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut args = base_args(model_arg);
    args.extend([
        MCP_CONFIG_FLAG.to_owned(),
        mcp_file.display().to_string(),
        SYSTEM_PROMPT_FILE_FLAG.to_owned(),
        prompt_file.display().to_string(),
    ]);
    let mut env = req.env.to_vec();
    for (name, value) in [
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
        Thinking::Effort(level) => args.extend([EFFORT_FLAG.to_owned(), level.to_owned()]),
        Thinking::Budget(tokens) => env.push((THINKING_BUDGET_ENV.to_owned(), tokens.to_string())),
    }
    args.extend([SHOWN_THINKING.0.to_owned(), SHOWN_THINKING.1.to_owned()]);
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
    run: Run<'_>,
    server: &mut Option<mcp::Server>,
) -> Result<(StreamResponse, Instant), Error> {
    let Run {
        mut turn,
        conversation,
        handoffs,
        events,
        limits,
    } = run;
    let mut prompted = Instant::now();
    let mut stdin = group.child.stdin.take();
    let mut lines = stdout_lines(group.child.stdout.take())?;
    let mut startup = Instant::now() + limits.startup;
    send_handshake(&mut stdin, startup).await?;
    let mut last_event = Instant::now();
    // Set when the reply is complete, and keep-alives must not move it.
    let mut finish_by = None;
    let mut result_late = false;
    let mut generating = false;
    loop {
        let deadline = if !turn.accepted() {
            startup
        } else if turn.awaits_handoff() {
            *finish_by.get_or_insert_with(|| Instant::now() + limits.handoff)
        } else if turn.awaits_result() || turn.is_broken() {
            *finish_by.get_or_insert_with(|| Instant::now() + limits.exit)
        } else {
            last_event + limits.idle
        };
        let step = match next(&mut lines, Some(&handoffs), deadline).await {
            Next::Line(Some(Ok(line))) => turn.feed(&line),
            Next::Line(Some(Err(source))) => Err(unreadable(source)),
            Next::Line(None) => {
                let ended = match group.wait(Instant::now() + limits.exit).await {
                    Ok(status) => Error::ExitedEarly(status),
                    Err(Error::ExitLate) => Error::WentQuiet,
                    Err(other) => other,
                };
                return Err(turn.take_broken().unwrap_or(ended));
            }
            Next::Handoff(handoff) => turn.park(handoff),
            // The reply and its usage are complete, so the result only
            // repeats them. Its delay is Claude Code's own business.
            Next::Late if turn.is_broken() => Err(turn.take_broken().unwrap_or(Error::WentQuiet)),
            Next::Late if turn.awaits_result() && turn.has_stream_usage() => {
                warn!(
                    secs = limits.exit.as_secs(),
                    "claude-code: the result is late after a complete reply, so maki uses the reply as it streamed"
                );
                result_late = true;
                Ok(Step::Done)
            }
            Next::Late => Err(if turn.awaits_handoff() {
                Error::HandoffLate(limits.handoff.as_secs())
            } else if turn.awaits_result() {
                Error::ResultLate(limits.exit.as_secs())
            } else if turn.accepted() {
                Error::Stalled(limits.idle.as_secs())
            } else {
                Error::StartupLate(limits.startup.as_secs())
            }),
        };
        // Keep-alives come from a timer, so they do not restart the clock.
        if !matches!(step, Ok(Step::Alive)) {
            last_event = Instant::now();
        }
        if let Err(error) = &step {
            debug!(%error, events = %turn.trace(), "claude-code: the request failed");
        }
        if !generating && turn.accepted() {
            generating = true;
            debug!(
                startup_ms = millis(prompted.elapsed()),
                "claude-code: init accepted, the reply streams"
            );
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
    // Claude Code would continue after a handoff or a cut reply, so it is
    // stopped here. Every event up to the end of the output is still checked,
    // so nothing after the reply can contradict it unnoticed.
    let stopped = turn.calls_tools() || turn.truncated() || result_late;
    if stopped {
        group.kill();
    }
    let deadline = Instant::now() + limits.exit;
    drain(&mut turn, &mut lines, &handoffs, deadline, stopped).await?;
    let status = group.wait(deadline).await?;
    // After the reap, Claude Code cannot make calls. Stop the server to end the queue and
    // validate all remaining calls.
    drop(server.take());
    queued_handoffs(&mut turn, handoffs, Instant::now() + limits.exit).await?;
    if !stopped && !status.success() {
        return Err(Error::ExitedAfterReply(status));
    }
    Ok((turn.response()?, prompted))
}

async fn queued_handoffs(
    turn: &mut Turn<'_>,
    handoffs: Receiver<Handoff>,
    deadline: Instant,
) -> Result<(), Error> {
    loop {
        let handoff = async { Some(handoffs.recv_async().await.ok()) }
            .or(async {
                Timer::at(deadline).await;
                None
            })
            .await
            .ok_or(Error::ServerLate)?;
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
    loop {
        let step = match next(lines, Some(handoffs), deadline).await {
            Next::Line(Some(Ok(line))) => turn.feed(&line).map(drop),
            Next::Line(Some(Err(source))) => Err(unreadable(source)),
            Next::Line(None) => return Ok(()),
            Next::Handoff(handoff) => turn.park(handoff).map(drop),
            Next::Late => Err(Error::ExitLate),
        };
        let Err(error) = step else {
            continue;
        };
        let cut = stopped
            && matches!(error, Error::NotAnEvent(_) | Error::Io { .. })
            && matches!(next(lines, None, deadline).await, Next::Line(None));
        return if cut { Ok(()) } else { Err(error) };
    }
}

/// Runs `work` on a Claude Code with no prompt and no MCP server, in an empty
/// directory checked to be outside the project.
async fn unprompted<T>(
    executable: &Path,
    env: &[(String, String)],
    project: &Path,
    temp_dir: &Path,
    work: impl AsyncFnOnce(&mut Group, &Path) -> Result<T, Error>,
) -> Result<T, Error> {
    let (_dir, dir_path) = private_dir(temp_dir, project)?;
    let mut args = base_args(PROBE_MODEL);
    args.extend([MCP_CONFIG_FLAG.to_owned(), EMPTY_MCP_CONFIG.to_owned()]);
    supervised(command(executable, env, &dir_path, &args), async |group| {
        work(group, &dir_path).await
    })
    .await
}

/// A hook that policy keeps on runs at startup, so the handshake first runs
/// in an empty directory, without a prompt.
async fn probe(
    executable: &Path,
    profile: &Profile,
    env: &[(String, String)],
    project: &Path,
    temp_dir: &Path,
    plan_usage: &Mutex<Option<ProviderUsage>>,
    startup: Duration,
) -> Result<Vec<Offered>, Error> {
    let empty = Catalog::new(&json!([]))?;
    unprompted(executable, env, project, temp_dir, async |group, dir| {
        let mut stdin = group.child.stdin.take();
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let tools = empty.exposed();
        let expect = InitExpect {
            profile,
            model: PROBE_MODEL,
            cwd: dir,
            server: SERVER,
            tools: &tools,
        };
        let mut turn = Turn::new(&empty, expect, plan_usage);
        let deadline = Instant::now() + startup;
        send_handshake(&mut stdin, deadline).await?;
        drop(stdin);
        let mut passed = false;
        loop {
            match next(&mut lines, None, deadline).await {
                Next::Line(Some(Ok(line))) => match turn.feed(&line)? {
                    Step::Ready => passed = true,
                    Step::Nothing | Step::Alive => {}
                    _ => return Err(Error::ProbeRan),
                },
                Next::Line(Some(Err(source))) => return Err(unreadable(source)),
                Next::Line(None) => break,
                Next::Handoff(_) | Next::Late => return Err(Error::ProbeLate(startup.as_secs())),
            }
        }
        let status = group.wait(deadline).await?;
        if !status.success() {
            return Err(Error::ExitedInChecks(status));
        }
        if !passed {
            return Err(Error::ChecksUnanswered);
        }
        Ok(turn.offered())
    })
    .await
}

/// A CLI upgrade between requests invalidates a cached version.
async fn checked_profile(
    executable: &Path,
    env: &[(String, String)],
    project: &Path,
    temp_dir: &Path,
    startup: Duration,
) -> Result<Profile, Error> {
    // Runs in a private directory outside the project, like the probe, so no
    // Claude Code starts in the project before the checks.
    let (_dir, dir_path) = private_dir(temp_dir, project)?;
    let output = version(executable, env, &dir_path, startup).await?;
    checks::profile(&output, env::consts::OS)
}

/// Returns the models by the ids they run, after the same checks a request
/// must pass.
pub(crate) async fn models(
    executable: &Path,
    env: &[(String, String)],
    project: &Path,
    temp_dir: &Path,
    plan_usage: &Mutex<Option<ProviderUsage>>,
    startup: Duration,
) -> Result<Vec<Listed>, Error> {
    let profile = checked_profile(executable, env, project, temp_dir, startup).await?;
    let offered = probe(
        executable, &profile, env, project, temp_dir, plan_usage, startup,
    )
    .await?;
    let mut ids: Vec<String> = Vec::new();
    for offer in offered {
        if !ids.contains(&offer.model) {
            ids.push(offer.model);
        }
    }
    // Each model switch can take Claude Code 2 s, so several processes split
    // the list. An unknown window falls back to the standard one, so the list
    // stays correct without it.
    let shares: Vec<&[String]> = ids
        .chunks(ids.len().div_ceil(WINDOW_PROCESSES).max(1))
        .collect();
    let answers = join_all(
        shares
            .iter()
            .map(|share| windows(executable, env, project, temp_dir, share, startup)),
    )
    .await;
    let windows: Vec<Option<u32>> = shares
        .iter()
        .zip(answers)
        .flat_map(|(share, answer)| {
            answer.unwrap_or_else(|error| {
                warn!(%error, "claude-code: maki cannot read the context windows");
                vec![None; share.len()]
            })
        })
        .collect();
    Ok(ids
        .into_iter()
        .zip(windows)
        .map(|(id, window)| Listed { id, window })
        .collect())
}

/// Returns the context window Claude Code opens for each name in `models`,
/// which is 1M for a new model even without `[1m]`. One process does it,
/// without a prompt, in an empty directory like the probe. The result is
/// `None` where the answer names a different model.
async fn windows(
    executable: &Path,
    env: &[(String, String)],
    project: &Path,
    temp_dir: &Path,
    models: &[String],
    startup: Duration,
) -> Result<Vec<Option<u32>>, Error> {
    unprompted(executable, env, project, temp_dir, async |group, _| {
        let mut stdin = group.child.stdin.take();
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let deadline = Instant::now() + startup;
        let initialize = json!({ "subtype": INITIALIZE });
        ask(&mut stdin, &mut lines, &initialize, deadline).await?;
        let mut windows = Vec::with_capacity(models.len());
        for model in models {
            let switch = json!({ "subtype": SET_MODEL, "model": model });
            ask(&mut stdin, &mut lines, &switch, deadline).await?;
            let usage = json!({ "subtype": CONTEXT_USAGE });
            let usage = ask(&mut stdin, &mut lines, &usage, deadline).await?;
            let window = usage["maxTokens"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok());
            windows.push(window.filter(|_| usage["model"] == model.as_str()));
        }
        Ok(windows)
    })
    .await
}

/// Sends `request` and waits for its answer. Requests go one at a time,
/// because Claude Code can answer a later request first. Other lines are
/// ignored, because this process has no prompt and its events do not matter.
async fn ask(
    stdin: &mut Option<ChildStdin>,
    lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    request: &Value,
    deadline: Instant,
) -> Result<Value, Error> {
    let id = request["subtype"].as_str().unwrap_or_default();
    send(stdin, &control_request(id, request), deadline).await?;
    loop {
        match next(lines, None, deadline).await {
            Next::Line(Some(Ok(line))) => {
                let event: Value = serde_json::from_str(&line).unwrap_or_default();
                let answer = &event["response"];
                if event["type"] == CONTROL_RESPONSE && answer["request_id"] == id {
                    return match answer["subtype"] == SUCCESS {
                        true => Ok(answer["response"].clone()),
                        false => Err(Error::NotAnswered(id.to_owned())),
                    };
                }
            }
            Next::Line(Some(Err(source))) => return Err(unreadable(source)),
            Next::Line(None) | Next::Handoff(_) | Next::Late => {
                return Err(Error::NotAnswered(id.to_owned()));
            }
        }
    }
}

async fn version(
    executable: &Path,
    env: &[(String, String)],
    temp_dir: &Path,
    startup: Duration,
) -> Result<String, Error> {
    let args = [VERSION_FLAG.to_owned()];
    supervised(command(executable, env, temp_dir, &args), async |group| {
        drop(group.child.stdin.take());
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let deadline = Instant::now() + startup;
        let first = match next(&mut lines, None, deadline).await {
            Next::Line(Some(Ok(line))) => Some(line),
            Next::Line(Some(Err(source))) => return Err(Error::UnreadableVersion(source)),
            _ => None,
        };
        let status = group.wait(deadline).await?;
        if !status.success() {
            return Err(Error::VersionFailed(status));
        }
        first.ok_or(Error::NoVersion)
    })
    .await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::env;
    use std::fs;
    use std::io;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{self, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use futures_lite::{FutureExt, StreamExt};
    use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process_group};
    use serde_json::{Value, json};
    use smol::Timer;
    use tempfile::tempdir;
    use test_case::test_case;

    use super::super::checks::{InitExpect, profile};
    use super::super::error::Error;
    use super::super::fake::{
        ALIAS, EXIT_LIMIT, Fake, HAIKU, IDLE, MARKER, OPUS, POLL, TEMP_BASE, TOOL, VERSION_ERROR,
        WAIT, wait_until,
    };
    use super::super::mcp::Handoff;
    use super::super::stream::Turn;
    use super::super::transcript::{Catalog, SERVER};
    use super::{
        DIR_PREFIX, EFFORT_FLAG, Group, HANDOFF, HELD_CALL_TIMEOUT_MS, Limits, Listed,
        NO_AUTO_COMPACT, NO_CLI_RETRIES, NO_MID_CONVERSATION_SYSTEM, NO_NONSTREAMING_FALLBACK,
        NO_THINKING, SHOWN_THINKING, STARTUP, Thinking, command, models, probe, queued_handoffs,
        spawn_piped, stdout_lines,
    };
    #[cfg(target_os = "linux")]
    use super::{OWNER_SEPARATOR, pid_namespace, sweep_dead_owners};
    use crate::providers::anthropic::{LABEL_SESSION, LABEL_WEEK_ALL};
    use crate::{ContentBlock, ImageMediaType, ImageSource, Role, StopReason};
    use crate::{Message, ProviderEvent, StreamResponse};

    const SHORT_EXIT: Duration = Duration::from_secs(2);
    const SLEEP: &str = "/bin/sleep";
    /// Set in the copy of the test binary that plays maki.
    const MAKI_ROLE: &str = "MAKI_TEST_PLAYS_MAKI";
    const KILLED_MAKI_TEST: &str = "a_killed_maki_takes_claude_code_along";
    const MEMBER_PID: &str = "member-pid";
    const LONG_SLEEP_SECS: &str = "30";
    /// Much larger than a pipe buffer, so the write blocks when nobody reads.
    const UNREAD_PROMPT_BYTES: usize = 1 << 20;
    /// The run's handshake, generous for a loaded machine.
    const UNREAD_STARTUP: Duration = Duration::from_secs(10);
    const LONG_CONTEXT_ID: &str = "claude-haiku-4-5-1m";
    /// A model the fake never runs.
    const OTHER_MODEL: &str = "claude-sonnet-5";
    /// What the fake's `get_context_usage` reports for its Opus and for every
    /// other model.
    const WIDE_WINDOW: u32 = 1_000_000;
    const STANDARD_WINDOW: u32 = 200_000;
    /// Makes the fake send an error for `get_context_usage`.
    const NO_WINDOWS: &str = "no_windows";
    const LONG_CONTEXT_ARG: &str = "claude-haiku-4-5[1m]";
    const OLDER_VERSION: &str = "2.1.283 (Claude Code)";
    const NEWER_VERSION: &str = "2.1.290 (Claude Code)";
    const SHORT_HANDOFF: Duration = Duration::from_secs(1);
    const SHORT_IDLE: Duration = Duration::from_secs(1);
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
    /// Prints the pid of a `sleep` that closed all its standard streams.
    const LINGERING_MEMBER: &str = "sleep 30 </dev/null >/dev/null 2>&1 & echo $!";

    /// The call can reach the server before or after the generation ends,
    /// which the stream tests pin in both orders. maki gets the whole batch with its own tool
    /// names and the
    /// final usage, and no process or private file is left behind.
    #[test_case("batch" ; "the_generation_first")]
    #[test_case("cut_line" ; "a_last_line_the_stop_cut_short")]
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
    #[test]
    fn a_late_result_after_a_complete_reply_keeps_the_reply() {
        let fake = Fake::new("stalled_result");
        let result = through_keep_alives(&fake, limits(STARTUP, IDLE, SHORT_HANDOFF, SHORT_EXIT));

        let response = result.unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(response.usage.output, FINAL_OUTPUT);
        assert!(fake.group_gone(), "Claude Code continued after the reply");
    }

    /// Events without content can follow the result, like the keep-alive
    /// Claude Code prints on long runs.
    #[test]
    fn a_reply_without_calls_ends_at_its_result() {
        let fake = Fake::new("text_then_keep_alive");
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
    #[test_case("text_then_fail" => matches Error::ExitedAfterReply(_) ; "a_text_result_followed_by_a_failed_exit")]
    #[test_case("text_then_api_key" => matches Error::Check(_) ; "a_text_result_then_an_init_that_contradicts_it")]
    #[test_case("text_then_garbage" => matches Error::NotAnEvent(_) ; "a_text_result_then_a_line_that_is_no_event")]
    fn a_run_maki_cannot_accept_stops_and_is_killed(scenario: &str) -> Error {
        let fake = Fake::new(scenario);
        let (result, _) = smol::block_on(fake.request());
        assert!(fake.leader_reaped(), "the request returned before the reap");
        assert!(
            fake.group_gone(),
            "a stopped run must not continue after its request"
        );
        result.unwrap_err()
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
            matches!(result, Err(Error::ApiRefused { .. })),
            "{result:?}"
        );
        assert!(fake.group_gone());
    }

    /// Cancellation can precede spawn completion or channel receipt. An unclaimed process
    /// group must stop in either case.
    #[cfg(target_os = "linux")]
    #[test_case(true ; "before_the_send")]
    #[test_case(false ; "in_the_channel")]
    fn a_process_nobody_takes_is_stopped(cancelled_before_the_send: bool) {
        let here = env::current_dir().unwrap();
        let args = [LONG_SLEEP_SECS.to_owned()];
        let group = spawn_piped(command(Path::new(SLEEP), &Fake::env(), &here, &args)).unwrap();
        let pid = Pid::from_raw(group.child.id().try_into().unwrap()).unwrap();
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
    #[test]
    fn a_run_that_never_exits_is_killed_at_the_exit_limit() {
        let fake = Fake::new("text_then_linger");
        let (result, _) = smol::block_on(fake.request_with(
            &format!("find {MARKER}"),
            ALIAS,
            limits(STARTUP, IDLE, HANDOFF, SHORT_EXIT),
        ));
        assert!(
            fake.group_gone(),
            "a stopped run must not continue after its request"
        );
        assert!(matches!(result, Err(Error::ExitLate)), "{result:?}");
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
        assert!(matches!(err, Error::InputNotTaken), "{err}");
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
        result.unwrap_err()
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
        let catalog =
            Catalog::new(&json!([{ "name": TOOL, "input_schema": { "type": "object" } }])).unwrap();
        let profile = profile("2.1.284", "linux").unwrap();
        let tools = catalog.exposed();
        let expect = InitExpect {
            profile: &profile,
            model: ALIAS,
            cwd: Path::new(ANY_CWD),
            server: SERVER,
            tools: &tools,
        };
        let plan_usage = Mutex::default();
        let mut turn = Turn::new(&catalog, expect, &plan_usage);
        let (sender, handoffs) = flume::unbounded();
        sender
            .send(Handoff::Invalid(Error::NotOffered(json!(TOOL))))
            .unwrap();
        drop(sender);
        let result = smol::block_on(queued_handoffs(&mut turn, handoffs, Instant::now() + WAIT));
        assert!(matches!(result, Err(Error::NotOffered(_))), "{result:?}");
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

    /// An unreadable version line and a missing one give different errors.
    #[test]
    fn an_unreadable_version_line_is_named() {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join("version"), NOT_UTF8_VERSION).unwrap();
        let err = smol::block_on(fake.request()).0.unwrap_err();
        let unreadable = |error: &Error| matches!(error, Error::UnreadableVersion(source) if source.kind() == io::ErrorKind::InvalidData);
        assert!(
            unreadable(&err)
                || matches!(&err, Error::WithStderr { error, .. } if unreadable(error)),
            "{err:?}"
        );
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
        assert!(matches!(result, Err(Error::WentQuiet)), "{result:?}");
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
        smol::block_on(fake.send(
            &format!("find {MARKER}"),
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
        let left: Vec<_> = fs::read_dir(fake.temp_base()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A signal can kill maki before any destructor runs, so Claude Code must
    /// die with maki rather than with the request. The test binary runs
    /// itself as that maki, with `TMPDIR` in a dir this test owns.
    #[cfg(target_os = "linux")]
    #[test_case(Signal::TERM, "sigterm" ; "sigterm")]
    #[test_case(Signal::KILL, "sigkill" ; "sigkill")]
    fn a_killed_maki_takes_claude_code_along(signal: Signal, case: &str) {
        if env::var_os(MAKI_ROLE).is_some() {
            let _ = smol::block_on(Fake::new("hang").request());
            return;
        }
        let temp = tempdir().unwrap();
        let (_, tests) = module_path!().split_once("::").unwrap();
        let mut maki = process::Command::new(env::current_exe().unwrap())
            .args(["--exact", &format!("{tests}::{KILLED_MAKI_TEST}::{case}")])
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
        assert!(running(), "the fake never started its descendant");

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
    /// namespace, where its pid names the same maki. A link to a directory
    /// stays, and so does what it points to.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_makis_private_dir_is_swept() {
        let base = tempdir().unwrap();
        let mut exited = process::Command::new("true").spawn().unwrap();
        exited.wait().unwrap();
        let namespace = pid_namespace().unwrap();
        let named = |namespace: u64, pid: u32, tail: &str| {
            base.path().join(format!(
                "{DIR_PREFIX}{namespace}{OWNER_SEPARATOR}{pid}.{tail}"
            ))
        };
        let dead = named(namespace, exited.id(), "x");
        let alive = named(namespace, process::id(), "x");
        let elsewhere = named(namespace + 1, exited.id(), "x");
        let other = base.path().join("unrelated");
        let target = tempdir().unwrap();
        for dir in [&dead, &alive, &elsewhere, &other] {
            fs::create_dir(dir).unwrap();
            fs::write(dir.join("mcp.json"), "{}").unwrap();
        }
        let link = named(namespace, exited.id(), "link");
        symlink(target.path(), &link).unwrap();

        sweep_dead_owners(base.path());

        assert!(!dead.exists(), "the dead maki's directory stayed");
        for kept in [&alive, &elsewhere, &other, &link] {
            assert!(kept.exists(), "{kept:?} went");
        }
    }

    /// The probe sends no prompt, so a run it starts can only be Claude Code
    /// acting on its own.
    #[test]
    fn a_probe_that_starts_a_run_is_refused() {
        let fake = Fake::new("probe_runs");
        let profile = profile("2.1.284", "linux").unwrap();
        let result = smol::block_on(probe(
            &fake.executable(),
            &profile,
            &Fake::env(),
            &fake.project(),
            &env::temp_dir(),
            &fake.plan_usage,
            STARTUP,
        ));
        assert!(
            fake.log("stdin_prompt").is_empty(),
            "a probe sends no prompt"
        );
        assert!(matches!(result, Err(Error::UncheckedStart)));
    }

    /// The models come from the account answer, each once, by the id it runs
    /// and with the window Claude Code opens for it. Model discovery sends no prompt,
    /// and the list is still correct without the windows.
    #[test_case(true ; "with_their_windows")]
    #[test_case(false ; "without_windows")]
    fn the_listed_models_are_what_the_account_offers(windows: bool) {
        let fake = Fake::new("text");
        if !windows {
            fs::write(fake.dir.path().join(NO_WINDOWS), "").unwrap();
        }
        let listed = smol::block_on(models(
            &fake.executable(),
            &Fake::env(),
            &fake.project(),
            &env::temp_dir(),
            &fake.plan_usage,
            STARTUP,
        ))
        .unwrap();

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
            matches!(result, Err(Error::OtherModel { .. })),
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

        assert!(matches!(err, Error::Check(_)), "{err}");
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
    }

    /// Each request reads the version again. A newer version runs, and one
    /// below the minimum stops before it starts.
    #[test_case(NEWER_VERSION, true ; "an_upgrade_runs")]
    #[test_case(OLDER_VERSION, false ; "a_downgrade_never_starts")]
    fn a_version_changed_between_requests(version: &str, runs: bool) {
        let fake = Fake::new("text");
        smol::block_on(fake.request()).0.unwrap();
        fs::write(fake.dir.path().join("version"), version).unwrap();
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
        let mut lines = stdout_lines(group.child.stdout.take()).unwrap();
        let member: String = smol::block_on(lines.next()).unwrap().unwrap();
        let status = smol::block_on(group.wait(Instant::now() + WAIT)).unwrap();
        assert!(status.success());
        let running = || fs::read(format!("/proc/{member}/cmdline")).is_ok_and(|c| !c.is_empty());
        assert!(
            wait_until(|| !running()),
            "the process in the group continued after its leader"
        );
    }

    /// A stderr drain often kills after `wait` reaped the leader, when its
    /// pid may name another group. Here the child is only marked reaped, so a
    /// kill would show as SIGKILL rather than the test's SIGTERM.
    #[test]
    fn a_reaped_group_is_never_signalled() {
        let here = env::current_dir().unwrap();
        let args = [LONG_SLEEP_SECS.to_owned()];
        let sleep = command(Path::new(SLEEP), &Fake::env(), &here, &args);
        let mut group = smol::block_on(Group::spawn(sleep)).unwrap();
        group.reaped = true;

        group.kill();

        let pid = Pid::from_raw(group.child.id().try_into().unwrap()).unwrap();
        kill_process(pid, Signal::TERM).unwrap();
        let status = smol::block_on(group.child.status()).unwrap();
        assert_eq!(status.signal(), Some(Signal::TERM.as_raw()));
    }
}
