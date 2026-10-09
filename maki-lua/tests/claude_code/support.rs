//! Items shared by the claude_code test binaries.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use maki_agent::ToolOutput;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use rustix::process::{Pid, geteuid, test_kill_process_group};
use serde_json::{Map, Value};
use smol::Timer;
use smol::future::or;

pub const TOOL: &str = "claude_code";
/// The Claude Code version the fakes report: the oldest maki runs.
pub const MINIMUM_VERSION: &str = "2.1.284";
pub const WRITE_SRC: &str = include_str!("../../../plugins/write/init.lua");
pub const BASH_SRC: &str = include_str!("../../../plugins/bash/init.lua");
/// The artifact folder where an import keeps the files it replaced.
pub const ORIGINALS: &str = "originals";
/// The artifact folder holding the worker's copy of the project.
pub const SNAPSHOT_DIR: &str = "snapshot";
pub const NOTHING_IMPORTED: &str = "maki imported no changes";
pub const IMPORT_TOOL: &str = "claude_code_import";
/// How the line naming the artifact starts in a coding call's reply.
pub const ARTIFACT_LINE: &str = "Changes in artifact ";
pub const PROJECT_RULE: &str = "Run the linter before every commit.";
pub const LEFT_BEHIND: &str = "maki did not remove: ";
/// The file a command substitution in a name creates if it runs.
pub const PWNED: &str = "pwned";
pub const OWNER_ONLY: u32 = 0o700;
/// How long a test waits for an event before it counts as hung. Only a hung
/// test waits this long. CI runs 64 tests at once on 4 cores, where a single
/// import has taken over 20 s.
pub const DEADLINE: Duration = Duration::from_secs(120);
/// How often a shell fake checks for its release, and how many checks fit in
/// [`DEADLINE`].
const RELEASE_POLL_MS: u128 = 10;
const RELEASE_LOOKS: u128 = DEADLINE.as_millis() / RELEASE_POLL_MS;
const DOT_GIT: &str = ".git";
const POLL: Duration = Duration::from_millis(10);
/// The user's home and XDG directories in every test process, all in this
/// one shared folder.
const FIXTURE_USER_DIR: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/claude_code_user");
const FIXTURE_CONFIG_DIR: &str = "config";
/// Each variable naming a user directory, with its folder in
/// [`FIXTURE_USER_DIR`].
const USER_DIR_VARS: [(&str, &str); 5] = [
    ("HOME", "home"),
    ("XDG_CONFIG_HOME", FIXTURE_CONFIG_DIR),
    ("XDG_STATE_HOME", "state"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
];
pub const CLAUDE_CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Set by `cargo nextest`, which runs each test in a process of its own.
const NEXTEST: &str = "NEXTEST";
const CLAUDE_DIR: &str = ".claude";
/// maki's global instructions in the XDG config directory.
const GLOBAL_INSTRUCTIONS: &str = "maki/AGENTS.md";
/// These instructions distinguish fixture config from developer config. Live prompts must
/// avoid test labels so the model treats them as ordinary instructions.
pub const FIXTURE_GLOBAL_RULE: &str = "Write the replies in English.";

/// All threads share one working directory, so under a threaded harness
/// this lets only one test at a time move it.
static WORKING_DIR: Mutex<()> = Mutex::new(());

/// Keep only the developer's Claude Code login. Fixture home and XDG directories isolate maki
/// config and instruction files.
static DEVELOPER_LOGIN: LazyLock<PathBuf> = LazyLock::new(|| {
    assert!(
        env::var_os(NEXTEST).is_some(),
        "these tests change the process environment, so run them with cargo nextest, which gives each test its own process"
    );
    let login = env::var_os(CLAUDE_CONFIG_ENV)
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || Path::new(&env::var_os("HOME").expect("HOME is set")).join(CLAUDE_DIR),
            PathBuf::from,
        );
    let root = Path::new(FIXTURE_USER_DIR);
    for (name, folder) in USER_DIR_VARS {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: nextest gives each test its own process. Environment changes precede all
        // test threads. Environment tests also hold a lock.
        unsafe { env::set_var(name, dir) };
    }
    // SAFETY: the same rule as above.
    unsafe { env::remove_var(CLAUDE_CONFIG_ENV) };
    let global = root.join(FIXTURE_CONFIG_DIR).join(GLOBAL_INSTRUCTIONS);
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    // Another test process can read this file. Atomic rename prevents a partial instruction
    // file.
    let staged = global.with_extension(process::id().to_string());
    fs::write(&staged, FIXTURE_GLOBAL_RULE).unwrap();
    fs::rename(&staged, &global).unwrap();
    login
});

/// Moves the process's working directory into a test's directory, and back
/// on drop.
pub struct WorkingDir {
    left: PathBuf,
    _turn: MutexGuard<'static, ()>,
}

impl WorkingDir {
    pub fn enter(dir: &Path) -> Self {
        let turn = WORKING_DIR.lock().unwrap_or_else(PoisonError::into_inner);
        let left = env::current_dir().unwrap();
        env::set_current_dir(dir).unwrap();
        Self { left, _turn: turn }
    }
}

impl Drop for WorkingDir {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.left);
    }
}

/// Root can bypass mode restrictions. Skip tests that depend on those restrictions when
/// they do not apply.
pub fn mode_bits_hold() -> bool {
    !geteuid().is_root()
}

/// Returns git's output when run in `dir` without user or system config, so
/// the developer's config cannot affect a test.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@localhost"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} exited with an error");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Writes `script` to `dir`/`name`, runnable by its owner only.
pub fn executable(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(OWNER_ONLY)).unwrap();
    path
}

pub fn on_path(name: &str) -> PathBuf {
    env::split_paths(&env::var_os("PATH").unwrap())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} on PATH"))
}

/// The entries of `dir`, sorted.
pub fn listing(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    entries
}

/// Returns true if `done` turned true within `limit`.
pub fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(POLL);
    }
    false
}

/// Exclude `.git` because background git maintenance can change it after a commit. Record all
/// other paths and bytes, including symlink targets.
pub fn contents(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                let target = fs::read_link(&path).unwrap();
                found.insert(path, target.into_os_string().into_encoded_bytes());
            } else if meta.is_dir() {
                if path.file_name().is_some_and(|name| name == DOT_GIT) {
                    continue;
                }
                found.insert(path.clone(), Vec::new());
                pending.push(path);
            } else {
                found.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    found
}

/// Returns true if leader `pid`'s process group, including whatever the
/// leader left behind, stopped within `limit`.
pub fn group_gone(pid: i32, limit: Duration) -> bool {
    let pid = Pid::from_raw(pid).expect("valid pid");
    wait_until(limit, || test_kill_process_group(pid).is_err())
}

/// A killed process stays a zombie until its new parent reaps it, and a
/// zombie has an empty cmdline.
pub fn running(pid: &str) -> bool {
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| !cmdline.is_empty())
}

/// Returns `tool`'s reply to `input`: its text, or its error.
pub async fn tool_reply(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    tool: &str,
    input: Value,
) -> Result<String, String> {
    let inv = reg
        .get(tool)
        .unwrap_or_else(|| panic!("{tool} registered"))
        .tool
        .parse(&input)
        .expect("input parses");
    inv.execute(ctx).await.output.map(|out| match out {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("incorrect output: {other:?}"),
    })
}

/// Returns a shell loop that makes a fake wait until `release` exists.
/// `release` is a quoted path or expression. The loop gives up after
/// [`DEADLINE`], so a missing release cannot hang the run.
pub fn wait_for_release(release: &str) -> String {
    format!(
        "n=0; until [ -e {release} ] || [ $n -ge {RELEASE_LOOKS} ]; do sleep 0.{RELEASE_POLL_MS:03}; n=$((n+1)); done"
    )
}

/// Swaps the user's directories for the fixture, once per process. maki
/// fixes its directories the first time it reads them, so this must run
/// before any host starts.
pub fn use_fixture_user() {
    LazyLock::force(&DEVELOPER_LOGIN);
}

/// The developer's Claude Code login, which the live tests hand to Claude
/// Code. Also calls [`use_fixture_user`].
pub fn developer_login() -> &'static Path {
    &DEVELOPER_LOGIN
}

/// Returns a host with the claude_code plugin loaded with `opts`.
pub fn load(opts: Map<String, Value>) -> (Arc<ToolRegistry>, PluginHost) {
    try_load(opts).unwrap()
}

/// Loads the plugin, or returns the error of an option it refuses.
pub fn try_load(opts: Map<String, Value>) -> Result<(Arc<ToolRegistry>, PluginHost), String> {
    use_fixture_user();
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&PluginsConfig {
        names: vec![TOOL.to_owned()],
        opts: HashMap::from([(TOOL.to_owned(), opts)]),
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    Ok((reg, host))
}

/// A lost reply must fail the test so the suite can continue.
pub async fn within<T>(limit: Duration, fut: impl Future<Output = T>) -> T {
    let expired = async {
        Timer::after(limit).await;
        None
    };
    or(async { Some(fut.await) }, expired)
        .await
        .unwrap_or_else(|| panic!("the call did not complete in {limit:?}"))
}
