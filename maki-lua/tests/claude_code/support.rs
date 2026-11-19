//! Items shared by the claude_code test binaries.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Arc, LazyLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{CancelToken, CancelTrigger, ToolOutput};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use rustix::process::geteuid;
use serde_json::{Map, Value};

pub use maki_providers::claude_code::worker::MINIMUM_VERSION;

pub use super::common::{Gate, environment, group_gone, wait_until};

pub const DEPENDENCY_DIR: &str = "node_modules";
pub const DEPENDENCY_FILE: &str = "node_modules/pkg/index.js";
pub const DEPENDENCY_UNSETTLED: &str = "dependencies changed during the copy";
pub const ARTIFACT_MARKER: &str = ".maki-claude-code-artifact";
pub const MANIFEST: &str = "manifest.json";
pub const STALE_AGE: Duration = Duration::from_hours(48);
pub const CONTROL_REPLY: &str = r#"reply() { printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":%s}}\n' "$1" "$2"; }"#;
pub const ACCOUNT_SUBSCRIPTION: &str = r#"{"current_permission_mode":"default","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro"},"models":[{"value":"sonnet","resolvedModel":"claude-sonnet-5"},{"value":"opus","resolvedModel":"claude-opus-5-5"},{"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001"}]}"#;
pub const HOOKS_OFF: &str =
    r#"{"hooks":[],"events":[],"policy":{"allDisabled":true,"policyHookCount":0}}"#;
pub const SWEEP_LOCK: &str = ".sweep.lock";
pub const TOOL: &str = "claude_code";
pub const CODE_PROFILE: &str = "code";
pub const WRITE_SRC: &str = include_str!("../../../plugins/write/init.lua");
pub const BASH_SRC: &str = include_str!("../../../plugins/bash/init.lua");
const PLUGIN_SRC: &str = include_str!("../../../plugins/claude_code/init.lua");
/// The artifact folder where an import keeps the files it replaced.
pub const ORIGINALS: &str = "originals";
/// The artifact folder where an import moves each file it replaced.
pub const DISPLACED: &str = "displaced";
/// The artifact folder holding the worker's copy of the project.
pub const SNAPSHOT_DIR: &str = "snapshot";
/// An artifact's repository, next to its snapshot.
pub const ARTIFACT_GIT: &str = "git";
/// An artifact's temporary folder, the worker's `TMPDIR`.
pub const ARTIFACT_TMP: &str = "tmp";
/// The prefix of every temporary file an import writes.
pub const IMPORT_TEMP: &str = ".maki-import-";
pub const NOTHING_IMPORTED: &str = "maki imported no changes";
pub const IMPORT_TOOL: &str = "claude_code_import";
/// How the line naming the artifact starts in a coding call's reply.
pub const ARTIFACT_LINE: &str = "Changes in artifact ";
pub const PROJECT_RULE: &str = "Run the linter before every commit.";
pub const LEFT_BEHIND: &str = "maki did not remove: ";
/// What a test writes into the checkout over a file an import is about to
/// replace.
pub const NEWER_WORK: &str = "the user's newer work\n";
/// The file the command substitution in a hostile name creates, in the
/// test's own folder, if a shell runs it.
pub const PWNED: &str = "pwned";
pub const DEPENDENCY_CONTENT: &str = "module.exports = 1;\n";
pub const EXECUTE_BITS: u32 = 0o111;
pub const OWNER_ONLY: u32 = 0o700;
/// How long a test waits for an event before it counts as hung. Only a hung
/// test waits this long. CI runs 64 tests at once on 4 cores, where a single
/// import has taken over 20 s.
pub const DEADLINE: Duration = Duration::from_secs(120);
const DOT_GIT: &str = ".git";
const MODE_BITS_SKIP: &str = "skipping mode-bit assertion: root bypasses filesystem permissions";
/// The user's home and XDG directories in every test process, all in this
/// one shared folder.
const FIXTURE_USER_DIR: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/claude_code_user");
const FIXTURE_CONFIG_DIR: &str = "config";
/// Each variable naming a user directory, with its folder in
/// [`FIXTURE_USER_DIR`].
pub const HOME_ENV: &str = "HOME";
const USER_DIR_VARS: [(&str, &str); 5] = [
    (HOME_ENV, "home"),
    ("XDG_CONFIG_HOME", FIXTURE_CONFIG_DIR),
    ("XDG_STATE_HOME", "state"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
];
pub const CLAUDE_CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";
const CLAUDE_DIR: &str = ".claude";
/// maki's global instructions in the XDG config directory.
const GLOBAL_INSTRUCTIONS: &str = "maki/AGENTS.md";
/// These instructions distinguish fixture config from developer config. Live prompts must
/// avoid test labels so the model treats them as ordinary instructions.
pub const FIXTURE_GLOBAL_RULE: &str = "Write the replies in English.";

/// Keep only the developer's Claude Code login. Fixture home and XDG directories isolate maki
/// config and instruction files.
static DEVELOPER_LOGIN: LazyLock<PathBuf> = LazyLock::new(|| {
    environment::require_nextest();
    let login = env::var_os(CLAUDE_CONFIG_ENV)
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || Path::new(&env::var_os(HOME_ENV).expect("HOME is set")).join(CLAUDE_DIR),
            PathBuf::from,
        );
    let root = Path::new(FIXTURE_USER_DIR);
    for (name, folder) in USER_DIR_VARS {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        environment::set(name, dir);
    }
    environment::remove(CLAUDE_CONFIG_ENV);
    let global = root.join(FIXTURE_CONFIG_DIR).join(GLOBAL_INSTRUCTIONS);
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    // Another test process can read this file. Rename prevents partial instructions.
    let staged = global.with_extension(process::id().to_string());
    fs::write(&staged, FIXTURE_GLOBAL_RULE).unwrap();
    fs::rename(&staged, &global).unwrap();
    login
});

pub fn cancel_trigger(ctx: &mut ToolContext) -> CancelTrigger {
    let (trigger, token) = CancelToken::new();
    ctx.cancel = token;
    trigger
}

pub fn cancel_after<T: Send + 'static>(
    ctx: &mut ToolContext,
    work: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    let trigger = cancel_trigger(ctx);
    thread::spawn(move || {
        let result = work();
        trigger.cancel();
        result
    })
}

pub fn backdate(path: &Path) {
    File::open(path)
        .unwrap()
        .set_modified(SystemTime::now() - STALE_AGE)
        .unwrap();
}

/// Runs `touch pwned` when a shell expands it. `printf` writes each `/`,
/// which a file name cannot hold. `pwned` is absolute, so the file shows
/// wherever the command ran.
pub fn touch_pwned(pwned: &Path) -> String {
    let escaped = pwned.to_string_lossy().replace('/', "\\057");
    format!("$(touch $(printf '{escaped}'))")
}

/// Moves the process's working directory into a test's directory, and back
/// on drop.
pub struct WorkingDir {
    left: PathBuf,
}

impl WorkingDir {
    pub fn enter(dir: &Path) -> Self {
        environment::require_nextest();
        let left = env::current_dir().unwrap();
        env::set_current_dir(dir).unwrap();
        Self { left }
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
    let applies = !geteuid().is_root();
    if !applies {
        eprintln!("{MODE_BITS_SKIP}");
    }
    applies
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

pub fn commit_all(project: &Path) {
    git(project, &["add", "-A"]);
    git(project, &["commit", "-q", "-m", "base"]);
}

/// Makes `project` a repository whose one commit holds `files` and whatever
/// else is there.
pub fn commit_repo(project: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        write(&project.join(path), content);
    }
    git(project, &["init", "-q"]);
    commit_all(project);
}

/// The id of the artifact a coding call's reply names.
pub fn artifact_id(reply: &str) -> String {
    let after = reply
        .split(ARTIFACT_LINE)
        .nth(1)
        .unwrap_or_else(|| panic!("there is no artifact in: {reply}"));
    after.split(',').next().unwrap().to_owned()
}

/// A folder in `root` holding `files`, all as old as a stale artifact.
pub fn old_folder(root: &Path, name: &str, files: &[&str]) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir(&dir).unwrap();
    for file in files {
        let path = dir.join(file);
        fs::write(&path, "{}").unwrap();
        backdate(&path);
    }
    backdate(&dir);
    dir
}

pub fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Writes `script` to `dir`/`name`, runnable by its owner only.
pub fn executable(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    let replacement = dir.join(format!(".{name}.new"));
    let script = script
        .replace("@CONTROL_REPLY@", CONTROL_REPLY)
        .replace("@ACCOUNT_SUBSCRIPTION@", ACCOUNT_SUBSCRIPTION)
        .replace("@HOOKS_OFF@", HOOKS_OFF);
    fs::write(&replacement, script).unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(OWNER_ONLY)).unwrap();
    fs::rename(replacement, &path).unwrap();
    path
}

/// Writes `script` to `dir`/`name` like [`executable`], with `@REAL@` as the
/// installed `name`.
pub fn wrap(dir: &Path, name: &str, script: &str) {
    executable(
        dir,
        name,
        &script.replace("@REAL@", &on_path(name).to_string_lossy()),
    );
}

/// Puts `script`, wrapping the installed `name`, first on the `PATH`.
pub fn hold(dir: &Path, name: &str, script: &str) {
    wrap(dir, name, script);
    environment::prepend_path(dir);
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

pub fn artifact_listing(dir: &Path) -> Vec<PathBuf> {
    listing(dir)
        .into_iter()
        .filter(|path| path.file_name() != Some(SWEEP_LOCK.as_ref()))
        .collect()
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

pub fn reply(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    tool: &str,
    input: Value,
) -> Result<String, String> {
    output(tool_reply(reg, ctx, tool, input))
}

pub fn output<T>(work: impl Future<Output = T>) -> T {
    maki_lua::test_support::runtime().block_on(within(DEADLINE, work))
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

/// Returns a host with the claude_code plugin loaded with `opts`, after
/// `prefix` ran in the plugin's environment.
pub fn load_patched(prefix: &str, opts: Map<String, Value>) -> (Arc<ToolRegistry>, PluginHost) {
    use_fixture_user();
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_bundled_source(TOOL, &format!("{prefix}\n{PLUGIN_SRC}"), opts)
        .unwrap();
    (reg, host)
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
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or_else(|_| panic!("the call did not complete in {limit:?}"))
}
