local M = {}

M.ABSOLUTE_PATH = " must be relative to the project"
M.OUTSIDE_PROJECT = " goes out of the project"
M.IMPORT_LOCK = "import.lock"
M.PROJECT_LOCK = "maki-import.lock"
M.SNAPSHOT_DIR = "snapshot"
M.GIT_DIR = "git"
M.MANIFEST = "manifest.json"
local DEFAULT_DENY = { ".env*", "**/.env*", "secrets", "**/secrets", "secrets/**", "**/secrets/**" }

M.ARTIFACT_ID = "^[A-Za-z0-9]+$"

M.IMPORT_ORIGINALS = "originals"
M.IMPORT_DISPLACED = "displaced"
-- A unique stage keeps retries from removing another attempt's files.
M.STAGE_TEMPLATE = "stage.XXXXXX"
local OWNER_EXECUTE_BITS = "[xs]"

--- `ipairs` skips object entries. Require a JSON array so an object cannot bypass list validation.
function M.is_list(value)
  if type(value) ~= "table" then
    return false
  end
  if next(value) == nil and getmetatable(value) == nil then
    return false
  end
  local count = 0
  for _ in pairs(value) do
    count = count + 1
  end
  for key in pairs(value) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > count then
      return false
    end
  end
  return true
end

function M.set_of(list)
  local set = {}
  for _, item in ipairs(M.is_list(list) and list or {}) do
    set[item] = true
  end
  return set
end

function M.trim(text)
  return text:match("^%s*(.-)%s*$")
end

function M.comma_list(option, validate)
  local values = {}
  for value in (option or ""):gmatch("[^,]+") do
    value = M.trim(value)
    if value ~= "" then
      local normalized, problem = validate(value)
      if not normalized then
        return nil, problem
      end
      if normalized ~= "" then
        values[#values + 1] = normalized
      end
    end
  end
  return values
end

function M.within(path, root)
  return root == "/" or path == root or path:sub(1, #root + 1) == root .. "/"
end

--- Returns the project-relative patterns: the defaults, then the
--- comma-separated patterns in {extra}, or nil and why one cannot be used.
--- As in `.gitignore`, a pattern without a `/` matches at any depth, so it
--- gets a `**/` twin.
function M.denied_paths(extra)
  local patterns = {}
  for _, pattern in ipairs(DEFAULT_DENY) do
    patterns[#patterns + 1] = pattern
  end
  local extra_patterns, problem = M.comma_list(extra, function(pattern)
    pattern = pattern:gsub("^%./", "")
    if pattern:sub(1, 1) == "/" or (("/" .. pattern .. "/"):find("/../", 1, true)) or pattern:find(")", 1, true) then
      return nil, pattern .. " must be relative to the project, without `..` or `)`"
    end
    return pattern
  end)
  if not extra_patterns then
    return nil, problem
  end
  for _, pattern in ipairs(extra_patterns) do
    patterns[#patterns + 1] = pattern
    if not pattern:find("/", 1, true) then
      patterns[#patterns + 1] = "**/" .. pattern
    end
  end
  return patterns
end

--- Returns true if the owner can run a file whose {mode} is as
--- `stat -c %A` prints it.
function M.owner_executes(mode)
  return mode:match("^..." .. OWNER_EXECUTE_BITS) ~= nil
end

function M.first(items, limit)
  return { unpack(items, 1, math.min(#items, limit)) }, math.max(#items - limit, 0)
end

function M.climbs(path)
  for part in path:gmatch("[^/]+") do
    if part == ".." then
      return true
    end
  end
  return false
end

--- Returns {path} normalized, without `.` parts or `//`, or nil and the
--- reason when {path} is not a file inside the project.
function M.relative_path(path)
  if type(path) ~= "string" or path == "" then
    return nil, "an input path must be a string that is not empty"
  end
  if path:sub(1, 1) == "/" then
    return nil, path .. M.ABSOLUTE_PATH
  end
  if M.climbs(path) then
    return nil, path .. M.OUTSIDE_PROJECT
  end
  local parts = {}
  for part in path:gmatch("[^/]+") do
    if part ~= "." then
      parts[#parts + 1] = part
    end
  end
  if #parts == 0 then
    return nil, path .. " is the full project"
  end
  return table.concat(parts, "/")
end

--- Returns why {path} is not a project path as git spells it, or nil.
function M.path_problem(path)
  local spelled, problem = M.relative_path(path)
  if not spelled then
    return problem
  end
  if spelled ~= path then
    return path .. " is not in the format that git gives a path"
  end
  return nil
end

function M.shell_quote(text)
  return "'" .. text:gsub("'", "'\\''") .. "'"
end

function M.dependency_dirs(option)
  return M.comma_list(option, M.relative_path)
end

--- Escapes only ASCII pattern characters, so non-ASCII names pass unchanged.
function M.exclude_rule(dir)
  return "/" .. dir:gsub("[%*%?%[%]\\!# ]", "\\%0") .. "/\n"
end

function M.bash_quote(text)
  local quoted = M.shell_quote(text):gsub("%c", function(char)
    return string.format("'$'\\%03o''", char:byte())
  end)
  return M.escape_invisible(quoted, true)
end

function M.escape_invisible(text, shell)
  if not utf8.len(text) then
    return text
  end
  local parts, previous, previous_code = {}, 1, nil
  for offset, code in utf8.codes(text) do
    if not maki.text.is_printable(code, previous_code) then
      parts[#parts + 1] = text:sub(previous, offset - 1)
      local following = utf8.offset(text, 2, offset) or #text + 1
      if shell then
        local escaped = text:sub(offset, following - 1):gsub(".", function(byte)
          return string.format("\\%03o", byte:byte())
        end)
        parts[#parts + 1] = "'$'" .. escaped .. "''"
      else
        parts[#parts + 1] = string.format(code <= 0xffff and "\\u%04x" or "\\U%08x", code)
      end
      previous = following
    end
    previous_code = code
  end
  parts[#parts + 1] = text:sub(previous)
  return table.concat(parts)
end

--- Terminal control bytes can execute commands. Escape them and non-UTF-8 path bytes before
--- display.
function M.printable(path)
  path = path:gsub("\\", "\\\\")
  return (
    M.escape_invisible(
      path:gsub(utf8.len(path) and "%c" or "[%c\128-\255]", function(char)
        return string.format("\\%03o", char:byte())
      end),
      false
    )
  )
end

function M.first_non_utf8(paths)
  for _, path in ipairs(paths) do
    if not utf8.len(path) then
      return path
    end
  end
  return nil
end

return M
