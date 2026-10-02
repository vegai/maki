//! Why a Claude Code request stopped. maki retries only temporary API errors,
//! where the API did not serve the request, because for any other error
//! Claude Code may already have sent it.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

const INVALID_REQUEST: &str = "invalid_request";
pub(crate) const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERROR: u16 = 500;
const OVERLOADED: u16 = 529;
/// Kinds of Claude Code API error messages that a later attempt can fix,
/// with the status assumed when the message carries none.
const TEMPORARY_KINDS: &[(&str, u16)] = &[
    ("rate_limit", TOO_MANY_REQUESTS),
    ("overloaded", OVERLOADED),
    ("server_error", SERVER_ERROR),
];
/// How much of a JSON value a message shows, enough to recognize it.
const SHOWN_JSON_CHARS: usize = 200;

/// Formats a JSON value for a message, cutting a long one so a huge value
/// from Claude Code cannot flood the error.
fn shown(value: &Value) -> String {
    let text = value.to_string();
    match text.char_indices().nth(SHOWN_JSON_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error(
        "the provider needs the session's working directory, and this request did not pass one"
    )]
    NoWorkingDir,
    #[error(transparent)]
    Check(#[from] Problem),
    #[error("Claude Code ran {ran:?} for {asked}, which organization policy can cause")]
    OtherModel { asked: String, ran: String },
    #[error("maki cannot read a Claude Code version from {0:?}")]
    UnknownVersion(String),
    #[error("Claude Code {version} is older than {oldest}, the oldest version maki runs")]
    TooOld { version: String, oldest: String },
    #[error("maki runs Claude Code only on {systems}, not on {os}")]
    UnsupportedSystem { os: String, systems: String },
    #[error(
        "the Claude Code config directory {} is not an absolute path. Set the claude_code plugin's `config_dir` option, CLAUDE_CONFIG_DIR or HOME to an absolute path.",
        .0.display()
    )]
    RelativeConfigDir(PathBuf),
    #[error(
        "maki cannot find the Claude Code config directory. Set the claude_code plugin's `config_dir` option, CLAUDE_CONFIG_DIR or HOME."
    )]
    NoConfigDir,
    #[error("maki ignores these Claude Code settings, which is not safe:\n{}", .0.join("\n"))]
    SkippedSettings(Vec<String>),
    #[error("the temporary directory {} is in the project. Set TMPDIR to a different directory.", .0.display())]
    TempInProject(PathBuf),
    #[error("maki cannot {what} {}: {source}", .path.display())]
    Path {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("maki cannot {what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error("maki cannot make a handoff token: {0}")]
    Token(getrandom::Error),

    #[error("Claude Code did not read its input in time")]
    InputNotTaken,
    #[error("Claude Code did not finish its startup checks within {0} s")]
    StartupLate(u64),
    #[error("Claude Code did not answer its checks within {0} s")]
    ProbeLate(u64),
    #[error("Claude Code stopped its output before its reply was complete")]
    WentQuiet,
    #[error("Claude Code sent nothing for {0} s during its reply")]
    Stalled(u64),
    #[error("Claude Code did not hand the reply's tool calls to maki within {0} s")]
    HandoffLate(u64),
    #[error("Claude Code did not send its result within {0} s of the reply's end")]
    ResultLate(u64),
    #[error("Claude Code did not exit in time")]
    ExitLate,
    #[error("maki's handoff server did not close in time")]
    ServerLate,
    #[error("`claude --version` printed nothing")]
    NoVersion,
    #[error(
        "Claude Code failed to list the models a moment ago, and maki waits {} minutes or a refresh before asking again: {message}",
        wait.as_secs() / 60
    )]
    ListedRecently { message: String, wait: Duration },
    #[error("Claude Code did not answer its {0} request")]
    NotAnswered(String),
    /// A glitch of the model, so maki asks again.
    #[error("Claude Code's reply stopped to call tools, but called none")]
    NoCallsToRun,
    #[error("maki cannot read the output of `claude --version`: {0}")]
    UnreadableVersion(io::Error),

    #[error("Claude Code exited ({0}) before its reply was complete")]
    ExitedEarly(ExitStatus),
    #[error("Claude Code exited ({0}) after its reply")]
    ExitedAfterReply(ExitStatus),
    #[error("Claude Code exited ({0}) during its checks")]
    ExitedInChecks(ExitStatus),
    #[error("`claude --version` exited ({0})")]
    VersionFailed(ExitStatus),
    #[error("Claude Code did not answer every check")]
    ChecksUnanswered,
    #[error("Claude Code started a run during its checks, before maki sent a prompt")]
    ProbeRan,

    #[error("Claude Code has no stdout")]
    NoStdout,
    #[error("Claude Code closed its stdin")]
    NoStdin,
    #[error("Claude Code printed a line that is not an event: {0}")]
    NotAnEvent(String),
    #[error("Claude Code sent an unknown event type: {}", shown(.0))]
    UnknownEvent(Value),
    #[error("Claude Code sent maki a {} request, which maki does not accept", shown(.0))]
    Asked(Value),
    #[error("Claude Code started a {0} hook")]
    HookStarted(String),
    #[error("Claude Code started its run before maki accepted its checks")]
    UncheckedStart,
    #[error("Claude Code answered a {0} request that maki never sent, or answered it twice")]
    StrayAnswer(String),
    /// A new attempt cannot pass before the plan's window resets, which Claude
    /// Code's text names.
    #[error("the Claude plan's limit is used up, so maki does not retry: {0}")]
    PlanLimit(String),
    #[error("the API rejected the request ({kind}): {text}")]
    ApiRefused {
        kind: String,
        text: String,
        status: Option<u16>,
    },
    /// Claude Code runs with its own retries off, so this comes only from a
    /// retry that the setting misses.
    #[error("Claude Code wanted to retry after an API error ({kind})")]
    CliRetry {
        kind: String,
        status: Option<u16>,
        delay: Option<Duration>,
    },
    #[error("Claude Code rejected the {id} request: {}", shown(.error))]
    RequestRefused { id: String, error: Value },
    #[error("Claude Code answered a tool call itself: {0}")]
    AnsweredItself(String),
    /// Claude Code would add a note and run the model once more.
    #[error("Anthropic's safety classifier stopped the reply: {0}")]
    Refused(String),
    #[error("Claude Code sent more of its reply after the reply was complete")]
    LateContent,
    #[error("Claude Code started a second generation in one request")]
    SecondGeneration,
    #[error("Claude Code started a generation without an id")]
    NoGenerationId,
    #[error("Claude Code sent a message outside the generation it streamed")]
    OutsideGeneration,
    #[error("Claude Code started {started} blocks of its reply, but sent {sent}")]
    BlockCount { started: usize, sent: usize },
    #[error(
        "Claude Code compacted the conversation, so its reply no longer answers maki's conversation"
    )]
    Compacted,
    #[error("Claude Code sent a {} block that maki cannot read", shown(.0))]
    UnreadableBlock(Value),
    #[error("Claude Code stopped with an error: {0}")]
    Failed(String),

    #[error("the model called {}, which is not one of maki's tools", shown(.0))]
    NotOffered(Value),
    #[error("a tool call without an id")]
    NoCallId,
    #[error("the model called {name} with {}, which is not an arguments object", shown(.input))]
    NotArguments { name: String, input: Value },
    #[error("the model made call {0} twice")]
    RepeatedCall(String),
    #[error("Claude Code called {0} for a call that is not in the reply")]
    NotInReply(String),
    #[error("Claude Code called {0} differently from the call in the reply")]
    CallDiffers(String),
    #[error("the reply has no stop reason")]
    NoStopReason,
    #[error("the reply has tool calls but stopped for {0}")]
    StoppedFor(String),
    #[error("Claude Code reported no usage for its reply")]
    NoUsage,
    #[error("Claude Code reported no final output count for its reply")]
    NoFinalOutput,

    #[error("a tool without a name: {}", shown(.0))]
    NamelessTool(Value),
    #[error("two tools share the name {0}")]
    SharedName(String),
    #[error("an image reached the transcript, which holds only text")]
    Image,

    /// Any error above, with the last lines Claude Code printed on stderr.
    #[error("{error}:\n{stderr}")]
    WithStderr { error: Box<Error>, stderr: String },
}

impl Error {
    /// Returns the text when the API rejects a conversation that is larger
    /// than the window, whatever stderr says.
    pub(crate) fn invalid_request(&self) -> Option<&str> {
        match self {
            Self::ApiRefused { kind, text, .. } if kind == INVALID_REQUEST => Some(text),
            Self::WithStderr { error, .. } => error.invalid_request(),
            _ => None,
        }
    }

    /// The status and wait for an error a new attempt can fix: a rate limit,
    /// an overload or a server error. Login, plan and protocol errors give
    /// `None`.
    /// The idle limit of a reply that stopped streaming, which maki's retry
    /// loop treats as the anthropic provider's stalled stream.
    pub(crate) fn stalled(&self) -> Option<u64> {
        match self {
            Self::Stalled(secs) => Some(*secs),
            Self::WithStderr { error, .. } => error.stalled(),
            _ => None,
        }
    }

    pub(crate) fn temporary(&self) -> Option<(u16, Option<Duration>)> {
        match self {
            Self::ApiRefused { kind, status, .. } => match status {
                Some(status) if is_temporary_status(*status) => Some((*status, None)),
                Some(_) => None,
                None => TEMPORARY_KINDS
                    .iter()
                    .find(|(name, _)| name == kind)
                    .map(|(_, status)| (*status, None)),
            },
            Self::CliRetry { status, delay, .. } => Some((status.unwrap_or(SERVER_ERROR), *delay)),
            Self::NoCallsToRun => Some((SERVER_ERROR, None)),
            Self::WithStderr { error, .. } => error.temporary(),
            _ => None,
        }
    }
}

fn is_temporary_status(status: u16) -> bool {
    status == TOO_MANY_REQUESTS || status >= SERVER_ERROR
}

#[derive(Debug, Error)]
pub(crate) enum Problem {
    #[error("Claude Code did not report its login")]
    NoLogin,
    #[error("Claude Code would use an API key from {} instead of the subscription", shown(.0))]
    ApiKey(Value),
    #[error("Claude Code sends requests to {} instead of Anthropic", shown(.0))]
    OtherProvider(Value),
    #[error(
        "Claude Code is not logged in with a claude.ai subscription. Run `claude auth login` \
         with your Claude subscription. For API billing, use maki's anthropic provider."
    )]
    NoSubscription,
    #[error("Claude Code starts in permission mode {}", shown(.0))]
    StartMode(Value),
    #[error("Claude Code did not report its settings")]
    NoSettings,
    #[error(
        "Claude Code rejected some settings as invalid, so maki cannot tell which settings apply"
    )]
    InvalidSettings,
    #[error("your organization's Claude Code policy sets {0}, which maki cannot confirm is safe")]
    Policy(String),
    #[error("your organization's Claude Code policy is not a settings object that maki can check")]
    UnreadablePolicy,
    #[error("Claude Code loaded {0} settings, which it should have ignored")]
    LoadedSettings(String),
    #[error("Claude Code would compact the conversation itself, but maki owns and compacts it")]
    OwnCompaction,
    #[error("Claude Code did not list its hooks")]
    NoHooks,
    #[error("Claude Code cannot turn its hooks off")]
    HooksStayOn,
    #[error("Claude Code would run a hook from {}, which can change the checkout", shown(.0))]
    Hook(Value),
    #[error("Claude Code started with API key source {}", shown(.0))]
    StartedWithKey(Value),
    #[error("Claude Code reported version {}", shown(.0))]
    OtherVersion(Value),
    #[error("Claude Code runs in permission mode {}", shown(.0))]
    RunMode(Value),
    #[error("Claude Code did not list its tools")]
    NoTools,
    #[error("Claude Code did not list its plugins")]
    NoPlugins,
    #[error("Claude Code has the tool {}", shown(.0))]
    ExtraTool(Value),
    #[error("Claude Code does not have the tool {0}")]
    MissingTool(String),
    #[error("Claude Code's MCP servers are not just maki's connected handoff server: {}", shown(.0))]
    HandoffServer(Value),
    #[error(
        "Claude Code loaded the plugin {}. maki does not run Claude Code with a plugin other than \
         its built-in ones, because a plugin can change what the model sees and does. Turn the \
         plugin off in Claude Code.",
        shown(.0)
    )]
    Plugin(Value),
    #[error("Claude Code runs in {}", shown(.0))]
    OtherDir(Value),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Error, SHOWN_JSON_CHARS};

    const TOOL: &str = "read";

    /// A huge value from Claude Code shows only its start, and a short one
    /// shows whole.
    #[test]
    fn an_error_shows_only_the_start_of_a_huge_value() {
        let huge = json!("x".repeat(SHOWN_JSON_CHARS * 10));
        let message = Error::NotArguments {
            name: TOOL.into(),
            input: huge,
        }
        .to_string();
        assert!(
            message.len() < SHOWN_JSON_CHARS * 2,
            "{} bytes",
            message.len()
        );
        assert!(message.contains('…'), "{message}");
        let short = Error::NotOffered(json!(TOOL)).to_string();
        assert!(short.contains(&format!("\"{TOOL}\",")), "{short}");
    }
}
