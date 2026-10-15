use std::ffi::{CStr, OsStr};
use std::fs::{self, File, FileTimes};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use maki_storage::id::MakiId;
use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Stat, open, openat, renameat, statat, unlinkat,
};
use rustix::path::Arg;

use maki_providers::claude_code::validation::GIT_ENTRY;

const MAX_DEPTH: usize = 64;
const MAX_FAILURES: usize = 10;
const CONFIG: &str = "config";
const HEAD: &str = "HEAD";
const COMMONDIR: &str = "commondir";
const REPOSITORY_ENTRIES: &[&str] = &["objects", "refs"];
const QUARANTINE_PREFIX: &str = "repository.";
const QUARANTINE_ENTRY: &str = "metadata";
const PRIVATE_COPY_PREFIX: &str = ".maki-private-";
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Default)]
pub(super) struct SnapshotMetadata {
    pub metadata: Vec<String>,
    pub repositories: Vec<String>,
    pub private_files: usize,
}

fn private_copy(parent: &impl AsFd, name: impl Arg + Copy) -> io::Result<()> {
    let mut source = File::from(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = source.metadata()?;
    if !metadata.is_file() || metadata.nlink() <= 1 {
        return Ok(());
    }
    let temporary = format!("{PRIVATE_COPY_PREFIX}{}", MakiId::generate());
    let mut copy = File::from(openat(
        parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let result = (|| {
        io::copy(&mut source, &mut copy)?;
        copy.set_permissions(metadata.permissions())?;
        copy.set_times(
            FileTimes::new()
                .set_accessed(metadata.accessed()?)
                .set_modified(metadata.modified()?),
        )?;
        renameat(parent, temporary.as_str(), parent, name)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn directory(parent: &impl AsFd, name: impl Arg) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        DIRECTORY_FLAGS,
        Mode::empty(),
    )?))
}

fn repository(dir: &impl AsFd) -> bool {
    statat(dir, HEAD, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|entry| {
        matches!(
            FileType::from_raw_mode(entry.st_mode),
            FileType::RegularFile | FileType::Symlink
        )
    }) && (statat(dir, COMMONDIR, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|entry| {
        matches!(
            FileType::from_raw_mode(entry.st_mode),
            FileType::RegularFile | FileType::Symlink
        )
    }) || REPOSITORY_ENTRIES.iter().any(|name| {
        statat(dir, *name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|entry| {
            matches!(
                FileType::from_raw_mode(entry.st_mode),
                FileType::Directory | FileType::Symlink
            )
        })
    }))
}

fn quarantine_entry(
    parent: &impl AsFd,
    name: impl Arg + Copy,
    quarantine: &Path,
) -> io::Result<()> {
    let destination = tempfile::Builder::new()
        .prefix(QUARANTINE_PREFIX)
        .tempdir_in(quarantine)?;
    let dir = File::from(open(destination.path(), DIRECTORY_FLAGS, Mode::empty())?);
    renameat(parent, name, &dir, QUARANTINE_ENTRY)?;
    let _ = destination.keep();
    let metadata = statat(&dir, QUARANTINE_ENTRY, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
        let root = directory(&dir, QUARANTINE_ENTRY)?;
        walk(root, |parent, name, _path, _metadata| {
            if name.to_bytes() == CONFIG.as_bytes() && repository(parent) {
                quarantine_entry(parent, name, quarantine)?;
                return Ok(Visit::Skip);
            }
            Ok(Visit::Scan)
        })?;
    }
    Ok(())
}

fn restore_pointer(root: &File, quarantine: &Path, git: &Path) -> io::Result<()> {
    let expected = format!("gitdir: {}\n", git.display());
    let pointer = openat(
        root,
        GIT_ENTRY,
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
                if metadata.nlink() > 1 {
                    private_copy(root, GIT_ENTRY)?;
                }
                return Ok(());
            }
        }
    }
    if statat(root, GIT_ENTRY, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        quarantine_entry(root, GIT_ENTRY, quarantine)?;
    }
    let mut pointer = File::from(openat(
        root,
        GIT_ENTRY,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    pointer.write_all(expected.as_bytes())
}

enum Visit {
    Scan,
    Skip,
    Open(File),
}

fn walk(
    root: File,
    mut visit: impl FnMut(&BorrowedFd<'_>, &CStr, &Path, &Stat) -> io::Result<Visit>,
) -> io::Result<()> {
    let mut pending = vec![(Dir::new(root)?, PathBuf::new())];
    let mut failures = Vec::new();
    let mut failure_count = 0;
    let mut record = |path: &Path, error: io::Error| {
        failure_count += 1;
        if failures.len() < MAX_FAILURES {
            failures.push(format!(
                "{}: {error}",
                path.as_os_str().as_bytes().escape_ascii()
            ));
        }
    };
    while let Some((entries, relative)) = pending.last_mut() {
        let Some(entry) = entries.next() else {
            pending.pop();
            continue;
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record(relative, error.into());
                continue;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let path = relative.join(OsStr::from_bytes(name.to_bytes()));
        let result = (|| {
            let parent = entries.fd()?;
            let metadata = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let child = match visit(&parent, name, &path, &metadata)? {
                Visit::Skip => return Ok(None),
                Visit::Open(child) => child,
                Visit::Scan if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory => {
                    directory(&parent, name)?
                }
                Visit::Scan => return Ok(None),
            };
            if path.components().count() > MAX_DEPTH {
                return Err(io::Error::other(format!(
                    "snapshot exceeds {MAX_DEPTH} directory levels"
                )));
            }
            Ok(Some(Dir::new(child)?))
        })();
        match result {
            Ok(Some(child)) => pending.push((child, path)),
            Ok(None) => {}
            Err(error) => record(&path, error),
        }
    }
    if failure_count == 0 {
        return Ok(());
    }
    if failure_count > failures.len() {
        failures.push(format!(
            "{} more snapshot errors",
            failure_count - failures.len()
        ));
    }
    Err(io::Error::other(failures.join("\n")))
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
    let scanned = walk(root, |parent, name, path, metadata| {
        if path == Path::new(GIT_ENTRY) {
            return Ok(Visit::Skip);
        }
        if root_repository
            && path.parent() == Some(Path::new(""))
            && (name.to_bytes() == HEAD.as_bytes() || name.to_bytes() == CONFIG.as_bytes())
        {
            quarantine_entry(parent, name, quarantine)?;
            return Ok(Visit::Skip);
        }
        if name.to_bytes().eq_ignore_ascii_case(GIT_ENTRY.as_bytes()) {
            quarantine_entry(parent, name, quarantine)?;
            relocated.metadata.push(path.to_string_lossy().into_owned());
            return Ok(Visit::Skip);
        }
        let kind = FileType::from_raw_mode(metadata.st_mode);
        if kind == FileType::RegularFile && metadata.st_nlink > 1 {
            private_copy(parent, name)?;
            relocated.private_files += 1;
        }
        if kind == FileType::Directory {
            let child = directory(parent, name)?;
            if repository(&child) {
                quarantine_entry(parent, name, quarantine)?;
                relocated
                    .repositories
                    .push(path.to_string_lossy().into_owned());
                return Ok(Visit::Skip);
            }
            return Ok(Visit::Open(child));
        }
        Ok(Visit::Scan)
    });
    pointer.and(scanned)?;
    Ok(relocated)
}

#[cfg(test)]
mod tests {
    use super::{
        COMMONDIR, CONFIG, GIT_ENTRY, HEAD, MAX_DEPTH, MAX_FAILURES, QUARANTINE_ENTRY, Visit,
        quarantine_entry, sanitize_snapshot, walk,
    };
    use std::fs::{self, File, Permissions};
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::Path;
    use tempfile::tempdir;
    use test_case::test_case;

    const REPOSITORY: &str = "fixtures/x.git";
    const CONTENT: &str = "preserved bytes";
    const UNREADABLE: &str = "unreadable";
    const WALK_FAILURE: &str = "cannot inspect entry";
    const LARGE_POINTER_BYTES: u64 = 1024 * 1024;

    #[test]
    fn the_trusted_git_pointer_cannot_share_an_external_inode() {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let git = dir.path().join("repository");
        let outside = dir.path().join(GIT_ENTRY);
        fs::create_dir(&snapshot).unwrap();
        let expected = format!("gitdir: {}\n", git.display());
        fs::write(&outside, &expected).unwrap();
        fs::hard_link(&outside, snapshot.join(GIT_ENTRY)).unwrap();
        sanitize_snapshot(&snapshot, &dir.path().join("quarantine"), &git).unwrap();
        assert_eq!(fs::metadata(snapshot.join(GIT_ENTRY)).unwrap().nlink(), 1);
        fs::write(snapshot.join(GIT_ENTRY), CONTENT).unwrap();
        assert_eq!(fs::read_to_string(outside).unwrap(), expected);
    }

    #[test_case(false ; "external_cache")]
    #[test_case(true ; "tracked_alias")]
    fn sanitizing_hard_links_preserves_contents_modes_and_timestamps(internal: bool) {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        fs::create_dir(&snapshot).unwrap();
        let source = if internal {
            snapshot.join(HEAD)
        } else {
            dir.path().join(HEAD)
        };
        fs::write(&source, CONTENT).unwrap();
        fs::set_permissions(&source, Permissions::from_mode(0o751)).unwrap();
        let before = fs::metadata(&source).unwrap();
        let copied = snapshot.join(CONFIG);
        fs::hard_link(&source, &copied).unwrap();
        assert!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .private_files
                > 0
        );
        let after = fs::metadata(&copied).unwrap();
        assert_eq!(after.nlink(), 1);
        assert_eq!(after.permissions().mode(), before.permissions().mode());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert_eq!(fs::read_to_string(&copied).unwrap(), CONTENT);
        assert_eq!(fs::read_to_string(&source).unwrap(), CONTENT);
        fs::write(copied, UNREADABLE).unwrap();
        assert_eq!(fs::read_to_string(source).unwrap(), CONTENT);
    }

    #[test_case(".git")]
    #[test_case(".GiT")]
    fn sanitize_keeps_the_trusted_pointer_and_moves_nested_metadata(name: &str) {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        fs::create_dir_all(snapshot.join("nested")).unwrap();
        let pointer = format!("gitdir: {}\n", git.display());
        fs::write(snapshot.join(GIT_ENTRY), &pointer).unwrap();
        fs::write(snapshot.join("nested").join(name), "untrusted metadata").unwrap();
        assert_eq!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .metadata,
            [format!("nested/{name}")]
        );
        assert_eq!(
            fs::read_to_string(snapshot.join(GIT_ENTRY)).unwrap(),
            pointer
        );
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
        let target = outside.join(GIT_ENTRY);
        let pointer = format!("gitdir: {}\n", git.display());
        fs::write(&target, &pointer).unwrap();
        symlink(&target, snapshot.join(GIT_ENTRY)).unwrap();
        symlink(&outside, snapshot.join("linked")).unwrap();
        assert!(
            sanitize_snapshot(&snapshot, &quarantine, &git)
                .unwrap()
                .metadata
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), pointer);
        assert!(
            fs::symlink_metadata(snapshot.join(GIT_ENTRY))
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 1);
        assert!(snapshot.join("linked/.git").exists());
    }

    #[test_case("objects")]
    #[test_case("refs")]
    #[test_case(COMMONDIR)]
    fn embedded_repositories_and_their_configs_are_quarantined(layout: &str) {
        let dir = tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        let quarantine = dir.path().join("quarantine");
        let git = dir.path().join("repository");
        let repository = snapshot.join(REPOSITORY);
        if layout == COMMONDIR {
            fs::create_dir_all(&repository).unwrap();
            fs::create_dir_all(snapshot.join("store/objects")).unwrap();
            fs::create_dir_all(snapshot.join("store/refs")).unwrap();
            fs::write(repository.join(COMMONDIR), "../../store\n").unwrap();
            fs::write(snapshot.join("store/config"), CONTENT).unwrap();
        } else {
            fs::create_dir_all(repository.join(layout)).unwrap();
        }
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
        if layout == COMMONDIR {
            assert!(metadata.join(COMMONDIR).exists());
            assert_eq!(
                fs::read_to_string(snapshot.join("store/config")).unwrap(),
                CONTENT
            );
        }
        assert_eq!(fs::read_to_string(config).unwrap(), CONTENT);
    }

    #[test]
    fn walk_caps_and_escapes_failures() {
        let root = tempdir().unwrap();
        let count = MAX_FAILURES * 2;
        for index in 0..count {
            fs::write(root.path().join(format!("entry\n{index}")), CONTENT).unwrap();
        }
        let error = walk(File::open(root.path()).unwrap(), |_, _, _, _| {
            Err(io::Error::other(WALK_FAILURE))
        })
        .unwrap_err()
        .to_string();
        assert_eq!(error.lines().count(), MAX_FAILURES + 1);
        assert!(error.contains("entry\\n"));
        assert!(error.ends_with(&format!("{} more snapshot errors", count - MAX_FAILURES)));
    }

    #[test]
    fn walk_refuses_excessive_depth() {
        let root = tempdir().unwrap();
        let deep = (0..=MAX_DEPTH).fold(root.path().to_path_buf(), |path, _| path.join("nested"));
        fs::create_dir_all(deep).unwrap();
        let error = walk(File::open(root.path()).unwrap(), |_, _, _, _| {
            Ok(Visit::Scan)
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("snapshot exceeds {MAX_DEPTH} directory levels"))
        );
    }

    #[test]
    fn walk_finishes_other_entries_after_an_error() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(UNREADABLE), CONTENT).unwrap();
        fs::write(dir.path().join(HEAD), CONTENT).unwrap();
        let mut visited = false;
        let error = walk(File::open(dir.path()).unwrap(), |_, name, _, _| {
            if name.to_bytes() == UNREADABLE.as_bytes() {
                return Err(io::Error::other(WALK_FAILURE));
            }
            visited = true;
            Ok(Visit::Scan)
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
        File::create(snapshot.join(GIT_ENTRY))
            .unwrap()
            .set_len(LARGE_POINTER_BYTES)
            .unwrap();
        sanitize_snapshot(&snapshot, &quarantine, &git).unwrap();
        assert_eq!(
            fs::read_to_string(snapshot.join(GIT_ENTRY)).unwrap(),
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
        fs::write(outside.join(GIT_ENTRY), CONTENT).unwrap();
        walk(
            File::open(&snapshot).unwrap(),
            |parent, name, path, _metadata| {
                if path == Path::new("nested/.git") {
                    fs::rename(snapshot.join("nested"), &moved)?;
                    symlink(&outside, snapshot.join("nested"))?;
                    quarantine_entry(parent, name, &quarantine)?;
                    return Ok(Visit::Skip);
                }
                Ok(Visit::Scan)
            },
        )
        .unwrap();
        assert!(!moved.join(GIT_ENTRY).exists());
        assert_eq!(
            fs::read_to_string(outside.join(GIT_ENTRY)).unwrap(),
            CONTENT
        );
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
