use std::env;
#[cfg(windows)]
use std::env::consts::ARCH;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table};

#[cfg(unix)]
use rustix::system::uname;

use crate::api::util::pair::{Pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const INVALID_TEMPLATE: &str = "temporary directory template must end in XXXXXX";
const TEMP_SUFFIX: &str = "XXXXXX";
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(windows)]
const WINDOWS_SYSNAME: &str = "Windows_NT";

/// Resolve symlinks before comparing sandbox paths.
/// @param path string Existing path.
/// @return (string?, string?) Physical path, or nil and the error.
#[lua_fn(guard = FsRead)]
async fn fs_realpath(_lua: Lua, path: String) -> LuaResult<Pair<String>> {
    Ok(pair(
        smol::fs::canonicalize(path)
            .await
            .map(|path| path.to_string_lossy().into_owned()),
    ))
}

/// Create a private temporary directory. The caller owns its cleanup.
/// @param template string Path ending in XXXXXX.
/// @return (string?, string?) Created directory, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn fs_mkdtemp(_lua: Lua, template: String) -> LuaResult<Pair<String>> {
    if !template.ends_with(TEMP_SUFFIX) {
        return Ok((None, Some(INVALID_TEMPLATE.into())));
    }
    let path = PathBuf::from(template);
    let dir = try_pair!(
        smol::unblock(move || {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let prefix = name.strip_suffix(TEMP_SUFFIX).unwrap_or_default();
            let mut builder = tempfile::Builder::new();
            builder.prefix(prefix);
            #[cfg(unix)]
            builder.permissions(Permissions::from_mode(PRIVATE_DIR_MODE));
            builder.tempdir_in(path.parent().unwrap_or_else(|| Path::new(".")))
        })
        .await
    );
    Ok((Some(dir.keep().to_string_lossy().into_owned()), None))
}

/// Remove an empty directory without following a symlink or deleting hook output.
/// @param path string Directory to remove.
/// @return (boolean?, string?) True on success, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn fs_rmdir(_lua: Lua, path: String) -> LuaResult<Pair<bool>> {
    Ok(pair(smol::fs::remove_dir(path).await.map(|_| true)))
}

/// Rename a file or directory. Like `vim.uv.fs_rename`.
/// @param path string Existing path.
/// @param new_path string Destination path.
/// @return (boolean?, string?) True on success, or nil and the error.
#[lua_fn(guard = FsWrite)]
async fn fs_rename(_lua: Lua, path: String, new_path: String) -> LuaResult<Pair<bool>> {
    Ok(pair(
        smol::unblock(move || std::fs::rename(path, new_path).map(|()| true)).await,
    ))
}

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
        fs_realpath(perms), fs_mkdtemp(perms), fs_rename(perms), fs_rmdir(perms),
    ]
}

#[cfg(test)]
mod tests {
    use super::{TEMP_SUFFIX, create_uv_table};
    use crate::plugin_permissions::PluginPermissions;
    use mlua::{Function, Lua};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;
    use test_case::test_case;

    #[test_case("" ; "unprefixed_template")]
    #[test_case("artifact." ; "prefixed_template")]
    fn mkdtemp_creates_inside_the_template_directory(prefix: &str) {
        let root = tempdir().unwrap();
        let lua = Lua::new();
        let uv = create_uv_table(&lua, &PluginPermissions::trusted()).unwrap();
        let create: Function = uv.get("fs_mkdtemp").unwrap();
        let template = root.path().join(format!("{prefix}{TEMP_SUFFIX}"));
        let (path, error): (Option<String>, Option<String>) =
            smol::block_on(create.call_async(template.to_str().unwrap())).unwrap();
        assert_eq!(error, None);
        let path = Path::new(path.as_ref().unwrap());
        assert_eq!(path.parent(), Some(root.path()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(prefix));
        assert_eq!(name.len(), prefix.len() + TEMP_SUFFIX.len());
        fs::remove_dir(path).unwrap();
    }
}
