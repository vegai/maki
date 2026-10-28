use maki_providers::claude_code::Error;
use mlua::{Error as LuaError, Lua, Result as LuaResult, Table};

use crate::plugin_permissions::PluginPermissions;

pub(crate) fn create_private_module(
    _lua: &Lua,
    _perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    Err(LuaError::external(Error::UnsupportedSystem))
}
