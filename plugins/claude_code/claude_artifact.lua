local jobs = require("claude_jobs")
local git_utils = require("claude_git")
local path_utils = require("claude_paths")
local M = {}

local TRASH_PREFIX = ".trash-"
local SWEEP_LOCK = ".sweep.lock"
M.CANNOT_SWEEP = jobs.ERROR_PREFIX .. "maki cannot lock artifact cleanup in "
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
local OWNER_MARKER = ".maki-claude-code-artifact"
local SECS_PER_HOUR = 3600

local function remove_artifact(call, dir, released)
  local function finished(_, code)
    if code ~= 0 then
      call:leave(dir)
    end
    if released then
      released()
    end
  end
  if not call:start({ "sh", "-c", SWEEP_CONTENTS, "sh", dir, OWNER_MARKER }, {}, finished) then
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
    if lock_err == maki.fs.lock_busy then
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
    local name, parent = maki.fs.basename(existing), maki.fs.dirname(existing)
    local entry, entry_err = maki.fs.metadata(existing, { follow = false })
    if entry then
      return nil, existing .. LINK_TO_NOWHERE
    elseif entry_err then
      return nil, "maki cannot read " .. existing .. ": " .. entry_err
    end
    table.insert(missing, 1, name)
    existing = parent
  end
  local resolved, err = maki.fs.realpath(existing)
  if not resolved then
    return nil, err
  end
  resolved = maki.fs.joinpath(resolved, unpack(missing))
  if path_utils.in_checkout(resolved, spec) then
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
  local lock, lock_err = maki.fs.try_lock(maki.fs.joinpath(dir, path_utils.IMPORT_LOCK))
  if not lock then
    discard_failed_allocation(call, dir)
    return nil, "maki cannot lock its coding artifact: " .. lock_err
  end
  call:on_released(function()
    lock:close()
  end)
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

return M
