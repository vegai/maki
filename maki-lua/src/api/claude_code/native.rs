use super::{blobs, snapshot};

use std::collections::HashMap;
use std::env;
use std::fmt::{self, Write};
use std::fs;
use std::io::{Error as IoError, ErrorKind};
use std::ops::RangeInclusive;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::claude_code::worker::{
    self, EventKind, WorkerHandshake as NativeHandshake, WorkerPolicy,
};
use maki_providers::claude_code::{Error as WorkerError, HOME_STATE_PREFIX};
use mlua::{
    Error as LuaError, Lua, LuaSerdeExt, LuaString, Result as LuaResult, Table, UserData,
    UserDataMethods, UserDataRef, UserDataRefMut, Value as LuaValue,
};
use rustix::io::Errno;
use serde_json::Value;
use tokio::task::spawn_blocking;
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::api::fs::path_text;
use crate::api::util::convert::{TableExt, json_to_lua};
use crate::api::util::ctx::LuaCtx;
use crate::api::util::pair::{Pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const OWNER_EXECUTE: u32 = 0o100;
const BMP_LIMIT: u32 = 0xffff;
const MAX_UTF8_BYTES: usize = 4;
// Fields consumed by claude_stream.tool_line; the bundled progress-line spec checks this boundary.
const TOOL_PROGRESS_FIELDS: &[&str] = &["file_path", "path", "pattern"];
const VARIATION_SELECTORS: &[RangeInclusive<u32>] = &[
    0x180b..=0x180d,
    0x180f..=0x180f,
    0xfe00..=0xfe0f,
    0xe0100..=0xe01ef,
];
/// Characters that draw nothing although their category usually draws: the
/// combining grapheme joiner, Khmer inherent vowels and the Hangul fillers.
const BLANK_CHARACTERS: &[RangeInclusive<u32>] = &[
    0x034f..=0x034f,
    0x115f..=0x1160,
    0x17b4..=0x17b5,
    0x3164..=0x3164,
    0xffa0..=0xffa0,
];

pub(super) fn create_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let table = create_claude_code_table(lua, perms)?;
    table.set("handshake_input", worker::handshake_input())?;
    Ok(table)
}

fn is_printable(character: char) -> bool {
    if character.is_ascii() {
        return !character.is_control();
    }
    let code = u32::from(character);
    !matches!(
        get_general_category(character),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::SpaceSeparator
    ) && !BLANK_CHARACTERS
        .iter()
        .chain(VARIATION_SELECTORS)
        .any(|range| range.contains(&code))
}

fn escape_bytes(escaped: &mut String, bytes: &[u8], shell: bool) -> fmt::Result {
    if shell {
        escaped.push_str("'$'");
    }
    for byte in bytes {
        write!(escaped, "\\{byte:03o}")?;
    }
    if shell {
        escaped.push_str("''");
    }
    Ok(())
}

/// Quote a path for Bash or escape invisible characters for display.
/// @param text string Raw path bytes.
/// @param shell boolean? Emit a Bash-quoted path.
/// @return (string) Escaped path.
#[lua_fn]
fn escape_path(lua: &Lua, text: LuaString, shell: Option<bool>) -> LuaResult<LuaString> {
    let shell = shell == Some(true);
    let bytes = text.as_bytes();
    let mut escaped = String::with_capacity(bytes.len());
    if shell {
        escaped.push('\'');
    }
    let mut previous: Option<char> = None;
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            let visible = is_printable(character);
            if character.is_ascii() && character.is_control() {
                escape_bytes(&mut escaped, &[character as u8], shell)
                    .map_err(LuaError::external)?;
            } else if visible
                || previous.is_some_and(|base| !base.is_ascii())
                    && VARIATION_SELECTORS
                        .iter()
                        .any(|range| range.contains(&u32::from(character)))
            {
                match (character, shell) {
                    ('\'', true) => escaped.push_str("'\\''"),
                    ('\\', false) => escaped.push_str("\\\\"),
                    _ => escaped.push(character),
                }
            } else if shell {
                escape_bytes(
                    &mut escaped,
                    character.encode_utf8(&mut [0; MAX_UTF8_BYTES]).as_bytes(),
                    true,
                )
                .map_err(LuaError::external)?;
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
        if !chunk.invalid().is_empty() {
            escape_bytes(&mut escaped, chunk.invalid(), shell).map_err(LuaError::external)?;
            previous = None;
        }
    }
    if shell {
        escaped.push('\'');
    }
    lua.create_string(escaped)
}

/// Report credentials that a coding worker could read from its shell environment.
/// @param env table Child environment.
/// @return (string?) Reason to refuse the coding call.
#[lua_fn]
fn shell_secret_problem(_lua: &Lua, env: HashMap<String, String>) -> LuaResult<Option<String>> {
    Ok(worker::shell_secret_problem(&env))
}

/// Resolve the checkout's Git directory for the shared import lock.
/// @param project string Checkout directory or one of its subdirectories.
/// @return (string?, string?) Git directory, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn project_git_dir(_lua: Lua, project: String) -> LuaResult<Pair<String>> {
    Ok(pair(
        spawn_blocking(move || {
            let project = worker::checkout_root(Path::new(&project)).map_err(IoError::other)?;
            worker::git_directory(&project.join(worker::GIT_ENTRY)).and_then(path_text)
        })
        .await
        .map_err(LuaError::external)?,
    ))
}

/// Existing home state and temporary copies that must be hidden from workers.
/// @param home string Home directory.
/// @return (table?, string?) State filenames, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn home_state_files(_lua: Lua, home: String) -> LuaResult<Pair<Vec<String>>> {
    Ok(pair(
        spawn_blocking(move || {
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
        .await
        .map_err(LuaError::external)?,
    ))
}

struct Policy(Arc<WorkerPolicy>);

impl UserData for Policy {}

/// Parse the immutable worker policy once for argv and handshake validation.
/// @param policy table Deny rules and optional sandbox paths.
/// @return (userdata) Shared policy.
#[lua_fn]
fn worker_policy(lua: &Lua, policy: Table) -> LuaResult<Policy> {
    Ok(Policy(Arc::new(lua.from_value(LuaValue::Table(policy))?)))
}

struct WorkerHandshake(NativeHandshake);

impl UserData for WorkerHandshake {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("resolved_model", |_, this, requested: String| {
            Ok(pair(this.0.resolved_model(&requested).map(str::to_owned)))
        });
        methods.add_method("subscription_type", |_, this, ()| {
            Ok(this.0.subscription_type().map(str::to_owned))
        });
    }
}

/// Collect and validate worker control responses with the provider's handshake rules.
/// @param policy userdata Shared worker policy.
/// @return (userdata) Handshake with resolved_model and subscription_type methods.
#[lua_fn]
fn worker_handshake(_lua: &Lua, policy: UserDataRef<Policy>) -> LuaResult<WorkerHandshake> {
    Ok(WorkerHandshake(NativeHandshake::new(Arc::clone(&policy.0))))
}

/// Build worker arguments without serializing launch settings through Lua.
/// @param executable string Absolute worker executable.
/// @param model string Model name.
/// @param policy userdata Shared worker policy.
/// @return (table) Command-line arguments.
#[lua_fn]
fn worker_args(
    _lua: &Lua,
    executable: String,
    model: String,
    policy: UserDataRef<Policy>,
) -> LuaResult<Vec<String>> {
    let mut args = policy.0.args(&model);
    args.insert(0, executable);
    Ok(args)
}

/// Encode prompt blocks in the shared worker protocol.
/// @param blocks table Prompt text blocks in order.
/// @return (string) NDJSON user message.
#[lua_fn]
fn user_message(_lua: &Lua, blocks: Vec<String>) -> LuaResult<String> {
    worker::user_message(&blocks).map_err(LuaError::external)
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
        spawn_blocking(move || worker::prepare_launch(
            environ.into_iter().collect(),
            configured.map(PathBuf::from),
            Path::new(&cwd)
        ))
        .await
        .map_err(LuaError::external)?
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
    result.set("temp_dir", try_pair!(path_text(prepared.temp_dir)))?;
    Ok((Some(result), None))
}

/// Validate the executable version with the shared process supervisor and cache.
/// @param ctx LuaCtx Calling handler context.
/// @param executable string Absolute CLI path.
/// @param prepared table Validated launch preparation.
/// @param cwd string Session directory.
/// @param startup_secs integer Startup timeout.
/// @return (string?, string?) Validated version, or nil and the error.
#[lua_fn(guard = [FsWrite, Run])]
async fn checked_profile(
    _lua: Lua,
    ctx: UserDataRef<LuaCtx>,
    executable: String,
    prepared: Table,
    cwd: String,
    startup_secs: u64,
) -> LuaResult<Pair<String>> {
    let cancel = ctx.cancel.clone();
    drop(ctx);
    let env = prepared
        .get::<Table>("env")?
        .pairs::<String, String>()
        .collect::<LuaResult<Vec<_>>>()?;
    let denied = prepared.get::<Vec<PathBuf>>("denied")?;
    let temp_dir = prepared.get::<PathBuf>("temp_dir")?;
    Ok(pair(
        worker::checked_profile(
            Path::new(&executable),
            &env,
            Path::new(&cwd),
            &denied,
            &temp_dir,
            startup_secs,
            cancel.cancelled(),
        )
        .await,
    ))
}

/// Run the unprompted worker checks under the shared process supervisor.
/// @param ctx LuaCtx Calling context.
/// @param spec table Prepared worker launch and immutable policy.
/// @param env table Child environment.
/// @param startup_secs integer Startup deadline in seconds.
/// @return (boolean?, string?, string?) Result, error, and any probe directory kept for its contents.
#[lua_fn(guard = [FsWrite, Run])]
async fn worker_probe(
    _lua: Lua,
    ctx: UserDataRef<LuaCtx>,
    spec: Table,
    env: HashMap<String, String>,
    startup_secs: u64,
) -> LuaResult<(Option<bool>, Option<String>, Option<String>)> {
    let cancel = ctx.cancel.clone();
    drop(ctx);
    let policy: UserDataRef<Policy> = spec.get("policy")?;
    let probe = worker::WorkerProbe {
        executable: PathBuf::from(spec.get::<String>("executable")?),
        env: env.into_iter().collect(),
        project: PathBuf::from(spec.get::<String>("cwd")?),
        denied: spec
            .get::<Vec<String>>("denied")?
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        temp_dir: PathBuf::from(spec.get::<String>("temp_dir")?),
        policy: Arc::clone(&policy.0),
        model: spec.get::<String>("model")?,
        startup: Duration::from_secs(startup_secs),
    };
    drop(policy);
    let (result, kept) = probe.run(cancel.cancelled()).await;
    let kept = kept
        .map(path_text)
        .transpose()
        .map_err(LuaError::external)?;
    let (ok, error) = pair(result.map(|()| true));
    Ok((ok, error, kept))
}

/// Move Git metadata out of a stopped worker's snapshot and restore its trusted root pointer.
/// Embedded repositories are quarantined with their configs disabled. Directory links are not followed.
/// @param snapshot string Snapshot directory.
/// @param quarantine string Private directory for preserved metadata.
/// @param git string Trusted artifact repository.
/// @param prune_links boolean Remove escaping links before the worker starts.
/// @return (table?, string?) Metadata and repository paths, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn sanitize_git(
    lua: Lua,
    snapshot: String,
    quarantine: String,
    git: String,
    prune_links: bool,
) -> LuaResult<Pair<Table>> {
    let result = spawn_blocking(move || {
        snapshot::sanitize_snapshot(
            Path::new(&snapshot),
            Path::new(&quarantine),
            Path::new(&git),
            prune_links,
        )
    })
    .await
    .map_err(LuaError::external)?;
    let paths = try_pair!(result);
    let result = lua.create_table()?;
    result.set("git_metadata", path_list(&lua, &paths.git_metadata)?)?;
    result.set("repositories", path_list(&lua, &paths.repositories)?)?;
    result.set("private_files", paths.private_files)?;
    result.set("dropped_links", path_list(&lua, &paths.dropped_links)?)?;
    Ok((Some(result), None))
}

fn path_list(lua: &Lua, paths: &[PathBuf]) -> LuaResult<Table> {
    let paths = paths
        .iter()
        .map(|path| lua.create_string(path.as_os_str().as_bytes()))
        .collect::<LuaResult<Vec<_>>>()?;
    lua.create_sequence_from(paths)
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
    Ok(pair(
        spawn_blocking(move || {
            blobs::unpack(Path::new(&batch), Path::new(&stage), &hashes).map(|()| true)
        })
        .await
        .map_err(LuaError::external)?,
    ))
}

/// Resolve an artifact root, including a missing tail, outside the checkout.
/// @param root string Artifact root.
/// @param cwd string Session directory.
/// @param denied table Protected checkout directories.
/// @return (string?, string?) Resolved path, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn resolve_root(
    _lua: Lua,
    root: String,
    cwd: String,
    denied: Vec<String>,
) -> LuaResult<Pair<String>> {
    Ok(pair(
        spawn_blocking(move || {
            let resolved = worker::resolve_missing(Path::new(&root))
                .map_err(|error| format!("maki cannot read {root}: {error}"))?;
            let denied: Vec<PathBuf> = denied.into_iter().map(PathBuf::from).collect();
            if worker::inside_checkout(&resolved, Path::new(&cwd), &denied) {
                return Err(format!(
                    "the artifact directory {} is in the project",
                    resolved.display()
                ));
            }
            path_text(resolved).map_err(|error| error.to_string())
        })
        .await
        .map_err(LuaError::external)?,
    ))
}

/// Inspect import targets in one filesystem operation. Missing entries are false.
/// @param paths table Checkout file paths.
/// @return (table?, string?) Regular and owner-executable flags, or nil and an I/O error.
#[lua_fn(guard = FsRead)]
async fn checkout_files(lua: Lua, paths: Vec<String>) -> LuaResult<Pair<Table>> {
    let values = try_pair!(
        spawn_blocking(move || paths
            .into_iter()
            .map(|path| {
                match fs::symlink_metadata(path) {
                    Ok(meta) => Ok(Some((
                        meta.is_file(),
                        meta.permissions().mode() & OWNER_EXECUTE != 0,
                    ))),
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                    Err(error)
                        if error.kind() == ErrorKind::NotADirectory
                            || error.raw_os_error() == Some(Errno::LOOP.raw_os_error()) =>
                    {
                        Ok(Some((false, false)))
                    }
                    Err(error) => Err(error),
                }
            })
            .collect::<Result<Vec<_>, IoError>>())
        .await
        .map_err(LuaError::external)?
    );
    let values = values
        .into_iter()
        .map(|meta| match meta {
            Some((regular, executable)) => lua
                .create_table_from([("regular", regular), ("executable", executable)])
                .map(LuaValue::Table),
            None => Ok(LuaValue::Boolean(false)),
        })
        .collect::<LuaResult<Vec<_>>>()?;
    Ok((Some(lua.create_sequence_from(values)?), None))
}

impl WorkerStream {
    fn invalidate(&self, error: &WorkerError) {
        if let Some(executable) = &self.executable {
            worker::invalidate_on_error(executable, error);
        }
    }
}

struct WorkerStream {
    stream: worker::WorkerStream,
    executable: Option<PathBuf>,
}

impl UserData for WorkerStream {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("usage_totals", |lua, this, ()| {
            match this.stream.usage_totals() {
                Some((totals, partial)) => Ok((json_to_lua(lua, &totals)?, partial)),
                None => Ok((LuaValue::Nil, false)),
            }
        });
        methods.add_method_mut("set_model", |_, this, model: String| {
            this.stream.set_model(model);
            Ok(())
        });
        methods.add_method_mut("mark_prompted", |_, this, ()| {
            this.stream.mark_prompted();
            Ok(())
        });
        methods.add_method("accepted", |_, this, ()| Ok(this.stream.accepted()));
        methods.add_method_mut(
            "feed",
            |lua, this, (line, handshake): (LuaString, Option<UserDataRefMut<WorkerHandshake>>)| {
                let line = line.to_str()?;
                let mut classified = try_pair!(
                    this.stream
                        .feed(&line)
                        .inspect_err(|error| this.invalidate(error))
                );
                if let Some(error) = &classified.problem {
                    this.invalidate(error);
                }
                let control = classified.kind == Some(EventKind::ControlResponse)
                    && classified.event["response"].is_object();
                let complete = if control {
                    try_pair!(
                        handshake
                            .map(|mut handshake| handshake.0.answer(&classified.event["response"]))
                            .transpose()
                            .inspect_err(|error| this.invalidate(error))
                    )
                } else {
                    None
                };
                event_table(lua, &mut classified, control, complete)
            },
        );
    }
}

/// Own worker init and model checks for one process.
/// @param expect table Expected cwd, CLI version and immutable worker policy.
/// @return (userdata) Stateful event classifier.
#[lua_fn]
fn worker_stream(_lua: &Lua, expect: Table) -> LuaResult<WorkerStream> {
    let policy: UserDataRef<Policy> = expect.get("policy")?;
    Ok(WorkerStream {
        executable: expect.opt::<String>("executable")?.map(PathBuf::from),
        stream: worker::WorkerStream::new(
            expect.get::<String>("version")?,
            PathBuf::from(expect.get::<String>("cwd")?),
            &policy.0,
        ),
    })
}

fn event_table(
    lua: &Lua,
    classified: &mut worker::WorkerEvent,
    control: bool,
    complete: Option<bool>,
) -> LuaResult<Pair<Table>> {
    let event = &mut classified.event;
    match classified.kind {
        Some(EventKind::Assistant) => {
            if let Some(content) = event["message"]
                .get_mut("content")
                .and_then(Value::as_array_mut)
            {
                content.retain(|block| block["type"] == worker::TOOL_USE_BLOCK);
                for block in content {
                    if let Some(input) = block.get_mut("input").and_then(Value::as_object_mut) {
                        input.retain(|key, _| TOOL_PROGRESS_FIELDS.contains(&key.as_str()));
                    }
                }
            }
        }
        Some(EventKind::Result) => {}
        _ => {
            if let Some(object) = event.as_object_mut() {
                object.retain(|key, _| key == "type" || key == "subtype");
            }
        }
    }
    let result = lua.create_table()?;
    result.set(
        "kind",
        classified
            .kind
            .map(|kind| lua.to_value(&kind))
            .transpose()?,
    )?;
    result.set("event", json_to_lua(lua, event)?)?;
    result.set("control", control)?;
    result.set("complete", complete)?;
    result.set(
        "problem",
        classified.problem.as_ref().map(|error| error.to_string()),
    )?;
    result.set("failure", classified.failure.as_deref())?;
    if let Some(rate_limit) = &classified.rate_limit {
        result.set("rate_limit", lua.to_value(rate_limit)?)?;
    }
    Ok((Some(result), None))
}

/// Find the configured worker executable.
/// @param name string Executable name or path.
#[lua_fn(guard = FsRead)]
async fn executable(_lua: Lua, name: String) -> LuaResult<Pair<String>> {
    let path = try_pair!(
        spawn_blocking(move || {
            let cwd = env::current_dir().map_err(|error| error.to_string())?;
            worker::executable(&name, &cwd).map_err(|error| error.to_string())
        })
        .await
        .map_err(LuaError::external)?
    );
    Ok(pair(path_text(path)))
}

/// Startup deadline diagnostic shared with the provider.
/// @param seconds integer Startup time limit.
#[lua_fn]
fn startup_late(_lua: &Lua, seconds: u64) -> LuaResult<String> {
    Ok(worker::startup_late(seconds))
}

/// Worker exit diagnostic shared with the provider.
/// @param code integer Exit code.
/// @param after_result boolean Whether the result arrived.
#[lua_fn]
fn worker_exit(_lua: &Lua, code: i32, after_result: bool) -> LuaResult<String> {
    Ok(worker::worker_exit(code, after_result).to_string())
}

lua_table! {
    /// Private helpers for the bundled Claude Code plugin. JSON inputs retain
    /// the distinction between null, objects and arrays.
    "maki.claude_code.internal" => pub(crate) fn create_claude_code_table(perms: &PluginPermissions), DOCS [
        executable(perms), startup_late, worker_exit, shell_secret_problem, escape_path, worker_policy, worker_handshake, worker_args, worker_probe(perms), user_message, prepare_launch(perms), resolve_root(perms), home_state_files(perms),
        worker_stream, checkout_files(perms), checked_profile(perms), sanitize_git(perms), project_git_dir(perms), unpack_blobs(perms),
    ]
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs::{self, Permissions};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::process::Command;

    use mlua::{AnyUserData, Function, Lua, LuaString, ObjectLike, Table, Value as LuaValue};
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use test_case::test_case;

    use super::{create_claude_code_table, is_printable};
    use crate::api::util::convert::json_to_lua;
    use crate::plugin_permissions::{Permission, PluginPermissions};
    use maki_providers::claude_code::worker::GIT_ENTRY;

    const LINKED: &str = "linked";
    const NESTED: &str = "src";
    const FILE_CONTENT: &str = "file";
    const PRINT_PATH: &str = "printf %s ";
    const NON_UTF8_DIR: &[u8] = b"nested\xff";

    fn worker_stream(lua: &Lua, table: &Table) -> AnyUserData {
        let create: Function = table.get("worker_stream").unwrap();
        let expect = lua.create_table().unwrap();
        expect.set("cwd", "/workspace").unwrap();
        expect.set("version", "2.1.284").unwrap();
        let policy: Function = table.get("worker_policy").unwrap();
        let policy: AnyUserData = policy
            .call(json_to_lua(lua, &json!({"deny": []})).unwrap())
            .unwrap();
        expect.set("policy", policy).unwrap();
        let stream: AnyUserData = create.call(expect).unwrap();
        stream.call_method::<()>("mark_prompted", ()).unwrap();
        stream
    }

    #[test_case("checked_profile", Permission::Run ; "version_check_requires_run")]
    #[test_case("checked_profile", Permission::FsWrite ; "version_check_requires_temporary_files")]
    #[test_case("worker_probe", Permission::Run ; "probe_requires_run")]
    #[test_case("worker_probe", Permission::FsWrite ; "probe_requires_temporary_files")]
    #[test_case("executable", Permission::FsRead ; "executable_lookup_requires_read")]
    fn native_launch_helpers_enforce_permissions(name: &str, permission: Permission) {
        let lua = Lua::new();
        let mut permissions = PluginPermissions::trusted();
        permissions.set(permission, false);
        let table = create_claude_code_table(&lua, &permissions).unwrap();
        let function: Function = table.get(name).unwrap();
        let error = crate::test_support::runtime()
            .block_on(function.call_async::<LuaValue>(()))
            .unwrap_err();
        assert!(error.to_string().contains(permission.manifest_key()));
    }

    #[test_case(false ; "git_metadata")]
    #[test_case(true ; "repository")]
    fn quarantined_paths_keep_their_filename_bytes_in_lua(repository: bool) {
        let root = tempdir().unwrap();
        let snapshot = root.path().join("snapshot");
        let quarantine = root.path().join("quarantine");
        let git = root.path().join("git");
        let relative = PathBuf::from(OsStr::from_bytes(NON_UTF8_DIR));
        let directory = snapshot.join(&relative);
        fs::create_dir_all(&directory).unwrap();
        let (field, expected) = if repository {
            fs::create_dir(directory.join("objects")).unwrap();
            fs::write(directory.join("HEAD"), FILE_CONTENT).unwrap();
            ("repositories", relative)
        } else {
            fs::write(directory.join(GIT_ENTRY), FILE_CONTENT).unwrap();
            ("git_metadata", relative.join(GIT_ENTRY))
        };
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let sanitize: Function = table.get("sanitize_git").unwrap();
        let (result, error): (Option<Table>, Option<String>) = crate::test_support::runtime()
            .block_on(sanitize.call_async((
                snapshot.to_str().unwrap(),
                quarantine.to_str().unwrap(),
                git.to_str().unwrap(),
                true,
            )))
            .unwrap();
        assert_eq!(error, None);
        let paths: Vec<LuaString> = result.unwrap().get(field).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(&*paths[0].as_bytes(), expected.as_os_str().as_bytes());
    }

    #[test_case(b"a'\\\n\xff\xc3\xa9", "a'\\\\\\012\\377é" ; "mixed_valid_and_invalid_utf8")]
    #[test_case(b"a\xe2\x80\xae\xff", "a\\u202e\\377" ; "bidi_and_invalid_byte")]
    fn path_escaping_preserves_shell_bytes_and_printable_unicode(bytes: &[u8], display: &str) {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let escape: Function = table.get("escape_path").unwrap();
        let input = lua.create_string(bytes).unwrap();
        let shown: String = escape.call((input.clone(), false)).unwrap();
        assert_eq!(shown, display);
        let quoted: String = escape.call((input, true)).unwrap();
        let output = Command::new("bash")
            .args(["-c", &format!("{PRINT_PATH}{quoted}")])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, bytes);
    }

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
    #[test_case('\u{a0}', false ; "no_break_space")]
    #[test_case('\u{34f}', false ; "combining_grapheme_joiner")]
    #[test_case('\u{3164}', false ; "hangul_filler")]
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
        let escape: Function = table.get("escape_path").unwrap();
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
        let stream = worker_stream(&lua, &table);
        let (event, error): (Option<Table>, Option<String>) =
            stream.call_method("feed", INVALID_JSON).unwrap();
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
        let inspect: Function = table.get("checkout_files").unwrap();
        let (meta, error): (Option<Table>, Option<String>) = crate::test_support::runtime()
            .block_on(inspect.call_async(vec![parent.join(LINKED).to_str().unwrap()]))
            .unwrap();
        assert!(
            !meta
                .unwrap()
                .get::<Table>(1)
                .unwrap()
                .get::<bool>("regular")
                .unwrap()
        );
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
        let inspect: Function = table.get("checkout_files").unwrap();
        let (meta, error): (Option<Table>, Option<String>) = crate::test_support::runtime()
            .block_on(inspect.call_async(vec![path.to_str().unwrap()]))
            .unwrap();
        assert_eq!(error, None);
        let meta: Table = meta.unwrap().get(1).unwrap();
        assert!(meta.get::<bool>("regular").unwrap());
        assert_eq!(meta.get::<bool>("executable").unwrap(), executable);
    }

    #[test]
    fn ignored_worker_events_do_not_copy_payloads_into_lua() {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let stream = worker_stream(&lua, &table);
        let input = json!({ "type": "user", "message": {"content": "x".repeat(4096)} }).to_string();
        let (result, error): (Option<Table>, Option<String>) =
            stream.call_method("feed", input).unwrap();
        assert_eq!(error, None);
        let event: Table = result.unwrap().get("event").unwrap();
        assert_eq!(event.get::<String>("type").unwrap(), "user");
        assert!(event.get::<Option<Table>>("message").unwrap().is_none());
    }

    #[test]
    fn oversized_worker_events_return_a_bounded_error() {
        let lua = Lua::new();
        let table = create_claude_code_table(&lua, &PluginPermissions::trusted()).unwrap();
        let stream = worker_stream(&lua, &table);
        let input = "x".repeat(maki_providers::claude_code::worker::MAX_EVENT_BYTES + 1);
        let (result, error): (Option<Table>, Option<String>) =
            stream.call_method("feed", input).unwrap();
        assert!(result.is_none());
        assert!(error.unwrap().contains("limit"));
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
            crate::test_support::runtime()
                .block_on(list.call_async(home.path().to_str().unwrap()))
                .unwrap();
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
        let parse: Function = table.get("worker_policy").unwrap();
        assert!(parse.call::<LuaValue>(policy).is_err());
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
        let (directory, error): (Option<String>, Option<String>) = crate::test_support::runtime()
            .block_on(resolve.call_async(root.path().join(path).to_str().unwrap()))
            .unwrap();
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
