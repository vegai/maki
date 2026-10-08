use super::super::error::Error;
#[cfg(target_os = "linux")]
use rustix::io::Errno;
#[cfg(target_os = "linux")]
use rustix::process::{Pid, getuid, test_kill_process};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(unix)]
use std::fs::Permissions;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;
#[cfg(target_os = "linux")]
use std::sync::{Mutex, PoisonError};
use tempfile::TempDir;
#[cfg(target_os = "linux")]
use tracing::{debug, warn};

pub(super) const DIR_PREFIX: &str = "maki-claude-code.";
#[cfg(target_os = "linux")]
pub(super) const OWNER_SEPARATOR: char = '-';
#[cfg(target_os = "linux")]
const PID_NAMESPACE: &str = "/proc/self/ns/pid";
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Creates a file that only the user can read and write, from the moment it
/// exists.
pub(super) fn private_file(path: &Path, content: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(PRIVATE_FILE_MODE);
    let mut file: File = options.open(path)?;
    file.write_all(content.as_bytes())
}

/// Private to the user from the moment it exists. Returns the resolved path,
/// because a link on the way to `base` could make a directory inside
/// `project` look like it is outside.
pub(super) fn private_dir(base: &Path, project: &Path) -> Result<(TempDir, PathBuf), Error> {
    #[cfg(target_os = "linux")]
    sweep_once(base);
    let prefix = dir_prefix();
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix);
    #[cfg(unix)]
    builder.permissions(Permissions::from_mode(PRIVATE_DIR_MODE));
    let dir = builder.tempdir_in(base).map_err(|source| Error::Path {
        what: "make a private directory in",
        path: base.to_owned(),
        source,
    })?;
    let path = dir.path().canonicalize().map_err(|source| Error::Path {
        what: "resolve",
        path: dir.path().to_owned(),
        source,
    })?;
    if path.starts_with(project) {
        return Err(Error::TempInProject(path));
    }
    Ok((dir, path))
}

/// Include the pid namespace in the directory name. A later maki process can then identify
/// dead owners with pids scoped to their own namespace.
fn dir_prefix() -> String {
    #[cfg(target_os = "linux")]
    if let Some(namespace) = pid_namespace() {
        return format!("{DIR_PREFIX}{namespace}{OWNER_SEPARATOR}{}.", process::id());
    }
    DIR_PREFIX.to_owned()
}

#[cfg(target_os = "linux")]
pub(super) fn pid_namespace() -> Option<u64> {
    fs::metadata(PID_NAMESPACE).ok().map(|meta| meta.ino())
}

/// Artifact cleanup can take a long time. Run it outside the async threads and scan each
/// base once.
#[cfg(target_os = "linux")]
fn sweep_once(base: &Path) {
    static SWEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut swept = SWEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if swept.iter().any(|dir| dir == base) {
        return;
    }
    swept.push(base.to_owned());
    let base = base.to_owned();
    smol::unblock(move || sweep_dead_owners(&base)).detach();
}

/// Removes the private directories in `base` whose maki is gone. A signal
/// can kill maki before the directory's destructor runs. Only the user's own
/// directories from this pid namespace go, and a link is never followed.
#[cfg(target_os = "linux")]
pub(super) fn sweep_dead_owners(base: &Path) {
    let (Ok(entries), Some(namespace)) = (fs::read_dir(base), pid_namespace()) else {
        return;
    };
    let uid = getuid().as_raw();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(owner) = name
            .to_str()
            .and_then(|name| name.strip_prefix(DIR_PREFIX))
            .and_then(|rest| rest.split_once('.'))
            .and_then(|(owner, _)| owner.split_once(OWNER_SEPARATOR))
            .filter(|(owner_namespace, _)| owner_namespace.parse() == Ok(namespace))
            .and_then(|(_, pid)| pid.parse().ok())
            .and_then(Pid::from_raw)
        else {
            continue;
        };
        let path = entry.path();
        let owned =
            fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && meta.uid() == uid);
        if !owned || test_kill_process(owner) != Err(Errno::SRCH) {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => debug!(
                path = %path.display(),
                owner = owner.as_raw_nonzero(),
                "claude-code: removed a dead maki's private directory"
            ),
            Err(error) => warn!(
                path = %path.display(),
                %error,
                "claude-code: could not remove a dead maki's private directory"
            ),
        }
    }
}
