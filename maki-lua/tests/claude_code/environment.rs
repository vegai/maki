//! Environment tests use fresh repositories so checkout settings cannot affect the result.
//!
//! nextest gives each test its own process. This also isolates the state directory, which
//! maki selects once per process.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use maki_providers::claude_code::{DEFAULT_EXECUTABLE, DEFAULT_MAX_CONCURRENT, MIN_MAX_CONCURRENT};
use serde_json::{Map, Value, json};
use tempfile::tempdir;
use test_case::test_case;

use super::policy::{ANSWER, ANSWERS, FakeClaude, NO_SCENARIO, code_with, sandbox_filesystem};
use super::support::{
    ARTIFACT_GIT, ARTIFACT_MARKER, CLAUDE_CONFIG_ENV, DEADLINE, DEPENDENCY_CONTENT, DEPENDENCY_DIR,
    DEPENDENCY_FILE, DEPENDENCY_UNSETTLED, Gate, HOME_ENV, MANIFEST, TOOL, WorkingDir,
    artifact_listing, commit_repo, contents, environment, hold, listing, load, old_folder,
    wait_until, write,
};

const HELD_REMOVAL: &str = "#!/bin/sh\ntouch \"@MARKER@\"\n@WAIT@\nexec \"@REAL@\" \"$@\"\n";
const VERSION_ONLY: &str = "#!/bin/sh\necho '@VERSION@ (Claude Code)'\n";
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
const EDIT_DEPENDENCY: &str = "echo edited >> node_modules/pkg/index.js";
const CHANGED_DURING_COPY: &str = "the checkout changed during the snapshot: ";
/// The checkout's tracked file.
const LIB: (&str, &str) = ("src/lib.rs", "base\n");
/// How the refusal names `TMPDIR`, before any file is made there.
const TMPDIR: &str = "TMPDIR";
const HOME_LINK: &str = "linked-home";
const HOME_DENY_PATHS: [&str; 4] = [
    ".ssh",
    ".gitconfig",
    "custom/token",
    ".claude.json.tmp.42.random",
];
const SECRET_DIR: &str = "credentials";
const LOGIN_LINK: &str = "usual-login";
const XDG_STATE_HOME: &str = "XDG_STATE_HOME";
const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const HTTPS_PROXY: &str = "HTTPS_PROXY";
const TEST_LOGIN_TOKEN: &str = "not-a-real-token";
const TEST_PROXY_LOGIN: &str = "http://user:not-a-real-password@proxy:3128/";
const API_KEY: &str = "ANTHROPIC_API_KEY";
const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// Where artifacts go in maki's state directory when `artifact_dir` is
/// unset.
const DEFAULT_ARTIFACT_ROOT: &str = "claude_code/changes";
const PATH: &str = "PATH";
/// A probe's error when the fake answers it with its version line.
const NO_EVENT_STOP: &str = "printed a line that is not an event";
/// Where a sweep moves the artifacts it removes.
const SWEEP_TRASH: &str = ".trash-old";
const NO_STATE_DIR: &str =
    "maki cannot find its state directory as a UTF-8 path for the coding sandbox";
const STARTUP_STDERR: &str = "fixture: unsupported startup flag";
const STARTUP_ERROR_CODE: i32 = 73;

#[test]
fn the_plugin_schema_uses_the_provider_defaults() {
    let (_registry, host) = load(Map::new());
    let schemas = host.plugin_options().unwrap();
    let schema = schemas.get(TOOL).unwrap();
    for (name, default) in [
        ("executable", json!(DEFAULT_EXECUTABLE)),
        ("config_dir", json!("")),
        ("provider_max_concurrent", json!(DEFAULT_MAX_CONCURRENT)),
    ] {
        let option = schema.iter().find(|option| option.name == name).unwrap();
        assert_eq!(option.default.as_ref(), Some(&default), "{name}");
        if name == "provider_max_concurrent" {
            assert_eq!(option.min, Some(MIN_MAX_CONCURRENT as f64));
        }
    }
}

fn new_dir(dir: PathBuf) -> PathBuf {
    fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn coding_refuses_a_state_directory_it_cannot_deny() {
    let fake = FakeClaude::new();
    let outside = tempdir().unwrap();
    let state = new_dir(outside.path().join(OsStr::from_bytes(b"state-\xff")));
    environment::set(XDG_STATE_HOME, &state);
    let artifacts = outside.path().join("artifacts");
    let (reg, _host) = load(fake.opts(&[("artifact_dir", json!(artifacts))]));
    let err = code_with(&reg, &fake.ctx(None, None), NO_SCENARIO, &[]).unwrap_err();
    assert!(err.contains(NO_STATE_DIR), "{err}");
    assert!(artifact_listing(&artifacts).is_empty());
}

/// A scratch base must be outside the whole checkout, including siblings of
/// the session directory, so maki gives way to a safe fallback for a
/// `TMPDIR` it cannot use.
#[test_case(|_: &Path, _: &Path| PathBuf::new(), false ; "empty")]
#[test_case(|_: &Path, outside: &Path| new_dir(outside.join(OsStr::from_bytes(b"scratch-\xff"))), false ; "not_utf8")]
#[test_case(|project: &Path, _: &Path| project.to_owned(), false ; "the_project")]
#[test_case(|project: &Path, outside: &Path| { let link = outside.join("tmp"); symlink(project, &link).unwrap(); link }, false ; "a_link_to_the_project")]
#[test_case(|project: &Path, _: &Path| new_dir(project.join("elsewhere")), true ; "elsewhere_in_the_checkout")]
fn an_unusable_tmpdir_gets_a_safe_fallback(
    tmpdir: fn(&Path, &Path) -> PathBuf,
    from_subdirectory: bool,
) {
    let fake = FakeClaude::new();
    let project = fake.project.path();
    let outside = tempdir().unwrap();
    environment::set(TMPDIR, tmpdir(&project, outside.path()));
    let session = if from_subdirectory {
        new_dir(project.join("sub"))
    } else {
        project.clone()
    };
    let before = contents(&project);
    assert_eq!(fake.ask_in(ANSWERS, &session).unwrap(), ANSWER);
    let tmpdirs = fake.log("tmpdirs");
    assert!(!tmpdirs.is_empty());
    for used in tmpdirs.lines().map(Path::new) {
        assert!(
            used.is_absolute() && used.is_dir() && !used.starts_with(&project),
            "{used:?}"
        );
    }
    assert_eq!(contents(&project), before);
}

/// Artifact cleanup can take a long time. It must run outside the Lua thread so subsequent
/// calls can obtain slots.
#[test]
fn a_call_does_not_wait_for_the_sweep() {
    let fake = FakeClaude::new();
    commit_repo(&fake.project.path(), &[LIB]);
    let artifacts = tempdir().unwrap();
    let old = old_folder(artifacts.path(), "old", &[ARTIFACT_MARKER, MANIFEST]);
    let tools = tempdir().unwrap();
    let removing = tools.path().join("removing");
    let release = Gate::closed();
    let held = HELD_REMOVAL
        .replace("@MARKER@", &removing.to_string_lossy())
        .replace("@WAIT@", &release.wait());
    hold(tools.path(), "rm", &held);

    let (reg, _host) = load(fake.opts(&[("artifact_dir", json!(artifacts.path()))]));
    let _ = code_with(&reg, &fake.ctx(None, None), NO_SCENARIO, &[]);
    assert!(
        wait_until(DEADLINE, || removing.exists()),
        "the sweep did not start"
    );
    assert!(
        !old.exists(),
        "expiry must first remove the artifact from the import namespace"
    );
    assert!(
        artifacts.path().join(SWEEP_TRASH).exists(),
        "the call waited for the sweep"
    );
    release.release();
    assert!(
        wait_until(DEADLINE, || !artifacts.path().join(SWEEP_TRASH).exists()),
        "the released artifact sweep did not finish"
    );
}

#[test]
fn probe_cleanup_needs_no_external_helpers() {
    let fake = FakeClaude::running(VERSION_ONLY);
    let no_helpers = tempdir().unwrap();
    let base = tempdir().unwrap();
    environment::set(PATH, no_helpers.path());
    environment::set(TMPDIR, base.path());
    let err = fake.ask(ANSWERS).unwrap_err();
    assert!(err.contains(NO_EVENT_STOP), "got: {err}");
    assert!(listing(base.path()).is_empty());
}

#[test]
fn a_startup_exit_keeps_its_stderr_and_status() {
    let fake = FakeClaude::running(&format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '@VERSION@ (Claude Code)'; else echo '{STARTUP_STDERR}' >&2; exit {STARTUP_ERROR_CODE}; fi\n"
    ));
    let base = tempdir().unwrap();
    environment::set(TMPDIR, base.path());
    let error = fake.ask(ANSWERS).unwrap_err();
    assert!(error.contains(STARTUP_STDERR), "{error}");
    assert!(
        error.contains(&format!("exit status: {STARTUP_ERROR_CODE}")),
        "{error}"
    );
    assert!(listing(base.path()).is_empty());
}

/// Concurrent file changes can produce a snapshot state that never existed. Refuse that
/// snapshot and remove its artifact.
#[test_case(EDIT_FILE, "src/lib.rs" ; "an_edit")]
#[test_case(MAKE_EXECUTABLE, "src/lib.rs" ; "an_executable_bit")]
#[test_case(RETARGET_LINK, "src/link" ; "a_link_target")]
fn a_change_during_the_copy_is_refused(action: &str, disturbed: &str) {
    let copy = code_with_cp(&DISTURBING_CP.replace("@ACTION@", action), "");
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
    let copy = code_with_cp(
        &DISTURBING_CP.replace("@ACTION@", EDIT_DEPENDENCY),
        DEPENDENCY_DIR,
    );
    assert!(
        copy.err.contains(DEPENDENCY_UNSETTLED) && copy.err.contains(DEPENDENCY_FILE),
        "got: {}",
        copy.err
    );
    assert_eq!(
        copy.prompt.matches(DEPENDENCY_UNSETTLED).count(),
        1,
        "{}",
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

/// Claude Code passes its environment to its shell. A read worker keeps the
/// login variables, and with a login in maki's environment a coding call
/// stops before Claude Code starts.
#[test_case(OAUTH_TOKEN, TEST_LOGIN_TOKEN ; "login_token")]
#[test_case(HTTPS_PROXY, TEST_PROXY_LOGIN ; "proxy_login")]
fn a_login_in_the_environment_reaches_only_read_workers(name: &str, value: &str) {
    let fake = FakeClaude::new();
    environment::set(name, value);
    let (reg, _host) = load(fake.opts(&[]));
    let err = code_with(&reg, &fake.ctx(None, None), NO_SCENARIO, &[]).unwrap_err();
    assert!(err.contains(name), "got: {err}");
    assert_eq!(fake.log("calls"), "", "Claude Code started");

    assert_eq!(fake.ask(ANSWERS).unwrap(), ANSWER);
    let expected = format!("{name}={value}");
    assert!(fake.log("env").lines().any(|line| line == expected));
}

/// An API key or another API address would move the requests off the
/// subscription, so maki gives neither to Claude Code.
#[test]
fn an_api_key_and_address_never_reach_claude_code() {
    let fake = FakeClaude::new();
    environment::set(API_KEY, "not-a-real-key");
    environment::set(BASE_URL, "http://127.0.0.1:9");
    let result = fake.ask(ANSWERS);

    assert!(
        !fake.log("prompt").is_empty(),
        "the task must reach Claude Code: {result:?}"
    );
    let child_env = fake.log("envs");
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
#[test_case(false ; "physical_home")]
#[test_case(true ; "linked_home")]
fn the_default_artifact_root_hides_all_of_makis_state_but_the_workers_artifact(linked_home: bool) {
    let fake = FakeClaude::new();
    commit_repo(&fake.project.path(), &[LIB]);
    let maki_dir = tempdir().unwrap();
    let _in_maki_dir = WorkingDir::enter(maki_dir.path());
    let tools = tempdir().unwrap();
    let home = tempdir().unwrap();
    let state_home = tempdir().unwrap();
    let alias = tools.path().join(HOME_LINK);
    symlink(home.path(), &alias).unwrap();
    let visible_home = if linked_home {
        alias.as_path()
    } else {
        home.path()
    };
    environment::set(HOME_ENV, visible_home);
    let secrets = tools.path().join(SECRET_DIR);
    for path in HOME_DENY_PATHS {
        let target = secrets.join(path);
        if path == ".ssh" {
            fs::create_dir_all(&target).unwrap();
        } else {
            write(&target, "private\n");
        }
        let link = home.path().join(path);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, link).unwrap();
    }
    let usual_login = env::current_dir().unwrap().join(LOGIN_LINK);
    let login_target = secrets.join(LOGIN_LINK);
    fs::create_dir(&login_target).unwrap();
    symlink(&login_target, &usual_login).unwrap();
    environment::set(CLAUDE_CONFIG_ENV, LOGIN_LINK);
    environment::set(XDG_STATE_HOME, state_home.path());
    let runtime = tempdir().unwrap();
    let agent = home.path().join("agent.sock");
    environment::set("XDG_RUNTIME_DIR", runtime.path());
    environment::set("SSH_AUTH_SOCK", &agent);
    let state_dir = maki_storage::paths::state_dir().unwrap();
    assert!(
        state_dir.starts_with(state_home.path()),
        "maki fixed its paths before this test set them: {state_dir:?}, so the test needs its own process, as nextest gives it"
    );

    let (reg, _host) = load(fake.opts(&[("deny_read_home", json!("custom/token"))]));
    let _ = code_with(&reg, &fake.ctx(None, None), NO_SCENARIO, &[]);

    assert!(!fake.log("prompt").is_empty(), "maki did not send the task");
    let filesystem = sandbox_filesystem(&fake);
    let denied = filesystem["denyRead"].as_array().unwrap();
    let root = state_dir
        .join(DEFAULT_ARTIFACT_ROOT)
        .canonicalize()
        .unwrap();
    for hidden in [
        &state_dir,
        &root,
        runtime.path(),
        &agent,
        &usual_login,
        &login_target,
    ] {
        assert!(
            denied.contains(&json!(hidden)),
            "the sandbox must deny all of {hidden:?}: {filesystem}"
        );
    }
    for path in HOME_DENY_PATHS {
        for hidden in [
            visible_home.join(path),
            home.path().join(path),
            secrets.join(path),
        ] {
            if path.starts_with(".claude.json") && hidden != secrets.join(path) {
                continue;
            }
            assert!(
                denied.contains(&json!(hidden)),
                "{hidden:?} missing from: {filesystem}"
            );
        }
    }
    for home_path in [visible_home, home.path()] {
        assert!(denied.contains(&json!(home_path.join(".claude.json*"))));
    }
    let opened = filesystem["allowRead"].as_array().unwrap();
    let in_root = |path: &Value| {
        let parts: Vec<_> = Path::new(path.as_str().unwrap())
            .strip_prefix(&root)
            .unwrap()
            .iter()
            .collect();
        parts.len() == 2 && parts[1] == ARTIFACT_GIT
    };
    assert!(
        matches!(opened.as_slice(), [git] if in_root(git)),
        "only the git directory of the worker can be open again: {filesystem}"
    );
}

fn code_with_cp(cp: &str, dependencies: &str) -> DisturbedCopy {
    let fake = FakeClaude::new();
    let project = fake.project.path();
    write(&project.join("src/other.rs"), "other\n");
    symlink("lib.rs", project.join("src/link")).unwrap();
    commit_repo(&project, &[LIB]);
    write(&project.join(DEPENDENCY_FILE), DEPENDENCY_CONTENT);
    let tools = tempdir().unwrap();
    hold(
        tools.path(),
        "cp",
        &cp.replace("@MARKER@", &project.to_string_lossy()),
    );

    let artifacts = tempdir().unwrap();
    let (reg, _host) = load(fake.opts(&[
        ("artifact_dir", json!(artifacts.path())),
        ("dependencies", json!(dependencies)),
    ]));
    let err = code_with(&reg, &fake.ctx(None, None), NO_SCENARIO, &[]).unwrap_err();
    DisturbedCopy {
        err,
        prompt: fake.log("prompt"),
        artifacts_left: artifact_listing(artifacts.path()),
    }
}
