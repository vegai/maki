//! Runs `claude_workspace`'s import command on an on-disk checkout and
//! artifact repository. A wrapped `git` or `chmod` holds the command at a
//! chosen call: right after staging a blob, at its last check, or while it
//! writes its new files. The test then changes the checkout or kills the
//! command there, and checks the outcome.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;
use rustix::process::{Pid, Signal, kill_process_group};
use tempfile::{TempDir, tempdir};
use test_case::test_case;

use super::support::{
    DEADLINE, NOTHING_IMPORTED, ORIGINALS, PWNED, contents, executable, git, listing, on_path,
    wait_for_release, wait_until,
};

/// Runs the installed tool, and when its arguments contain @HOLD_ON@, waits
/// for the release before returning.
const HELD: &str = r#"#!/bin/sh
case " $* " in
*"@HOLD_ON@"*) "@REAL@" "$@" || exit; touch "@HELD@"; @WAIT_FOR_RELEASE@ ;;
*) exec "@REAL@" "$@" ;;
esac
"#;
/// Runs the installed tool, but fails when its arguments contain @FAIL_ON@.
const FAILS: &str = r#"#!/bin/sh
case " $* " in
*"@FAIL_ON@"*) exit 1 ;;
*) exec "@REAL@" "$@" ;;
esac
"#;
/// The tool that holds, and the arguments that trigger the hold.
type Hold = (&'static str, &'static str);
/// The tool the command reads each blob from the artifact with.
const STAGING: Hold = ("git", " cat-file ");
const USER_GIT_CONFIG: &str = "gitconfig";
/// A line git cannot parse, so any git that reads the file fails.
const BROKEN_GIT_CONFIG: &str = "[core\n";
/// The artifact folder where each test's import stages its blobs, named as
/// `mktemp` would.
const STAGE: &str = "stage.a1b2c3";
/// The last check hashes the file the import replaces next.
const LAST_CHECK: Hold = ("git", " -- ./src/lib.rs ");
/// The last new file gets its mode, in the folder the command made for it,
/// after the file beside the edited one was written.
const WRITING: Hold = ("chmod", " -x -- ./src/new/");
/// Writes the command importing the @CHANGES@ changes to @OUT@, and the
/// command cleaning up after the import to @LEFTOVERS@. The names are long
/// strings, so every byte arrives unchanged.
const GENERATE: &str = r#"
local workspace = require("claude_workspace")
local changes = ({
  edit_and_add = {
    { path = [==[@PATH@]==], status = "M", old_sha = "@OLD@", new_sha = "@NEW@", old_mode = "100644", new_mode = "100644" },
    { path = [==[@ADDED@]==], status = "A", new_sha = "@NEW@", new_mode = "100644" },
  },
  mode_only = {
    { path = [==[@PATH@]==], status = "M", old_sha = "@OLD@", new_sha = "@OLD@", old_mode = "100644", new_mode = "100755" },
  },
})["@CHANGES@"]
maki.fs.write("@OUT@", workspace.import_script(changes, "@PROJECT@", "@GIT@", "@ARTIFACT@", "@STAGE@"))
maki.fs.write("@LEFTOVERS@", workspace.leftovers_script(changes, "@PROJECT@", "@STAGE@", { [==[@MADE@]==] }))
"#;
/// The changes an import applies, by their name in `GENERATE`.
const EDIT_AND_ADD: &str = "edit_and_add";
const MODE_ONLY: &str = "mode_only";
const TARGET: &str = "src/lib.rs";
const ADDED: &str = "src/new/added.rs";
/// Names that survive only with proper shell quoting: a quote, a command
/// substitution, a space, a newline and a glob.
const HOSTILE_EDITED: &str = "src/it's $(touch pwned) *\nlib.rs";
const HOSTILE_ADDED: &str = "src/a 'b' $(touch pwned)/*\nadded.rs";
/// A temporary file from another import of the same artifact, beside the
/// target.
const OTHER_IMPORTS_TEMP: &str = ".maki-import-artifact.x9y8z7.q1w2e3";
const ORIGINAL: &str = "the checkout as the snapshot saw it\n";
const NEWER: &str = "the user's newer work\n";
const RECORDED: &str = "the worker's change\n";
const CHANGED_EXIT: i32 = 3;
/// The import names each file relative to the checkout, starting here.
const CURRENT_DIR: &str = ".";
const OWNER_EXECUTE: u32 = 0o100;

/// A checkout with one file, an artifact whose repository holds the worker's
/// version, and the command importing that change.
struct Import {
    root: TempDir,
    edited: &'static str,
    project: PathBuf,
    artifact: PathBuf,
    temp: PathBuf,
    tools: PathBuf,
    script: String,
    leftovers: String,
    recorded_blob: String,
}

impl Import {
    fn new(hold: Hold) -> Self {
        Self::named(hold, TARGET, ADDED)
    }

    /// Like `new`, with the edited file at `edited` and the added file at
    /// `added`, whose folder the command creates.
    fn named(hold: Hold, edited: &'static str, added: &'static str) -> Self {
        Self::with(hold, EDIT_AND_ADD, edited, added)
    }

    fn with(
        (tool, hold_on): Hold,
        changes: &str,
        edited: &'static str,
        added: &'static str,
    ) -> Self {
        let root = tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let (project, artifact, temp, tools) = (
            base.join("project"),
            base.join("artifact"),
            base.join("tmp"),
            base.join("tools"),
        );
        for dir in [&project.join("src"), &artifact, &temp, &tools] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(project.join(edited), ORIGINAL).unwrap();
        let git_dir = artifact.join("git");
        let stage = artifact.join(STAGE);
        fs::create_dir(&stage).unwrap();
        git(&base, &["init", "-q", "--bare", &git_dir.to_string_lossy()]);
        let old = git(
            &base,
            &[
                &format!("--git-dir={}", git_dir.display()),
                "hash-object",
                "-w",
                "--no-filters",
                &project.join(edited).to_string_lossy(),
            ],
        );
        let recorded = base.join("recorded");
        fs::write(&recorded, RECORDED).unwrap();
        let new = git(
            &base,
            &[
                &format!("--git-dir={}", git_dir.display()),
                "hash-object",
                "-w",
                &recorded.to_string_lossy(),
            ],
        );
        let out = base.join("import.sh");
        let leftovers = base.join("leftovers.sh");
        let source = GENERATE
            .replace("@CHANGES@", changes)
            .replace("@STAGE@", &stage.to_string_lossy())
            .replace("@PATH@", edited)
            .replace("@ADDED@", added)
            .replace(
                "@MADE@",
                &project.join(added).parent().unwrap().to_string_lossy(),
            )
            .replace("@LEFTOVERS@", &leftovers.to_string_lossy())
            .replace("@OLD@", &old)
            .replace("@NEW@", &new)
            .replace("@OUT@", &out.to_string_lossy())
            .replace("@PROJECT@", &project.to_string_lossy())
            .replace("@GIT@", &git_dir.to_string_lossy())
            .replace("@ARTIFACT@", &artifact.to_string_lossy());
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source("import_script", &source).unwrap();
        let script = fs::read_to_string(&out).unwrap();
        let leftovers = fs::read_to_string(&leftovers).unwrap();
        let held = HELD
            .replace("@HOLD_ON@", hold_on)
            .replace("@REAL@", &on_path(tool).to_string_lossy())
            .replace("@HELD@", &base.join("held").to_string_lossy())
            .replace("@WAIT_FOR_RELEASE@", &wait_for_release("\"@RELEASE@\""))
            .replace("@RELEASE@", &base.join("release").to_string_lossy());
        executable(&tools, tool, &held);
        Self {
            root,
            edited,
            project,
            artifact,
            temp,
            tools,
            script,
            leftovers,
            recorded_blob: new,
        }
    }

    fn target(&self) -> PathBuf {
        self.project.join(self.edited)
    }

    /// Puts a `tool` first on the command's `PATH` that fails when its
    /// arguments contain `fail_on`.
    fn failing(&self, tool: &str, fail_on: &Path) {
        let script = FAILS
            .replace("@FAIL_ON@", &fail_on.to_string_lossy())
            .replace("@REAL@", &on_path(tool).to_string_lossy());
        executable(&self.tools, tool, &script);
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Starts the command in a new process group and returns once it holds.
    fn start_held(&self) -> Held {
        let child = self
            .command(&self.script)
            .env("TMPDIR", &self.temp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let held = self.marker("held");
        assert!(
            wait_until(DEADLINE, || held.exists()),
            "the import did not come to that point"
        );
        Held(Some(child))
    }

    fn path(&self) -> OsString {
        env::join_paths(
            [self.tools.clone()]
                .into_iter()
                .chain(env::split_paths(&env::var_os("PATH").unwrap())),
        )
        .unwrap()
    }

    /// Returns `bash -c `script`` in the fixture, with the held tools first on
    /// `PATH`.
    fn command(&self, script: &str) -> Command {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(script)
            .current_dir(self.root.path())
            .env("PATH", self.path());
        command
    }

    /// Runs all of `script` in the test's directory without holding it.
    fn run(&self, script: &str) -> Output {
        fs::File::create(self.marker("release")).unwrap();
        self.command(script).output().unwrap()
    }

    fn release(&self, mut held: Held) -> Output {
        fs::File::create(self.marker("release")).unwrap();
        held.0.take().unwrap().wait_with_output().unwrap()
    }
}

/// The command at its hold. Dropping it kills the whole group, as a cancel
/// does, so a test that fails before the release leaves no fake waiting.
struct Held(Option<Child>);

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let group = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
            let _ = kill_process_group(group, Signal::KILL);
            let _ = child.wait();
        }
    }
}

/// Moves `target`, still holding the snapshot's bytes, to `outside`, and puts
/// a link to it in its place.
fn swap_for_a_link(target: &Path, outside: &Path) {
    fs::rename(target, outside).unwrap();
    symlink(outside, target).unwrap();
}

/// Staging reads the artifact and can take long, so the checks come after
/// it. A file that changed or became a link during staging stops the
/// import, which writes nothing, through the link or anywhere else.
#[test_case(false ; "an_edit")]
#[test_case(true ; "a_link_swapped_in")]
fn a_change_while_blobs_stage_is_refused(link: bool) {
    let import = Import::new(STAGING);
    let outside = import.root.path().join("outside.rs");
    let child = import.start_held();
    if link {
        swap_for_a_link(&import.target(), &outside);
    } else {
        fs::write(import.target(), NEWER).unwrap();
    }
    let output = import.release(child);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(CHANGED_EXIT), "{stderr}");
    assert!(stderr.contains(NOTHING_IMPORTED), "{stderr}");
    if link {
        assert_eq!(fs::read_to_string(&outside).unwrap(), ORIGINAL);
    } else {
        assert_eq!(fs::read_to_string(import.target()).unwrap(), NEWER);
    }
}

/// The command enters the checkout before its checks and writes by relative
/// path, so a checkout swapped for a link after the checks cannot redirect
/// the writes to the link's target.
#[test]
fn a_checkout_swapped_for_a_link_after_the_checks_keeps_the_writes() {
    let import = Import::new(LAST_CHECK);
    let moved = import.root.path().join("moved");
    let decoy = import.root.path().join("decoy");
    fs::create_dir_all(decoy.join("src")).unwrap();
    fs::write(decoy.join(TARGET), ORIGINAL).unwrap();
    let child = import.start_held();
    fs::rename(&import.project, &moved).unwrap();
    symlink(&decoy, &import.project).unwrap();
    let output = import.release(child);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(fs::read_to_string(moved.join(TARGET)).unwrap(), RECORDED);
    assert_eq!(fs::read_to_string(moved.join(ADDED)).unwrap(), RECORDED);
    assert_eq!(listing(&decoy.join("src")), [decoy.join(TARGET)]);
    assert_eq!(fs::read_to_string(decoy.join(TARGET)).unwrap(), ORIGINAL);
}

/// A folder swapped for a link after the checks stops the import at its next
/// step, so nothing reaches the link's target.
#[test]
fn a_folder_swapped_for_a_link_after_the_checks_stops_the_import() {
    let import = Import::new(LAST_CHECK);
    let src = import.project.join("src");
    let moved = import.root.path().join("moved");
    let decoy = import.root.path().join("decoy");
    fs::create_dir(&decoy).unwrap();
    fs::write(decoy.join("lib.rs"), ORIGINAL).unwrap();
    let child = import.start_held();
    fs::rename(&src, &moved).unwrap();
    symlink(&decoy, &src).unwrap();
    let output = import.release(child);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(CHANGED_EXIT), "{stderr}");
    assert_eq!(listing(&decoy), [decoy.join("lib.rs")]);
    assert_eq!(fs::read_to_string(decoy.join("lib.rs")).unwrap(), ORIGINAL);
    assert_eq!(fs::read_to_string(moved.join("lib.rs")).unwrap(), ORIGINAL);
}

/// A mode-only change keeps the file's bytes.
#[test]
fn a_mode_change_lands_with_the_same_bytes() {
    let import = Import::with(STAGING, MODE_ONLY, TARGET, ADDED);
    let output = import.run(&import.script);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(fs::read_to_string(import.target()).unwrap(), ORIGINAL);
    let mode = fs::metadata(import.target()).unwrap().permissions().mode();
    assert_ne!(mode & OWNER_EXECUTE, 0, "mode {mode:o}");
}

/// If a rename fails partway, files already renamed stay in place and the
/// rest are untouched. Every file the import replaces was kept in the
/// originals before the first rename.
#[test]
fn a_rename_that_fails_partway_keeps_what_landed_and_every_original() {
    let import = Import::new(STAGING);
    let added = import.project.join(ADDED);
    import.failing("mv", &Path::new(CURRENT_DIR).join(ADDED));

    let out = import.run(&import.script);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(import.target()).unwrap(), RECORDED);
    assert!(!added.exists());
    let kept = import.artifact.join(ORIGINALS).join(TARGET);
    assert_eq!(fs::read_to_string(kept).unwrap(), ORIGINAL);
}

/// A cancel kills the command at once, so none of its traps run. The staged
/// files sit in the artifact, where its sweep removes them, rather than in
/// the temporary directory, which nothing cleans.
#[test]
fn a_killed_import_leaves_nothing_outside_its_artifact() {
    let import = Import::new(STAGING);
    drop(import.start_held());

    assert_eq!(fs::read_dir(&import.temp).unwrap().count(), 0);
    assert_eq!(fs::read_to_string(import.target()).unwrap(), ORIGINAL);
    assert_eq!(
        fs::read_to_string(import.artifact.join(STAGE).join(&import.recorded_blob)).unwrap(),
        RECORDED,
        "the staged blob must wait in the artifact for its expiry"
    );
}

/// A write through a descriptor opened before the import lands in the
/// replaced file, at its new name, even after the import checked it. The
/// replaced file keeps a name in the artifact, so the written data is not
/// lost.
#[test]
fn a_write_through_a_descriptor_opened_before_is_kept_in_the_artifact() {
    let import = Import::new(LAST_CHECK);
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .open(import.target())
        .unwrap();
    let child = import.start_held();
    writer.write_all(NEWER.as_bytes()).unwrap();
    writer.set_len(NEWER.len() as u64).unwrap();
    let output = import.release(child);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(fs::read_to_string(import.target()).unwrap(), RECORDED);
    assert_eq!(
        fs::read_to_string(import.artifact.join(ORIGINALS).join(TARGET)).unwrap(),
        NEWER
    );
}

/// A kill skips the command's cleanup while its new files wait beside their
/// targets, one in a folder the command made. The next cleanup removes both
/// files, the folder and the stage, and leaves alone another import's
/// temporary file from the same artifact.
#[test]
fn what_a_killed_import_wrote_goes_with_the_cleanup_after_it() {
    let import = Import::new(WRITING);
    drop(import.start_held());
    let src = import.project.join("src");
    assert!(
        listing(&src).len() > 2,
        "the kill occurred before the new files: {:?}",
        listing(&src)
    );
    fs::write(src.join(OTHER_IMPORTS_TEMP), RECORDED).unwrap();

    let output = import.run(&import.leftovers);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        listing(&src),
        [src.join(OTHER_IMPORTS_TEMP), src.join("lib.rs")]
    );
    assert_eq!(fs::read_to_string(import.target()).unwrap(), ORIGINAL);
    assert!(!import.artifact.join(STAGE).exists());
}

/// The cleanup never enters a folder that became a link, so its temporary
/// files stay, and the cleanup fails to say so.
#[test]
fn a_cleanup_that_cannot_clear_a_folder_fails() {
    let import = Import::new(WRITING);
    drop(import.start_held());
    let src = import.project.join("src");
    let moved = import.project.join("moved");
    fs::rename(&src, &moved).unwrap();
    symlink(&moved, &src).unwrap();

    let output = import.run(&import.leftovers);
    assert!(
        !output.status.success(),
        "the cleanup exited without an error"
    );
    assert!(
        listing(&moved).len() > 1,
        "the cleanup removed the temporary files through the link: {:?}",
        listing(&moved)
    );
}

/// The command runs git on the artifact's repository without the user's
/// global or system config, like the collect step, so a config git cannot
/// parse does not affect the import.
#[test]
fn an_import_runs_git_without_the_users_config() {
    let import = Import::new(STAGING);
    let config = import.root.path().join(USER_GIT_CONFIG);
    fs::write(&config, BROKEN_GIT_CONFIG).unwrap();
    fs::File::create(import.marker("release")).unwrap();
    let output = import
        .command(&import.script)
        .env("GIT_CONFIG_GLOBAL", &config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(import.target()).unwrap(), RECORDED);
}

/// Names reach the command only through shell quoting. A name with a quote,
/// a command substitution, a space, a newline and a glob arrives byte for
/// byte, and no part of it runs, in the import or its cleanup. The cleanup
/// runs only after a failed import, which the full import in
/// `policy::coding` cannot trigger.
#[test]
fn a_hostile_file_name_lands_as_it_is_and_runs_nothing() {
    let import = Import::named(STAGING, HOSTILE_EDITED, HOSTILE_ADDED);
    for script in [&import.script, &import.leftovers] {
        let output = import.run(script);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert_eq!(fs::read_to_string(import.target()).unwrap(), RECORDED);
    assert_eq!(
        fs::read_to_string(import.project.join(HOSTILE_ADDED)).unwrap(),
        RECORDED
    );
    for path in contents(import.root.path()).keys() {
        assert_ne!(path.file_name().unwrap(), PWNED, "a name ran a command");
    }
}
