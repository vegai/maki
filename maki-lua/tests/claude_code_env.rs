//! Environment tests use fresh repositories so checkout settings cannot affect the result.
//! Each test holds the environment lock.
//!
//! nextest gives each test its own process. This also isolates the state directory, which
//! maki selects once per process.
#![cfg(target_os = "linux")]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use maki_agent::tools::test_support::stub_ctx_in;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_lua::PluginHost;
use serde_json::{Map, Value, json};
use tempfile::{TempDir, tempdir};
use test_case::test_case;

/// This binary uses only some of the items shared with the claude_code
/// binary.
#[path = "claude_code/support.rs"]
#[allow(dead_code)]
mod support;

use support::{
    DEADLINE, MINIMUM_VERSION, TOOL, WorkingDir, executable, git, listing, load, on_path,
    tool_reply, use_fixture_user, wait_for_release, within, write,
};

const HELD_REMOVAL: &str =
    "#!/bin/sh\ntouch \"@MARKER@\"\n@WAIT_FOR_RELEASE@\nexec \"@REAL@\" \"$@\"\n";
const VERSION_ONLY: &str = "#!/bin/sh\necho '@VERSION@ (Claude Code)'\n";
/// Answers the handshake like a clean subscription login, in the mode it was
/// started in, and runs no task. It saves its settings, the prompt it gets
/// and each start's environment next to itself.
const HANDSHAKE_ONLY: &str = r#"#!/bin/sh
env >> "$(dirname "$0")/env"
if [ "$1" = --version ]; then echo '@VERSION@ (Claude Code)'; exit 0; fi
flags=; mode=; prev=
for arg; do
  [ "$prev" = --settings ] && flags=$arg
  [ "$prev" = --permission-mode ] && mode=$arg
  prev=$arg
done
[ "$mode" = manual ] && mode=default
printf '%s' "$flags" > "$(dirname "$0")/settings"
reply() { printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":%s}}\n' "$1" "$2"; }
while IFS= read -r msg; do
  id=$(printf '%s' "$msg" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  case "$msg" in
    *'"type":"user"'*) printf '%s\n' "$msg" > "$(dirname "$0")/prompt" ;;
    *'"initialize"'*) reply "$id" "{\"current_permission_mode\":\"$mode\",\"account\":{\"apiProvider\":\"firstParty\",\"subscriptionType\":\"Claude Pro\"},\"models\":[{\"value\":\"sonnet\",\"resolvedModel\":\"claude-sonnet-5\"}]}" ;;
    *'"get_settings"'*) reply "$id" "{\"effective\":$flags,\"sources\":[{\"source\":\"flagSettings\",\"settings\":$flags}]}" ;;
    *'"get_hooks_listing"'*) reply "$id" '{"hooks":[],"events":[],"policy":{"allDisabled":true,"policyHookCount":0}}' ;;
  esac
done
"#;
/// Refuses to clone with the given cause, naming itself by its full path as a
/// `cp` run from a path does, and copies normally otherwise.
const CLONE_REFUSING_CP: &str = "#!/bin/sh\ncase \" $* \" in *\" --reflink=always \"*) echo \"$0: failed to clone 'x' from 'y': @REFUSED@\" >&2; exit 1 ;; esac\nexec \"@REAL@\" \"$@\"\n";
const FULL_COPY: &str = "as a full copy, because the filesystem cannot clone it";
const NO_CLONE: &str = "maki cannot clone the dependency";
/// Copy the project first, then run @ACTION@ at @MARKER@ to simulate a concurrent edit.
const DISTURBING_CP: &str = "#!/bin/sh\n\"@REAL@\" \"$@\" || exit\ncase \" $* \" in *\" --parents \"*) cd \"@MARKER@\" && @ACTION@ ;; esac\n";
const EDIT_FILE: &str = "echo edited >> src/lib.rs";
const MAKE_EXECUTABLE: &str = "chmod +x src/lib.rs";
const RETARGET_LINK: &str = "ln -sfn other.rs src/link";
const DEPENDENCY_DIR: &str = "node_modules";
const DEPENDENCY_FILE: &str = "node_modules/pkg/index.js";
const EDIT_DEPENDENCY: &str = "echo edited >> node_modules/pkg/index.js";
const DEPENDENCY_UNSETTLED: &str = "dependencies changed during the copy";
const WORKER_TOLD_UNSETTLED: &str = "Some dependencies changed while maki copied them";
const CHANGED_DURING_COPY: &str = "the checkout changed during the snapshot: ";
const INSIDE_PROJECT: &str = "is in the project";
/// How the refusal names `TMPDIR`, before any file is made there.
const TEMP_DIR: &str = "the temporary directory";
const TMPDIR: &str = "TMPDIR";
const HOME: &str = "HOME";
const XDG_STATE_HOME: &str = "XDG_STATE_HOME";
const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const API_KEY: &str = "ANTHROPIC_API_KEY";
const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// An artifact's repository, next to its snapshot.
const GIT_DIR: &str = "git";
/// Where artifacts go in maki's state directory when `artifact_dir` is
/// unset.
const DEFAULT_ARTIFACT_ROOT: &str = "claude_code/changes";
const PATH: &str = "PATH";
/// A probe's error when the fake answers it with its version line.
const NO_EVENT_STOP: &str = "printed a line that is not an event";
/// The plugin's names for an artifact's files.
const ARTIFACT_MARKER: &str = ".maki-claude-code-artifact";
const MANIFEST: &str = "manifest.json";
/// Older than the default artifact time limit of a day.
const STALE_AGE: Duration = Duration::from_secs(48 * 60 * 60);

/// Each test holds this lock while it runs, because these tests change the
/// whole process's environment and a threaded harness would run them at
/// once.
static PROCESS: Mutex<()> = Mutex::new(());

/// A test's turn on the process: inside an empty repository, with the
/// fixture as the user's directories, until it drops.
struct Scenario {
    _in_project: WorkingDir,
    _project: TempDir,
    _turn: MutexGuard<'static, ()>,
}

impl Scenario {
    fn enter() -> Self {
        let turn = PROCESS.lock().unwrap_or_else(PoisonError::into_inner);
        use_fixture_user();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        Self {
            _in_project: WorkingDir::enter(project.path()),
            _project: project,
            _turn: turn,
        }
    }
}

/// Sets a variable for one test and restores the old value, even when an
/// assertion unwinds, so the next test starts with the process's value.
struct EnvVar {
    name: &'static str,
    before: Option<OsString>,
}

impl EnvVar {
    fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let before = env::var_os(name);
        // SAFETY: `Scenario::enter` enforces a separate nextest process. Each plugin host
        // starts after the environment change.
        unsafe { env::set_var(name, value) };
        Self { name, before }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: the same as in `set`.
        unsafe {
            match &self.before {
                Some(value) => env::set_var(self.name, value),
                None => env::remove_var(self.name),
            }
        }
    }
}

/// Wraps the installed `name` with `template` in `dir`, and returns a guard
/// that puts `dir` first on the `PATH`.
fn hold(dir: &Path, name: &str, template: &str, marker: &Path, release: &Path) -> EnvVar {
    let real = on_path(name);
    let script = template
        .replace("@REAL@", &real.to_string_lossy())
        .replace("@MARKER@", &marker.to_string_lossy())
        .replace("@WAIT_FOR_RELEASE@", &wait_for_release("\"@RELEASE@\""))
        .replace("@RELEASE@", &release.to_string_lossy());
    executable(dir, name, &script);
    path_first(dir)
}

/// Puts `dir` first on the `PATH` until the guard drops.
fn path_first(dir: &Path) -> EnvVar {
    let path = env::var_os(PATH).unwrap();
    let entries = [dir.to_path_buf()]
        .into_iter()
        .chain(env::split_paths(&path));
    EnvVar::set(PATH, env::join_paths(entries).unwrap())
}

/// A fake `claude` in `dir` that runs `script` and reports the minimum
/// version.
fn fake_claude(dir: &Path, script: &str) -> PathBuf {
    executable(dir, "claude", &script.replace("@VERSION@", MINIMUM_VERSION))
}

/// Keep the host alive until call cleanup completes. Host shutdown kills all jobs,
/// including cleanup processes.
fn fake_host(claude: &Path, config_dir: &Path) -> (Arc<ToolRegistry>, PluginHost) {
    fake_host_with(claude, config_dir, Map::new())
}

fn fake_host_with(
    claude: &Path,
    config_dir: &Path,
    mut opts: Map<String, Value>,
) -> (Arc<ToolRegistry>, PluginHost) {
    opts.insert("executable".into(), json!(claude));
    opts.insert("config_dir".into(), json!(config_dir));
    load(opts)
}

/// A tool context for a session working in these tests' project, which is
/// the process's working directory here.
fn session_ctx() -> ToolContext {
    stub_ctx_in(&env::current_dir().unwrap(), None, None)
}

fn ask(reg: &ToolRegistry, ctx: &ToolContext) -> Result<String, String> {
    ask_with(reg, ctx, json!({ "prompt": "anything" }))
}

fn ask_with(reg: &ToolRegistry, ctx: &ToolContext, input: Value) -> Result<String, String> {
    smol::block_on(within(DEADLINE, tool_reply(reg, ctx, TOOL, input)))
}

/// A `TMPDIR` that leads into the project, directly or through a symlink
/// from outside it, is refused before any file is made there. The fake
/// answers only `--version`, so no probe can run, even after an error.
#[test]
fn a_temp_dir_that_leads_into_the_project_is_refused() {
    let _scenario = Scenario::enter();
    let project = env::current_dir().unwrap().canonicalize().unwrap();
    let outside = tempdir().unwrap();
    let claude = fake_claude(outside.path(), VERSION_ONLY);
    let link = outside.path().join("tmp");
    symlink(&project, &link).unwrap();
    let before = listing(&project);

    for tmpdir in [&project, &link] {
        let _tmpdir = EnvVar::set(TMPDIR, tmpdir);
        let (reg, _host) = fake_host(&claude, outside.path());
        let err = ask(&reg, &session_ctx()).unwrap_err();
        assert!(err.contains(INSIDE_PROJECT), "got: {err}");
        assert_eq!(
            listing(&project),
            before,
            "maki must make no file in the project"
        );
    }
}

/// Artifact cleanup can take a long time. It must run outside the Lua thread so subsequent
/// calls can obtain slots.
#[test]
fn a_call_does_not_wait_for_the_sweep() {
    let _scenario = Scenario::enter();
    let tools = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let artifacts = tempdir().unwrap();
    let project = repo.path().canonicalize().unwrap();
    write(&project.join("src/lib.rs"), "base\n");
    git(&project, &["init", "-q"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "base"]);
    let old = artifacts.path().join("old");
    fs::create_dir(&old).unwrap();
    for name in [ARTIFACT_MARKER, MANIFEST] {
        let file = old.join(name);
        fs::write(&file, "{}").unwrap();
        File::open(&file)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_AGE)
            .unwrap();
    }
    let removing = tools.path().join("removing");
    let never = tools.path().join("never");
    let _path = hold(tools.path(), "rm", HELD_REMOVAL, &removing, &never);
    let claude = fake_claude(tools.path(), HANDSHAKE_ONLY);

    let mut opts = Map::new();
    opts.insert("artifact_dir".into(), json!(artifacts.path()));
    let (reg, _host) = fake_host_with(&claude, tools.path(), opts);
    let input = json!({ "prompt": "anything", "profile": "code" });
    let _ = ask_with(&reg, &stub_ctx_in(&project, None, None), input);
    assert!(removing.exists(), "the sweep did not start");
    assert!(old.exists(), "the call waited for the sweep");
}

/// From a subdirectory, the project is still the whole checkout, so a
/// `TMPDIR` elsewhere in it is refused too.
#[test]
fn a_temp_dir_elsewhere_in_the_checkout_is_refused() {
    let _scenario = Scenario::enter();
    let project = env::current_dir().unwrap().canonicalize().unwrap();
    let sub = project.join("sub");
    let elsewhere = project.join("elsewhere");
    fs::create_dir(&sub).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    let outside = tempdir().unwrap();
    let claude = fake_claude(outside.path(), VERSION_ONLY);
    let _tmpdir = EnvVar::set(TMPDIR, &elsewhere);

    let (reg, _host) = fake_host(&claude, outside.path());
    let err = ask(&reg, &stub_ctx_in(&sub, None, None)).unwrap_err();
    let refused = format!("{TEMP_DIR} {} {INSIDE_PROJECT}", elsewhere.display());
    assert!(err.contains(&refused), "got: {err}");
    assert!(
        listing(&elsewhere).is_empty(),
        "maki must make no file there"
    );
}

#[test]
fn probe_cleanup_needs_no_external_helpers() {
    let _scenario = Scenario::enter();
    let tools = tempdir().unwrap();
    let base = tempdir().unwrap();
    let claude = fake_claude(tools.path(), VERSION_ONLY);
    let _path = EnvVar::set(PATH, tools.path());
    let _tmpdir = EnvVar::set(TMPDIR, base.path());
    let (reg, _host) = fake_host(&claude, tools.path());
    let err = ask(&reg, &session_ctx()).unwrap_err();
    assert!(err.contains(NO_EVENT_STOP), "got: {err}");
    assert!(listing(base.path()).is_empty());
}

/// Concurrent file changes can produce a snapshot state that never existed. Refuse that
/// snapshot and remove its artifact.
#[test_case(EDIT_FILE, "src/lib.rs" ; "an_edit")]
#[test_case(MAKE_EXECUTABLE, "src/lib.rs" ; "an_executable_bit")]
#[test_case(RETARGET_LINK, "src/link" ; "a_link_target")]
fn a_change_during_the_copy_is_refused(action: &str, disturbed: &str) {
    let _scenario = Scenario::enter();
    let copy = code_while_the_copy_is_disturbed_by(action, "");
    let want = format!("{CHANGED_DURING_COPY}{disturbed}");
    assert!(copy.err.contains(&want), "got: {}", copy.err);
    assert_eq!(
        copy.artifacts_left,
        Vec::<PathBuf>::new(),
        "the rejected snapshot stayed"
    );
}

/// Dependency trees are too large for file comparisons. Report concurrent changes so a
/// development server can update its cache without repeated call failures.
#[test]
fn a_dependency_changed_during_the_copy_is_reported() {
    let _scenario = Scenario::enter();
    let copy = code_while_the_copy_is_disturbed_by(EDIT_DEPENDENCY, DEPENDENCY_DIR);
    assert!(
        copy.err.contains(DEPENDENCY_UNSETTLED) && copy.err.contains(DEPENDENCY_FILE),
        "got: {}",
        copy.err
    );
    assert!(
        copy.prompt.contains(WORKER_TOLD_UNSETTLED),
        "got: {}",
        copy.prompt
    );
}

/// maki makes a full copy of a dependency only when the filesystem cannot
/// clone it, and says why. A different cause, such as a full disk, stops the
/// call.
#[test_case("Operation not supported", FULL_COPY ; "on_a_filesystem_without_clones")]
#[test_case("Invalid cross-device link", FULL_COPY ; "across_filesystems")]
#[test_case("No space left on device", NO_CLONE ; "on_a_full_disk")]
fn a_failed_clone_says_why(cause: &str, outcome: &str) {
    let _scenario = Scenario::enter();
    let copy = code_with_cp(
        &CLONE_REFUSING_CP.replace("@REFUSED@", cause),
        DEPENDENCY_DIR,
    );
    for text in [outcome, cause] {
        assert!(copy.err.contains(text), "{text} missing from: {}", copy.err);
    }
}

struct DisturbedCopy {
    err: String,
    prompt: String,
    artifacts_left: Vec<PathBuf>,
}

/// Claude Code passes its environment to its shell, so with a login token in
/// maki's environment a coding call stops before Claude Code starts.
#[test]
fn a_login_token_in_the_environment_keeps_the_coding_worker_from_starting() {
    let _scenario = Scenario::enter();
    let tools = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let project = repo.path().canonicalize().unwrap();
    write(&project.join("src/lib.rs"), "base\n");
    git(&project, &["init", "-q"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "base"]);
    let _token = EnvVar::set(OAUTH_TOKEN, "not-a-real-token");
    let claude = fake_claude(tools.path(), HANDSHAKE_ONLY);

    let (reg, _host) = fake_host(&claude, tools.path());
    let input = json!({ "prompt": "anything", "profile": "code" });
    let err = ask_with(&reg, &stub_ctx_in(&project, None, None), input).unwrap_err();

    assert!(err.contains(OAUTH_TOKEN), "got: {err}");
    assert!(
        !tools.path().join("settings").exists(),
        "Claude Code started"
    );
}

/// An API key or another API address would move the requests off the
/// subscription, so maki gives neither to Claude Code.
#[test]
fn an_api_key_and_address_never_reach_claude_code() {
    let _scenario = Scenario::enter();
    let tools = tempdir().unwrap();
    let _key = EnvVar::set(API_KEY, "not-a-real-key");
    let _url = EnvVar::set(BASE_URL, "http://127.0.0.1:9");
    let claude = fake_claude(tools.path(), HANDSHAKE_ONLY);

    let (reg, _host) = fake_host(&claude, tools.path());
    let _ = ask(&reg, &session_ctx());

    let child_env = fs::read_to_string(tools.path().join("env")).unwrap();
    assert!(
        fs::exists(tools.path().join("prompt")).unwrap(),
        "the task must reach Claude Code"
    );
    for name in [API_KEY, BASE_URL] {
        assert!(
            !child_env
                .lines()
                .any(|line| line.starts_with(&format!("{name}="))),
            "{name} reached Claude Code:\n{child_env}"
        );
    }
}

/// Without `artifact_dir`, the artifact root sits in maki's state directory.
/// The sandbox mounts an empty filesystem over both and reopens only the
/// worker's git directory. The call proceeds until it sends the task.
#[test]
fn the_default_artifact_root_hides_all_of_makis_state_but_the_workers_artifact() {
    let _scenario = Scenario::enter();
    let tools = tempdir().unwrap();
    let home = tempdir().unwrap();
    let state_home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let project = repo.path().canonicalize().unwrap();
    write(&project.join("src/lib.rs"), "base\n");
    git(&project, &["init", "-q"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "base"]);
    let _home = EnvVar::set(HOME, home.path());
    let _state_home = EnvVar::set(XDG_STATE_HOME, state_home.path());
    let state_dir = maki_storage::paths::state_dir().unwrap();
    assert!(
        state_dir.starts_with(state_home.path()),
        "maki fixed its paths before this test set them: {state_dir:?}, so the test needs its own process, as nextest gives it"
    );
    let claude = fake_claude(tools.path(), HANDSHAKE_ONLY);

    let (reg, _host) = fake_host_with(&claude, tools.path(), Map::new());
    let input = json!({ "prompt": "anything", "profile": "code" });
    let _ = ask_with(&reg, &stub_ctx_in(&project, None, None), input);

    assert!(
        tools.path().join("prompt").exists(),
        "maki did not send the task"
    );
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(tools.path().join("settings")).unwrap()).unwrap();
    let filesystem = &settings["sandbox"]["filesystem"];
    let root = state_dir
        .join(DEFAULT_ARTIFACT_ROOT)
        .canonicalize()
        .unwrap();
    for hidden in [&state_dir, &root] {
        assert!(
            filesystem["denyRead"]
                .as_array()
                .unwrap()
                .contains(&json!(hidden)),
            "the sandbox must deny all of {hidden:?}: {filesystem}"
        );
    }
    let opened = filesystem["allowRead"].as_array().unwrap();
    let in_root = |path: &Value| {
        let parts: Vec<_> = Path::new(path.as_str().unwrap())
            .strip_prefix(&root)
            .unwrap()
            .iter()
            .collect();
        parts.len() == 2 && parts[1] == GIT_DIR
    };
    assert!(
        matches!(opened.as_slice(), [git] if in_root(git)),
        "only the git directory of the worker can be open again: {filesystem}"
    );
}

fn code_while_the_copy_is_disturbed_by(action: &str, dependencies: &str) -> DisturbedCopy {
    code_with_cp(&DISTURBING_CP.replace("@ACTION@", action), dependencies)
}

fn code_with_cp(cp: &str, dependencies: &str) -> DisturbedCopy {
    let tools = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let artifacts = tempdir().unwrap();
    let project = repo.path().canonicalize().unwrap();
    write(&project.join("src/lib.rs"), "base\n");
    write(&project.join("src/other.rs"), "other\n");
    symlink("lib.rs", project.join("src/link")).unwrap();
    git(&project, &["init", "-q"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "base"]);
    write(&project.join(DEPENDENCY_FILE), "module.exports = 1;\n");
    let _path = hold(
        tools.path(),
        "cp",
        cp,
        &project,
        &tools.path().join("unused"),
    );
    let claude = fake_claude(tools.path(), HANDSHAKE_ONLY);

    let mut opts = Map::new();
    opts.insert("artifact_dir".into(), json!(artifacts.path()));
    opts.insert("dependencies".into(), json!(dependencies));
    let (reg, _host) = fake_host_with(&claude, tools.path(), opts);
    let input = json!({ "prompt": "anything", "profile": "code" });
    let err = ask_with(&reg, &stub_ctx_in(&project, None, None), input).unwrap_err();
    DisturbedCopy {
        err,
        prompt: fs::read_to_string(tools.path().join("prompt")).unwrap_or_default(),
        artifacts_left: listing(artifacts.path()),
    }
}
