local path_utils = require("claude_paths")

local M = {}

local ESCAPES = { a = "\a", b = "\b", t = "\t", n = "\n", v = "\v", f = "\f", r = "\r" }
local GIT_UNCONFIGURED = { GIT_CONFIG_GLOBAL = "/dev/null", GIT_CONFIG_NOSYSTEM = "1" }
-- Repository-local variables listed by `git rev-parse --local-env-vars`.
local GIT_LOCAL_ENV_VARS = {
  "GIT_ALTERNATE_OBJECT_DIRECTORIES",
  "GIT_CONFIG",
  "GIT_CONFIG_PARAMETERS",
  "GIT_CONFIG_COUNT",
  "GIT_OBJECT_DIRECTORY",
  "GIT_DIR",
  "GIT_WORK_TREE",
  "GIT_IMPLICIT_WORK_TREE",
  "GIT_GRAFT_FILE",
  "GIT_INDEX_FILE",
  "GIT_NO_REPLACE_OBJECTS",
  "GIT_REPLACE_REF_BASE",
  "GIT_PREFIX",
  "GIT_SHALLOW_FILE",
  "GIT_COMMON_DIR",
}

-- With these attributes a blob id is the hash of the bytes on disk, the same
-- id `git hash-object --no-filters` gives for the checkout.
M.RAW_ATTRIBUTES = "* -text -filter -ident -working-tree-encoding !diff\n"
-- Override anything the worker writes into the snapshot's git config.
-- Quoted paths keep each listing in ASCII, which the job reader never splits
-- mid-character.
local GIT_CONFIG = {
  "-c",
  "user.name=maki",
  "-c",
  "user.email=maki@localhost",
  "-c",
  "core.hooksPath=/dev/null",
  "-c",
  "core.fsmonitor=false",
  "-c",
  "diff.ignoreSubmodules=all",
  "-c",
  "status.submoduleSummary=false",
  "-c",
  "core.quotePath=true",
  "-c",
  "commit.gpgSign=false",
  "-c",
  "gc.auto=0",
  "-c",
  "init.defaultBranch=snapshot",
}

M.ABSENT_MODE = "000000"
M.SYMLINK_MODE = "120000"
M.GITLINK_MODE = "160000"
M.EXECUTABLE_MODE = "100755"
M.REGULAR_MODE = "100644"
M.ADDED = "A"
M.DELETED = "D"
M.TYPE_CHANGED = "T"
M.NEVER_COLLECTED = { ".git", ".claude", ".maki" }
local ALSO_EXCLUDED, uncollected = {}, {}
for _, name in ipairs(M.NEVER_COLLECTED) do
  ALSO_EXCLUDED[#ALSO_EXCLUDED + 1] = name
  ALSO_EXCLUDED[#ALSO_EXCLUDED + 1] = "**/" .. name
  uncollected[#uncollected + 1] = name:gsub("%a", function(char)
    return "[" .. char:lower() .. char:upper() .. "]"
  end) .. "\n"
end
M.UNCOLLECTED_RULE = table.concat(uncollected)

function M.git(args)
  local argv = { "git" }
  for _, arg in ipairs(GIT_CONFIG) do
    argv[#argv + 1] = arg
  end
  for _, arg in ipairs(args) do
    argv[#argv + 1] = arg
  end
  return argv
end

function M.snapshot_git_env(env, git_dir, snapshot)
  local git_env = { GIT_DIR = git_dir, GIT_WORK_TREE = snapshot }
  for name, value in pairs(GIT_UNCONFIGURED) do
    git_env[name] = value
  end
  for name, value in pairs(env) do
    git_env[name] = git_env[name] or value
  end
  return git_env
end

function M.collection_pathspecs(dependencies)
  local specs = {}
  for _, pattern in ipairs(ALSO_EXCLUDED) do
    specs[#specs + 1] = ":(exclude,icase,glob)" .. pattern
    specs[#specs + 1] = ":(exclude,icase,glob)" .. pattern .. "/**"
  end
  for _, dir in ipairs(dependencies) do
    specs[#specs + 1] = ":(exclude,literal)" .. dir
  end
  return specs
end

function M.excluded_pathspecs(deny_read)
  local specs = M.collection_pathspecs({})
  for _, pattern in ipairs(assert(path_utils.denied_paths(deny_read))) do
    specs[#specs + 1] = ":(exclude,icase,glob)" .. pattern
    specs[#specs + 1] = ":(exclude,icase,glob)" .. pattern .. "/**"
  end
  return specs
end

function M.without_local_git_env(command)
  return "unset " .. table.concat(GIT_LOCAL_ENV_VARS, " ") .. "\n" .. command
end

function M.shell_environment()
  local words = { "env" }
  for _, name in ipairs(GIT_LOCAL_ENV_VARS) do
    words[#words + 1] = "-u"
    words[#words + 1] = path_utils.shell_quote(name)
  end
  local assignments = {}
  for name, value in pairs(GIT_UNCONFIGURED) do
    assignments[#assignments + 1] = name .. "=" .. path_utils.shell_quote(value)
  end
  table.sort(assignments)
  for _, assignment in ipairs(assignments) do
    words[#words + 1] = assignment
  end
  return words
end

function M.diff_command(artifact)
  local words = M.shell_environment()
  words[#words + 1] = "GIT_DIR=" .. path_utils.bash_quote(artifact.git)
  words[#words + 1] = "GIT_WORK_TREE=" .. path_utils.bash_quote(artifact.snapshot)
  for _, word in
    ipairs(M.git({
      "diff",
      "--no-ext-diff",
      "--no-textconv",
      "--ignore-submodules=all",
      artifact.base,
      artifact.tree,
      "--",
    }))
  do
    words[#words + 1] = path_utils.bash_quote(word)
  end
  return table.concat(words, " ")
end

function M.unquote(field)
  if field:sub(1, 1) ~= '"' then
    return field
  end
  local out, i, last = {}, 2, #field - 1
  while i <= last do
    local char = field:sub(i, i)
    local octal = char == "\\" and field:match("^[0-7][0-7][0-7]", i + 1)
    if octal then
      out[#out + 1] = string.char(tonumber(octal, 8))
      i = i + 4
    elseif char == "\\" then
      local escaped = field:sub(i + 1, i + 1)
      out[#out + 1] = ESCAPES[escaped] or escaped
      i = i + 2
    else
      out[#out + 1] = char
      i = i + 1
    end
  end
  return table.concat(out)
end

function M.quoted_lines(paths)
  local lines = {}
  for i, path in ipairs(paths) do
    lines[i] = '"'
      .. path:gsub('[%c"\\]', function(char)
        return string.format("\\%03o", char:byte())
      end)
      .. '"\n'
  end
  return table.concat(lines)
end

function M.listed_paths(output)
  local paths = {}
  for line in (output or ""):gmatch("[^\n]+") do
    paths[#paths + 1] = M.unquote(line)
  end
  return paths
end

function M.untracked_paths(output)
  local files, repositories = {}, {}
  for _, path in ipairs(M.listed_paths(output)) do
    local list = path:sub(-1) == "/" and repositories or files
    list[#list + 1] = path
  end
  return files, repositories
end

local function staged_entries(output)
  local entries = {}
  for line in (output or ""):gmatch("[^\n]+") do
    local tag, mode, sha, path = line:match("^([%a%?]?) ?(%d+) (%x+) %d+\t(.*)$")
    if mode and tag ~= "S" then
      entries[#entries + 1] = { mode = mode, sha = sha, path = M.unquote(path) }
    end
  end
  return entries
end

--- Returns each path once, even with several merge stages. Submodules come
--- back separately as the second value, because a submodule checkout is a
--- different repository.
function M.tracked_paths(output)
  local paths, submodules, seen = {}, {}, {}
  for _, entry in ipairs(staged_entries(output)) do
    if not seen[entry.path] then
      seen[entry.path] = true
      local list = entry.mode == M.GITLINK_MODE and submodules or paths
      list[#list + 1] = entry.path
    end
  end
  return paths, submodules
end

--- Returns each change as `{ path, status, old_mode, new_mode, old_sha,
--- new_sha }`.
function M.parse_raw_diff(output)
  local changes = {}
  for line in (output or ""):gmatch("[^\n]+") do
    local old_mode, new_mode, old_sha, new_sha, status, path = line:match("^:(%d+) (%d+) (%x+) (%x+) (%a)%d*\t(.*)$")
    if old_mode then
      changes[#changes + 1] = {
        path = M.unquote(path),
        status = status,
        old_mode = old_mode,
        new_mode = new_mode,
        old_sha = old_sha,
        new_sha = new_sha,
      }
    end
  end
  return changes
end

function M.binary_paths(output)
  local binary = {}
  for line in (output or ""):gmatch("[^\n]+") do
    local path = line:match("^%-\t%-\t(.*)$")
    if path then
      binary[M.unquote(path)] = true
    end
  end
  return binary
end

--- A file that became a folder, or the reverse, shows up as a change to the
--- file plus changes under the same path. Returns all of those paths,
--- because none of them can be applied without the others.
function M.retyped_paths(changes)
  local paths, folders, found = {}, {}, {}
  for _, change in ipairs(changes) do
    paths[change.path] = true
    for slash in change.path:gmatch("()/") do
      folders[change.path:sub(1, slash - 1)] = true
    end
  end
  for _, change in ipairs(changes) do
    found[change.path] = folders[change.path]
    for slash in change.path:gmatch("()/") do
      found[change.path] = found[change.path] or paths[change.path:sub(1, slash - 1)]
    end
  end
  return found
end

return M
