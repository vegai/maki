use std::ffi::{CStr, OsStr};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, open, openat, renameat, statat};

const DOT_GIT: &str = ".git";
const CONFIG: &str = "config";
const HEAD: &str = "HEAD";
const REPOSITORY_ENTRIES: &[&str] = &["objects", "refs"];
const QUARANTINE_PREFIX: &str = "repository.";
const QUARANTINE_ENTRY: &str = "metadata";
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Default)]
pub(super) struct SnapshotMetadata {
    pub metadata: Vec<String>,
    pub repositories: Vec<String>,
}

fn directory(parent: &File, name: &CStr) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        DIRECTORY_FLAGS,
        Mode::empty(),
    )?))
}

fn repository(dir: &File) -> bool {
    statat(dir, HEAD, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|entry| {
        matches!(
            FileType::from_raw_mode(entry.st_mode),
            FileType::RegularFile | FileType::Symlink
        )
    }) && REPOSITORY_ENTRIES.iter().any(|name| {
        statat(dir, *name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|entry| {
            matches!(
                FileType::from_raw_mode(entry.st_mode),
                FileType::Directory | FileType::Symlink
            )
        })
    })
}

fn quarantine_entry(parent: &File, name: &CStr, quarantine: &Path) -> io::Result<()> {
    let destination = tempfile::Builder::new()
        .prefix(QUARANTINE_PREFIX)
        .tempdir_in(quarantine)?;
    let dir = File::from(open(destination.path(), DIRECTORY_FLAGS, Mode::empty())?);
    renameat(parent, name, &dir, QUARANTINE_ENTRY)?;
    let _ = destination.keep();
    let metadata = statat(&dir, QUARANTINE_ENTRY, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
        let root = directory(&dir, c"metadata")?;
        walk(root, |parent, name, _path| {
            if name.to_bytes() == CONFIG.as_bytes() && repository(parent) {
                quarantine_entry(parent, name, quarantine)?;
                return Ok(false);
            }
            Ok(true)
        })?;
    }
    Ok(())
}

fn restore_pointer(root: &File, quarantine: &Path, git: &Path) -> io::Result<()> {
    let expected = format!("gitdir: {}\n", git.display());
    let pointer = openat(
        root,
        DOT_GIT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    );
    if let Ok(pointer) = pointer {
        let mut pointer = File::from(pointer);
        let metadata = pointer.metadata()?;
        if metadata.is_file() && metadata.len() == expected.len() as u64 {
            let mut content = Vec::new();
            (&mut pointer)
                .take(expected.len() as u64 + 1)
                .read_to_end(&mut content)?;
            if content == expected.as_bytes() {
                return Ok(());
            }
        }
    }
    if statat(root, DOT_GIT, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        quarantine_entry(root, c".git", quarantine)?;
    }
    let mut pointer = File::from(openat(
        root,
        DOT_GIT,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    pointer.write_all(expected.as_bytes())
}

fn walk(
    root: File,
    mut visit: impl FnMut(&File, &CStr, &Path) -> io::Result<bool>,
) -> io::Result<()> {
    let entries = Dir::read_from(&root)?;
    let mut pending = vec![(root, entries, PathBuf::new())];
    let mut failures = Vec::new();
    while let Some((parent, entries, relative)) = pending.last_mut() {
        let Some(entry) = entries.next() else {
            pending.pop();
            continue;
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.push(format!("{}: {error}", relative.display()));
                continue;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let path = relative.join(OsStr::from_bytes(name.to_bytes()));
        let result = (|| {
            if !visit(parent, name, &path)? {
                return Ok(None);
            }
            let metadata = statat(&*parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
                return Ok(None);
            }
            let child = directory(parent, name)?;
            let entries = Dir::read_from(&child)?;
            Ok::<_, io::Error>(Some((child, entries)))
        })();
        match result {
            Ok(Some((child, entries))) => pending.push((child, entries, path)),
            Ok(None) => {}
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(failures.join("\n")))
    }
}

pub(super) fn sanitize_snapshot(
    snapshot: &Path,
    quarantine: &Path,
    git: &Path,
) -> io::Result<SnapshotMetadata> {
    fs::create_dir_all(quarantine)?;
    let root = File::from(open(snapshot, DIRECTORY_FLAGS, Mode::empty())?);
    let pointer = restore_pointer(&root, quarantine, git);
    let mut relocated = SnapshotMetadata::default();
    let root_repository = repository(&root);
    if root_repository {
        relocated.repositories.push(".".into());
    }
    let scanned = walk(root, |parent, name, path| {
        if path == Path::new(DOT_GIT) {
            return Ok(false);
        }
        if root_repository
            && path.parent() == Some(Path::new(""))
            && (name.to_bytes() == HEAD.as_bytes() || name.to_bytes() == CONFIG.as_bytes())
        {
            quarantine_entry(parent, name, quarantine)?;
            return Ok(false);
        }
        if name.to_bytes().eq_ignore_ascii_case(DOT_GIT.as_bytes()) {
            quarantine_entry(parent, name, quarantine)?;
            relocated.metadata.push(path.to_string_lossy().into_owned());
            return Ok(false);
        }
        if FileType::from_raw_mode(statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode)
            == FileType::Directory
            && repository(&directory(parent, name)?)
        {
            quarantine_entry(parent, name, quarantine)?;
            relocated
                .repositories
                .push(path.to_string_lossy().into_owned());
            return Ok(false);
        }
        Ok(true)
    });
    pointer.and(scanned)?;
    Ok(relocated)
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIG, DOT_GIT, HEAD, QUARANTINE_ENTRY, quarantine_entry, sanitize_snapshot, walk,
    };
    use std::fs::{self, File, Permissions};
    use std::io;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use tempfile::tempdir;
    use test_case::test_case;

    const REPOSITORY: &str = "fixtures/x.git";
    const CONTENT: &str = "preserved bytes";
    const UNREADABLE: &str = "unreadable";
    const WALK_FAILURE: &str = "cannot inspect entry";
    const LARGE_POINTER_BYTES: u64 = 1024 * 1024;

    #[test_case(".git")]
    #[test_case(".GiT")]
    fn sanitize_keeps_the_trusted_pointer_and_moves_nested_metadata(name: &str) {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        fs::create_dir_all(snapshot.join("nested")).unwrap();
        let pointer = format!("gitdir: {}\n", git.display());
        fs::write(snapshot.join(DOT_GIT), &pointer).unwrap();
        fs::write(snapshot.join("nested").join(name), "untrusted metadata").unwrap();
        assert_eq!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .metadata,
            [format!("nested/{name}")]
        );
        assert_eq!(fs::read_to_string(snapshot.join(DOT_GIT)).unwrap(), pointer);
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
        assert!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .metadata
                .is_empty()
        );
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
    }

    #[test]
    fn sanitize_moves_metadata_links_without_following_directory_links() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join(DOT_GIT);
        let pointer = format!("gitdir: {}\n", git.display());
        fs::write(&target, &pointer).unwrap();
        symlink(&target, snapshot.join(DOT_GIT)).unwrap();
        symlink(&outside, snapshot.join("linked")).unwrap();
        assert!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .metadata
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), pointer);
        assert!(
            fs::symlink_metadata(snapshot.join(DOT_GIT))
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
        assert!(snapshot.join("linked/.git").exists());
    }

    #[test_case("objects")]
    #[test_case("refs")]
    fn embedded_repositories_and_their_configs_are_quarantined(layout: &str) {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        let repository = snapshot.join(REPOSITORY);
        fs::create_dir_all(repository.join(layout)).unwrap();
        fs::write(repository.join(HEAD), CONTENT).unwrap();
        fs::write(repository.join(CONFIG), CONTENT).unwrap();
        let paths = sanitize_snapshot(&snapshot, &quarantine, &git).unwrap();
        assert_eq!(paths.repositories, [REPOSITORY]);
        assert!(!repository.exists());
        let entries: Vec<_> = fs::read_dir(&quarantine)
            .unwrap()
            .map(|entry| entry.unwrap().path().join(QUARANTINE_ENTRY))
            .collect();
        let metadata = entries.iter().find(|entry| entry.is_dir()).unwrap();
        assert!(!metadata.join(CONFIG).exists());
        assert_eq!(fs::read_to_string(metadata.join(HEAD)).unwrap(), CONTENT);
        let config = entries.iter().find(|entry| entry.is_file()).unwrap();
        assert_eq!(fs::read_to_string(config).unwrap(), CONTENT);
    }

    #[test]
    fn walk_finishes_other_entries_after_an_error() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(UNREADABLE), CONTENT).unwrap();
        fs::write(dir.path().join(HEAD), CONTENT).unwrap();
        let mut visited = false;
        let error = walk(File::open(dir.path()).unwrap(), |_, name, _| {
            if name.to_bytes() == UNREADABLE.as_bytes() {
                return Err(io::Error::other(WALK_FAILURE));
            }
            visited = true;
            Ok(true)
        })
        .unwrap_err();
        assert!(visited);
        assert!(error.to_string().contains(WALK_FAILURE));
    }

    #[test]
    fn unreadable_directories_do_not_leave_other_git_metadata_in_the_snapshot() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        let unreadable = snapshot.join(UNREADABLE);
        fs::create_dir_all(&unreadable).unwrap();
        fs::create_dir(snapshot.join("nested")).unwrap();
        fs::write(snapshot.join("nested/.git"), CONTENT).unwrap();
        fs::set_permissions(&unreadable, Permissions::from_mode(0o0)).unwrap();
        let result = sanitize_snapshot(&snapshot, &quarantine, &git);
        fs::set_permissions(&unreadable, Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(!snapshot.join("nested/.git").exists());
    }

    #[test]
    fn a_large_root_pointer_is_preserved_without_reading_it() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        fs::create_dir(&snapshot).unwrap();
        File::create(snapshot.join(DOT_GIT))
            .unwrap()
            .set_len(LARGE_POINTER_BYTES)
            .unwrap();
        sanitize_snapshot(&snapshot, &quarantine, &git).unwrap();
        assert_eq!(
            fs::read_to_string(snapshot.join(DOT_GIT)).unwrap(),
            format!("gitdir: {}\n", git.display())
        );
        let preserved = fs::read_dir(&quarantine)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(QUARANTINE_ENTRY);
        assert_eq!(fs::metadata(preserved).unwrap().len(), LARGE_POINTER_BYTES);
    }
    #[test]
    fn renaming_a_parent_cannot_redirect_quarantine() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let outside = dir.path().join("outside");
        let moved = dir.path().join("moved");
        fs::create_dir_all(snapshot.join("nested")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&quarantine).unwrap();
        fs::write(snapshot.join("nested/.git"), CONTENT).unwrap();
        fs::write(outside.join(DOT_GIT), CONTENT).unwrap();
        walk(File::open(&snapshot).unwrap(), |parent, name, path| {
            if path == Path::new("nested/.git") {
                fs::rename(snapshot.join("nested"), &moved)?;
                symlink(&outside, snapshot.join("nested"))?;
                quarantine_entry(parent, name, &quarantine)?;
                return Ok(false);
            }
            Ok(true)
        })
        .unwrap();
        assert!(!moved.join(DOT_GIT).exists());
        assert_eq!(fs::read_to_string(outside.join(DOT_GIT)).unwrap(), CONTENT);
    }
    #[test]
    fn the_snapshot_root_cannot_import_bare_metadata() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        fs::create_dir_all(snapshot.join("objects")).unwrap();
        fs::write(snapshot.join(HEAD), CONTENT).unwrap();
        fs::write(snapshot.join(CONFIG), CONTENT).unwrap();
        fs::write(snapshot.join("work.txt"), CONTENT).unwrap();
        let result = sanitize_snapshot(&snapshot, &quarantine, &git).unwrap();
        assert_eq!(result.repositories, ["."]);
        assert!(!snapshot.join(HEAD).exists());
        assert!(!snapshot.join(CONFIG).exists());
        assert_eq!(
            fs::read_to_string(snapshot.join("work.txt")).unwrap(),
            CONTENT
        );
    }
}
