use crate::plugin_permissions::PluginPermissions;
#[cfg(any(test, not(target_os = "linux")))]
use maki_providers::claude_code::UNSUPPORTED_SYSTEM;
use maki_providers::claude_code::{
    CONFIG_DIR_OPTION, DEFAULT_EXECUTABLE, DEFAULT_MAX_CONCURRENT, EXECUTABLE_OPTION,
    MAX_CONCURRENT_OPTION, MAX_EVENT_BYTES, MIN_MAX_CONCURRENT, STARTUP_TIMEOUT_SECS,
};
use mlua::{Lua, Result as LuaResult, Table};

#[cfg(target_os = "linux")]
use rustix::fd::AsFd;
#[cfg(target_os = "linux")]
use rustix::fs::{Mode, OFlags, openat};
#[cfg(target_os = "linux")]
use rustix::path::Arg;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;

pub(crate) const MODULE: &str = "maki.claude_code.internal";

#[cfg(target_os = "linux")]
pub(super) const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
#[cfg(target_os = "linux")]
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
#[cfg(target_os = "linux")]
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
#[cfg(target_os = "linux")]
const EXPECTED_REGULAR_FILE: &str = "expected a regular file";

#[cfg(target_os = "linux")]
pub(super) fn open_regular(dir: &impl AsFd, name: impl Arg) -> io::Result<File> {
    let file = File::from(openat(dir, name, READ_FLAGS, Mode::empty())?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(EXPECTED_REGULAR_FILE));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
pub(super) fn create_private(dir: &impl AsFd, name: impl Arg) -> io::Result<File> {
    Ok(File::from(openat(
        dir,
        name,
        CREATE_FLAGS,
        Mode::RUSR | Mode::WUSR,
    )?))
}

pub(super) fn add_defaults(table: &Table) -> LuaResult<()> {
    table.set("startup_timeout_secs", STARTUP_TIMEOUT_SECS)?;
    table.set(
        "error_bytes",
        super::r#fn::EXIT_STDERR_BYTES * super::r#fn::EXIT_STDERR_LINES,
    )?;
    table.set("max_event_bytes", MAX_EVENT_BYTES)?;
    table.set("default_executable", DEFAULT_EXECUTABLE)?;
    table.set("default_provider_max_concurrent", DEFAULT_MAX_CONCURRENT)?;
    table.set("min_provider_max_concurrent", MIN_MAX_CONCURRENT)?;
    table.set("executable_option", EXECUTABLE_OPTION)?;
    table.set("config_dir_option", CONFIG_DIR_OPTION)?;
    table.set("max_concurrent_option", MAX_CONCURRENT_OPTION)
}

#[cfg(target_os = "linux")]
mod blobs;
#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
mod snapshot;

pub(crate) fn create_private_module(
    lua: &Lua,
    perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    #[cfg(target_os = "linux")]
    {
        let table = native::create_table(lua, perms)?;
        add_defaults(&table)?;
        Ok((MODULE, table))
    }
    #[cfg(not(target_os = "linux"))]
    create_unsupported_module(lua, perms)
}

#[cfg(any(test, not(target_os = "linux")))]
pub(crate) fn create_unsupported_module(
    lua: &Lua,
    _perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    let table = lua.create_table()?;
    table.set("unsupported", UNSUPPORTED_SYSTEM)?;
    add_defaults(&table)?;
    Ok((MODULE, table))
}
