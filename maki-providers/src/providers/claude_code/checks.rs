//! The rules that decide whether a Claude Code process may serve a request.
//! Native Lua helpers apply these checks to plugin workers.
//! Here the process hands tool calls to maki and runs no tools itself.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use maki_storage::paths::normalize_path;
#[cfg(unix)]
use rustix::fs::{Mode, OFlags, open};
use serde::Serialize;
use serde_json::{Map, Value};

use super::OAUTH_TOKEN_ENV;
use super::error::{Error, Problem};
use super::transcript::SERVER;

pub(crate) const MINIMUM_VERSION: [u64; 3] = [2, 1, 284];
const SYSTEMS: &[&str] = &["linux"];
pub(crate) const ACCOUNT_ANSWER: &str = "account";
pub(crate) const SETTINGS_ANSWER: &str = "settings";
pub(crate) const HOOKS_ANSWER: &str = "hooks";
pub(crate) const HANDSHAKE: &[HandshakeStep] = &[
    HandshakeStep {
        id: ACCOUNT_ANSWER,
        subtype: "initialize",
    },
    HandshakeStep {
        id: SETTINGS_ANSWER,
        subtype: "get_settings",
    },
    HandshakeStep {
        id: HOOKS_ANSWER,
        subtype: "get_hooks_listing",
    },
];
const NO_PROXY: &str = "NO_PROXY";
const LOOPBACK: &str = "127.0.0.1";
const ESSENTIAL_TRAFFIC_ONLY: (&str, &str) = ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
pub(super) const PROXY_ENV: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY"];
const PASSED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TMPDIR",
    "TZ",
    PROXY_ENV[0],
    PROXY_ENV[1],
    NO_PROXY,
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CLAUDE_CONFIG_DIR",
    OAUTH_TOKEN_ENV,
];
const PASSED_ENV_PREFIXES: &[&str] = &["LC_", "XDG_"];
const ROUTE_ENV: &[&str] = &[
    "CLAUDE_CODE_API_BASE_URL",
    OAUTH_TOKEN_ENV,
    "CLAUDE_CONFIG_DIR",
];
const ROUTE_ENV_PREFIXES: &[&str] = &["CLAUDE_CODE_USE_", "ANTHROPIC_"];
const HARMLESS_ENV: &[&str] = &[
    "CLAUDE_CODE_USE_COWORK_PLUGINS",
    "CLAUDE_CODE_USE_NATIVE_FILE_SEARCH",
    "CLAUDE_CODE_USE_POWERSHELL_TOOL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_CUSTOM_MODEL_OPTION",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
];
const ROUTE_SETTINGS: &[&str] = &[
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
    "forceLoginOrgUUID",
];
const LOGIN_METHOD_KEY: &str = "forceLoginMethod";
const SUBSCRIPTION_LOGIN_METHOD: &str = "claudeai";
const HARMLESS_POLICY: &[&str] = &[
    "availableModels",
    "cleanupPeriodDays",
    "companyAnnouncements",
    "disableAllHooks",
    "disableClaudeAiConnectors",
    "forceLoginOrgUUID",
    "includeCoAuthoredBy",
    "model",
    "permissions",
];
const PERMISSIONS_KEY: &str = "permissions";
const HARMLESS_POLICY_PERMISSIONS: &[&str] = &["deny", "disableBypassPermissionsMode"];
const ENV_KEY: &str = "env";
pub(crate) const FIRST_PARTY: &str = "firstParty";
pub(crate) const NO_KEY_SOURCE: &str = "none";
pub(crate) const FLAG_SOURCE: &str = "flagSettings";
const POLICY_SOURCE: &str = "policySettings";
pub(crate) const CLAUDE_DIR: &str = ".claude";
const SETTINGS_FILE: &str = "settings.json";
const LOCAL_SETTINGS_FILE: &str = "settings.local.json";
const BUILTIN_PLUGIN_MARKER: &str = "builtin";
const UNREADABLE_FILE: &str = "maki cannot read it";
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_GIT_PATH_BYTES: u64 = 4096;
const SETTINGS_TOO_LARGE: &str = "settings file exceeds the 1 MiB limit";
const GIT_PATH_TOO_LARGE: &str = "Git path file exceeds the 4 KiB limit";
const NOT_A_FILE: &str = "path is not a regular file";
const NOT_AN_OBJECT: &str = "is not a JSON object";

pub(crate) const DEFAULT_MODE: &str = "default";
pub(crate) const CONNECTED: &str = "connected";
const UNNAMED_SOURCE: &str = "unnamed";
pub const GIT_ENTRY: &str = ".git";
const GITDIR_PREFIX: &str = "gitdir:";
const COMMONDIR_FILE: &str = "commondir";

// The fields of Claude Code's answers and init event that the checks read.
pub(super) const ACCOUNT: &str = "account";
const API_KEY_SOURCE: &str = "apiKeySource";
const API_PROVIDER: &str = "apiProvider";
pub(super) const SUBSCRIPTION_TYPE: &str = "subscriptionType";
const START_MODE: &str = "current_permission_mode";
const VERSION: &str = "claude_code_version";
const RUN_MODE: &str = "permissionMode";
const TOOLS: &str = "tools";
const MCP_SERVERS: &str = "mcp_servers";
const NAME: &str = "name";
const STATUS: &str = "status";
const PLUGINS: &str = "plugins";
const SOURCE: &str = "source";
const PLUGIN_PATH: &str = "path";
const CWD: &str = "cwd";
const SOURCES: &str = "sources";
const ERRORS: &str = "errors";
const SETTINGS: &str = "settings";
const EFFECTIVE: &str = "effective";
pub(super) const AUTO_COMPACT: &str = "autoCompactEnabled";
const HOOKS: &str = "hooks";
const POLICY: &str = "policy";
const ALL_DISABLED: &str = "allDisabled";
const DISABLED: &str = "disabled";

#[derive(Debug, Serialize)]
pub(crate) struct HandshakeStep {
    pub id: &'static str,
    pub subtype: &'static str,
}

fn version_parts(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.').map(|part| {
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    });
    let parsed = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(parsed)
}

pub(super) fn require_system(os: &str) -> Result<(), Error> {
    if !SYSTEMS.contains(&os.to_ascii_lowercase().as_str()) {
        return Err(Error::UnsupportedSystem {
            os: os.to_owned(),
            systems: SYSTEMS.join(", "),
        });
    }
    Ok(())
}

/// A first word that is not `major.minor.patch` stops the request.
pub(crate) fn profile(output: &str, os: &str) -> Result<String, Error> {
    let version = output.split_whitespace().next().unwrap_or_default();
    let parts = version_parts(version).ok_or_else(|| Error::UnknownVersion(output.to_owned()))?;
    let minimum = MINIMUM_VERSION;
    if parts < minimum {
        return Err(Error::TooOld {
            version: version.to_owned(),
            oldest: minimum.map(|part| part.to_string()).join("."),
        });
    }
    require_system(os)?;
    Ok(version.to_owned())
}

fn passed(name: &str) -> bool {
    PASSED_ENV.contains(&name)
        || PASSED_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// A new `CLAUDE_CODE_USE_*` cloud switch fails closed, and only the model
/// variables that Claude Code knows pass as `ANTHROPIC_*`.
fn routes_login(name: &str) -> bool {
    ROUTE_ENV.contains(&name)
        || (ROUTE_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && !HARMLESS_ENV.contains(&name))
}

/// Also returns the sorted names held back because they can change the
/// login.
pub(crate) fn launch_env(
    environ: impl IntoIterator<Item = (String, String)>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut env = Vec::new();
    let mut withheld = Vec::new();
    for (name, value) in environ {
        let upper = name.to_ascii_uppercase();
        if passed(&upper) {
            env.push((name, value));
        } else if routes_login(&upper) {
            withheld.push(name);
        }
    }
    for (name, value) in &mut env {
        if name.eq_ignore_ascii_case(NO_PROXY)
            && !value.split(',').any(|host| host.trim() == LOOPBACK)
        {
            if !value.is_empty() {
                value.push(',');
            }
            value.push_str(LOOPBACK);
        }
    }
    if !env
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(NO_PROXY))
    {
        env.push((NO_PROXY.to_owned(), LOOPBACK.to_owned()));
    }
    env.push((
        ESSENTIAL_TRAFFIC_ONLY.0.to_owned(),
        ESSENTIAL_TRAFFIC_ONLY.1.to_owned(),
    ));
    withheld.sort();
    (env, withheld)
}

pub fn git_directory(entry: &Path) -> io::Result<PathBuf> {
    if fs::metadata(entry)?.is_dir() {
        return Ok(entry.to_owned());
    }
    let text = limited_text(entry, MAX_GIT_PATH_BYTES, GIT_PATH_TOO_LARGE)?;
    let pointed = text
        .trim()
        .strip_prefix(GITDIR_PREFIX)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| io::Error::other("checkout has an invalid Git directory pointer"))?;
    Ok(normalize_path(
        &entry.parent().unwrap_or(entry).join(pointed),
    ))
}

/// A submodule has no `commondir` and returns `None`. Unreadable git paths must fail
/// because the primary checkout can contain local settings. Resolve paths lexically, as
/// Claude Code and the plugin do.
fn main_checkout(git_file: &Path) -> io::Result<(Option<PathBuf>, Option<PathBuf>)> {
    let gitdir = git_directory(git_file)?;
    let common = match limited_text(
        &gitdir.join(COMMONDIR_FILE),
        MAX_GIT_PATH_BYTES,
        GIT_PATH_TOO_LARGE,
    ) {
        Ok(common) => common,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok((None, Some(normalize_path(&gitdir))));
        }
        Err(e) => return Err(e),
    };
    let common = normalize_path(&gitdir.join(common.trim()));
    fs::metadata(&common)?;
    let main = (common.file_name() == Some(GIT_ENTRY.as_ref()))
        .then(|| common.parent().map(Path::to_path_buf))
        .flatten();
    Ok((main, Some(common)))
}

/// Returns the directories whose `.claude/settings.local.json` Claude Code
/// can read from `cwd`: `cwd`, the repository root, and a worktree's primary
/// checkout.
pub(crate) fn local_settings_dirs(cwd: &Path) -> Result<(Vec<PathBuf>, Option<PathBuf>), Error> {
    let mut git_dir = None;
    let mut dirs = vec![cwd.to_path_buf()];
    let root = checkout_root(cwd)?;
    let git = root.join(GIT_ENTRY);
    if let Ok(meta) = fs::metadata(&git) {
        if !dirs.contains(&root) {
            dirs.push(root);
        }
        if !meta.is_dir() {
            let (main, external_git) = main_checkout(&git).map_err(|source| Error::Path {
                what: "examine the worktree of",
                path: git.clone(),
                source,
            })?;
            git_dir = external_git;
            if let Some(main) = main
                && !dirs.contains(&main)
            {
                dirs.push(main);
            }
        }
    }
    Ok((dirs, git_dir))
}

pub(super) fn checkout_root(cwd: &Path) -> Result<PathBuf, Error> {
    for dir in cwd.ancestors() {
        let git = dir.join(GIT_ENTRY);
        match fs::metadata(&git) {
            Ok(_) => return Ok(dir.to_owned()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Path {
                    what: "examine",
                    path: git,
                    source,
                });
            }
        }
    }
    Ok(cwd.to_owned())
}

/// Use `configured`, or `.claude` in `home`. Treat an empty value as unset. The checks and
/// each child resolve the path from different directories, so it must be absolute.
pub(crate) fn config_dir(
    configured: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Error> {
    let configured = configured.filter(|dir| !dir.is_empty());
    let home = home.filter(|home| !home.is_empty());
    let dir = match (configured, home) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(home)) => PathBuf::from(home).join(CLAUDE_DIR),
        (None, None) => return Err(Error::NoConfigDir),
    };
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(Error::RelativeConfigDir(dir))
    }
}

/// Claude Code reads the project's `settings.json` only in the working
/// directory (checked on 2.1.284 from a subdirectory), but `settings.local.json`
/// in every dir of `local_dirs`.
pub(crate) fn skipped_settings(
    config_dir: &Path,
    cwd: &Path,
    local_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = vec![
        config_dir.join(SETTINGS_FILE),
        cwd.join(CLAUDE_DIR).join(SETTINGS_FILE),
    ];
    paths.extend(
        local_dirs
            .iter()
            .map(|dir| dir.join(CLAUDE_DIR).join(LOCAL_SETTINGS_FILE)),
    );
    paths
}

/// Returns only the names, because `env` can hold secrets.
fn settings_conflicts(settings: &Value) -> Vec<String> {
    let mut keys: Vec<String> = ROUTE_SETTINGS
        .iter()
        .filter(|key| settings.get(**key).is_some())
        .map(|key| (*key).to_owned())
        .collect();
    if settings
        .get(LOGIN_METHOD_KEY)
        .is_some_and(|method| method != SUBSCRIPTION_LOGIN_METHOD)
    {
        keys.push(LOGIN_METHOD_KEY.to_owned());
    }
    if let Some(env) = settings.get(ENV_KEY).and_then(Value::as_object) {
        let mut names: Vec<&String> = env
            .keys()
            .filter(|name| routes_login(&name.to_ascii_uppercase()))
            .collect();
        names.sort();
        let env_key = ENV_KEY;
        keys.extend(names.into_iter().map(|name| format!("{env_key}.{name}")));
    }
    keys
}

fn limited_text(path: &Path, max_bytes: u64, too_large: &'static str) -> io::Result<String> {
    #[cfg(unix)]
    let file = File::from(open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(ErrorKind::InvalidData, NOT_A_FILE));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(ErrorKind::InvalidData, too_large));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
}

/// An unreadable file could hold an API key helper, so it counts as a conflict.
pub(crate) fn file_conflicts(paths: &[PathBuf]) -> Vec<String> {
    let mut found = Vec::new();
    for path in paths {
        let shown = path.display();
        let text = match limited_text(path, MAX_SETTINGS_BYTES, SETTINGS_TOO_LARGE) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                found.push(format!("{shown}: {UNREADABLE_FILE} ({e})"));
                continue;
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(settings) if settings.is_object() => found.extend(
                settings_conflicts(&settings)
                    .into_iter()
                    .map(|key| format!("{shown}: {key}")),
            ),
            Ok(_) => found.push(format!("{shown}: {NOT_AN_OBJECT}")),
            Err(e) => found.push(format!("{shown}: {e}")),
        }
    }
    found
}

/// `None` only for a claude.ai subscription login without an API key, in the
/// default permission mode.
pub(crate) fn account_problem(init: &Value) -> Option<Problem> {
    account_problem_in(init, &[DEFAULT_MODE.to_owned()])
}

pub(super) fn account_problem_in(init: &Value, modes: &[String]) -> Option<Problem> {
    let Some(account) = init.get(ACCOUNT).filter(|a| a.is_object()) else {
        return Some(Problem::NoLogin);
    };
    if let Some(source) = account
        .get(API_KEY_SOURCE)
        .filter(|source| *source != NO_KEY_SOURCE)
    {
        return Some(Problem::ApiKey(source.clone()));
    }
    if account[API_PROVIDER] != FIRST_PARTY {
        return Some(Problem::OtherProvider(account[API_PROVIDER].clone()));
    }
    if account[SUBSCRIPTION_TYPE]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Some(Problem::NoSubscription);
    }
    if !modes.iter().any(|mode| init[START_MODE] == mode.as_str()) {
        return Some(Problem::StartMode(init[START_MODE].clone()));
    }
    None
}

fn policy_key_problem(entries: &Map<String, Value>) -> Option<String> {
    let mut keys: Vec<&String> = entries.keys().collect();
    keys.sort();
    for key in keys {
        let value = &entries[key];
        if *key == LOGIN_METHOD_KEY {
            if value != SUBSCRIPTION_LOGIN_METHOD {
                return Some(key.clone());
            }
        } else if !HARMLESS_POLICY.contains(&key.as_str()) {
            return Some(key.clone());
        } else if *key == PERMISSIONS_KEY {
            let Some(permissions) = value.as_object() else {
                return Some(key.clone());
            };
            let mut subs: Vec<&String> = permissions.keys().collect();
            subs.sort();
            if let Some(sub) = subs
                .into_iter()
                .find(|sub| !HARMLESS_POLICY_PERMISSIONS.contains(&sub.as_str()))
            {
                return Some(format!("{}.{sub}", PERMISSIONS_KEY));
            }
        }
    }
    None
}

/// `None` only when just maki's flags and safe managed keys apply, every
/// setting is valid, and compaction and every hook are off.
pub(crate) fn policy_problem(settings: &Value, hooks: &Value) -> Option<Problem> {
    policy_problem_in(settings, hooks, true)
}

pub(super) fn policy_problem_in(
    settings: &Value,
    hooks: &Value,
    owns_history: bool,
) -> Option<Problem> {
    let Some(sources) = settings[SOURCES].as_array() else {
        return Some(Problem::NoSettings);
    };
    // Claude Code omits the list when every setting is valid, and an empty
    // list means the same.
    let errors = &settings[ERRORS];
    if !errors.is_null() && errors.as_array().is_none_or(|errors| !errors.is_empty()) {
        return Some(Problem::InvalidSettings);
    }
    for source in sources {
        let name = source[SOURCE].as_str();
        if name == Some(FLAG_SOURCE) {
            continue;
        }
        if name != Some(POLICY_SOURCE) {
            return Some(Problem::LoadedSettings(
                name.unwrap_or(UNNAMED_SOURCE).to_owned(),
            ));
        }
        let Some(policy) = source[SETTINGS].as_object() else {
            return Some(Problem::UnreadablePolicy);
        };
        if let Some(key) = policy_key_problem(policy) {
            return Some(Problem::Policy(key));
        }
    }
    if owns_history && settings[EFFECTIVE][AUTO_COMPACT] != false {
        return Some(Problem::OwnCompaction);
    }
    let (Some(listed), Some(policy)) = (hooks[HOOKS].as_array(), hooks[POLICY].as_object()) else {
        return Some(Problem::NoHooks);
    };
    if policy.get(ALL_DISABLED) != Some(&Value::Bool(true)) {
        return Some(Problem::HooksStayOn);
    }
    listed
        .iter()
        .find(|hook| hook[DISABLED] != true)
        .map(|hook| Problem::Hook(hook[SOURCE].clone()))
}

pub(crate) struct InitExpect<'a> {
    pub version: &'a str,
    /// The model, or a dated snapshot of it.
    pub model: &'a str,
    pub cwd: &'a Path,
    pub tools: &'a HashSet<String>,
}

/// Claude Code ships its own plugins, and a version or a feature gate can add
/// one. The other checks cover what such a plugin could change: the tools, the
/// MCP servers, the hooks and the settings.
pub(super) fn plugins_problem(plugins: &Value) -> Option<Problem> {
    let Some(plugins) = plugins.as_array() else {
        return Some(Problem::NoPlugins);
    };
    let marker = BUILTIN_PLUGIN_MARKER;
    plugins
        .iter()
        .find(|plugin| {
            plugin[PLUGIN_PATH] != marker
                || plugin[SOURCE]
                    .as_str()
                    .and_then(|source| source.strip_suffix(marker))
                    .is_none_or(|name| !name.ends_with('@'))
        })
        .map(|plugin| Problem::Plugin(plugin[SOURCE].clone()))
}

/// The API key, mode, tool catalog and handoff connection must all satisfy the route checks.
pub(crate) fn init_problem(event: &Value, expect: &InitExpect<'_>) -> Option<Problem> {
    init_problem_in(
        event,
        expect.version,
        expect.cwd,
        expect.tools,
        &[DEFAULT_MODE.to_owned()],
        Some(SERVER),
    )
}

pub(super) fn init_problem_in(
    event: &Value,
    version: &str,
    cwd: &Path,
    expected_tools: &HashSet<String>,
    modes: &[String],
    handoff: Option<&str>,
) -> Option<Problem> {
    if event[API_KEY_SOURCE] != NO_KEY_SOURCE {
        return Some(Problem::StartedWithKey(event[API_KEY_SOURCE].clone()));
    }
    if event[VERSION] != version {
        return Some(Problem::OtherVersion(event[VERSION].clone()));
    }
    if !modes.iter().any(|mode| event[RUN_MODE] == mode.as_str()) {
        return Some(Problem::RunMode(event[RUN_MODE].clone()));
    }
    let Some(tools) = event[TOOLS].as_array() else {
        return Some(Problem::NoTools);
    };
    if let Some(tool) = tools.iter().find(|tool| {
        tool.as_str()
            .is_none_or(|name| !expected_tools.contains(name))
    }) {
        return Some(Problem::ExtraTool(tool.clone()));
    }
    if let Some(missing) = expected_tools
        .iter()
        .find(|name| !tools.iter().any(|tool| tool == name.as_str()))
    {
        return Some(Problem::MissingTool(missing.clone()));
    }
    let Some(servers) = event[MCP_SERVERS].as_array() else {
        return Some(Problem::HandoffServer(event[MCP_SERVERS].clone()));
    };
    let connected = match (handoff, servers.as_slice()) {
        (Some(name), [server]) => server[NAME] == name && server[STATUS] == CONNECTED,
        (None, []) => true,
        _ => false,
    };
    if !connected {
        return Some(Problem::HandoffServer(event[MCP_SERVERS].clone()));
    }
    if let Some(problem) = plugins_problem(&event[PLUGINS]) {
        return Some(problem);
    }
    if event[CWD].as_str().map(Path::new) != Some(cwd) {
        return Some(Problem::OtherDir(event[CWD].clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io::ErrorKind;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use rustix::fs::{CWD as AT_CWD, Mode, mkfifoat};
    use serde_json::{Value, json};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::super::error::{Error, Problem};
    #[cfg(unix)]
    use super::NOT_A_FILE;
    use super::{
        ACCOUNT_ANSWER, COMMONDIR_FILE, CONNECTED, DEFAULT_MODE, ESSENTIAL_TRAFFIC_ONLY,
        FLAG_SOURCE, GIT_ENTRY, HANDSHAKE, HOOKS_ANSWER, InitExpect, MAX_GIT_PATH_BYTES,
        MAX_SETTINGS_BYTES, NO_KEY_SOURCE, NOT_AN_OBJECT, POLICY_SOURCE, SETTINGS_ANSWER,
        SETTINGS_TOO_LARGE, UNREADABLE_FILE, account_problem, config_dir, file_conflicts,
        init_problem, launch_env, local_settings_dirs, plugins_problem, policy_problem, profile,
        settings_conflicts,
    };

    const VERSION: &str = "2.1.284";
    const SETTINGS_FIXTURE: &str = "settings.json";

    #[test]
    fn oversized_settings_fail_closed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FIXTURE);
        File::create(&path)
            .unwrap()
            .set_len(MAX_SETTINGS_BYTES + 1)
            .unwrap();
        assert!(file_conflicts(&[path])[0].contains(SETTINGS_TOO_LARGE));
    }

    #[test]
    fn oversized_utf8_settings_report_the_size_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FIXTURE);
        let mut bytes = vec![b' '; MAX_SETTINGS_BYTES as usize];
        bytes.extend_from_slice("é".as_bytes());
        fs::write(&path, bytes).unwrap();
        let error = super::limited_text(&path, MAX_SETTINGS_BYTES, SETTINGS_TOO_LARGE).unwrap_err();
        assert_eq!(error.to_string(), SETTINGS_TOO_LARGE);
    }

    #[cfg(unix)]
    #[test_case(false ; "fifo")]
    #[test_case(true ; "device_symlink")]
    fn nonregular_settings_fail_without_waiting(device: bool) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FIXTURE);
        if device {
            symlink("/dev/zero", &path).unwrap();
        } else {
            mkfifoat(AT_CWD, &path, Mode::RUSR | Mode::WUSR).unwrap();
        }
        assert!(file_conflicts(&[path])[0].contains(NOT_A_FILE));
    }
    const LINUX: &str = "linux";
    const SERVER: &str = "maki";
    const TOOL: &str = "mcp__maki__read";
    const OTHER_TOOL: &str = "mcp__maki__bash";
    const CWD: &str = "/work";
    const MODEL: &str = "claude-haiku-4-5";
    const SECRET: &str = "sk-ant-secret";
    const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
    const BEDROCK_ENV: &str = "CLAUDE_CODE_USE_BEDROCK";
    const PATH_ENV: &str = "PATH";
    const PROXY_ENV: &str = "https_proxy";
    const LOCALE_ENV: &str = "LC_ALL";
    const EDITS_MODE: &str = "acceptEdits";
    const NEWER_VERSION: &str = "2.1.285";
    const NATIVE_TOOL: &str = "Bash";
    const FOREIGN_PLUGIN: &str = "evil@market";
    const OTHER_DIR: &str = "/elsewhere";
    const USER_SETTINGS: &str = "userSettings";
    /// Where serde reports the end of the broken settings file.
    const SYNTAX_ERROR_AT: &str = "line 2 column 17";

    #[test]
    fn the_child_gets_only_known_names_and_the_route_ones_are_named() {
        let environ = [
            (PATH_ENV, "/bin"),
            (PROXY_ENV, "http://proxy"),
            (LOCALE_ENV, "C"),
            (API_KEY_ENV, SECRET),
            ("ANTHROPIC_MODEL", "opus"),
            (BEDROCK_ENV, "1"),
            ("CARGO_HOME", "/cargo"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let (env, withheld) = launch_env(environ);
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                PATH_ENV,
                PROXY_ENV,
                LOCALE_ENV,
                "NO_PROXY",
                ESSENTIAL_TRAFFIC_ONLY.0
            ]
        );
        assert_eq!(withheld, [API_KEY_ENV, BEDROCK_ENV]);
    }

    #[test_case(None ; "unset")]
    #[test_case(Some("") ; "empty")]
    #[test_case(Some(ESSENTIAL_TRAFFIC_ONLY.1) ; "already_disabled")]
    fn children_disable_nonessential_traffic(setting: Option<&str>) {
        let environ = setting.map(|value| (ESSENTIAL_TRAFFIC_ONLY.0.to_owned(), value.to_owned()));
        let (env, _) = launch_env(environ);
        let settings: Vec<_> = env
            .iter()
            .filter(|(name, _)| name == ESSENTIAL_TRAFFIC_ONLY.0)
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(settings, [ESSENTIAL_TRAFFIC_ONLY.1]);
    }

    #[test_case("2.1.284 (Claude Code)", "linux" => matches Ok(version) if version == "2.1.284" ; "minimum_version")]
    #[test_case("2.1.285 (Claude Code)", "Linux" => matches Ok(version) if version == "2.1.285" ; "newer_patch_case_insensitive_platform")]
    #[test_case("  2.1.1000", "linux" => matches Ok(version) if version == "2.1.1000" ; "leading_whitespace")]
    #[test_case("3.0.0", "linux" => matches Ok(version) if version == "3.0.0" ; "new_major_version")]
    #[test_case("2.1.283 (Claude Code)", "linux" => matches Err(Error::TooOld { .. }) ; "too_old_patch")]
    #[test_case("1.9.999", "linux" => matches Err(Error::TooOld { .. }) ; "too_old_major")]
    #[test_case("2.1.284-beta (Claude Code)", "linux" => matches Err(Error::UnknownVersion(_)) ; "prerelease_refused")]
    #[test_case("2.1.284.1", "linux" => matches Err(Error::UnknownVersion(_)) ; "extra_version_component")]
    #[test_case("Update available", "linux" => matches Err(Error::UnknownVersion(_)) ; "update_notice")]
    #[test_case("claude: not found", "linux" => matches Err(Error::UnknownVersion(_)) ; "missing_executable")]
    #[test_case("", "linux" => matches Err(Error::UnknownVersion(_)) ; "empty_version")]
    #[test_case("2.1.284", "darwin" => matches Err(Error::UnsupportedSystem { .. }) ; "unsupported_macos")]
    #[test_case("2.1.284", "Windows_NT" => matches Err(Error::UnsupportedSystem { .. }) ; "unsupported_windows")]
    fn versions_meet_the_launch_requirements(output: &str, system: &str) -> Result<String, Error> {
        profile(output, system)
    }

    #[test_case("PATH" => (true, false) ; "passed_path")]
    #[test_case("https_proxy" => (true, false) ; "passed_https_proxy")]
    #[test_case("LC_ALL" => (true, false) ; "passed_lc_all")]
    #[test_case("XDG_RUNTIME_DIR" => (true, false) ; "passed_xdg_runtime_dir")]
    #[test_case("CLAUDE_CONFIG_DIR" => (true, false) ; "passed_claude_config_dir")]
    #[test_case("anthropic_api_key" => (false, true) ; "lowercase_api_key_is_reported")]
    #[test_case("claude_code_use_bedrock" => (false, true) ; "lowercase_cloud_route_is_reported")]
    #[test_case("ANTHROPIC_API_KEY" => (false, true) ; "withheld_anthropic_api_key")]
    #[test_case("ANTHROPIC_BASE_URL" => (false, true) ; "withheld_anthropic_base_url")]
    #[test_case("ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION" => (false, true) ; "withheld_anthropic_small_fast_model_aws_region")]
    #[test_case("CLAUDE_CODE_API_BASE_URL" => (false, true) ; "withheld_claude_code_api_base_url")]
    #[test_case("CLAUDE_CODE_USE_BEDROCK" => (false, true) ; "withheld_claude_code_use_bedrock")]
    #[test_case("CLAUDE_CODE_USE_NEWCLOUD" => (false, true) ; "withheld_claude_code_use_newcloud")]
    #[test_case("CLAUDE_CODE_USE_POWERSHELL_TOOL" => (false, false) ; "dropped_claude_code_use_powershell_tool")]
    #[test_case("ANTHROPIC_MODEL" => (false, false) ; "dropped_anthropic_model")]
    #[test_case("CARGO_HOME" => (false, false) ; "dropped_cargo_home")]
    fn environment_names_are_passed_withheld_or_dropped(name: &str) -> (bool, bool) {
        let (env, withheld) = launch_env([(name.to_owned(), String::new())]);
        (env.iter().any(|(key, _)| key == name), !withheld.is_empty())
    }

    #[test_case(Some("/cfg"), Some("/home/u") => matches Ok(path) if path == Path::new("/cfg") ; "configured_absolute")]
    #[test_case(None, Some("/home/u") => matches Ok(path) if path == Path::new("/home/u/.claude") ; "home_default")]
    #[test_case(Some(""), Some("/home/u") => matches Ok(path) if path == Path::new("/home/u/.claude") ; "empty_config_uses_home")]
    #[test_case(Some("cfg"), Some("/home/u") => matches Err(Error::RelativeConfigDir(_)) ; "relative_config_refused")]
    #[test_case(None, Some("home") => matches Err(Error::RelativeConfigDir(_)) ; "relative_home_refused")]
    #[test_case(None, Some("") => matches Err(Error::NoConfigDir) ; "empty_home_refused")]
    #[test_case(None, None => matches Err(Error::NoConfigDir) ; "missing_home_refused")]
    fn login_directory_is_absolute(
        configured: Option<&str>,
        home: Option<&str>,
    ) -> Result<PathBuf, Error> {
        config_dir(configured.map(OsString::from), home.map(OsString::from))
    }

    #[test_case(json!({"apiKeyHelper":"/bin/helper","forceLoginMethod":"console","env":{"ANTHROPIC_API_KEY":"sk-ant-secret","ANTHROPIC_MODEL":"opus","FOO":"bar"}}) => json!(["apiKeyHelper", "forceLoginMethod", "env.ANTHROPIC_API_KEY"]) ; "credential_helper_and_env")]
    #[test_case(json!({"gcpAuthRefresh":"g","awsAuthRefresh":"a"}) => json!(["awsAuthRefresh", "gcpAuthRefresh"]) ; "cloud_credential_helpers")]
    #[test_case(json!({"env":{"CLAUDE_CODE_USE_VERTEX":"1","ANTHROPIC_BASE_URL":"u"}}) => json!(["env.ANTHROPIC_BASE_URL", "env.CLAUDE_CODE_USE_VERTEX"]) ; "cloud_routing_env")]
    #[test_case(json!({"forceLoginMethod":"claudeai"}) => json!([]) ; "subscription_login")]
    #[test_case(json!({"model":"opus"}) => json!([]) ; "non_subscription_login")]
    fn ignored_settings_cannot_select_a_different_route(settings: Value) -> Value {
        json!(settings_conflicts(&settings))
    }

    #[test_case(json!({"companyAnnouncements":["hi"],"permissions":{"deny":["Bash"]}}) => matches None ; "announcements_and_tool_denials")]
    #[test_case(json!({"model":"opus","availableModels":["opus"],"forceLoginMethod":"claudeai"}) => matches None ; "model_and_subscription_login")]
    #[test_case(json!({"permissions":{"allow":["Bash"]}}) => matches Some(Problem::Policy(_)) ; "tool_allowlist")]
    #[test_case(json!({"permissions":{"disableBypassPermissionsMode":"disable","defaultMode":"plan"}}) => matches Some(Problem::Policy(_)) ; "bypass_disabled_and_plan_mode")]
    #[test_case(json!({"apiKeyHelper":"x"}) => matches Some(Problem::Policy(_)) ; "api_key_helper")]
    #[test_case(json!({"forceLoginMethod":"console"}) => matches Some(Problem::Policy(_)) ; "console_login")]
    #[test_case(json!({"env":{"EDITOR":"vi"}}) => matches Some(Problem::Policy(_)) ; "managed_environment")]
    #[test_case(json!({"permissions":null}) => matches Some(Problem::Policy(_)) ; "null_permissions")]
    #[test_case(json!({"permissions":[]}) => matches Some(Problem::Policy(_)) ; "array_permissions")]
    fn managed_policy_cannot_restore_tools_or_routes(value: Value) -> Option<Problem> {
        policy_problem(&settings_with(Some(value), None), &hooks_off())
    }

    #[test_case(json!({"current_permission_mode":"default","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro"}}) => matches None ; "subscription_login")]
    #[test_case(json!({"current_permission_mode":"default","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro","apiKeySource":"none"}}) => matches None ; "subscription_without_api_key")]
    #[test_case(json!({}) => matches Some(Problem::NoLogin) ; "missing_login")]
    #[test_case(json!({"current_permission_mode":"default","account":{"apiProvider":"firstParty"}}) => matches Some(Problem::NoSubscription) ; "missing_subscription")]
    #[test_case(json!({"current_permission_mode":"default","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro","apiKeySource":"ANTHROPIC_API_KEY"}}) => matches Some(Problem::ApiKey(_)) ; "environment_api_key")]
    #[test_case(json!({"current_permission_mode":"default","account":{"apiProvider":"firstParty","apiKeySource":"/login managed key"}}) => matches Some(Problem::ApiKey(_)) ; "managed_login_api_key")]
    #[test_case(json!({"current_permission_mode":"default","account":{"apiProvider":"bedrock","subscriptionType":"Claude Pro"}}) => matches Some(Problem::OtherProvider(_)) ; "cloud_provider")]
    #[test_case(json!({"current_permission_mode":"bypassPermissions","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro"}}) => matches Some(Problem::StartMode(_)) ; "bypass_permission_mode")]
    fn accounts_use_a_subscription_route(value: Value) -> Option<Problem> {
        account_problem(&value)
    }

    #[test_case(json!([]) => matches None ; "empty_plugin_list")]
    #[test_case(json!([{"name":"agents-md","path":"builtin","source":"agents-md@builtin"}]) => matches None ; "builtin_instruction_plugin")]
    #[test_case(json!([{"name":"cc-plugin-new","path":"builtin","source":"cc-plugin-new@builtin"}]) => matches None ; "new_builtin_plugin")]
    #[test_case(json!([{"name":"new","path":"/home/u/.claude/plugins/new","source":"new@builtin"}]) => matches Some(Problem::Plugin(_)) ; "filesystem_plugin_claiming_builtin_source")]
    #[test_case(json!([{"name":"evil","path":"builtin","source":"evil@market"}]) => matches Some(Problem::Plugin(_)) ; "marketplace_plugin")]
    #[test_case(json!([{"name":"agents-md","source":"agents-md@builtin"}]) => matches Some(Problem::Plugin(_)) ; "missing_plugin_path")]
    #[test_case(json!(["agents-md@builtin"]) => matches Some(Problem::Plugin(_)) ; "string_plugin_entry")]
    #[test_case(json!("agents-md@builtin") => matches Some(Problem::NoPlugins) ; "non_array_plugin_list")]
    #[test_case(json!(null) => matches Some(Problem::NoPlugins) ; "null_plugin_list")]
    fn only_builtin_cli_plugins_are_accepted(value: Value) -> Option<Problem> {
        plugins_problem(&value)
    }

    #[test_case(json!({"hooks":[],"policy":{"allDisabled":true}}) => matches None ; "all_hooks_disabled")]
    #[test_case(json!({"hooks":[{"source":"userSettings","disabled":true}],"policy":{"allDisabled":true}}) => matches None ; "disabled_user_hook")]
    #[test_case(json!({"hooks":[{"event":"PreToolUse","source":"policySettings","disabled":false}],"policy":{"allDisabled":true,"policyHookCount":1}}) => matches Some(Problem::Hook(_)) ; "enabled_policy_hook")]
    #[test_case(json!({"hooks":[{"source":"userSettings"}],"policy":{"allDisabled":true}}) => matches Some(Problem::Hook(_)) ; "hook_without_disabled_flag")]
    #[test_case(json!({"hooks":{"h":{"source":"policySettings"}},"policy":{"allDisabled":true}}) => matches Some(Problem::NoHooks) ; "non_array_hooks")]
    #[test_case(json!({"hooks":[],"policy":{"allDisabled":false}}) => matches Some(Problem::HooksStayOn) ; "hooks_enabled")]
    #[test_case(json!({"hooks":[],"policy":{}}) => matches Some(Problem::HooksStayOn) ; "missing_all_disabled_flag")]
    #[test_case(json!({"hooks":[]}) => matches Some(Problem::NoHooks) ; "missing_policy")]
    #[test_case(json!({"policy":{"allDisabled":true}}) => matches Some(Problem::NoHooks) ; "missing_hooks")]
    #[test_case(json!(null) => matches Some(Problem::NoHooks) ; "null_hooks_response")]
    fn hooks_are_disabled(value: Value) -> Option<Problem> {
        policy_problem(&settings_with(None, None), &value)
    }

    #[test]
    fn an_unreadable_or_odd_settings_file_counts_as_a_conflict() {
        let dir = tempdir().unwrap();
        let odd = dir.path().join("odd.json");
        fs::write(&odd, "[1]").unwrap();
        let unreadable = dir.path().join("dir.json");
        fs::create_dir(&unreadable).unwrap();
        let missing = dir.path().join("missing.json");
        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{\n  \"apiKeyHelper\":").unwrap();
        let found = file_conflicts(&[odd, unreadable, missing, broken]);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].ends_with(NOT_AN_OBJECT));
        assert!(found[1].contains(UNREADABLE_FILE));
        assert!(found[2].contains(SYNTAX_ERROR_AT), "{}", found[2]);
    }

    #[test]
    fn local_settings_reach_the_repository_root_and_a_worktrees_main_checkout() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        let tree = root.join("tree");
        let sub = tree.join("src");
        fs::create_dir_all(main.join(".git/worktrees/tree")).unwrap();
        fs::write(main.join(".git/worktrees/tree/commondir"), "../..\n").unwrap();
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/tree").display()),
        )
        .unwrap();
        assert_eq!(
            local_settings_dirs(&sub).unwrap().0,
            [sub.clone(), tree, main]
        );
    }

    /// Returns a directory whose `.git` file holds `git_file`. Next to it are
    /// a superproject's submodule git directory and a primary checkout's
    /// worktree git directory, whose `commondir` file holds `commondir`.
    fn worktree(git_file: &[u8], commondir: Option<&[u8]>) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let tree = root.join("wt");
        let gitdir = root.join("main/.git/worktrees/wt");
        fs::create_dir_all(root.join("super/.git/modules/sub")).unwrap();
        fs::create_dir_all(&gitdir).unwrap();
        if let Some(commondir) = commondir {
            fs::write(gitdir.join(COMMONDIR_FILE), commondir).unwrap();
        }
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join(GIT_ENTRY), git_file).unwrap();
        (dir, tree)
    }

    #[test_case(false ; "git_pointer")]
    #[test_case(true ; "commondir")]
    fn oversized_git_paths_fail_closed(common: bool) {
        let (_dir, tree) = worktree(b"gitdir: ../main/.git/worktrees/wt\n", Some(b"../..\n"));
        let path = if common {
            tree.join("../main/.git/worktrees/wt").join(COMMONDIR_FILE)
        } else {
            tree.join(GIT_ENTRY)
        };
        File::create(path)
            .unwrap()
            .set_len(MAX_GIT_PATH_BYTES + 1)
            .unwrap();
        let error = local_settings_dirs(&tree).unwrap_err();
        assert!(
            matches!(error, Error::Path { source, .. } if source.kind() == ErrorKind::InvalidData)
        );
    }

    #[cfg(unix)]
    #[test_case(false, false ; "git_pointer_fifo")]
    #[test_case(false, true ; "git_pointer_device")]
    #[test_case(true, false ; "commondir_fifo")]
    #[test_case(true, true ; "commondir_device")]
    fn nonregular_git_paths_fail_without_waiting(common: bool, device: bool) {
        let (_dir, tree) = worktree(b"gitdir: ../main/.git/worktrees/wt\n", Some(b"../..\n"));
        let path = if common {
            tree.join("../main/.git/worktrees/wt").join(COMMONDIR_FILE)
        } else {
            tree.join(GIT_ENTRY)
        };
        fs::remove_file(&path).unwrap();
        if device {
            symlink("/dev/zero", &path).unwrap();
        } else {
            mkfifoat(AT_CWD, &path, Mode::RUSR | Mode::WUSR).unwrap();
        }
        let error = local_settings_dirs(&tree).unwrap_err();
        assert!(matches!(error, Error::Path { source, .. } if source.to_string() == NOT_A_FILE));
    }

    /// Claude Code follows a `.git` link, so a link to a worktree's `.git`
    /// file still leads to the primary checkout.
    #[cfg(unix)]
    #[test]
    fn a_linked_git_file_still_reaches_the_main_checkout() {
        let (_dir, tree) = worktree(b"gitdir: ../main/.git/worktrees/wt\n", Some(b"../..\n"));
        let linked = tree.with_file_name("linked");
        fs::create_dir(&linked).unwrap();
        symlink(tree.join(GIT_ENTRY), linked.join(GIT_ENTRY)).unwrap();
        let main = tree.with_file_name("main");
        assert_eq!(local_settings_dirs(&linked).unwrap().0, [linked, main]);
    }

    /// A submodule does not use its superproject's local settings.
    #[test]
    fn a_submodule_adds_no_other_checkout() {
        let (_dir, tree) = worktree(b"gitdir: ../super/.git/modules/sub\n", None);
        assert_eq!(local_settings_dirs(&tree).unwrap().0, [tree]);
    }

    /// An unreadable worktree file, or a missing primary checkout, leaves
    /// that checkout's local settings unchecked, which is an error.
    #[test_case(b"gitdir: ../main/.git/worktrees/wt\n", Some(b"\xff") ; "an_unreadable_commondir")]
    #[test_case(b"gitdir: \xff\n", None ; "a_git_file_that_is_not_utf8")]
    #[test_case(b"gitdir: ../main/.git/worktrees/wt\n", Some(b"../../../moved/.git\n") ; "a_moved_primary_checkout")]
    fn a_worktree_that_cannot_be_read_is_an_error(git_file: &[u8], commondir: Option<&[u8]>) {
        let (_dir, tree) = worktree(git_file, commondir);
        let error = local_settings_dirs(&tree).unwrap_err();
        let git = tree.join(GIT_ENTRY);
        assert!(
            matches!(&error, Error::Path { path, .. } if *path == git),
            "{error}"
        );
    }

    fn settings_with(policy: Option<Value>, extra_source: Option<&str>) -> Value {
        let mut sources = vec![json!({ "source": FLAG_SOURCE, "settings": {} })];
        if let Some(policy) = policy {
            sources.push(json!({ "source": POLICY_SOURCE, "settings": policy }));
        }
        if let Some(name) = extra_source {
            sources.push(json!({ "source": name, "settings": {} }));
        }
        json!({ "effective": { "autoCompactEnabled": false }, "sources": sources })
    }

    fn hooks_off() -> Value {
        json!({ "hooks": [], "policy": { "allDisabled": true } })
    }

    #[test_case(settings_with(None, None), hooks_off() => matches None ; "our_flags_alone")]
    #[test_case(json!({ "effective": {}, "sources": [] }), hooks_off() => matches Some(Problem::OwnCompaction) ; "native_compaction_left_on")]
    #[test_case(settings_with(None, Some(USER_SETTINGS)), hooks_off() => matches Some(Problem::LoadedSettings(source)) if source == USER_SETTINGS ; "user_settings_loaded")]
    #[test_case(json!({ "sources": [], "errors": [{ "file": "x" }] }), hooks_off() => matches Some(Problem::InvalidSettings) ; "invalid_settings")]
    #[test_case(json!({ "effective": { "autoCompactEnabled": false }, "sources": [], "errors": [] }), hooks_off() => matches None ; "an_empty_error_list")]
    #[test_case(settings_with(Some(json!("anything")), None), hooks_off() => matches Some(Problem::UnreadablePolicy) ; "a_policy_that_is_no_object")]
    #[test_case(json!({}), hooks_off() => matches Some(Problem::NoSettings) ; "no_settings")]
    fn only_our_settings_and_harmless_policy_pass(
        settings: Value,
        hooks: Value,
    ) -> Option<Problem> {
        policy_problem(&settings, &hooks)
    }

    fn init(edit: impl FnOnce(&mut Value)) -> Value {
        let mut event = json!({
            "type": "system",
            "subtype": "init",
            "apiKeySource": NO_KEY_SOURCE,
            "claude_code_version": VERSION,
            "permissionMode": DEFAULT_MODE,
            "tools": [TOOL, OTHER_TOOL],
            "mcp_servers": [{ "name": SERVER, "status": CONNECTED }],
            "plugins": [{ "name": "agents-md", "path": "builtin", "source": "agents-md@builtin" }],
            "cwd": CWD,
        });
        edit(&mut event);
        event
    }

    #[test_case(init(|_| {}) => matches None ; "a_clean_start")]
    #[test_case(init(|e| e["apiKeySource"] = json!(API_KEY_ENV)) => matches Some(Problem::StartedWithKey(source)) if source == API_KEY_ENV ; "an_api_key")]
    #[test_case(init(|e| e["claude_code_version"] = json!(NEWER_VERSION)) => matches Some(Problem::OtherVersion(version)) if version == NEWER_VERSION ; "another_version")]
    #[test_case(init(|e| e["permissionMode"] = json!(EDITS_MODE)) => matches Some(Problem::RunMode(mode)) if mode == EDITS_MODE ; "another_mode")]
    #[test_case(init(|e| e["tools"] = json!([TOOL, OTHER_TOOL, NATIVE_TOOL])) => matches Some(Problem::ExtraTool(tool)) if tool == NATIVE_TOOL ; "a_native_tool")]
    #[test_case(init(|e| e["tools"] = json!([TOOL])) => matches Some(Problem::MissingTool(tool)) if tool == OTHER_TOOL ; "a_missing_tool")]
    #[test_case(init(|e| e["tools"] = json!([])) => matches Some(Problem::MissingTool(_)) ; "an_empty_tool_list")]
    #[test_case(init(|e| e["tools"] = Value::Null) => matches Some(Problem::NoTools) ; "no_tool_list")]
    #[test_case(init(|e| e["mcp_servers"] = json!([{ "name": SERVER, "status": "failed" }])) => matches Some(Problem::HandoffServer(_)) ; "a_failed_handoff_server")]
    #[test_case(init(|e| e["mcp_servers"] = json!([{ "name": SERVER, "status": CONNECTED }, { "name": "other", "status": CONNECTED }])) => matches Some(Problem::HandoffServer(_)) ; "another_server")]
    #[test_case(init(|e| e["plugins"] = json!([{ "source": FOREIGN_PLUGIN }])) => matches Some(Problem::Plugin(source)) if source == FOREIGN_PLUGIN ; "a_plugin")]
    #[test_case(init(|e| e["plugins"] = Value::Null) => matches Some(Problem::NoPlugins) ; "no_plugin_list")]
    #[test_case(init(|e| e["cwd"] = json!(OTHER_DIR)) => matches Some(Problem::OtherDir(dir)) if dir == OTHER_DIR ; "another_dir")]
    fn init_must_match_the_launch(event: Value) -> Option<Problem> {
        let profile = profile(VERSION, LINUX).unwrap();
        let tools: HashSet<String> = [TOOL.to_owned(), OTHER_TOOL.to_owned()].into();
        let expect = InitExpect {
            version: &profile,
            model: MODEL,
            cwd: Path::new(CWD),
            tools: &tools,
        };
        init_problem(&event, &expect)
    }

    /// The decoder keys the answers on these ids, in the order the requests go
    /// out.
    #[test]
    fn handshake_requests_match_the_decoder_roles() {
        let ids: Vec<&str> = HANDSHAKE.iter().map(|step| step.id).collect();
        assert_eq!(ids, [ACCOUNT_ANSWER, SETTINGS_ANSWER, HOOKS_ANSWER]);
    }
}
