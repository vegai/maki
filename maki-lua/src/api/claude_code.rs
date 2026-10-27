use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::io::{Error as IoError, ErrorKind};
use std::ops::RangeInclusive;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::from_utf8;

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::claude_code::validation::{
    self, WorkerHandshake as NativeHandshake, WorkerPolicy,
};
#[cfg(unix)]
use maki_providers::claude_code::{blobs, snapshot};
use mlua::{
    Error as LuaError, Lua, LuaSerdeExt, LuaString, Result as LuaResult, Table, UserData,
    UserDataMethods, Value as LuaValue,
};
use serde_json::Value;

use crate::api::fs::{LOCK_BUSY, path_text};
use crate::api::util::convert::{TableExt, json_to_lua};
use crate::api::util::pair::{Pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

pub(crate) const MODULE: &str = "maki.claude_code.internal";
#[cfg(unix)]
const OWNER_EXECUTE: u32 = 0o100;
const HOME_STATE_PREFIX: &str = ".claude.json";
const BMP_LIMIT: u32 = 0xffff;
const MAX_UTF8_BYTES: usize = 4;
const SYSTEM_EVENT: &str = "system";
const INIT_EVENT: &str = "init";
// Fields consumed by claude_stream.tool_line; the bundled progress-line spec checks this boundary.
const TOOL_PROGRESS_FIELDS: &[&str] = &["file_path", "path", "pattern"];
const VARIATION_SELECTORS: &[RangeInclusive<u32>] = &[
    0x180b..=0x180d,
    0x180f..=0x180f,
    0xfe00..=0xfe0f,
    0xe0100..=0xe01ef,
];

pub(crate) fn create_private_module(
    lua: &Lua,
    perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    let table = create_claude_code_table(lua, perms)?;
    table.set("lock_busy", LOCK_BUSY)?;
    table.set("handshake", json_to_lua(lua, &validation::handshake())?)?;
    table.set("empty_mcp_config", validation::EMPTY_MCP_CONFIG)?;
    table.set("startup_timeout_secs", validation::STARTUP_TIMEOUT_SECS)?;
    table.set("default_executable", validation::DEFAULT_EXECUTABLE)?;
    table.set(
        "default_provider_max_concurrent",
        validation::DEFAULT_MAX_CONCURRENT,
    )?;
    table.set(
        "min_provider_max_concurrent",
        validation::MIN_MAX_CONCURRENT,
    )?;
    Ok((MODULE, table))
}

fn is_printable(character: char) -> bool {
    if character.is_ascii() {
        !character.is_control()
    } else {
        let mut buffer = [0; MAX_UTF8_BYTES + 1];
        buffer[0] = b'a';
        let length = character.encode_utf8(&mut buffer[1..]).len() + 1;
        from_utf8(&buffer[..length]).is_ok_and(|text| text.escape_debug().count() == 2)
    }
}

/// Escape invisible path characters while preserving visible Unicode sequences.
/// @param text string Path or shell-quoted path.
/// @param shell boolean? Emit byte escapes for a Bash command.
/// @return (string) Escaped text, or unchanged invalid UTF-8 bytes.
#[lua_fn]
fn escape_invisible(lua: &Lua, text: LuaString, shell: Option<bool>) -> LuaResult<LuaString> {
    let Ok(value) = text.to_str() else {
        return Ok(text);
    };
    let mut escaped = String::with_capacity(value.len());
    let mut previous: Option<char> = None;
    for character in value.chars() {
        let visible = is_printable(character);
        if visible
            || previous.is_some_and(|base| !base.is_ascii())
                && VARIATION_SELECTORS
                    .iter()
                    .any(|range| range.contains(&u32::from(character)))
        {
            escaped.push(character);
        } else if shell == Some(true) {
            escaped.push_str("'$'");
            for byte in character.encode_utf8(&mut [0; MAX_UTF8_BYTES]).bytes() {
                write!(escaped, "\\{byte:03o}").map_err(LuaError::external)?;
            }
            escaped.push_str("''");
        } else {
            let code = u32::from(character);
            if code <= BMP_LIMIT {
                write!(escaped, "\\u{code:04x}")
            } else {
                write!(escaped, "\\U{code:08x}")
            }
            .map_err(LuaError::external)?;
        }
        previous = visible.then_some(character);
    }
    lua.create_string(escaped)
}

/// Report credentials that a coding worker could read from its shell environment.
/// @param env table Child environment.
/// @return (string?) Reason to refuse the coding call.
#[lua_fn]
fn shell_secret_problem(_lua: &Lua, env: HashMap<String, String>) -> LuaResult<Option<String>> {
    Ok(validation::shell_secret_problem(&env))
}

/// Resolve the checkout's Git directory for the shared import lock.
/// @param project string Checkout directory or one of its subdirectories.
/// @return (string?, string?) Git directory, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn project_git_dir(_lua: Lua, project: String) -> LuaResult<Pair<String>> {
    Ok(pair(
        smol::unblock(move || {
            let project = validation::checkout_root(Path::new(&project)).map_err(IoError::other)?;
            validation::git_directory(&project.join(validation::GIT_ENTRY)).and_then(path_text)
        })
        .await,
    ))
}

/// Create an owned probe directory outside every checkout alias and Git directory.
/// @param base string Prepared temporary directory.
/// @param project string Session directory.
/// @param denied table Checkout aliases and Git directories.
/// @return (string?, string?) Created directory, or nil and the error. The caller owns cleanup.
#[lua_fn(guard = FsWrite)]
async fn private_dir(
    _lua: Lua,
    base: String,
    project: String,
    denied: Vec<String>,
) -> LuaResult<Pair<String>> {
    Ok(pair(
        validation::private_dir(
            PathBuf::from(base),
            PathBuf::from(project),
            denied.into_iter().map(PathBuf::from).collect(),
        )
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
                    files.push(path_text(PathBuf::from(name))?);
                }
            }
            Ok(files)
        })
        .await,
    ))
}

struct WorkerHandshake(NativeHandshake);

impl UserData for WorkerHandshake {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("answer", |_, this, line: String| {
            let event = try_pair!(serde_json::from_str::<Value>(&line));
            Ok(pair(this.0.answer(&event["response"])))
        });
        methods.add_method("finish", |_, this, ()| {
            Ok(pair(this.0.finish().map(|()| true)))
        });
        methods.add_method("resolved_model", |_, this, requested: String| {
            Ok(this.0.resolved_model(&requested).map(str::to_owned))
        });
        methods.add_method("subscription_type", |_, this, ()| {
            Ok(this.0.subscription_type().map(str::to_owned))
        });
    }
}

/// Collect and validate worker control responses with the provider's handshake rules.
/// @param modes table Allowed permission modes.
/// @param policy table Deny rules and optional sandbox paths.
/// @return (userdata) Handshake with answer, finish, resolved_model and subscription_type methods.
#[lua_fn]
fn worker_handshake(lua: &Lua, modes: Vec<String>, policy: Table) -> LuaResult<WorkerHandshake> {
    let policy = lua.from_value(LuaValue::Table(policy))?;
    Ok(WorkerHandshake(NativeHandshake::new(modes, policy)))
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

/// Build the worker settings from deny rules and optional sandbox paths.
/// @param policy table Deny rules and optional sandbox paths.
/// @return (table) Launch settings.
#[lua_fn]
fn worker_settings(lua: &Lua, policy: Table) -> LuaResult<LuaValue> {
    let policy: WorkerPolicy = lua.from_value(LuaValue::Table(policy))?;
    json_to_lua(lua, &policy.settings())
}

/// Prepare the login environment and validate every skipped settings file.
/// @param environ table Parent environment variables.
/// @param configured string? Explicit config directory.
/// @param cwd string Session directory.
/// @return (table?, string?) Validated preparation, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn prepare_launch(
    lua: Lua,
    environ: HashMap<String, String>,
    configured: Option<String>,
    cwd: String,
) -> LuaResult<Pair<Table>> {
    let prepared = try_pair!(
        smol::unblock(move || validation::worker_launch(environ, configured, Path::new(&cwd)))
            .await
    );
    let result = lua.create_table()?;
    result.set("env", lua.create_table_from(prepared.env)?)?;
    result.set("withheld", prepared.withheld)?;
    result.set(
        "denied",
        try_pair!(
            prepared
                .denied
                .into_iter()
                .map(path_text)
                .collect::<Result<Vec<_>, _>>()
        ),
    )?;
    result.set("config_dir", try_pair!(path_text(prepared.config_dir)))?;
    Ok((Some(result), None))
}

struct VersionCheck(validation::VersionCheck);

impl UserData for VersionCheck {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("version", |_, this, ()| Ok(this.0.version.clone()));
        methods.add_method("save", |_, this, output: String| {
            Ok(pair(this.0.save(&output)))
        });
    }
}

/// Capture the executable identity before reading its version.
/// @param executable string Absolute CLI path.
/// @return (userdata?, string?) Check with version() and save(output).
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
                result.set("git_metadata", paths.git_metadata)?;
                result.set("repositories", paths.repositories)?;
                result.set("private_files", paths.private_files)?;
                let symlinks = paths
                    .symlinks
                    .iter()
                    .map(|path| lua.create_string(path.as_os_str().as_bytes()))
                    .collect::<LuaResult<Vec<_>>>()?;
                result.set("symlinks", lua.create_sequence_from(symlinks)?)?;
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

/// Inspect an import target. Missing paths return nil, and blocked or looping parents count as conflicts.
/// @param path string Checkout file path.
/// @return (table?, string?) The target's regular and executable flags, or nil and an I/O error.
#[lua_fn(guard = FsRead)]
async fn checkout_file(lua: Lua, path: String) -> LuaResult<Pair<Table>> {
    let value = try_pair!(
        smol::unblock(move || match fs::symlink_metadata(path) {
            Ok(meta) => {
                #[cfg(unix)]
                let executable = meta.permissions().mode() & OWNER_EXECUTE != 0;
                #[cfg(not(unix))]
                let executable = false;
                Ok(Some((meta.is_file(), executable)))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) if error.kind() == ErrorKind::NotADirectory => Ok(Some((false, false))),
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
                Ok(Some((false, false))),
            Err(error) => Err(error),
        })
        .await
    );
    let value = value
        .map(|(regular, executable)| {
            lua.create_table_from([("regular", regular), ("executable", executable)])
        })
        .transpose()?;
    Ok((value, None))
}

/// Classify a worker event with the provider's protocol rules before conversion to Lua.
/// @param line string Raw stream-json event.
/// @param mode table Accepted-init and expected-model state. Init events require mode.init with cwd, cli and worker.
/// @return (table?, string?) Event and normalized usage, or nil and the protocol error.
#[lua_fn]
fn classify(lua: &Lua, line: String, mode: Table) -> LuaResult<Pair<Table>> {
    let model: Option<String> = mode.opt("model")?;
    let mut classified = try_pair!(validation::classify(
        &line,
        mode.opt::<bool>("accepted")?.unwrap_or(false),
        model.as_deref(),
        mode.opt::<bool>("model_checked")?.unwrap_or(false),
    ));
    let event = &classified.event;
    if classified.problem.is_none()
        && event["type"] == SYSTEM_EVENT
        && event["subtype"] == INIT_EVENT
    {
        let expect: Table = mode.get("init")?;
        let version: String = expect.get::<Table>("cli")?.get("version")?;
        let cwd: String = expect.get("cwd")?;
        let worker: Table = expect.get("worker")?;
        let tools: Vec<String> = worker.get("tools")?;
        let modes: Vec<String> = worker.get("reported_modes")?;
        classified.problem = validation::init_problem(
            event,
            &version,
            Path::new(&cwd),
            tools.into_iter().collect(),
            &modes,
        );
    }
    let event = &mut classified.event;
    match event["type"].as_str() {
        Some("assistant") => {
            if let Some(content) = event["message"]
                .get_mut("content")
                .and_then(Value::as_array_mut)
            {
                content.retain(|block| block["type"] == "tool_use");
                for block in content {
                    if let Some(input) = block.get_mut("input").and_then(Value::as_object_mut) {
                        input.retain(|key, _| TOOL_PROGRESS_FIELDS.contains(&key.as_str()));
                    }
                }
            }
        }
        Some("result" | "control_response" | "rate_limit_event") => {}
        Some(SYSTEM_EVENT) if event["subtype"] == INIT_EVENT => {}
        _ => {
            if let Some(object) = event.as_object_mut() {
                object.retain(|key, _| key == "type" || key == "subtype");
            }
        }
    }
    let result = lua.create_table()?;
    result.set("event", json_to_lua(lua, event)?)?;
    result.set("model_checked", classified.model_checked)?;
    result.set("problem", classified.problem)?;
    result.set("failure", classified.failure)?;
    if let Some(usage) = &classified.usage {
        result.set("usage", json_to_lua(lua, usage)?)?;
    }
    if let Some(rate_limit) = &classified.rate_limit {
        result.set("rate_limit", lua.to_value(rate_limit)?)?;
    }
    Ok((Some(result), None))
}

lua_table! {
    /// Private helpers for the bundled Claude Code plugin. JSON inputs retain
    /// the distinction between null, objects and arrays.
    "maki.claude_code.internal" => pub(crate) fn create_claude_code_table(perms: &PluginPermissions), DOCS [
        shell_secret_problem, escape_invisible, worker_handshake, base_args, worker_settings, prepare_launch(perms), private_dir(perms), home_state_files(perms),
        classify, checkout_file(perms), version_check(perms), invalidate_version, sanitize_git(perms), project_git_dir(perms), unpack_blobs(perms),
    ]
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::{PermissionsExt, symlink};

    use mlua::{AnyUserData, Function, Lua, ObjectLike, Table, Value as LuaValue};
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use test_case::test_case;

    use super::{create_claude_code_table, is_printable};
    use crate::api::util::convert::json_to_lua;
    use crate::plugin_permissions::PluginPermissions;
    use maki_providers::claude_code::validation::GIT_ENTRY;

    const LINKED: &str = "linked";
    const NESTED: &str = "src";
    const FILE_CONTENT: &str = "file";

    #[test_case('é', true ; "accented_letter")]
    #[test_case('\u{301}', true ; "combining_accent")]
    #[test_case('\u{94d}', true ; "virama")]
    #[test_case('\u{fe0f}', false ; "emoji_variation_selector")]
    #[test_case('\u{e0100}', false ; "supplemental_variation_selector")]
    #[test_case('\u{180b}', false ; "mongolian_variation_selector")]
    #[test_case('\u{200b}', false ; "zero_width_space")]
    #[test_case('\u{202e}', false ; "bidi_override")]
    #[test_case('\u{2028}', false ; "line_separator")]
    #[test_case('\u{80}', false ; "control")]
    fn printability_identifies_standalone_visible_characters(character: char, expected: bool) {
        assert_eq!(is_printable(character), expected);
    }

    #[test_case(0x180b ; "mongolian")]
    #[test_case(0x180f ; "fourth_mongolian")]
    #[test_case(0xfe0f ; "emoji")]
    #[test_case(0xe0100 ; "supplemental")]
    fn variation_selectors_need_a_non_ascii_predecessor(code: u32) {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let escape: Function = table.get("escape_invisible").unwrap();
        for previous in [None, Some('n')] {
            let input = format!(
                "{}{}",
                previous.map(String::from).unwrap_or_default(),
                char::from_u32(code).unwrap()
            );
            let escaped: String = escape.call(input.clone()).unwrap();
            assert_ne!(escaped, input);
        }
    }

    #[test]
    fn malformed_worker_json_returns_errors_without_throwing() {
        const INVALID_JSON: &str = "{";
        let expected = serde_json::from_str::<Value>(INVALID_JSON)
            .unwrap_err()
            .to_string();
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let create: Function = table.get("worker_handshake").unwrap();
        let policy = json_to_lua(&lua, &json!({ "deny": [] })).unwrap();
        let handshake: AnyUserData = create.call((vec!["default"], policy)).unwrap();
        let (complete, error): (Option<bool>, Option<String>) =
            handshake.call_method("answer", INVALID_JSON).unwrap();
        assert_eq!(complete, None);
        assert_eq!(error, Some(expected.clone()));
        let classify: Function = table.get("classify").unwrap();
        let (event, error): (Option<Table>, Option<String>) = classify
            .call((INVALID_JSON, lua.create_table().unwrap()))
            .unwrap();
        assert!(event.is_none());
        assert!(error.unwrap().contains(&expected));
    }

    #[test_case(false ; "file_parent")]
    #[test_case(true ; "looping_parent")]
    fn checkout_targets_with_blocked_parents_are_conflicts(looping: bool) {
        let root = tempdir().unwrap();
        let parent = root.path().join(NESTED);
        if looping {
            symlink(NESTED, &parent).unwrap();
        } else {
            fs::write(&parent, "file").unwrap();
        }
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let inspect: Function = table.get("checkout_file").unwrap();
        let (meta, error): (Option<Table>, Option<String>) =
            smol::block_on(inspect.call_async(parent.join(LINKED).to_str().unwrap())).unwrap();
        assert!(!meta.unwrap().get::<bool>("regular").unwrap());
        assert_eq!(error, None);
    }

    #[test_case(0o700, true ; "owner_execute")]
    #[test_case(0o750, true ; "owner_and_group_execute")]
    #[test_case(0o650, false ; "group_execute_only")]
    #[test_case(0o4600, false ; "setuid_without_execute")]
    fn checkout_metadata_uses_the_owners_execute_bit(mode: u32, executable: bool) {
        let root = tempdir().unwrap();
        let path = root.path().join(LINKED);
        fs::write(&path, FILE_CONTENT).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let inspect: Function = table.get("checkout_file").unwrap();
        let (meta, error): (Option<Table>, Option<String>) =
            smol::block_on(inspect.call_async(path.to_str().unwrap())).unwrap();
        assert_eq!(error, None);
        let meta = meta.unwrap();
        assert!(meta.get::<bool>("regular").unwrap());
        assert_eq!(meta.get::<bool>("executable").unwrap(), executable);
    }

    #[test]
    fn ignored_worker_events_do_not_copy_payloads_into_lua() {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let classify: Function = table.get("classify").unwrap();
        let input = json!({ "type": "user", "message": {"content": "x".repeat(4096)} }).to_string();
        let mode = lua.create_table().unwrap();
        mode.set("accepted", true).unwrap();
        let (result, error): (Option<Table>, Option<String>) =
            classify.call((input, mode)).unwrap();
        assert_eq!(error, None);
        let event: Table = result.unwrap().get("event").unwrap();
        assert_eq!(event.get::<String>("type").unwrap(), "user");
        assert!(event.get::<Option<Table>>("message").unwrap().is_none());
    }

    #[test]
    fn oversized_worker_events_return_a_bounded_error() {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let classify: Function = table.get("classify").unwrap();
        let input = "x".repeat(maki_providers::claude_code::validation::MAX_EVENT_BYTES + 1);
        let (result, error): (Option<Table>, Option<String>) =
            classify.call((input, lua.create_table().unwrap())).unwrap();
        assert!(result.is_none());
        assert!(error.unwrap().contains("limit"));
    }

    #[test_case(false ; "direct_denied_directory")]
    #[test_case(true ; "symlinked_denied_directory")]
    fn worker_probe_directories_reject_checkout_aliases(through_link: bool) {
        const DENIED_TEMP: &str = "is in the project";
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        let denied = root.path().join("denied");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&denied).unwrap();
        let alias = root.path().join(LINKED);
        symlink(&denied, &alias).unwrap();
        let base = if through_link { &alias } else { &denied };
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let create: Function = table.get("private_dir").unwrap();
        let (path, error): (Option<String>, Option<String>) = smol::block_on(create.call_async((
            base.to_str().unwrap(),
            project.to_str().unwrap(),
            vec![denied.to_str().unwrap()],
        )))
        .unwrap();
        assert_eq!(path, None);
        assert!(error.unwrap().contains(DENIED_TEMP));
        assert!(fs::read_dir(&denied).unwrap().next().is_none());
    }

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

    #[test_case(json!({}) ; "missing_deny_rules")]
    #[test_case(json!({ "deny": [], "sandbox": {} }) ; "missing_sandbox_paths")]
    #[test_case(json!({ "deny": [], "sandbox": { "deny_read": [], "allow_read": [], "allow_write": [], "disabled": true } }) ; "unknown_sandbox_policy")]
    fn worker_policy_requires_valid_input(policy: Value) {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let policy = json_to_lua(&lua, &policy).unwrap();
        let settings: Function = table.get("worker_settings").unwrap();
        assert!(settings.call::<LuaValue>(policy.clone()).is_err());
        let handshake: Function = table.get("worker_handshake").unwrap();
        assert!(
            handshake
                .call::<LuaValue>((Vec::<String>::new(), policy))
                .is_err()
        );
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
