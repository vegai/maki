use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::claude_code::validation;
use mlua::{Lua, Result as LuaResult, Table, Value as LuaValue};
use serde_json::Value;

use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, pair};
use crate::plugin_permissions::PluginPermissions;

pub(crate) const MODULE: &str = "maki.claude_code.internal";
pub(crate) const PLUGIN: &str = "claude_code";
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

/// Discover all checkout settings directories and external Git objects.
/// @param cwd string Working directory.
/// @return (table?, string?) Checkout paths or error.
#[lua_fn(guard = FsRead)]
async fn local_settings_dirs(lua: Lua, cwd: String) -> LuaResult<Pair<Table>> {
    let (dirs, git_dir) =
        match smol::unblock(move || validation::local_settings_dirs(Path::new(&cwd))).await {
            Ok(paths) => paths,
            Err(error) => return Ok((None, Some(error))),
        };
    let result = lua.create_table()?;
    result.set(
        "dirs",
        dirs.into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    )?;
    result.set(
        "git_dir",
        git_dir.map(|path| path.to_string_lossy().into_owned()),
    )?;
    Ok((Some(result), None))
}

/// Check raw settings files before JSON-to-Lua conversion can discard null values.
/// @param config_dir string Login directory.
/// @param cwd string Working directory.
/// @param local_dirs table Local settings directories.
/// @return (table) Conflicts.
#[lua_fn(guard = FsRead)]
async fn file_conflicts(
    _lua: Lua,
    config_dir: String,
    cwd: String,
    local_dirs: Vec<String>,
) -> LuaResult<Vec<String>> {
    Ok(smol::unblock(move || {
        validation::file_conflicts(
            Path::new(&config_dir),
            Path::new(&cwd),
            &local_dirs
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
        )
    })
    .await)
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

/// Reject unsafe policy and return its validated effective settings.
/// @param settings string JSON settings response or control event.
/// @param hooks string JSON hooks response or control event.
/// @return (string?, table?) Reason the policy is unsafe, or its effective settings.
#[lua_fn]
fn policy_problem(
    lua: &Lua,
    settings: String,
    hooks: String,
) -> LuaResult<(Option<String>, Option<LuaValue>)> {
    let settings = json(&settings)?;
    let hooks = json(&hooks)?;
    let (problem, effective) = validation::policy_problem(&settings, &hooks);
    let effective = if problem.is_none() {
        Some(json_to_lua(lua, effective)?)
    } else {
        None
    };
    Ok((problem, effective))
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
    let expected = format!("gitdir: {}\n", git.display());
    let trusted = fs::symlink_metadata(&root_git).is_ok_and(|meta| meta.is_file())
        && fs::read(&root_git)? == expected.as_bytes();
    if !trusted {
        if fs::symlink_metadata(&root_git).is_ok() {
            quarantine_entry(&root_git, quarantine)?;
        }
        let mut pointer = File::options()
            .write(true)
            .create_new(true)
            .open(&root_git)?;
        pointer.write_all(expected.as_bytes())?;
    }
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

/// Reject plugins that can change the worker's tools or instructions.
/// @param plugins string JSON plugin list.
/// @return (string?) Reason the list is unsafe, or nil.
#[lua_fn]
fn plugins_problem(_lua: &Lua, plugins: String) -> LuaResult<Option<String>> {
    Ok(validation::plugins_problem(&json(&plugins)?))
}

lua_table! {
    /// Claude Code launch checks shared with the subscription provider. JSON inputs retain
    /// the distinction between null, objects and arrays.
    "maki.claude_code" => pub(crate) fn create_claude_code_table(perms: &PluginPermissions), DOCS [
        version, environment, config_dir, settings_conflicts, local_settings_dirs(perms), file_conflicts(perms),
        account_problem, policy_problem, init_problem, same_model,
        resolved_model, subscription_type, plugins_problem, cached_version(perms), cache_version(perms), invalidate_version, sanitize_git(perms),
    ]
}

#[cfg(test)]
mod tests {
    use super::{DOT_GIT, sanitize_snapshot};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;
    use test_case::test_case;

    #[test_case(".git")]
    #[test_case(".GiT")]
    fn sanitize_keeps_the_trusted_pointer_and_moves_nested_metadata(name: &str) {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        fs::create_dir_all(snapshot.join("nested")).unwrap();
        let pointer = format!("gitdir: {}\n", git.display());
        fs::write(snapshot.join(DOT_GIT), &pointer).unwrap();
        fs::write(snapshot.join("nested").join(name), "untrusted metadata").unwrap();
        assert_eq!(
            sanitize_snapshot(&snapshot, &quarantine, &git).unwrap(),
            [format!("nested/{name}")]
        );
        assert_eq!(fs::read_to_string(snapshot.join(DOT_GIT)).unwrap(), pointer);
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
        assert!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn sanitize_moves_metadata_links_without_following_directory_links() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join(DOT_GIT);
        let pointer = format!("gitdir: {}\n", git.display());
        fs::write(&target, &pointer).unwrap();
        symlink(&target, snapshot.join(DOT_GIT)).unwrap();
        symlink(&outside, snapshot.join("linked")).unwrap();
        assert!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), pointer);
        assert!(
            fs::symlink_metadata(snapshot.join(DOT_GIT))
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
        assert!(snapshot.join("linked/.git").exists());
    }
}
