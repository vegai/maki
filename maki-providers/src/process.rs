//! Finds a program the way `execvp` does, and signals a child's process
//! group only while the child's pid still names that group.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf, absolute};
#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(unix)]
use rustix::fs::{Access, access};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};

const PATH: &str = "PATH";
#[cfg(windows)]
const PATHEXT: &str = "PATHEXT";
const PATHEXT_SEPARATOR: char = ';';

/// Returns the absolute path `name` runs as from `cwd`, using maki's `$PATH`
/// (and `$PATHEXT` on Windows).
pub fn find_program(name: &str, cwd: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let extensions: Option<OsString> = env::var_os(PATHEXT);
    #[cfg(not(windows))]
    let extensions: Option<OsString> = None;
    find_program_in(
        name,
        cwd,
        env::var_os(PATH).as_deref(),
        extensions.as_deref(),
    )
}

/// `find_program` with `path` as `$PATH` and `extensions` as `$PATHEXT`. Like
/// `execvp`, it skips a file that cannot run, so such a file never shadows a
/// program later in the path.
fn find_program_in(
    name: &str,
    cwd: &Path,
    path: Option<&OsStr>,
    extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    let names = spellings(name, extensions);
    let found = if Path::new(name).components().count() > 1 {
        names
            .iter()
            .map(|spelled| cwd.join(spelled))
            .find(|candidate| runnable(candidate))
    } else {
        env::split_paths(path?).find_map(|dir| {
            names
                .iter()
                .map(|spelled| cwd.join(&dir).join(spelled))
                .find(|candidate| runnable(candidate))
        })
    }?;
    absolute(found).ok()
}

/// Windows uses `PATHEXT` to resolve extensionless commands such as `git` to `git.exe`.
fn spellings(name: &str, extensions: Option<&OsStr>) -> Vec<String> {
    match extensions.and_then(OsStr::to_str) {
        Some(extensions) if Path::new(name).extension().is_none() => extensions
            .split(PATHEXT_SEPARATOR)
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("{name}{extension}"))
            .collect(),
        _ => vec![name.to_owned()],
    }
}

fn runnable(path: &Path) -> bool {
    #[cfg(unix)]
    let allowed = access(path, Access::EXEC_OK).is_ok();
    #[cfg(not(unix))]
    let allowed = true;
    path.is_file() && allowed
}

/// A child started as a group leader has its pid as the group id.
#[cfg(unix)]
fn group_of(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// Wait for exit without a reap. The zombie reserves its pid, which identifies the process
/// group. Return false if the wait fails.
#[cfg(unix)]
pub fn wait_without_reaping(pid: u32) -> bool {
    let Some(pid) = group_of(pid) else {
        return false;
    };
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Err(Errno::INTR) => continue,
            result => return result.is_ok(),
        }
    }
}

/// Kills the process group of leader `pid`. Call it only before the leader
/// is reaped, because the kernel can hand a reaped pid to another group.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Some(pid) = group_of(pid) {
        let _ = kill_process_group(pid, Signal::KILL);
    }
    // Hold the process handle until `taskkill` completes. Otherwise the pid can identify
    // another process before `taskkill` sends its signal.
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use test_case::test_case;

    use super::{env, find_program, find_program_in, spellings};

    const PLAIN_DIR: &str = "plain";
    const RUNNABLE_DIR: &str = "runnable";
    const TOOL_NAME: &str = "tool";
    const SHEBANG: &str = "#!/bin/sh\n";
    const PLAIN_MODE: u32 = 0o644;
    const RUNNABLE_MODE: u32 = 0o755;
    const WINDOWS_EXTENSIONS: &str = ".COM;.EXE;;.BAT";

    #[test_case("sh" => true ; "a_name_on_path")]
    #[test_case("no-such-program-maki" => false ; "a_name_not_on_path")]
    fn a_found_program_is_absolute(name: &str) -> bool {
        let cwd = env::current_dir().unwrap();
        let found = find_program(name, &cwd);
        if let Some(path) = &found {
            assert!(path.is_absolute(), "{path:?}");
            assert!(path.ends_with(name));
        }
        found.is_some()
    }

    /// `plain` holds a `tool` that cannot run, ahead of the one in
    /// `runnable`, on a PATH of directories relative to the working
    /// directory.
    #[test_case("tool" => Some(PathBuf::from("runnable/tool")) ; "on_path_past_one_that_cannot_run")]
    #[test_case("./runnable/tool" => Some(PathBuf::from("runnable/tool")) ; "by_a_path_from_the_working_dir")]
    #[test_case("./plain/tool" => None ; "not_by_a_path_that_cannot_run")]
    #[test_case("./missing/tool" => None ; "not_by_a_missing_path")]
    fn only_a_program_that_can_run_is_found(name: &str) -> Option<PathBuf> {
        let root = tempfile::tempdir().unwrap();
        for (dir, mode) in [(PLAIN_DIR, PLAIN_MODE), (RUNNABLE_DIR, RUNNABLE_MODE)] {
            let file = root.path().join(dir).join(TOOL_NAME);
            fs::create_dir(root.path().join(dir)).unwrap();
            fs::write(&file, SHEBANG).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        }
        let path = env::join_paths([PLAIN_DIR, RUNNABLE_DIR]).unwrap();
        find_program_in(name, root.path(), Some(&path), None)
            .map(|found| found.strip_prefix(root.path()).unwrap().to_path_buf())
    }

    /// Windows runs `git` as `git.exe`, trying each `PATHEXT` extension in
    /// order, and runs a name that already has an extension as it is.
    #[test_case("git", Some(WINDOWS_EXTENSIONS) => vec!["git.COM", "git.EXE", "git.BAT"] ; "a_bare_name_on_windows")]
    #[test_case("git.exe", Some(WINDOWS_EXTENSIONS) => vec!["git.exe"] ; "a_name_with_its_extension")]
    #[test_case("git", None => vec!["git"] ; "no_pathext")]
    fn a_name_is_tried_with_each_extension(name: &str, extensions: Option<&str>) -> Vec<String> {
        spellings(name, extensions.map(OsStr::new))
    }
}
