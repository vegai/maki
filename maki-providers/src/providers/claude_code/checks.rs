//! The rules that decide whether a Claude Code process may serve a request.
//! The `claude_code` plugin applies the same rules in `claude_launch.lua`.
//! Here the process hands tool calls to maki and runs no tools itself.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use maki_storage::paths::normalize_path;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::error::{Error, Problem};

/// The rules shared with the plugin, from its `claude_rules.lua`, which holds
/// one JSON document in a long string.
const RULES_SOURCE: &str = include_str!("../../../../plugins/claude_code/claude_rules.lua");
const RULES_OPEN: &str = "[==[";
const RULES_CLOSE: &str = "]==]";
const UNREADABLE_FILE: &str = "maki cannot read it";
const NOT_AN_OBJECT: &str = "is not a JSON object";

pub(crate) const DEFAULT_MODE: &str = "default";
pub(crate) const CONNECTED: &str = "connected";
const UNNAMED_SOURCE: &str = "unnamed";
const GIT_ENTRY: &str = ".git";
const GITDIR_PREFIX: &str = "gitdir:";
const COMMONDIR_FILE: &str = "commondir";

// The fields of Claude Code's answers and init event that the checks read.
const ACCOUNT: &str = "account";
const API_KEY_SOURCE: &str = "apiKeySource";
const API_PROVIDER: &str = "apiProvider";
const SUBSCRIPTION_TYPE: &str = "subscriptionType";
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
const AUTO_COMPACT: &str = "autoCompactEnabled";
const HOOKS: &str = "hooks";
const POLICY: &str = "policy";
const ALL_DISABLED: &str = "allDisabled";
const DISABLED: &str = "disabled";

/// The minimum version supplies the necessary flags and control requests. Each request must
/// validate all login, setting, hook, tool and plugin inputs.
#[derive(Debug, Deserialize)]
pub(crate) struct Rules {
    pub minimum_version: [u64; 3],
    /// Path rules, symlinks and process cleanup differ per OS.
    pub systems: Vec<String>,
    pub handshake: Vec<HandshakeStep>,
    /// Only these variables reach the child, so an exported
    /// `ANTHROPIC_API_KEY` cannot move it off the subscription. Names compare
    /// in upper case, so `https_proxy` passes too.
    pub passed_env: Vec<String>,
    pub passed_env_prefixes: Vec<String>,
    /// Variables that select a route, each one a conflict in an ignored
    /// settings file. Every name with a prefix in `route_env_prefixes` counts
    /// too, unless `harmless_env` lists it.
    pub route_env: Vec<String>,
    pub route_env_prefixes: Vec<String>,
    pub harmless_env: Vec<String>,
    /// These keys can change the login even in settings that maki ignores.
    pub route_settings: Vec<String>,
    pub login_method_key: String,
    pub subscription_login_method: String,
    /// Managed keys that only restrict or inform. Any other managed key can
    /// reopen what the flags closed.
    pub harmless_policy: Vec<String>,
    pub permissions_key: String,
    pub harmless_policy_permissions: Vec<String>,
    pub env_key: String,
    pub first_party: String,
    /// A subscription login sends no key source, or "none".
    pub no_key_source: String,
    pub flag_source: String,
    pub policy_source: String,
    pub claude_dir: String,
    pub settings_file: String,
    pub local_settings_file: String,
    /// Where Claude Code says a plugin of its own lives, and the marketplace
    /// in its source.
    pub builtin_plugin_marker: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HandshakeStep {
    pub id: String,
    pub subtype: String,
}

/// A malformed file is a panic here rather than a compile error, so a test
/// forces it in CI.
pub(crate) static RULES: LazyLock<Rules> = LazyLock::new(|| {
    serde_json::from_str(long_string(RULES_SOURCE)).expect("claude_rules.lua holds valid rules")
});

/// The JSON document that a Lua file shared with the plugin holds in a long
/// string.
fn long_string(source: &str) -> &str {
    source
        .split_once(RULES_OPEN)
        .and_then(|(_, rest)| rest.split_once(RULES_CLOSE))
        .map(|(json, _)| json)
        .expect("the shared Lua file holds its JSON in a long string")
}

fn listed(list: &[String], name: &str) -> bool {
    list.iter().any(|entry| entry == name)
}

#[derive(Debug)]
pub(crate) struct Profile {
    pub version: String,
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

/// A first word that is not `major.minor.patch` stops the request.
pub(crate) fn profile(output: &str, os: &str) -> Result<Profile, Error> {
    let version = output.split_whitespace().next().unwrap_or_default();
    let parts = version_parts(version).ok_or_else(|| Error::UnknownVersion(output.to_owned()))?;
    let minimum = RULES.minimum_version;
    if parts < minimum {
        return Err(Error::TooOld {
            version: version.to_owned(),
            oldest: minimum.map(|part| part.to_string()).join("."),
        });
    }
    if !listed(&RULES.systems, &os.to_ascii_lowercase()) {
        return Err(Error::UnsupportedSystem {
            os: os.to_owned(),
            systems: RULES.systems.join(", "),
        });
    }
    Ok(Profile {
        version: version.to_owned(),
    })
}

fn passed(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    listed(&RULES.passed_env, &upper)
        || RULES
            .passed_env_prefixes
            .iter()
            .any(|prefix| upper.starts_with(prefix.as_str()))
}

/// A new `CLAUDE_CODE_USE_*` cloud switch fails closed, and only the model
/// variables that Claude Code knows pass as `ANTHROPIC_*`.
pub(crate) fn routes_login(name: &str) -> bool {
    listed(&RULES.route_env, name)
        || (RULES
            .route_env_prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()))
            && !listed(&RULES.harmless_env, name))
}

/// Also returns the sorted names held back because they can change the
/// login.
pub(crate) fn child_env(
    environ: impl IntoIterator<Item = (String, String)>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut env = Vec::new();
    let mut withheld = Vec::new();
    for (name, value) in environ {
        if passed(&name) {
            env.push((name, value));
        } else if routes_login(&name) {
            withheld.push(name);
        }
    }
    withheld.sort();
    (env, withheld)
}

/// A submodule has no `commondir` and returns `None`. Unreadable git paths must fail
/// because the primary checkout can contain local settings. Resolve paths lexically, as
/// Claude Code and the plugin do.
fn main_checkout(git_file: &Path) -> io::Result<Option<PathBuf>> {
    let text = fs::read_to_string(git_file)?;
    let Some(gitdir) = text.trim().strip_prefix(GITDIR_PREFIX) else {
        return Ok(None);
    };
    let gitdir = git_file.parent().unwrap_or(git_file).join(gitdir.trim());
    let common = match fs::read_to_string(gitdir.join(COMMONDIR_FILE)) {
        Ok(common) => common,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let common = normalize_path(&gitdir.join(common.trim()));
    fs::metadata(&common)?;
    Ok((common.file_name() == Some(GIT_ENTRY.as_ref()))
        .then(|| common.parent().map(Path::to_path_buf))
        .flatten())
}

/// Returns the directories whose `.claude/settings.local.json` Claude Code
/// can read from `cwd`: `cwd`, the repository root, and a worktree's primary
/// checkout.
pub(crate) fn local_settings_dirs(cwd: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut dirs = vec![cwd.to_path_buf()];
    for dir in cwd.ancestors() {
        let git = dir.join(GIT_ENTRY);
        match fs::metadata(&git) {
            Ok(meta) => {
                if !dirs.iter().any(|d| d == dir) {
                    dirs.push(dir.to_path_buf());
                }
                if meta.is_file()
                    && let Some(main) = main_checkout(&git).map_err(|source| Error::Path {
                        what: "examine the worktree of",
                        path: git.clone(),
                        source,
                    })?
                    && !dirs.contains(&main)
                {
                    dirs.push(main);
                }
                break;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Path {
                    what: "examine",
                    path: git,
                    source,
                });
            }
        }
    }
    Ok(dirs)
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
        (None, Some(home)) => PathBuf::from(home).join(&RULES.claude_dir),
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
        config_dir.join(&RULES.settings_file),
        cwd.join(&RULES.claude_dir).join(&RULES.settings_file),
    ];
    paths.extend(
        local_dirs
            .iter()
            .map(|dir| dir.join(&RULES.claude_dir).join(&RULES.local_settings_file)),
    );
    paths
}

/// Returns only the names, because `env` can hold secrets.
pub(crate) fn settings_conflicts(settings: &Value) -> Vec<String> {
    let mut keys: Vec<String> = RULES
        .route_settings
        .iter()
        .filter(|key| settings.get(key.as_str()).is_some())
        .cloned()
        .collect();
    if settings
        .get(&RULES.login_method_key)
        .is_some_and(|method| method != RULES.subscription_login_method.as_str())
    {
        keys.push(RULES.login_method_key.clone());
    }
    if let Some(env) = settings.get(&RULES.env_key).and_then(Value::as_object) {
        let mut names: Vec<&String> = env.keys().filter(|name| routes_login(name)).collect();
        names.sort();
        let env_key = &RULES.env_key;
        keys.extend(names.into_iter().map(|name| format!("{env_key}.{name}")));
    }
    keys
}

/// An unreadable file could hold an API key helper, so it counts as a
/// conflict.
pub(crate) fn file_conflicts(paths: &[PathBuf]) -> Vec<String> {
    let mut found = Vec::new();
    for path in paths {
        let shown = path.display();
        let text = match fs::read_to_string(path) {
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
    let Some(account) = init.get(ACCOUNT).filter(|a| a.is_object()) else {
        return Some(Problem::NoLogin);
    };
    if let Some(source) = account
        .get(API_KEY_SOURCE)
        .filter(|source| *source != RULES.no_key_source.as_str())
    {
        return Some(Problem::ApiKey(source.clone()));
    }
    if account[API_PROVIDER] != RULES.first_party.as_str() {
        return Some(Problem::OtherProvider(account[API_PROVIDER].clone()));
    }
    if account[SUBSCRIPTION_TYPE]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Some(Problem::NoSubscription);
    }
    if init[START_MODE] != DEFAULT_MODE {
        return Some(Problem::StartMode(init[START_MODE].clone()));
    }
    None
}

fn policy_key_problem(entries: &Map<String, Value>) -> Option<String> {
    let mut keys: Vec<&String> = entries.keys().collect();
    keys.sort();
    for key in keys {
        let value = &entries[key];
        if *key == RULES.login_method_key {
            if value != RULES.subscription_login_method.as_str() {
                return Some(key.clone());
            }
        } else if !listed(&RULES.harmless_policy, key) {
            return Some(key.clone());
        } else if let Some(permissions) =
            value.as_object().filter(|_| *key == RULES.permissions_key)
        {
            let mut subs: Vec<&String> = permissions.keys().collect();
            subs.sort();
            if let Some(sub) = subs
                .into_iter()
                .find(|sub| !listed(&RULES.harmless_policy_permissions, sub))
            {
                return Some(format!("{}.{sub}", RULES.permissions_key));
            }
        }
    }
    None
}

/// `None` only when just maki's flags and safe managed keys apply, every
/// setting is valid, and compaction and every hook are off.
pub(crate) fn policy_problem(settings: &Value, hooks: &Value) -> Option<Problem> {
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
        if name == Some(RULES.flag_source.as_str()) {
            continue;
        }
        if name != Some(RULES.policy_source.as_str()) {
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
    if settings[EFFECTIVE][AUTO_COMPACT] != false {
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
    pub profile: &'a Profile,
    /// The model, or a dated snapshot of it.
    pub model: &'a str,
    pub cwd: &'a Path,
    pub server: &'a str,
    pub tools: &'a HashSet<String>,
}

/// Claude Code ships its own plugins, and a version or a feature gate can add
/// one. The other checks cover what such a plugin could change: the tools, the
/// MCP servers, the hooks and the settings.
fn plugins_problem(plugins: &Value) -> Option<Problem> {
    let Some(plugins) = plugins.as_array() else {
        return Some(Problem::NoPlugins);
    };
    let marker = RULES.builtin_plugin_marker.as_str();
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
    if event[API_KEY_SOURCE] != RULES.no_key_source.as_str() {
        return Some(Problem::StartedWithKey(event[API_KEY_SOURCE].clone()));
    }
    if event[VERSION] != expect.profile.version {
        return Some(Problem::OtherVersion(event[VERSION].clone()));
    }
    if event[RUN_MODE] != DEFAULT_MODE {
        return Some(Problem::RunMode(event[RUN_MODE].clone()));
    }
    let Some(tools) = event[TOOLS].as_array() else {
        return Some(Problem::NoTools);
    };
    if let Some(tool) = tools.iter().find(|tool| {
        tool.as_str()
            .is_none_or(|name| !expect.tools.contains(name))
    }) {
        return Some(Problem::ExtraTool(tool.clone()));
    }
    if let Some(missing) = expect
        .tools
        .iter()
        .find(|name| !tools.iter().any(|tool| tool == name.as_str()))
    {
        return Some(Problem::MissingTool(missing.clone()));
    }
    let servers = event[MCP_SERVERS]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    match servers {
        [server] if server[NAME] == expect.server && server[STATUS] == CONNECTED => {}
        _ => return Some(Problem::HandoffServer(event[MCP_SERVERS].clone())),
    }
    if let Some(problem) = plugins_problem(&event[PLUGINS]) {
        return Some(problem);
    }
    if event[CWD].as_str().map(Path::new) != Some(expect.cwd) {
        return Some(Problem::OtherDir(event[CWD].clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::super::error::{Error, Problem};
    use super::super::stream::{ACCOUNT_ANSWER, HOOKS_ANSWER, SETTINGS_ANSWER};
    use super::{
        COMMONDIR_FILE, CONNECTED, DEFAULT_MODE, GIT_ENTRY, InitExpect, NOT_AN_OBJECT, RULES,
        UNREADABLE_FILE, account_problem, child_env, config_dir, file_conflicts, init_problem,
        local_settings_dirs, long_string, plugins_problem, policy_problem, profile,
        settings_conflicts,
    };

    /// `spec.lua` runs the same cases against the plugin's checks.
    const RULE_CASES: &str = include_str!("../../../../plugins/claude_code/tests/rule_cases.lua");
    const PASSED: &str = "passed";
    const WITHHELD: &str = "withheld";
    const DROPPED: &str = "dropped";

    const VERSION: &str = "2.1.284";
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
        let (env, withheld) = child_env(environ);
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, [PATH_ENV, PROXY_ENV, LOCALE_ENV]);
        assert_eq!(withheld, [API_KEY_ENV, BEDROCK_ENV]);
    }

    /// Both implementations must reach the same verdict and report the same refusal text for
    /// each shared case.
    #[test_case("versions" ; "versions")]
    #[test_case("env" ; "env")]
    #[test_case("config_dirs" ; "config_dirs")]
    #[test_case("settings" ; "settings")]
    #[test_case("policies" ; "policies")]
    #[test_case("accounts" ; "accounts")]
    #[test_case("plugin_lists" ; "plugin_lists")]
    #[test_case("hooks" ; "hooks")]
    fn the_shared_rule_cases_hold(section: &str) {
        let cases: Value = serde_json::from_str(long_string(RULE_CASES)).unwrap();
        for case in cases[section].as_array().unwrap() {
            let text = |key: &str| case[key].as_str();
            let problem = match section {
                "versions" => {
                    let checked = profile(text("output").unwrap(), text("system").unwrap());
                    let version = checked.as_ref().ok().map(|p| p.version.as_str());
                    assert_eq!(version, text("version"), "{case}");
                    checked.err().map(|error| error.to_string())
                }
                "env" => {
                    let (env, withheld) =
                        child_env([(text("name").unwrap().to_owned(), String::new())]);
                    let verdict = match (env.is_empty(), withheld.is_empty()) {
                        (false, _) => PASSED,
                        (true, false) => WITHHELD,
                        (true, true) => DROPPED,
                    };
                    assert_eq!(Some(verdict), text("verdict"), "{case}");
                    None
                }
                "config_dirs" => {
                    let dir = config_dir(
                        text("configured").map(OsString::from),
                        text("home").map(OsString::from),
                    );
                    let found = dir.as_deref().ok().and_then(Path::to_str);
                    assert_eq!(found, text("dir"), "{case}");
                    dir.err().map(|error| error.to_string())
                }
                "settings" => {
                    let conflicts = json!(settings_conflicts(&case["settings"]));
                    assert_eq!(conflicts, case["conflicts"], "{case}");
                    None
                }
                "policies" => {
                    let settings = settings_with(Some(case["policy"].clone()), None);
                    policy_problem(&settings, &hooks_off()).map(|problem| problem.to_string())
                }
                "accounts" => account_problem(&case["init"]).map(|problem| problem.to_string()),
                "plugin_lists" => {
                    plugins_problem(&case["plugins"]).map(|problem| problem.to_string())
                }
                "hooks" => policy_problem(&settings_with(None, None), &case["hooks"])
                    .map(|problem| problem.to_string()),
                _ => unreachable!("an unknown section {section}"),
            };
            match text("problem") {
                Some(want) => assert!(
                    problem.as_deref().is_some_and(|found| found.contains(want)),
                    "{case}: got {problem:?}"
                ),
                None => assert_eq!(problem, None, "{case}"),
            }
        }
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
            local_settings_dirs(&sub).unwrap(),
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
        assert_eq!(local_settings_dirs(&linked).unwrap(), [linked, main]);
    }

    /// A submodule does not use its superproject's local settings.
    #[test]
    fn a_submodule_adds_no_other_checkout() {
        let (_dir, tree) = worktree(b"gitdir: ../super/.git/modules/sub\n", None);
        assert_eq!(local_settings_dirs(&tree).unwrap(), [tree]);
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
        let mut sources = vec![json!({ "source": RULES.flag_source.as_str(), "settings": {} })];
        if let Some(policy) = policy {
            sources.push(json!({ "source": RULES.policy_source.as_str(), "settings": policy }));
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
            "apiKeySource": RULES.no_key_source.as_str(),
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
            profile: &profile,
            model: MODEL,
            cwd: Path::new(CWD),
            server: SERVER,
            tools: &tools,
        };
        init_problem(&event, &expect)
    }

    /// The decoder keys the answers on these ids, in the order the requests go
    /// out.
    #[test]
    fn the_shared_rules_parse_with_the_handshake_roles() {
        let ids: Vec<&str> = RULES
            .handshake
            .iter()
            .map(|step| step.id.as_str())
            .collect();
        assert_eq!(ids, [ACCOUNT_ANSWER, SETTINGS_ANSWER, HOOKS_ANSWER]);
    }
}
