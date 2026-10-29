local native = require("maki.claude_code.internal")
local workspace = require("claude_workspace")
local git_utils = require("claude_git")
local import_scripts = require("claude_import_script")
local path_utils = require("claude_paths")
local jobs = require("claude_jobs")

local M = {}

local MAX_IMPORT_SCRIPT_BYTES = 96 * 1024
local IMPORT_TOO_LARGE = "maki cannot import this many changes in one command. Use `paths` to import smaller groups."
local IMPORT_COMMAND = "the import command"
local LEFTOVERS_REMAIN = "maki could not remove its temporary files, so files starting with %s may remain "
  .. "next to the files it wrote."
local UNRECORDED = "maki could not record this in the artifact (%s), so a second import will show these files as "
  .. "changed."

local function disk_blobs(call, env, cwd, paths, keep_on_cancel, timeout_ms)
  if #paths == 0 then
    return {}
  end
  local out, err = call:run_lines(git_utils.git({ "hash-object", "--no-filters", "--stdin-paths" }), {
    env = env,
    cwd = cwd,
    stdin = git_utils.quoted_lines(paths),
    keep_on_cancel = keep_on_cancel,
    timeout_ms = timeout_ms,
  })
  if not out then
    return nil, err
  end
  return out
end

-- Maps each path to its `file_state`, false when missing, or a note when it
-- is not a regular file. Hashes use the object format of {artifact}'s
-- repository, which can differ from the checkout's.
local function checkout_state(call, artifact, changes, keep_on_cancel)
  local project = artifact.manifest.project
  local timeout_ms = import_scripts.import_timeout(changes) * jobs.MS_PER_SEC
  local git_env = git_utils.snapshot_git_env({}, artifact.git, project)
  local state, files, executable = {}, {}, {}
  for _, change in ipairs(changes) do
    local meta, meta_err = native.checkout_file(maki.fs.joinpath(project, change.path))
    if meta_err then
      return nil, meta_err
    elseif meta == nil then
      state[change.path] = false
    elseif meta.regular then
      files[#files + 1] = change.path
      executable[#files] = meta.executable
    else
      state[change.path] = "not a file"
    end
  end
  local blobs, err = disk_blobs(call, git_env, project, files, keep_on_cancel, timeout_ms)
  if not blobs then
    return nil, err
  end
  for i, path in ipairs(files) do
    state[path] = workspace.file_state(blobs[i], executable[i])
  end
  return state
end

local function missing_artifact(id)
  return "there is no change artifact " .. id .. ". If it was there before, the sweep removed it."
end

--- Manifest fields end up in the command the user approves, so the manifest
--- must come from a collect step for {spec.project}. The open operation rewrites it,
--- because the sweep goes by its date and the approval can outlast the
--- artifact's time limit.
local function open_import(spec)
  local dir = maki.fs.joinpath(spec.root, spec.id :: string)
  local manifest_path = maki.fs.joinpath(dir, path_utils.MANIFEST)
  local text = maki.fs.read(manifest_path)
  local manifest = text and maki.json.decode(text)
  if
    type(manifest) ~= "table"
    or manifest.version ~= workspace.MANIFEST_VERSION
    or type(manifest.changes) ~= "table"
  then
    return nil, missing_artifact(spec.id)
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
  local seen = {}
  for _, change in ipairs(manifest.changes) do
    local problem = workspace.change_problem(change)
    if not problem and seen[change.path] then
      problem = "duplicate path " .. path_utils.printable(change.path)
    end
    if problem then
      return nil, "artifact " .. spec.id .. " has a manifest that maki did not write: " .. problem
    end
    seen[change.path] = true
  end
  local _, touch_err = maki.fs.atomic_write(manifest_path, text)
  if touch_err then
    return nil, "maki cannot stop the expiry of artifact " .. spec.id .. " during the import: " .. touch_err
  end
  manifest.imported = type(manifest.imported) == "table" and manifest.imported or {}
  return {
    dir = dir,
    git = maki.fs.joinpath(dir, path_utils.GIT_DIR),
    manifest_path = manifest_path,
    manifest = manifest,
  }
end

--- Returns the changes to import, and for every other change the reason it
--- is skipped. The default is all changes. A path may also be spelled as the
--- report shows it, with control bytes escaped.
local function select_changes(manifest, paths)
  local by_path = {}
  local function add(path, change)
    if by_path[path] == nil then
      by_path[path] = change
    elseif by_path[path] ~= change then
      by_path[path] = false
    end
  end
  for _, change in ipairs(manifest.changes) do
    add(change.path, change)
    add(path_utils.printable(change.path), change)
  end
  local selected = manifest.changes
  if paths then
    selected = {}
    local named = {}
    for _, path in ipairs(paths) do
      local change = by_path[path]
      if change == false then
        return nil, path_utils.printable(path) .. " matches more than one change. Omit `paths` to import all changes"
      end
      if not change then
        return nil, path_utils.printable(path) .. " is not a change of the artifact"
      end
      if not named[change] then
        named[change] = true
        selected[#selected + 1] = change
      end
    end
  end
  local todo, skipped = {}, {}
  for _, change in ipairs(selected) do
    local shown_path = path_utils.printable(change.path)
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

--- Reject checkout edits made since the snapshot before requesting approval.
local function check_import(call, artifact, todo)
  local current, current_err = checkout_state(call, artifact, todo)
  if not current then
    return "maki cannot examine the checkout: " .. current_err
  end
  local conflicts = workspace.conflicts(todo, current)
  if #conflicts > 0 then
    return "maki imported no changes: these files changed in the checkout after the snapshot: "
      .. path_utils.shown(conflicts)
      .. ". Merge them manually from "
      .. maki.fs.joinpath(artifact.dir, path_utils.SNAPSHOT_DIR)
      .. ", or import the remaining changes with `paths`."
  end
end

--- Deepest folders first.
local function missing_folders(project, changes)
  local missing, seen = {}, {}
  for _, change in ipairs(changes) do
    if change.status == git_utils.ADDED then
      for parent in path_utils.folders(change.path) do
        local folder = maki.fs.joinpath(project, parent)
        if not seen[folder] then
          seen[folder] = true
          local meta, meta_err = maki.fs.metadata(folder)
          if not meta and not meta_err then
            missing[#missing + 1] = folder
          end
        end
      end
    end
  end
  table.sort(missing, function(a, b)
    return #a > #b
  end)
  return missing
end

local function stage_blobs(call, artifact, todo, stage, timeout)
  local hashes, seen = {}, {}
  for _, change in ipairs(todo) do
    if change.status ~= git_utils.DELETED and not seen[change.new_sha] then
      seen[change.new_sha] = true
      hashes[#hashes + 1] = change.new_sha
    end
  end
  if #hashes == 0 then
    return true
  end
  local batch = maki.fs.joinpath(stage, "batch")
  local output, err = call:run_quick(git_utils.git({ "--git-dir=" .. artifact.git, "cat-file", "--batch" }), {
    cwd = artifact.manifest.project,
    env = git_utils.snapshot_git_env({}, artifact.git, artifact.manifest.project),
    stdin = table.concat(hashes, "\n") .. "\n",
    stdout = batch,
    timeout_ms = timeout * jobs.MS_PER_SEC,
  })
  if not output then
    return nil, "maki cannot stage the artifact blobs: " .. err
  end
  return native.unpack_blobs(batch, stage, hashes)
end

--- Returns the changes the import applied, and the error if it stopped. The
--- command can stop anywhere between approval and the last rename, so maki
--- reads the checkout to see what landed.
local function apply_import(ctx, call, artifact, todo)
  local project = artifact.manifest.project
  local made = missing_folders(project, todo)
  local stage, stage_err = maki.fs.mkdtemp(maki.fs.joinpath(artifact.dir, path_utils.STAGE_TEMPLATE))
  if not stage then
    return nil, "maki cannot make a stage folder in the artifact: " .. stage_err
  end
  local script = import_scripts.import_script(todo, project, artifact.git, artifact.dir, stage)
  local timeout = import_scripts.import_timeout(todo)
  local staged, blob_err
  if #script > MAX_IMPORT_SCRIPT_BYTES then
    blob_err = IMPORT_TOO_LARGE
  else
    staged, blob_err = stage_blobs(call, artifact, todo, stage, timeout)
  end
  if not staged then
    maki.fs.rm(stage, { recursive = true, force = true })
    return nil, blob_err
  end
  local receipt = import_scripts.receipt_path(artifact.dir, stage)
  local _, err = maki.agent.call_tool(ctx, "bash", {
    command = script,
    workdir = project,
    timeout = timeout,
    description = "Import " .. #todo .. " changes from Claude Code",
  })
  if not err then
    maki.fs.rm(receipt)
    return todo, nil, {}
  end
  -- This also runs after a cancel, so the cleanup and the read must survive
  -- it.
  local ran, code = pcall(
    call.run_to_end,
    call,
    { "bash", "-c", import_scripts.leftovers_script(todo, project, stage, made) },
    { cwd = project },
    { keep_on_cancel = true },
    call.startup_ms
  )
  local first, last = err:find(script, 1, true)
  if first then
    err = err:sub(1, first - 1) .. IMPORT_COMMAND .. err:sub(last + 1)
  end
  if not ran or code ~= 0 then
    err = err .. "\n" .. LEFTOVERS_REMAIN:format(import_scripts.temp_name(stage))
  end
  local now, now_err = checkout_state(call, artifact, todo, true)
  if not now then
    local paths = {}
    for i, change in ipairs(todo) do
      paths[i] = change.path
    end
    return nil, err .. "\nmaki cannot read the checkout (" .. now_err .. "). Examine " .. path_utils.shown(paths) .. "."
  end
  local written = maki.split(maki.fs.read(receipt) or "", "\0")
  maki.fs.rm(receipt)
  local landed, matched = workspace.landed(todo, now, path_utils.set_of(written))
  return landed, err, matched
end

--- One bash command, which checks every file again when it runs because the
--- approval can take long. If a file it changes also changed in the checkout
--- since the snapshot, or in the artifact since the collect step, nothing
--- applies.
local function import_locked(ctx, call, spec)
  local artifact, open_err = open_import(spec)
  if not artifact then
    return nil, open_err
  end
  local todo, skipped = select_changes(artifact.manifest, spec.paths)
  if not todo then
    return nil, skipped
  end
  local problem = check_import(call, artifact, todo)
  if problem then
    return nil, problem
  end
  if #todo == 0 then
    return { applied = {}, skipped = skipped }
  end
  local landed, failure, matched = apply_import(ctx, call, artifact, todo)
  if not landed then
    return nil, failure
  end
  for _, change in ipairs(matched) do
    skipped[#skipped + 1] = path_utils.printable(change.path) .. ": already matched"
    artifact.manifest.imported[change.path] = true
  end
  local replaced = false
  for _, change in ipairs(todo) do
    replaced = replaced or change.status ~= git_utils.ADDED
  end
  for _, change in ipairs(landed) do
    artifact.manifest.imported[change.path] = true
  end
  local _, record_err = maki.fs.atomic_write(artifact.manifest_path, maki.json.encode(artifact.manifest))
  local unrecorded = record_err and UNRECORDED:format(record_err) or nil
  local originals = replaced and maki.fs.joinpath(artifact.dir, path_utils.IMPORT_ORIGINALS) or nil
  local displaced = maki.fs.joinpath(artifact.dir, path_utils.IMPORT_DISPLACED)
  if #landed + #matched == #todo then
    return {
      applied = workspace.summary(landed),
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
  local written = {}
  for _, change in ipairs(landed) do
    written[change.path] = true
  end
  for _, change in ipairs(todo) do
    if written[change.path] then
      applied[#applied + 1] = change.path
    elseif not artifact.manifest.imported[change.path] then
      missed[#missed + 1] = change.path
    end
  end
  local kept = originals and " The preserved versions are in " .. originals .. " and " .. displaced .. "." or ""
  return nil, M.stopped_partway(applied, missed, kept .. (unrecorded and " " .. unrecorded or ""), failure)
end

function M.import(ctx, call, spec)
  if type(spec.id) ~= "string" or not spec.id:match(path_utils.ARTIFACT_ID) then
    return nil, "an artifact id contains only the characters A to Z, a to z and 0 to 9"
  end
  local dir = maki.fs.joinpath(spec.root, spec.id :: string)
  local missing = missing_artifact(spec.id)
  if not maki.fs.metadata(dir) then
    return nil, missing
  end
  local lock, lock_err = maki.fs.try_lock(maki.fs.joinpath(dir, path_utils.IMPORT_LOCK))
  if not lock then
    if not maki.fs.metadata(dir) then
      return nil, missing
    end
    return nil, "maki cannot lock artifact " .. spec.id .. ": " .. lock_err
  end
  local git_dir, project_err = native.project_git_dir(spec.project)
  local project_lock
  if git_dir then
    project_lock, project_err = maki.fs.try_lock(maki.fs.joinpath(git_dir, path_utils.PROJECT_LOCK))
  end
  if not project_lock then
    lock:close()
    return nil, "maki cannot lock checkout " .. spec.project .. ": " .. project_err
  end
  local ok, result, err = maki.async.await(1, function(done)
    maki.async.on_cancel(function()
      call:stop()
    end)
    -- Cleanup owns the locks even if the caller is abandoned during cancellation.
    maki.async.spawn(function()
      local ran, reply, problem = pcall(import_locked, ctx, call, spec)
      call:when_idle(function()
        project_lock:close()
        lock:close()
      end)
      done(ran, reply, problem)
    end)
  end)
  if not ok then
    error(result, 0)
  end
  return result, err
end

--- Reports an import that stopped partway: the paths it {applied} and
--- {missed}, other notes for the user, and why it stopped.
function M.stopped_partway(applied, missed, notes, failure)
  return "the import stopped before it completed. It applied "
    .. path_utils.shown(applied)
    .. ", and not "
    .. path_utils.shown(missed)
    .. "."
    .. notes
    .. "\n"
    .. failure
end

return M
