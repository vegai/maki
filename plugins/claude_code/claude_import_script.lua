local git_utils = require("claude_git")
local path_utils = require("claude_paths")

local M = {}

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
local TEMP_PREFIX = ".maki-import-"
local TEMP_RANDOM = "XXXXXX"

local CURRENT_DIR = "./"
local HASH_FAILED = "cannot read a checkout or staged file, " .. NOTHING_IMPORTED
local CHECKOUT_GONE = "checkout path no longer resolves to the approved directory"
local GIT_FUNCTION = 'g() { %s "$@"; }'
-- Each write checks parent folders again to detect links swapped after validation.
local IMPORT_FUNCTIONS = [=[
changed() { printf '%s\n' "$note$1 $changed_note" >&2; exit "$changed_exit"; }
gone() { printf '%s\n' "$note$1 $gone_note" >&2; exit "$changed_exit"; }
hash_failed() { printf '%s\n' "$note$hash_note" >&2; exit "$changed_exit"; }
checkout_gone() { printf '%s\n' "$note$checkout_note" >&2; exit "$changed_exit"; }
folder() { [ ! -L "$1" ] && { [ -d "$1" ] || [ ! -e "$1" ]; } || changed "$1"; }
folders() {
  local dir=$1
  while [ "${dir%/*}" != "$dir" ]; do
    dir=${dir%/*}
    folder "$dir"
  done
}
regular() { [ -f "$1" ] && [ ! -L "$1" ] || changed "$1"; }
same_mode() { (( (0$2 & 0100) == (0$3 & 0100) )) || changed "$1"; }
absent() { [ ! -e "$1" ] && [ ! -L "$1" ] || changed "$1"; }
record() { printf '%s\0' "$1" >> "$receipt"; }
declare -A temp_of=() temp_dirs=() temp_modes=() modes=()
declare -a executable=() regular_files=()
trap 'rm -rf -- "$stage" "${temp_dirs[@]}"' EXIT
staged() {
  [ -f "$stage/$1" ] && [ ! -L "$stage/$1" ] || gone "$1"
}
fresh=$(printf '%o' $((0666 & ~0$(umask))))
new() {
  local parent=${1%/*} temp mode=$4
  folders "$1"
  if [ -z "${temp_dirs[$parent]:-}" ]; then
    mkdir -p -- "$parent"
    temp_dirs[$parent]=$(mktemp -d "$parent/$temp_template")
  fi
  temp=${temp_dirs[$parent]}/${#temp_of[@]}
  temp_of[$1]=$temp
  cp -- "$stage/$2" "$temp"
  temp_modes[$temp]=$mode
  modes[$mode]=1
  case "$3" in +x) executable+=("$temp");; -x) regular_files+=("$temp");; esac
}
permissions() {
  local mode temp
  local -a files
  for mode in "${!modes[@]}"; do
    files=()
    for temp in "${!temp_modes[@]}"; do
      [ "${temp_modes[$temp]}" != "$mode" ] || files+=("$temp")
    done
    printf '%s\0' "${files[@]}" | xargs -0 chmod "$(printf '%o' $((0$mode & 0777)))" --
  done
  [ "${#executable[@]}" = 0 ] || printf '%s\0' "${executable[@]}" | xargs -0 chmod +x --
  [ "${#regular_files[@]}" = 0 ] || printf '%s\0' "${regular_files[@]}" | xargs -0 chmod a-x --
}
keep() {
  local folder=$1 path
  shift
  for path in "$@"; do folders "$path"; done
  mkdir -p -- "$originals/$folder" "$displaced/$folder"
  printf '%s\0' "$@" | xargs -0 ln --target-directory "$originals/$folder" -- || {
    printf '%s\n' "$note$link_note" >&2
    exit 1
  }
}
put() { folders "$1"; ln -T -- "${temp_of[$1]}" "$1"; }
drop() { folders "$1"; mv -fT -- "$1" "$displaced/$1"; }
]=]

function M.import_timeout(changes)
  return IMPORT_TIMEOUT_SECS + #changes * IMPORT_SECS_PER_FILE
end

function M.temp_name(stage)
  local artifact, name = stage:match("([^/]+)/([^/]+)$")
  return TEMP_PREFIX .. artifact .. "." .. name:match("[^.]+$") .. "."
end

function M.receipt_path(artifact, stage)
  return artifact .. "/receipt." .. stage:match("[^/]+$")
end

--- Validates pre-staged blobs and the checkout before any write.
--- Relative paths stay in {project} even if its name becomes a link.
--- Hard links preserve writes through open descriptors. Renamed originals
--- also preserve atomic editor saves that replace those inodes. New files
--- use links that cannot overwrite a save made after the original moved.
--- A failed import runs `leftovers_script` to remove its temporary files.
--- Each change must first pass `claude_workspace.change_problem`.
function M.import_script(changes, project, git_dir, artifact, stage)
  local words = git_utils.shell_environment()
  for _, word in ipairs(git_utils.git({ "--git-dir=" .. git_dir })) do
    words[#words + 1] = path_utils.bash_quote(word)
  end
  local quoted_project = path_utils.bash_quote(project)
  local lines = { "# Import changes into " .. path_utils.printable(project) }
  for _, change in ipairs(changes) do
    lines[#lines + 1] = "# " .. change.status .. " " .. path_utils.printable(change.path)
  end
  lines[#lines + 1] = ""
  for _, line in ipairs({
    "set -e",
    "note=" .. path_utils.bash_quote(IMPORT_NOTE),
    "changed_note=" .. path_utils.bash_quote(IMPORT_CHANGED),
    "gone_note=" .. path_utils.bash_quote(ARTIFACT_GONE),
    "link_note=" .. path_utils.bash_quote(IMPORT_NO_LINK),
    "hash_note=" .. path_utils.bash_quote(HASH_FAILED),
    "checkout_note=" .. path_utils.bash_quote(CHECKOUT_GONE),
    "changed_exit=" .. IMPORT_CHANGED_EXIT,
    "originals="
      .. path_utils.bash_quote(artifact .. "/" .. path_utils.IMPORT_ORIGINALS .. "/" .. stage:match("[^/]+$")),
    "displaced="
      .. path_utils.bash_quote(artifact .. "/" .. path_utils.IMPORT_DISPLACED .. "/" .. stage:match("[^/]+$")),
    "stage=" .. path_utils.bash_quote(stage),
    "receipt=" .. path_utils.bash_quote(M.receipt_path(artifact, stage)),
    "temp_template=" .. path_utils.bash_quote(M.temp_name(stage) .. TEMP_RANDOM),
    GIT_FUNCTION:format(table.concat(words, " ")),
    IMPORT_FUNCTIONS,
    "cd -P -- " .. quoted_project .. ' && [ "$(pwd -P)" = ' .. quoted_project .. " ] || checkout_gone",
  }) do
    lines[#lines + 1] = line
  end
  -- Every original is kept before the first write, so a failed keep (a full
  -- disk, say) stops the import before it replaces or deletes any file. Each path starts
  -- with `./`, so it never reads as an option and `${1%/*}` is its folder.
  local stages, checks, modes, news, keeps, writes = {}, {}, {}, {}, {}, {}
  local mode_paths = {}
  local keep_folders, keep_order = {}, {}
  local hashed_paths, hashed_checks = {}, {}
  local function hash_path(path, sha, refused)
    local index = #hashed_paths
    hashed_paths[#hashed_paths + 1] = path
    hashed_checks[#hashed_checks + 1] = '[ "${hashes['
      .. index
      .. ']}" = '
      .. path_utils.bash_quote(sha)
      .. " ] || "
      .. refused
  end
  local staged, checked = {}, {}
  for _, change in ipairs(changes) do
    for folder in path_utils.folders(change.path) do
      if not checked[folder] then
        checked[folder] = true
        checks[#checks + 1] = "folder " .. path_utils.bash_quote(CURRENT_DIR .. folder)
      end
    end
    local path = path_utils.bash_quote(CURRENT_DIR .. change.path)
    if change.status == git_utils.ADDED then
      checks[#checks + 1] = "absent " .. path
    else
      checks[#checks + 1] = "regular " .. path
      hash_path(CURRENT_DIR .. change.path, change.old_sha, "changed " .. path)
      local index = #mode_paths
      mode_paths[#mode_paths + 1] = CURRENT_DIR .. change.path
      modes[#modes + 1] = "same_mode "
        .. path
        .. ' "${old_modes['
        .. index
        .. ']}" '
        .. path_utils.bash_quote(change.old_mode)
      local folder = maki.fs.dirname(CURRENT_DIR .. change.path)
      if not keep_folders[folder] then
        keep_folders[folder] = {}
        keep_order[#keep_order + 1] = folder
      end
      table.insert(keep_folders[folder], path)
    end
    if change.status == git_utils.DELETED then
      writes[#writes + 1] = "drop " .. path
    else
      if not staged[change.new_sha] then
        staged[change.new_sha] = true
        stages[#stages + 1] = "staged " .. path_utils.bash_quote(change.new_sha)
        hash_path(stage .. "/" .. change.new_sha, change.new_sha, "gone " .. path_utils.bash_quote(change.new_sha))
      end
      local flag = "keep"
      if change.status == git_utils.ADDED or change.old_mode ~= change.new_mode then
        flag = change.new_mode == git_utils.EXECUTABLE_MODE and "+x" or "-x"
      end
      local permissions = change.status == git_utils.ADDED and '"$fresh"'
        or '"${old_modes[' .. (#mode_paths - 1) .. ']}"'
      news[#news + 1] = "new "
        .. path
        .. " "
        .. path_utils.bash_quote(change.new_sha)
        .. " "
        .. path_utils.bash_quote(flag)
        .. " "
        .. permissions
      if change.status ~= git_utils.ADDED then
        writes[#writes + 1] = "drop " .. path
      end
      writes[#writes + 1] = "put " .. path
    end
    writes[#writes + 1] = "record " .. path_utils.bash_quote(change.path)
  end
  for _, folder in ipairs(keep_order) do
    keeps[#keeps + 1] = "keep " .. path_utils.bash_quote(folder) .. " " .. table.concat(keep_folders[folder], " ")
  end
  local hashes = {}
  if #hashed_paths > 0 then
    hashes = {
      "hashes=$(printf '%s' "
        .. path_utils.bash_quote(git_utils.quoted_lines(hashed_paths))
        .. " | g hash-object --no-filters --stdin-paths) || hash_failed",
      'mapfile -t hashes <<< "$hashes"',
    }
    table.move(hashed_checks, 1, #hashed_checks, #hashes + 1, hashes)
  end
  local metadata = {}
  for i, path in ipairs(mode_paths) do
    mode_paths[i] = path_utils.bash_quote(path)
  end
  if #mode_paths > 0 then
    metadata = {
      "old_modes=$(printf '%s\\0' "
        .. table.concat(mode_paths, " ")
        .. " | xargs -0 stat -c %a --) || changed "
        .. path_utils.bash_quote(project),
      'mapfile -t old_modes <<< "$old_modes"',
    }
  end
  for _, part in ipairs({ stages, checks, hashes, metadata, modes, news, { "permissions" }, keeps, writes }) do
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
    local dir = maki.fs.dirname(project .. "/" .. change.path)
    if change.status ~= git_utils.DELETED and not cleared[dir] then
      cleared[dir] = true
      local quoted = path_utils.shell_quote(dir)
      lines[#lines + 1] = "if into "
        .. quoted
        .. "; then rm -rf -- "
        .. path_utils.shell_quote(M.temp_name(stage))
        .. "* || status=1; elif [ -e "
        .. quoted
        .. " ] || [ -L "
        .. quoted
        .. " ]; then status=1; fi"
    end
  end
  for _, folder in ipairs(made) do
    local parent, name = maki.fs.dirname(folder), maki.fs.basename(folder)
    lines[#lines + 1] = "into "
      .. path_utils.shell_quote(parent)
      .. " && rmdir -- "
      .. path_utils.shell_quote(name)
      .. " 2>/dev/null"
  end
  lines[#lines + 1] = "rm -rf -- " .. path_utils.shell_quote(stage) .. " || status=1"
  lines[#lines + 1] = "exit $status"
  return table.concat(lines, "\n")
end

return M
