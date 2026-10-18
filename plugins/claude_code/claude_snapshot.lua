local native = require("maki.claude_code.internal")

-- The private snapshot a coding worker runs in, and the import of its
-- changes. Each step runs as a job of the call, so a cancel or a time limit
-- stops it.

local launch = require("claude_launch")
local workspace = require("claude_workspace")
local git_utils = require("claude_git")
local path_utils = require("claude_paths")

local M = {}

local TRASH_PREFIX = ".trash-"
local SWEEP_LOCK = ".sweep.lock"
M.CANNOT_SWEEP = "claude_code: maki cannot lock artifact cleanup in "
local SWEEP_CONTENTS = [[
set -e
cd -P -- "$1"
[ "$(pwd -P)" = "$1" ]
find -P . -type d ! -perm -u=rx -exec chmod u+rwx -- {} \;
find -P . -type d ! -perm -u=w -exec chmod u+w -- {} +
find . -mindepth 1 -maxdepth 1 ! -name "$2" -exec rm -rf -- {} +
rm -f -- "./$2"
rmdir -- "$1"
]]
local ARTIFACT_TEMPLATE = "XXXXXX"
local LINK_TO_NOWHERE = " is a link to a missing target"
local TMP_DIR = "tmp"
local QUARANTINE_DIR = "git-metadata"

local CHECK_INDEX = "consistency-index"
local COPY_STARTED = "copy-started"
-- The artifact directory can be a folder that holds other things, so the
-- sweep removes only folders with this marker.
local OWNER_MARKER = ".maki-claude-code-artifact"
local VENV_MARKER = "pyvenv.cfg"
local SHOWN_PATHS = 10
local C_LOCALE = "C"
-- What `cp --reflink=always` reports when the filesystem cannot clone:
-- EOPNOTSUPP, EXDEV and EINVAL.
local CLONE_UNSUPPORTED = {
  ["Operation not supported"] = true,
  ["Invalid cross-device link"] = true,
  ["Invalid argument"] = true,
}
local SECS_PER_HOUR = 3600

local NON_UTF8_PATH = "the file name %s is not UTF-8, and maki cannot give it to a process"
local NOT_UTF8_CHANGES = "the names of %s are not UTF-8, so the manifest cannot hold these changes. "
  .. "Apply them manually from %s"
local SUBMODULES_LEFT_OUT = "the snapshot has no submodules, so the worker did not see "
local PREPARED = "the prepare command changed %s, and the changes for import do not include these files"

local DROP_ESCAPING_LINKS = [[
root=$1
shift
for link do
  target=$(realpath -m -- "$link" 2>/dev/null) || target=
  case $target in "$root" | "$root"/*) continue ;; esac
  rm -f -- "$link" || exit
  printf '%s' "${link#"$root"/}" | od -An -v -tx1 | tr -d ' \n'
  echo
done
]]

-- Replace bytes that can break the job reader with `?`. Prefix paths with `./`
-- so names such as `-delete` cannot become `find` expressions.
local CHANGED_SINCE = [[find "./$1" -cnewer "$2" -print | LC_ALL=C tr -c '\n[:print:]' '?']]

function M.shown(paths)
  local listed, more = path_utils.first(paths, SHOWN_PATHS)
  for i, path in ipairs(listed) do
    listed[i] = path_utils.printable(path)
  end
  return table.concat(listed, ", ") .. (more > 0 and " and " .. more .. " more" or "")
end

local function from_hex(line)
  return (line:gsub("%x%x", function(pair)
    return string.char(tonumber(pair, 16))
  end))
end

local function checkout_git(call, spec, args)
  return call:run_quick(git_utils.git(args), { env = spec.env, cwd = spec.cwd })
end

local function list_files(call, spec, mode, pathspecs)
  local args = { "ls-files", mode, "--" }
  if mode == "-s" then
    table.insert(args, 2, "-t")
  end
  for _, pathspec in ipairs(pathspecs) do
    args[#args + 1] = pathspec
  end
  for _, pathspec in ipairs(spec.excluded_pathspecs) do
    args[#args + 1] = pathspec
  end
  return checkout_git(call, spec, args)
end

-- Copies links as links. {reflink} is a `--reflink` flag, or nil. `cp`
-- runs in the C locale, so `clone_unsupported` can read its errors.
local function copy_into(call, artifact, spec, paths, reflink)
  local argv = { "xargs", "-0", "-r", "cp", "-P", "--preserve=mode,timestamps", "--parents", "-t", artifact.snapshot }
  argv[#argv + 1] = reflink
  argv[#argv + 1] = "--"
  local env = table.clone(spec.env)
  env.LC_ALL = C_LOCALE
  return call:run_quick(argv, {
    env = env,
    cwd = spec.cwd,
    stdin = table.concat(paths, "\0"),
    timeout_ms = artifact.timeout_ms,
  })
end

-- Returns the reason when every `cp` error says the filesystem cannot
-- clone. Any other error, such as a full disk, would stop a full copy too.
-- The first line of {err} is the exit, and each later one is
-- `<program>: <message>: <cause>`, where the program can be a full path.
local function clone_unsupported(err)
  local cause
  for line in err:gmatch("\n([^\n]+)") do
    cause = line:match(":%s*([^:]+)$")
    if not CLONE_UNSUPPORTED[cause] then
      return nil
    end
  end
  return cause
end

local function snapshot_git(call, artifact, args, stdin)
  return call:run_quick(
    git_utils.git(args),
    { env = artifact.git_env, cwd = artifact.snapshot, stdin = stdin, timeout_ms = artifact.timeout_ms }
  )
end

local function index_files(call, artifact, paths)
  if #paths == 0 then
    return nil
  end
  local _, err = snapshot_git(call, artifact, { "update-index", "--add", "-z", "--stdin" }, table.concat(paths, "\0"))
  return err
end

local function commit_snapshot(call, artifact, message)
  local _, commit_err = snapshot_git(call, artifact, { "commit", "-q", "--allow-empty", "-m", message })
  if commit_err then
    return nil, commit_err
  end
  return snapshot_git(call, artifact, { "rev-parse", "HEAD" })
end

function M.disk_blobs(call, env, cwd, paths, keep_on_cancel, timeout_ms)
  if #paths == 0 then
    return {}
  end
  local out, err = call:run_quick(git_utils.git({ "hash-object", "--no-filters", "--stdin-paths" }), {
    env = env,
    cwd = cwd,
    stdin = git_utils.quoted_lines(paths),
    keep_on_cancel = keep_on_cancel,
    timeout_ms = timeout_ms,
  })
  if not out then
    return nil, err
  end
  return maki.split(out, "\n")
end

-- Returns the tracked files as they are on disk, plus the untracked files
-- under {spec.include}. Submodules are left out, with a note in {artifact}.
local function snapshot_paths(call, artifact, spec)
  local tracked, tracked_err = list_files(call, spec, "-s", { "." })
  if not tracked then
    return nil, tracked_err
  end
  local deleted, deleted_err = list_files(call, spec, "-d", { "." })
  if not deleted then
    return nil, deleted_err
  end
  local seen = {}
  for _, path in ipairs(git_utils.listed_paths(deleted)) do
    seen[path] = true
  end
  local paths = {}
  local function take(path)
    if not seen[path] then
      seen[path] = true
      paths[#paths + 1] = path
    end
  end
  local files, submodules = git_utils.tracked_paths(tracked)
  for _, path in ipairs(files) do
    take(path)
  end
  if #spec.include > 0 then
    local specs = {}
    for _, path in ipairs(spec.include) do
      if not maki.fs.metadata(maki.fs.joinpath(spec.cwd, path)) then
        return nil, "the input " .. path .. " is missing"
      end
      specs[#specs + 1] = ":(literal)" .. path
    end
    local untracked, untracked_err = list_files(call, spec, "-o", specs)
    if not untracked then
      return nil, untracked_err
    end
    local included, repositories = git_utils.untracked_paths(untracked)
    if #repositories > 0 then
      artifact.notes[#artifact.notes + 1] = "maki did not copy the repositories under include: "
        .. M.shown(repositories)
    end
    for _, path in ipairs(included) do
      local dependency = false
      for _, dir in ipairs(spec.dependencies) do
        dependency = dependency or path_utils.within(path, dir)
      end
      if not dependency then
        take(path)
      end
    end
  end
  local unusual = path_utils.first_non_utf8(paths)
  if unusual then
    return nil, NON_UTF8_PATH:format(path_utils.printable(unusual))
  end
  if #submodules > 0 then
    artifact.notes[#artifact.notes + 1] = SUBMODULES_LEFT_OUT .. M.shown(submodules)
  end
  return paths, nil, files
end

-- The project's exclusions apply, because the worker's shell can read the
-- whole copy.
local function copy_dependencies(call, artifact, spec, paths)
  local copied, tracked_folders = {}, {}
  for _, path in ipairs(paths) do
    tracked_folders[path] = true
    for slash in path:gmatch("()/") do
      tracked_folders[path:sub(1, slash - 1)] = true
    end
  end
  for _, dir in ipairs(spec.dependencies) do
    local source = maki.fs.joinpath(spec.cwd, dir)
    local target = maki.fs.joinpath(artifact.snapshot, dir)
    local meta = maki.fs.metadata(source)
    if not (meta and meta.is_dir) then
      artifact.notes[#artifact.notes + 1] = "the dependency "
        .. dir
        .. " is not a directory here, so maki did not copy it"
    elseif tracked_folders[dir] then
      artifact.notes[#artifact.notes + 1] = "the dependency "
        .. dir
        .. " contains tracked files, so maki copied only those. List only untracked directories."
    elseif maki.fs.metadata(maki.fs.joinpath(source, VENV_MARKER)) then
      artifact.notes[#artifact.notes + 1] = "the dependency "
        .. dir
        .. " is a Python virtual environment, and its scripts hard-code its path in the checkout, so maki did not "
        .. "copy it. Recreate it with the `prepare` command."
    else
      local listed, list_err = list_files(call, spec, "-o", { ":(literal)" .. dir })
      if not listed then
        return nil, "maki cannot list the dependency " .. dir .. ": " .. list_err
      end
      local files, nested = git_utils.untracked_paths(listed)
      local unusual = path_utils.first_non_utf8(files)
      if unusual then
        return nil, NON_UTF8_PATH:format(path_utils.printable(unusual))
      end
      local cloned, clone_err, timed_out = copy_into(call, artifact, spec, files, "--reflink=always")
      if timed_out then
        return nil, "maki cannot copy the dependency " .. dir .. ": " .. clone_err
      end
      if not cloned then
        local cause = clone_unsupported(clone_err)
        if not cause then
          return nil, "maki cannot clone the dependency " .. dir .. ": " .. clone_err
        end
        maki.fs.rm(target, { recursive = true, force = true })
        local _, copy_err = copy_into(call, artifact, spec, files, "--reflink=auto")
        if copy_err then
          return nil, "maki cannot copy the dependency " .. dir .. ": " .. copy_err
        end
        artifact.notes[#artifact.notes + 1] = "maki copied the dependency "
          .. dir
          .. " as a full copy, because the filesystem cannot clone it: "
          .. cause
      end
      if #nested > 0 then
        artifact.notes[#artifact.notes + 1] = "maki did not copy the repositories in the dependency "
          .. dir
          .. ": "
          .. M.shown(nested)
      end
      copied[#copied + 1] = dir
    end
  end
  return copied
end

-- A link out of the snapshot could lead the worker into the checkout or
-- anywhere else.
local function drop_escaping_links(call, artifact, env)
  local out, err = call:run_quick({
    "find",
    artifact.snapshot,
    "-type",
    "l",
    "-exec",
    "sh",
    "-c",
    DROP_ESCAPING_LINKS,
    "sh",
    artifact.snapshot,
    "{}",
    "+",
  }, { env = env, timeout_ms = artifact.timeout_ms })
  if not out then
    return nil, err
  end
  local dropped, names = {}, {}
  for line in out:gmatch("%x+") do
    local path = from_hex(line)
    dropped[path] = true
    names[#names + 1] = path
  end
  if #names > 0 then
    artifact.notes[#artifact.notes + 1] = "maki removed external symlinks from the snapshot: " .. M.shown(names)
  end
  return dropped
end

-- Commits exactly {paths} as the base, ignored paths included. The
-- repository sits next to the snapshot, where the worker cannot write, so
-- later git commands read metadata the worker could not touch. A `.git`
-- file lets the worker's git read it.
local function commit_base(call, artifact, paths, dependencies)
  local _, init_err = snapshot_git(call, artifact, { "init", "-q", "--template=" })
  if init_err then
    return init_err
  end
  local info = maki.fs.joinpath(artifact.git, "info")
  local exclude = { git_utils.UNCOLLECTED_RULE }
  for _, dir in ipairs(dependencies) do
    exclude[#exclude + 1] = path_utils.exclude_rule(dir)
  end
  local _, mkdir_err = maki.fs.mkdir(info)
  local _, attributes_err = maki.fs.write(maki.fs.joinpath(info, "attributes"), git_utils.RAW_ATTRIBUTES)
  local _, exclude_err = maki.fs.write(maki.fs.joinpath(info, "exclude"), table.concat(exclude))
  local link_err = M.sanitize(artifact)
  local setup_err = mkdir_err or attributes_err or exclude_err or link_err
  if setup_err then
    return "maki cannot make the repository of the snapshot: " .. setup_err
  end
  if artifact.repositories then
    local kept = {}
    for _, path in ipairs(paths) do
      if not workspace.repository_path(path, artifact.repositories) then
        kept[#kept + 1] = path
      end
    end
    paths = kept
  end
  local add_err = index_files(call, artifact, paths)
  if add_err then
    return add_err
  end
  local base, base_err = commit_snapshot(call, artifact, "maki snapshot")
  artifact.base = base
  return base_err
end

-- Diffs the base against the checkout as its work tree, which shows every
-- write during the copy. A temporary index leaves the snapshot's index
-- alone.
local function consistency_problem(call, artifact, spec)
  local env = git_utils.snapshot_git_env(spec.env, artifact.git, spec.cwd)
  env.GIT_INDEX_FILE = maki.fs.joinpath(artifact.dir, CHECK_INDEX)
  local changed, err
  for _, args in ipairs({
    { "read-tree", artifact.base },
    { "update-index", "-q", "--refresh" },
    { "diff-files", "--name-only" },
  }) do
    changed, err = call:run_quick(git_utils.git(args), { env = env, cwd = spec.cwd, timeout_ms = artifact.timeout_ms })
    if not changed then
      break
    end
  end
  maki.fs.rm(env.GIT_INDEX_FILE, { force = true })
  if not changed then
    return "maki cannot compare the snapshot with the checkout: " .. err
  end
  local paths = git_utils.listed_paths(changed)
  if #paths > 0 then
    return "the checkout changed during the snapshot: "
      .. M.shown(paths)
      .. ". Try again after these edits are complete."
  end
  return nil
end

-- Dependencies are too large to compare file by file, so maki compares
-- change times with {marker}, written just before the copy.
local function unsettled_dependencies(call, artifact, spec, dependencies, marker)
  local changed = {}
  for _, dir in ipairs(dependencies) do
    local out, err = call:run_quick(
      { "sh", "-c", CHANGED_SINCE, "sh", dir, marker },
      { env = spec.env, cwd = spec.cwd, timeout_ms = artifact.timeout_ms }
    )
    if not out then
      return nil, "maki cannot examine the dependency " .. dir .. ": " .. err
    end
    for path in out:gmatch("[^\n]+") do
      changed[#changed + 1] = path:gsub("^%./", "")
    end
  end
  return changed
end

--- {spec} has `cwd` (resolved), `env`, `deny_read`, `include` and
--- `dependencies`. A dependency that changed during the copy sets
--- `artifact.unsettled`.
function M.fill(call, artifact, spec)
  spec.excluded_pathspecs = git_utils.excluded_pathspecs(spec.deny_read)
  artifact.excluded_pathspecs = git_utils.collection_pathspecs(spec.dependencies)
  local paths, paths_err, tracked = snapshot_paths(call, artifact, spec)
  if not paths then
    return paths_err
  end
  local marker = maki.fs.joinpath(artifact.dir, COPY_STARTED)
  local _, marker_err = maki.fs.write(marker, "")
  if marker_err then
    return "maki cannot record the start of the copy: " .. marker_err
  end
  local _, copy_err = copy_into(call, artifact, spec, paths)
  if copy_err then
    return "maki cannot copy the project: " .. copy_err
  end
  local dependencies, deps_err = copy_dependencies(call, artifact, spec, tracked)
  if not dependencies then
    return deps_err
  end
  local unsettled, unsettled_err = unsettled_dependencies(call, artifact, spec, dependencies, marker)
  maki.fs.rm(marker, { force = true })
  if not unsettled then
    return unsettled_err
  end
  if #unsettled > 0 then
    artifact.unsettled = M.shown(unsettled)
    artifact.notes[#artifact.notes + 1] = "dependencies changed during the copy, so they may mix two versions: "
      .. artifact.unsettled
  end
  local dropped, links_err = drop_escaping_links(call, artifact, spec.env)
  if not dropped then
    return links_err
  end
  local kept = {}
  for _, path in ipairs(paths) do
    if not dropped[path] then
      kept[#kept + 1] = path
    end
  end
  return commit_base(call, artifact, kept, spec.dependencies) or consistency_problem(call, artifact, spec)
end

local function remove_artifact(call, dir, released)
  local function finished(_, code)
    if code ~= 0 then
      call:leave(dir)
    end
    if released then
      released()
    end
  end
  if not pcall(call.start, call, { "sh", "-c", SWEEP_CONTENTS, "sh", dir, OWNER_MARKER }, {}, finished) then
    finished()
  end
end

local function discard_failed_allocation(call, dir)
  maki.async.await(1, function(done)
    remove_artifact(call, dir, function()
      done(true)
    end)
  end)
end

--- An import rewrites the manifest, so the sweep keeps the artifact of an
--- unfinished import. An artifact that stopped before the collect step is
--- dated by its marker.
function M.sweep(call, root, ttl_hours)
  -- Artifact locks are unlinked during deletion. Keep this lock outside the deleted tree.
  local sweep_lock, lock_err = maki.fs.try_lock(maki.fs.joinpath(root, SWEEP_LOCK))
  if not sweep_lock then
    if lock_err == native.lock_busy then
      return
    end
    local meta, meta_err = maki.fs.metadata(root)
    if meta or meta_err then
      maki.notify(M.CANNOT_SWEEP .. root .. ": " .. lock_err, "warn")
    end
    return
  end
  local entries = maki.fs.dir(root)
  local cutoff = os.time() - ttl_hours * SECS_PER_HOUR
  for _, entry in ipairs(entries or {}) do
    local name, kind = entry[1], entry[2]
    local dir = maki.fs.joinpath(root, name)
    local marker = kind == "directory" and maki.fs.metadata(maki.fs.joinpath(dir, OWNER_MARKER))
    local meta = marker and (maki.fs.metadata(maki.fs.joinpath(dir, path_utils.MANIFEST)) or marker)
    if meta and meta.mtime and meta.mtime < cutoff then
      local lock = maki.fs.try_lock(maki.fs.joinpath(dir, path_utils.IMPORT_LOCK))
      if lock then
        local current = maki.fs.metadata(maki.fs.joinpath(dir, path_utils.MANIFEST)) or marker
        if current.mtime and current.mtime < cutoff then
          local trash = maki.fs.joinpath(root, TRASH_PREFIX .. name)
          local already_trash = name:sub(1, #TRASH_PREFIX) == TRASH_PREFIX
          local renamed = already_trash or maki.fs.rename(dir, trash)
          if already_trash then
            trash = dir
          end
          if renamed then
            remove_artifact(call, trash, function()
              lock:close()
            end)
          else
            call:leave(dir)
            lock:close()
          end
        else
          lock:close()
        end
      end
    end
  end
  call:when_idle(function()
    sweep_lock:close()
  end)
end

--- Returns {root} resolved on disk, even when it is missing. It must lie
--- outside the checkout, so maki creates or removes nothing there until this
--- function accepts it.
function M.resolve_root(root, spec)
  local existing, missing = root, {}
  while true do
    local meta, meta_err = maki.fs.metadata(existing)
    if meta then
      break
    elseif meta_err then
      return nil, "maki cannot read " .. existing .. ": " .. meta_err
    end
    -- A link to a missing target looks missing, and a folder made through
    -- it lands at the link's target.
    local name, parent = maki.fs.basename(existing), maki.fs.dirname(existing)
    for _, entry in ipairs(maki.fs.dir(parent) or {}) do
      if entry[1] == name then
        return nil, existing .. LINK_TO_NOWHERE
      end
    end
    table.insert(missing, 1, name)
    existing = parent
  end
  local resolved, err = maki.fs.realpath(existing)
  if not resolved then
    return nil, err
  end
  resolved = maki.fs.joinpath(resolved, unpack(missing))
  if launch.in_checkout(resolved, spec) then
    return nil, "the artifact directory " .. resolved .. " is in the project"
  end
  return resolved
end

--- `tmp` is the only other place the worker's shell can write.
function M.allocate(call, root, env, timeout_ms)
  local _, root_err = maki.fs.mkdir(root, { parents = true })
  if root_err then
    return nil, "maki cannot make " .. root .. ": " .. root_err
  end
  local dir, dir_err = maki.fs.mkdtemp(maki.fs.joinpath(root, ARTIFACT_TEMPLATE))
  if not dir then
    return nil, dir_err
  end
  local _, mark_err = maki.fs.write(maki.fs.joinpath(dir, OWNER_MARKER), "")
  if mark_err then
    discard_failed_allocation(call, dir)
    return nil, "maki cannot mark " .. dir .. " as an artifact: " .. mark_err
  end
  local snapshot = maki.fs.joinpath(dir, path_utils.SNAPSHOT_DIR)
  local git = maki.fs.joinpath(dir, path_utils.GIT_DIR)
  local artifact = {
    id = maki.fs.basename(dir),
    dir = dir,
    snapshot = snapshot,
    git = git,
    tmp = maki.fs.joinpath(dir, TMP_DIR),
    git_env = git_utils.snapshot_git_env(env, git, snapshot),
    timeout_ms = timeout_ms,
    notes = {},
  }
  for _, sub in ipairs({ artifact.snapshot, artifact.tmp }) do
    local _, mkdir_err = maki.fs.mkdir(sub)
    if mkdir_err then
      discard_failed_allocation(call, artifact.dir)
      return nil, "maki cannot make " .. sub .. ": " .. mkdir_err
    end
  end
  return artifact
end

function M.discard(call, artifact, removed)
  remove_artifact(call, artifact.dir, removed)
end

-- Git traverses nested repositories during ordinary status commands. Keep their metadata
-- outside the work tree, without discarding the worker's files or repository objects.
function M.sanitize(artifact)
  local quarantine = maki.fs.joinpath(artifact.dir, QUARANTINE_DIR)
  local paths, err = native.sanitize_git(artifact.snapshot, quarantine, artifact.git)
  if not paths then
    return err
  end
  if #paths.metadata > 0 then
    artifact.notes[#artifact.notes + 1] = "maki preserved the Git metadata for "
      .. M.shown(paths.metadata)
      .. " in "
      .. quarantine
      .. ". Their ordinary files remain in the snapshot."
  end
  if #paths.repositories > 0 then
    artifact.repositories = artifact.repositories or {}
    for _, path in ipairs(paths.repositories) do
      artifact.repositories[path] = true
    end
    artifact.notes[#artifact.notes + 1] = "maki quarantined the embedded repository metadata for "
      .. M.shown(paths.repositories)
      .. " in "
      .. quarantine
      .. ". Apply these changes manually."
  end
  if paths.private_files > 0 then
    artifact.notes[#artifact.notes + 1] = "maki made private copies of "
      .. paths.private_files
      .. " hard-linked files in the snapshot"
  end
end

local function stage_changes(call, artifact)
  local _, update_err = snapshot_git(call, artifact, { "add", "-u" })
  if update_err then
    return nil, update_err
  end
  local args = { "ls-files", "--others", "--exclude-standard", "--", "." }
  for _, pathspec in ipairs(artifact.excluded_pathspecs) do
    args[#args + 1] = pathspec
  end
  local listed, list_err = snapshot_git(call, artifact, args)
  if not listed then
    return nil, list_err
  end
  local add_err = index_files(call, artifact, git_utils.listed_paths(listed))
  if add_err then
    return nil, add_err
  end
  -- After staging, only gitignored paths remain untracked. Exclude maki's
  -- dependency paths from this list without also hiding the .gitignore paths.
  local ignored, ignored_err = snapshot_git(call, artifact, {
    "ls-files",
    "--others",
    "--directory",
    "--no-empty-directory",
    "--exclude-from=" .. maki.fs.joinpath(artifact.git, "info", "exclude"),
  })
  if not ignored then
    return nil, ignored_err
  end
  return git_utils.listed_paths(ignored)
end

-- Preparation changes become the worker baseline. The checkout lacks these
-- changes, so files the worker changes again cannot be imported.
local function commit_prepared(call, artifact)
  local metadata_err = M.sanitize(artifact)
  if metadata_err then
    return metadata_err
  end
  local ignored, add_err = stage_changes(call, artifact)
  if not ignored then
    return add_err
  end
  artifact.prepared_ignored = path_utils.set_of(ignored)
  local prepared, prepared_err = commit_snapshot(call, artifact, "maki prepare")
  if not prepared then
    return prepared_err
  end
  local changed, changed_err =
    snapshot_git(call, artifact, { "diff-tree", "-r", "--name-only", "--no-renames", artifact.base, prepared })
  if not changed then
    return changed_err
  end
  local paths = git_utils.listed_paths(changed)
  if #paths > 0 then
    artifact.notes[#artifact.notes + 1] = PREPARED:format(M.shown(paths))
  end
  artifact.base = prepared
end

--- `prepare` runs as the user, outside the sandbox, with local Git variables
--- unset and the artifact's `TMPDIR`. maki removes external symlinks and makes
--- hard-linked files private before the worker starts.
--- Records a command error and returns any fatal sanitation or baseline error.
function M.prepare(call, artifact, command, env, timeout_ms)
  local _, err = call:run_quick({ "sh", "-c", git_utils.without_local_git_env(command) }, {
    display = command,
    env = { TMPDIR = env.TMPDIR },
    inherit_env = true,
    cwd = artifact.snapshot,
    timeout_ms = timeout_ms,
  })
  artifact.prepare_err = err
  local dropped, links_err = drop_escaping_links(call, artifact, env)
  if not dropped then
    return links_err
  end
  return commit_prepared(call, artifact)
end

-- Sanitation moves metadata before restoring the trusted root pointer, so a worker link
-- cannot redirect the write.
function M.collect(call, artifact, project)
  local metadata_err = M.sanitize(artifact)
  if metadata_err then
    return nil, metadata_err
  end
  local ignored, add_err = stage_changes(call, artifact)
  if not ignored then
    return nil, add_err
  end
  local unrecorded = {}
  for _, path in ipairs(ignored) do
    if not (artifact.prepared_ignored or {})[path] then
      unrecorded[#unrecorded + 1] = path
    end
  end
  if #unrecorded > 0 then
    artifact.notes[#artifact.notes + 1] = "gitignored output stays in the snapshot and is outside the import: "
      .. M.shown(unrecorded)
  end
  local tree, tree_err = snapshot_git(call, artifact, { "write-tree" })
  if not tree then
    return nil, tree_err
  end
  artifact.tree = tree
  local raw, raw_err =
    snapshot_git(call, artifact, { "diff-tree", "-r", "--raw", "--numstat", "--no-renames", artifact.base, tree })
  if not raw then
    return nil, raw_err
  end
  -- JSON holds only UTF-8, so the manifest cannot record the other names.
  local changes, unnamed = {}, {}
  for _, change in ipairs(git_utils.parse_raw_diff(raw)) do
    if utf8.len(change.path) then
      changes[#changes + 1] = change
    else
      unnamed[#unnamed + 1] = change.path
    end
  end
  local binary = git_utils.binary_paths(raw)
  local retyped = git_utils.retyped_paths(changes)
  for _, change in ipairs(changes) do
    change.kind = workspace.repository_path(change.path, artifact.repositories) and workspace.REPOSITORY_KIND
      or retyped[change.path] and workspace.TYPE_KIND
      or workspace.kind(change, binary)
  end
  if #unnamed > 0 then
    artifact.notes[#artifact.notes + 1] = NOT_UTF8_CHANGES:format(M.shown(unnamed), artifact.snapshot)
  end
  local manifest = {
    version = workspace.MANIFEST_VERSION,
    project = project,
    base = artifact.base,
    tree = tree,
    changes = changes,
    imported = {},
  }
  local _, save_err =
    maki.fs.atomic_write(maki.fs.joinpath(artifact.dir, path_utils.MANIFEST), maki.json.encode(manifest))
  if save_err then
    return nil, save_err
  end
  return changes
end

return M
