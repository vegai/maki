//! Launch policy shared by the provider and plugin, and the plugin worker protocol.

use super::run::{Launch, Limits, command, private_dir_async, probe_with, supervised};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::future::Future;
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf, absolute};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use maki_storage::permissions::{PERMISSION_BITS, PRIVATE_DIR_MODE};
use rustix::process::geteuid;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::ProviderUsage;

use super::OAUTH_TOKEN_ENV;
use super::checks::{self, DISABLED, EFFECTIVE, PERMISSIONS_KEY};
use super::error::{Error, Problem};
use super::stream::{
    self, API_ERROR_MARK, CACHE_CREATION_INPUT_TOKENS, CACHE_READ_INPUT_TOKENS, Handshake,
    INPUT_TOKENS, OUTPUT_TOKENS, StreamMode, count,
};

pub use super::MAX_EVENT_BYTES;
pub use super::checks::{
    GIT_ENTRY, GITDIR_PREFIX, git_directory, handshake_input, inside_checkout,
};
pub use super::run::invalidate_version;
use super::settings::{self, base_settings};
use super::stream::usage_of as plan_usage;
pub use super::stream::{EventKind, TOOL_USE_BLOCK, user_message};

pub use super::{DEFAULT_EXECUTABLE, DEFAULT_MAX_CONCURRENT, MIN_MAX_CONCURRENT};
const EXIT_CODE_SHIFT: u32 = 8;

pub(super) const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub(super) const HOME_ENV: &str = "HOME";
const RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";
const CACHE_HOME_ENV: &str = "XDG_CACHE_HOME";
const FALLBACK_TEMP_DIR: &str = "/var/tmp";
const DEFAULT_CACHE_DIR: &str = ".cache";
const SCRATCH_DIRECTORY: &str = "maki/claude-code";
pub(super) const TMPDIR_ENV: &str = "TMPDIR";
const UNIX_TEMP_DIR: &str = "/tmp";
const LINK_TO_NOWHERE: &str = "is a link to a missing target";
const BLOCK_OUTSIDE: &str = "blockReadsOutsideWorkingDirectories";
const DENY: &str = "deny";
const SANDBOX: &str = "sandbox";
const ENABLED: &str = "enabled";
const FAIL_IF_UNAVAILABLE: &str = "failIfUnavailable";
const AUTO_ALLOW_BASH: &str = "autoAllowBashIfSandboxed";
const ALLOW_UNSANDBOXED: &str = "allowUnsandboxedCommands";
const NETWORK: &str = "network";
const ALLOWED_DOMAINS: &str = "allowedDomains";
const STRICT_ALLOWLIST: &str = "strictAllowlist";
const FILESYSTEM: &str = "filesystem";
const DENY_READ: &str = "denyRead";
const ALLOW_READ: &str = "allowRead";
const ALLOW_WRITE: &str = "allowWrite";
const EXCLUDED_COMMANDS: &str = "excludedCommands";
const WEAKER_NESTED_SANDBOX: &str = "enableWeakerNestedSandbox";
const ALLOW_ALL_UNIX_SOCKETS: &str = "allowAllUnixSockets";
const ALLOW_UNIX_SOCKETS: &str = "allowUnixSockets";
const ALLOW_LOCAL_BINDING: &str = "allowLocalBinding";
const DENIED_DOMAINS: &str = "deniedDomains";
const DENY_WRITE: &str = "denyWrite";
const ADDITIONAL_DIRECTORIES: &str = "additionalDirectories";
const READ_CONTRACT: &str = "You are a read-only worker for another coding agent. You can read and search the project, but you cannot edit files or run commands. Reply only with your results. Keep them short, with file:line locations. If the request needs a tool you do not have, say so clearly.";
const CODE_CONTRACT: &str = "You are a coding worker for another coding agent. You work in a private copy of the project. Make the task's changes there, and run the commands that check them. Its git repository is read-only to you: git can show and diff, but it cannot add, commit or stash. The other agent reviews your changes before it applies them to its own checkout, which you cannot change. Reply with a short summary of your changes and how you checked them. If a check could not run, say so clearly.";

pub fn shell_secret_problem(env: &HashMap<String, String>) -> Option<String> {
    for (name, value) in env {
        let upper = name.to_ascii_uppercase();
        if upper == OAUTH_TOKEN_ENV {
            return Some(format!(
                "Claude Code passes {name} to its shell, where a coding worker can read it. Remove the variable and log in with `claude auth login`, or use the read profile."
            ));
        }
        let authority = value
            .split_once("://")
            .map_or(value.as_str(), |(_, rest)| rest)
            .split('/')
            .next()
            .unwrap_or_default();
        if checks::PROXY_ENV.contains(&upper.as_str()) && authority.contains('@') {
            return Some(format!(
                "Claude Code passes {name} to its shell, where a coding worker can read the login in its URL. Use a proxy without a login in the URL, or use the read profile."
            ));
        }
    }
    None
}

enum WorkerProfile {
    Read,
    Code,
}

impl WorkerProfile {
    pub fn tools(&self) -> &'static [&'static str] {
        match self {
            Self::Read => &["Read", "Glob", "Grep"],
            Self::Code => &["Read", "Glob", "Grep", "Edit", "Write", "Bash"],
        }
    }

    fn modes(&self) -> &'static [&'static str] {
        match self {
            // The CLI can report default even though read workers launch in manual mode.
            Self::Read => &["default", "manual"],
            Self::Code => &["acceptEdits"],
        }
    }

    fn mode(&self) -> &'static str {
        match self {
            Self::Read => "manual",
            Self::Code => "acceptEdits",
        }
    }

    fn contract(&self) -> &'static str {
        match self {
            Self::Read => READ_CONTRACT,
            Self::Code => CODE_CONTRACT,
        }
    }
}

#[derive(Deserialize)]
#[serde(from = "PolicyInput")]
pub struct WorkerPolicy {
    settings: Value,
    profile: WorkerProfile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyInput {
    deny: Vec<String>,
    sandbox: Option<WorkerPaths>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerPaths {
    deny_read: Vec<String>,
    allow_read: Vec<String>,
    allow_write: Vec<String>,
}

impl WorkerPolicy {
    pub fn args(&self, model: &str) -> Vec<String> {
        let mut args = settings::base_args(
            model,
            &self.profile.tools().join(","),
            self.profile.mode(),
            &self.settings().to_string(),
        );
        args.extend(
            [
                settings::MCP_CONFIG_FLAG,
                settings::EMPTY_MCP_CONFIG,
                settings::APPEND_SYSTEM_PROMPT_FLAG,
                self.profile.contract(),
            ]
            .map(str::to_owned),
        );
        args
    }

    pub fn settings(&self) -> &Value {
        &self.settings
    }
}

impl From<PolicyInput> for WorkerPolicy {
    fn from(input: PolicyInput) -> Self {
        let mut settings = base_settings();
        settings[PERMISSIONS_KEY] = json!({
            (BLOCK_OUTSIDE): true,
            (DENY): input.deny,
        });
        if let Some(paths) = &input.sandbox {
            settings[SANDBOX] = json!({
                (ENABLED): true,
                (FAIL_IF_UNAVAILABLE): true,
                (AUTO_ALLOW_BASH): true,
                (ALLOW_UNSANDBOXED): false,
                (NETWORK): { (ALLOWED_DOMAINS): [], (STRICT_ALLOWLIST): true },
                (FILESYSTEM): {
                    (DENY_READ): paths.deny_read,
                    (ALLOW_READ): paths.allow_read,
                    (ALLOW_WRITE): paths.allow_write,
                },
            });
        }
        Self {
            settings,
            profile: if input.sandbox.is_some() {
                WorkerProfile::Code
            } else {
                WorkerProfile::Read
            },
        }
    }
}

pub fn executable(name: &str, cwd: &Path) -> Result<PathBuf, Error> {
    crate::process::find_program(name, cwd).ok_or_else(|| Error::ExecutableMissing(name.to_owned()))
}

pub fn startup_late(seconds: u64) -> String {
    Error::Late(stream::Wait::Startup, seconds).to_string()
}

pub fn worker_exit(code: i32, after_result: bool) -> Error {
    if code < 0 {
        return if after_result {
            Error::StoppedAfterResult
        } else {
            Error::WentQuiet
        };
    }
    let status = ExitStatus::from_raw(code << EXIT_CODE_SHIFT);
    if after_result {
        Error::ExitedAfterResult(status)
    } else {
        Error::ExitedEarly(status)
    }
}

pub fn invalidate_on_error(executable: &Path, error: &Error) {
    if error.invalidates_version() {
        invalidate_version(executable);
    }
}

pub struct WorkerProbe {
    pub executable: PathBuf,
    pub env: Vec<(String, String)>,
    pub project: PathBuf,
    pub denied: Vec<PathBuf>,
    /// Outside the artifact so hook output survives artifact cleanup.
    pub temp_dir: PathBuf,
    pub policy: Arc<WorkerPolicy>,
    pub model: String,
    pub startup: Duration,
}

impl WorkerProbe {
    pub async fn run(
        self,
        cancel: impl Future<Output = ()> + Send,
    ) -> (Result<(), Error>, Option<PathBuf>) {
        let (dir, path) = match private_dir_async(&self.temp_dir, &self.project, &self.denied).await
        {
            Ok(directory) => directory,
            Err(error) => return (Err(error), None),
        };
        tokio::pin!(cancel);
        if cancel.as_mut().now_or_never().is_some() {
            return (Err(Error::Cancelled), dir.close_empty().await);
        }
        let args = self.policy.args(&self.model);
        let mut handshake = WorkerHandshake::new(self.policy);
        let result = supervised(command(&self.executable, &self.env, &path, &args), async |group| {
            tokio::select! {
                biased;
                _ = cancel => Err(Error::Cancelled),
                result = probe_with(group, self.startup, |event| handshake.feed(event)) => result,
            }
        }).await.inspect_err(|error| invalidate_on_error(&self.executable, error));
        (result, dir.close_empty().await)
    }
}

pub struct WorkerHandshake {
    answers: Handshake,
    policy: Arc<WorkerPolicy>,
}

impl WorkerHandshake {
    pub fn new(policy: Arc<WorkerPolicy>) -> Self {
        Self {
            answers: Handshake::default(),
            policy,
        }
    }

    fn feed(&mut self, event: &Value) -> Result<stream::Step, Error> {
        Ok(match stream::classify(event, StreamMode::Worker, false)? {
            EventKind::ControlResponse => {
                if self.answer(&event["response"])? {
                    stream::Step::Ready
                } else {
                    stream::Step::Nothing
                }
            }
            EventKind::Result => {
                return Err(stream::result_failure(event).unwrap_or(Error::UncheckedStart));
            }
            EventKind::RateLimit | EventKind::Alive | EventKind::System => stream::Step::Nothing,
            _ => return Err(Error::UncheckedStart),
        })
    }

    pub fn answer(&mut self, response: &Value) -> Result<bool, Error> {
        self.answers.answer(response, |account, settings, hooks| {
            checks::account_problem(account, self.policy.profile.modes())
                .or_else(|| checks::policy_problem(settings, hooks, false))
                .or_else(|| worker_problem(&settings[EFFECTIVE], self.policy.settings()))
        })
    }

    pub fn subscription_type(&self) -> Option<&str> {
        self.answers.account()[checks::ACCOUNT][checks::SUBSCRIPTION_TYPE].as_str()
    }

    pub fn resolved_model<'a>(&'a self, requested: &'a str) -> Result<&'a str, Error> {
        self.answers.resolved_model(requested)
    }
}

pub struct LaunchPreparation {
    pub env: Vec<(String, String)>,
    pub withheld: Vec<String>,
    pub config_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub denied: Vec<PathBuf>,
}

fn env_value(vars: &[(String, String)], name: &str) -> Option<OsString> {
    vars.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| OsString::from(value))
}

pub fn prepare_launch(
    vars: Vec<(String, String)>,
    configured: Option<PathBuf>,
    cwd: &Path,
) -> Result<LaunchPreparation, Error> {
    let var = |name| env_value(&vars, name);
    let config_dir = login_dir(&vars, configured.as_deref())?;
    let temp_dir = var(TMPDIR_ENV)
        .filter(|path| !path.is_empty())
        .map_or_else(|| PathBuf::from(UNIX_TEMP_DIR), PathBuf::from);
    let temp_dir =
        absolute(&temp_dir).map_err(Error::path("resolve the temporary directory", &temp_dir))?;
    let cache = var(CACHE_HOME_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            var(HOME_ENV)
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(DEFAULT_CACHE_DIR))
        });
    let fallback_dirs = [
        var(RUNTIME_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|root| root.join(SCRATCH_DIRECTORY)),
        cache.map(|root| root.join(SCRATCH_DIRECTORY)),
        Some(
            PathBuf::from(FALLBACK_TEMP_DIR)
                .join(format!("maki-claude-code-{}", geteuid().as_raw())),
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let (mut env, withheld) = checks::launch_env(vars);
    env.retain(|(name, value)| name != CONFIG_DIR_ENV || !value.is_empty());
    if configured.is_some() {
        env.retain(|(name, _)| name != CONFIG_DIR_ENV);
        env.push((CONFIG_DIR_ENV.to_owned(), config_dir.display().to_string()));
    }
    let (local_dirs, git_dir) = checks::local_settings_dirs(cwd)?;
    let conflicts =
        checks::file_conflicts(&checks::skipped_settings(&config_dir, cwd, &local_dirs));
    if !conflicts.is_empty() {
        return Err(Error::SkippedSettings(conflicts));
    }
    let denied: Vec<PathBuf> = local_dirs.iter().chain(git_dir.iter()).cloned().collect();
    let temp_dir = scratch_base(&temp_dir, cwd, &denied, fallback_dirs)?;
    env.retain(|(name, _)| name != TMPDIR_ENV);
    env.push((TMPDIR_ENV.to_owned(), temp_dir.display().to_string()));
    Ok(LaunchPreparation {
        env,
        withheld,
        config_dir,
        temp_dir,
        denied,
    })
}

fn scratch_base(
    preferred: &Path,
    cwd: &Path,
    denied: &[PathBuf],
    fallbacks: Vec<PathBuf>,
) -> Result<PathBuf, Error> {
    let outside =
        |path: &Path| path.to_str().is_some() && !checks::inside_checkout(path, cwd, denied);
    if let Ok(base) = preferred.canonicalize()
        && base.is_dir()
        && outside(&base)
    {
        return Ok(base);
    }
    for path in fallbacks {
        let Ok(resolved) = resolve_missing(&path) else {
            continue;
        };
        if !outside(&resolved) {
            continue;
        }
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        builder.mode(PRIVATE_DIR_MODE);
        if builder.create(&path).is_err() {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        if metadata.uid() != geteuid().as_raw()
            || metadata.permissions().mode() & PERMISSION_BITS != PRIVATE_DIR_MODE
        {
            debug!(path = %path.display(), uid = metadata.uid(), mode = metadata.permissions().mode() & PERMISSION_BITS, "claude-code: skipping scratch fallback with unsafe ownership or permissions");
            continue;
        }
        if let Ok(base) = path.canonicalize()
            && outside(&base)
        {
            return Ok(base);
        }
    }
    Err(Error::NoScratch(preferred.to_owned()))
}

pub(super) fn login_dir(
    vars: &[(String, String)],
    configured: Option<&Path>,
) -> Result<PathBuf, Error> {
    let var = |name| env_value(vars, name);
    checks::config_dir(
        configured
            .map(OsString::from)
            .or_else(|| var(CONFIG_DIR_ENV)),
        var(HOME_ENV),
    )
}

pub fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for name in missing.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::symlink_metadata(existing) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            ErrorKind::NotFound,
                            format!("{} {LINK_TO_NOWHERE}", existing.display()),
                        ));
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let Some(name) = existing.file_name() else {
                    return Err(error);
                };
                missing.push(name);
                let Some(parent) = existing.parent() else {
                    return Err(error);
                };
                existing = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

pub struct WorkerStream {
    version: String,
    cwd: PathBuf,
    tools: &'static [&'static str],
    modes: &'static [&'static str],
    model: Option<String>,
    model_checked: bool,
    prompted: bool,
    accepted: bool,
    messages: HashMap<String, Value>,
    result_usage: Option<Value>,
}

impl WorkerStream {
    pub fn new(version: String, cwd: PathBuf, policy: &WorkerPolicy) -> Self {
        Self {
            version,
            cwd,
            tools: policy.profile.tools(),
            modes: policy.profile.modes(),
            model: None,
            model_checked: false,
            prompted: false,
            accepted: false,
            messages: HashMap::new(),
            result_usage: None,
        }
    }

    pub fn set_model(&mut self, model: String) {
        self.model = Some(model);
        self.model_checked = false;
    }

    pub fn mark_prompted(&mut self) {
        self.prompted = true;
    }

    pub fn usage_totals(&self) -> Option<(Value, bool)> {
        if self.messages.is_empty() && self.result_usage.is_none() {
            return None;
        }
        let mut totals = Map::new();
        let mut partial = false;
        for key in [
            INPUT_TOKENS,
            CACHE_READ_INPUT_TOKENS,
            CACHE_CREATION_INPUT_TOKENS,
            OUTPUT_TOKENS,
        ] {
            let reported = self
                .result_usage
                .as_ref()
                .and_then(|usage| usage[key].as_u64());
            partial |= reported.is_none();
            let measured = || {
                if key == OUTPUT_TOKENS || self.messages.is_empty() {
                    return None;
                }
                self.messages
                    .values()
                    .try_fold(0_u64, |sum, usage| sum.checked_add(usage[key].as_u64()?))
            };
            if let Some(value) = reported.or_else(measured) {
                totals.insert(key.into(), value.into());
            }
        }
        Some((Value::Object(totals), partial))
    }

    pub fn accepted(&self) -> bool {
        self.accepted
    }
}

pub struct WorkerEvent {
    pub kind: Option<EventKind>,
    pub event: Value,
    pub rate_limit: Option<ProviderUsage>,
    pub problem: Option<Error>,
    pub failure: Option<String>,
}

/// Workers may compact and retry within their own run. Provider turns must preserve maki's
/// transcript and retry accounting, so their classifier rejects both events.
impl WorkerStream {
    pub fn feed(&mut self, line: &str) -> Result<WorkerEvent, Error> {
        if line.len() > MAX_EVENT_BYTES {
            return Err(Error::EventTooLarge {
                limit: MAX_EVENT_BYTES,
            });
        }
        let event = stream::parse_event(line)?;
        let kind = stream::event_kind(&event, StreamMode::Worker);
        let mut check = |event_kind| -> Result<(), Error> {
            stream::check_start(&event, event_kind, StreamMode::Worker, self.accepted)?;
            if let Some(expected) = self.model.as_deref() {
                let message = if event_kind == EventKind::Stream {
                    &event["event"]["message"]
                } else {
                    &event["message"]
                };
                if event[API_ERROR_MARK] != true {
                    let required = event_kind == EventKind::Stream
                        && event["event"]["type"] == stream::MESSAGE_START;
                    self.model_checked |=
                        stream::generation_model(message, required, expected)?.is_some();
                }
                if event_kind == EventKind::Result
                    && !stream::failed_result(&event)
                    && !self.model_checked
                {
                    return Err(Error::NoModel);
                }
            }
            Ok(())
        };
        let (kind, mut problem) = match kind {
            Ok(kind) => (Some(kind), check(kind).err()),
            Err(error) => (None, Some(error)),
        };
        if kind == Some(EventKind::Init) {
            if !self.prompted {
                problem = Some(Error::UncheckedStart);
            }
            if problem.is_none() {
                problem = checks::init_problem(
                    &event,
                    &self.version,
                    &self.cwd,
                    self.tools.iter().copied(),
                    self.modes,
                    None,
                )
                .map(Error::from);
            }
            self.accepted = problem.is_none();
        }
        let usage = if kind == Some(EventKind::Assistant) {
            &event["message"]["usage"]
        } else {
            &event["usage"]
        };
        let usage = usage.is_object().then(|| usage_counts(usage));
        if kind == Some(EventKind::Assistant)
            && let (Some(id), Some(usage)) = (event["message"]["id"].as_str(), &usage)
        {
            self.messages.insert(id.to_owned(), usage.clone());
        }
        if kind == Some(EventKind::Result) {
            self.result_usage = Some(usage.clone().unwrap_or_default());
        }
        let rate_limit = (kind == Some(EventKind::RateLimit)
            && event["rate_limit_info"].is_object())
        .then(|| plan_usage(&event["rate_limit_info"]));
        let failure = (kind == Some(EventKind::Result))
            .then(|| stream::result_failure(&event))
            .flatten()
            .map(|error| error.to_string());
        Ok(WorkerEvent {
            kind,
            event,
            rate_limit,
            problem,
            failure,
        })
    }
}

fn usage_counts(usage: &Value) -> Value {
    json!({
        INPUT_TOKENS: count(usage, INPUT_TOKENS),
        CACHE_READ_INPUT_TOKENS: count(usage, CACHE_READ_INPUT_TOKENS),
        CACHE_CREATION_INPUT_TOKENS: count(usage, CACHE_CREATION_INPUT_TOKENS),
        OUTPUT_TOKENS: count(usage, OUTPUT_TOKENS),
    })
}

pub fn checkout_root(cwd: &Path) -> Result<PathBuf, Error> {
    let resolved = cwd.canonicalize().map_err(Error::path("resolve", cwd))?;
    checks::checkout_root(&resolved)
}

fn empty_array(value: Option<&Value>) -> bool {
    value.is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
}

fn foreign_path(value: Option<&Value>, ours: &Value) -> Option<String> {
    let paths = value?;
    let Some(paths) = paths.as_array() else {
        return Some("paths given as a table".to_owned());
    };
    paths
        .iter()
        .find(|path| {
            !ours
                .as_array()
                .is_some_and(|allowed| allowed.contains(path))
        })
        .map(|path| {
            path.as_str()
                .map_or_else(|| path.to_string(), str::to_owned)
        })
}

fn sandbox_problem(sandbox: &Value, expected: &Value) -> Option<Problem> {
    let sandbox = match sandbox.as_object() {
        Some(sandbox) => sandbox,
        None => return Some(Problem::SandboxOutside),
    };
    let safe_keys = [
        ENABLED,
        FAIL_IF_UNAVAILABLE,
        AUTO_ALLOW_BASH,
        ALLOW_UNSANDBOXED,
        EXCLUDED_COMMANDS,
        WEAKER_NESTED_SANDBOX,
        NETWORK,
        FILESYSTEM,
    ];
    if sandbox.keys().any(|key| !safe_keys.contains(&key.as_str()))
        || [ENABLED, FAIL_IF_UNAVAILABLE, ALLOW_UNSANDBOXED]
            .iter()
            .any(|key| sandbox.get(*key) != expected.get(*key))
        || !empty_array(sandbox.get(EXCLUDED_COMMANDS))
        || sandbox
            .get(WEAKER_NESTED_SANDBOX)
            .is_some_and(|value| value != false)
    {
        return Some(Problem::SandboxOutside);
    }
    let network = match sandbox.get(NETWORK).and_then(Value::as_object) {
        Some(network) => network,
        None => return Some(Problem::SandboxNetwork),
    };
    let network_keys = [
        STRICT_ALLOWLIST,
        ALLOWED_DOMAINS,
        ALLOW_ALL_UNIX_SOCKETS,
        ALLOW_UNIX_SOCKETS,
        ALLOW_LOCAL_BINDING,
        DENIED_DOMAINS,
    ];
    if network
        .keys()
        .any(|key| !network_keys.contains(&key.as_str()))
        || network.get(STRICT_ALLOWLIST) != expected[NETWORK].get(STRICT_ALLOWLIST)
        || foreign_path(
            network.get(ALLOWED_DOMAINS),
            &expected[NETWORK][ALLOWED_DOMAINS],
        )
        .is_some()
        || !empty_array(network.get(ALLOW_UNIX_SOCKETS))
        || network
            .get(ALLOW_ALL_UNIX_SOCKETS)
            .is_some_and(|value| value != false)
        || network
            .get(ALLOW_LOCAL_BINDING)
            .is_some_and(|value| value != false)
    {
        return Some(Problem::SandboxNetwork);
    }
    let filesystem = match sandbox.get(FILESYSTEM).and_then(Value::as_object) {
        Some(filesystem) => filesystem,
        None => return Some(Problem::SandboxReadDenied),
    };
    let filesystem_keys = [DISABLED, ALLOW_READ, DENY_READ, ALLOW_WRITE, DENY_WRITE];
    if filesystem
        .keys()
        .any(|key| !filesystem_keys.contains(&key.as_str()))
        || filesystem.get(DISABLED).is_some_and(|value| value != false)
    {
        return Some(Problem::SandboxReadDenied);
    }
    let expected = &expected[FILESYSTEM];
    if let Some(path) = foreign_path(filesystem.get(ALLOW_READ), &expected[ALLOW_READ]) {
        return Some(Problem::SandboxRead(path));
    }
    for path in expected[DENY_READ].as_array().into_iter().flatten() {
        if !filesystem
            .get(DENY_READ)
            .and_then(Value::as_array)
            .is_some_and(|denied| denied.contains(path))
        {
            return Some(Problem::SandboxRead(
                path.as_str().unwrap_or_default().to_owned(),
            ));
        }
    }
    foreign_path(filesystem.get(ALLOW_WRITE), &expected[ALLOW_WRITE]).map(Problem::SandboxWrite)
}

fn worker_problem(effective: &Value, expected: &Value) -> Option<Problem> {
    if !effective.is_object() {
        return Some(Problem::NoEffectiveSettings);
    }
    let permissions = &effective[PERMISSIONS_KEY];
    if permissions[BLOCK_OUTSIDE] != expected[PERMISSIONS_KEY][BLOCK_OUTSIDE] {
        return Some(Problem::OutsideProjectReads);
    }
    if !empty_array(permissions.get(ADDITIONAL_DIRECTORIES)) {
        let directories = &permissions[ADDITIONAL_DIRECTORIES];
        let shown = directories.as_array().map_or_else(
            || "directories given as a table".to_owned(),
            |paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        return Some(Problem::AdditionalDirectories(shown));
    }
    for rule in expected[PERMISSIONS_KEY][DENY]
        .as_array()
        .into_iter()
        .flatten()
    {
        if !permissions[DENY]
            .as_array()
            .is_some_and(|denied| denied.contains(rule))
        {
            return Some(Problem::MissingDenyRule(
                rule.as_str().unwrap_or_default().to_owned(),
            ));
        }
    }
    expected
        .get(SANDBOX)
        .and_then(|sandbox| sandbox_problem(&effective[SANDBOX], sandbox))
}

pub async fn checked_profile(
    executable: &Path,
    env: &[(String, String)],
    cwd: &Path,
    denied: &[PathBuf],
    temp_dir: &Path,
    startup_secs: u64,
    cancel: impl Future<Output = ()> + Send,
) -> Result<String, Error> {
    let limits = Limits {
        startup: Duration::from_secs(startup_secs),
        ..Limits::new(Duration::ZERO)
    };
    super::run::checked_profile(
        &Launch {
            executable,
            env,
            project: cwd,
            denied,
            temp_dir,
            limits: &limits,
        },
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        Error, PolicyInput, Problem, WorkerHandshake, WorkerProbe, WorkerProfile, WorkerStream,
        checks, stream,
    };
    use super::{
        SANDBOX, TMPDIR_ENV, WorkerPaths, WorkerPolicy, prepare_launch, sandbox_problem,
        scratch_base, worker_problem,
    };
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::future::ready;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    use test_case::test_case;

    const SECRET: &str = "worker-secret";
    const READ_RULE: &str = "Read(./secrets)";
    const DENIED: &str = "/checkout";
    const OUTSIDE: &str = "/outside";
    const RELATIVE_TEMP: &str = "relative-temp";
    const SCRATCH_FALLBACK: &str = "fallback";
    const PRIVATE_SCRATCH_MODE: u32 = 0o700;
    const UNSAFE_SCRATCH_MODE: u32 = 0o755;

    const MODEL: &str = "claude-test-model";
    const OTHER_MODEL: &str = "claude-other-model";

    #[tokio::test]
    async fn a_probe_cancelled_during_preparation_never_launches() {
        let scratch = tempdir().unwrap();
        let project = tempdir().unwrap();
        let probe = WorkerProbe {
            executable: scratch.path().join("missing-executable"),
            env: Vec::new(),
            project: project.path().to_owned(),
            denied: Vec::new(),
            temp_dir: scratch.path().to_owned(),
            policy: Arc::new(read_policy()),
            model: MODEL.into(),
            startup: Duration::ZERO,
        };
        let (result, kept) = probe.run(ready(())).await;
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(kept.is_none());
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test_case(-1, false => matches Error::WentQuiet ; "signal_before_reply")]
    #[test_case(-1, true => matches Error::StoppedAfterResult ; "signal_after_reply")]
    #[test_case(2, false => matches Error::ExitedEarly(_) ; "error_before_reply")]
    #[test_case(2, true => matches Error::ExitedAfterResult(_) ; "error_after_reply")]
    fn worker_exit_reports_whether_the_reply_completed(code: i32, after_result: bool) -> Error {
        super::worker_exit(code, after_result)
    }

    #[test_case(false ; "before_prompt")]
    #[test_case(true ; "after_prompt")]
    fn worker_init_requires_a_prompt(prompted: bool) {
        let version = checks::minimum_version();
        let mut stream = WorkerStream::new(version.clone(), PathBuf::from(DENIED), &read_policy());
        if prompted {
            stream.mark_prompted();
        }
        let init = json!({"type":"system", "subtype":"init", "apiKeySource":"none", "claude_code_version":version, "permissionMode":checks::DEFAULT_MODE, "tools": WorkerProfile::Read.tools(), "mcp_servers":[], "plugins":[], "cwd":DENIED});
        let event = stream.feed(&init.to_string()).unwrap();
        assert_eq!(
            matches!(event.problem, Some(Error::UncheckedStart)),
            !prompted
        );
        assert_eq!(stream.accepted(), prompted);
    }

    #[test_case("CLAUDE_CODE_OAUTH_TOKEN", SECRET, true ; "token")]
    #[test_case("claude_code_oauth_token", SECRET, true ; "lowercase_token")]
    #[test_case("HTTPS_PROXY", "http://user:worker-secret@proxy:3128/", true ; "proxy_password")]
    #[test_case("http_proxy", "worker-secret@proxy:3128", true ; "proxy_without_scheme")]
    #[test_case("HTTPS_PROXY", "http://proxy:3128/a@b", false ; "path_is_not_a_login")]
    #[test_case("PATH", "/bin", false ; "no_secret")]
    fn coding_shell_refuses_secrets_without_disclosing_them(
        name: &str,
        value: &str,
        refused: bool,
    ) {
        let problem = super::shell_secret_problem(&HashMap::from([(name.into(), value.into())]));
        assert_eq!(problem.is_some(), refused);
        if let Some(problem) = problem {
            assert!(problem.contains(name));
            assert!(!problem.contains(SECRET));
        }
    }

    fn read_policy() -> WorkerPolicy {
        WorkerPolicy::from(PolicyInput {
            deny: Vec::new(),
            sandbox: None,
        })
    }

    fn read_stream() -> WorkerStream {
        let version = checks::minimum_version();
        let mut stream = WorkerStream::new(version.clone(), PathBuf::from(DENIED), &read_policy());
        stream.mark_prompted();
        let init = json!({"type":"system", "subtype":"init", "apiKeySource":"none", "claude_code_version":version, "permissionMode":checks::DEFAULT_MODE, "tools": WorkerProfile::Read.tools(), "mcp_servers":[], "plugins":[], "cwd":DENIED});
        assert!(stream.feed(&init.to_string()).unwrap().problem.is_none());
        stream.set_model(MODEL.into());
        stream
    }

    #[test_case(None => matches Some(Error::NoModel) ; "message_start_without_model")]
    #[test_case(Some(OTHER_MODEL) => matches Some(Error::OtherModel { .. }) ; "different_model")]
    #[test_case(Some(MODEL) => matches None ; "requested_model")]
    fn worker_message_start_checks_the_requested_model(model: Option<&str>) -> Option<Error> {
        let mut worker = read_stream();
        let event = json!({"type":"stream_event", "event":{"type":"message_start", "message":{"id":"m1", "model":model}}});
        worker.feed(&event.to_string()).unwrap().problem
    }

    #[test_case(false ; "never_reported")]
    #[test_case(true ; "reset_after_a_previous_model")]
    fn worker_success_requires_a_reported_model(reset: bool) {
        let mut worker = read_stream();
        if reset {
            let event = json!({"type":"stream_event", "event":{"type":"message_start", "message":{"id":"m1", "model":MODEL}}});
            assert!(worker.feed(&event.to_string()).unwrap().problem.is_none());
            worker.set_model(MODEL.into());
        }
        let result = worker
            .feed(&json!({"type":"result", "subtype":"success", "result":"done"}).to_string())
            .unwrap();
        assert!(matches!(result.problem, Some(Error::NoModel)));
    }

    fn handshake() -> WorkerHandshake {
        let policy = read_policy();
        WorkerHandshake::new(Arc::new(policy))
    }

    #[test]
    fn worker_checks_the_account_before_the_policy() {
        let mut handshake = handshake();
        let responses = [
            json!({"current_permission_mode":"default", "account":{"apiProvider":"other", "subscriptionType":"Claude Pro"}}),
            Value::Null,
            Value::Null,
        ];
        let mut result = Ok(false);
        for (step, response) in checks::HANDSHAKE.iter().zip(responses) {
            result = handshake.answer(
                &json!({"request_id":step.id, "subtype":stream::SUCCESS, "response":response}),
            );
        }
        assert!(matches!(
            result,
            Err(Error::Check(Problem::OtherProvider(_)))
        ));
    }

    #[test]
    fn a_complete_worker_handshake_accepts_the_policy() {
        let policy = read_policy();
        let settings = policy.settings().clone();
        let mut handshake = WorkerHandshake::new(Arc::new(policy));
        let responses = [
            json!({"current_permission_mode":"default", "account":{"apiProvider":"firstParty", "subscriptionType":"Claude Pro"}}),
            json!({"effective":settings, "sources":[{"source":"flagSettings", "settings":settings}]}),
            json!({"hooks":[], "policy":{"allDisabled":true,"policyHookCount":0}}),
        ];
        for (index, (step, response)) in checks::HANDSHAKE.iter().zip(responses).enumerate() {
            let complete = handshake
                .answer(
                    &json!({"request_id":step.id, "subtype":stream::SUCCESS, "response":response}),
                )
                .unwrap();
            assert_eq!(complete, index == checks::HANDSHAKE.len() - 1);
        }
    }

    #[test]
    fn a_relative_tmpdir_is_absolute_in_the_child_environment() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir(&cwd).unwrap();
        let relative = tempfile::Builder::new()
            .prefix(RELATIVE_TEMP)
            .tempdir_in(env::current_dir().unwrap())
            .unwrap();
        let name = relative.path().file_name().unwrap().to_str().unwrap();
        let prepared = prepare_launch(
            vec![(TMPDIR_ENV.into(), name.into())],
            Some(root.path().join("login")),
            &cwd,
        )
        .unwrap();
        let expected = relative.path().canonicalize().unwrap();
        assert_eq!(prepared.temp_dir, expected);
        let (_, given) = prepared
            .env
            .iter()
            .find(|(name, _)| name == TMPDIR_ENV)
            .unwrap();
        assert_eq!(given, expected.to_str().unwrap());
    }

    #[test_case(PRIVATE_SCRATCH_MODE, true ; "private_fallback")]
    #[test_case(UNSAFE_SCRATCH_MODE, false ; "shared_fallback_is_refused")]
    fn scratch_fallback_requires_private_permissions(mode: u32, accepted: bool) {
        let project = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let fallback = outside.path().join(SCRATCH_FALLBACK);
        fs::create_dir(&fallback).unwrap();
        fs::set_permissions(&fallback, fs::Permissions::from_mode(mode)).unwrap();
        let result = scratch_base(project.path(), project.path(), &[], vec![fallback.clone()]);
        if accepted {
            assert_eq!(result.unwrap(), fallback.canonicalize().unwrap());
        } else {
            assert!(matches!(result, Err(Error::NoScratch(path)) if path == project.path()));
            assert_eq!(
                fs::metadata(&fallback).unwrap().permissions().mode() & super::PERMISSION_BITS,
                mode
            );
        }
    }

    #[test_case(false ; "non_utf8_preferred_target")]
    #[test_case(true ; "non_utf8_fallback_target")]
    fn scratch_skips_paths_whose_resolved_names_are_not_utf8(fallback: bool) {
        let project = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let invalid = outside.path().join(OsStr::from_bytes(b"scratch-\xff"));
        fs::create_dir(&invalid).unwrap();
        fs::set_permissions(&invalid, fs::Permissions::from_mode(PRIVATE_SCRATCH_MODE)).unwrap();
        let alias = outside.path().join("alias");
        symlink(&invalid, &alias).unwrap();
        let safe = outside.path().join(SCRATCH_FALLBACK);
        let preferred = if fallback { project.path() } else { &alias };
        let result = scratch_base(preferred, project.path(), &[], vec![invalid, safe.clone()]);
        assert_eq!(result.unwrap(), safe.canonicalize().unwrap());
    }

    fn expected() -> Value {
        WorkerPolicy::from(PolicyInput {
            deny: vec![READ_RULE.into()],
            sandbox: Some(WorkerPaths {
                deny_read: vec![DENIED.into()],
                allow_read: Vec::new(),
                allow_write: Vec::new(),
            }),
        })
        .settings()
        .clone()
    }

    #[test_case("/excludedCommands", json!(null), Problem::SandboxOutside ; "null_command_exclusions")]
    #[test_case("/network/allowedDomains", json!([null]), Problem::SandboxNetwork ; "null_domain_entry")]
    #[test_case("/network/allowAllUnixSockets", json!(null), Problem::SandboxNetwork ; "null_socket_policy")]
    #[test_case("/network/strictAllowlist", json!(false), Problem::SandboxNetwork ; "network_allowlist_disabled")]
    #[test_case("/enabled", json!(false), Problem::SandboxOutside ; "sandbox_disabled")]
    #[test_case("/failIfUnavailable", json!(false), Problem::SandboxOutside ; "sandbox_fallback")]
    #[test_case("/allowUnsandboxedCommands", json!(true), Problem::SandboxOutside ; "unsandboxed_commands")]
    #[test_case("/filesystem/allowRead", json!([null]), Problem::SandboxRead("null".into()) ; "null_read_entry")]
    #[test_case("/filesystem/allowWrite", json!([OUTSIDE]), Problem::SandboxWrite(OUTSIDE.into()) ; "foreign_write_path")]
    #[test_case("/unknownEscape", json!(true), Problem::SandboxOutside ; "unknown_sandbox_setting")]
    fn effective_sandbox_rejects_nulls_and_unknown_escape_settings(
        path: &str,
        value: Value,
        want: Problem,
    ) {
        let expected = expected();
        let mut sandbox = expected[SANDBOX].clone();
        let (parent, key) = path.rsplit_once('/').unwrap();
        sandbox
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), value);
        let actual = sandbox_problem(&sandbox, &expected[SANDBOX]).unwrap();
        assert_eq!(actual, want);
    }

    #[test_case("", json!(null), Problem::NoEffectiveSettings ; "missing_effective_settings")]
    #[test_case("/permissions/blockReadsOutsideWorkingDirectories", json!(false), Problem::OutsideProjectReads ; "reads_outside_project")]
    #[test_case("/permissions/additionalDirectories", json!([null]), Problem::AdditionalDirectories(String::new()) ; "null_additional_directories")]
    #[test_case("/permissions/additionalDirectories", json!({"root":"/"}), Problem::AdditionalDirectories("directories given as a table".into()) ; "object_additional_directories")]
    #[test_case("/permissions/deny", json!([]), Problem::MissingDenyRule(READ_RULE.into()) ; "missing_deny_rule")]
    fn effective_worker_permissions_report_the_rule(path: &str, value: Value, want: Problem) {
        let expected = expected();
        let mut effective = expected.clone();
        if let Some((parent, key)) = path.rsplit_once('/') {
            effective
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(key.to_owned(), value);
        } else {
            effective = value;
        }
        assert_eq!(worker_problem(&effective, &expected), Some(want));
    }
}
