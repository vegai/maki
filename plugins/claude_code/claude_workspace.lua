-- The process-free half of the snapshot and the import: what a snapshot
-- holds, how maki parses git output, and the scripts the import runs.

local M = {}

M.REPOSITORY_KIND = "repository"

-- Keeps hashes and listings independent of the user's git config.
local GIT_UNCONFIGURED = { GIT_CONFIG_GLOBAL = "/dev/null", GIT_CONFIG_NOSYSTEM = "1" }

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

-- Artifact names come from mktemp, and an id from the model must match them.
M.ARTIFACT_ID = "^%w+$"

local ABSENT_MODE = "000000"
local SYMLINK_MODE = "120000"
local GITLINK_MODE = "160000"
local EXECUTABLE_MODE = "100755"
local REGULAR_MODE = "100644"
local ADDED = "A"
local DELETED = "D"
M.ADDED = ADDED
local TYPE_CHANGED = "T"
local TYPE_KIND = "type"
M.TYPE_KIND = TYPE_KIND
-- Symlinks, submodules and type changes can affect paths outside an ordinary file import. The
-- user must apply these changes manually.
local IMPORTABLE = { text = true, binary = true, mode = true }
-- Manifest fields end up in the command the user approves, so any other
-- value is refused.
local KNOWN_STATUSES = { A = true, M = true, D = true, T = true }
local KNOWN_MODES = {
  [ABSENT_MODE] = true,
  [REGULAR_MODE] = true,
  [EXECUTABLE_MODE] = true,
  [SYMLINK_MODE] = true,
  [GITLINK_MODE] = true,
}
-- The snapshot copies neither, and Claude Code config can add hooks.
local NEVER_COLLECTED = { [".git"] = true, [".claude"] = true, [".maki"] = true }
local OBJECT_ID_LENGTHS = { [40] = true, [64] = true }
-- Every check runs before the first write, and a failed check exits with
-- this code.
local IMPORT_CHANGED_EXIT = 3
-- The bash tool's default, plus time for each imported file.
local IMPORT_TIMEOUT_SECS = 120
local IMPORT_SECS_PER_FILE = 1
local IMPORT_NOTE = "claude_code_import: "
local NOTHING_IMPORTED = "so maki imported no changes"
local IMPORT_CHANGED = "changed in the checkout after the snapshot, " .. NOTHING_IMPORTED
local ARTIFACT_GONE = "is no longer in the artifact, " .. NOTHING_IMPORTED
local IMPORT_NO_LINK = "cannot preserve open-file writes. Check the ln error above. "
  .. "For a cross-filesystem error, put artifact_dir on the checkout filesystem."
M.IMPORT_ORIGINALS = "originals"
M.IMPORT_DISPLACED = "displaced"
M.STAGE_TEMPLATE = "stage.XXXXXX"
-- Each attempt needs unique stage and temporary paths so its cleanup cannot remove another
-- attempt's files.
local TEMP_PREFIX = ".maki-import-"
local TEMP_RANDOM = "XXXXXX"

-- Claude Code project config can hold hooks or MCP servers. A file named
-- `.claude` is left out too, because the manifest check refuses that name.
local ALSO_EXCLUDED = { ".claude", "**/.claude", ".maki", "**/.maki" }
M.UNCOLLECTED_RULE = ".[cC][lL][aA][uU][dD][eE]\n.[mM][aA][kK][iI]\n"
-- The owner's execute bit in `stat -c %A`: `x`, or `s` with setuid.
local OWNER_EXECUTE_BITS = "[xs]"
-- As a shell `case` pattern.
local OWNER_EXECUTES = "???" .. OWNER_EXECUTE_BITS .. "*"
local CURRENT_DIR = "./"
local ESCAPES = { a = "\a", b = "\b", t = "\t", n = "\n", v = "\v", f = "\f", r = "\r" }

--- Returns true if the owner can run a file whose {mode} is as
--- `stat -c %A` prints it.
function M.owner_executes(mode)
  return mode:match("^..." .. OWNER_EXECUTE_BITS) ~= nil
end

function M.first(items, limit)
  return { unpack(items, 1, math.min(#items, limit)) }, math.max(#items - limit, 0)
end

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

function M.excluded_pathspecs(deny_read)
  local launch = require("claude_launch")
  local specs = {}
  for _, pattern in ipairs(assert(launch.denied_paths(deny_read))) do
    specs[#specs + 1] = ":(exclude,glob)" .. pattern
    specs[#specs + 1] = ":(exclude,glob)" .. pattern .. "/**"
  end
  for _, pattern in ipairs(ALSO_EXCLUDED) do
    specs[#specs + 1] = ":(exclude,icase,glob)" .. pattern
    specs[#specs + 1] = ":(exclude,icase,glob)" .. pattern .. "/**"
  end
  return specs
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
    return nil, path .. " must be relative to the project"
  end
  if M.climbs(path) then
    return nil, path .. " goes out of the project"
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

--- Returns the import command's time limit in seconds. A large import gets
--- more than the bash default, because it stages and checks every file
--- before the first write.
function M.import_timeout(changes)
  return IMPORT_TIMEOUT_SECS + #changes * IMPORT_SECS_PER_FILE
end

function M.shell_quote(text)
  return "'" .. text:gsub("'", "'\\''") .. "'"
end

local function git_environment()
  local words = { "env", "-u", "GIT_CONFIG_PARAMETERS", "-u", "GIT_CONFIG_COUNT" }
  local assignments = {}
  for name, value in pairs(GIT_UNCONFIGURED) do
    assignments[#assignments + 1] = name .. "=" .. M.shell_quote(value)
  end
  table.sort(assignments)
  for _, assignment in ipairs(assignments) do
    words[#words + 1] = assignment
  end
  return words
end

function M.diff_command(artifact)
  local words = git_environment()
  words[#words + 1] = "GIT_DIR=" .. M.bash_quote(artifact.git)
  words[#words + 1] = "GIT_WORK_TREE=" .. M.bash_quote(artifact.snapshot)
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
    words[#words + 1] = M.bash_quote(word)
  end
  return table.concat(words, " ")
end

--- Normalizes each directory, so one directory always has one spelling.
function M.dependency_dirs(option)
  local dirs = {}
  for dir in (option or ""):gmatch("[^,]+") do
    dir = dir:match("^%s*(.-)%s*$")
    if dir ~= "" then
      local spelled, problem = M.relative_path(dir)
      if not spelled then
        return nil, problem
      end
      dirs[#dirs + 1] = spelled
    end
  end
  return dirs
end

--- Escapes only ASCII pattern characters, so non-ASCII names pass unchanged.
function M.exclude_rule(dir)
  return "/" .. dir:gsub("[%*%?%[%]\\!# ]", "\\%0") .. "/\n"
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

--- Quotes {text} for bash, with each control byte as a `$'\ooo'` escape, so
--- the command that the user approves shows no control bytes.
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
  local parts, previous = {}, 1
  for offset, code in utf8.codes(text) do
    if
      code == 0x061c
      or code == 0x034f
      or code == 0x180e
      or code == 0xfeff
      or code >= 0x200b and code <= 0x200f
      or code >= 0x202a and code <= 0x202e
      or code >= 0x2060 and code <= 0x206f
      or code >= 0xfe00 and code <= 0xfe0f
      or code >= 0xe0000 and code <= 0xe007f
    then
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
  end
  parts[#parts + 1] = text:sub(previous)
  return table.concat(parts)
end

--- Terminal control bytes can execute commands. Escape them and non-UTF-8 path bytes before
--- display.
function M.printable(path)
  return (
    M.escape_invisible(
      path:gsub(utf8.len(path) and "%c" or "[%c\128-\255]", function(char)
        return string.format("\\%03o", char:byte())
      end),
      false
    )
  )
end

--- Quotes every name, so no byte in a name can start a new line.
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

--- maki can only pass UTF-8 names to a process.
function M.first_non_utf8(paths)
  for _, path in ipairs(paths) do
    if not utf8.len(path) then
      return path
    end
  end
  return nil
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
      local list = entry.mode == GITLINK_MODE and submodules or paths
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

function M.repository_path(path, repositories)
  for repository in pairs(repositories or {}) do
    if repository == "." or path == repository or path:sub(1, #repository + 1) == repository .. "/" then
      return true
    end
  end
  return false
end

function M.kind(change, binary)
  if change.old_mode == SYMLINK_MODE or change.new_mode == SYMLINK_MODE then
    return "symlink"
  end
  if change.old_mode == GITLINK_MODE or change.new_mode == GITLINK_MODE then
    return "submodule"
  end
  if change.status == TYPE_CHANGED then
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
    return "a change is not a table"
  end
  local path_problem = M.path_problem(change.path)
  if path_problem then
    return path_problem
  end
  for part in change.path:gmatch("[^/]+") do
    if NEVER_COLLECTED[part:lower()] then
      return change.path .. " is in " .. part .. ", which a snapshot does not copy"
    end
  end
  if not KNOWN_STATUSES[change.status] then
    return change.path .. " has the unknown status " .. tostring(change.status)
  end
  for _, field in ipairs({ "old_sha", "new_sha" }) do
    local sha = change[field]
    if type(sha) ~= "string" or not sha:match("^%x+$") or not OBJECT_ID_LENGTHS[#sha] then
      return change.path .. " has no object id in " .. field
    end
  end
  for _, field in ipairs({ "old_mode", "new_mode" }) do
    if not KNOWN_MODES[change[field]] then
      return change.path .. " has an unknown " .. field
    end
  end
  -- Only an added file has no `old_mode`, and only a deleted file has no
  -- `new_mode`.
  for _, side in ipairs({ "old", "new" }) do
    local absent = change[side .. "_mode"] == ABSENT_MODE
    local lacks = side == "old" and ADDED or DELETED
    if absent ~= (change.status == lacks) then
      return change.path .. ": its " .. side .. "_mode does not agree with its status " .. change.status
    end
    if absent == (change[side .. "_sha"]:match("[^0]") ~= nil) then
      return change.path .. " has a " .. side .. " object id that does not agree with its " .. side .. " mode"
    end
  end
  -- `retyped_paths` can make any change a type change, which is never
  -- imported.
  local kind = M.kind(change, { [change.path] = change.kind == "binary" })
  if change.kind ~= kind and change.kind ~= TYPE_KIND and change.kind ~= M.REPOSITORY_KIND then
    return change.path .. " is a " .. kind .. " change, and not " .. tostring(change.kind)
  end
  return nil
end

function M.importable(change)
  return IMPORTABLE[change.kind] == true
end

--- Includes the mode, because a chmod changes the mode and leaves the blob.
function M.file_state(sha, executable)
  return sha .. " " .. (executable and EXECUTABLE_MODE or REGULAR_MODE)
end

--- {current} maps each path to its `file_state`, or false when it is missing.
function M.conflicts(changes, current)
  local found = {}
  for _, change in ipairs(changes) do
    local expected = change.status ~= ADDED and M.file_state(change.old_sha, change.old_mode == EXECUTABLE_MODE)
    if current[change.path] ~= expected then
      found[#found + 1] = change.path
    end
  end
  return found
end

function M.landed(changes, current)
  local found = {}
  for _, change in ipairs(changes) do
    local wanted = change.status ~= DELETED and M.file_state(change.new_sha, change.new_mode == EXECUTABLE_MODE)
    if current[change.path] == wanted then
      found[#found + 1] = change
    end
  end
  return found
end

function M.summary(changes)
  local lines = {}
  for _, change in ipairs(changes) do
    local line = change.status .. " " .. M.printable(change.path)
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
function M.temp_name(stage)
  local artifact, name = stage:match("([^/]+)/([^/]+)$")
  return TEMP_PREFIX .. artifact .. "." .. name:match("[^.]+$") .. "."
end

--- The command stages blobs, then validates the checkout before any write.
--- Relative paths stay in {project} even if its name becomes a link.
--- Hard links preserve writes through open descriptors. Renamed originals
--- also preserve atomic editor saves that replace those inodes. New files
--- use links that cannot overwrite a save made after the original moved.
--- A failed import runs `leftovers_script` to remove its temporary files.
function M.import_script(changes, project, git_dir, artifact, stage)
  local words = git_environment()
  for _, word in ipairs(M.git({ "--git-dir=" .. git_dir })) do
    words[#words + 1] = M.bash_quote(word)
  end
  local function refuse(name, message)
    return name
      .. "() { printf '%s\\n' \""
      .. IMPORT_NOTE
      .. "$1 "
      .. message
      .. '" >&2; exit '
      .. IMPORT_CHANGED_EXIT
      .. "; }"
  end
  local quoted_project = M.bash_quote(project)
  local lines = {
    "set -e",
    refuse("changed", IMPORT_CHANGED),
    refuse("gone", ARTIFACT_GONE),
    "cd -P -- " .. quoted_project .. ' && [ "$(pwd -P)" = ' .. quoted_project .. " ] || changed " .. quoted_project,
    "g() { " .. table.concat(words, " ") .. ' "$@"; }',
    'folder() { [ ! -L "$1" ] && { [ -d "$1" ] || [ ! -e "$1" ]; } || changed "$1"; }',
    -- Each step that touches a file checks its folders again, so a folder
    -- swapped for a link after the checks stops the import there.
    'folders() { local dir=$1; while [ "${dir%/*}" != "$dir" ]; do dir=${dir%/*}; folder "$dir"; done; }',
    'mode() { case $(stat -c %A -- "$1") in '
      .. OWNER_EXECUTES
      .. ") echo "
      .. EXECUTABLE_MODE
      .. ";; *) echo "
      .. REGULAR_MODE
      .. ";; esac; }",
    -- Hashes use the manifest's object format, which can differ from the
    -- checkout repository's.
    'hash() { g hash-object --no-filters -- "$1"; }',
    'same() { [ -f "$1" ] && [ ! -L "$1" ] && [ "$(hash "$1" 2>/dev/null)" = "$2" ] '
      .. '&& [ "$(mode "$1")" = "$3" ] || changed "$1"; }',
    'absent() { [ ! -e "$1" ] && [ ! -L "$1" ] || changed "$1"; }',
    "originals=" .. M.bash_quote(artifact .. "/" .. M.IMPORT_ORIGINALS .. "/" .. stage:match("[^/]+$")),
    "displaced=" .. M.bash_quote(artifact .. "/" .. M.IMPORT_DISPLACED .. "/" .. stage:match("[^/]+$")),
    "stage=" .. M.bash_quote(stage),
    "declare -A temp_of=()",
    'trap \'rm -rf -- "$stage"; rm -f -- "${temp_of[@]}"\' EXIT',
    'staged() { g cat-file blob "$1" > "$stage/$1" && [ "$(hash "$stage/$1")" = "$1" ] || gone "$1"; }',
    "fresh=$(printf '%o' $((0666 & ~0$(umask))))",
    'new() { local temp; folders "$1"; mkdir -p -- "${1%/*}"; temp=$(mktemp "${1%/*}/'
      .. M.temp_name(stage)
      .. TEMP_RANDOM
      .. '"); temp_of[$1]=$temp; '
      .. 'cat -- "$stage/$2" > "$temp"; if [ -e "$1" ]; then chmod --reference="$1" -- "$temp"; '
      .. 'else chmod "$fresh" -- "$temp"; fi; chmod "$3" -- "$temp"; }',
    'keep() { folders "$1"; mkdir -p -- "$originals/${1%/*}" "$displaced/${1%/*}"; '
      .. '[ "$1" -ef "$originals/$1" ] || ln -T -- "$1" "$originals/$1" || { printf "%s\\n" '
      .. M.bash_quote(IMPORT_NOTE .. IMPORT_NO_LINK)
      .. " >&2; exit 1; }; }",
    'put() { folders "$1"; ln -T -- "${temp_of[$1]}" "$1"; }',
    'drop() { folders "$1"; mv -fT -- "$1" "$displaced/$1"; }',
  }
  -- Every original is kept before the first write, so a failed keep (a full
  -- disk, say) stops the import before it changes anything. Each path starts
  -- with `./`, so it never reads as an option and `${1%/*}` is its folder.
  local stages, checks, news, keeps, writes = {}, {}, {}, {}, {}
  local staged, checked = {}, {}
  for _, change in ipairs(changes) do
    for slash in change.path:gmatch("()/") do
      local folder = change.path:sub(1, slash - 1)
      if not checked[folder] then
        checked[folder] = true
        checks[#checks + 1] = "folder " .. M.bash_quote(CURRENT_DIR .. folder)
      end
    end
    local path = M.bash_quote(CURRENT_DIR .. change.path)
    if change.status == ADDED then
      checks[#checks + 1] = "absent " .. path
    else
      checks[#checks + 1] = "same " .. path .. " " .. change.old_sha .. " " .. change.old_mode
      keeps[#keeps + 1] = "keep " .. path
    end
    if change.status == DELETED then
      writes[#writes + 1] = "drop " .. path
    else
      if not staged[change.new_sha] then
        staged[change.new_sha] = true
        stages[#stages + 1] = "staged " .. change.new_sha
      end
      local flag = change.new_mode == EXECUTABLE_MODE and "+x" or "-x"
      news[#news + 1] = "new " .. path .. " " .. change.new_sha .. " " .. flag
      if change.status ~= ADDED then
        writes[#writes + 1] = "drop " .. path
      end
      writes[#writes + 1] = "put " .. path
    end
  end
  for _, part in ipairs({ stages, checks, news, keeps, writes }) do
    for _, line in ipairs(part) do
      lines[#lines + 1] = line
    end
  end
  return table.concat(lines, "\n")
end

--- Cleans up after a failed import: its temporary files, each empty folder
--- in {made}, and {stage}. It enters each folder by its resolved path. The
--- import follows a folder that became a link, as maki's write tool does,
--- but the cleanup does not. It exits with an error when it cannot enter an
--- existing folder or remove its files, because temporary files may remain.
function M.leftovers_script(changes, project, stage, made)
  local lines = { 'into() { cd -P -- "$1" 2>/dev/null && [ "$(pwd -P)" = "$1" ]; }', "status=0" }
  local cleared = {}
  for _, change in ipairs(changes) do
    local dir = (project .. "/" .. change.path):match("^(.*)/")
    if change.status ~= DELETED and not cleared[dir] then
      cleared[dir] = true
      local quoted = M.shell_quote(dir)
      lines[#lines + 1] = "if into "
        .. quoted
        .. "; then rm -f -- "
        .. M.temp_name(stage)
        .. "* || status=1; elif [ -e "
        .. quoted
        .. " ] || [ -L "
        .. quoted
        .. " ]; then status=1; fi"
    end
  end
  for _, folder in ipairs(made) do
    local parent, name = folder:match("^(.*)/([^/]+)$")
    lines[#lines + 1] = "into " .. M.shell_quote(parent) .. " && rmdir -- " .. M.shell_quote(name) .. " 2>/dev/null"
  end
  lines[#lines + 1] = "rm -rf -- " .. M.shell_quote(stage) .. " || status=1"
  lines[#lines + 1] = "exit $status"
  return table.concat(lines, "\n")
end

return M
