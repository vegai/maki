//! Tests execute the import on real checkout and artifact repositories. Wrapped commands
//! pause at filesystem operations so tests can inject concurrent edits.

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

const HELD: &str = r#"#!/bin/sh
case " $* " in
*"@HOLD_ON@"*) "@REAL@" "$@" || exit; touch "@HELD@"; @WAIT_FOR_RELEASE@ ;;
*) exec "@REAL@" "$@" ;;
esac
"#;
/// Runs the installed tool, but fails when its arguments contain @FAIL_ON@.
const FAILS: &str = r#"#!/bin/sh
case " $* " in
*"@FAIL_ON@"*) echo "@CAUSE@" >&2; exit 1 ;;
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
const AFTER_KEEP: Hold = ("ln", " ./src/lib.rs ");
const AFTER_MOVE: Hold = ("mv", " ./src/lib.rs ");
const DISPLACED: &str = "displaced";
const EDITOR_SAVE: &str = "editor-save";
const CANNOT_PRESERVE: &str = "cannot preserve open-file writes";
const NEW_FILE_PREFIX: &str = ".maki-import-";
/// The last new file gets its mode, in the folder the command made for it,
/// after the file beside the edited one was written.
const WRITING: Hold = ("chmod", " -x -- ./src/new/");
/// Shell paths can contain arbitrary bytes. Long Lua strings preserve them in the generated
/// import and cleanup commands.
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
const BIDI_EDITED: &str = "src/a\u{202e}b\u{200b}.rs";
const BIDI_ADDED: &str = "src/c\u{2066}d\u{e0061}.rs";
const LINK_FAILURE: &str = "ln: fixture preservation failure";
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
            .replace("@CAUSE@", LINK_FAILURE)
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

    fn run(&self, script: &str) -> Output {
        fs::File::create(self.marker("release")).unwrap();
        self.command(script).output().unwrap()
    }

    fn release(&self, mut held: Held) -> Output {
        fs::File::create(self.marker("release")).unwrap();
        held.0.take().unwrap().wait_with_output().unwrap()
    }
}

/// The guard must kill the paused command's entire group if a test fails before release.
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

/// Relative paths must keep writes in the original checkout if its directory becomes a symlink.
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

/// A failed installation preserves earlier changes and all original files.
#[test]
fn a_link_that_fails_partway_keeps_what_landed_and_every_original() {
    let import = Import::new(STAGING);
    let added = import.project.join(ADDED);
    import.failing("ln", &Path::new(CURRENT_DIR).join(ADDED));

    let out = import.run(&import.script);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(import.target()).unwrap(), RECORDED);
    assert!(!added.exists());
    let kept = import.artifact.join(ORIGINALS).join(STAGE).join(TARGET);
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
#[test_case(LAST_CHECK ; "after_validation")]
#[test_case(AFTER_KEEP ; "after_backup")]
#[test_case(AFTER_MOVE ; "after_removal")]
fn a_write_through_a_descriptor_opened_before_is_kept_in_the_artifact(hold: Hold) {
    let import = Import::new(hold);
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
        fs::read_to_string(import.artifact.join(ORIGINALS).join(STAGE).join(TARGET)).unwrap(),
        NEWER
    );
}

#[test_case(AFTER_KEEP, true ; "before_removal")]
#[test_case(AFTER_MOVE, false ; "after_removal")]
fn an_atomic_editor_save_survives_import(hold: Hold, succeeds: bool) {
    let import = Import::new(hold);
    let child = import.start_held();
    let replacement = import.project.join(EDITOR_SAVE);
    fs::write(&replacement, NEWER).unwrap();
    fs::rename(replacement, import.target()).unwrap();
    let output = import.release(child);

    assert_eq!(output.status.success(), succeeds);
    let saved = if succeeds {
        import.artifact.join(DISPLACED).join(STAGE).join(TARGET)
    } else {
        import.target()
    };
    assert_eq!(fs::read_to_string(saved).unwrap(), NEWER);
    assert_eq!(
        fs::read_to_string(import.artifact.join(ORIGINALS).join(STAGE).join(TARGET)).unwrap(),
        ORIGINAL
    );
}

#[test]
fn a_failed_hard_link_stops_before_any_checkout_write() {
    let import = Import::new(STAGING);
    import.failing("ln", &Path::new(CURRENT_DIR).join(TARGET));
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .open(import.target())
        .unwrap();
    let output = import.run(&import.script);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(CANNOT_PRESERVE));
    assert!(String::from_utf8_lossy(&output.stderr).contains(LINK_FAILURE));
    writer.write_all(NEWER.as_bytes()).unwrap();
    writer.set_len(NEWER.len() as u64).unwrap();
    assert_eq!(fs::read_to_string(import.target()).unwrap(), NEWER);
    assert!(!import.project.join(ADDED).exists());
}

#[test]
fn a_retry_cannot_overwrite_a_preserved_save() {
    let import = Import::new(LAST_CHECK);
    import.failing("ln", Path::new(NEW_FILE_PREFIX));
    let child = import.start_held();
    fs::write(import.target(), NEWER).unwrap();
    let output = import.release(child);
    assert!(!output.status.success());
    assert!(!import.target().exists());

    fs::write(import.target(), ORIGINAL).unwrap();
    fs::create_dir(import.artifact.join(STAGE)).unwrap();
    let output = import.run(&import.script);
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(import.target()).unwrap(), ORIGINAL);
    for saved in [
        import.artifact.join(ORIGINALS).join(STAGE).join(TARGET),
        import.artifact.join(DISPLACED).join(STAGE).join(TARGET),
    ] {
        assert_eq!(fs::read_to_string(saved).unwrap(), NEWER);
    }
}

const RETRY_STAGE: &str = "stage.d4e5f6";

#[test]
fn a_new_attempt_can_import_without_overwriting_earlier_backups() {
    let import = Import::new(LAST_CHECK);
    import.failing("ln", Path::new(NEW_FILE_PREFIX));
    let child = import.start_held();
    fs::write(import.target(), NEWER).unwrap();
    assert!(!import.release(child).status.success());
    fs::write(import.target(), ORIGINAL).unwrap();
    fs::remove_file(import.tools.join("ln")).unwrap();
    fs::create_dir(import.artifact.join(RETRY_STAGE)).unwrap();
    let output = import.run(&import.script.replace(STAGE, RETRY_STAGE));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(import.target()).unwrap(), RECORDED);
    assert_eq!(
        fs::read_to_string(import.artifact.join(ORIGINALS).join(STAGE).join(TARGET)).unwrap(),
        NEWER
    );
    assert_eq!(
        fs::read_to_string(
            import
                .artifact
                .join(ORIGINALS)
                .join(RETRY_STAGE)
                .join(TARGET)
        )
        .unwrap(),
        ORIGINAL
    );
}

#[test]
fn invisible_names_are_visible_in_approval_and_keep_their_bytes_in_a_c_locale() {
    let import = Import::named(STAGING, BIDI_EDITED, BIDI_ADDED);
    for hidden in ['\u{202e}', '\u{200b}', '\u{2066}', '\u{e0061}'] {
        assert!(!import.script.contains(hidden));
    }
    fs::File::create(import.marker("release")).unwrap();
    let output = import
        .command(&import.script)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(import.project.join(BIDI_EDITED)).unwrap(),
        RECORDED
    );
    assert_eq!(
        fs::read_to_string(import.project.join(BIDI_ADDED)).unwrap(),
        RECORDED
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

/// User git config must not affect the artifact repository or the import.
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

/// Paths can contain shell syntax. The import and its cleanup must preserve each path byte
/// without execution.
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
