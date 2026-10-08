use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::claude_code::validation;
use mlua::{Lua, Result as LuaResult, Table, UserData, UserDataMethods};
use serde_json::Value;

use crate::api::util::pair::{Pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const IMPORT_LOCK: &str = "import.lock";
const DOT_GIT: &str = ".git";
const QUARANTINE_PREFIX: &str = "repository.";
const QUARANTINE_ENTRY: &str = "metadata";

/// Reject CLI versions and platforms that cannot enforce the launch contract.
/// @param output string Version output.
/// @param system string Operating system name.
/// @return (string?, string?) Accepted version, or nil and the reason.
#[lua_fn]
fn version(_lua: &Lua, output: String, system: String) -> LuaResult<Pair<String>> {
    Ok(pair(validation::version(&output, &system)))
}

/// Keep only variables that cannot redirect the subscription login.
/// @param environ table Environment variables.
/// @return (table, table) Child environment and withheld variable names.
#[lua_fn]
fn environment(lua: &Lua, environ: HashMap<String, String>) -> LuaResult<(Table, Vec<String>)> {
    let (env, withheld) = validation::environment(environ);
    Ok((lua.create_table_from(env)?, withheld))
}

/// Require an absolute login directory so probes and workers use the same account.
/// @param configured string? Explicit config directory.
/// @param home string? Home directory.
/// @return (string?, string?) Config directory, or nil and the reason.
#[lua_fn]
fn config_dir(
    _lua: &Lua,
    configured: Option<String>,
    home: Option<String>,
) -> LuaResult<Pair<String>> {
    Ok(pair(
        validation::config_dir(configured, home).map(|path| path.to_string_lossy().into_owned()),
    ))
}

fn json(text: &str) -> LuaResult<Value> {
    serde_json::from_str(text).map_err(mlua::Error::external)
}

/// Return conflicting setting names without revealing their secret values.
/// @param settings string JSON settings.
/// @return (table) Conflicting names.
#[lua_fn]
fn settings_conflicts(_lua: &Lua, settings: String) -> LuaResult<Vec<String>> {
    Ok(validation::settings_conflicts(&json(&settings)?))
}

/// Check a subscription login before a prompt is sent.
/// @param init string JSON account response or control event.
/// @param modes table Allowed permission modes.
/// @return (string?) Reason the login is unsafe, or nil.
#[lua_fn]
fn account_problem(_lua: &Lua, init: String, modes: Vec<String>) -> LuaResult<Option<String>> {
    Ok(validation::account_problem(&json(&init)?, &modes))
}

/// Reject policy that can restore hooks or override the restricted launch.
/// @param settings string JSON settings response or control event.
/// @param hooks string JSON hooks response or control event.
/// @return (string?) Reason the policy is unsafe, or nil.
#[lua_fn]
fn policy_problem(_lua: &Lua, settings: String, hooks: String) -> LuaResult<Option<String>> {
    Ok(validation::policy_problem(
        &json(&settings)?,
        &json(&hooks)?,
    ))
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

/// Accept a dated snapshot of the requested Claude model.
/// @param ran string Reported model.
/// @param expected string Requested model.
/// @return (boolean) Whether the names identify the same model.
#[lua_fn]
fn same_model(_lua: &Lua, ran: String, expected: String) -> LuaResult<bool> {
    Ok(validation::same_model(&ran, &expected))
}

/// Reuse a validated version only while the executable's identity is unchanged.
/// @param executable string Absolute CLI path.
/// @return (string?, string?) Cached version, or nil and an optional error.
#[lua_fn(guard = FsRead)]
async fn cached_version(_lua: Lua, executable: String) -> LuaResult<Pair<String>> {
    match validation::cached_version(Path::new(&executable)).await {
        Ok(version) => Ok((version, None)),
        Err(error) => Ok((None, Some(error))),
    }
}

/// Validate and remember version output. Every worker also checks its init version.
/// @param executable string Absolute CLI path.
/// @param output string Version output.
/// @param system string Operating system name.
/// @return (string?, string?) Accepted version, or nil and the reason.
#[lua_fn(guard = FsRead)]
async fn cache_version(
    _lua: Lua,
    executable: String,
    output: String,
    system: String,
) -> LuaResult<Pair<String>> {
    Ok(pair(
        validation::cache_version(Path::new(&executable), &output, &system).await,
    ))
}

/// Recheck the installed version after a worker reports a launch failure.
/// @param executable string Absolute CLI path.
#[lua_fn]
fn invalidate_version(_lua: &Lua, executable: String) -> LuaResult<()> {
    validation::invalidate_version(Path::new(&executable));
    Ok(())
}

fn quarantine_entry(path: &Path, quarantine: &Path) -> io::Result<()> {
    let destination = tempfile::Builder::new()
        .prefix(QUARANTINE_PREFIX)
        .tempdir_in(quarantine)?;
    fs::rename(path, destination.path().join(QUARANTINE_ENTRY))?;
    let _ = destination.keep();
    Ok(())
}

fn sanitize_snapshot(snapshot: &Path, quarantine: &Path, git: &Path) -> io::Result<Vec<String>> {
    fs::create_dir_all(quarantine)?;
    let root_git = snapshot.join(DOT_GIT);
    if fs::symlink_metadata(&root_git).is_ok() {
        quarantine_entry(&root_git, quarantine)?;
    }
    let mut pointer = File::options()
        .write(true)
        .create_new(true)
        .open(&root_git)?;
    writeln!(pointer, "gitdir: {}", git.display())?;
    let mut pending = vec![snapshot.to_owned()];
    let mut relocated = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path == root_git {
                continue;
            }
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(DOT_GIT))
            {
                quarantine_entry(&path, quarantine)?;
                relocated.push(
                    path.strip_prefix(snapshot)
                        .map_err(io::Error::other)?
                        .to_string_lossy()
                        .into_owned(),
                );
            } else if entry.file_type()?.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(relocated)
}

/// Move Git metadata out of a stopped worker's snapshot and restore its trusted root pointer.
/// Directory symlinks are not followed, and nested files remain ordinary import candidates.
/// @param snapshot string Snapshot directory.
/// @param quarantine string Private directory for preserved metadata.
/// @param git string Trusted artifact repository.
/// @return (table?, string?) Relocated nested metadata paths, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn sanitize_git(
    _lua: Lua,
    snapshot: String,
    quarantine: String,
    git: String,
) -> LuaResult<Pair<Vec<String>>> {
    Ok(pair(
        smol::unblock(move || {
            sanitize_snapshot(
                Path::new(&snapshot),
                Path::new(&quarantine),
                Path::new(&git),
            )
        })
        .await,
    ))
}

struct ArtifactLock(Option<File>);

impl UserData for ArtifactLock {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("close", |_, this, ()| {
            this.0.take();
            Ok(())
        });
    }
}

/// Resolve an alias from the checked account rather than from a local model table.
/// @param account string JSON account response or control event.
/// @param requested string Model alias.
/// @return (string?) Resolved model, or nil if the account did not report it.
#[lua_fn]
fn resolved_model(_lua: &Lua, account: String, requested: String) -> LuaResult<Option<String>> {
    Ok(validation::resolved_model(&json(&account)?, &requested))
}

/// Reject plugins that can change the worker's tools or instructions.
/// @param plugins string JSON plugin list.
/// @return (string?) Reason the list is unsafe, or nil.
#[lua_fn]
fn plugins_problem(_lua: &Lua, plugins: String) -> LuaResult<Option<String>> {
    Ok(validation::plugins_problem(&json(&plugins)?))
}

/// Hold an artifact across approval, import and manifest updates. Closing or dropping the
/// handle releases the lock, including after a host crash.
/// @param dir string Artifact directory.
/// @return (userdata?, string?) Lock with close(), or nil if the artifact is busy.
#[lua_fn(guard = FsWrite)]
async fn lock_artifact(_lua: Lua, dir: String) -> LuaResult<Pair<ArtifactLock>> {
    let file = try_pair!(
        smol::unblock(move || {
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(Path::new(&dir).join(IMPORT_LOCK))?;
            file.try_lock().map_err(io::Error::other)?;
            Ok::<_, io::Error>(file)
        })
        .await
    );
    Ok((Some(ArtifactLock(Some(file))), None))
}

lua_table! {
    /// Claude Code launch checks shared with the subscription provider. JSON inputs retain
    /// the distinction between null, objects and arrays.
    "maki.claude_code" => pub(crate) fn create_claude_code_table(perms: &PluginPermissions), DOCS [
        version, environment, config_dir, settings_conflicts,
        account_problem, policy_problem, init_problem, same_model, lock_artifact(perms),
        resolved_model, plugins_problem, cached_version(perms), cache_version(perms), invalidate_version, sanitize_git(perms),
    ]
}
