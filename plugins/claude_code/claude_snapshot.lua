local native = require("maki.claude_code.internal")

-- Builds the private snapshot and records the worker's changes.

local jobs = require("claude_jobs")
local workspace = require("claude_workspace")
local git_utils = require("claude_git")
local path_utils = require("claude_paths")

local M = {}

local QUARANTINE_DIR = "git-metadata"

local CHECK_INDEX = "consistency-index"
local COPY_STARTED = "copy-started"
local VENV_MARKER = "pyvenv.cfg"
local C_LOCALE = "C"
-- What `cp --reflink=always` reports when the filesystem cannot clone:
-- EOPNOTSUPP, EXDEV and EINVAL.
local CLONE_UNSUPPORTED = {
  ["Operation not supported"] = true,
  ["Invalid cross-device link"] = true,
  ["Invalid argument"] = true,
}

local NON_UTF8_PATH = "the file name %s is not UTF-8, and maki cannot give it to a process"
local NOT_UTF8_CHANGES = "the names of %s are not UTF-8, so the manifest cannot hold these changes. "
  .. "Apply them manually from %s"
local PREPARED = "the prepare command changed %s, and the changes for import do not include these files"

local INCLUDED_REPOSITORIES = "maki did not copy the repositories under include: %s"
local SUBMODULES = "the snapshot has no submodules, so the worker did not see %s"
local SYMLINKED_DEPENDENCY = "the dependency %s is under the symlink %s, so maki did not copy it"
local MISSING_DEPENDENCY = "the dependency %s is not a directory here, so maki did not copy it"
local TRACKED_DEPENDENCY =
  "the dependency %s contains tracked files, so maki copied only those. List only untracked directories."
local VIRTUAL_ENVIRONMENT =
  "the dependency %s is a Python virtual environment, and its scripts hard-code its path in the checkout, so maki did not copy it. Recreate it with the `prepare` command."
local FULL_COPY = "maki copied the dependency %s as a full copy, because the filesystem cannot clone it: %s"
local DEPENDENCY_REPOSITORIES = "maki did not copy the repositories in the dependency %s: %s"
local UNSETTLED_DEPENDENCIES =
  "dependencies changed during the copy, so they may mix two versions: %s. Mention this for every check that uses them."
local PRESERVED_GIT = "maki preserved the Git metadata for %s in %s. Their ordinary files remain in the snapshot."
local QUARANTINED_REPOSITORIES =
  "maki quarantined the embedded repository metadata for %s in %s. Apply these changes manually."
local PRIVATE_COPIES = "maki made private copies of %s hard-linked files in the snapshot"
local DROPPED_LINKS = "maki removed external or unresolved symlinks from the snapshot: %s"
local IGNORED_OUTPUT = "gitignored output stays in the snapshot and is outside the import: %s"

local function note(artifact, format, ...)
  artifact.notes[#artifact.notes + 1] = format:format(...)
end

local function checkout_git(call, spec, args)
  return call:run_lines(git_utils.git(args), { env = spec.env, cwd = spec.cwd })
end

local function list_files(call, spec, args, pathspecs)
  for _, pathspec in ipairs(pathspecs) do
    args[#args + 1] = pathspec
  end
  for _, pathspec in ipairs(spec.denied_pathspecs) do
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
  return call:run_lines(argv, {
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
  return call:run_lines(
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
  local lines, err = snapshot_git(call, artifact, { "rev-parse", "HEAD" })
  return lines and lines[1], err
end

local function symlinked_parent(cwd, path, checked)
  local parent = cwd
  for part in path:gmatch("([^/]+)/") do
    parent = maki.fs.joinpath(parent, part)
    if checked[parent] == nil then
      local meta = maki.fs.metadata(parent, { follow = false })
      checked[parent] = meta and meta.is_symlink or false
    end
    if checked[parent] then
      return parent
    end
  end
end

local function tracked_parent_problem(cwd, path, checked)
  local parent = symlinked_parent(cwd, path, checked)
  if parent then
    return "the tracked folder "
      .. path_utils.printable(parent)
      .. " is a symlink. Restore a directory before retrying the snapshot."
  end
end

-- Returns the tracked files as they are on disk, plus the untracked files
-- under {spec.include}. Submodules are left out, with a note in {artifact}.
local function snapshot_paths(call, artifact, spec)
  local tracked, tracked_err = list_files(call, spec, { "ls-files", "-t", "-s", "--" }, { "." })
  if not tracked then
    return nil, tracked_err
  end
  local deleted, deleted_err = list_files(
    call,
    spec,
    { "diff-files", "--relative", "--name-only", "--diff-filter=D", "--" },
    { "." }
  )
  if not deleted then
    return nil, deleted_err
  end
  local seen, checked = {}, {}
  for _, path in ipairs(git_utils.listed_paths(deleted)) do
    local problem = tracked_parent_problem(spec.cwd, path, checked)
    if problem then
      return nil, problem
    end
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
    local untracked, untracked_err = list_files(call, spec, { "ls-files", "-o", "--" }, specs)
    if not untracked then
      return nil, untracked_err
    end
    local included, repositories = git_utils.untracked_paths(untracked)
    if #repositories > 0 then
      note(artifact, INCLUDED_REPOSITORIES, path_utils.shown(repositories))
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
    note(artifact, SUBMODULES, path_utils.shown(submodules))
  end
  return paths, nil, files
end

-- The project's exclusions apply, because the worker's shell can read the
-- whole copy.
local function copy_dependencies(call, artifact, spec, paths)
  local copied, tracked_folders, checked = {}, {}, {}
  for _, path in ipairs(paths) do
    tracked_folders[path] = true
    for folder in path_utils.folders(path) do
      tracked_folders[folder] = true
    end
  end
  for _, dir in ipairs(spec.dependencies) do
    local source = maki.fs.joinpath(spec.cwd, dir)
    local target = maki.fs.joinpath(artifact.snapshot, dir)
    local parent = symlinked_parent(spec.cwd, dir, checked)
    local meta = maki.fs.metadata(source, { follow = false })
    if parent then
      note(artifact, SYMLINKED_DEPENDENCY, dir, path_utils.printable(parent))
    elseif not (meta and meta.is_dir) then
      note(artifact, MISSING_DEPENDENCY, dir)
    elseif tracked_folders[dir] then
      note(artifact, TRACKED_DEPENDENCY, dir)
    elseif maki.fs.metadata(maki.fs.joinpath(source, VENV_MARKER)) then
      note(artifact, VIRTUAL_ENVIRONMENT, dir)
    else
      local listed, list_err = list_files(call, spec, { "ls-files", "-o", "--" }, { ":(literal)" .. dir })
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
        note(artifact, FULL_COPY, dir, cause)
      end
      if #nested > 0 then
        note(artifact, DEPENDENCY_REPOSITORIES, dir, path_utils.shown(nested))
      end
      copied[#copied + 1] = dir
    end
  end
  return copied
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
  if call.stopped then
    return jobs.CANCELLED
  end
  local dropped, metadata_err = M.sanitize(artifact)
  local setup_err = mkdir_err or attributes_err or exclude_err or metadata_err
  if setup_err then
    return "maki cannot make the repository of the snapshot: " .. setup_err
  end
  if artifact.repositories or next(dropped) then
    local kept = {}
    for _, path in ipairs(paths) do
      if not dropped[path] and not workspace.repository_path(path, artifact.repositories) then
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
    changed, err = call:run_lines(git_utils.git(args), { env = env, cwd = spec.cwd, timeout_ms = artifact.timeout_ms })
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
    local checked = {}
    for _, path in ipairs(paths) do
      local problem = tracked_parent_problem(spec.cwd, path, checked)
      if problem then
        return problem
      end
    end
    return "the checkout changed during the snapshot: "
      .. path_utils.shown(paths)
      .. ". Try again after these edits are complete."
  end
  return nil
end

-- Dependencies are too large to compare file by file, so maki compares
-- change times with {marker}, written just before the copy.
local function unsettled_dependencies(call, artifact, spec, dependencies, marker)
  if #dependencies == 0 then
    return {}
  end
  -- Prefix paths so dependency names cannot become find expressions.
  local argv = { "find" }
  for _, dir in ipairs(dependencies) do
    argv[#argv + 1] = "./" .. dir
  end
  for _, arg in ipairs({ "-cnewer", marker, "-print" }) do
    argv[#argv + 1] = arg
  end
  local out, err = call:run_lines(argv, { env = spec.env, cwd = spec.cwd, timeout_ms = artifact.timeout_ms })
  if not out then
    return nil, "maki cannot examine the dependencies: " .. err
  end
  local changed = {}
  for _, path in ipairs(out) do
    changed[#changed + 1] = path:gsub("^%./", "")
  end
  return changed
end

--- {spec} has `cwd` (resolved), `env`, `deny_read`, `include` and
--- `dependencies`. Reports dependencies that changed during the copy.
function M.fill(call, artifact, spec)
  spec.denied_pathspecs = git_utils.excluded_pathspecs(spec.deny_read)
  artifact.collection_pathspecs = git_utils.collection_pathspecs(spec.dependencies)
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
    note(artifact, UNSETTLED_DEPENDENCIES, path_utils.shown(unsettled))
  end
  return commit_base(call, artifact, paths, spec.dependencies) or consistency_problem(call, artifact, spec)
end

-- Git traverses nested repositories during ordinary status commands. Keep their metadata
-- outside the work tree, without discarding the worker's files or repository objects.
-- Prune external or unresolved links unless keep_links is set for worker output.
function M.sanitize(artifact, keep_links)
  local quarantine = maki.fs.joinpath(artifact.dir, QUARANTINE_DIR)
  local paths, err = native.sanitize_git(artifact.snapshot, quarantine, artifact.git, keep_links ~= true)
  if not paths then
    return nil, err
  end
  if #paths.git_metadata > 0 then
    note(artifact, PRESERVED_GIT, path_utils.shown(paths.git_metadata), quarantine)
  end
  if #paths.repositories > 0 then
    artifact.repositories = artifact.repositories or {}
    for _, path in ipairs(paths.repositories) do
      artifact.repositories[path] = true
    end
    note(artifact, QUARANTINED_REPOSITORIES, path_utils.shown(paths.repositories), quarantine)
  end
  if paths.private_files > 0 then
    note(artifact, PRIVATE_COPIES, paths.private_files)
  end
  if #paths.dropped_links > 0 then
    note(artifact, DROPPED_LINKS, path_utils.shown(paths.dropped_links))
  end
  return path_utils.set_of(paths.dropped_links)
end

local function stage_changes(call, artifact)
  local _, update_err = snapshot_git(call, artifact, { "add", "-u" })
  if update_err then
    return nil, update_err
  end
  local args = { "ls-files", "--others", "--exclude-standard", "--", "." }
  for _, pathspec in ipairs(artifact.collection_pathspecs) do
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
  if call.stopped then
    return jobs.CANCELLED
  end
  local dropped, metadata_err = M.sanitize(artifact)
  if not dropped then
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
    note(artifact, PREPARED, path_utils.shown(paths))
  end
  artifact.base = prepared
end

--- `prepare` runs as the user, outside the sandbox, with local Git variables
--- unset and the artifact's `TMPDIR`. maki removes external or unresolved symlinks and makes
--- hard-linked files private before the worker starts.
--- Records a command error and returns any fatal sanitation or baseline error.
function M.prepare(call, artifact, command, env, timeout_ms)
  local _, err = call:run_lines({ "sh", "-c", git_utils.without_local_git_env(command) }, {
    display = command,
    env = { TMPDIR = env.TMPDIR },
    inherit_env = true,
    cwd = artifact.snapshot,
    timeout_ms = timeout_ms,
  })
  artifact.prepare_err = err
  return commit_prepared(call, artifact)
end

-- Sanitation moves metadata before restoring the trusted root pointer, so a worker link
-- cannot redirect the write.
function M.collect(call, artifact, project)
  local _, metadata_err = M.sanitize(artifact, true)
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
    note(artifact, IGNORED_OUTPUT, path_utils.shown(unrecorded))
  end
  local tree_lines, tree_err = snapshot_git(call, artifact, { "write-tree" })
  local tree = tree_lines and tree_lines[1]
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
  local parsed, binary = git_utils.parse_raw_diff(raw)
  for _, change in ipairs(parsed) do
    if utf8.len(change.path) then
      changes[#changes + 1] = change
    else
      unnamed[#unnamed + 1] = change.path
    end
  end
  local retyped = git_utils.retyped_paths(changes)
  for _, change in ipairs(changes) do
    change.kind = workspace.repository_path(change.path, artifact.repositories) and workspace.REPOSITORY_KIND
      or retyped[change.path] and workspace.TYPE_KIND
      or workspace.kind(change, binary)
  end
  if #unnamed > 0 then
    note(artifact, NOT_UTF8_CHANGES, path_utils.shown(unnamed), artifact.snapshot)
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
