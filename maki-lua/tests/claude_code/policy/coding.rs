//! Coding profile tests use real repositories, private snapshots and filesystem imports.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::iter;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use maki_agent::cancel::CancelToken;
use maki_agent::permissions::{PermissionAnswer, PermissionManager, TaggedAnswer};
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentEvent, EventSender};
use maki_config::{PermissionsConfig, ProjectConfig};
use maki_lua::PluginHost;
use serde_json::{Map, Value, json};
use smol::lock::Mutex;
use tempfile::{TempDir, tempdir};
use test_case::test_case;

use super::super::support::{
    ARTIFACT_LINE, BASH_SRC, DEADLINE, IMPORT_TOOL, LEFT_BEHIND, NOTHING_IMPORTED, ORIGINALS,
    PWNED, SNAPSHOT_DIR, TOOL, WRITE_SRC, contents, git, listing, load, mode_bits_hold, tool_reply,
    try_load, wait_until, write,
};
use super::{
    ANSWERS, CODE_CLAUDE_CONFIG, CODE_EDIT, CODE_HANG, CODE_HOSTILE, CODE_LONG, CODE_PROFILE,
    CODE_RETYPE, FakeClaude, HOOK_SENTINEL, HOSTILE_NAME, MODEL_ALIAS, NO_ANSWER_IN_TIME, ONE_SLOT,
    WORKING, add_a_managed_hook, ctx_in, within_deadline,
};

/// What a test writes over a file the artifact recorded.
const TAMPERED: &str = "tampered\n";
/// Untracked in the test repository, so a coding call copies it only on
/// request.
const UNTRACKED_INPUT: &str = "fixtures";
const NODE_MODULES: &str = "node_modules";
const DEPENDENCIES_WITH_A_VENV: &str = "node_modules,.venv";
/// Makes the fake point the snapshot's `.git/info/attributes` at a file in
/// the live checkout, then edit a file.
const CODE_REDIRECT: &str = "code_redirect";
const DEPENDENCY_SECRET: &str = "node_modules/.env";
/// The test writes this into the checkout while an import waits for
/// approval.
const NEWER_WORK: &str = "the user's newer work\n";
const DEPENDENCY_DENIED: &str = "node_modules/pkg/private.key";
const BASE_LIB: &str = "base\n";
const DIRTY_LIB: &str = "base\ndirty\n";
const WORKER_LIB: &str = "base\ndirty\nworker\n";
const IMPORTED_LIB: &str = "src/lib.rs";
const DELETED_FILE: &str = "src/old.rs";
const ORIGINALS_KEPT: &str = "The previous versions stay in";
const ADDED_FILE: &str = "src/new.rs";
const CHANGED_PATHS: [&str; 5] = [IMPORTED_LIB, ADDED_FILE, DELETED_FILE, "logo.bin", "run.sh"];
const DELETION_IMPORTED: &str = "src/old.rs: imported before";
/// Where a test moves part of the checkout to put a link in its place.
const OUTSIDE_COPY: &str = "copy";
const WORKER_NEW: &str = "fresh\n";
const NEWER_LIB: &str = "newer\n";
const DEPENDENCY: &str = "node_modules/pkg/index.js";
const DEPENDENCY_CONTENT: &str = "module.exports = 1;\n";
const EXPRESSION_NAMED_DIR: &str = "-print";
const EXPRESSION_NAMED_DEPENDENCY: &str = "-print/index.js";
const UNSETTLED_NOTE: &str = "dependencies changed during the copy";
const UNICODE_DEPENDENCY_DIR: &str = "d\u{e9}ps";
const UNICODE_DEPENDENCY: &str = "d\u{e9}ps/pkg/index.js";
/// Target of a link in the test repository that points outside the project.
const OUTSIDE_TARGET: &str = "/etc";
const REPORTED_CHANGES: [&str; 5] = [
    "M src/lib.rs",
    "A src/new.rs",
    "D src/old.rs",
    "A logo.bin (binary)",
    "A run.sh",
];
const BINARY_BYTES: [u8; 2] = [0, 1];
const EXECUTABLE_BITS: u32 = 0o111;
const ALREADY_IMPORTED: &str = "src/lib.rs: imported before";
/// What both checks say about a changed file, before and after approval.
const CHANGED_IN_CHECKOUT: &str = "changed in the checkout after the snapshot";
const DENIED_IMPORT: &str = "Permission denied for `bash` (the import command).";
/// In the bash tool's marker or the runtime's one-word note, whichever
/// answers the cancel first.
const CANCELLED: &str = "cancelled";
const IMPORT_GUIDANCE: &str = "merge it manually";
/// A line of the import command, which a denial must not echo.
const STAGING_STEP: &str = "staged ";
/// The prefix of every temporary file an import writes.
const IMPORT_TEMP: &str = ".maki-import-";
/// An artifact's repository, next to its snapshot.
const ARTIFACT_GIT: &str = "git";
const SHA1: &str = "sha1";
const SHA256: &str = "sha256";
const HOSTILE_CONTENT: &str = "hostile\n";
/// As long as a SHA-1 id, so only the hex check refuses it.
const COMMAND_FOR_A_SHA: &str = "$(touch pwned)aaaaaaaaaaaaaaaaaaaaaaaaaa";
const NOT_WRITTEN_BY_MAKI: &str = "has a manifest that maki did not write";
const FOR_ANOTHER_CHECKOUT: &str = "contains changes for";
const UNRECORDED: &str = "maki could not record this in the artifact";
const EXECUTABLE_MODE: u32 = 0o755;
const PREPARE_FAILED: &str = "the prepare command stopped with an error";
/// Longer than `DEADLINE`, so a stop that fails makes the test fail while it
/// waits.
const PREPARE_NEVER_ENDS: &str = "sleep 240";
const SNAPSHOT_STAYS: &str = "The worker's snapshot stays in";
const REPLY_LINES: usize = 8;
/// The blank line and the note `maki.truncate` appends past the limit.
const TRUNCATION_LINES: usize = 2;
/// A file the prepare command writes, the way a package tool rewrites its
/// lock file.
const PREPARED_FILE: &str = "lock.json";
const PREPARE_WRITES_LOCK: &str = "printf 'locked\\n' > lock.json";
const CREATE_DEPENDENCY: &str =
    "mkdir -p node_modules/pkg; printf 'module.exports = 1;\\n' > node_modules/pkg/index.js";
const BREAK_BASELINE: &str = "git config core.repositoryformatversion 999";
const CANNOT_RECORD_PREPARED: &str = "maki cannot record the prepared snapshot";
const PREPARED_NOTE: &str = "the prepare command changed lock.json";
const WORKER_EDIT: &str = "worker\n";
const PREPARED_LINK: &str = "prepared_link";
const LINKS_LEFT_OUT: &str = "maki did not copy the links that went out of the snapshot";
/// Exits with the code `timeout(1)` gives a command it killed, long before
/// the limit, so only the plugin's own clock can mark a timeout.
const EXITS_AS_A_TIMEOUT_WOULD: &str = "exit 124";
const EXITED_WITH_124: &str = "exited with code 124";
const PREPARE_STARTED: &str = "prepare-started";
const LINKED_PROJECT: &str = "linked";
const PREPARE_TOLD: &str = "could not prepare the dependencies";
const VENV_NOTE: &str = ".venv is a Python virtual environment";
const SECS_PER_HOUR: u64 = 3600;
const STALE_AGE_HOURS: u64 = 48;
const OLD_FOLDER: &str = "Ab12Cd34";
/// A stale artifact beside the one a sweep test checks, whose removal shows
/// that the sweep ran.
const SWEPT_FOLDER: &str = "Ef56Gh78";
const MANIFEST: &str = "manifest.json";
const ARTIFACT_MARKER: &str = ".maki-claude-code-artifact";
const INSIDE_ARTIFACTS: &str = "artifacts";
const ARTIFACTS_INSIDE: &str = "is in the project";
const LINK_TO_NOWHERE: &str = "is a link to a missing target";
const ARTIFACT_DIR_RELATIVE: &str = "must be an absolute path";
const ARTIFACT_DIR_CLIMBS: &str = "must not contain `..`";
const CANNOT_READ: &str = "cannot read";
const BLOCKER: &str = "blocker";
const NOWHERE: &str = "nowhere";
const NO_ACCESS_MODE: u32 = 0o000;
/// git prints this name quoted and escaped, and reads it back only quoted.
const QUOTED_NAME: &str = "odd \"name\"\\\tand\nlines.txt";
const NOT_UTF8_NAME: &[u8] = b"latin\xe9.txt";
const NOT_UTF8_STOP: &str = "the file name latin\\351.txt is not UTF-8";
/// `CODE_RETYPE`'s report: a file that became a folder is a type change on
/// both sides, and an escape byte in a name shows escaped.
const RETYPED_REPORT: [&str; 3] = [
    "D src/lib.rs (type, apply manually)",
    "A src/lib.rs/mod.rs (type, apply manually)",
    "A esc\\033[2J.txt",
];
const RETYPED_SKIPPED: &str = "src/lib.rs: a type change. Apply it manually";
const NOT_UTF8_NOTE: &str = "the names of latin\\351.txt are not UTF-8";
const ESCAPED_NAME: &str = "esc\\033[2J.txt";
const ESCAPE_NAME: &str = "esc\x1b[2J.txt";
const SUBMODULE: &str = "vendor/lib";
const SUBMODULE_NOTE: &str = "the snapshot has no submodules, so the worker did not see vendor/lib";
/// A bash tool that runs the command and still fails, as a command killed
/// after its last write does.
const BASH_FAILING_AFTER: &str = r#"
maki.api.register_tool({
  name = "bash",
  description = "runs a command, then fails",
  schema = { type = "object", properties = { command = { type = "string" }, workdir = { type = "string" } } },
  audiences = { "main" },
  handler = function(input)
    maki.fn.jobwait(maki.fn.jobstart({ "bash", "-c", input.command }, { cwd = input.workdir }), 60000)
    return { llm_output = "@FAILED@", is_error = true }
  end,
})
"#;
const FAILED_AFTER_WRITES: &str = "killed after its last write";
/// Set for every test process and never passed to Claude Code.
const HOLD_IMPORT: &str = r#"
local jobstart = maki.fn.jobstart
maki.fn.jobstart = function(command, opts)
  return jobstart([==[ln() { touch '@HELD@'; : > '@PIPE@'; command ln "$@"; }
]==] .. command, opts)
end
"#;
const IMPORT_HELD: &str = "import-held";
const IMPORT_RELEASE: &str = "import-release";
const USER_ONLY_VARIABLE: &str = "CARGO_MANIFEST_DIR";
const PREPARED_ENV: &str = "prepared_env.txt";

fn coding_repo(project: &Path, object_format: &str) {
    fs::remove_dir(project.join(".git")).unwrap();
    for (path, content) in [
        (IMPORTED_LIB, BASE_LIB),
        (DELETED_FILE, "old\n"),
        (QUOTED_NAME, "odd\n"),
        (".env", "SECRET=1\n"),
        ("secrets/key", "key\n"),
        (".claude/agents/reviewer.md", "agent\n"),
        ("docs/.claude", "{}\n"),
    ] {
        write(&project.join(path), content);
    }
    symlink(OUTSIDE_TARGET, project.join("out")).unwrap();
    symlink("lib.rs", project.join("src/inner")).unwrap();
    git(
        project,
        &["init", "-q", &format!("--object-format={object_format}")],
    );
    git(project, &["add", "-A"]);
    git(project, &["commit", "-q", "-m", "base"]);
    write(&project.join(IMPORTED_LIB), DIRTY_LIB);
    write(&project.join("notes.txt"), "untracked\n");
    write(&project.join("fixtures/in.json"), "{}\n");
}

/// A fake with an on-disk git repository, an artifact directory, and maki's
/// write and bash tools for imports.
struct Coding {
    fake: FakeClaude,
    artifacts: TempDir,
}

impl Coding {
    fn new() -> Self {
        Self::in_format(SHA1)
    }

    /// A checkout whose git names objects in `object_format`.
    fn in_format(object_format: &str) -> Self {
        let fake = FakeClaude::new();
        coding_repo(&fake.project.path(), object_format);
        Self {
            fake,
            artifacts: tempdir().unwrap(),
        }
    }

    fn project(&self) -> PathBuf {
        self.fake.project.path()
    }

    fn host(&self, extra: &[(&str, Value)]) -> (Arc<ToolRegistry>, PluginHost) {
        self.host_with(extra, BASH_SRC)
    }

    fn host_with(&self, extra: &[(&str, Value)], bash: &str) -> (Arc<ToolRegistry>, PluginHost) {
        let mut opts = self.fake.opts(ONE_SLOT);
        opts.insert("artifact_dir".into(), json!(self.artifacts.path()));
        for (name, value) in extra {
            opts.insert((*name).into(), value.clone());
        }
        let (reg, host) = load(opts);
        host.load_source_with_opts("write", WRITE_SRC, Map::new())
            .unwrap();
        host.load_source_with_opts("bash", bash, Map::new())
            .unwrap();
        (reg, host)
    }

    fn coded(prompt: &str) -> (Self, Arc<ToolRegistry>, PluginHost, String) {
        Self::coded_in(SHA1, prompt)
    }

    fn coded_in(
        object_format: &str,
        prompt: &str,
    ) -> (Self, Arc<ToolRegistry>, PluginHost, String) {
        let coding = Self::in_format(object_format);
        let (reg, host) = coding.host(&[]);
        let id = artifact_id(&coding.code(&reg, prompt, &[]).unwrap());
        (coding, reg, host, id)
    }

    /// Nested tool calls find their tools through the context's registry.
    fn ctx(&self, reg: &Arc<ToolRegistry>) -> ToolContext {
        let mut ctx = self.fake.ctx(None, None);
        ctx.registry = Arc::clone(reg);
        ctx
    }

    fn code(
        &self,
        reg: &Arc<ToolRegistry>,
        prompt: &str,
        include: &[&str],
    ) -> Result<String, String> {
        let input = json!({ "prompt": prompt, "model": MODEL_ALIAS, "profile": CODE_PROFILE, "include": include });
        run_tool(reg, &self.ctx(reg), TOOL, input)
    }

    /// Runs a coding call from a session in a worktree made in `outside`, or
    /// in the checkout's `src`, and returns the session's directory too.
    fn code_away_from_root(
        &self,
        reg: &Arc<ToolRegistry>,
        outside: &Path,
        from_worktree: bool,
    ) -> (PathBuf, Result<String, String>) {
        let project = self.project();
        let session = if from_worktree {
            let tree = outside.canonicalize().unwrap().join("tree");
            git(&project, &["worktree", "add", "-q", tree.to_str().unwrap()]);
            tree
        } else {
            project.join("src")
        };
        let mut ctx = ctx_in(&session, None, None);
        ctx.registry = Arc::clone(reg);
        let input = json!({ "prompt": CODE_EDIT, "model": MODEL_ALIAS, "profile": CODE_PROFILE });
        let result = run_tool(reg, &ctx, TOOL, input);
        (session, result)
    }

    fn import(&self, reg: &Arc<ToolRegistry>, input: Value) -> Result<String, String> {
        run_tool(reg, &self.ctx(reg), IMPORT_TOOL, input)
    }

    fn import_approved_after(
        &self,
        reg: &Arc<ToolRegistry>,
        id: &str,
        paths: &[&str],
        meanwhile: impl FnOnce() + Send + 'static,
    ) -> Result<String, String> {
        self.import_answered(reg, id, paths, PermissionAnswer::AllowOnce, meanwhile)
    }

    fn import_answered(
        &self,
        reg: &Arc<ToolRegistry>,
        id: &str,
        paths: &[&str],
        answer: PermissionAnswer,
        meanwhile: impl FnOnce() + Send + 'static,
    ) -> Result<String, String> {
        let project = self.project();
        let mut ctx = self.ctx(reg);
        ctx.permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            project.clone(),
            ProjectConfig::discover(&project),
            Arc::default(),
        ));
        let (event_tx, events) = flume::unbounded();
        let (answer_tx, answers) = flume::unbounded();
        ctx.event_tx = EventSender::new(event_tx, 0);
        ctx.user_response_rx = Some(Arc::new(Mutex::new(answers)));
        let approver = thread::spawn(move || {
            while let Ok(envelope) = events.recv_timeout(DEADLINE) {
                if let AgentEvent::PermissionRequest { id, .. } = envelope.event {
                    meanwhile();
                    let answer = TaggedAnswer::new(id, answer);
                    answer_tx.send(answer.encode()).unwrap();
                    return true;
                }
            }
            false
        });
        let result = run_tool(reg, &ctx, IMPORT_TOOL, json!({ "id": id, "paths": paths }));
        assert!(
            approver.join().unwrap(),
            "the import did not wait for an approval"
        );
        result
    }
}

fn run_tool(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    tool: &str,
    input: Value,
) -> Result<String, String> {
    smol::block_on(within_deadline(tool_reply(reg, ctx, tool, input)))
}

/// Returns every import temporary file in or under `dir`.
fn temp_files(dir: &Path) -> Vec<PathBuf> {
    contents(dir)
        .into_keys()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(IMPORT_TEMP))
        })
        .collect()
}

fn artifact_id(reply: &str) -> String {
    let after = reply
        .split(ARTIFACT_LINE)
        .nth(1)
        .unwrap_or_else(|| panic!("there is no artifact in: {reply}"));
    after.split(',').next().unwrap().to_owned()
}

const NESTED_FILE: &str = "nested/file.txt";
const NESTED_WORK: &str = "nested work\n";
const METADATA_MARKER: &str = "metadata_ran";
const QUARANTINE: &str = "git-metadata";
const SPARSE_MISSING: &str = "omitted/file.rs";
const DENIED_CONTENT: &str = "secret\n";

#[test_case("code_nested_empty" ; "without_a_commit")]
#[test_case("code_nested_commit" ; "with_a_commit")]
#[test_case("code_nested_pointer" ; "with_a_rewritten_root_pointer")]
#[test_case("code_nested_casefold" ; "with_casefolded_metadata")]
fn worker_git_metadata_is_quarantined_and_ordinary_files_import(scenario: &str) {
    let (coding, reg, _host, id) = Coding::coded(scenario);
    let artifact = coding.artifacts.path().join(&id);
    let snapshot = artifact.join(SNAPSHOT_DIR);
    assert!(!snapshot.join("nested/.git").exists());
    assert!(!snapshot.join("nested/.GiT").exists());
    let expected = if scenario == "code_nested_pointer" {
        2
    } else {
        1
    };
    assert_eq!(listing(&artifact.join(QUARANTINE)).len(), expected);
    git(&snapshot, &["status", "--porcelain"]);
    git(&snapshot, &["diff"]);
    assert!(!coding.fake.path(METADATA_MARKER).exists());
    coding
        .import(&reg, json!({ "id": id, "paths": [NESTED_FILE] }))
        .unwrap();
    assert_eq!(
        fs::read_to_string(coding.project().join(NESTED_FILE)).unwrap(),
        NESTED_WORK
    );
}

#[test]
fn sparse_checkout_omissions_stay_out_of_the_snapshot() {
    let coding = Coding::new();
    let project = coding.project();
    write(&project.join(SPARSE_MISSING), BASE_LIB);
    git(&project, &["add", SPARSE_MISSING]);
    git(&project, &["commit", "-qm", "omitted"]);
    git(
        &project,
        &["update-index", "--skip-worktree", SPARSE_MISSING],
    );
    fs::remove_file(project.join(SPARSE_MISSING)).unwrap();
    let (reg, _host) = coding.host(&[]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();
    let id = artifact_id(&reply);
    assert!(
        !coding
            .artifacts
            .path()
            .join(&id)
            .join(SNAPSHOT_DIR)
            .join(SPARSE_MISSING)
            .exists()
    );
    coding
        .import(&reg, json!({ "id": id, "paths": [IMPORTED_LIB] }))
        .unwrap();
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
}

#[test_case(".env/prod" ; "an_env_directory")]
#[test_case("deep/.env/prod" ; "a_nested_env_directory")]
#[test_case(".maki/init.lua" ; "maki_configuration")]
#[test_case("deep/.MaKi/permissions.toml" ; "casefolded_maki_configuration")]
#[test_case(".Claude/settings.json" ; "casefolded_claude_configuration")]
fn protected_directory_contents_never_enter_the_snapshot(path: &str) {
    let coding = Coding::new();
    let project = coding.project();
    if project.join(".env").is_file() && path.starts_with(".env/") {
        fs::remove_file(project.join(".env")).unwrap();
    }
    write(&project.join(path), DENIED_CONTENT);
    git(&project, &["add", "-f", path]);
    git(&project, &["commit", "-qm", "protected"]);
    let (reg, _host) = coding.host(&[]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();
    let id = artifact_id(&reply);
    assert!(
        !coding
            .artifacts
            .path()
            .join(id)
            .join(SNAPSHOT_DIR)
            .join(path)
            .exists()
    );
}

#[test]
fn a_coding_run_changes_a_snapshot_and_never_the_checkout() {
    let include = &[UNTRACKED_INPUT];
    let coding = Coding::new();
    let project = coding.project();
    let before = contents(&project);
    let (reg, _host) = coding.host(&[]);
    let reply = coding.code(&reg, CODE_EDIT, include).unwrap();

    for change in REPORTED_CHANGES {
        assert!(reply.contains(change), "{change} missing from: {reply}");
    }
    let listing = coding.fake.log("snapshot_listing");
    let listed = |path: &str| listing.lines().any(|line| line == path);
    for taken in ["./src/lib.rs", "./src/inner", "./fixtures/in.json"] {
        assert!(
            listed(taken),
            "{taken} missing from the snapshot:\n{listing}"
        );
    }
    for left_out in [
        "./.env",
        "./secrets",
        "./.claude",
        "./docs/.claude",
        "./notes.txt",
        "./out",
    ] {
        assert!(
            !listed(left_out),
            "{left_out} must stay out of the snapshot"
        );
    }
    assert_eq!(coding.fake.log("snapshot_lib"), DIRTY_LIB);
    assert_eq!(contents(&project), before, "the checkout must not change");

    let argv: Vec<String> = coding.fake.log("argv").lines().map(String::from).collect();
    assert!(
        argv.windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"])
    );
    assert!(
        argv.windows(2)
            .any(|w| w == ["--tools", "Read,Glob,Grep,Edit,Write,Bash"])
    );
    let started = coding.fake.log("started_in");
    let run_dir = Path::new(started.lines().last().unwrap());
    let artifact = run_dir.parent().unwrap();
    assert!(
        artifact.starts_with(coding.artifacts.path().canonicalize().unwrap()),
        "ran in {run_dir:?}"
    );

    let filesystem = sandbox_filesystem(&coding.fake);
    let denied = denied_reads(&filesystem);
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    for path in [
        project.clone(),
        coding.fake.path("config"),
        maki_storage::paths::state_dir().unwrap(),
        maki_storage::paths::config_dir().unwrap(),
        home.join(".claude"),
        home.join(".claude.json"),
        coding.artifacts.path().canonicalize().unwrap(),
    ] {
        assert!(
            denied.contains(&path),
            "the sandbox must deny reads of {path:?}"
        );
    }
    assert_eq!(
        filesystem["allowRead"],
        json!([artifact.join(ARTIFACT_GIT)]),
        "of the paths that the sandbox does not show, the worker must read only its git directory"
    );
    let temp = artifact.join("tmp");
    assert_eq!(filesystem["allowWrite"], json!([temp]));
    let child_env = coding.fake.log("env");
    assert!(
        child_env
            .lines()
            .any(|line| line == format!("TMPDIR={}", temp.display())),
        "the temporary directory of the worker must be the directory of the artifact"
    );
}

/// The `sandbox.filesystem` settings the fake's last run received.
fn sandbox_filesystem(fake: &FakeClaude) -> Value {
    let argv: Vec<String> = fake.log("argv").lines().map(String::from).collect();
    let settings = argv
        .windows(2)
        .find(|w| w[0] == "--settings")
        .map(|w| serde_json::from_str::<Value>(&w[1]).unwrap())
        .expect("the run gives its settings");
    settings["sandbox"]["filesystem"].clone()
}

fn denied_reads(filesystem: &Value) -> Vec<PathBuf> {
    filesystem["denyRead"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect()
}

/// A worktree and its primary checkout share git objects. Deny every path that can expose the
/// original project.
#[test_case(true ; "from_a_worktree")]
#[test_case(false ; "from_a_subdirectory")]
fn the_sandbox_denies_every_path_to_the_checkout(from_worktree: bool) {
    let coding = Coding::new();
    let project = coding.project();
    let outside = tempdir().unwrap();
    let (reg, _host) = coding.host(&[]);

    let (session, result) = coding.code_away_from_root(&reg, outside.path(), from_worktree);
    result.unwrap();

    let also_denied = if from_worktree {
        vec![project.clone(), project.join(".git")]
    } else {
        vec![project.clone()]
    };

    let denied = denied_reads(&sandbox_filesystem(&coding.fake));
    for path in iter::once(&session).chain(&also_denied) {
        assert!(
            denied.contains(path),
            "the sandbox must deny reads of {path:?}: {denied:?}"
        );
    }
}

/// An import applies the bytes and executable bits the artifact recorded,
/// binary files included, and keeps what it replaced or deleted in the
/// artifact. A second import has nothing left to do. Artifact ids are SHA-1
/// whatever the checkout's format, and every check compares ids in that
/// format.
#[test_case(SHA1   ; "in_a_sha1_checkout")]
#[test_case(SHA256 ; "in_a_sha256_checkout")]
fn an_import_lands_what_the_artifact_recorded(object_format: &str) {
    let (coding, reg, _host, id) = Coding::coded_in(object_format, CODE_EDIT);
    let project = coding.project();

    let reply = coding.import(&reg, json!({ "id": id })).unwrap();
    assert!(reply.contains(ORIGINALS_KEPT), "got: {reply}");
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
    let originals = listing(&coding.artifacts.path().join(&id).join(ORIGINALS))
        .pop()
        .unwrap();
    assert_eq!(
        fs::read_to_string(originals.join(IMPORTED_LIB)).unwrap(),
        DIRTY_LIB
    );
    assert!(
        originals.join(DELETED_FILE).exists(),
        "the deleted file is missing"
    );
    assert_eq!(
        fs::read_to_string(project.join(ADDED_FILE)).unwrap(),
        WORKER_NEW
    );
    assert!(
        !project.join(DELETED_FILE).exists(),
        "the import did not delete the file"
    );
    assert_eq!(fs::read(project.join("logo.bin")).unwrap(), BINARY_BYTES);
    let mode = fs::metadata(project.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & EXECUTABLE_BITS,
        EXECUTABLE_BITS,
        "the executable bit is missing"
    );

    let again = coding.import(&reg, json!({ "id": id })).unwrap();
    assert!(again.contains(ALREADY_IMPORTED), "got: {again}");
}

/// A file that changed in the checkout after the snapshot stops the whole
/// import, so no part of a change set lands on newer work. The user can
/// still pick the other changes by path.
#[test]
fn an_import_applies_nothing_over_newer_work() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let project = coding.project();
    fs::write(project.join(IMPORTED_LIB), NEWER_LIB).unwrap();

    let err = coding.import(&reg, json!({ "id": id })).unwrap_err();
    assert!(
        err.contains(NOTHING_IMPORTED) && err.contains(IMPORTED_LIB),
        "got: {err}"
    );
    assert!(!project.join(ADDED_FILE).exists());
    assert!(project.join(DELETED_FILE).exists());
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        NEWER_LIB
    );

    coding
        .import(&reg, json!({ "id": id, "paths": [ADDED_FILE] }))
        .unwrap();
    assert_eq!(
        fs::read_to_string(project.join(ADDED_FILE)).unwrap(),
        WORKER_NEW
    );
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        NEWER_LIB
    );
}

/// An import writes the objects the collect step recorded, not the snapshot
/// folder, which the user can open and edit. If a file there changes after
/// the collect step, the recorded data still wins.
#[test]
fn an_import_lands_what_was_recorded_whatever_the_snapshot_holds() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let snapshot = coding.artifacts.path().join(&id).join(SNAPSHOT_DIR);
    fs::write(snapshot.join(ADDED_FILE), TAMPERED).unwrap();

    coding.import(&reg, json!({ "id": id })).unwrap();
    assert_eq!(
        fs::read_to_string(coding.project().join(ADDED_FILE)).unwrap(),
        WORKER_NEW
    );
}

/// A non-UTF-8 name cannot pass through the Lua process API. Stop the call so the snapshot
/// cannot silently omit it.
#[test]
fn a_file_name_maki_cannot_pass_on_is_refused() {
    let coding = Coding::new();
    let project = coding.project();
    fs::write(project.join(OsStr::from_bytes(NOT_UTF8_NAME)), "x\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "latin1"]);
    let (reg, _host) = coding.host(&[]);

    let err = coding.code(&reg, CODE_EDIT, &[]).unwrap_err();
    assert!(err.contains(NOT_UTF8_STOP), "got: {err}");
    assert!(
        !coding.fake.log("calls").contains("print"),
        "maki must not send a prompt"
    );
    assert_eq!(fs::read_dir(coding.artifacts.path()).unwrap().count(), 0);
}

/// A submodule is a different repository, so the snapshot leaves it out and
/// the reply says so.
#[test]
fn a_submodule_left_out_of_the_snapshot_is_named() {
    let coding = Coding::new();
    let project = coding.project();
    let commit = git(&project, &["rev-parse", "HEAD"]);
    let gitlink = format!("160000,{commit},{SUBMODULE}");
    git(
        &project,
        &["update-index", "--add", "--cacheinfo", &gitlink],
    );
    let (reg, _host) = coding.host(&[]);

    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();
    assert!(reply.contains(SUBMODULE_NOTE), "got: {reply}");
}

/// A path given twice is imported once, so a second delete of the same file
/// cannot break the import partway.
#[test]
fn a_path_named_twice_is_imported_once() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    coding
        .import(
            &reg,
            json!({ "id": id, "paths": [DELETED_FILE, DELETED_FILE] }),
        )
        .unwrap();

    assert!(!coding.project().join(DELETED_FILE).exists());
    let again = coding
        .import(&reg, json!({ "id": id, "paths": [DELETED_FILE] }))
        .unwrap();
    assert!(again.contains(DELETION_IMPORTED), "got: {again}");
}

/// Approval can take long, so a file can change before its step runs. Each
/// step checks its file again after approval, and the import never
/// overwrites or deletes newer work.
#[test_case(DELETED_FILE ; "a_deletion")]
#[test_case(ADDED_FILE ; "an_addition")]
fn an_import_rechecks_each_file_after_its_approval(path: &str) {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let target = coding.project().join(path);
    let edited = target.clone();
    let err = coding
        .import_approved_after(&reg, &id, &[path], move || {
            fs::write(&edited, NEWER_WORK).unwrap();
        })
        .unwrap_err();

    assert!(
        err.contains(CHANGED_IN_CHECKOUT) && err.contains(path),
        "got: {err}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), NEWER_WORK);
}

/// Failure to preserve any original must stop the import before the first checkout change.
#[test]
fn an_original_that_cannot_be_kept_changes_no_file() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let project = coding.project();
    let before = contents(&project);
    let blocker = coding.artifacts.path().join(&id).join(ORIGINALS);
    fs::write(&blocker, BLOCKER).unwrap();

    let input = json!({ "id": id, "paths": CHANGED_PATHS });
    let err = coding.import(&reg, input.clone()).unwrap_err();
    assert!(err.contains(NOTHING_IMPORTED), "got: {err}");
    assert_eq!(contents(&project), before);
    assert_eq!(temp_files(&project), Vec::<PathBuf>::new());

    fs::remove_file(&blocker).unwrap();
    coding.import(&reg, input).unwrap();
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
    assert!(!project.join(DELETED_FILE).exists());
}

#[test]
fn pending_approval_blocks_another_import_and_expiry() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let artifact = coding.artifacts.path().join(&id);
    let control = old_folder(
        coding.artifacts.path(),
        SWEPT_FOLDER,
        &[ARTIFACT_MARKER, MANIFEST],
    );
    let queued_id = id.clone();
    let queued_reg = Arc::clone(&reg);
    let queued_ctx = coding.ctx(&reg);
    coding
        .import_approved_after(&reg, &id, &[IMPORTED_LIB], move || {
            let second = run_tool(
                &queued_reg,
                &queued_ctx,
                IMPORT_TOOL,
                json!({ "id": queued_id, "paths": [DELETED_FILE] }),
            )
            .unwrap_err();
            assert!(second.contains("cannot lock artifact"), "{second}");
            for name in [ARTIFACT_MARKER, MANIFEST] {
                backdate(&artifact.join(name));
            }
            run_tool(
                &queued_reg,
                &queued_ctx,
                TOOL,
                json!({ "prompt": CODE_EDIT, "profile": CODE_PROFILE }),
            )
            .unwrap();
            assert!(
                wait_until(DEADLINE, || !control.exists()),
                "the sweep did not complete"
            );
            assert!(
                artifact.exists(),
                "expiry deleted an artifact awaiting approval"
            );
        })
        .unwrap();
    coding
        .import(&reg, json!({ "id": id, "paths": [DELETED_FILE] }))
        .unwrap();
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(coding.artifacts.path().join(id).join(MANIFEST)).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["imported"][IMPORTED_LIB], true);
    assert_eq!(manifest["imported"][DELETED_FILE], true);
}

/// Manifest fields enter an approved shell command. Reject manifests that collection cannot
/// produce or that identify a different checkout.
#[test_case(|manifest: &mut Value, _: &Path| manifest["changes"][0]["new_sha"] = json!(COMMAND_FOR_A_SHA), NOT_WRITTEN_BY_MAKI ; "a_command_for_an_object_id")]
#[test_case(|manifest: &mut Value, outside: &Path| manifest["project"] = json!(outside), FOR_ANOTHER_CHECKOUT ; "another_checkout")]
fn an_import_refuses_a_manifest_maki_did_not_write(tamper: fn(&mut Value, &Path), want: &str) {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let manifest_path = coding.artifacts.path().join(&id).join(MANIFEST);
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let outside = tempdir().unwrap();
    tamper(&mut manifest, outside.path());
    fs::write(&manifest_path, manifest.to_string()).unwrap();
    let before = contents(&coding.project());

    let err = coding.import(&reg, json!({ "id": id })).unwrap_err();
    assert!(err.contains(want), "got: {err}");
    assert_eq!(contents(&coding.project()), before);
    assert!(
        !coding.project().join(PWNED).exists(),
        "the manifest ran a command"
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

/// If an applied import cannot be recorded in the artifact, the user is
/// told, because a second import will then show the files as changed.
#[test]
fn an_import_that_cannot_be_recorded_says_so() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let manifest = coding.artifacts.path().join(&id).join(MANIFEST);
    let blocked = manifest.clone();
    let reply = coding
        .import_approved_after(&reg, &id, &[IMPORTED_LIB], move || {
            fs::remove_file(&blocked).unwrap();
            fs::create_dir(&blocked).unwrap();
        })
        .unwrap();

    assert!(reply.contains(UNRECORDED), "got: {reply}");
    assert_eq!(
        fs::read_to_string(coding.project().join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
}

/// Import denials must preserve the checkout and include the user's guidance.
/// The reply must omit the command.
#[test]
fn a_denied_import_passes_on_the_guidance_and_not_its_command() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let before = contents(&coding.project());
    let denial = PermissionAnswer::DenyWithGuidance(IMPORT_GUIDANCE.into());
    let err = coding
        .import_answered(&reg, &id, &CHANGED_PATHS, denial, || {})
        .unwrap_err();

    for part in [NOTHING_IMPORTED, DENIED_IMPORT, IMPORT_GUIDANCE] {
        assert!(err.contains(part), "{part} missing from: {err}");
    }
    assert!(
        !err.contains(STAGING_STEP),
        "the error contains the command: {err}"
    );
    assert_eq!(contents(&coding.project()), before);
}

/// Pause the first backup link after all temporary files exist. Cancellation must remove
/// them before the test examines the unchanged checkout.
#[test]
fn a_cancelled_import_leaves_the_checkout_as_it_was() {
    let coding = Coding::new();
    let held = coding.artifacts.path().join(IMPORT_HELD);
    let pipe = coding.artifacts.path().join(IMPORT_RELEASE);
    assert!(
        Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap()
            .success()
    );
    let prefix = HOLD_IMPORT
        .replace("@HELD@", held.to_str().unwrap())
        .replace("@PIPE@", pipe.to_str().unwrap());
    let (reg, _host) = coding.host_with(&[], &(prefix + BASH_SRC));
    let id = artifact_id(&coding.code(&reg, CODE_EDIT, &[]).unwrap());
    let project = coding.project();
    let before = contents(&project);
    let mut ctx = coding.ctx(&reg);
    let (trigger, token) = CancelToken::new();
    ctx.cancel = token;
    let written = project.clone();
    let canceller = thread::spawn(move || {
        let paused = wait_until(DEADLINE, || held.exists());
        let wrote_all = paused
            && temp_files(&written).iter().any(|temp| {
                fs::metadata(temp)
                    .is_ok_and(|meta| meta.permissions().mode() & EXECUTABLE_BITS != 0)
            });
        trigger.cancel();
        wrote_all
    });

    let result = run_tool(
        &reg,
        &ctx,
        IMPORT_TOOL,
        json!({ "id": id, "paths": CHANGED_PATHS }),
    );
    assert!(
        canceller.join().unwrap(),
        "the import did not write its new files"
    );
    let cancelled = result.unwrap_err();
    for part in [NOTHING_IMPORTED, CANCELLED] {
        assert!(cancelled.contains(part), "{part} missing from: {cancelled}");
    }
    let settled = wait_until(DEADLINE, || contents(&project) == before);
    assert!(settled, "the import left {:?}", temp_files(&project));
}

/// Cancellation must remove the incomplete snapshot and its dependencies immediately.
#[test]
fn a_cancel_while_the_snapshot_is_made_discards_it() {
    let coding = Coding::new();
    let started = tempdir().unwrap();
    let marker = started.path().join(PREPARE_STARTED);
    let prepare = format!("touch '{}'; {PREPARE_NEVER_ENDS}", marker.display());
    let (reg, _host) = coding.host(&[("prepare", json!(prepare))]);
    let mut ctx = coding.ctx(&reg);
    let (trigger, token) = CancelToken::new();
    ctx.cancel = token;
    let canceller = thread::spawn(move || {
        let preparing = wait_until(DEADLINE, || marker.exists());
        trigger.cancel();
        preparing
    });

    let input = json!({ "prompt": CODE_EDIT, "model": MODEL_ALIAS, "profile": CODE_PROFILE });
    let result = run_tool(&reg, &ctx, TOOL, input);
    assert!(canceller.join().unwrap(), "prepare did not start");
    // When the cancel lands decides the text: the runtime's cancel note, or
    // the error of a step whose process it stopped.
    assert!(result.is_err(), "the call was cancelled");
    let artifacts = coding.artifacts.path().to_owned();
    let emptied = wait_until(DEADLINE, || {
        fs::read_dir(&artifacts).unwrap().next().is_none()
    });
    assert!(emptied, "these files stay: {:?}", listing(&artifacts));
}

/// A hook policy keeps on writes into the probe directory before the coding
/// call is stopped. The hook's file stays in the directory the reply names,
/// but the stopped call removes its artifact. With one slot, the next call
/// starts only after that cleanup.
#[test]
fn a_refused_coding_call_keeps_what_a_policy_hook_wrote() {
    let coding = Coding::new();
    add_a_managed_hook(&coding.fake);
    let (reg, _host) = coding.host(&[]);
    let err = coding.code(&reg, CODE_EDIT, &[]).unwrap_err();
    let ran_in: Vec<PathBuf> = coding
        .fake
        .log("hook_ran_in")
        .lines()
        .map(PathBuf::from)
        .collect();
    let next = json!({ "prompt": ANSWERS, "model": MODEL_ALIAS });
    let _ = run_tool(&reg, &coding.ctx(&reg), TOOL, next);

    let kept = ran_in.iter().all(|dir| dir.join(HOOK_SENTINEL).exists());
    for dir in coding.fake.log("hook_ran_in").lines().map(Path::new) {
        let _ = fs::remove_file(dir.join(HOOK_SENTINEL));
        let _ = fs::remove_dir(dir);
    }
    let [dir] = ran_in.as_slice() else {
        panic!("the hook ran in {ran_in:?}");
    };
    assert!(
        err.contains(&format!("{LEFT_BEHIND}{}", dir.display())),
        "got: {err}"
    );
    assert!(kept, "the file that the hook wrote in {dir:?} is missing");
}

/// Preparation changes belong to the worker baseline. They must stay outside the import.
#[test]
fn what_prepare_writes_is_not_offered_for_import() {
    let coding = Coding::new();
    let (reg, _host) = coding.host(&[("prepare", json!(PREPARE_WRITES_LOCK))]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();

    for change in REPORTED_CHANGES {
        assert!(reply.contains(change), "{change} missing from: {reply}");
    }
    assert!(
        !reply.contains(&format!("A {PREPARED_FILE}")),
        "the reply gives the files of prepare for import: {reply}"
    );
    assert!(reply.contains(PREPARED_NOTE), "got: {reply}");
}

/// The change report names the artifact the import needs, so Claude's text
/// is shortened to make room and the reply stays within the output limits.
#[test]
fn the_change_report_fits_within_the_output_limits() {
    let coding = Coding::new();
    let (reg, _host) = coding.host(&[("max_output_lines", json!(REPLY_LINES))]);
    let reply = coding.code(&reg, CODE_LONG, &[]).unwrap();

    let lines = reply.lines().count();
    assert!(
        lines <= REPLY_LINES + TRUNCATION_LINES,
        "{lines} lines: {reply}"
    );
    assert!(reply.contains(ARTIFACT_LINE), "got: {reply}");
    assert!(reply.contains(REPORTED_CHANGES[0]), "got: {reply}");
}

/// A cancel after the worker started keeps its snapshot, work included, as
/// the reply says. With one slot, the next call starts only after the
/// cancelled call's cleanup, which proves the snapshot survived it.
#[test_case(CODE_HANG ; "ordinary_changes")]
#[test_case("code_nested_cancel" ; "nested_repository_metadata")]
fn a_cancel_during_the_run_keeps_the_workers_snapshot(prompt: &str) {
    let coding = Coding::new();
    let (reg, _host) = coding.host(&[]);
    let mut ctx = coding.ctx(&reg);
    let (trigger, token) = CancelToken::new();
    ctx.cancel = token;
    let working = coding.fake.path(WORKING);
    let canceller = thread::spawn(move || {
        let started = wait_until(DEADLINE, || working.exists());
        trigger.cancel();
        started
    });

    let input = json!({ "prompt": prompt, "model": MODEL_ALIAS, "profile": CODE_PROFILE });
    let err = run_tool(&reg, &ctx, TOOL, input).unwrap_err();
    assert!(canceller.join().unwrap(), "the worker did not start");
    assert!(err.contains(SNAPSHOT_STAYS), "got: {err}");
    let next = json!({ "prompt": ANSWERS, "model": MODEL_ALIAS });
    run_tool(&reg, &coding.ctx(&reg), TOOL, next).unwrap();
    let kept = listing(coding.artifacts.path());
    let [artifact] = kept.as_slice() else {
        panic!("kept {kept:?}");
    };
    let lib = artifact.join(SNAPSHOT_DIR).join("src/lib.rs");
    assert!(fs::read_to_string(lib).unwrap().ends_with(WORKER_EDIT));
    if prompt == "code_nested_cancel" {
        let snapshot = artifact.join(SNAPSHOT_DIR);
        assert!(!snapshot.join("nested/.git").exists());
        assert_eq!(
            fs::read_to_string(snapshot.join(NESTED_FILE)).unwrap(),
            NESTED_WORK
        );
        git(&snapshot, &["status", "--porcelain"]);
        assert!(!coding.fake.path(METADATA_MARKER).exists());
    }
}

/// Every change can land and the command still fail. The reply says so,
/// and must not report a clean import.
#[test]
fn an_import_that_landed_and_still_failed_says_so() {
    let coding = Coding::new();
    let bash = BASH_FAILING_AFTER.replace("@FAILED@", FAILED_AFTER_WRITES);
    let (reg, _host) = coding.host_with(&[], &bash);
    let id = artifact_id(&coding.code(&reg, CODE_EDIT, &[]).unwrap());
    let reply = coding
        .import(&reg, json!({ "id": id, "paths": [IMPORTED_LIB] }))
        .unwrap();

    assert!(reply.contains(FAILED_AFTER_WRITES), "got: {reply}");
    assert_eq!(
        fs::read_to_string(coding.project().join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
}

/// The import copies and checks every blob from the artifact before any
/// file changes, so removing the artifact during approval changes nothing.
#[test]
fn an_artifact_gone_during_approval_changes_no_file() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let artifact = coding.artifacts.path().join(&id);
    let before = contents(&coding.project());
    let err = coding
        .import_approved_after(&reg, &id, &CHANGED_PATHS, move || {
            fs::remove_dir_all(&artifact).unwrap();
        })
        .unwrap_err();

    assert!(err.contains(NOTHING_IMPORTED), "got: {err}");
    assert_eq!(contents(&coding.project()), before);
}

/// Approval can outlast artifact expiry. Refresh the artifact timestamp at import startup
/// so concurrent cleanup preserves it.
#[test]
fn a_sweep_while_approval_waits_keeps_the_artifact() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    backdate(&coding.artifacts.path().join(&id).join(MANIFEST));
    let (sweeping_reg, sweeping_ctx) = (Arc::clone(&reg), coding.ctx(&reg));
    coding
        .import_approved_after(&reg, &id, &[IMPORTED_LIB], move || {
            let input =
                json!({ "prompt": CODE_EDIT, "model": MODEL_ALIAS, "profile": CODE_PROFILE });
            run_tool(&sweeping_reg, &sweeping_ctx, TOOL, input).unwrap();
        })
        .unwrap();

    assert_eq!(
        fs::read_to_string(coding.project().join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
}

/// A chmod leaves a file's bytes alone, so the import also compares
/// executable bits, after approval too.
#[test]
fn an_import_refuses_a_file_made_executable_during_approval() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let target = coding.project().join(IMPORTED_LIB);
    let chmodded = target.clone();
    let err = coding
        .import_approved_after(&reg, &id, &[IMPORTED_LIB], move || {
            fs::set_permissions(&chmodded, fs::Permissions::from_mode(EXECUTABLE_MODE)).unwrap()
        })
        .unwrap_err();

    assert!(
        err.contains(CHANGED_IN_CHECKOUT) && err.contains(IMPORTED_LIB),
        "got: {err}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), DIRTY_LIB);
    let mode = fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(
        mode & EXECUTABLE_BITS,
        EXECUTABLE_BITS,
        "the import changed the mode back"
    );
}

/// A symlink swap must not redirect import writes outside the validated checkout.
#[test_case("src" ; "a_folder_above_it")]
#[test_case("" ; "the_checkout")]
fn an_import_refuses_a_link_swapped_in_during_approval(swapped: &str) {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let outside = tempdir().unwrap();
    let copy = outside.path().join(OUTSIDE_COPY);
    let original: PathBuf = coding.project().join(swapped).components().collect();
    let moved: PathBuf = copy.join(swapped).components().collect();
    let err = coding
        .import_approved_after(&reg, &id, &[IMPORTED_LIB], move || {
            fs::create_dir_all(moved.parent().unwrap()).unwrap();
            fs::rename(&original, &moved).unwrap();
            symlink(&moved, &original).unwrap();
        })
        .unwrap_err();

    assert!(err.contains(NOTHING_IMPORTED), "got: {err}");
    assert_eq!(
        fs::read_to_string(copy.join(IMPORTED_LIB)).unwrap(),
        DIRTY_LIB
    );
}

/// `prepare` runs outside the sandbox and can create external links. Remove these links
/// before the worker starts.
#[test]
fn a_link_prepare_makes_out_of_the_snapshot_is_dropped() {
    let coding = Coding::new();
    let prepare = format!("ln -s {OUTSIDE_TARGET} {PREPARED_LINK}");
    let (reg, _host) = coding.host(&[("prepare", json!(prepare))]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();

    let snapshot = coding
        .artifacts
        .path()
        .join(artifact_id(&reply))
        .join(SNAPSHOT_DIR);
    assert!(
        fs::symlink_metadata(snapshot.join(PREPARED_LINK)).is_err(),
        "the link stayed in the snapshot"
    );
    assert!(
        reply.contains(LINKS_LEFT_OUT) && reply.contains(PREPARED_LINK),
        "got: {reply}"
    );
}

/// `prepare` runs as the user, outside the sandbox, so it sees maki's
/// environment and not only the variables that reach Claude Code.
#[test]
fn prepare_runs_with_the_users_environment() {
    let coding = Coding::new();
    let prepare = format!("printf %s \"${USER_ONLY_VARIABLE}\" > {PREPARED_ENV}");
    let (reg, _host) = coding.host(&[("prepare", json!(prepare))]);
    let id = artifact_id(&coding.code(&reg, CODE_EDIT, &[]).unwrap());
    let snapshot = coding.artifacts.path().join(id).join(SNAPSHOT_DIR);

    let seen = fs::read_to_string(snapshot.join(PREPARED_ENV)).unwrap();
    assert_eq!(seen, env::var(USER_ONLY_VARIABLE).unwrap());
}

/// If the prepare step fails or times out, the checks it enables are
/// unavailable, and both the worker and the reply are told.
#[test_case("echo broken >&2; exit 7", 60, "broken" ; "that_fails")]
#[test_case(PREPARE_NEVER_ENDS, 1, NO_ANSWER_IN_TIME ; "that_runs_out_of_time")]
#[test_case(EXITS_AS_A_TIMEOUT_WOULD, 60, EXITED_WITH_124 ; "that_exits_as_a_timeout_would")]
fn a_failed_prepare_reaches_the_worker_and_the_reply(command: &str, limit_secs: u64, detail: &str) {
    let coding = Coding::new();
    let command = format!("{PREPARE_WRITES_LOCK}; {command}");
    let (reg, _host) = coding.host(&[
        ("prepare", json!(command)),
        ("prepare_timeout_secs", json!(limit_secs)),
    ]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();
    assert!(
        reply.contains(PREPARE_FAILED) && reply.contains(detail),
        "got: {reply}"
    );
    assert!(
        coding.fake.log("prompt").contains(PREPARE_TOLD),
        "maki did not tell the worker"
    );
    assert!(reply.contains(PREPARED_NOTE), "{reply}");
    assert!(!reply.contains(&format!("A {PREPARED_FILE}")), "{reply}");
    let snapshot = coding
        .artifacts
        .path()
        .join(artifact_id(&reply))
        .join(SNAPSHOT_DIR);
    assert!(snapshot.join(PREPARED_FILE).exists());
}

#[test_case(false ; "successful_command")]
#[test_case(true ; "failed_command")]
fn an_unrecorded_prepare_baseline_stops_the_worker(fails: bool) {
    let coding = Coding::new();
    let command = if fails {
        format!("{BREAK_BASELINE}; {EXITS_AS_A_TIMEOUT_WOULD}")
    } else {
        BREAK_BASELINE.to_owned()
    };
    let (reg, _host) = coding.host(&[("prepare", json!(command))]);
    let error = coding.code(&reg, CODE_EDIT, &[]).unwrap_err();

    assert!(error.contains(CANNOT_RECORD_PREPARED), "{error}");
    assert!(coding.fake.log("prompt").is_empty());
}

#[test_case(false ; "included_dependency")]
#[test_case(true ; "prepared_dependency")]
fn dependency_exclusions_do_not_depend_on_copying(prepared: bool) {
    let coding = Coding::new();
    if !prepared {
        write(&coding.project().join(DEPENDENCY), DEPENDENCY_CONTENT);
    }
    let mut options = vec![("dependencies", json!(NODE_MODULES))];
    if prepared {
        options.push(("prepare", json!(CREATE_DEPENDENCY)));
    }
    let (reg, _host) = coding.host(&options);
    let included = if prepared {
        &[][..]
    } else {
        &[NODE_MODULES][..]
    };
    let reply = coding.code(&reg, CODE_EDIT, included).unwrap();

    assert!(coding.fake.log("snapshot_listing").contains(DEPENDENCY));
    assert!(!reply.contains(&format!("M {DEPENDENCY}")), "{reply}");
    assert!(!reply.contains(&format!("A {DEPENDENCY}")), "{reply}");
}

/// Dependency paths stay outside the import. Python virtual environment scripts contain
/// absolute paths, so preparation must recreate them.
#[test]
fn dependencies_come_along_but_never_as_changes() {
    let coding = Coding::new();
    let project = coding.project();
    write(&project.join(DEPENDENCY), DEPENDENCY_CONTENT);
    write(&project.join(".venv/pyvenv.cfg"), "home = /usr/bin\n");
    let (reg, _host) = coding.host(&[("dependencies", json!(DEPENDENCIES_WITH_A_VENV))]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();

    let listing = coding.fake.log("snapshot_listing");
    assert!(
        listing
            .lines()
            .any(|line| line == format!("./{DEPENDENCY}")),
        "{listing}"
    );
    assert!(
        !listing.lines().any(|line| line.starts_with("./.venv")),
        "{listing}"
    );
    assert!(reply.contains(VENV_NOTE), "got: {reply}");
    assert!(
        !reply.contains(&format!("{NODE_MODULES}/")),
        "a dependency shows as a change: {reply}"
    );
    assert_eq!(
        fs::read_to_string(project.join(DEPENDENCY)).unwrap(),
        DEPENDENCY_CONTENT
    );
}

/// Everything the collect step writes lies outside the worker's directory,
/// so a link the worker plants there cannot redirect the write into the
/// checkout.
#[test]
fn collecting_changes_never_follows_the_workers_links() {
    let coding = Coding::new();
    let project = coding.project();
    coding.fake.write("project", &project.display().to_string());
    let before = contents(&project);
    let (reg, _host) = coding.host(&[]);
    let reply = coding.code(&reg, CODE_REDIRECT, &[]).unwrap();

    assert_eq!(contents(&project), before, "the checkout must not change");
    assert!(reply.contains(REPORTED_CHANGES[0]), "got: {reply}");
}

/// A dependency directory is copied with the project's exclusions, so a
/// secret in it never reaches the worker's shell.
#[test]
fn dependency_copies_keep_the_snapshot_exclusions() {
    let (dependencies, deny_read) = (NODE_MODULES, DEPENDENCY_DENIED);
    let coding = Coding::new();
    let project = coding.project();
    write(&project.join(DEPENDENCY), DEPENDENCY_CONTENT);
    write(&project.join(DEPENDENCY_SECRET), "SECRET=1\n");
    write(&project.join(DEPENDENCY_DENIED), "key\n");
    let (reg, _host) = coding.host(&[
        ("dependencies", json!(dependencies)),
        ("deny_read", json!(deny_read)),
    ]);
    coding.code(&reg, CODE_EDIT, &[]).unwrap();

    let listing = coding.fake.log("snapshot_listing");
    let listed = |path: &str| listing.lines().any(|line| line == format!("./{path}"));
    assert!(listed(DEPENDENCY), "{listing}");
    for left_out in [DEPENDENCY_SECRET, DEPENDENCY_DENIED] {
        assert!(
            !listed(left_out),
            "{left_out} must stay out of the snapshot"
        );
    }
}

/// Dependency names can resemble `find` expressions. They must remain path arguments
/// regardless of project ignore rules.
#[test_case(EXPRESSION_NAMED_DIR, EXPRESSION_NAMED_DEPENDENCY ; "named_like_an_expression")]
#[test_case(UNICODE_DEPENDENCY_DIR, UNICODE_DEPENDENCY ; "named_outside_ascii")]
fn a_dependency_dir_is_copied_and_never_a_change(dir: &str, file: &str) {
    let coding = Coding::new();
    write(&coding.project().join(file), DEPENDENCY_CONTENT);
    let (reg, _host) = coding.host(&[("dependencies", json!(dir))]);
    let reply = coding.code(&reg, CODE_EDIT, &[]).unwrap();

    let listing = coding.fake.log("snapshot_listing");
    assert!(
        listing.lines().any(|line| line == format!("./{file}")),
        "{listing}"
    );
    assert!(reply.contains(REPORTED_CHANGES[0]), "got: {reply}");
    assert!(!reply.contains(file), "got: {reply}");
    assert!(!reply.contains(UNSETTLED_NOTE), "got: {reply}");
}

/// The next coding call removes an artifact untouched for longer than its
/// time limit, and keeps its own. An artifact is dated by its manifest,
/// which an import rewrites, or by its marker if it stopped before the
/// collect step. The artifact directory can be a user folder with other
/// contents, so a folder the plugin did not make stays. The sweep removes in
/// the background, so a second stale artifact shows when it has run.
#[test_case(&[ARTIFACT_MARKER, MANIFEST], false, true ; "a_stale_artifact")]
#[test_case(&[ARTIFACT_MARKER], false, true ; "one_that_died_before_collecting")]
#[test_case(&[ARTIFACT_MARKER], true, false ; "one_imported_from_lately")]
#[test_case(&[], false, false ; "an_empty_folder")]
#[test_case(&[MANIFEST], false, false ; "a_folder_with_a_manifest")]
fn the_sweep_takes_only_stale_artifacts(files: &[&str], fresh_manifest: bool, swept: bool) {
    let coding = Coding::new();
    let old = old_folder(coding.artifacts.path(), OLD_FOLDER, files);
    let control = old_folder(
        coding.artifacts.path(),
        SWEPT_FOLDER,
        &[ARTIFACT_MARKER, MANIFEST],
    );
    if fresh_manifest {
        fs::write(old.join(MANIFEST), "{}").unwrap();
    }
    let (reg, _host) = coding.host(&[]);
    coding.code(&reg, CODE_EDIT, &[]).unwrap();

    assert!(
        wait_until(DEADLINE, || !control.exists()),
        "the sweep did not run"
    );
    if swept {
        assert!(wait_until(DEADLINE, || !old.exists()), "{old:?} stayed");
    }
    assert_eq!(old.exists(), !swept, "{old:?}");
    for file in files {
        assert_eq!(old.join(file).exists(), !swept, "{file}");
    }
    let kept = fs::read_dir(coding.artifacts.path()).unwrap().count();
    assert_eq!(
        kept,
        if swept { 1 } else { 2 },
        "the artifact of the call must stay"
    );
}

/// An artifact directory inside the checkout can expose project files to cleanup.
#[test_case(false ; "that_does_not_exist_yet")]
#[test_case(true ; "that_holds_an_old_artifact")]
fn an_artifact_dir_inside_the_checkout_changes_nothing_there(exists: bool) {
    let coding = Coding::new();
    let inside = coding.project().join(INSIDE_ARTIFACTS);
    let old = exists.then(|| {
        fs::create_dir(&inside).unwrap();
        old_folder(&inside, OLD_FOLDER, &[ARTIFACT_MARKER])
    });
    let (reg, _host) = coding.host(&[("artifact_dir", json!(inside))]);
    let err = coding.code(&reg, CODE_EDIT, &[]).unwrap_err();

    assert!(err.contains(ARTIFACTS_INSIDE), "got: {err}");
    assert_eq!(inside.exists(), exists, "maki made the artifact directory");
    if let Some(old) = old {
        assert!(
            old.exists(),
            "the sweep removed an artifact in the checkout"
        );
    }
}

/// The checkout reaches past a session in a subdirectory or a worktree, so
/// an artifact directory elsewhere in it is refused too.
#[test_case(true ; "from_a_worktree")]
#[test_case(false ; "from_a_subdirectory")]
fn an_artifact_dir_beside_the_session_in_the_checkout_is_refused(from_worktree: bool) {
    let coding = Coding::new();
    let inside = coding.project().join(INSIDE_ARTIFACTS);
    let outside = tempdir().unwrap();
    let (reg, _host) = coding.host(&[("artifact_dir", json!(inside))]);

    let (_, result) = coding.code_away_from_root(&reg, outside.path(), from_worktree);

    let err = result.unwrap_err();
    assert!(err.contains(ARTIFACTS_INSIDE), "got: {err}");
    assert!(!inside.exists(), "maki made the artifact directory");
}

/// An inaccessible parent can hide an existing artifact directory. Refuse the path before
/// creation or cleanup.
#[test_case(true, LINK_TO_NOWHERE ; "through_a_dangling_link")]
#[test_case(false, CANNOT_READ ; "through_an_unreadable_folder")]
fn an_artifact_dir_maki_cannot_follow_is_refused(dangling: bool, problem: &str) {
    if !dangling && !mode_bits_hold() {
        return;
    }
    let coding = Coding::new();
    let base = tempdir().unwrap();
    let blocker = base.path().join(BLOCKER);
    if dangling {
        symlink(base.path().join(NOWHERE), &blocker).unwrap();
    } else {
        fs::create_dir(&blocker).unwrap();
        fs::set_permissions(&blocker, fs::Permissions::from_mode(NO_ACCESS_MODE)).unwrap();
    }
    let artifacts = blocker.join(INSIDE_ARTIFACTS);
    let (reg, _host) = coding.host(&[("artifact_dir", json!(artifacts))]);
    let err = coding.code(&reg, CODE_EDIT, &[]).unwrap_err();

    assert!(err.contains(problem), "got: {err}");
    assert!(err.contains(base.path().to_str().unwrap()), "got: {err}");
}

/// A `..` component after a nonexistent directory has no resolved target. Refuse these
/// artifact paths at plugin load.
#[test_case(false, ARTIFACT_DIR_RELATIVE ; "a_relative_path")]
#[test_case(true, ARTIFACT_DIR_CLIMBS ; "a_path_that_climbs")]
fn an_artifact_dir_it_cannot_trust_is_refused(climbs: bool, problem: &str) {
    let coding = Coding::new();
    let base = tempdir().unwrap();
    let dir = if climbs {
        base.path().join(NOWHERE).join("..").join(INSIDE_ARTIFACTS)
    } else {
        PathBuf::from(INSIDE_ARTIFACTS)
    };
    let mut opts = coding.fake.opts(ONE_SLOT);
    opts.insert("artifact_dir".into(), json!(dir));
    let err = try_load(opts)
        .err()
        .expect("the plugin must refuse the option");

    assert!(err.contains(problem), "got: {err}");
    assert_eq!(
        fs::read_dir(base.path()).unwrap().count(),
        0,
        "maki made a folder"
    );
}

fn old_folder(root: &Path, name: &str, files: &[&str]) -> PathBuf {
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

/// Backdates `path` past the sweep's time limit.
fn backdate(path: &Path) {
    let written = SystemTime::now() - Duration::from_secs(STALE_AGE_HOURS * SECS_PER_HOUR);
    File::open(path).unwrap().set_modified(written).unwrap();
}

/// Inputs must lie inside the project and only make sense for a snapshot.
/// A bad input is refused before any process starts.
#[test_case(CODE_PROFILE, "../outside", "goes out of the project" ; "a_path_out_of_the_project")]
#[test_case("read", "fixtures", "only the code profile can use `include`" ; "an_input_to_a_read_task")]
fn a_wrong_input_is_refused_before_anything_runs(profile: &str, include: &str, want: &str) {
    let coding = Coding::new();
    let (reg, _host) = coding.host(&[]);
    let input = json!({ "prompt": CODE_EDIT, "profile": profile, "include": [include] });
    let err = run_tool(&reg, &coding.ctx(&reg), TOOL, input).unwrap_err();
    assert!(err.contains(want), "got: {err}");
    assert_eq!(coding.fake.log("calls"), "", "no process must run");
}

/// The probe checks the start the run will make, so both start with the
/// same environment, the worker's temporary directory included.
#[test]
fn the_probe_starts_as_the_worker_does() {
    let coding = Coding::new();
    let (reg, _host) = coding.host(&[]);
    coding.code(&reg, CODE_EDIT, &[]).unwrap();

    let tmpdirs = coding.fake.log("tmpdirs");
    let started: Vec<&str> = tmpdirs.lines().collect();
    assert_eq!(started.len(), 2, "{tmpdirs}");
    assert_eq!(started[0], started[1]);
}

/// A session that names its project through a link can still import. The
/// project is spelled as `getcwd` spells it, like the import command does.
#[test]
fn a_session_in_a_linked_project_imports() {
    let (coding, reg, _host, id) = Coding::coded(CODE_EDIT);
    let links = tempdir().unwrap();
    let linked = links.path().join(LINKED_PROJECT);
    symlink(coding.project(), &linked).unwrap();
    let mut ctx = ctx_in(&linked, None, None);
    ctx.registry = Arc::clone(&reg);

    run_tool(&reg, &ctx, IMPORT_TOOL, json!({ "id": id })).unwrap();
    assert_eq!(
        fs::read_to_string(coding.project().join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
}

#[test]
fn agent_config_the_worker_writes_stays_out_of_the_changes() {
    let (coding, reg, _host, id) = Coding::coded(CODE_CLAUDE_CONFIG);
    coding.import(&reg, json!({ "id": id })).unwrap();

    let project = coding.project();
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        WORKER_LIB
    );
    assert!(!project.join(".claude/settings.local.json").exists());
    assert!(!project.join("src/.claude").exists());
    assert!(!project.join("tools/.claude").exists());
    assert!(!project.join(".maki/init.lua").exists());
    assert!(!project.join("src/.MaKi").exists());
    assert!(!project.join(".Claude").exists());
}

/// A file that became a folder, and a name that the manifest cannot hold, are
/// left to the user with a note. A name with an escape byte shows escaped,
/// and the import finds it by that spelling and lands it with all its bytes.
/// The other changes land.
#[test]
fn changes_the_import_cannot_apply_are_left_to_the_user() {
    let coding = Coding::new();
    let (reg, _host) = coding.host(&[]);
    let reply = coding.code(&reg, CODE_RETYPE, &[]).unwrap();
    for line in RETYPED_REPORT.into_iter().chain([NOT_UTF8_NOTE]) {
        assert!(reply.contains(line), "{line} missing from: {reply}");
    }
    assert!(!reply.contains('\x1b'), "got: {reply:?}");

    let id = artifact_id(&reply);
    let paths = [IMPORTED_LIB, ADDED_FILE, ESCAPED_NAME];
    let imported = coding
        .import(&reg, json!({ "id": id, "paths": paths }))
        .unwrap();
    assert!(imported.contains(RETYPED_SKIPPED), "got: {imported}");
    let project = coding.project();
    assert_eq!(
        fs::read_to_string(project.join(IMPORTED_LIB)).unwrap(),
        DIRTY_LIB
    );
    assert_eq!(
        fs::read_to_string(project.join(ADDED_FILE)).unwrap(),
        WORKER_NEW
    );
    assert!(project.join(ESCAPE_NAME).exists());
}

/// Worker paths can contain shell syntax. Preserve their bytes through collection, manifest
/// validation and import without execution.
#[test]
fn a_hostile_file_name_lands_through_the_whole_import() {
    let (coding, reg, _host, id) = Coding::coded(CODE_HOSTILE);
    coding.import(&reg, json!({ "id": id })).unwrap();

    let project = coding.project();
    assert_eq!(
        fs::read_to_string(project.join(HOSTILE_NAME)).unwrap(),
        HOSTILE_CONTENT
    );
    assert!(
        contents(&project)
            .keys()
            .all(|path| path.file_name().is_none_or(|name| name != PWNED)),
        "a name ran a command"
    );
}

#[test]
fn worker_attributes_cannot_hide_text_changes_from_review() {
    let (coding, _reg, _host, id) = Coding::coded("code_attributes");
    let artifact = coding.artifacts.path().join(id);
    let manifest: Value =
        serde_json::from_slice(&fs::read(artifact.join(MANIFEST)).unwrap()).unwrap();
    let change = manifest["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == IMPORTED_LIB)
        .unwrap();
    assert_eq!(change["kind"], "text");
    let diff = git(
        &artifact.join(SNAPSHOT_DIR),
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "HEAD",
            "--",
            IMPORTED_LIB,
        ],
    );
    assert!(diff.contains("+worker"), "{diff}");
}
