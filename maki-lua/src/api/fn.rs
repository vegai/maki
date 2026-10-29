use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flume::{Receiver, Sender};
use maki_lua_macro::{lua_fn, lua_table, lua_value};
use maki_providers::process;
use maki_storage::id::MakiId;
use mlua::{
    Function, IntoLuaMulti, Lua, LuaString, RegistryKey, Result as LuaResult, Table, Value,
};

use crate::api::fs::expand_tilde;
use crate::api::util::command::{UiAction, ui_roundtrip, ui_send};
use crate::api::util::convert::TableExt;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::plugin_permissions::{Permission, PluginPermissions, denied_error};
use crate::runtime::{active_task_id, job_task_id, strip_traceback, with_jobs};

mod jobs;
pub use jobs::set_shell_preference;
use jobs::{
    CallbackUpdate, CallbackUpdates, JobCommand, JobSnapshot, MAX_STDIN_BYTES, Redirect,
    STDIN_FULL_ERR, STDIN_OVERSIZED_ERR,
};
pub(crate) use jobs::{JobEvent, JobOwner, JobSpec, JobStore};

const MAX_TAIL_LINES: usize = 1024;
const DEFAULT_WAIT_MS: u64 = 30_000;

const NO_TASK_SCOPE_ERR: &str =
    "jobstart: no active task; use scope = \"plugin\" or { session = ... }";
const TABLE_SCOPE_ERR: &str = "jobstart: table scope must be { session = <id> }";
const SCOPE_TYPE_ERR: &str = "jobstart: scope must be \"task\", \"plugin\", or { session = <id> }";
const JOB_NOT_FOUND_ERR: &str = "job: not found";
const JOB_UNWAITABLE_ERR: &str = "jobwait: unknown job id or already waited";
const JOB_WAIT_TIMEOUT_ERR: &str = "jobwait: timed out";
const BLANK_NAME_ERR: &str = "jobstart: name must be non-blank";
const EMPTY_ARGV_ERR: &str = "jobstart: argv table must not be empty";
const CMD_TYPE_ERR: &str = "jobstart: cmd must be a shell string or an argv table";
const STDIN_MODE_ERR: &str = "jobstart: stdin must be \"pipe\" or \"null\"";
const STDIN_PIPE: &str = "pipe";
const STDIN_NULL: &str = "null";
const CHANCLOSE_STREAM_ERR: &str = "chanclose: you can close only \"stdin\"";
const CHANSEND_DATA_ERR: &str = "chansend: data must be a string or a list";
const ZERO_LINE_LIMIT_ERR: &str = "jobstart: max_line_bytes must be positive";
const STDIN_STREAM: &str = "stdin";

struct CheckedOutReceiver {
    lua: Lua,
    job_id: u32,
    receiver: Option<Receiver<JobEvent>>,
}

impl CheckedOutReceiver {
    fn new(lua: &Lua, job_id: u32, receiver: Receiver<JobEvent>) -> Self {
        Self {
            lua: lua.clone(),
            job_id,
            receiver: Some(receiver),
        }
    }

    fn get(&self) -> &Receiver<JobEvent> {
        self.receiver.as_ref().expect("receiver is checked out")
    }
}

impl Drop for CheckedOutReceiver {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            with_jobs(&self.lua, |store| {
                store.restore_receiver(self.job_id, receiver);
            });
        }
    }
}

/// Run a command in the background. A string runs through `bash -c` on Unix
/// and through [`agent.shell`](/docs/configuration/#agent) on Windows. A table
/// is spawned as argv with no shell, so nothing in it is read as a redirect, a
/// pipe, or `$(...)`.
/// You get back a job id that you can pass to `jobstop` or `jobwait` to
/// control the process.
///
/// `stdout` and `stderr` can append directly to files. Redirected streams
/// have no callbacks, tails or events and count as truncated in job reports.
/// A redirect conflicts with the corresponding output callback and needs
/// `fs_write`. Use separate writer and reader jobs to store and process output.
///
/// Yields while starting the process. Call from a coroutine. Cancel hooks and
/// describe callbacks cannot yield.
///
/// @param cmd string|table Shell command, or an argv table like
///   `{ "tail", "-F", path }`.
/// @param opts table? Optional settings:
///   `cwd` (string?) working directory (tilde is expanded).
///   `env` (table?) environment variables, `{ VAR = "value" }`, added to
///     maki's environment.
///   `clear_env` (boolean?) use only `env` for the child
///     environment (default false). Without an explicit `PATH`, the program
///     (or shell for a string command) is resolved through maki's `PATH`.
///   `stdin` (string?) `"pipe"` to write to the job with `chansend`. Defaults
///     to `"null"`, no input. Neovim defaults to `"pipe"`, but a job that
///     reads an open pipe with no data hangs.
///   `guard` (boolean?) on Linux, stop remaining group members as soon as the leader exits,
///     including descendants that retain output pipes, or when maki dies, even from SIGKILL.
///   `timeout_ms` (integer?) stop the process group after this many milliseconds.
///     The deadline runs independently of Lua callbacks.
///   `on_stdout` (function?) called with `(job_id, line, "stdout")` for each
///     stdout line. Bytes that are not UTF-8 become U+FFFD, where Neovim
///     passes the raw bytes.
///   `on_stderr` (function?) the same for each stderr line, with `"stderr"`.
///   `on_exit` (function?) called with `(job_id, code, "exit", timed_out)` when the
///     process stops.
///   `stdout` (string|false?) append stdout to this path, or `false` to
///     discard it.
///   `stderr` (string|false?) same for stderr; both may name one path.
///   `scope` (string|table?) job lifetime. `"task"` (default) ends the job
///     with the current call. `"plugin"` keeps it alive until the plugin
///     unloads or reloads. `{ session = "<id>" }` keeps it alive until that
///     session ends, and survives plugin reload.
///   `max_line_bytes` (integer?) maximum bytes per output line, excluding the newline.
///     An oversized line stops the job and reports the limit on stderr.
///   `tail` (integer?) trailing lines per stream kept for `jobinfo`
///     (default 20, 0 disables, max 1024).
///   `name` (string?) handle for `jobfind`, unique among the live jobs this
///     plugin can see. A second job with the same live name fails.
/// @return (integer?, string?) Job id, or nil plus an error message when the
///   process could not start (binary not found, bad `cwd`, redirect file not
///   writable or a live name is already held).
/// @example
/// local id, err = maki.fn.jobstart({ "rg", "--json", pattern, dir }, {
///   on_stdout = function(_, line) print(line) end,
///   on_exit = function(_, code) print("exit: " .. code) end,
/// })
/// if not id then
///   maki.log.warn("rg failed to start: " .. err)
/// end
#[lua_fn(guard = Run)]
async fn jobstart(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] fs_write: bool,
    cmd: Value,
    opts: Option<Table>,
) -> LuaResult<Pair<u32>> {
    let scope = opts
        .as_ref()
        .map(|opts| opts.opt::<Value>("scope"))
        .transpose()?
        .flatten()
        .unwrap_or(Value::Nil);
    let mut spec = JobSpec::new(parse_scope(&lua, &plugin, scope)?, parse_command(cmd)?);
    let mut tail = None;

    if let Some(ref opts) = opts {
        spec.cwd = opts.opt("cwd")?;
        spec.env = opts.opt::<HashMap<String, String>>("env")?;
        spec.clear_env = opts.opt::<bool>("clear_env")?.unwrap_or(false);
        spec.guard = opts.opt::<bool>("guard")?.unwrap_or(false);
        spec.timeout = opts.opt::<u64>("timeout_ms")?.map(Duration::from_millis);
        spec.max_line_bytes = opts.opt("max_line_bytes")?;
        if spec.max_line_bytes == Some(0) {
            return Err(mlua::Error::runtime(ZERO_LINE_LIMIT_ERR));
        }
        spec.pipe_stdin = match opts.opt::<String>("stdin")?.as_deref() {
            None | Some(STDIN_NULL) => false,
            Some(STDIN_PIPE) => true,
            Some(_) => return Err(mlua::Error::runtime(STDIN_MODE_ERR)),
        };
        spec.name = job_name(opts)?;
        spec.on_stdout = callback_key(&lua, opts, "on_stdout")?;
        spec.on_stderr = callback_key(&lua, opts, "on_stderr")?;
        spec.on_exit = callback_key(&lua, opts, "on_exit")?;
        spec.stdout = parse_redirect(opts, "stdout", spec.on_stdout.is_some(), fs_write)?;
        spec.stderr = parse_redirect(opts, "stderr", spec.on_stderr.is_some(), fs_write)?;
        tail = opts.opt("tail")?;
        if let Some(n) = tail
            && n > MAX_TAIL_LINES
        {
            return Err(mlua::Error::runtime(format!(
                "jobstart: tail must be in 0..={MAX_TAIL_LINES}"
            )));
        }
    }

    let task_id = active_task_id(&lua);
    let gate = with_jobs(&lua, |store| Arc::clone(&store.start_gate));
    let _starting = if spec.name.is_some() {
        Some(gate.lock().await)
    } else {
        None
    };
    let available = with_jobs(&lua, |store| {
        if let Some(ref name) = spec.name
            && let Some(held) = store.find_named(name, task_id, &plugin)
        {
            return Err(format!(
                "jobstart: name {name:?} is already held by live job {held}"
            ));
        }
        Ok(())
    });
    try_pair!(available);
    let spawned = try_pair!(smol::unblock(move || spec.spawn()).await);
    with_jobs(&lua, |store| {
        let id = try_pair!(store.register(spawned));
        store.set_tail(id, tail);
        Ok((Some(id), None))
    })
}

fn parse_command(cmd: Value) -> LuaResult<JobCommand> {
    match cmd {
        Value::String(cmd) => Ok(JobCommand::Shell(cmd.to_str()?.to_string())),
        Value::Table(argv) => {
            let argv: Vec<String> = argv.sequence_values().collect::<LuaResult<_>>()?;
            if argv.is_empty() {
                return Err(mlua::Error::runtime(EMPTY_ARGV_ERR));
            }
            Ok(JobCommand::Argv(argv))
        }
        _ => Err(mlua::Error::runtime(CMD_TYPE_ERR)),
    }
}

fn parse_redirect(
    opts: &Table,
    key: &str,
    has_callback: bool,
    fs_write: bool,
) -> LuaResult<Redirect> {
    let Some(value) = opts.opt::<Value>(key)? else {
        return Ok(Redirect::Capture);
    };
    if has_callback {
        return Err(mlua::Error::runtime(format!(
            "jobstart: {key} and on_{key} are mutually exclusive; tail the file from a second job to get both"
        )));
    }
    match value {
        Value::Boolean(false) => Ok(Redirect::Discard),
        Value::String(path) => {
            if !fs_write {
                return Err(denied_error(Permission::FsWrite));
            }
            Ok(Redirect::File(expand_tilde(&path.to_str()?)))
        }
        _ => Err(mlua::Error::runtime(format!(
            "jobstart: {key} must be a path string or false"
        ))),
    }
}

fn callback_key(lua: &Lua, opts: &Table, key: &str) -> LuaResult<Option<RegistryKey>> {
    opts.opt::<Function>(key)?
        .map(|f| lua.create_registry_value(f))
        .transpose()
}

fn job_name(opts: &Table) -> LuaResult<Option<String>> {
    let name = opts.opt::<String>("name")?;
    if name.as_deref().is_some_and(|name| name.trim().is_empty()) {
        return Err(mlua::Error::runtime(BLANK_NAME_ERR));
    }
    Ok(name)
}

/// Find the live job of this plugin that `jobstart` registered under {name}.
/// An exited job never answers. The name stays on its `joblist` row so you can inspect
/// why it died. Another coroutine may claim the name while `jobstart` yields.
/// Check for a start error, or look up the winner again.
///
/// @param name string Name passed to `jobstart`.
/// @return (integer|nil, string|nil) Job id, or nil and an error when no live
///   job holds the name.
/// @example
/// local id = maki.fn.jobfind("log-tail")
/// if not id then
///   local err
///   id, err = maki.fn.jobstart("tail -F /tmp/log", { name = "log-tail", scope = "plugin" })
///   if not id then id = maki.fn.jobfind("log-tail") end
///   if not id then return nil, err end
/// end
#[lua_fn(guard = Run)]
fn jobfind(lua: &Lua, #[ctx] plugin: Arc<str>, name: String) -> LuaResult<Pair<u32>> {
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.find_named(&name, task_id, &plugin)) {
        Some(id) => Ok((Some(id), None)),
        None => Ok(err_pair(JOB_NOT_FOUND_ERR)),
    }
}

fn parse_scope(lua: &Lua, plugin: &Arc<str>, scope: Value) -> LuaResult<JobOwner> {
    let task_scope = || {
        job_task_id(lua)
            .map(JobOwner::Task)
            .ok_or_else(|| mlua::Error::runtime(NO_TASK_SCOPE_ERR))
    };
    match scope {
        Value::Nil => task_scope(),
        Value::String(name) => match &*name.to_str()? {
            "task" => task_scope(),
            "plugin" => Ok(JobOwner::Plugin(Arc::clone(plugin))),
            other => Err(mlua::Error::runtime(format!(
                "jobstart: unknown scope {other:?}; expected \"task\", \"plugin\", or {{ session = ... }}"
            ))),
        },
        Value::Table(scope) => {
            let Value::String(raw) = scope.get::<Value>("session")? else {
                return Err(mlua::Error::runtime(TABLE_SCOPE_ERR));
            };
            let session: MakiId =
                raw.to_str()?
                    .parse()
                    .map_err(|e: maki_storage::id::MakiIdParseError| {
                        mlua::Error::runtime(e.to_string())
                    })?;
            Ok(JobOwner::Session {
                session,
                plugin: Arc::clone(plugin),
            })
        }
        _ => Err(mlua::Error::runtime(SCOPE_TYPE_ERR)),
    }
}

/// Snapshot a job this plugin can see. Live jobs report tails collected
/// so far; session-owned jobs still answer after they exit.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return (table|nil, string|nil) `{ id, command, name, pid, session, status,
///   exit_code, timed_out, elapsed_secs, stdout_lines, stderr_lines }`, or nil and
///   an error. `status` is `"running"` or `"exited"`.
/// @example
/// local info = maki.fn.jobinfo(id)
#[lua_fn(guard = Run)]
fn jobinfo(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<Pair<Value>> {
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.snapshot(job_id, task_id, &plugin)) {
        Some(snap) => Ok((Some(Value::Table(snapshot_table(lua, &snap, true)?)), None)),
        None => Ok(err_pair(JOB_NOT_FOUND_ERR)),
    }
}

/// Attach (or replace) callbacks on a job this plugin can see. This is how a
/// plugin picks its jobs back up after a reload: unloading drops the Lua
/// callbacks of its session-owned jobs, but the processes keep running.
///
/// Keys absent from {opts} leave the current callback alone. Attaching
/// `on_exit` to a job that already exited still fires it once, with the
/// recorded exit code, so a reload racing the exit cannot lose it.
///
/// @param job_id integer Job id, e.g. from `joblist`.
/// @param opts table `on_stdout`, `on_stderr`, `on_exit`: a function, or `false` to clear.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// -- A monitor that survives /reload: adopt the live job or start one.
/// local sid = maki.session.current()
/// local id = maki.fn.jobfind("log-tail")
///   or maki.fn.jobstart({ "tail", "-F", path }, {
///     name = "log-tail",
///     scope = { session = sid },
///   })
/// maki.fn.jobattach(id, {
///   on_stdout = function(_, line) maki.session.notify(line, { session = sid }) end,
///   on_exit = function(_, code) maki.session.notify("tail died: " .. code, { session = sid }) end,
/// })
#[lua_fn(guard = Run)]
fn jobattach(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    job_id: u32,
    opts: Table,
) -> LuaResult<Pair<bool>> {
    let updates = CallbackUpdates {
        on_stdout: callback_update(lua, &opts, "on_stdout")?,
        on_stderr: callback_update(lua, &opts, "on_stderr")?,
        on_exit: callback_update(lua, &opts, "on_exit")?,
    };
    let task_id = active_task_id(lua);
    let attached = with_jobs(lua, |store| {
        store.attach(lua, job_id, task_id, &plugin, updates)
    });
    if attached {
        Ok((Some(true), None))
    } else {
        Ok(err_pair(JOB_NOT_FOUND_ERR))
    }
}

fn callback_update(lua: &Lua, opts: &Table, key: &str) -> LuaResult<CallbackUpdate> {
    match opts.get::<Value>(key)? {
        Value::Nil => Ok(CallbackUpdate::Keep),
        Value::Boolean(false) => Ok(CallbackUpdate::Clear),
        Value::Function(callback) => Ok(CallbackUpdate::Set(lua.create_registry_value(callback)?)),
        _ => Err(mlua::Error::runtime(format!(
            "jobattach: {key} must be a function or false"
        ))),
    }
}

/// List jobs this plugin can see, including exited session-owned jobs (so an
/// id started before a reload stays findable). Rows identify the job; call
/// `jobinfo` for tails. Pass a session id to list only that session's jobs.
/// Plugin and task jobs carry no session, so a filter never matches them.
///
/// @param session string? Session id filter.
/// @return (table) array of `{ id, command, name, pid, session, status,
///   exit_code, timed_out, elapsed_secs }`.
/// @example
/// local jobs = maki.fn.joblist(maki.session.current())
#[lua_fn(guard = Run)]
fn joblist(lua: &Lua, #[ctx] plugin: Arc<str>, session: Option<String>) -> LuaResult<Pair<Value>> {
    let filter = match session {
        Some(raw) => match raw.parse::<MakiId>() {
            Ok(id) => Some(id),
            Err(e) => return Ok(err_pair(e.to_string())),
        },
        None => None,
    };
    let task_id = active_task_id(lua);
    let snaps = with_jobs(lua, |store| store.list(filter, task_id, &plugin));
    let result = lua.create_table()?;
    for (i, snap) in snaps.iter().enumerate() {
        result.set(i + 1, snapshot_table(lua, snap, false)?)?;
    }
    Ok((Some(Value::Table(result)), None))
}

fn snapshot_table(lua: &Lua, snap: &JobSnapshot, tails: bool) -> LuaResult<Table> {
    let row = lua.create_table()?;
    row.set("id", snap.id)?;
    row.set("command", snap.command.as_str())?;
    row.set("name", snap.name.as_deref())?;
    row.set("pid", snap.pid)?;
    row.set("session", snap.session.map(|s| s.to_string()))?;
    row.set("elapsed_secs", snap.elapsed_secs)?;
    row.set(
        "status",
        if snap.exit_code.is_some() {
            "exited"
        } else {
            "running"
        },
    )?;
    row.set("exit_code", snap.exit_code)?;
    row.set("timed_out", snap.timed_out)?;
    if tails {
        let stdout = lua.create_table()?;
        for (i, line) in snap.stdout_lines.iter().enumerate() {
            stdout.set(i + 1, line.as_str())?;
        }
        row.set("stdout_lines", stdout)?;
        let stderr = lua.create_table()?;
        for (i, line) in snap.stderr_lines.iter().enumerate() {
            stderr.set(i + 1, line.as_str())?;
        }
        row.set("stderr_lines", stderr)?;
    }
    Ok(row)
}

/// Kill a running job immediately (SIGKILL on Unix). Safe to call on
/// jobs that already exited or on unknown ids.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return
/// @example
/// maki.fn.jobstop(id)
#[lua_fn(guard = Run)]
fn jobstop(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<()> {
    let task_id = active_task_id(lua);
    with_jobs(lua, |store| store.kill(job_id, task_id, &plugin));
    Ok(())
}

/// Write {data} to job {id}'s stdin. Start the job with `stdin = "pipe"`.
/// Unlike `vim.fn.chansend`, an error returns nil and a message instead of 0.
/// List items use newline separators. A newline inside an item becomes NUL.
/// The final item has no newline suffix.
///
/// Unread data stays in memory. A descendant can retain stdin after the job
/// exits. Use `guard` to stop that descendant and release the data.
///
/// `chansend_limits.max_bytes` bounds the queue, including a write in progress.
/// `chansend_limits.full_error` means wait for the child to consume input and retry.
/// `chansend_limits.oversized_error` means split the write into smaller chunks.
///
/// @param id integer Job id returned by `jobstart`.
/// @param data string|table Text to write, or a list of lines.
/// @return (integer?, string?) Bytes accepted from this call, or nil and an error.
/// @example
/// local id = maki.fn.jobstart({ "cat" }, { stdin = "pipe" })
/// maki.fn.chansend(id, "hello\n")
/// maki.fn.chanclose(id, "stdin")
#[lua_fn(guard = Run)]
fn chansend(lua: &Lua, #[ctx] plugin: Arc<str>, id: u32, data: Value) -> LuaResult<Pair<usize>> {
    let bytes = chansend_bytes(data)?;
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.send(id, task_id, &plugin, bytes)) {
        Ok(len) => Ok((Some(len), None)),
        Err(e) => Ok(err_pair(e)),
    }
}

/// List items can contain arbitrary bytes, as in Neovim. A newline inside an item becomes NUL.
fn chansend_bytes(data: Value) -> LuaResult<Vec<u8>> {
    match data {
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Table(lines) => {
            let mut bytes = Vec::new();
            for (index, line) in lines.sequence_values::<LuaString>().enumerate() {
                if index > 0 {
                    bytes.push(b'\n');
                }
                bytes.extend(line?.as_bytes().iter().map(
                    |&byte| {
                        if byte == b'\n' { b'\0' } else { byte }
                    },
                ));
            }
            Ok(bytes)
        }
        _ => Err(mlua::Error::runtime(CHANSEND_DATA_ERR)),
    }
}

/// Close job {id}'s stdin, so the job reads end of file. Like
/// `vim.fn.chanclose`, but only for the `"stdin"` stream: without a stream,
/// Neovim closes every stream, while this closes stdin. The {stream}
/// argument is kept, so a call written for Neovim works unchanged.
///
/// @param id integer Job id returned by `jobstart`.
/// @param stream string? `"stdin"`, which is also the default.
/// @return (integer?, string?) `1`, as Neovim returns, or nil and an error.
/// @example
/// maki.fn.chanclose(id, "stdin")
#[lua_fn(guard = Run)]
fn chanclose(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    id: u32,
    stream: Option<String>,
) -> LuaResult<Pair<i32>> {
    if stream.is_some_and(|s| s != STDIN_STREAM) {
        return Err(mlua::Error::runtime(CHANCLOSE_STREAM_ERR));
    }
    let task_id = active_task_id(lua);
    match with_jobs(lua, |store| store.close_stdin(id, task_id, &plugin)) {
        Ok(()) => Ok((Some(1), None)),
        Err(e) => Ok(err_pair(e)),
    }
}

/// Drop an exited session-owned job from the store. Running jobs are left
/// alone; use `jobstop` to kill those. Unknown ids are a no-op.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return
/// @example
/// maki.fn.jobforget(id)
#[lua_fn(guard = Run)]
fn jobforget(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<()> {
    let task_id = active_task_id(lua);
    with_jobs(lua, |store| store.forget(lua, job_id, task_id, &plugin));
    Ok(())
}

/// Wait for a job to finish and collect its output. Returns a result
/// table with `stdout`, `stderr`, `exit_code`, `timed_out`, and `truncated`. A job that
/// already exited answers from its captured tail, so `truncated` says
/// whether that tail ever lost a line (`tail` too small or 0, or the stream
/// redirected away). Waiting on a live job collects every line and is never
/// truncated.
/// `timed_out` is true when the job's own `timeout_ms` deadline stopped it.
///
/// While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
/// callbacks fire as events arrive (like Neovim), so you can stream
/// output into a buffer while parked here. An already-exited
/// session-owned job answers from its snapshot and fires no callbacks.
/// Task and plugin jobs leave the store on exit, so waiting after that
/// answers nil plus an error.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @param timeout_ms integer? Maximum wait in milliseconds (default 30000).
/// @return (table?, string?) `{ stdout, stderr, exit_code, timed_out, truncated }`, or
///   nil plus an error on timeout or an unknown job.
/// @example
/// local id = maki.fn.jobstart("echo hello")
/// local result, err = maki.fn.jobwait(id, 5000)
/// if result then
///   print(result.stdout)
/// end
#[lua_fn(guard = Run)]
async fn jobwait(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    job_id: u32,
    timeout_ms: Option<u64>,
) -> LuaResult<Pair<Table>> {
    let task_id = active_task_id(&lua);
    if let Some(snap) = with_jobs(&lua, |store| store.snapshot(job_id, task_id, &plugin))
        && let Some(code) = snap.exit_code
    {
        return wait_result(
            &lua,
            &snap.stdout_lines,
            &snap.stderr_lines,
            code,
            snap.timed_out,
            snap.dropped_output,
        );
    }
    let Some(receiver) = with_jobs(&lua, |store| store.take_receiver(job_id, task_id, &plugin))
    else {
        return Ok(err_pair(JOB_UNWAITABLE_ERR));
    };
    let receiver = CheckedOutReceiver::new(&lua, job_id, receiver);

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
    let deadline = smol::Timer::after(timeout);
    futures_lite::pin!(deadline);

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();

    let (exit_code, timed_out) = loop {
        let event =
            futures_lite::future::or(async { receiver.get().recv_async().await.ok() }, async {
                (&mut deadline).await;
                None
            })
            .await;

        let Some(event) = event else {
            return Ok(err_pair(JOB_WAIT_TIMEOUT_ERR));
        };
        // A failing callback must not abort the wait: the event is already
        // recorded and the exit still needs collecting. Same policy as the
        // detached dispatch pump.
        if let Err(e) = deliver_job_event(&lua, job_id, &event).await {
            tracing::warn!(job_id, error = %strip_traceback(&e), "jobwait callback failed");
        }
        match event {
            JobEvent::Stdout(line) => stdout_lines.push(line),
            JobEvent::Stderr(line) => stderr_lines.push(line),
            JobEvent::Exit(code, timed_out) => break (code, timed_out),
        }
    };

    wait_result(
        &lua,
        &stdout_lines,
        &stderr_lines,
        exit_code,
        timed_out,
        false,
    )
}

/// The single shape `jobwait` answers with, so its two paths cannot drift
/// apart from the documented keys.
fn wait_result(
    lua: &Lua,
    stdout: &[String],
    stderr: &[String],
    exit_code: i32,
    timed_out: bool,
    truncated: bool,
) -> LuaResult<Pair<Table>> {
    let result = lua.create_table()?;
    result.set("stdout", stdout.join("\n"))?;
    result.set("stderr", stderr.join("\n"))?;
    result.set("exit_code", exit_code)?;
    result.set("timed_out", timed_out)?;
    result.set("truncated", truncated)?;
    Ok((Some(result), None))
}

/// Fire the job's Lua callback for {event} (if any) and mark the job
/// dead on exit. Shared by `jobwait` and the async dispatch loop so
/// both deliver events identically.
///
/// The callback runs in a fresh coroutine so it may suspend (the
/// `maki.fs.*` helpers park on `smol::unblock`); resumed inline from a
/// poll loop it would die with "attempt to yield across metamethod /
/// C-call boundary" on its first suspension.
pub(crate) async fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let callback = with_jobs(lua, |store| {
        store.record_event(job_id, event);
        store
            .callback_key(job_id, event)
            .and_then(|key| lua.registry_value::<Function>(key).ok())
    });
    if let JobEvent::Exit(code, _) = event {
        with_jobs(lua, |store| store.complete(lua, job_id, *code));
    }
    match callback {
        Some(callback) => call_back(lua, callback, job_id, event).await,
        None => Ok(()),
    }
}

async fn call_back(lua: &Lua, callback: Function, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let arg = match event {
        JobEvent::Stdout(line) | JobEvent::Stderr(line) => Value::String(lua.create_string(line)?),
        JobEvent::Exit(code, _) => Value::Integer((*code).into()),
    };
    let mut args = (job_id, arg, event.name()).into_lua_multi(lua)?;
    if let JobEvent::Exit(_, timed_out) = event {
        args.push_back(Value::Boolean(*timed_out));
    }
    lua.create_thread(callback)?.into_async::<()>(args)?.await
}

/// Check whether {name} names a runnable file, resolving relative paths
/// and `$PATH` from maki's directory. Returns 1 when found, 0 otherwise (matches Neovim's
/// `vim.fn.executable`).
///
/// @param name string Program name (e.g. `"git"`) or absolute path.
/// @return (integer) `1` if found, `0` otherwise.
/// @example
/// if maki.fn.executable("rg") == 1 then
///   -- use ripgrep
/// end
// A file probe over `$PATH`: it answers whether a file exists, never what the
// environment holds, so `fs_read` covers it.
#[lua_fn(guard = FsRead)]
fn executable(_lua: &Lua, name: String) -> LuaResult<i32> {
    Ok(i32::from(find_program_here(&name).is_some()))
}

/// Resolve {name} to an absolute executable path, or return `""` if no
/// executable exists, as in Neovim's `vim.fn.exepath`.
/// Names with a path separator resolve from the working directory. Other
/// names resolve through `$PATH`, with files this process cannot execute
/// excluded. Resolve the program before a directory change so a relative
/// name cannot select a different executable.
///
/// @param name string The name of a program (for example `"git"`), or a relative or absolute path.
/// @return (string) Absolute path, or `""` if not found.
/// @example
/// local git = maki.fn.exepath("git")
/// if git ~= "" then
///   maki.fn.jobstart({ git, "status" }, { cwd = "/tmp" })
/// end
#[lua_fn(guard = FsRead)]
fn exepath(_lua: &Lua, name: String) -> LuaResult<String> {
    Ok(find_program_here(&name)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default())
}

fn find_program_here(name: &str) -> Option<PathBuf> {
    process::find_program(name, &env::current_dir().ok()?)
}

/// Read the viewport of the focused chat transcript, like Neovim's
/// `vim.fn.winsaveview()`. The transcript is the only scrollable window
/// maki has, so there is no window argument.
///
/// `topline` is the 1-based transcript line at the top of the viewport, so
/// the last visible one is `math.min(topline + height - 1, line_count)`.
/// `auto_scroll` has no Vim counterpart: it is true while the transcript
/// follows streaming output.
///
/// @return (table|nil, string|nil) `{topline, line_count, height, auto_scroll}`, or nil and an error.
/// @example
/// local view = maki.fn.winsaveview()
/// maki.fn.winrestview({ topline = view.topline + 1 })
#[lua_fn]
async fn winsaveview(lua: Lua, #[ctx] tx: Option<Sender<UiAction>>) -> LuaResult<Pair<Table>> {
    let view =
        try_pair!(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::WinSaveView { reply_tx }).await);
    let t = lua.create_table()?;
    t.set("topline", i64::from(view.scroll_top) + 1)?;
    t.set("line_count", view.line_count)?;
    t.set("height", view.height)?;
    t.set("auto_scroll", view.auto_scroll)?;
    Ok((Some(t), None))
}

/// Scroll the focused chat transcript so that the `topline` field of
/// {view} becomes the top visible line, like Neovim's
/// `vim.fn.winrestview()`. Out of range values are clamped. Other keys are
/// ignored, so a table straight from `winsaveview()` round-trips.
///
/// Scrolling away from the bottom unpins the transcript; landing back at
/// the bottom re-pins it so streaming output keeps following.
///
/// @param view table View to restore. Only `topline` (1-based) is read.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// maki.fn.winrestview({ topline = 1 })
#[lua_fn]
fn winrestview(
    _lua: &Lua,
    #[ctx] tx: Option<Sender<UiAction>>,
    view: Table,
) -> LuaResult<Pair<bool>> {
    let topline = view.opt::<i64>("topline")?.unwrap_or(1);
    let scroll_top = topline.saturating_sub(1).clamp(0, i64::from(u32::MAX)) as u32;
    try_pair!(ui_send(tx.as_ref(), UiAction::WinRestView { scroll_top }));
    Ok((Some(true), None))
}

/// Stdin queue limits: `max_bytes` includes a write in progress, `full_error`
/// means retry later, and `oversized_error` means split the write.
#[lua_value]
fn chansend_limits(lua: &Lua) -> LuaResult<Table> {
    let limits = lua.create_table()?;
    limits.set("max_bytes", MAX_STDIN_BYTES)?;
    limits.set("full_error", STDIN_FULL_ERR)?;
    limits.set("oversized_error", STDIN_OVERSIZED_ERR)?;
    Ok(limits)
}

lua_table! {
    /// Process and environment helpers, modeled after Neovim's `vim.fn` job
    /// control. Use these to run shell commands, wait for output, and check
    /// whether programs are installed.
    ///
    /// ```lua
    /// local id = maki.fn.jobstart("git status", {
    ///   on_exit = function(_, code) print("done: " .. code) end,
    /// })
    /// ```
    "maki.fn" => pub(crate) fn create_fn_table(
        plugin: Arc<str>,
        perms: &PluginPermissions,
        fs_write: bool,
        tx: Option<Sender<UiAction>>,
    ), DOCS [
        jobstart(perms, plugin, fs_write), jobstop(perms, plugin), jobforget(perms, plugin),
        jobwait(perms, plugin), jobinfo(perms, plugin), joblist(perms, plugin),
        jobattach(perms, plugin), jobfind(perms, plugin),
        chansend(perms, plugin), chansend_limits, chanclose(perms, plugin),
        executable(perms), exepath(perms),
        winsaveview(tx), winrestview(tx),
    ]
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{create_fn_table, with_jobs};
    #[cfg(unix)]
    use crate::plugin_permissions::PluginPermissions;
    use flume::Sender;
    #[cfg(unix)]
    use futures_lite::{FutureExt, future::poll_once};
    #[cfg(unix)]
    use mlua::Function;
    use mlua::{Lua, Table, Value};
    #[cfg(unix)]
    use std::pin::pin;
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::time::Duration;
    use test_case::test_case;

    use super::{
        JobEvent, call_back, chansend_bytes, winrestview__register, winsaveview__register,
    };
    use crate::api::util::command::{NO_UI_ERR, UiAction, WinView};

    const CALLBACK_LINE: &str = "output";
    const FIRST_LINE: &[u8] = b"first";
    const NOT_UTF8_LINE: &[u8] = b"latin\xe9";
    const LINE_WITH_NEWLINE: &[u8] = b"two\nparts";
    const LINE_WITH_NUL: &[u8] = b"two\0parts";

    #[cfg(unix)]
    #[test]
    fn only_named_starts_take_the_gate() {
        const PLUGIN: &str = "gate-test";
        const COMMAND: &str = "cat";
        const START_TIMEOUT: Duration = Duration::from_secs(5);
        smol::block_on(async {
            let lua = Lua::new();
            let table = create_fn_table(
                &lua,
                PLUGIN.into(),
                &PluginPermissions::trusted(),
                false,
                None,
            )
            .unwrap();
            let gate = with_jobs(&lua, |store| Arc::clone(&store.start_gate));
            let held = gate.lock().await;
            let named_options = lua
                .create_table_from([("scope", "plugin"), ("stdin", "pipe")])
                .unwrap();
            named_options.set("name", PLUGIN).unwrap();
            let start: Function = table.get("jobstart").unwrap();
            let mut starting =
                pin!(start.call_async::<(Option<u32>, Option<String>)>((COMMAND, named_options)));
            assert!(poll_once(starting.as_mut()).await.is_none());
            let unnamed_options = lua
                .create_table_from([("scope", "plugin"), ("stdin", "pipe")])
                .unwrap();
            let unnamed =
                start.call_async::<(Option<u32>, Option<String>)>((COMMAND, unnamed_options));
            let (id, error) = unnamed
                .or(async {
                    let result = starting.as_mut().await;
                    panic!("named start passed the held gate: {result:?}")
                })
                .or(async {
                    smol::Timer::after(START_TIMEOUT).await;
                    panic!("jobstart waited on an unrelated start")
                })
                .await
                .unwrap();
            assert!(id.is_some(), "{error:?}");
            assert!(poll_once(starting.as_mut()).await.is_none());
            assert!(with_jobs(&lua, |store| store.find_named(PLUGIN, None, PLUGIN)).is_none());
            drop(held);
            let (id, error) = starting.await.unwrap();
            assert!(id.is_some(), "{error:?}");
            assert_eq!(
                with_jobs(&lua, |store| store.find_named(PLUGIN, None, PLUGIN)),
                id
            );
        });
    }

    #[test_case(JobEvent::Stdout(CALLBACK_LINE.into()), 3 ; "stdout")]
    #[test_case(JobEvent::Stderr(CALLBACK_LINE.into()), 3 ; "stderr")]
    #[test_case(JobEvent::Exit(0, false), 4 ; "normal_exit")]
    #[test_case(JobEvent::Exit(1, true), 4 ; "timeout_exit")]
    fn job_callbacks_keep_their_argument_counts(event: JobEvent, count: usize) {
        let lua = Lua::new();
        let callback = lua
            .load("return function(...) args = table.pack(...) end")
            .eval()
            .unwrap();
        smol::block_on(call_back(&lua, callback, 1, &event)).unwrap();
        let args: Table = lua.globals().get("args").unwrap();
        assert_eq!(args.get::<usize>("n").unwrap(), count);
        assert_eq!(args.get::<String>(3).unwrap(), event.name());
        if let JobEvent::Exit(_, timed_out) = event {
            assert_eq!(args.get::<bool>(4).unwrap(), timed_out);
        }
    }

    /// List items can contain arbitrary bytes, as in Neovim. A newline inside an item becomes NUL.
    #[test]
    fn chansend_sends_a_list_as_bytes() {
        let lua = Lua::new();
        let lines = lua
            .create_sequence_from([
                lua.create_string(FIRST_LINE).unwrap(),
                lua.create_string(NOT_UTF8_LINE).unwrap(),
                lua.create_string(LINE_WITH_NEWLINE).unwrap(),
            ])
            .unwrap();
        let expected = [
            FIRST_LINE,
            b"\n".as_slice(),
            NOT_UTF8_LINE,
            b"\n".as_slice(),
            LINE_WITH_NUL,
        ]
        .concat();
        assert_eq!(chansend_bytes(Value::Table(lines)).unwrap(), expected);
    }

    fn lua_with_view(tx: Option<Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        winsaveview__register(&t, &lua, tx.clone()).unwrap();
        winrestview__register(&t, &lua, tx).unwrap();
        lua.globals().set("f", t).unwrap();
        lua
    }

    #[test_case("return f.winsaveview()" ; "winsaveview")]
    #[test_case("return f.winrestview({ topline = 3 })" ; "winrestview")]
    fn view_without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_view(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test]
    fn winsaveview_reports_the_viewport_one_based() {
        const SCROLL_TOP: u32 = 6;
        const LINE_COUNT: u32 = 100;
        const HEIGHT: u16 = 24;

        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::WinSaveView { reply_tx }) = rx.recv() else {
                panic!("expected winsaveview request");
            };
            reply_tx
                .send(WinView {
                    scroll_top: SCROLL_TOP,
                    line_count: LINE_COUNT,
                    height: HEIGHT,
                    auto_scroll: false,
                })
                .unwrap();
        });
        let (view, err): (Table, Option<String>) =
            smol::block_on(lua.load("return f.winsaveview()").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(view.get::<u32>("topline").unwrap(), SCROLL_TOP + 1);
        assert_eq!(view.get::<u32>("line_count").unwrap(), LINE_COUNT);
        assert_eq!(view.get::<u16>("height").unwrap(), HEIGHT);
        assert!(!view.get::<bool>("auto_scroll").unwrap());
    }

    #[test_case("{ topline = 12 }", 11 ; "explicit_topline")]
    #[test_case("{}", 0 ; "missing_topline_defaults_to_first_line")]
    #[test_case("{ topline = -5 }", 0 ; "below_range_clamps_to_first_line")]
    fn winrestview_forwards_zero_based_scroll_top(arg: &str, expected: u32) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        let (ok, err): (bool, Option<String>) = smol::block_on(
            lua.load(format!("return f.winrestview({arg})"))
                .eval_async(),
        )
        .unwrap();
        assert!(ok);
        assert_eq!(err, None);
        let Ok(UiAction::WinRestView { scroll_top }) = rx.recv() else {
            panic!("expected winrestview request");
        };
        assert_eq!(scroll_top, expected);
    }
}
