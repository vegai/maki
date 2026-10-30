use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::mem;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use maki_config::ShellPreference;
use maki_providers::process::GroupSignals;
use maki_providers::{process, shell_command, strip_provider_keys};
use maki_storage::id::MakiId;
use mlua::{Lua, RegistryKey};
use shell_words::join as shell_join;
use smol::lock::Mutex as AsyncMutex;

use crate::api::fs::expand_tilde;

use super::{JOB_NOT_FOUND_ERR, MAX_TAIL_LINES};

const DEFAULT_TAIL: usize = 20;
const MAX_COMPLETED_SESSION_JOBS: usize = 256;
#[cfg(not(target_os = "linux"))]
const GUARD_UNSUPPORTED: &str = "jobstart: guard requires Linux";
const READER_BUF_SIZE: usize = 8 * 1024;
const JOB_EXITED_ERR: &str = "job: exited or closed its stdin";
const STDIN_WRITE_ERR: &str = "job: writing stdin failed";
const NO_STDIN_ERR: &str = "job: stdin is not an open pipe";
pub(super) const STDIN_FULL_ERR: &str =
    "job: stdin queue is full; wait for the job to consume input";
pub(super) const STDIN_OVERSIZED_ERR: &str =
    "job: stdin write exceeds the queue capacity; send smaller chunks";
pub(super) const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;
const STDOUT_STREAM: &str = "stdout";
const STDERR_STREAM: &str = "stderr";
const EXIT_EVENT: &str = "exit";
const STDIN_EXITED: u8 = 1;
const STDIN_FAILED: u8 = 2;

const CWD_NOT_DIRECTORY: &str = "cwd is not a directory";

/// `agent.shell`, set once from config at startup. Jobs get built far away
/// from any config, on whatever Lua thread runs the plugin, so they read it here.
static SHELL: LazyLock<ArcSwap<ShellPreference>> = LazyLock::new(ArcSwap::default);

pub fn set_shell_preference(shell: ShellPreference) {
    SHELL.store(Arc::new(shell));
}

#[derive(Clone)]
pub(crate) enum JobEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32, bool),
}

impl JobEvent {
    /// The third argument Neovim passes a job callback.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Stdout(_) => STDOUT_STREAM,
            Self::Stderr(_) => STDERR_STREAM,
            Self::Exit(..) => EXIT_EVENT,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum JobOwner {
    Task(u64),
    Plugin(Arc<str>),
    /// Lives until the session ends (or the host process exits). Survives
    /// plugin reload: the starting plugin can still inspect and stop it.
    Session {
        session: MakiId,
        plugin: Arc<str>,
    },
}

/// A shell line, or an argv the plugin built itself so there are no quoting
/// rules to get wrong.
pub(crate) enum JobCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl From<&str> for JobCommand {
    fn from(cmd: &str) -> Self {
        Self::Shell(cmd.to_string())
    }
}

impl JobCommand {
    fn build(&self, lookup: Option<(&Path, Option<&OsStr>)>) -> Result<Command, String> {
        let command = match self {
            Self::Shell(text) => shell_command(text, &SHELL.load()),
            Self::Argv(argv) => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
        };
        match lookup {
            Some((dir, path)) => {
                let program = command.get_program().to_str().ok_or_else(|| {
                    format!(
                        "jobstart: program path is not valid UTF-8: {:?}",
                        command.get_program()
                    )
                })?;
                let path = process::find_program_with_path(program, dir, path)
                    .ok_or_else(|| format!("jobstart: program not found on PATH: {program}"))?;
                let mut resolved = Command::new(path);
                resolved.args(command.get_args());
                Ok(resolved)
            }
            None => Ok(command),
        }
    }

    /// Only for `jobinfo` / `joblist` rows: an argv job spawns from the vec,
    /// so nothing ever re-parses this.
    fn display(&self) -> String {
        match self {
            Self::Shell(cmd) => cmd.clone(),
            Self::Argv(argv) => shell_join(argv),
        }
    }
}

pub(crate) enum Redirect {
    /// Piped to a reader thread, so callbacks and tails see the lines.
    Capture,
    Discard,
    /// The child appends to the file on its own: no reader thread, no events,
    /// no tail.
    File(PathBuf),
}

impl Redirect {
    fn stdio(&self) -> Result<Stdio, String> {
        match self {
            Self::Capture => Ok(Stdio::piped()),
            Self::Discard => Ok(Stdio::null()),
            Self::File(path) => File::options()
                .create(true)
                .append(true)
                .open(path)
                .map(Stdio::from)
                .map_err(|e| format!("cannot open {}: {e}", path.display())),
        }
    }
}

pub(crate) struct JobSpec {
    pub owner: JobOwner,
    pub cmd: JobCommand,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    /// `env` becomes the whole environment, with nothing inherited from maki,
    /// so a variable the child must not see cannot leak in.
    pub clear_env: bool,
    /// Give the job a stdin pipe for `chansend` instead of /dev/null.
    pub pipe_stdin: bool,
    /// Kill remaining group members before the exit callback so descendants cannot outlive
    /// the job.
    pub guard: bool,
    pub timeout: Option<Duration>,
    pub max_line_bytes: Option<usize>,
    pub stdout: Redirect,
    pub stderr: Redirect,
    pub on_stdout: Option<RegistryKey>,
    pub on_stderr: Option<RegistryKey>,
    pub on_exit: Option<RegistryKey>,
}

impl JobSpec {
    pub(super) fn spawn(self) -> Result<SpawnedJob, String> {
        let command = self.command()?;
        self.spawn_command(command)
    }

    fn spawn_command(self, mut command: Command) -> Result<SpawnedJob, String> {
        #[cfg(target_os = "linux")]
        let lifetime = if self.guard {
            Some(process::guard::bind(&mut command).map_err(|error| error.to_string())?)
        } else {
            None
        };
        let child = Unwatched(Some(command.spawn().map_err(|error| error.to_string())?));
        Ok(SpawnedJob {
            child,
            #[cfg(target_os = "linux")]
            lifetime,
            spec: self,
        })
    }

    fn command(&self) -> Result<Command, String> {
        #[cfg(not(target_os = "linux"))]
        if self.guard {
            return Err(GUARD_UNSUPPORTED.into());
        }
        let cwd = self.cwd.as_deref().map(expand_tilde);
        if let Some(dir) = &cwd
            && !dir.is_dir()
        {
            return Err(format!("{CWD_NOT_DIRECTORY}: {}", dir.display()));
        }
        let lookup_dir = if self.clear_env
            && self.env.as_ref().is_none_or(|env| {
                !env.keys().any(|name| {
                    if cfg!(windows) {
                        name.eq_ignore_ascii_case("PATH")
                    } else {
                        name == "PATH"
                    }
                })
            }) {
            Some(match &cwd {
                Some(dir) => dir.clone(),
                None => env::current_dir().map_err(|error| error.to_string())?,
            })
        } else {
            None
        };
        let invoking_path = env::var_os("PATH");
        let mut command = self.cmd.build(
            lookup_dir
                .as_deref()
                .map(|dir| (dir, invoking_path.as_deref())),
        )?;
        // `clear_env` already keeps every key out.
        if !self.clear_env {
            strip_provider_keys(&mut command);
        }
        command
            .stdout(self.stdout.stdio()?)
            .stderr(self.stderr.stdio()?)
            .stdin(if self.pipe_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            });

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
            unsafe {
                command.pre_exec(|| {
                    rustix::process::setsid()?;
                    Ok(())
                });
            }
        }

        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        if self.clear_env {
            command.env_clear();
        }
        if let Some(ref env_map) = self.env {
            for (k, v) in env_map {
                command.env(k, v);
            }
        }

        Ok(command)
    }

    pub(crate) fn new(owner: JobOwner, cmd: impl Into<JobCommand>) -> Self {
        Self {
            owner,
            cmd: cmd.into(),
            name: None,
            cwd: None,
            env: None,
            clear_env: false,
            pipe_stdin: false,
            guard: false,
            timeout: None,
            max_line_bytes: None,
            stdout: Redirect::Capture,
            stderr: Redirect::Capture,
            on_stdout: None,
            on_stderr: None,
            on_exit: None,
        }
    }
}

struct JobMeta {
    owner: JobOwner,
    command: String,
    /// A reloaded plugin looks its job up by this instead of matching on the
    /// command string. See [`JobStore::find_named`].
    name: Option<String>,
    pid: u32,
    started: Instant,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    /// Feeds the writer thread that owns the child's stdin. Dropping it closes
    /// stdin, which is what `chanclose` and removing the job do.
    stdin: Option<flume::Sender<Vec<u8>>>,
    event_rx: Option<flume::Receiver<JobEvent>>,
    stdout_tail: VecDeque<String>,
    stderr_tail: VecDeque<String>,
    tail_cap: usize,
    /// Whether the tails ever lost a line, which is what `truncated` answers.
    /// True from the start for a stream sent to a file or dropped, since
    /// nothing ever reaches the tail there and an empty tail is no evidence
    /// the job stayed quiet.
    dropped_output: bool,
    /// Coordinates reaping, signals, timeouts and stdin errors across the job's threads.
    reap_state: Arc<ReapState>,
    deadline: Option<smol::Task<()>>,
    exit_code: Option<i32>,
    /// Recorded at exit so elapsed time stops counting once the process is gone.
    elapsed_secs: Option<u64>,
    /// Exit event owed to an `on_exit` attached after the process already died,
    /// served once by [`JobStore::next_matching`] as a synthetic event.
    replay_exit: Option<(i32, bool)>,
}

impl JobMeta {
    fn session(&self) -> Option<MakiId> {
        match self.owner {
            JobOwner::Session { session, .. } => Some(session),
            _ => None,
        }
    }

    /// Owning plugin of a session job. Other owners drop their job on exit, so
    /// only these keep a history worth capping.
    fn session_plugin(&self) -> Option<&Arc<str>> {
        match &self.owner {
            JobOwner::Session { plugin, .. } => Some(plugin),
            _ => None,
        }
    }

    fn record_line(&mut self, stdout: bool, line: &str) {
        let cap = self.tail_cap;
        if cap == 0 {
            self.dropped_output = true;
            return;
        }
        let tail = if stdout {
            &mut self.stdout_tail
        } else {
            &mut self.stderr_tail
        };
        let evicting = tail.len() >= cap;
        if evicting {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
        self.dropped_output |= evicting;
    }

    fn has_pending(&self) -> bool {
        self.replay_exit.is_some() || self.event_rx.as_ref().is_some_and(|rx| !rx.is_empty())
    }
}

/// What a `jobattach` opts table says about one callback slot.
pub(super) enum CallbackUpdate {
    Keep,
    Clear,
    Set(RegistryKey),
}

impl CallbackUpdate {
    fn apply(self, lua: &Lua, slot: &mut Option<RegistryKey>) {
        let replacement = match self {
            Self::Keep => return,
            Self::Clear => None,
            Self::Set(key) => Some(key),
        };
        if let Some(old) = mem::replace(slot, replacement) {
            lua.remove_registry_value(old).ok();
        }
    }
}

pub(crate) struct CallbackUpdates {
    pub(super) on_stdout: CallbackUpdate,
    pub(super) on_stderr: CallbackUpdate,
    pub(super) on_exit: CallbackUpdate,
}

pub(super) struct SpawnedJob {
    child: Unwatched,
    #[cfg(target_os = "linux")]
    lifetime: Option<UnixStream>,
    spec: JobSpec,
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
    pub(super) start_gate: Arc<AsyncMutex<()>>,
    /// Id served by the last [`JobStore::next_matching`], so the next scan
    /// starts past it.
    scan_cursor: u32,
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
            start_gate: Arc::default(),
            scan_cursor: 0,
        }
    }

    #[cfg(test)]
    pub fn start(&mut self, spec: JobSpec) -> Result<u32, String> {
        self.register(spec.spawn()?)
    }

    pub(super) fn register(&mut self, spawned: SpawnedJob) -> Result<u32, String> {
        let SpawnedJob {
            mut child,
            #[cfg(target_os = "linux")]
            lifetime,
            spec,
        } = spawned;
        let JobSpec {
            owner,
            cmd,
            name,
            #[cfg(target_os = "linux")]
            guard,
            timeout,
            max_line_bytes,
            stdout,
            stderr,
            on_stdout,
            on_stderr,
            on_exit,
            ..
        } = spec;
        let process = child.0.as_mut().ok_or(JOB_EXITED_ERR)?;
        let pid = process.id();
        let (stdin_pipe, stdout_pipe, stderr_pipe) = (
            process.stdin.take(),
            process.stdout.take(),
            process.stderr.take(),
        );
        let id = self.next_id;
        self.next_id += 1;

        let (event_tx, event_rx) = flume::unbounded();

        let reap_state = Arc::new(ReapState {
            group: Mutex::new(GroupSignals::new(pid)),
            ..Default::default()
        });
        let stdin = stdin_pipe
            .map(|stdin| spawn_stdin_writer(stdin, Arc::clone(&reap_state)))
            .transpose()?;
        let stdout_handle = spawn_reader(
            stdout_pipe,
            "job-stdout",
            &event_tx,
            JobEvent::Stdout,
            max_line_bytes,
            Arc::clone(&reap_state),
        )?;
        let stderr_handle = spawn_reader(
            stderr_pipe,
            "job-stderr",
            &event_tx,
            JobEvent::Stderr,
            max_line_bytes,
            Arc::clone(&reap_state),
        )?;

        let wait_reaped = Arc::clone(&reap_state);
        thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                let _lifetime = lifetime;
                let Some(child) = child.0.take() else {
                    return;
                };
                #[cfg(target_os = "linux")]
                if guard {
                    kill_group_once_leader_exits(pid);
                }
                // Reaping frees the pid, and that pid is the process group
                // `kill_job` signals. The readers only return once every
                // descendant dropped the pipes, so joining them first keeps a
                // kill target around for as long as the job is really alive.
                if let Some(h) = stdout_handle {
                    let _ = h.join();
                }
                if let Some(h) = stderr_handle {
                    let _ = h.join();
                }
                let code = reap(child, &wait_reaped);
                let timed_out = wait_reaped.timed_out.load(Ordering::Acquire);
                let _ = event_tx.send(JobEvent::Exit(code, timed_out));
            })
            .map_err(|e| e.to_string())?;

        let deadline = timeout.map(|timeout| {
            let state = Arc::clone(&reap_state);
            smol::spawn(async move {
                smol::Timer::after(timeout).await;
                smol::unblock(move || kill_unreaped(&state, true)).await;
            })
        });

        self.jobs.insert(
            id,
            JobMeta {
                owner,
                command: cmd.display(),
                name,
                pid,
                started: Instant::now(),
                on_stdout,
                on_stderr,
                on_exit,
                stdin,
                event_rx: Some(event_rx),
                stdout_tail: VecDeque::new(),
                stderr_tail: VecDeque::new(),
                tail_cap: DEFAULT_TAIL,
                dropped_output: !matches!(
                    (&stdout, &stderr),
                    (Redirect::Capture, Redirect::Capture)
                ),
                reap_state,
                deadline,
                exit_code: None,
                elapsed_secs: None,
                replay_exit: None,
            },
        );

        Ok(id)
    }

    pub(super) fn set_tail(&mut self, id: u32, tail: Option<usize>) {
        if let Some(job) = self.jobs.get_mut(&id)
            && let Some(cap) = tail
        {
            job.tail_cap = cap.min(MAX_TAIL_LINES);
        }
    }

    pub fn is_empty(&self, owner: &JobOwner) -> bool {
        !self.jobs.values().any(|job| job.owner == *owner)
    }

    pub fn callback_key(&self, job_id: u32, event: &JobEvent) -> Option<&RegistryKey> {
        let meta = self.jobs.get(&job_id)?;
        match event {
            JobEvent::Stdout(_) => meta.on_stdout.as_ref(),
            JobEvent::Stderr(_) => meta.on_stderr.as_ref(),
            JobEvent::Exit(..) => meta.on_exit.as_ref(),
        }
    }

    pub fn take_receiver(
        &mut self,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Option<flume::Receiver<JobEvent>> {
        let job = self.jobs.get_mut(&job_id)?;
        job.can_access(task_id, plugin)
            .then(|| job.event_rx.take())?
    }

    pub fn restore_receiver(&mut self, job_id: u32, receiver: flume::Receiver<JobEvent>) {
        if let Some(job) = self.jobs.get_mut(&job_id)
            && job.event_rx.is_none()
        {
            job.event_rx = Some(receiver);
        }
    }

    /// Pop one queued event. Deliberately one at a time: a batch pulled into
    /// a caller-owned buffer is lost if that caller is dropped mid-delivery,
    /// and with it the tail and exit code the event carries.
    pub fn next_event(&mut self, owner: &JobOwner) -> Option<(u32, JobEvent)> {
        self.next_matching(|job| job.owner == *owner)
    }

    /// [`Self::next_event`] for the jobs the host pumps: plugin-owned ones and
    /// session-owned ones, which outlive the task that started them.
    pub fn next_plugin_event(&mut self) -> Option<(u32, JobEvent)> {
        self.next_matching(|job| {
            matches!(job.owner, JobOwner::Plugin(_) | JobOwner::Session { .. })
        })
    }

    /// Round-robins over the jobs holding events: taking the first match every
    /// time would let a job printing faster than we deliver starve the rest.
    fn next_matching(&mut self, pred: impl Fn(&JobMeta) -> bool) -> Option<(u32, JobEvent)> {
        let mut past_cursor = None;
        let mut lowest = None;
        for (&id, job) in &self.jobs {
            if !pred(job) || !job.has_pending() {
                continue;
            }
            if id > self.scan_cursor {
                past_cursor = Some(past_cursor.map_or(id, |seen: u32| seen.min(id)));
            }
            lowest = Some(lowest.map_or(id, |seen: u32| seen.min(id)));
        }
        let id = past_cursor.or(lowest)?;
        self.scan_cursor = id;
        let job = self.jobs.get_mut(&id)?;
        if let Some((code, timed_out)) = job.replay_exit.take() {
            return Some((id, JobEvent::Exit(code, timed_out)));
        }
        Some((id, job.event_rx.as_ref()?.try_recv().ok()?))
    }

    pub fn record_event(&mut self, job_id: u32, event: &JobEvent) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        match event {
            JobEvent::Stdout(line) => job.record_line(true, line),
            JobEvent::Stderr(line) => job.record_line(false, line),
            // Termination is [`Self::complete`]'s business: it owns
            // `exit_code` and runs once per job.
            JobEvent::Exit(..) => {}
        }
    }

    /// Session-owned jobs stay inspectable after exit. Task/plugin jobs are
    /// removed the way they always were.
    pub fn complete(&mut self, lua: &Lua, job_id: u32, code: i32) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        // A replayed exit reaches here a second time: the job is already
        // booked, only the callbacks the replay just used need releasing.
        if job.exit_code.is_some() {
            drop_callbacks(lua, job);
            return;
        }
        job.exit_code = Some(code);
        job.deadline = None;
        job.elapsed_secs = Some(job.started.elapsed().as_secs());
        // A session job's record outlives the process, and the writer thread
        // waits on this sender until it drops.
        job.stdin = None;
        let session_plugin = job.session_plugin().cloned();
        drop_callbacks(lua, job);
        match session_plugin {
            Some(plugin) => self.evict_completed(lua, &plugin),
            None => self.finish(lua, job_id),
        }
    }

    /// Cap the exited session jobs {plugin} keeps, dropping the oldest ids
    /// first, so a chatty plugin only ever evicts its own history. Scanning
    /// `jobs` beats keeping a second list in sync with it.
    fn evict_completed(&mut self, lua: &Lua, plugin: &str) {
        let mut exited: Vec<u32> = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.exit_code.is_some()
                    && job
                        .session_plugin()
                        .is_some_and(|owner| owner.as_ref() == plugin)
            })
            .map(|(&id, _)| id)
            .collect();
        if exited.len() <= MAX_COMPLETED_SESSION_JOBS {
            return;
        }
        exited.sort_unstable();
        for oldest in &exited[..exited.len() - MAX_COMPLETED_SESSION_JOBS] {
            self.remove(lua, *oldest, false);
        }
    }

    pub fn kill_session(&mut self, lua: &Lua, session: MakiId) {
        let ids: Vec<u32> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.session() == Some(session))
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.remove(lua, id, true);
        }
    }

    /// Drop Lua callbacks for session jobs started by {plugin} without
    /// killing the process. Used on plugin unload/reload.
    pub fn detach_plugin_callbacks(&mut self, lua: &Lua, plugin: &str) {
        for job in self.jobs.values_mut() {
            if let JobOwner::Session {
                plugin: owner_plugin,
                ..
            } = &job.owner
                && owner_plugin.as_ref() == plugin
            {
                drop_callbacks(lua, job);
            }
        }
    }

    /// Re-arm callbacks on an existing job, the way a reloaded plugin picks up
    /// a session job it started before. An `on_exit` attached to an already
    /// exited job is owed a replay, queued rather than fired inline so the
    /// attaching plugin finishes its own setup first.
    pub fn attach(
        &mut self,
        lua: &Lua,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
        updates: CallbackUpdates,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return false;
        };
        if !job.can_access(task_id, plugin) {
            return false;
        }
        if matches!(updates.on_exit, CallbackUpdate::Set(_)) {
            job.replay_exit = job
                .exit_code
                .map(|code| (code, job.reap_state.timed_out.load(Ordering::Acquire)));
        }
        updates.on_stdout.apply(lua, &mut job.on_stdout);
        updates.on_stderr.apply(lua, &mut job.on_stderr);
        updates.on_exit.apply(lua, &mut job.on_exit);
        true
    }

    pub fn snapshot(&self, job_id: u32, task_id: Option<u64>, plugin: &str) -> Option<JobSnapshot> {
        let job = self.jobs.get(&job_id)?;
        job.can_access(task_id, plugin)
            .then(|| JobSnapshot::from_job(job_id, job, true))
    }

    /// Id of the live job this plugin can see holding `name`. An exited job
    /// keeps its name on the row but stops answering here, so a plugin that
    /// adopts by name restarts a job that died instead of taking over its id.
    pub fn find_named(&self, name: &str, task_id: Option<u64>, plugin: &str) -> Option<u32> {
        self.jobs
            .iter()
            .find(|(_, job)| {
                job.exit_code.is_none()
                    && job.name.as_deref() == Some(name)
                    && job.can_access(task_id, plugin)
            })
            .map(|(&id, _)| id)
    }

    /// List jobs this plugin can see. Task and plugin jobs leave the map on
    /// exit; session-owned jobs stay so exited ids stay findable. Tails live
    /// on `snapshot` / `jobinfo`.
    pub fn list(
        &self,
        session: Option<MakiId>,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Vec<JobSnapshot> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.can_access(task_id, plugin))
            .filter(|(_, job)| session.is_none_or(|s| job.session() == Some(s)))
            .map(|(&id, job)| JobSnapshot::from_job(id, job, false))
            .collect()
    }

    /// Drop an exited job this plugin can see. Running jobs are left alone.
    pub fn forget(&mut self, lua: &Lua, job_id: u32, task_id: Option<u64>, plugin: &str) {
        let Some(job) = self.jobs.get(&job_id) else {
            return;
        };
        if !job.can_access(task_id, plugin) || job.exit_code.is_none() {
            return;
        }
        self.remove(lua, job_id, false);
    }

    pub fn kill(&mut self, job_id: u32, task_id: Option<u64>, plugin: &str) {
        if let Some(job) = self.jobs.get_mut(&job_id)
            && job.can_access(task_id, plugin)
        {
            job.deadline = None;
            kill_job(job);
        }
    }

    pub fn send(
        &self,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
        data: Vec<u8>,
    ) -> Result<usize, &'static str> {
        let job = self
            .jobs
            .get(&job_id)
            .filter(|job| job.can_access(task_id, plugin))
            .ok_or(JOB_NOT_FOUND_ERR)?;
        if job.exit_code.is_some() {
            return Err(JOB_EXITED_ERR);
        }
        let stdin = job.stdin.as_ref().ok_or(NO_STDIN_ERR)?;
        match job.reap_state.stdin_error.load(Ordering::Acquire) {
            STDIN_EXITED => return Err(JOB_EXITED_ERR),
            STDIN_FAILED => return Err(STDIN_WRITE_ERR),
            _ => {}
        }
        let len = data.len();
        if len == 0 {
            return Ok(0);
        }
        if len > MAX_STDIN_BYTES {
            return Err(STDIN_OVERSIZED_ERR);
        }
        job.reap_state
            .stdin_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(len)
                    .filter(|bytes| *bytes <= MAX_STDIN_BYTES)
            })
            .map_err(|_| STDIN_FULL_ERR)?;
        if stdin.send(data).is_err() {
            job.reap_state.stdin_bytes.fetch_sub(len, Ordering::AcqRel);
            return Err(
                if job.reap_state.stdin_error.load(Ordering::Acquire) == STDIN_EXITED {
                    JOB_EXITED_ERR
                } else {
                    STDIN_WRITE_ERR
                },
            );
        }
        Ok(len)
    }

    pub fn close_stdin(
        &mut self,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Result<(), &'static str> {
        let job = self
            .jobs
            .get_mut(&job_id)
            .filter(|job| job.can_access(task_id, plugin))
            .ok_or(JOB_NOT_FOUND_ERR)?;
        job.stdin.take().map(drop).ok_or(NO_STDIN_ERR)
    }

    pub fn kill_owner(&mut self, lua: &Lua, owner: &JobOwner) -> Vec<u32> {
        let ids = self
            .jobs
            .iter()
            .filter_map(|(&id, job)| (job.owner == *owner).then_some(id))
            .collect::<Vec<_>>();
        for id in &ids {
            self.remove(lua, *id, true);
        }
        ids
    }

    pub fn finish(&mut self, lua: &Lua, job_id: u32) {
        self.remove(lua, job_id, false);
    }

    fn remove(&mut self, lua: &Lua, job_id: u32, kill: bool) {
        if let Some(job) = self.jobs.remove(&job_id) {
            if kill {
                kill_job(&job);
            }
            for key in [job.on_stdout, job.on_stderr, job.on_exit]
                .into_iter()
                .flatten()
            {
                lua.remove_registry_value(key).ok();
            }
        }
    }

    fn kill_all(&self) {
        for job in self.jobs.values() {
            kill_job(job);
        }
    }
}

impl Drop for JobStore {
    fn drop(&mut self) {
        self.kill_all();
    }
}

pub(crate) struct JobSnapshot {
    pub id: u32,
    pub command: String,
    pub name: Option<String>,
    pub session: Option<MakiId>,
    pub pid: u32,
    pub elapsed_secs: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
    /// Some output never reached the tails, so what they hold is a window
    /// onto a longer stream.
    pub dropped_output: bool,
}

impl JobSnapshot {
    fn from_job(id: u32, job: &JobMeta, tails: bool) -> Self {
        Self {
            id,
            command: job.command.clone(),
            name: job.name.clone(),
            session: job.session(),
            pid: job.pid,
            elapsed_secs: job
                .elapsed_secs
                .unwrap_or_else(|| job.started.elapsed().as_secs()),
            exit_code: job.exit_code,
            timed_out: job.reap_state.timed_out.load(Ordering::Acquire),
            stdout_lines: if tails {
                job.stdout_tail.iter().cloned().collect()
            } else {
                Vec::new()
            },
            stderr_lines: if tails {
                job.stderr_tail.iter().cloned().collect()
            } else {
                Vec::new()
            },
            dropped_output: job.dropped_output,
        }
    }
}

impl JobMeta {
    fn can_access(&self, task_id: Option<u64>, plugin: &str) -> bool {
        match &self.owner {
            JobOwner::Task(owner_id) => task_id == Some(*owner_id),
            JobOwner::Plugin(owner_plugin) => owner_plugin.as_ref() == plugin,
            JobOwner::Session {
                plugin: owner_plugin,
                ..
            } => owner_plugin.as_ref() == plugin,
        }
    }
}

fn drop_callbacks(lua: &Lua, job: &mut JobMeta) {
    for key in [&mut job.on_stdout, &mut job.on_stderr, &mut job.on_exit]
        .into_iter()
        .filter_map(Option::take)
    {
        lua.remove_registry_value(key).ok();
    }
}

fn spawn_reader(
    stream: Option<impl Read + Send + 'static>,
    name: &str,
    events: &flume::Sender<JobEvent>,
    event: fn(String) -> JobEvent,
    max_line_bytes: Option<usize>,
    reap_state: Arc<ReapState>,
) -> Result<Option<thread::JoinHandle<()>>, String> {
    let Some(stream) = stream else {
        return Ok(None);
    };
    let events = events.clone();
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut reader = BufReader::with_capacity(READER_BUF_SIZE, stream);
            loop {
                match read_output_line(&mut reader, max_line_bytes) {
                    Ok(Some(line)) => {
                        if events.send(event(text_line(line))).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = events.send(JobEvent::Stderr(error.to_string()));
                        kill_unreaped(&reap_state, false);
                        break;
                    }
                }
            }
        })
        .map(Some)
        .map_err(|error| error.to_string())
}

fn read_output_line(
    reader: &mut impl BufRead,
    limit: Option<usize>,
) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let bytes = reader
        .take(limit.map_or(u64::MAX, |limit| (limit as u64).saturating_add(1)))
        .read_until(b'\n', &mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if let Some(limit) = limit
        && line.len() > limit
    {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("job: output line exceeds the {limit}-byte limit"),
        ));
    }
    Ok(Some(line))
}

/// Writing to a pipe the child does not read blocks, so writes happen on
/// their own thread instead of the Lua thread. The thread ends, closing
/// stdin, when every sender drops or the child stops reading.
fn spawn_stdin_writer(
    mut stdin: ChildStdin,
    state: Arc<ReapState>,
) -> Result<flume::Sender<Vec<u8>>, String> {
    let (tx, rx) = flume::unbounded::<Vec<u8>>();
    thread::Builder::new()
        .name("job-stdin".into())
        .spawn(move || {
            for chunk in rx {
                let result = stdin.write_all(&chunk);
                state.stdin_bytes.fetch_sub(chunk.len(), Ordering::AcqRel);
                if let Err(error) = result {
                    state.stdin_error.store(
                        if matches!(
                            error.kind(),
                            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                        ) {
                            STDIN_EXITED
                        } else {
                            STDIN_FAILED
                        },
                        Ordering::Release,
                    );
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(tx)
}

/// The reap lock protects pid ownership through each signal. Status reads stay independent
/// so a slow platform kill cannot block the Lua thread.
#[derive(Default)]
struct ReapState {
    group: Mutex<GroupSignals>,
    timed_out: AtomicBool,
    stdin_error: AtomicU8,
    stdin_bytes: AtomicUsize,
}

impl ReapState {
    /// A panic elsewhere must not block a reap or a kill.
    fn lock(state: &Self) -> MutexGuard<'_, GroupSignals> {
        state.group.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Waits for {child} to exit, then reaps it under the {state} lock, so no
/// kill can hit a freed pid. The wait itself is outside the lock, so a kill
/// never waits on the child. Until the reap no other process can get the
/// pid: the zombie keeps it on Unix, the open process handle on Windows.
fn reap(mut child: Child, state: &ReapState) -> i32 {
    let lock = || ReapState::lock(state);
    #[cfg(unix)]
    let status = if process::wait_without_reaping(child.id()) {
        let mut state = lock();
        state.disable();
        child.wait()
    } else {
        // With no exit to wait for, the reap waits by itself, outside the
        // lock every kill uses. The kills stop first.
        tracing::warn!(
            pid = child.id(),
            "maki cannot wait for the job without reaping it, so a stop cannot kill it"
        );
        lock().disable();
        child.wait()
    };
    #[cfg(not(unix))]
    let status = child.wait();
    #[cfg(not(unix))]
    {
        lock().disable();
    }
    drop(child);
    status.map_or(-1, |status| status.code().unwrap_or(-1))
}

/// A descendant that closed its output keeps no reader open, so it can
/// outlive the leader unseen. Only this thread reaps the leader, and has not
/// yet, so the group is still the right target.
#[cfg(target_os = "linux")]
fn kill_group_once_leader_exits(pid: u32) {
    if process::wait_without_reaping(pid) {
        process::kill_group(pid);
    }
}

/// Without a wait thread, this guard must kill the group and reap the child. The unreaped pid
/// still identifies the group.
struct Unwatched(Option<Child>);

impl Drop for Unwatched {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            process::kill_group(child.id());
            let _ = child.wait();
        }
    }
}

/// A reaped pid can identify another process. Hold the lock until the signal completes so
/// [`reap`] cannot release the pid first.
fn kill_unreaped(state: &ReapState, timed_out: bool) {
    let reaped = ReapState::lock(state);
    if reaped.is_enabled() {
        if timed_out {
            state.timed_out.store(true, Ordering::Release);
        }
        reaped.kill();
    }
}

fn kill_job(job: &JobMeta) {
    #[cfg(windows)]
    let pid = job.pid;
    let state = Arc::clone(&job.reap_state);
    let kill = move || kill_unreaped(&state, false);
    #[cfg(windows)]
    if let Err(error) = thread::Builder::new().name("job-kill".into()).spawn(kill) {
        tracing::warn!(pid, %error, "could not start the job termination thread");
    }
    #[cfg(not(windows))]
    kill();
}

/// Returns {bytes} as one line of a job's output, without its line ending.
/// A byte that is not UTF-8 becomes U+FFFD, so the following lines still
/// arrive and a reader checking each line sees this one.
fn text_line(mut bytes: Vec<u8>) -> String {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use crate::runtime::with_jobs;
    #[cfg(target_os = "linux")]
    use futures_lite::future::poll_once;
    #[cfg(target_os = "linux")]
    use rustix::process::getpid;
    #[cfg(unix)]
    use std::fs::{self, Permissions};
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use test_case::test_case;

    use super::*;
    use crate::api::r#fn::deliver_job_event;

    const LINE_LIMIT: usize = 4;
    const LINE_LIMIT_ERROR: &str = "job: output line exceeds the 4-byte limit";

    #[test_case(b"test\n", false ; "at_limit")]
    #[test_case(b"tests\n", true ; "over_limit")]
    #[test_case(b"tests", true ; "unterminated_over_limit")]
    fn output_line_limits_bound_the_read(bytes: &[u8], too_large: bool) {
        let mut reader = Cursor::new(bytes);
        let line = read_output_line(&mut reader, Some(LINE_LIMIT));
        if too_large {
            assert_eq!(line.unwrap_err().to_string(), LINE_LIMIT_ERROR);
        } else {
            assert_eq!(line.unwrap().unwrap(), b"test");
        }
        assert!(reader.position() <= (LINE_LIMIT + 1) as u64);
    }

    #[cfg(unix)]
    #[test]
    fn an_oversized_unterminated_line_stops_the_job_before_dispatch() {
        const COMMAND: &str = "printf tests; exec sleep 300";
        const WAIT: Duration = Duration::from_secs(5);
        let mut store = JobStore::new();
        let mut spec = JobSpec::new(task_owner(1), COMMAND);
        spec.max_line_bytes = Some(LINE_LIMIT);
        let id = store.start(spec).unwrap();
        let rx = store.jobs[&id].event_rx.as_ref().unwrap();
        assert!(
            matches!(rx.recv_timeout(WAIT).unwrap(), JobEvent::Stderr(error) if error == LINE_LIMIT_ERROR)
        );
        assert!(matches!(rx.recv_timeout(WAIT).unwrap(), JobEvent::Exit(code, false) if code != 0));
    }

    #[cfg(unix)]
    const JOB_SHELL: &str = "bash";
    #[cfg(unix)]
    const SHELL_MODE: u32 = 0o700;
    #[cfg(unix)]
    const SELECTED_SHELL: &str = "the shell selected from the invoking path";
    const TEST_PLUGIN: &str = "test-plugin";
    const JOB_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
    const JOB_POLL_INTERVAL: Duration = Duration::from_millis(10);

    #[cfg(unix)]
    const EXIT_WITHOUT_REAP: &str =
        "an exit event must mean the child was reaped, or a later kill can signal a recycled pid";
    const NEVER_EXITED: &str = "job never reported its exit";
    #[cfg(target_os = "linux")]
    const ZOMBIE: char = 'Z';
    /// Exits only after the test writes `@GO@`, so it can neither exit nor be
    /// reaped before the test holds the lock.
    #[cfg(target_os = "linux")]
    const EXITS_ONCE_TOLD: &str = "until [ -e '@GO@' ]; do sleep 0.01; done";
    #[cfg(target_os = "linux")]
    const GO_MARKER: &str = "go";
    /// The shell exits at once, leaving a reader on its stdin that sees end of
    /// file only after the writer thread releases the pipe. The script keeps
    /// stdin itself, because a background job gets /dev/null.
    #[cfg(unix)]
    const STDIN_HOLDER: &str = "{ cat >/dev/null; touch '@EOF@'; } <&0 >/dev/null 2>&1 &";
    #[cfg(unix)]
    const EOF_MARKER: &str = "eof";
    #[cfg(unix)]
    const BAD_BYTE_BETWEEN_LINES: &str = r"printf 'first\n\377\nlast\r\n'";
    #[cfg(unix)]
    const PIPED_LINE: &str = "hello";
    #[cfg(unix)]
    const KEPT_VARIABLE: (&str, &str) = ("MAKI_JOB_ENV", "kept");
    #[cfg(unix)]
    const NEVER_ENDS: &str = "sleep 30";
    #[cfg(unix)]
    const ECHOES_STDIN: &str = "cat";
    #[cfg(unix)]
    const PRINTS_ENV: &str = "env";
    /// Keeps a `sleep` that closed all its output, and prints its pid.
    #[cfg(target_os = "linux")]
    const LEFT_ITS_OUTPUT: &str = "sleep 30 >/dev/null 2>&1 </dev/null & echo $!";

    #[cfg(unix)]
    #[test]
    fn a_job_deadline_kills_without_lua_polling() {
        let mut store = JobStore::new();
        let mut spec = JobSpec::new(JobOwner::Plugin(TEST_PLUGIN.into()), NEVER_ENDS);
        spec.timeout = Some(Duration::ZERO);
        let id = store.start(spec).unwrap();
        let receiver = store.jobs[&id].event_rx.as_ref().unwrap();
        loop {
            if let JobEvent::Exit(_, timed_out) = receiver.recv_timeout(JOB_EXIT_TIMEOUT).unwrap() {
                assert!(timed_out);
                assert!(!ReapState::lock(&store.jobs[&id].reap_state).is_enabled());
                break;
            }
        }
    }

    /// Pull events until {id} reports its exit, so assertions run against a
    /// job that is certainly done.
    fn collect_until_exit(
        id: u32,
        mut next: impl FnMut() -> Option<(u32, JobEvent)>,
    ) -> Vec<(u32, JobEvent)> {
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        let mut events = Vec::new();
        loop {
            while let Some(event) = next() {
                events.push(event);
            }
            if events
                .iter()
                .any(|(job_id, event)| *job_id == id && matches!(event, JobEvent::Exit(..)))
            {
                return events;
            }
            assert!(Instant::now() < deadline, "{NEVER_EXITED}");
            thread::sleep(JOB_POLL_INTERVAL);
        }
    }

    /// What `deliver_job_event` does with {event}, minus the callback.
    fn deliver(store: &mut JobStore, lua: &Lua, id: u32, event: &JobEvent) {
        store.record_event(id, event);
        if let JobEvent::Exit(code, _) = event {
            store.complete(lua, id, *code);
        }
    }

    fn make_store() -> JobStore {
        JobStore::new()
    }

    fn task_owner(id: u64) -> JobOwner {
        JobOwner::Task(id)
    }

    fn plugin_owner() -> JobOwner {
        JobOwner::Plugin(Arc::from(TEST_PLUGIN))
    }

    fn session_owner(session: MakiId) -> JobOwner {
        JobOwner::Session {
            session,
            plugin: Arc::from(TEST_PLUGIN),
        }
    }

    fn stub_job(
        owner: JobOwner,
        on_stdout: Option<RegistryKey>,
        on_exit: Option<RegistryKey>,
    ) -> JobMeta {
        JobMeta {
            owner,
            command: String::new(),
            name: None,
            pid: 0,
            started: Instant::now(),
            on_stdout,
            on_stderr: None,
            on_exit,
            stdin: None,
            event_rx: None,
            stdout_tail: VecDeque::new(),
            stderr_tail: VecDeque::new(),
            tail_cap: DEFAULT_TAIL,
            dropped_output: false,
            reap_state: Arc::new(ReapState::default()),
            deadline: None,
            exit_code: None,
            elapsed_secs: None,
            replay_exit: None,
        }
    }

    #[test]
    fn job_status_does_not_wait_for_process_tree_termination() {
        let job = stub_job(task_owner(1), None, None);
        let state = Arc::clone(&job.reap_state);
        let killing = ReapState::lock(&state);
        state.timed_out.store(true, Ordering::Release);
        let (sent, received) = flume::bounded(1);
        let reader = thread::spawn(move || {
            sent.send(JobSnapshot::from_job(1, &job, true)).unwrap();
        });
        let snapshot = received.recv_timeout(JOB_EXIT_TIMEOUT);
        drop(killing);
        reader.join().unwrap();
        assert!(snapshot.unwrap().timed_out);
    }

    fn start_echo(store: &mut JobStore) -> u32 {
        store
            .start(JobSpec::new(task_owner(1), "echo hello"))
            .unwrap()
    }

    #[cfg(unix)]
    fn group_alive(pid: u32) -> bool {
        use rustix::process::{Pid, test_kill_process_group};
        i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .is_some_and(|pid| test_kill_process_group(pid).is_ok())
    }

    #[cfg(unix)]
    fn wait_for_group_exit(pid: u32) -> bool {
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        while Instant::now() < deadline {
            if !group_alive(pid) {
                return true;
            }
            thread::sleep(JOB_POLL_INTERVAL);
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_store_kills_its_jobs() {
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(task_owner(1), "sleep 30"))
            .expect("job started");
        let pid = store.jobs[&id].pid;
        assert!(group_alive(pid), "job should be running before the drop");

        drop(store);

        assert!(
            wait_for_group_exit(pid),
            "dropping the store must not orphan the process group"
        );
    }

    #[test_case(false ; "inherited_environment")]
    #[test_case(true ; "clear_environment")]
    fn start_validates_cwd_before_lookup_and_redirection(clear_env: bool) {
        const INVALID_CWD: &str = "missing-cwd";
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join(INVALID_CWD);
        let redirected = temp.path().join("output");
        let mut store = make_store();
        let result = store.start(JobSpec {
            cwd: Some(cwd.to_str().unwrap().into()),
            clear_env,
            stdout: Redirect::File(redirected.clone()),
            ..JobSpec::new(
                task_owner(1),
                JobCommand::Argv(vec!["nonexistent_program_xyz".into()]),
            )
        });
        assert_eq!(
            result.unwrap_err(),
            format!("{CWD_NOT_DIRECTORY}: {}", cwd.display())
        );
        assert!(!redirected.exists());
    }

    #[test]
    fn finishing_a_job_removes_it() {
        let lua = Lua::new();
        let mut store = make_store();
        let owner = task_owner(1);
        assert!(store.is_empty(&owner));

        let id = start_echo(&mut store);
        assert!(!store.is_empty(&owner));
        let receiver = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();
        while !matches!(
            receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
            JobEvent::Exit(..)
        ) {}

        store.finish(&lua, id);
        assert!(store.is_empty(&owner));
    }

    #[test]
    fn unknown_job_operations_are_noops() {
        let mut store = make_store();
        store.kill(999, Some(1), TEST_PLUGIN);
        assert!(store.take_receiver(999, Some(1), TEST_PLUGIN).is_none());
        assert!(store.callback_key(999, &JobEvent::Exit(0, false)).is_none());
    }

    #[test]
    fn take_receiver_lifecycle() {
        let mut store = make_store();
        assert!(store.take_receiver(999, Some(1), TEST_PLUGIN).is_none());

        let id = start_echo(&mut store);
        assert!(
            store.take_receiver(id, Some(2), TEST_PLUGIN).is_none(),
            "another task must not access the job"
        );
        assert!(store.take_receiver(id, Some(1), TEST_PLUGIN).is_some());
        assert!(
            store.take_receiver(id, Some(1), TEST_PLUGIN).is_none(),
            "second take should fail (receiver already moved)"
        );
    }

    #[test]
    fn plugin_owner_can_be_accessed_only_by_its_plugin() {
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(plugin_owner(), "echo hello"))
            .unwrap();

        assert!(store.take_receiver(id, Some(1), "other-plugin").is_none());
        assert!(store.take_receiver(id, None, TEST_PLUGIN).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn kill_requires_owner_access() {
        let lua = Lua::new();
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                timeout: Some(JOB_EXIT_TIMEOUT),
                ..JobSpec::new(task_owner(1), "sleep 30")
            })
            .unwrap();
        let pid = store.jobs[&id].pid;

        store.kill(id, Some(2), TEST_PLUGIN);
        assert!(group_alive(pid));
        assert!(store.jobs[&id].deadline.is_some());

        store.kill(id, Some(1), TEST_PLUGIN);
        assert!(store.jobs[&id].deadline.is_none());
        assert!(wait_for_group_exit(pid));
        store.finish(&lua, id);
    }

    #[cfg(unix)]
    #[test]
    fn owner_cleanup_is_isolated() {
        let lua = Lua::new();
        let mut store = make_store();
        let task = task_owner(1);
        let plugin = plugin_owner();
        let task_id = store.start(JobSpec::new(task.clone(), "sleep 30")).unwrap();
        let plugin_id = store
            .start(JobSpec::new(plugin.clone(), "sleep 30"))
            .unwrap();
        let task_pid = store.jobs[&task_id].pid;
        let plugin_pid = store.jobs[&plugin_id].pid;

        store.kill_owner(&lua, &task);

        assert!(store.is_empty(&task));
        assert!(!store.is_empty(&plugin));
        assert!(wait_for_group_exit(task_pid));
        assert!(group_alive(plugin_pid));
        store.kill_owner(&lua, &plugin);
        assert!(wait_for_group_exit(plugin_pid));
    }

    #[test]
    fn callback_key_returns_none_without_callbacks() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        assert!(
            store
                .callback_key(id, &JobEvent::Stdout("x".into()))
                .is_none()
        );
        assert!(
            store
                .callback_key(id, &JobEvent::Stderr("x".into()))
                .is_none()
        );
        assert!(store.callback_key(id, &JobEvent::Exit(0, false)).is_none());
    }

    #[test]
    fn take_receiver_delivers_events() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let rx = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        let mut got_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(JobEvent::Exit(..)) => {
                    got_exit = true;
                    break;
                }
                Ok(_) => continue,
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(got_exit, "should receive exit event for completed job");
    }

    /// A line that is not UTF-8 arrives as text with U+FFFD for its bad
    /// bytes, and the lines after it still arrive.
    #[cfg(unix)]
    #[test]
    fn a_line_that_is_not_utf8_arrives_and_so_do_the_rest() {
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(plugin_owner(), BAD_BYTE_BETWEEN_LINES))
            .unwrap();
        let lines: Vec<String> = collect_until_exit(id, || store.next_plugin_event())
            .into_iter()
            .filter_map(|(_, event)| match event {
                JobEvent::Stdout(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(lines, ["first", "\u{FFFD}", "last"]);
    }

    #[test]
    fn next_event_filters_by_owner() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let plugin_id = store
            .start(JobSpec::new(plugin_owner(), "echo plugin"))
            .unwrap();

        let task_events = collect_until_exit(id, || store.next_event(&task_owner(1)));
        assert!(task_events.iter().all(|(job_id, _)| *job_id != plugin_id));

        let plugin_events = collect_until_exit(plugin_id, || store.next_plugin_event());
        assert!(plugin_events.iter().all(|(job_id, _)| *job_id != id));
    }

    #[test]
    fn next_event_is_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _rx = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        assert!(
            store.next_event(&task_owner(1)).is_none(),
            "a checked out receiver yields no events to the pump"
        );
    }

    #[test]
    fn next_event_round_robins_so_a_chatty_job_cannot_starve_its_sibling() {
        const QUEUED_PER_JOB: usize = 2;
        let mut store = make_store();
        for id in [1, 2] {
            let (tx, rx) = flume::unbounded();
            for _ in 0..QUEUED_PER_JOB {
                tx.send(JobEvent::Stdout("spam".into())).unwrap();
            }
            let mut job = stub_job(task_owner(1), None, None);
            job.event_rx = Some(rx);
            store.jobs.insert(id, job);
        }

        let served: Vec<u32> = std::iter::from_fn(|| store.next_event(&task_owner(1)))
            .map(|(id, _)| id)
            .collect();

        assert_eq!(served, [1, 2, 1, 2]);
    }

    #[test]
    fn exit_cleanup_runs_before_a_failing_callback() {
        let lua = Lua::new();
        lua.set_app_data(JobStore::new());
        let callback = lua
            .create_function(|_, ()| Err::<(), _>(mlua::Error::runtime("callback failed")))
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        with_jobs(&lua, |store| {
            store
                .jobs
                .insert(1, stub_job(task_owner(1), None, Some(callback_key)));
        });

        assert!(smol::block_on(deliver_job_event(&lua, 1, &JobEvent::Exit(0, false))).is_err());
        assert!(with_jobs(&lua, |store| store.is_empty(&task_owner(1))));
    }

    #[test]
    fn finish_releases_callback_registry_values() {
        let lua = Lua::new();
        let capture = Arc::new(());
        let callback_capture = Arc::clone(&capture);
        let callback = lua
            .create_function(move |_, ()| {
                let _ = &callback_capture;
                Ok(())
            })
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        let mut store = make_store();
        store
            .jobs
            .insert(1, stub_job(task_owner(1), Some(callback_key), None));
        assert_eq!(Arc::strong_count(&capture), 2);

        store.finish(&lua, 1);
        lua.gc_collect().unwrap();

        assert_eq!(Arc::strong_count(&capture), 1);
    }

    #[test]
    fn snapshot_reports_live_tails_and_hides_inaccessible_jobs() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        store.record_event(id, &JobEvent::Stdout("hello".into()));
        store.record_event(id, &JobEvent::Stderr("warn".into()));

        let snap = store.snapshot(id, Some(1), TEST_PLUGIN).unwrap();
        assert_eq!(snap.command, "echo hello");
        assert_eq!(snap.stdout_lines, ["hello"]);
        assert_eq!(snap.stderr_lines, ["warn"]);
        assert!(snap.exit_code.is_none());
        assert!(store.snapshot(id, Some(2), TEST_PLUGIN).is_none());
        assert!(store.snapshot(999, Some(1), TEST_PLUGIN).is_none());
    }

    #[test]
    fn tail_cap_drops_oldest_lines() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        store.set_tail(id, Some(2));
        store.record_event(id, &JobEvent::Stdout("a".into()));
        store.record_event(id, &JobEvent::Stdout("b".into()));
        store.record_event(id, &JobEvent::Stdout("c".into()));
        let snap = store.snapshot(id, Some(1), TEST_PLUGIN).unwrap();
        assert_eq!(snap.stdout_lines, ["b", "c"]);
    }

    #[test]
    fn list_is_live_and_access_scoped() {
        let lua = Lua::new();
        let mut store = make_store();
        let task = start_echo(&mut store);
        let plugin = store
            .start(JobSpec::new(plugin_owner(), "echo plugin"))
            .unwrap();

        let from_task: Vec<u32> = store
            .list(None, Some(1), TEST_PLUGIN)
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(from_task.contains(&task));
        assert!(from_task.contains(&plugin));

        let from_other: Vec<u32> = store
            .list(None, Some(2), "other-plugin")
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(from_other.is_empty());

        store.finish(&lua, task);
        let live: Vec<u32> = store
            .list(None, Some(1), TEST_PLUGIN)
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(live, [plugin]);
        assert!(store.snapshot(task, Some(1), TEST_PLUGIN).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn session_job_survives_plugin_detach_and_answers_after_exit() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let owner = JobOwner::Session {
            session,
            plugin: Arc::from(TEST_PLUGIN),
        };
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(owner.clone(), "echo hi; echo err >&2; exit 3"))
            .unwrap();
        store.set_tail(id, Some(5));

        store.detach_plugin_callbacks(&lua, TEST_PLUGIN);
        assert!(store.jobs.contains_key(&id), "detach must keep the job");

        for (_, event) in collect_until_exit(id, || store.next_plugin_event()) {
            deliver(&mut store, &lua, id, &event);
        }

        let snap = store
            .snapshot(id, None, TEST_PLUGIN)
            .expect("peek after exit");
        assert_eq!(snap.exit_code, Some(3));
        assert!(
            snap.stdout_lines.iter().any(|l| l.contains("hi")),
            "stdout tail: {:?}",
            snap.stdout_lines
        );
        assert!(
            snap.stderr_lines.iter().any(|l| l.contains("err")),
            "stderr tail: {:?}",
            snap.stderr_lines
        );

        store.kill_session(&lua, session);
        assert!(store.snapshot(id, None, TEST_PLUGIN).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn kill_session_does_not_touch_other_sessions() {
        let lua = Lua::new();
        let a = MakiId::generate();
        let b = MakiId::generate();
        let mut store = make_store();
        let first = store
            .start(JobSpec::new(session_owner(a), "sleep 30"))
            .unwrap();
        let second = store
            .start(JobSpec::new(session_owner(b), "sleep 30"))
            .unwrap();
        store.kill_session(&lua, a);
        assert!(store.snapshot(first, None, TEST_PLUGIN).is_none());
        assert!(store.snapshot(second, None, TEST_PLUGIN).is_some());
        store.kill_session(&lua, b);
    }

    #[test]
    fn snapshot_elapsed_freezes_at_recorded_exit() {
        const PAST_SECS: u64 = 45;
        let lua = Lua::new();
        let mut store = make_store();
        let mut job = stub_job(session_owner(MakiId::generate()), None, None);
        job.started = Instant::now()
            .checked_sub(Duration::from_secs(PAST_SECS))
            .expect("clock has enough history");
        store.jobs.insert(1, job);

        deliver(&mut store, &lua, 1, &JobEvent::Exit(0, false));
        let at_exit = store.snapshot(1, None, TEST_PLUGIN).unwrap().elapsed_secs;
        assert!(
            at_exit >= PAST_SECS,
            "elapsed at exit should reflect the backdated start, got {at_exit}"
        );

        store.jobs.get_mut(&1).unwrap().started = Instant::now();
        let later = store.snapshot(1, None, TEST_PLUGIN).unwrap().elapsed_secs;
        assert_eq!(later, at_exit, "elapsed must freeze once the job exits");
    }

    fn keep_all() -> CallbackUpdates {
        CallbackUpdates {
            on_stdout: CallbackUpdate::Keep,
            on_stderr: CallbackUpdate::Keep,
            on_exit: CallbackUpdate::Keep,
        }
    }

    fn exit_updates(lua: &Lua) -> CallbackUpdates {
        CallbackUpdates {
            on_exit: CallbackUpdate::Set(noop_key(lua)),
            ..keep_all()
        }
    }

    fn noop_key(lua: &Lua) -> RegistryKey {
        let noop = lua.create_function(|_, ()| Ok(())).unwrap();
        lua.create_registry_value(noop).unwrap()
    }

    /// Which callbacks the stub job (always id 1 here) still holds, as
    /// (stdout, stderr, exit).
    fn armed_slots(store: &JobStore) -> (bool, bool, bool) {
        let held = |event| store.callback_key(1, &event).is_some();
        (
            held(JobEvent::Stdout(String::new())),
            held(JobEvent::Stderr(String::new())),
            held(JobEvent::Exit(0, false)),
        )
    }

    fn exited_session_job(lua: &Lua, store: &mut JobStore, code: i32) {
        store
            .jobs
            .insert(1, stub_job(session_owner(MakiId::generate()), None, None));
        deliver(store, lua, 1, &JobEvent::Exit(code, false));
    }

    #[test]
    fn an_argv_row_reads_back_as_the_same_argv() {
        const ARGV: [&str; 2] = ["echo", "a; echo pwned"];
        let row = JobCommand::Argv(ARGV.map(String::from).to_vec()).display();
        assert_eq!(
            shell_words::split(&row).unwrap(),
            ARGV,
            "the row a user reads must quote what the shell would have eaten"
        );
    }

    #[test_case(0, STDIN_WRITE_ERR ; "disconnected_writer")]
    #[test_case(STDIN_EXITED, JOB_EXITED_ERR ; "exited_writer")]
    #[test_case(STDIN_FAILED, STDIN_WRITE_ERR ; "failed_writer")]
    fn stdin_queue_errors_do_not_leak_reservations(state: u8, expected: &str) {
        let mut store = make_store();
        let mut job = stub_job(task_owner(1), None, None);
        let (tx, rx) = flume::unbounded();
        drop(rx);
        job.stdin = Some(tx);
        job.reap_state.stdin_error.store(state, Ordering::Release);
        store.jobs.insert(1, job);
        assert_eq!(store.send(1, Some(1), TEST_PLUGIN, vec![0]), Err(expected));
        assert_eq!(
            store.jobs[&1]
                .reap_state
                .stdin_bytes
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn stdin_queue_bounds_bytes_and_ignores_empty_writes() {
        let mut store = make_store();
        let mut job = stub_job(task_owner(1), None, None);
        let (tx, rx) = flume::unbounded();
        job.stdin = Some(tx);
        store.jobs.insert(1, job);
        assert_eq!(store.send(1, Some(1), TEST_PLUGIN, Vec::new()), Ok(0));
        assert!(rx.is_empty());
        assert_eq!(
            store.send(1, Some(1), TEST_PLUGIN, vec![0; MAX_STDIN_BYTES + 1]),
            Err(STDIN_OVERSIZED_ERR)
        );
        assert!(rx.is_empty());
        assert_eq!(
            store.send(1, Some(1), TEST_PLUGIN, vec![0; MAX_STDIN_BYTES]),
            Ok(MAX_STDIN_BYTES)
        );
        assert_eq!(
            store.send(1, Some(1), TEST_PLUGIN, vec![0]),
            Err(STDIN_FULL_ERR)
        );
        assert_eq!(rx.len(), 1);
        assert_eq!(
            store.jobs[&1]
                .reap_state
                .stdin_bytes
                .load(Ordering::Acquire),
            MAX_STDIN_BYTES
        );
    }

    #[cfg(unix)]
    #[test]
    fn piped_stdin_reaches_the_job_until_it_is_closed() {
        let line = PIPED_LINE;
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                pipe_stdin: true,
                ..JobSpec::new(task_owner(1), JobCommand::Argv(vec![ECHOES_STDIN.into()]))
            })
            .unwrap();
        let data = format!("{line}\n").into_bytes();
        let len = data.len();
        assert_eq!(store.send(id, Some(1), TEST_PLUGIN, data), Ok(len));
        store.close_stdin(id, Some(1), TEST_PLUGIN).unwrap();

        let events = collect_until_exit(id, || store.next_event(&task_owner(1)));
        let out: Vec<&str> = events
            .iter()
            .filter_map(|(_, event)| match event {
                JobEvent::Stdout(l) => Some(l.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(out, [line]);
        assert_eq!(
            store.jobs[&id]
                .reap_state
                .stdin_bytes
                .load(Ordering::Acquire),
            0
        );
        assert!(
            events
                .iter()
                .any(|(_, e)| matches!(e, JobEvent::Exit(0, false))),
            "after the close of stdin, cat must exit"
        );
    }

    /// The exited job's record stays for `joblist`, so `complete` must drop
    /// the sender too, or the writer thread waits on it forever.
    #[cfg(unix)]
    #[test]
    fn a_completed_session_job_lets_its_stdin_writer_go() {
        let lua = Lua::new();
        let dir = tempfile::tempdir().unwrap();
        let eof = dir.path().join(EOF_MARKER);
        let owner = session_owner(MakiId::generate());
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                pipe_stdin: true,
                ..JobSpec::new(
                    owner.clone(),
                    STDIN_HOLDER
                        .replace("@EOF@", &eof.to_string_lossy())
                        .as_str(),
                )
            })
            .unwrap();
        for (_, event) in collect_until_exit(id, || store.next_event(&owner)) {
            deliver(&mut store, &lua, id, &event);
        }
        assert!(store.jobs[&id].exit_code.is_some());
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        while !eof.exists() && Instant::now() < deadline {
            thread::sleep(JOB_POLL_INTERVAL);
        }
        assert!(
            eof.exists(),
            "the writer thread keeps stdin open after the job completed"
        );
    }

    #[cfg(unix)]
    #[test_case(false ; "without_a_pipe")]
    #[test_case(true ; "after_closing_it")]
    fn writing_to_a_closed_or_missing_stdin_is_an_error(pipe_stdin: bool) {
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                pipe_stdin,
                ..JobSpec::new(task_owner(1), NEVER_ENDS)
            })
            .unwrap();
        if pipe_stdin {
            store.close_stdin(id, Some(1), TEST_PLUGIN).unwrap();
        }
        assert_eq!(
            store.send(id, Some(1), TEST_PLUGIN, b"x".to_vec()),
            Err(NO_STDIN_ERR)
        );
        assert_eq!(
            store.close_stdin(id, Some(1), TEST_PLUGIN),
            Err(NO_STDIN_ERR)
        );
    }

    #[cfg(windows)]
    #[test_case("PATH")]
    #[test_case("Path")]
    #[test_case("path")]
    fn explicit_windows_path_is_case_insensitive(name: &str) {
        const PROGRAM: &str = "maki-uninstalled-test-helper.exe";
        let command = JobSpec {
            clear_env: true,
            env: Some(HashMap::from([(name.to_owned(), String::new())])),
            ..JobSpec::new(task_owner(1), JobCommand::Argv(vec![PROGRAM.into()]))
        }
        .command()
        .unwrap();
        assert_eq!(command.get_program(), PROGRAM);
    }

    #[cfg(unix)]
    #[test]
    fn clear_env_shell_jobs_use_the_invoking_path() {
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join(JOB_SHELL);
        fs::write(
            &shell,
            format!("#!/bin/sh\nprintf '%s\\n' '{SELECTED_SHELL}'\n"),
        )
        .unwrap();
        fs::set_permissions(&shell, Permissions::from_mode(SHELL_MODE)).unwrap();
        let mut command = JobCommand::Shell("exit 1".to_owned())
            .build(Some((dir.path(), Some(dir.path().as_os_str()))))
            .unwrap();
        let output = command.env_clear().output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            SELECTED_SHELL
        );
    }

    #[cfg(unix)]
    #[test]
    fn clear_env_leaves_only_the_given_variables() {
        let (var, value) = KEPT_VARIABLE;
        let path = env::var("PATH").unwrap();
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                env: Some(HashMap::from([
                    ("PATH".to_string(), path.clone()),
                    (var.to_string(), value.to_string()),
                ])),
                clear_env: true,
                ..JobSpec::new(task_owner(1), JobCommand::Argv(vec![PRINTS_ENV.into()]))
            })
            .unwrap();

        let mut lines: Vec<String> = collect_until_exit(id, || store.next_event(&task_owner(1)))
            .into_iter()
            .filter_map(|(_, event)| match event {
                JobEvent::Stdout(line) => Some(line),
                _ => None,
            })
            .collect();
        lines.sort();
        assert_eq!(lines, [format!("{var}={value}"), format!("PATH={path}")]);
    }

    /// A kill holds the lock from its state check to its signal. Meanwhile the
    /// exited child must stay an unreaped zombie, so no other process can get
    /// its pid and its exit is not reported yet.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kill_in_progress_keeps_the_child_unreaped() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join(GO_MARKER);
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(
                task_owner(1),
                EXITS_ONCE_TOLD
                    .replace("@GO@", &go.to_string_lossy())
                    .as_str(),
            ))
            .unwrap();
        let (pid, reaped) = (store.jobs[&id].pid, Arc::clone(&store.jobs[&id].reap_state));
        let kill_in_progress = ReapState::lock(&reaped);
        fs::write(&go, "").unwrap();
        let zombie = || {
            fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with(ZOMBIE))
            })
        };
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        while !zombie() && Instant::now() < deadline {
            thread::sleep(JOB_POLL_INTERVAL);
        }
        assert!(
            zombie(),
            "the child did not become a zombie that is not reaped"
        );
        assert!(
            store.next_event(&task_owner(1)).is_none(),
            "maki gave an exit while a kill held the lock"
        );
        drop(kill_in_progress);
        collect_until_exit(id, || store.next_event(&task_owner(1)));
    }

    /// A job whose setup fails after it started has no wait thread, so only
    /// the guard stops and reaps it. Otherwise a zombie would hold its pid
    /// until maki exits.
    #[cfg(unix)]
    #[test]
    fn an_unwatched_job_is_killed_and_reaped() {
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
        drop(Unwatched(Some(child)));
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelling_a_guarded_spawn_kills_and_reaps_the_unregistered_child() {
        smol::block_on(async {
            let (mut entered, mut notification) = UnixStream::pair().unwrap();
            let (mut release, mut blocked) = UnixStream::pair().unwrap();
            entered.set_read_timeout(Some(JOB_EXIT_TIMEOUT)).unwrap();
            let spec = JobSpec {
                guard: true,
                ..JobSpec::new(task_owner(1), NEVER_ENDS)
            };
            let mut command = spec.command().unwrap();
            // SAFETY: the hook only writes a stack integer and waits on a socket, without
            // allocation or locks. The parent owns the peer that releases it after cancellation.
            unsafe {
                command.pre_exec(move || {
                    notification.write_all(&getpid().as_raw_nonzero().get().to_ne_bytes())?;
                    let mut ready = [0];
                    blocked.read_exact(&mut ready)?;
                    Ok(())
                });
            }
            let mut spawning = Box::pin(smol::unblock(move || spec.spawn_command(command)));
            assert!(poll_once(spawning.as_mut()).await.is_none());
            let pid = smol::unblock(move || {
                let mut bytes = [0; size_of::<i32>()];
                entered.read_exact(&mut bytes).unwrap();
                i32::from_ne_bytes(bytes) as u32
            })
            .await;
            assert!(group_alive(pid));
            drop(spawning);
            release.write_all(&[0]).unwrap();
            assert!(
                wait_for_group_exit(pid),
                "the cancelled spawn left its process group alive"
            );
        });
    }

    /// The `sleep` holds none of the job's pipes, so without the group kill
    /// nothing waits for it or stops it after the shell exits.
    #[cfg(target_os = "linux")]
    #[test_case(LEFT_ITS_OUTPUT ; "closed_output")]
    #[test_case("sleep 30 & echo $!" ; "retained_output")]
    fn a_child_dies_with_the_guarded_leader(command: &str) {
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                guard: true,
                ..JobSpec::new(
                    task_owner(1),
                    JobCommand::Argv(vec!["sh".into(), "-c".into(), command.into()]),
                )
            })
            .unwrap();
        let child: u32 = collect_until_exit(id, || store.next_event(&task_owner(1)))
            .into_iter()
            .find_map(|(_, event)| match event {
                JobEvent::Stdout(line) => line.trim().parse().ok(),
                _ => None,
            })
            .expect("the shell printed the child's pid");
        // A killed child stays a zombie until its new parent reaps it, and a
        // zombie has an empty cmdline.
        let running =
            || fs::read(format!("/proc/{child}/cmdline")).is_ok_and(|cmdline| !cmdline.is_empty());
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        while running() && Instant::now() < deadline {
            thread::sleep(JOB_POLL_INTERVAL);
        }
        assert!(
            !running(),
            "the child in the background continued after its job"
        );
    }

    #[cfg(unix)]
    #[test]
    fn redirected_streams_append_to_the_file_and_keep_no_tail() {
        const EXISTING: &str = "older\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.log");
        std::fs::write(&path, EXISTING).unwrap();
        let mut store = make_store();
        let id = store
            .start(JobSpec {
                stdout: Redirect::File(path.clone()),
                stderr: Redirect::Discard,
                ..JobSpec::new(task_owner(1), "echo hi; echo err >&2")
            })
            .unwrap();

        for (_, event) in collect_until_exit(id, || store.next_event(&task_owner(1))) {
            store.record_event(id, &event);
        }

        let snap = store.snapshot(id, Some(1), TEST_PLUGIN).unwrap();
        assert!(
            snap.stdout_lines.is_empty() && snap.stderr_lines.is_empty(),
            "a redirected stream must not be buffered here"
        );
        assert!(
            snap.dropped_output,
            "an empty tail on a redirected stream must not read as the whole output"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{EXISTING}hi\n")
        );
    }

    #[test]
    fn a_name_is_held_by_the_live_job_only() {
        const NAME: &str = "log-tail";
        let lua = Lua::new();
        let mut store = make_store();
        let mut job = stub_job(session_owner(MakiId::generate()), None, None);
        job.name = Some(NAME.to_string());
        store.jobs.insert(1, job);

        assert_eq!(store.find_named(NAME, None, TEST_PLUGIN), Some(1));
        assert_eq!(store.find_named(NAME, None, "other-plugin"), None);
        assert_eq!(store.find_named("absent", None, TEST_PLUGIN), None);

        deliver(&mut store, &lua, 1, &JobEvent::Exit(0, false));

        assert_eq!(
            store.find_named(NAME, None, TEST_PLUGIN),
            None,
            "an exited job must release its name so the next start is not blocked"
        );
        assert_eq!(
            store
                .snapshot(1, None, TEST_PLUGIN)
                .unwrap()
                .name
                .as_deref(),
            Some(NAME),
            "the name stays on the row that explains the exit"
        );
    }

    #[test_case(false ; "completed")]
    #[test_case(true ; "timed_out")]
    fn attaching_on_exit_after_the_exit_replays_it_once(timed_out: bool) {
        const CODE: i32 = 5;
        const REWIND: Duration = Duration::from_secs(60);
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, CODE);
        store.jobs[&1]
            .reap_state
            .timed_out
            .store(timed_out, Ordering::Release);
        let at_exit = store.jobs[&1].elapsed_secs;

        assert!(store.attach(&lua, 1, None, TEST_PLUGIN, exit_updates(&lua)));

        let (id, event) = store.next_plugin_event().expect("replayed exit");
        assert_eq!(id, 1);
        assert!(matches!(event, JobEvent::Exit(CODE, reported) if reported == timed_out));
        assert!(
            store.next_plugin_event().is_none(),
            "the replay must be served once"
        );

        // Rewind the start so a second round of bookkeeping would show up in
        // the elapsed time instead of hiding in the same second.
        store.jobs.get_mut(&1).unwrap().started = Instant::now() - REWIND;
        deliver(&mut store, &lua, 1, &event);
        assert_eq!(
            store.jobs[&1].elapsed_secs, at_exit,
            "a replayed exit must not restate when the job died"
        );
        assert_eq!(
            armed_slots(&store),
            (false, false, false),
            "completing on the replay must release the callback it used"
        );
    }

    #[test]
    fn attach_is_refused_for_jobs_this_plugin_cannot_see() {
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, 0);

        assert!(!store.attach(&lua, 1, None, "other-plugin", exit_updates(&lua)));
        assert!(!store.attach(&lua, 999, None, TEST_PLUGIN, exit_updates(&lua)));
        assert!(
            store.next_plugin_event().is_none(),
            "a refused attach must not queue a replay"
        );
    }

    #[test]
    fn attach_keeps_absent_callbacks_and_clears_on_false() {
        let lua = Lua::new();
        let mut store = make_store();
        store
            .jobs
            .insert(1, stub_job(plugin_owner(), Some(noop_key(&lua)), None));

        store.attach(
            &lua,
            1,
            None,
            TEST_PLUGIN,
            CallbackUpdates {
                on_stderr: CallbackUpdate::Set(noop_key(&lua)),
                ..keep_all()
            },
        );
        assert_eq!(
            armed_slots(&store),
            (true, true, false),
            "an absent key must keep the callback it found"
        );

        store.attach(
            &lua,
            1,
            None,
            TEST_PLUGIN,
            CallbackUpdates {
                on_stdout: CallbackUpdate::Clear,
                ..keep_all()
            },
        );
        assert_eq!(
            armed_slots(&store),
            (false, true, false),
            "false must clear that callback and leave the rest"
        );
    }

    #[test_case(3, false ; "tail_holds_every_line")]
    #[test_case(2, true ; "tail_evicted_a_line")]
    #[test_case(0, true ; "tail_disabled")]
    fn dropped_says_whether_the_tail_is_the_whole_output(cap: usize, expected: bool) {
        let mut store = make_store();
        store.jobs.insert(1, stub_job(task_owner(1), None, None));
        store.set_tail(1, Some(cap));

        for line in ["a", "b", "c"] {
            store.record_event(1, &JobEvent::Stdout(line.into()));
        }

        assert_eq!(
            store
                .snapshot(1, Some(1), TEST_PLUGIN)
                .unwrap()
                .dropped_output,
            expected
        );
    }

    #[test]
    fn completed_history_is_evicted_per_plugin() {
        const QUIET_PLUGIN: &str = "quiet-plugin";
        const QUIET_JOB: u32 = 1;
        const OLDEST_CHATTY_JOB: u32 = 2;
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        let exit_job = |store: &mut JobStore, id: u32, plugin: &str| {
            let owner = JobOwner::Session {
                session,
                plugin: Arc::from(plugin),
            };
            store.jobs.insert(id, stub_job(owner, None, None));
            deliver(store, &lua, id, &JobEvent::Exit(0, false));
        };

        exit_job(&mut store, QUIET_JOB, QUIET_PLUGIN);
        for id in OLDEST_CHATTY_JOB..=(MAX_COMPLETED_SESSION_JOBS as u32 + OLDEST_CHATTY_JOB) {
            exit_job(&mut store, id, TEST_PLUGIN);
        }

        assert!(
            store.snapshot(QUIET_JOB, None, QUIET_PLUGIN).is_some(),
            "a chatty plugin must not evict another plugin's history"
        );
        assert!(
            store
                .snapshot(OLDEST_CHATTY_JOB, None, TEST_PLUGIN)
                .is_none(),
            "the chatty plugin evicts its own oldest job first"
        );
        assert_eq!(
            store.list(None, None, TEST_PLUGIN).len(),
            MAX_COMPLETED_SESSION_JOBS,
            "the cap counts only this plugin's exited jobs"
        );
    }

    #[test]
    fn list_omits_tails() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        store.record_event(id, &JobEvent::Stdout("hello".into()));
        let listed = store.list(None, Some(1), TEST_PLUGIN);
        assert!(
            listed
                .iter()
                .any(|s| s.id == id && s.stdout_lines.is_empty() && s.stderr_lines.is_empty()),
            "joblist must not clone tails"
        );
        assert_eq!(
            store
                .snapshot(id, Some(1), TEST_PLUGIN)
                .unwrap()
                .stdout_lines,
            ["hello"]
        );
    }

    #[test]
    fn forget_drops_exited_session_jobs_only() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        store.jobs.insert(
            1,
            stub_job(
                JobOwner::Session {
                    session,
                    plugin: Arc::from(TEST_PLUGIN),
                },
                None,
                None,
            ),
        );

        store.forget(&lua, 1, None, TEST_PLUGIN);
        assert!(
            store.snapshot(1, None, TEST_PLUGIN).is_some(),
            "running job must stay"
        );

        deliver(&mut store, &lua, 1, &JobEvent::Exit(0, false));
        assert!(
            store
                .list(Some(session), None, TEST_PLUGIN)
                .iter()
                .any(|s| s.id == 1)
        );

        store.forget(&lua, 1, None, "other-plugin");
        assert!(
            store.snapshot(1, None, TEST_PLUGIN).is_some(),
            "other plugin cannot forget"
        );

        store.forget(&lua, 1, None, TEST_PLUGIN);
        assert!(store.snapshot(1, None, TEST_PLUGIN).is_none());
        assert!(store.list(Some(session), None, TEST_PLUGIN).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn exited_session_job_stays_listed_and_kill_is_a_noop() {
        let lua = Lua::new();
        let session = MakiId::generate();
        let mut store = make_store();
        let id = store
            .start(JobSpec::new(session_owner(session), "sleep 0.1"))
            .unwrap();
        for (_, event) in collect_until_exit(id, || store.next_plugin_event()) {
            store.record_event(id, &event);
        }
        assert!(
            !ReapState::lock(&store.jobs[&id].reap_state).is_enabled(),
            "{EXIT_WITHOUT_REAP}"
        );
        deliver(&mut store, &lua, id, &JobEvent::Exit(0, false));
        store.kill(id, None, TEST_PLUGIN);
        let snap = store
            .snapshot(id, None, TEST_PLUGIN)
            .expect("exited session job must stay inspectable");
        assert_eq!(snap.exit_code, Some(0));
        assert!(
            store
                .list(Some(session), None, TEST_PLUGIN)
                .iter()
                .any(|listed| listed.id == id && listed.exit_code == Some(0)),
            "exited session job must stay listed"
        );
        store.kill_session(&lua, session);
    }
}
