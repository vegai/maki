local git_utils = require("claude_git")
local path_utils = require("claude_paths")

local M = {}

M.CHANGE_NOT_TABLE = "a change is not a table"
M.PROTECTED_PATH = "%s is in %s, which a snapshot does not copy"
M.UNKNOWN_STATUS = "%s has the unknown status %s"
M.NO_OBJECT_ID = "%s has no object id in %s"
M.UNKNOWN_MODE = "%s has an unknown %s"
M.STATUS_MODE_MISMATCH = "%s: its %s_mode does not agree with its status %s"
M.OBJECT_MODE_MISMATCH = "%s has a %s object id that does not agree with its %s mode"
M.KIND_MISMATCH = "%s is a %s change, and not %s"
M.REPOSITORY_KIND = "repository"
M.MANIFEST_VERSION = 1

local TYPE_KIND = "type"
M.TYPE_KIND = TYPE_KIND
-- Symlinks, submodules and type changes can affect paths outside an ordinary file import. The
-- user must apply these changes manually.
local IMPORTABLE = { text = true, binary = true, mode = true }
-- Manifest fields end up in the command the user approves, so any other
-- value is refused.
local KNOWN_STATUSES = { A = true, M = true, D = true, T = true }
local KNOWN_MODES = {
  [git_utils.ABSENT_MODE] = true,
  [git_utils.REGULAR_MODE] = true,
  [git_utils.EXECUTABLE_MODE] = true,
  [git_utils.SYMLINK_MODE] = true,
  [git_utils.GITLINK_MODE] = true,
}
-- The snapshot copies neither, and Claude Code config can add hooks.
local NEVER_COLLECTED = { [".git"] = true, [".claude"] = true, [".maki"] = true }
local OBJECT_ID_LENGTHS = { [40] = true, [64] = true }
function M.repository_path(path, repositories)
  for repository in pairs(repositories or {}) do
    if repository == "." or path == repository or path:sub(1, #repository + 1) == repository .. "/" then
      return true
    end
  end
  return false
end

function M.kind(change, binary)
  if change.old_mode == git_utils.SYMLINK_MODE or change.new_mode == git_utils.SYMLINK_MODE then
    return "symlink"
  end
  if change.old_mode == git_utils.GITLINK_MODE or change.new_mode == git_utils.GITLINK_MODE then
    return "submodule"
  end
  if change.status == git_utils.TYPE_CHANGED then
    return TYPE_KIND
  end
  if binary[change.path] then
    return "binary"
  end
  if change.old_sha == change.new_sha then
    return "mode"
  end
  return "text"
end

--- Returns why the collect step could not have written {change}, or nil.
function M.change_problem(change)
  if type(change) ~= "table" then
    return M.CHANGE_NOT_TABLE
  end
  local path_problem = path_utils.path_problem(change.path)
  if path_problem then
    return path_problem
  end
  for part in change.path:gmatch("[^/]+") do
    if NEVER_COLLECTED[part:lower()] then
      return M.PROTECTED_PATH:format(change.path, part)
    end
  end
  if not KNOWN_STATUSES[change.status] then
    return M.UNKNOWN_STATUS:format(change.path, tostring(change.status))
  end
  for _, field in ipairs({ "old_sha", "new_sha" }) do
    local sha = change[field]
    if type(sha) ~= "string" or not sha:match("^%x+$") or not OBJECT_ID_LENGTHS[#sha] then
      return M.NO_OBJECT_ID:format(change.path, field)
    end
  end
  for _, field in ipairs({ "old_mode", "new_mode" }) do
    if not KNOWN_MODES[change[field]] then
      return M.UNKNOWN_MODE:format(change.path, field)
    end
  end
  -- Only an added file has no `old_mode`, and only a deleted file has no
  -- `new_mode`.
  for _, side in ipairs({ "old", "new" }) do
    local absent = change[side .. "_mode"] == git_utils.ABSENT_MODE
    local lacks = side == "old" and git_utils.ADDED or git_utils.DELETED
    if absent ~= (change.status == lacks) then
      return M.STATUS_MODE_MISMATCH:format(change.path, side, change.status)
    end
    if absent == (change[side .. "_sha"]:match("[^0]") ~= nil) then
      return M.OBJECT_MODE_MISMATCH:format(change.path, side, side)
    end
  end
  -- `retyped_paths` can make any change a type change, which is never
  -- imported.
  local kind = M.kind(change, { [change.path] = change.kind == "binary" })
  if change.kind ~= kind and change.kind ~= TYPE_KIND and change.kind ~= M.REPOSITORY_KIND then
    return M.KIND_MISMATCH:format(change.path, kind, tostring(change.kind))
  end
  return nil
end

function M.importable(change)
  return IMPORTABLE[change.kind] == true
end

--- Includes the mode, because a chmod changes the mode and leaves the blob.
function M.file_state(sha, executable)
  return sha .. " " .. (executable and git_utils.EXECUTABLE_MODE or git_utils.REGULAR_MODE)
end

--- {current} maps each path to its `file_state`, or false when it is missing.
function M.conflicts(changes, current)
  local found = {}
  for _, change in ipairs(changes) do
    local expected = change.status ~= git_utils.ADDED
      and M.file_state(change.old_sha, change.old_mode == git_utils.EXECUTABLE_MODE)
    if current[change.path] ~= expected then
      found[#found + 1] = change.path
    end
  end
  return found
end

function M.landed(changes, current, receipts)
  local found, matched = {}, {}
  for _, change in ipairs(changes) do
    local wanted = change.status ~= git_utils.DELETED
      and M.file_state(change.new_sha, change.new_mode == git_utils.EXECUTABLE_MODE)
    if current[change.path] == wanted then
      local list = receipts[change.path] and found or matched
      list[#list + 1] = change
    end
  end
  return found, matched
end

function M.summary(changes)
  local lines = {}
  for _, change in ipairs(changes) do
    local line = change.status .. " " .. path_utils.printable(change.path)
    if not M.importable(change) then
      line = line .. " (" .. change.kind .. ", apply manually)"
    elseif change.kind ~= "text" then
      line = line .. " (" .. change.kind .. ")"
    end
    lines[#lines + 1] = line
  end
  return lines
end

--- The prefix of the temporary files of the import staging in {stage}: the
--- artifact id and the stage name.

return M
