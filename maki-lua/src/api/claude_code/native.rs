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
use std::str::from_utf8;
use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::claude_code::Error as WorkerError;
use maki_providers::claude_code::worker::{
    self, EventKind, WorkerHandshake as NativeHandshake, WorkerPolicy,
};
use mlua::{
    Error as LuaError, Lua, LuaSerdeExt, LuaString, Result as LuaResult, Table, UserData,
    UserDataMethods, UserDataRef, UserDataRefMut, Value as LuaValue,
};
use rustix::io::Errno;
use serde_json::Value;

use crate::api::fs::path_text;
use crate::api::util::convert::{TableExt, json_to_lua};
use crate::api::util::ctx::LuaCtx;
use crate::api::util::pair::{Pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const OWNER_EXECUTE: u32 = 0o100;
const HOME_STATE_PREFIX: &str = ".claude.json";
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

pub(super) fn create_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let table = create_claude_code_table(lua, perms)?;
    table.set("handshake_input", worker::handshake_input())?;
    table.set("unchecked_start", WorkerError::UncheckedStart.to_string())?;
    Ok(table)
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
        smol::unblock(move || {
            let project = worker::checkout_root(Path::new(&project)).map_err(IoError::other)?;
            worker::git_directory(&project.join(worker::GIT_ENTRY)).and_then(path_text)
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
        worker::private_dir(
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
        methods.add_method("probe_exit", |_, this, (code, secs): (Option<i32>, u64)| {
            Ok(pair(this.0.probe_exit(code, secs).map(|()| true)))
        });
        methods.add_method("resolved_model", |_, this, requested: String| {
            Ok(pair(this.0.resolved_model(&requested).map(str::to_owned)))
        });
        methods.add_method("subscription_type", |_, this, ()| {
            Ok(this.0.subscription_type().map(str::to_owned))
        });
    }
}

/// Collect and validate worker control responses with the provider's handshake rules.
/// @param modes table Allowed permission modes.
/// @param policy userdata Shared worker policy.
/// @return (userdata) Handshake with probe_exit, resolved_model and subscription_type methods.
#[lua_fn]
fn worker_handshake(
    _lua: &Lua,
    modes: Vec<String>,
    policy: UserDataRef<Policy>,
) -> LuaResult<WorkerHandshake> {
    Ok(WorkerHandshake(NativeHandshake::new(
        modes,
        Arc::clone(&policy.0),
    )))
}

/// Build worker arguments without serializing launch settings through Lua.
/// @param model string Model name.
/// @param tools table Claude Code tool names.
/// @param mode string Permission mode.
/// @param policy userdata Shared worker policy.
/// @param contract string Worker instructions.
/// @return (table) Command-line arguments.
#[lua_fn]
fn worker_args(
    _lua: &Lua,
    model: String,
    tools: Vec<String>,
    mode: String,
    policy: UserDataRef<Policy>,
    contract: String,
) -> LuaResult<Vec<String>> {
    Ok(policy.0.args(&model, &tools.join(","), &mode, &contract))
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
        smol::unblock(move || worker::prepare_launch(
            environ.into_iter().collect(),
            configured.map(PathBuf::from),
            Path::new(&cwd)
        ))
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

/// Validate the executable version with the shared process supervisor and cache.
/// @param ctx LuaCtx Calling handler context.
/// @param executable string Absolute CLI path.
/// @param env table Validated child environment.
/// @param cwd string Session directory.
/// @param denied table Protected paths.
/// @param startup_secs integer Startup timeout.
/// @return (string?, string?) Validated version, or nil and the error.
#[lua_fn(guard = [FsWrite, Run])]
async fn checked_profile(
    _lua: Lua,
    ctx: UserDataRef<LuaCtx>,
    executable: String,
    env: HashMap<String, String>,
    cwd: String,
    denied: Vec<String>,
    startup_secs: u64,
) -> LuaResult<Pair<String>> {
    let cancel = ctx.cancel.clone();
    drop(ctx);
    Ok(pair(
        worker::checked_profile(
            Path::new(&executable),
            &env.into_iter().collect::<Vec<_>>(),
            Path::new(&cwd),
            &denied.into_iter().map(PathBuf::from).collect::<Vec<_>>(),
            startup_secs,
            cancel.cancelled(),
        )
        .await,
    ))
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
    let result = smol::unblock(move || {
        snapshot::sanitize_snapshot(
            Path::new(&snapshot),
            Path::new(&quarantine),
            Path::new(&git),
            prune_links,
        )
    })
    .await;
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
        smol::unblock(move || {
            blobs::unpack(Path::new(&batch), Path::new(&stage), &hashes).map(|()| true)
        })
        .await,
    ))
}

/// Check whether a path lies in the checkout or its protected directories.
/// @param path string Absolute candidate path.
/// @param cwd string Session directory.
/// @param denied table Protected checkout directories.
/// @return (boolean) Whether the candidate is inside the checkout.
#[lua_fn]
fn in_checkout(_lua: &Lua, path: String, cwd: String, denied: Vec<String>) -> LuaResult<bool> {
    Ok(worker::inside_checkout(
        Path::new(&path),
        Path::new(&cwd),
        &denied.into_iter().map(PathBuf::from).collect::<Vec<_>>(),
    ))
}

/// Inspect import targets in one filesystem operation. Missing entries are false.
/// @param paths table Checkout file paths.
/// @return (table?, string?) Regular and owner-executable flags, or nil and an I/O error.
#[lua_fn(guard = FsRead)]
async fn checkout_files(lua: Lua, paths: Vec<String>) -> LuaResult<Pair<Table>> {
    let values = try_pair!(
        smol::unblock(move || paths
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
        methods.add_method_mut("set_model", |_, this, model: String| {
            this.stream.set_model(model);
            Ok(())
        });
        methods.add_method("accepted", |_, this, ()| Ok(this.stream.accepted()));
        methods.add_method_mut(
            "feed",
            |lua, this, (line, handshake): (String, Option<UserDataRefMut<WorkerHandshake>>)| {
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
/// @param expect table Expected cwd, cli version and worker profile.
/// @return (userdata) Stateful event classifier.
#[lua_fn]
fn worker_stream(_lua: &Lua, expect: Table) -> LuaResult<WorkerStream> {
    let cli: Table = expect.get("cli")?;
    let worker: Table = expect.get("worker")?;
    Ok(WorkerStream {
        executable: expect.opt::<String>("executable")?.map(PathBuf::from),
        stream: worker::WorkerStream::new(
            cli.get("version")?,
            PathBuf::from(expect.get::<String>("cwd")?),
            worker.get("tools")?,
            worker.get("reported_modes")?,
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
        Some(EventKind::Result | EventKind::RateLimit | EventKind::Init) => {}
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
    if let Some(usage) = &classified.usage {
        result.set("usage", json_to_lua(lua, usage)?)?;
    }
    if let Some(rate_limit) = &classified.rate_limit {
        result.set("rate_limit", lua.to_value(rate_limit)?)?;
    }
    Ok((Some(result), None))
}

/// Find the configured worker executable.
/// @param name string Executable name or path.
#[lua_fn(guard = FsRead)]
fn executable(_lua: &Lua, name: String) -> LuaResult<Pair<String>> {
    let cwd = try_pair!(env::current_dir());
    Ok(pair(
        worker::executable(&name, &cwd).map(|path| path.display().to_string()),
    ))
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
    Ok(worker::worker_exit(code, after_result))
}

lua_table! {
    /// Private helpers for the bundled Claude Code plugin. JSON inputs retain
    /// the distinction between null, objects and arrays.
    "maki.claude_code.internal" => pub(crate) fn create_claude_code_table(perms: &PluginPermissions), DOCS [
        executable(perms), startup_late, worker_exit, shell_secret_problem, escape_path, worker_policy, worker_handshake, worker_args, user_message, prepare_launch(perms), private_dir(perms), home_state_files(perms),
        worker_stream, in_checkout, checkout_files(perms), checked_profile(perms), sanitize_git(perms), project_git_dir(perms), unpack_blobs(perms),
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
        create.call(json_to_lua(lua, &json!({ "cwd": "/workspace", "cli": { "version": "2.1.284" }, "worker": { "tools": [], "reported_modes": [] } })).unwrap()).unwrap()
    }

    #[test_case("checked_profile", Permission::Run ; "version_check_requires_run")]
    #[test_case("checked_profile", Permission::FsWrite ; "version_check_requires_temporary_files")]
    #[test_case("executable", Permission::FsRead ; "executable_lookup_requires_read")]
    fn native_launch_helpers_enforce_permissions(name: &str, permission: Permission) {
        let lua = Lua::new();
        let mut permissions = PluginPermissions::trusted();
        permissions.set(permission, false);
        let table = create_claude_code_table(&lua, &permissions).unwrap();
        let function: Function = table.get(name).unwrap();
        let error = smol::block_on(function.call_async::<LuaValue>(())).unwrap_err();
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
        let (result, error): (Option<Table>, Option<String>) =
            smol::block_on(sanitize.call_async((
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
        let (meta, error): (Option<Table>, Option<String>) =
            smol::block_on(inspect.call_async(vec![parent.join(LINKED).to_str().unwrap()]))
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
        let (meta, error): (Option<Table>, Option<String>) =
            smol::block_on(inspect.call_async(vec![path.to_str().unwrap()])).unwrap();
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
