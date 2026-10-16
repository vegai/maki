use std::env;
#[cfg(windows)]
use std::env::consts::ARCH;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table};

#[cfg(unix)]
use rustix::system::uname;

use crate::plugin_permissions::PluginPermissions;

#[cfg(windows)]
const WINDOWS_SYSNAME: &str = "Windows_NT";

/// Return the current working directory as an absolute path. Like `vim.uv.cwd`.
///
/// @return (string?) Current working directory, or nil if it cannot be determined.
/// @example
/// local cwd = maki.uv.cwd()
/// if cwd then print("working in: " .. cwd) end
#[lua_fn(guard = FsRead)]
fn cwd(_lua: &Lua) -> LuaResult<Option<String>> {
    Ok(env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from)))
}

/// Return the current user's home directory. Like `vim.uv.os_homedir`.
///
/// @return (string?) Home directory path, or nil if it cannot be determined.
/// @example
/// local home = maki.uv.os_homedir() -- e.g. "/home/user"
#[lua_fn(guard = FsRead)]
fn os_homedir(_lua: &Lua) -> LuaResult<Option<String>> {
    Ok(maki_storage::paths::home().and_then(|p| p.to_str().map(String::from)))
}

/// Look up the environment variable {name}. Like `vim.uv.os_getenv`.
/// Returns nil when the variable is not set.
///
/// @param name string Name of the environment variable.
/// @return (string?) Variable value, or nil if not set.
/// @example
/// local editor = maki.uv.os_getenv("EDITOR") or "vi"
#[lua_fn(guard = Env)]
fn os_getenv(_lua: &Lua, name: String) -> LuaResult<Option<String>> {
    Ok(env::var(&name).ok())
}

/// Return every environment variable as a `{ NAME = value }` table, like
/// `vim.uv.os_environ`. Variables whose name or value is not UTF-8 are left
/// out.
///
/// @return (table) Environment variables, with the name as the key.
/// @example
/// for name in pairs(maki.uv.os_environ()) do print(name) end
#[lua_fn(guard = Env)]
fn os_environ(lua: &Lua) -> LuaResult<Table> {
    lua.create_table_from(
        env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }),
    )
}

/// Return the operating system's name and version, like `vim.uv.os_uname`.
///
/// @return (table) `sysname` (for example "Linux", "Darwin" or "Windows_NT"),
///   `release`, `version` and `machine`. On Windows, `release` and `version`
///   are empty.
/// @example
/// if maki.uv.os_uname().sysname == "Linux" then print("on Linux") end
#[lua_fn]
fn os_uname(lua: &Lua) -> LuaResult<Table> {
    let table = lua.create_table()?;
    #[cfg(unix)]
    {
        let system = uname();
        table.set("sysname", system.sysname().to_string_lossy())?;
        table.set("release", system.release().to_string_lossy())?;
        table.set("version", system.version().to_string_lossy())?;
        table.set("machine", system.machine().to_string_lossy())?;
    }
    #[cfg(windows)]
    {
        table.set("sysname", WINDOWS_SYSNAME)?;
        table.set("release", "")?;
        table.set("version", "")?;
        table.set("machine", ARCH)?;
    }
    Ok(table)
}

lua_table! {
    /// System and environment utilities, modelled after `vim.uv`.
    ///
    /// Provides access to the working directory, home directory, and environment
    /// variables. None of these functions throw.
    ///
    /// Filesystem location queries (`cwd`, `os_homedir`) need `fs_read`, while
    /// `os_getenv` and `os_environ` read the process environment, which can
    /// hold secrets, so they need `env`.
    ///
    /// ```lua
    /// local home = maki.uv.os_homedir()
    /// ```
    "maki.uv" => pub(crate) fn create_uv_table(perms: &PluginPermissions), DOCS [
        cwd(perms), os_homedir(perms), os_getenv(perms), os_environ(perms), os_uname,
    ]
}
