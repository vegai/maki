//! Shared launch validation for the provider and the Lua worker.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf, absolute};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::ProviderUsage;

use super::checks;
use super::error::{Error, Problem};
use super::stream::{
    self, API_ERROR_MARK, ASSISTANT_EVENT, EventKind, Handshake, RATE_LIMIT_EVENT, StreamMode,
    count,
};

pub use super::checks::{GIT_ENTRY, git_directory};
pub use super::run::{VersionCheck, invalidate_profile as invalidate_version};
use super::stream::usage_of as plan_usage;

pub const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;
pub const STARTUP_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_EXECUTABLE: &str = "claude";
pub const DEFAULT_MAX_CONCURRENT: usize = 8;
pub const MIN_MAX_CONCURRENT: usize = 1;
pub(super) const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub(super) const HOME_ENV: &str = "HOME";
const TMPDIR_ENV: &str = "TMPDIR";
#[cfg(unix)]
const UNIX_TEMP_DIR: &str = "/tmp";
const PROBE_PATH_NOT_UTF8: &str = "probe path is not valid UTF-8";
const PERMISSIONS: &str = "permissions";
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

const BASE_ARGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--no-session-persistence",
    "--restricted",
    "--permission-prompts",
    "none",
    "--setting-sources",
    "",
    "--strict-mcp-config",
];

pub fn shell_secret_problem(env: &HashMap<String, String>) -> Option<String> {
    for (name, value) in env {
        let upper = name.to_ascii_uppercase();
        if upper == super::OAUTH_TOKEN_ENV {
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

pub fn base_args(model: &str, tools: &str, mode: &str, settings: &str) -> Vec<String> {
    let mut args: Vec<_> = BASE_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    args.extend(
        [
            "--model",
            model,
            "--tools",
            tools,
            "--permission-mode",
            mode,
            "--settings",
            settings,
        ]
        .map(str::to_owned),
    );
    args
}

pub(crate) fn base_settings() -> Value {
    json!({ "disableAllHooks": true, "autoMemoryEnabled": false, "claudeMdExcludes": ["**"], "disableClaudeAiConnectors": true })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPolicy {
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
    pub fn settings(&self) -> Value {
        let mut settings = base_settings();
        settings[PERMISSIONS] = json!({
            (BLOCK_OUTSIDE): true,
            (DENY): self.deny,
        });
        if let Some(paths) = &self.sandbox {
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
        settings
    }
}

pub fn handshake() -> Value {
    json!(checks::HANDSHAKE)
}

pub struct WorkerHandshake {
    answers: Handshake,
    modes: Vec<String>,
    policy: WorkerPolicy,
}

impl WorkerHandshake {
    pub fn new(modes: Vec<String>, policy: WorkerPolicy) -> Self {
        Self {
            answers: Handshake::default(),
            modes,
            policy,
        }
    }

    pub fn answer(&mut self, response: &Value) -> Result<bool, String> {
        if !self
            .answers
            .record(response)
            .map_err(|error| error.to_string())?
        {
            return Ok(false);
        }
        self.finish()?;
        Ok(true)
    }

    pub fn finish(&self) -> Result<(), String> {
        self.answers
            .check(
                |account| checks::account_problem_in(account, &self.modes),
                |settings, hooks| {
                    checks::policy_problem_in(settings, hooks, false).or_else(|| {
                        worker_problem(&settings["effective"], &self.policy.settings())
                            .map(Problem::WorkerPolicy)
                    })
                },
            )
            .map_err(|error| error.to_string())
    }

    pub fn account(&self) -> &Value {
        self.answers.account()
    }
}

pub struct LaunchPreparation {
    pub env: Vec<(String, String)>,
    pub withheld: Vec<String>,
    pub config_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub local_dirs: Vec<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub denied: Vec<PathBuf>,
}

fn default_temp_dir() -> PathBuf {
    let path = env::temp_dir();
    #[cfg(unix)]
    if path.as_os_str().is_empty() {
        return PathBuf::from(UNIX_TEMP_DIR);
    }
    path
}

fn env_value(vars: &[(String, String)], name: &str) -> Option<OsString> {
    vars.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| OsString::from(value))
}

pub(super) fn prepare_launch(
    vars: Vec<(String, String)>,
    configured: Option<PathBuf>,
    cwd: &Path,
) -> Result<LaunchPreparation, Error> {
    let var = |name| env_value(&vars, name);
    let config_dir = login_dir(&vars, configured.as_deref())?;
    let temp_dir = var(TMPDIR_ENV)
        .filter(|path| !path.is_empty())
        .map_or_else(default_temp_dir, PathBuf::from);
    let temp_dir = absolute(&temp_dir).map_err(|source| Error::Path {
        what: "resolve the temporary directory",
        path: temp_dir,
        source,
    })?;
    let (mut env, withheld) = checks::launch_env(vars);
    env.retain(|(name, value)| {
        ![CONFIG_DIR_ENV, TMPDIR_ENV].contains(&name.as_str()) || !value.is_empty()
    });
    for (name, value) in &mut env {
        if name == TMPDIR_ENV {
            *value = temp_dir.display().to_string();
        }
    }
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
    let denied = local_dirs.iter().chain(git_dir.iter()).cloned().collect();
    Ok(LaunchPreparation {
        env,
        withheld,
        config_dir,
        temp_dir,
        local_dirs,
        git_dir,
        denied,
    })
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

pub fn worker_launch(
    vars: HashMap<String, String>,
    configured: Option<String>,
    cwd: &Path,
) -> Result<LaunchPreparation, String> {
    prepare_launch(
        vars.into_iter().collect(),
        configured.map(PathBuf::from),
        cwd,
    )
    .map_err(|error| error.to_string())
}

/// Make a worker probe directory with the provider's ownership tag and checkout checks.
pub async fn private_dir(
    base: Option<PathBuf>,
    project: PathBuf,
    denied: Vec<PathBuf>,
) -> Result<String, String> {
    smol::unblock(move || {
        let base = base
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(default_temp_dir);
        let (dir, path) =
            super::run::private_dir(&base, &project, &denied).map_err(|error| error.to_string())?;
        let name = path.to_str().ok_or(PROBE_PATH_NOT_UTF8)?.to_owned();
        dir.keep();
        Ok(name)
    })
    .await
}

pub fn init_problem(
    event: &Value,
    version: &str,
    cwd: &Path,
    tools: HashSet<String>,
    modes: &[String],
) -> Option<String> {
    checks::init_problem_in(event, version, cwd, &tools, modes, None)
        .map(|problem| problem.to_string())
}

pub struct WorkerEvent {
    pub event: Value,
    pub usage: Option<Value>,
    pub rate_limit: Option<ProviderUsage>,
    pub model_checked: bool,
    pub problem: Option<String>,
    pub failure: Option<String>,
}

/// Workers may compact and retry within their own run. Provider turns must preserve maki's
/// transcript and retry accounting, so their classifier rejects both events.
pub fn classify(
    line: &str,
    accepted: bool,
    expected_model: Option<&str>,
    mut model_checked: bool,
) -> Result<WorkerEvent, String> {
    let event = stream::parse_event(line).map_err(|error| error.to_string())?;
    let mut check = || -> Result<(), Error> {
        let kind = stream::classify(&event, StreamMode::Worker, accepted)?;
        if let Some(expected) = expected_model {
            let message = if kind == EventKind::Stream {
                &event["event"]["message"]
            } else {
                &event["message"]
            };
            if event[API_ERROR_MARK] != true
                && let Some(ran) = message["model"].as_str()
            {
                stream::check_model(ran, expected)?;
                model_checked = true;
            }
            if kind == EventKind::Result
                && event["subtype"] == stream::SUCCESS
                && event["is_error"] != true
                && !model_checked
            {
                return Err(Error::NoModel);
            }
        }
        Ok(())
    };
    let problem = check().err().map(|error| error.to_string());
    let usage = if event["type"] == ASSISTANT_EVENT {
        &event["message"]["usage"]
    } else {
        &event["usage"]
    };
    let usage = usage.is_object().then(|| usage_counts(usage));
    let rate_limit = (event["type"] == RATE_LIMIT_EVENT && event["rate_limit_info"].is_object())
        .then(|| plan_usage(&event["rate_limit_info"]));
    let failure = (event["type"] == stream::RESULT_EVENT)
        .then(|| stream::result_failure(&event))
        .flatten();
    Ok(WorkerEvent {
        event,
        usage,
        rate_limit,
        model_checked,
        problem,
        failure,
    })
}

fn usage_counts(usage: &Value) -> Value {
    json!({
        "input_tokens": count(usage, "input_tokens"),
        "cache_read_input_tokens": count(usage, "cache_read_input_tokens"),
        "cache_creation_input_tokens": count(usage, "cache_creation_input_tokens"),
        "output_tokens": count(usage, "output_tokens"),
    })
}

pub fn resolved_model(account: &Value, requested: &str) -> Option<String> {
    account["models"]
        .as_array()?
        .iter()
        .find(|offer| offer["value"] == requested)?["resolvedModel"]
        .as_str()
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

pub async fn version_check(executable: &Path) -> Result<VersionCheck, String> {
    super::run::version_check(executable)
        .await
        .map_err(|error| error.to_string())
}

pub fn checkout_root(cwd: &Path) -> Result<PathBuf, String> {
    let cwd = cwd.canonicalize().map_err(|error| error.to_string())?;
    checks::checkout_root(&cwd).map_err(|error| error.to_string())
}

pub fn subscription_type(account: &Value) -> Option<String> {
    account[checks::ACCOUNT][checks::SUBSCRIPTION_TYPE]
        .as_str()
        .map(str::to_owned)
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

fn sandbox_problem(sandbox: &Value, expected: &Value) -> Option<String> {
    let outside = "Claude Code can run shell commands outside its sandbox";
    let network_error = "Claude Code's sandbox lets shell commands use the network";
    let read_error = "Claude Code's sandbox lets shell commands read paths that maki denies";
    let sandbox = match sandbox.as_object() {
        Some(sandbox) => sandbox,
        None => return Some(outside.to_owned()),
    };
    let safe_keys = [
        ENABLED,
        FAIL_IF_UNAVAILABLE,
        AUTO_ALLOW_BASH,
        ALLOW_UNSANDBOXED,
        "excludedCommands",
        "enableWeakerNestedSandbox",
        NETWORK,
        FILESYSTEM,
    ];
    if sandbox.keys().any(|key| !safe_keys.contains(&key.as_str()))
        || [ENABLED, FAIL_IF_UNAVAILABLE, ALLOW_UNSANDBOXED]
            .iter()
            .any(|key| sandbox.get(*key) != expected.get(*key))
        || !empty_array(sandbox.get("excludedCommands"))
        || sandbox
            .get("enableWeakerNestedSandbox")
            .is_some_and(|value| value != false)
    {
        return Some(outside.to_owned());
    }
    let network = match sandbox.get(NETWORK).and_then(Value::as_object) {
        Some(network) => network,
        None => return Some(network_error.to_owned()),
    };
    let network_keys = [
        STRICT_ALLOWLIST,
        ALLOWED_DOMAINS,
        "allowAllUnixSockets",
        "allowUnixSockets",
        "allowLocalBinding",
        "deniedDomains",
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
        || !empty_array(network.get("allowUnixSockets"))
        || network
            .get("allowAllUnixSockets")
            .is_some_and(|value| value != false)
        || network
            .get("allowLocalBinding")
            .is_some_and(|value| value != false)
    {
        return Some(network_error.to_owned());
    }
    let filesystem = match sandbox.get(FILESYSTEM).and_then(Value::as_object) {
        Some(filesystem) => filesystem,
        None => return Some(read_error.to_owned()),
    };
    let filesystem_keys = ["disabled", ALLOW_READ, DENY_READ, ALLOW_WRITE, "denyWrite"];
    if filesystem
        .keys()
        .any(|key| !filesystem_keys.contains(&key.as_str()))
        || filesystem
            .get("disabled")
            .is_some_and(|value| value != false)
    {
        return Some(read_error.to_owned());
    }
    let expected = &expected[FILESYSTEM];
    if let Some(path) = foreign_path(filesystem.get(ALLOW_READ), &expected[ALLOW_READ]) {
        return Some(format!(
            "Claude Code's sandbox lets shell commands read {path}"
        ));
    }
    for path in expected[DENY_READ].as_array().into_iter().flatten() {
        if !filesystem
            .get(DENY_READ)
            .and_then(Value::as_array)
            .is_some_and(|denied| denied.contains(path))
        {
            return Some(format!(
                "Claude Code's sandbox lets shell commands read {}",
                path.as_str().unwrap_or_default()
            ));
        }
    }
    foreign_path(filesystem.get(ALLOW_WRITE), &expected[ALLOW_WRITE])
        .map(|path| format!("Claude Code's sandbox lets shell commands write to {path}"))
}

fn worker_problem(effective: &Value, expected: &Value) -> Option<String> {
    if !effective.is_object() {
        return Some("Claude Code does not report effective settings".to_owned());
    }
    let permissions = &effective[PERMISSIONS];
    if permissions[BLOCK_OUTSIDE] != expected[PERMISSIONS][BLOCK_OUTSIDE] {
        return Some("Claude Code does not block reads outside the project".to_owned());
    }
    if !empty_array(permissions.get("additionalDirectories")) {
        let directories = &permissions["additionalDirectories"];
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
        return Some(format!("Claude Code can also read {shown}"));
    }
    for rule in expected[PERMISSIONS][DENY].as_array().into_iter().flatten() {
        if !permissions[DENY]
            .as_array()
            .is_some_and(|denied| denied.contains(rule))
        {
            return Some(format!(
                "Claude Code does not apply the deny rule {}",
                rule.as_str().unwrap_or_default()
            ));
        }
    }
    expected
        .get(SANDBOX)
        .and_then(|sandbox| sandbox_problem(&effective[SANDBOX], sandbox))
}

#[cfg(test)]
mod tests {
    use super::{
        PERMISSIONS, SANDBOX, TMPDIR_ENV, WorkerPaths, WorkerPolicy, prepare_launch,
        sandbox_problem, worker_problem,
    };
    use serde_json::{Value, json};
    use std::env;
    use std::fs;
    use tempfile::tempdir;
    use test_case::test_case;

    const READ_RULE: &str = "Read(./secrets)";
    const DENIED: &str = "/checkout";
    const OUTSIDE: &str = "/outside";
    const RELATIVE_TEMP: &str = "relative-temp";

    #[test]
    fn a_relative_tmpdir_is_absolute_in_the_child_environment() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir(&cwd).unwrap();
        let prepared = prepare_launch(
            vec![(TMPDIR_ENV.into(), RELATIVE_TEMP.into())],
            Some(root.path().join("login")),
            &cwd,
        )
        .unwrap();
        let expected = env::current_dir().unwrap().join(RELATIVE_TEMP);
        assert_eq!(prepared.temp_dir, expected);
        let (_, given) = prepared
            .env
            .iter()
            .find(|(name, _)| name == TMPDIR_ENV)
            .unwrap();
        assert_eq!(given, expected.to_str().unwrap());
    }

    fn expected() -> Value {
        WorkerPolicy {
            deny: vec![READ_RULE.into()],
            sandbox: Some(WorkerPaths {
                deny_read: vec![DENIED.into()],
                allow_read: Vec::new(),
                allow_write: Vec::new(),
            }),
        }
        .settings()
    }

    #[test_case("/excludedCommands", json!(null) ; "null_command_exclusions")]
    #[test_case("/network/allowedDomains", json!([null]) ; "null_domain_entry")]
    #[test_case("/network/allowAllUnixSockets", json!(null) ; "null_socket_policy")]
    #[test_case("/network/strictAllowlist", json!(false) ; "network_allowlist_disabled")]
    #[test_case("/enabled", json!(false) ; "sandbox_disabled")]
    #[test_case("/failIfUnavailable", json!(false) ; "sandbox_fallback")]
    #[test_case("/allowUnsandboxedCommands", json!(true) ; "unsandboxed_commands")]
    #[test_case("/filesystem/allowRead", json!([null]) ; "null_read_entry")]
    #[test_case("/filesystem/allowWrite", json!([OUTSIDE]) ; "foreign_write_path")]
    #[test_case("/unknownEscape", json!(true) ; "unknown_sandbox_setting")]
    fn effective_sandbox_rejects_nulls_and_unknown_escape_settings(path: &str, value: Value) {
        let expected = expected();
        let mut sandbox = expected[SANDBOX].clone();
        let (parent, key) = path.rsplit_once('/').unwrap();
        sandbox
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), value);
        assert!(sandbox_problem(&sandbox, &expected[SANDBOX]).is_some());
    }

    #[test]
    fn null_additional_directories_are_not_an_empty_list() {
        let expected = expected();
        let mut effective = expected.clone();
        effective[PERMISSIONS]["additionalDirectories"] = json!([null]);
        assert!(worker_problem(&effective, &expected).is_some());
    }
}
