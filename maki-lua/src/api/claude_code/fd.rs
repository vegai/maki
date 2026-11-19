use std::fs::File;
use std::io;

use maki_storage::permissions::PRIVATE_FILE_MODE;
use rustix::fd::AsFd;
use rustix::fs::{Mode, OFlags, openat};
use rustix::path::Arg;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const EXPECTED_REGULAR_FILE: &str = "expected a regular file";

pub(super) fn open_regular(dir: &impl AsFd, name: impl Arg) -> io::Result<File> {
    let file = File::from(openat(dir, name, READ_FLAGS, Mode::empty())?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(EXPECTED_REGULAR_FILE));
    }
    Ok(file)
}

pub(super) fn create_private(dir: &impl AsFd, name: impl Arg) -> io::Result<File> {
    Ok(File::from(openat(
        dir,
        name,
        CREATE_FLAGS,
        Mode::from_raw_mode(PRIVATE_FILE_MODE),
    )?))
}

pub(super) fn directory(parent: &impl AsFd, name: impl Arg) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        DIRECTORY_FLAGS,
        Mode::empty(),
    )?))
}
