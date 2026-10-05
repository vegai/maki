-- The private snapshot a coding worker runs in, and the import of its
-- changes. Each step runs as a job of the call, so a cancel or a time limit
-- stops it.

local launch = require("claude_launch")
local workspace = require("claude_workspace")

local M = {}

local ARTIFACT_TEMPLATE = "XXXXXXXX"
local LINK_TO_NOWHERE = " is a link to a missing target"
local SNAPSHOT_DIR = "snapshot"
local TMP_DIR = "tmp"
local GIT_DIR = "git"
local DOT_GIT = ".git"
local MANIFEST = "manifest.json"
local CHECK_INDEX = "consistency-index"
local COPY_STARTED = "copy-started"
-- The artifact directory can be a folder that holds other things, so the
-- sweep removes only folders with this marker.
local OWNER_MARKER = ".maki-claude-code-artifact"
local MANIFEST_VERSION = 1
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
local CURRENT_DIR = "./"
-- Stands in for the script in a denied import's error.
local IMPORT_COMMAND = "the import command"
local LEFTOVERS_REMAIN = "maki could not remove its temporary files, so files starting with %s may remain "
  .. "next to the files it wrote."
local NOT_UTF8_CHANGES = "the names of %s are not UTF-8, so the manifest cannot hold these changes. "
  .. "Apply them manually from %s"
local SUBMODULES_LEFT_OUT = "the snapshot has no submodules, so the worker did not see "
local PREPARED = "the prepare command changed %s, and the changes for import do not include these files"
local UNRECORDED = "maki could not record this in the artifact (%s), so a second import will show these files as "
  .. "changed."
-- Removes each link under $1 that points outside $1 and prints its path in
-- hex, so maki gets every byte of the name. The shell does the removal, so
-- a link whose name maki cannot read goes too.
local DROP_ESCAPING_LINKS = [[
root=$1
shift
for link do
  target=$(realpath -m -- "$link" 2>/dev/null) || target=
  case $target in
  "$root" | "$root"/*) ;;
  *) rm -f -- "$link" && { printf '%s' "${link#"$root"/}" | od -An -v -tx1 | tr -d ' \n'; echo; } ;;
  esac
done
]]

-- Bytes that could break the job reader become `?`. The `./` keeps `find`
-- from reading a directory named like `-delete` as an expression.
local CHANGED_SINCE = [[find "./$1" -cnewer "$2" -print | LC_ALL=C tr -c '\n[:print:]' '?']]

local function shown(paths)
  local listed, more = workspace.first(paths, SHOWN_PATHS)
  for i, path in ipairs(listed) do
    listed[i] = workspace.printable(path)
  end
  return table.concat(listed, ", ") .. (more > 0 and " and " .. more .. " more" or "")
end

local function from_hex(line)
  return (line:gsub("%x%x", function(pair)
    return string.char(tonumber(pair, 16))
  end))
end

local function checkout_git(call, spec, args, stdin)
  return call:run_quick(workspace.git(args), { env = spec.env, cwd = spec.cwd, stdin = stdin })
end

local function list_files(call, spec, mode, pathspecs)
  local args = { "ls-files", mode, "--" }
  for _, pathspec in ipairs(pathspecs) do
    args[#args + 1] = pathspec
  end
  for _, pathspec in ipairs(workspace.excluded_pathspecs(spec.deny_read)) do
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
    workspace.git(args),
    { env = artifact.git_env, cwd = artifact.snapshot, stdin = stdin, timeout_ms = artifact.timeout_ms }
  )
end

local function disk_blobs(call, env, cwd, paths, keep_on_cancel)
  if #paths == 0 then
    return {}
  end
  local out, err = call:run_quick(
    workspace.git({ "hash-object", "--no-filters", "--stdin-paths" }),
    { env = env, cwd = cwd, stdin = workspace.quoted_lines(paths), keep_on_cancel = keep_on_cancel }
  )
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
  for _, path in ipairs(workspace.listed_paths(deleted)) do
    seen[path] = true
  end
  local paths = {}
  local function take(path)
    if not seen[path] then
      seen[path] = true
      paths[#paths + 1] = path
    end
  end
  local files, submodules = workspace.tracked_paths(tracked)
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
    for _, path in ipairs(workspace.listed_paths(untracked)) do
      local dependency = false
      for _, dir in ipairs(spec.dependencies) do
        dependency = dependency or path:sub(1, #dir + 1) == dir .. "/"
      end
      if not dependency then
        take(path)
      end
    end
  end
  local unusual = workspace.first_non_utf8(paths)
  if unusual then
    return nil,
      "the file name " .. workspace.printable(unusual) .. " is not UTF-8, and maki cannot give it to a process"
  end
  if #submodules > 0 then
    artifact.notes[#artifact.notes + 1] = SUBMODULES_LEFT_OUT .. shown(submodules)
  end
  return paths, nil, files
end

-- The project's exclusions apply, because the worker's shell can read the
-- whole copy.
local function copy_dependencies(call, artifact, spec, paths)
  local copied = {}
  for _, dir in ipairs(spec.dependencies) do
    local source = maki.fs.joinpath(spec.cwd, dir)
    local target = maki.fs.joinpath(artifact.snapshot, dir)
    local meta = maki.fs.metadata(source)
    local holds_tracked = false
    for _, path in ipairs(paths) do
      holds_tracked = holds_tracked or path:sub(1, #dir + 1) == dir .. "/"
    end
    if not (meta and meta.is_dir) then
      artifact.notes[#artifact.notes + 1] = "the dependency "
        .. dir
        .. " is not a directory here, so maki did not copy it"
    elseif holds_tracked then
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
      -- git lists a nested repository as the directory itself.
      local files, nested = {}, {}
      for _, path in ipairs(workspace.listed_paths(listed)) do
        local list = path:sub(-1) == "/" and nested or files
        list[#list + 1] = path
      end
      local unusual = workspace.first_non_utf8(files)
      if unusual then
        return nil,
          "the file name " .. workspace.printable(unusual) .. " is not UTF-8, and maki cannot give it to a process"
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
          .. shown(nested)
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
    artifact.notes[#artifact.notes + 1] = "maki did not copy the links that went out of the snapshot: " .. shown(names)
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
  local exclude = { workspace.UNCOLLECTED_RULE }
  for _, dir in ipairs(dependencies) do
    exclude[#exclude + 1] = workspace.exclude_rule(dir)
  end
  local _, mkdir_err = maki.fs.mkdir(info)
  local _, attributes_err = maki.fs.write(maki.fs.joinpath(info, "attributes"), workspace.RAW_ATTRIBUTES)
  local _, exclude_err = maki.fs.write(maki.fs.joinpath(info, "exclude"), table.concat(exclude))
  local _, link_err = maki.fs.write(maki.fs.joinpath(artifact.snapshot, DOT_GIT), "gitdir: " .. artifact.git .. "\n")
  local setup_err = mkdir_err or attributes_err or exclude_err or link_err
  if setup_err then
    return "maki cannot make the repository of the snapshot: " .. setup_err
  end
  if #paths > 0 then
    local _, add_err = call:run_quick(
      workspace.git({ "--literal-pathspecs", "add", "-f", "--pathspec-from-file=-", "--pathspec-file-nul" }),
      {
        env = artifact.git_env,
        cwd = artifact.snapshot,
        stdin = table.concat(paths, "\0"),
        timeout_ms = artifact.timeout_ms,
      }
    )
    if add_err then
      return add_err
    end
  end
  local _, commit_err = snapshot_git(call, artifact, { "commit", "-q", "--allow-empty", "-m", "maki snapshot" })
  if commit_err then
    return commit_err
  end
  local base, base_err = snapshot_git(call, artifact, { "rev-parse", "HEAD" })
  artifact.base = base
  return base_err
end

-- Diffs the base against the checkout as its work tree, which shows every
-- write during the copy. A temporary index leaves the snapshot's index
-- alone.
local function consistency_problem(call, artifact, spec)
  local env = workspace.snapshot_git_env(spec.env, artifact.git, spec.cwd)
  env.GIT_INDEX_FILE = maki.fs.joinpath(artifact.dir, CHECK_INDEX)
  local changed, err
  for _, args in ipairs({
    { "read-tree", artifact.base },
    { "update-index", "-q", "--refresh" },
    { "diff-files", "--name-only" },
  }) do
    changed, err = call:run_quick(workspace.git(args), { env = env, cwd = spec.cwd, timeout_ms = artifact.timeout_ms })
    if not changed then
      break
    end
  end
  maki.fs.rm(env.GIT_INDEX_FILE, { force = true })
  if not changed then
    return "maki cannot compare the snapshot with the checkout: " .. err
  end
  local paths = workspace.listed_paths(changed)
  if #paths > 0 then
    return "the checkout changed during the snapshot: " .. shown(paths) .. ". Try again after these edits are complete."
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
      changed[#changed + 1] = path:sub(#CURRENT_DIR + 1)
    end
  end
  return changed
end

--- {spec} has `cwd` (resolved), `env`, `deny_read`, `include` and
--- `dependencies`. A dependency that changed during the copy sets
--- `artifact.unsettled`.
function M.fill(call, artifact, spec)
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
    artifact.unsettled = shown(unsettled)
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

--- An import rewrites the manifest, so the sweep keeps the artifact of an
--- unfinished import. An artifact that stopped before the collect step is
--- dated by its marker.
function M.sweep(root, ttl_hours)
  local entries = maki.fs.dir(root)
  local cutoff = os.time() - ttl_hours * SECS_PER_HOUR
  for _, entry in ipairs(entries or {}) do
    local name, kind = entry[1], entry[2]
    local dir = maki.fs.joinpath(root, name)
    local marker = kind == "directory" and maki.fs.metadata(maki.fs.joinpath(dir, OWNER_MARKER))
    local meta = marker and (maki.fs.metadata(maki.fs.joinpath(dir, MANIFEST)) or marker)
    if meta and meta.mtime and meta.mtime < cutoff then
      -- In the background, because a large tree takes long and the call
      -- does not need it gone. The next sweep finishes a stopped one.
      maki.fn.jobstart({ "rm", "-rf", "--", dir }, { scope = "plugin" })
    end
  end
end

local function resolved_path(call, path, env)
  return call:run_quick({ "pwd", "-P" }, { env = env, cwd = path })
end

--- Returns {root} resolved on disk, even when it is missing. It must lie
--- outside the checkout, so maki creates or removes nothing there until this
--- function accepts it.
function M.resolve_root(call, root, spec)
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
  local resolved, err = resolved_path(call, existing, spec.env)
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
  local dir, dir_err = call:run_quick({ "mktemp", "-d", maki.fs.joinpath(root, ARTIFACT_TEMPLATE) }, { env = env })
  if not dir then
    return nil, dir_err
  end
  local _, mark_err = maki.fs.write(maki.fs.joinpath(dir, OWNER_MARKER), "")
  if mark_err then
    maki.fs.rm(dir, { recursive = true, force = true })
    return nil, "maki cannot mark " .. dir .. " as an artifact: " .. mark_err
  end
  local snapshot = maki.fs.joinpath(dir, SNAPSHOT_DIR)
  local git = maki.fs.joinpath(dir, GIT_DIR)
  local artifact = {
    id = maki.fs.basename(dir),
    dir = dir,
    snapshot = snapshot,
    git = git,
    tmp = maki.fs.joinpath(dir, TMP_DIR),
    git_env = workspace.snapshot_git_env(env, git, snapshot),
    timeout_ms = timeout_ms,
    notes = {},
  }
  for _, sub in ipairs({ artifact.snapshot, artifact.tmp }) do
    local _, mkdir_err = maki.fs.mkdir(sub)
    if mkdir_err then
      M.discard(artifact)
      return nil, "maki cannot make " .. sub .. ": " .. mkdir_err
    end
  end
  return artifact
end

function M.discard(artifact)
  maki.fs.rm(artifact.dir, { recursive = true, force = true })
end

-- What `prepare` wrote becomes the base for the worker's changes, so it is
-- never offered for import, and a note lists it. A file that the worker
-- also changed cannot be imported, because the checkout lacks the `prepare`
-- changes underneath.
local function commit_prepared(call, artifact)
  local _, add_err = snapshot_git(call, artifact, { "add", "-A" })
  if add_err then
    return add_err
  end
  local _, commit_err = snapshot_git(call, artifact, { "commit", "-q", "--allow-empty", "-m", "maki prepare" })
  if commit_err then
    return commit_err
  end
  local prepared, prepared_err = snapshot_git(call, artifact, { "rev-parse", "HEAD" })
  if not prepared then
    return prepared_err
  end
  local changed, changed_err =
    snapshot_git(call, artifact, { "diff-tree", "-r", "--name-only", "--no-renames", artifact.base, prepared })
  if not changed then
    return changed_err
  end
  local paths = workspace.listed_paths(changed)
  if #paths > 0 then
    artifact.notes[#artifact.notes + 1] = PREPARED:format(shown(paths))
  end
  artifact.base = prepared
end

--- `prepare` runs as the user, outside the sandbox, with maki's environment
--- and the artifact's `TMPDIR`. maki removes any links it made out of the
--- snapshot.
--- Returns the command error, then any fatal sanitation or baseline error.
function M.prepare(call, artifact, command, env, timeout_ms)
  local _, err = call:run_quick(
    { "sh", "-c", command },
    { env = { TMPDIR = env.TMPDIR }, inherit_env = true, cwd = artifact.snapshot, timeout_ms = timeout_ms }
  )
  local dropped, links_err = drop_escaping_links(call, artifact, env)
  if not dropped then
    return err, links_err
  end
  return err, commit_prepared(call, artifact)
end

--- Writes nothing inside the snapshot, because the worker controls it and a
--- link there could redirect the write.
function M.collect(call, artifact, project)
  local _, add_err = snapshot_git(call, artifact, { "add", "-A" })
  if add_err then
    return nil, add_err
  end
  local tree, tree_err = snapshot_git(call, artifact, { "write-tree" })
  if not tree then
    return nil, tree_err
  end
  local raw, raw_err = snapshot_git(call, artifact, { "diff-tree", "-r", "--raw", "--no-renames", artifact.base, tree })
  if not raw then
    return nil, raw_err
  end
  local numstat, numstat_err =
    snapshot_git(call, artifact, { "diff-tree", "-r", "--numstat", "--no-renames", artifact.base, tree })
  if not numstat then
    return nil, numstat_err
  end
  -- JSON holds only UTF-8, so the manifest cannot record the other names.
  local changes, unnamed = {}, {}
  for _, change in ipairs(workspace.parse_raw_diff(raw)) do
    if utf8.len(change.path) then
      changes[#changes + 1] = change
    else
      unnamed[#unnamed + 1] = change.path
    end
  end
  local binary = workspace.binary_paths(numstat)
  local retyped = workspace.retyped_paths(changes)
  for _, change in ipairs(changes) do
    change.kind = retyped[change.path] and workspace.TYPE_KIND or workspace.kind(change, binary)
  end
  if #unnamed > 0 then
    artifact.notes[#artifact.notes + 1] = NOT_UTF8_CHANGES:format(shown(unnamed), artifact.snapshot)
  end
  local manifest = {
    version = MANIFEST_VERSION,
    project = project,
    base = artifact.base,
    tree = tree,
    changes = changes,
    imported = {},
  }
  local _, save_err = maki.fs.atomic_write(maki.fs.joinpath(artifact.dir, MANIFEST), maki.json.encode(manifest))
  if save_err then
    return nil, save_err
  end
  return changes
end

-- git records the owner's execute bit, whoever runs maki.
local function owner_executes(call, env, cwd, paths, keep_on_cancel)
  if #paths == 0 then
    return {}
  end
  local out, err = call:run_quick(
    { "xargs", "-0", "stat", "-c", "%A", "--" },
    { env = env, cwd = cwd, stdin = table.concat(paths, "\0"), keep_on_cancel = keep_on_cancel }
  )
  if not out then
    return nil, err
  end
  local executes = {}
  for i, mode in ipairs(maki.split(out, "\n")) do
    executes[i] = workspace.owner_executes(mode)
  end
  return executes
end

-- Maps each path to its `file_state`, false when missing, or a note when it
-- is not a regular file. Hashes use the object format of {artifact}'s
-- repository, which can differ from the checkout's.
local function checkout_state(call, env, artifact, changes, keep_on_cancel)
  local project = artifact.manifest.project
  local git_env = workspace.snapshot_git_env(env, maki.fs.joinpath(artifact.dir, GIT_DIR), project)
  local state, files = {}, {}
  for _, change in ipairs(changes) do
    local meta, meta_err = maki.fs.metadata(maki.fs.joinpath(project, change.path))
    if meta_err then
      return nil, meta_err
    elseif not meta then
      state[change.path] = false
    elseif meta.is_file then
      files[#files + 1] = change.path
    else
      state[change.path] = "not a file"
    end
  end
  local blobs, err = disk_blobs(call, git_env, project, files, keep_on_cancel)
  if not blobs then
    return nil, err
  end
  local executable, executable_err = owner_executes(call, env, project, files, keep_on_cancel)
  if not executable then
    return nil, executable_err
  end
  for i, path in ipairs(files) do
    state[path] = workspace.file_state(blobs[i], executable[i])
  end
  return state
end

--- Manifest fields end up in the command the user approves, so the manifest
--- must come from a collect step for {spec.project}. Opening it rewrites it,
--- because the sweep goes by its date and the approval can outlast the
--- artifact's time limit.
local function open_import(spec)
  if type(spec.id) ~= "string" or not spec.id:match(workspace.ARTIFACT_ID) then
    return nil, "an artifact id contains only the characters A to Z, a to z and 0 to 9"
  end
  local dir = maki.fs.joinpath(spec.root, spec.id)
  local manifest_path = maki.fs.joinpath(dir, MANIFEST)
  local text = maki.fs.read(manifest_path)
  local manifest = text and maki.json.decode(text)
  if type(manifest) ~= "table" or manifest.version ~= MANIFEST_VERSION or type(manifest.changes) ~= "table" then
    return nil, "there is no change artifact " .. spec.id .. ". If it was there before, the sweep removed it."
  end
  if manifest.project ~= spec.project then
    return nil,
      "artifact "
        .. spec.id
        .. " contains changes for "
        .. tostring(manifest.project)
        .. ", and not for "
        .. spec.project
  end
  for _, change in ipairs(manifest.changes) do
    local problem = workspace.change_problem(change)
    if problem then
      return nil, "artifact " .. spec.id .. " has a manifest that maki did not write: " .. problem
    end
  end
  local _, touch_err = maki.fs.atomic_write(manifest_path, text)
  if touch_err then
    return nil, "maki cannot stop the expiry of artifact " .. spec.id .. " during the import: " .. touch_err
  end
  manifest.imported = type(manifest.imported) == "table" and manifest.imported or {}
  return { dir = dir, manifest_path = manifest_path, manifest = manifest }
end

--- Returns the changes to import, and for every other change the reason it
--- is skipped. The default is all changes. A path may also be spelled as the
--- report shows it, with control bytes escaped.
local function select_changes(manifest, paths)
  local by_path = {}
  for _, change in ipairs(manifest.changes) do
    by_path[workspace.printable(change.path)] = change
  end
  for _, change in ipairs(manifest.changes) do
    by_path[change.path] = change
  end
  local selected = manifest.changes
  if paths then
    selected = {}
    local named = {}
    for _, path in ipairs(paths) do
      local change = by_path[path]
      if not change then
        return nil, workspace.printable(path) .. " is not a change of the artifact"
      end
      if not named[change] then
        named[change] = true
        selected[#selected + 1] = change
      end
    end
  end
  local todo, skipped = {}, {}
  for _, change in ipairs(selected) do
    local shown_path = workspace.printable(change.path)
    if manifest.imported[change.path] then
      skipped[#skipped + 1] = shown_path .. ": imported before"
    elseif not workspace.importable(change) then
      skipped[#skipped + 1] = shown_path .. ": a " .. change.kind .. " change. Apply it manually"
    else
      todo[#todo + 1] = change
    end
  end
  return todo, skipped
end

--- The user can edit the snapshot after collection. Import only artifact objects whose bytes
--- match their ids.
local function check_import(call, env, artifact, todo)
  local current, current_err = checkout_state(call, env, artifact, todo)
  if not current then
    return "maki cannot examine the checkout: " .. current_err
  end
  local conflicts = workspace.conflicts(todo, current)
  if #conflicts > 0 then
    return "maki imported no changes: these files changed in the checkout after the snapshot: "
      .. shown(conflicts)
      .. ". Merge them manually from "
      .. maki.fs.joinpath(artifact.dir, SNAPSHOT_DIR)
      .. ", or import the remaining changes with `paths`."
  end
end

--- Deepest folders first.
local function missing_folders(project, changes)
  local missing, seen = {}, {}
  for _, change in ipairs(changes) do
    if change.status == workspace.ADDED then
      for slash in change.path:gmatch("()/") do
        local folder = maki.fs.joinpath(project, change.path:sub(1, slash - 1))
        local meta, meta_err = maki.fs.metadata(folder)
        if not seen[folder] and not meta and not meta_err then
          seen[folder] = true
          missing[#missing + 1] = folder
        end
      end
    end
  end
  table.sort(missing, function(a, b)
    return #a > #b
  end)
  return missing
end

--- Returns the changes the import applied, and the error if it stopped. The
--- command can stop anywhere between approval and the last rename, so maki
--- reads the checkout to see what landed.
local function apply_import(ctx, call, env, artifact, todo)
  local project = artifact.manifest.project
  local made = missing_folders(project, todo)
  local stage, stage_err = call:run_quick(
    { "mktemp", "-d", maki.fs.joinpath(artifact.dir, workspace.STAGE_TEMPLATE) },
    { env = env }
  )
  if not stage then
    return nil, "maki cannot make a stage folder in the artifact: " .. stage_err
  end
  local script = workspace.import_script(todo, project, maki.fs.joinpath(artifact.dir, GIT_DIR), artifact.dir, stage)
  local _, err = maki.agent.call_tool(ctx, "bash", {
    command = script,
    workdir = project,
    timeout = workspace.import_timeout(todo),
    description = "Import " .. #todo .. " changes from Claude Code",
  })
  if not err then
    return todo
  end
  -- This also runs after a cancel, so the cleanup and the read must survive
  -- it.
  local ran, code = pcall(
    call.run_to_end,
    call,
    { "bash", "-c", workspace.leftovers_script(todo, project, stage, made) },
    { env = env, clear_env = true, cwd = project },
    { keep_on_cancel = true },
    call.startup_ms
  )
  local first, last = err:find(script, 1, true)
  if first then
    err = err:sub(1, first - 1) .. IMPORT_COMMAND .. err:sub(last + 1)
  end
  if not ran or code ~= 0 then
    err = err .. "\n" .. LEFTOVERS_REMAIN:format(workspace.temp_name(stage))
  end
  local now, now_err = checkout_state(call, env, artifact, todo, true)
  if not now then
    local paths = {}
    for i, change in ipairs(todo) do
      paths[i] = change.path
    end
    return nil, err .. "\nmaki cannot read the checkout (" .. now_err .. "). Examine " .. shown(paths) .. "."
  end
  return workspace.landed(todo, now), err
end

--- One bash command, which checks every file again when it runs because the
--- approval can take long. If a file it changes also changed in the checkout
--- since the snapshot, or in the artifact since the collect step, nothing
--- applies.
function M.import(ctx, call, spec)
  local artifact, open_err = open_import(spec)
  if not artifact then
    return nil, open_err
  end
  local todo, skipped = select_changes(artifact.manifest, spec.paths)
  if not todo then
    return nil, skipped
  end
  local problem = check_import(call, spec.env, artifact, todo)
  if problem then
    return nil, problem
  end
  if #todo == 0 then
    return { applied = {}, skipped = skipped }
  end
  local landed, failure = apply_import(ctx, call, spec.env, artifact, todo)
  if not landed then
    return nil, failure
  end
  local replaced = false
  for _, change in ipairs(landed) do
    artifact.manifest.imported[change.path] = true
    replaced = replaced or change.status ~= workspace.ADDED
  end
  local _, record_err = maki.fs.atomic_write(artifact.manifest_path, maki.json.encode(artifact.manifest))
  local unrecorded = record_err and UNRECORDED:format(record_err) or nil
  local originals = replaced and maki.fs.joinpath(artifact.dir, workspace.IMPORT_ORIGINALS) or nil
  local displaced = maki.fs.joinpath(artifact.dir, workspace.IMPORT_DISPLACED)
  if #landed == #todo then
    return {
      applied = workspace.summary(todo),
      skipped = skipped,
      originals = originals,
      displaced = replaced and displaced or nil,
      unrecorded = unrecorded,
      -- The command can still fail after its last write, and its error and
      -- any leftovers belong in the reply.
      failure = failure,
    }
  elseif #landed == 0 then
    return nil, "maki imported no changes: " .. failure .. "\nExamine any preserved files in " .. artifact.dir .. "."
  end
  local applied, missed = {}, {}
  for _, change in ipairs(todo) do
    local list = artifact.manifest.imported[change.path] and applied or missed
    list[#list + 1] = change.path
  end
  local kept = originals and " The preserved versions are in " .. originals .. " and " .. displaced .. "." or ""
  return nil, M.stopped_partway(applied, missed, kept .. (unrecorded and " " .. unrecorded or ""), failure)
end

--- Reports an import that stopped partway: the paths it {applied} and
--- {missed}, other notes for the user, and why it stopped.
function M.stopped_partway(applied, missed, notes, failure)
  return "the import stopped before it completed. It applied "
    .. shown(applied)
    .. ", and not "
    .. shown(missed)
    .. "."
    .. notes
    .. "\n"
    .. failure
end

return M
