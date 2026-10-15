#[cfg(unix)]
mod blobs;
#[cfg(unix)]
mod snapshot;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::claude_code::validation;
use mlua::{Lua, Result as LuaResult, Table, UserData, UserDataMethods, Value as LuaValue};
use serde_json::Value;

use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

pub(crate) const MODULE: &str = "maki.claude_code.internal";
#[cfg(test)]
pub(crate) const PLUGIN: &str = "claude_code";
const HOME_STATE_PREFIX: &str = ".claude.json";

pub(crate) fn create_private_module(
    lua: &Lua,
    perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    Ok((MODULE, create_claude_code_table(lua, perms)?))
}

/// Resolve the checkout's Git directory for the shared import lock.
/// @param project string Checkout directory or one of its subdirectories.
/// @return (string?, string?) Git directory, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn project_git_dir(_lua: Lua, project: String) -> LuaResult<Pair<String>> {
    Ok(pair(
        smol::unblock(move || {
            let project = validation::checkout_root(Path::new(&project)).map_err(IoError::other)?;
            validation::git_directory(&project.join(validation::GIT_ENTRY))
                .map(|path| path.to_string_lossy().into_owned())
        })
        .await,
    ))
}

/// Existing home state and temporary copies that must be hidden from workers.
/// @param home string Home directory.
/// @return (table?, string?) State filenames, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn home_state_files(_lua: Lua, home: String) -> LuaResult<Pair<Vec<String>>> {
    Ok(pair(
        smol::unblock(move || {
            let entries = match fs::read_dir(home) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            let mut files = Vec::new();
            for entry in entries {
                let name = entry?.file_name();
                if name
                    .as_encoded_bytes()
                    .starts_with(HOME_STATE_PREFIX.as_bytes())
                {
                    files.push(name.into_string().map_err(|_| {
                        IoError::new(
                            ErrorKind::InvalidData,
                            "Claude Code state path is not valid UTF-8",
                        )
                    })?);
                }
            }
            Ok(files)
        })
        .await,
    ))
}

/// The control requests each worker must answer before its prompt.
/// @return (table) Requests in handshake order.
#[lua_fn]
fn handshake(lua: &Lua) -> LuaResult<LuaValue> {
    json_to_lua(lua, &validation::handshake())
}

/// Launch arguments shared with the subscription provider.
/// @param model string Model name.
/// @param tools string Comma-separated tool names.
/// @param mode string Permission mode.
/// @param settings string Serialized launch settings.
/// @return (table) Command-line arguments.
#[lua_fn]
fn base_args(
    _lua: &Lua,
    model: String,
    tools: String,
    mode: String,
    settings: String,
) -> LuaResult<Vec<String>> {
    Ok(validation::base_args(&model, &tools, &mode, &settings))
}

/// Settings that disable implicit hooks, memory and instruction loading.
#[lua_fn]
fn base_settings(lua: &Lua) -> LuaResult<LuaValue> {
    json_to_lua(lua, &validation::base_settings())
}

/// Keep only variables that cannot redirect the subscription login.
/// @param environ table Environment variables.
/// @return (table, table) Child environment and withheld variable names.
#[lua_fn]
fn environment(lua: &Lua, environ: HashMap<String, String>) -> LuaResult<(Table, Vec<String>)> {
    let (env, withheld) = validation::environment(environ);
    Ok((lua.create_table_from(env)?, withheld))
}

/// Prepare the login environment and validate every skipped settings file.
/// @param environ table Parent environment variables.
/// @param configured string? Explicit config directory.
/// @param cwd string Session directory.
/// @return (table?, string?) Validated preparation, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn prepare_launch(
    lua: Lua,
    environ: HashMap<String, String>,
    configured: Option<String>,
    cwd: String,
) -> LuaResult<Pair<Table>> {
    let prepared = try_pair!(
        smol::unblock(move || validation::prepared_launch(environ, configured, Path::new(&cwd)))
            .await
    );
    let paths = |path: PathBuf| {
        path.into_os_string()
            .into_string()
            .map_err(|_| "launch path is not valid UTF-8")
    };
    let result = lua.create_table()?;
    result.set("env", lua.create_table_from(prepared.env)?)?;
    result.set("withheld", prepared.withheld)?;
    result.set("config_dir", try_pair!(paths(prepared.config_dir)))?;
    result.set(
        "own_config_dir",
        try_pair!(prepared.own_config_dir.map(paths).transpose()),
    )?;
    result.set(
        "dirs",
        try_pair!(
            prepared
                .local_dirs
                .into_iter()
                .map(paths)
                .collect::<Result<Vec<_>, _>>()
        ),
    )?;
    result.set(
        "git_dir",
        try_pair!(prepared.git_dir.map(paths).transpose()),
    )?;
    Ok((Some(result), None))
}

/// Empty MCP configuration shared with the subscription provider.
/// @return (string) Serialized empty MCP configuration.
#[lua_fn]
fn empty_mcp_config(_lua: &Lua) -> LuaResult<&'static str> {
    Ok(validation::EMPTY_MCP_CONFIG)
}

fn json(text: &str) -> LuaResult<Value> {
    serde_json::from_str(text).map_err(mlua::Error::external)
}

/// Check a subscription login before a prompt is sent.
/// @param init string JSON account response or control event.
/// @param modes table Allowed permission modes.
/// @return (string?) Reason the login is unsafe, or nil.
#[lua_fn]
fn account_problem(_lua: &Lua, init: String, modes: Vec<String>) -> LuaResult<Option<String>> {
    Ok(validation::account_problem(&json(&init)?, &modes))
}

/// Reject unsafe policy and require the worker's expected settings.
/// @param settings string JSON settings response or control event.
/// @param hooks string JSON hooks response or control event.
/// @param expected string JSON worker settings generated by maki.
/// @return (string?) Reason the policy is unsafe, or nil.
#[lua_fn]
fn policy_problem(
    _lua: &Lua,
    settings: String,
    hooks: String,
    expected: String,
) -> LuaResult<Option<String>> {
    Ok(validation::worker_policy(&json(&settings)?, &json(&hooks)?, &json(&expected)?).err())
}

/// Check the worker catalog, login route and directory before accepting output.
/// @param event string JSON init event.
/// @param version string Validated version.
/// @param cwd string Expected directory.
/// @param tools table Allowed tool names.
/// @param modes table Allowed permission modes.
/// @return (string?) Reason the init is unsafe, or nil.
#[lua_fn]
fn init_problem(
    _lua: &Lua,
    event: String,
    version: String,
    cwd: String,
    tools: Vec<String>,
    modes: Vec<String>,
) -> LuaResult<Option<String>> {
    Ok(validation::init_problem(
        &json(&event)?,
        &version,
        Path::new(&cwd),
        tools.into_iter().collect::<HashSet<_>>(),
        &modes,
    ))
}

struct VersionCheck(validation::VersionCheck);

impl UserData for VersionCheck {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("version", |_, this, ()| Ok(this.0.version.clone()));
        methods.add_method("save", |_, this, (output, system): (String, String)| {
            Ok(pair(this.0.save(&output, &system)))
        });
    }
}

/// Capture the executable identity before reading its version.
/// @param executable string Absolute CLI path.
/// @return (userdata?, string?) Check with version() and save(output, system).
#[lua_fn(guard = FsRead)]
async fn version_check(_lua: Lua, executable: String) -> LuaResult<Pair<VersionCheck>> {
    Ok(pair(
        validation::version_check(Path::new(&executable))
            .await
            .map(VersionCheck),
    ))
}

/// Recheck the installed version after a worker reports a launch failure.
/// @param executable string Absolute CLI path.
#[lua_fn]
fn invalidate_version(_lua: &Lua, executable: String) -> LuaResult<()> {
    validation::invalidate_version(Path::new(&executable));
    Ok(())
}

/// Move Git metadata out of a stopped worker's snapshot and restore its trusted root pointer.
/// Embedded repositories are quarantined with their configs disabled. Directory links are not followed.
/// @param snapshot string Snapshot directory.
/// @param quarantine string Private directory for preserved metadata.
/// @param git string Trusted artifact repository.
/// @return (table?, string?) Metadata and repository paths, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn sanitize_git(
    lua: Lua,
    snapshot: String,
    quarantine: String,
    git: String,
) -> LuaResult<Pair<Table>> {
    #[cfg(unix)]
    {
        let result = smol::unblock(move || {
            snapshot::sanitize_snapshot(
                Path::new(&snapshot),
                Path::new(&quarantine),
                Path::new(&git),
            )
        })
        .await;
        match result {
            Ok(paths) => {
                let result = lua.create_table()?;
                result.set("metadata", paths.metadata)?;
                result.set("repositories", paths.repositories)?;
                result.set("private_files", paths.private_files)?;
                Ok((Some(result), None))
            }
            Err(error) => Ok((None, Some(error.to_string()))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (lua, snapshot, quarantine, git);
        Ok((None, Some("snapshot sanitization requires Unix".into())))
    }
}

/// Unpack a supervised git cat-file batch into private stage files.
/// @param batch string Batch output file.
/// @param stage string Stage directory.
/// @param hashes table Requested object ids in batch order.
/// @return (boolean?, string?) True, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn unpack_blobs(
    _lua: Lua,
    batch: String,
    stage: String,
    hashes: Vec<String>,
) -> LuaResult<Pair<bool>> {
    #[cfg(unix)]
    {
        Ok(pair(
            smol::unblock(move || {
                blobs::unpack(Path::new(&batch), Path::new(&stage), &hashes).map(|()| true)
            })
            .await,
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (batch, stage, hashes);
        Ok((None, Some("blob staging requires Unix".into())))
    }
}

/// Classify a worker event with the provider's protocol rules before conversion to Lua.
/// @param line string Raw stream-json event.
/// @param mode table Accepted-init and expected-model state.
/// @return (table?, string?) Event and normalized usage, or nil and the protocol error.
#[lua_fn]
fn classify(lua: &Lua, line: String, mode: Table) -> LuaResult<Pair<Table>> {
    let model: Option<String> = mode.get("model")?;
    let classified = try_pair!(validation::classify(
        &line,
        mode.get::<Option<bool>>("accepted")?.unwrap_or(false),
        model.as_deref(),
        mode.get::<Option<bool>>("model_checked")?.unwrap_or(false),
    ));
    let event = &classified.event;
    let result = lua.create_table()?;
    result.set("event", json_to_lua(lua, event)?)?;
    result.set("model_checked", classified.model_checked)?;
    result.set("problem", classified.problem)?;
    let usage = if event["type"] == "assistant" {
        &event["message"]["usage"]
    } else {
        &event["usage"]
    };
    if usage.is_object() {
        result.set("usage", json_to_lua(lua, &validation::usage_counts(usage))?)?;
    }
    if event["type"] == "rate_limit_event" && event["rate_limit_info"].is_object() {
        result.set(
            "rate_limit",
            json_to_lua(
                lua,
                &serde_json::to_value(validation::plan_usage(&event["rate_limit_info"]))
                    .map_err(mlua::Error::external)?,
            )?,
        )?;
    }
    Ok((Some(result), None))
}

/// Extract an account's billing label from a control response.
/// @param account string JSON account response or control event.
/// @return (string?) Subscription label.
#[lua_fn]
fn subscription_type(_lua: &Lua, account: String) -> LuaResult<Option<String>> {
    Ok(validation::subscription_type(&json(&account)?))
}

/// Resolve an alias from the checked account rather than from a local model table.
/// @param account string JSON account response or control event.
/// @param requested string Model alias.
/// @return (string?) Resolved model, or nil if the account did not report it.
#[lua_fn]
fn resolved_model(_lua: &Lua, account: String, requested: String) -> LuaResult<Option<String>> {
    Ok(validation::resolved_model(&json(&account)?, &requested))
}

lua_table! {
    /// Claude Code launch checks shared with the subscription provider. JSON inputs retain
    /// the distinction between null, objects and arrays.
    "maki.claude_code" => pub(crate) fn create_claude_code_table(perms: &PluginPermissions), DOCS [
        handshake, base_args, base_settings, empty_mcp_config, environment, prepare_launch(perms), home_state_files(perms),
        account_problem, policy_problem, init_problem,
        resolved_model, subscription_type, classify, version_check(perms), invalidate_version, sanitize_git(perms), project_git_dir(perms), unpack_blobs(perms),
    ]
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use mlua::{Function, Lua};
    use tempfile::tempdir;
    use test_case::test_case;

    use super::create_claude_code_table;
    use crate::plugin_permissions::PluginPermissions;
    use maki_providers::claude_code::validation::GIT_ENTRY;

    const LINKED: &str = "linked";
    const NESTED: &str = "src";

    #[test]
    fn home_state_files_include_temporary_copies() {
        let home = tempdir().unwrap();
        let names = [
            ".claude.json",
            ".claude.json.backup",
            ".claude.json.tmp.42.random",
        ];
        for name in names {
            fs::write(home.path().join(name), "state").unwrap();
        }
        fs::write(home.path().join("unrelated"), "other").unwrap();
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let list: Function = table.get("home_state_files").unwrap();
        let (mut found, error): (Option<Vec<String>>, Option<String>) =
            smol::block_on(list.call_async(home.path().to_str().unwrap())).unwrap();
        assert_eq!(error, None);
        found.as_mut().unwrap().sort();
        assert_eq!(found.unwrap(), names);
    }

    #[test]
    fn policy_checks_require_worker_expectations() {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let check: Function = table.get("policy_problem").unwrap();
        assert!(check.call::<Option<String>>(("{}", "{}")).is_err());
        for expected in ["null", "{}"] {
            let problem = check
                .call::<Option<String>>(("{}", "{}", expected))
                .unwrap();
            assert!(problem.unwrap().contains("expected worker settings"));
        }
    }

    #[test_case("" ; "checkout")]
    #[test_case(NESTED ; "subdirectory")]
    #[test_case(LINKED ; "symlink")]
    fn checkout_git_directory_is_shared_across_spellings(path: &str) {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(GIT_ENTRY)).unwrap();
        fs::create_dir(root.path().join(NESTED)).unwrap();
        symlink(root.path(), root.path().join(LINKED)).unwrap();
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let resolve: Function = table.get("project_git_dir").unwrap();
        let (directory, error): (Option<String>, Option<String>) =
            smol::block_on(resolve.call_async(root.path().join(path).to_str().unwrap())).unwrap();
        assert_eq!(error, None);
        assert_eq!(
            directory.unwrap(),
            root.path()
                .canonicalize()
                .unwrap()
                .join(GIT_ENTRY)
                .to_str()
                .unwrap()
        );
    }
}
