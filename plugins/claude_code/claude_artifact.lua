local jobs = require("claude_jobs")
local git_utils = require("claude_git")
local M = {}

M.TRASH_PREFIX = ".trash-"
M.SWEEP_LOCK = ".sweep.lock"
M.CANNOT_SWEEP = jobs.ERROR_PREFIX .. "maki cannot lock artifact cleanup in "
-- Each step of a coding snapshot, such as the copy, and each artifact removal
-- must finish in this time.
M.STEP_TIMEOUT_MS = 120 * jobs.MS_PER_SEC
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
-- An artifact's files by role. `tmp` is the only other place the worker's shell can write.
local LAYOUT = {
  snapshot = "snapshot",
  git = "git",
  tmp = "tmp",
  manifest_path = "manifest.json",
  lock = "import.lock",
  marker = ".maki-claude-code-artifact",
  originals = "originals",
  displaced = "displaced",
  quarantine = "git-metadata",
  check_index = "consistency-index",
  copy_started = "copy-started",
  -- A unique stage keeps retries from removing another attempt's files.
  stage_template = "stage.XXXXXX",
  receipt = "receipt",
}
local SECS_PER_HOUR = 3600

--- Removes the artifact {dir}, then runs {released}. The removal outlives a
--- cancel of {call}.
function M.remove(call, dir, released)
  local function finished(_, code)
    if code ~= 0 then
      call:leave(dir)
    end
    if released then
      released()
    end
  end
  local argv = { "sh", "-c", SWEEP_CONTENTS, "sh", dir, LAYOUT.marker }
  if not call:start(argv, { timeout_ms = M.STEP_TIMEOUT_MS }, finished, true) then
    finished()
  end
end

function M.layout(dir)
  local paths = { id = maki.fs.basename(dir), dir = dir }
  for role, name in pairs(LAYOUT) do
    paths[role] = maki.fs.joinpath(dir, name)
  end
  return paths
end

--- An import rewrites the manifest, so the sweep keeps the artifact of an
--- unfinished import. An artifact that stopped before the collect step is
--- dated by its marker.
function M.sweep(call, root, ttl_hours)
  -- Artifact locks are unlinked during deletion. Keep this lock outside the deleted tree.
  local sweep_lock, lock_err = maki.fs.try_lock(maki.fs.joinpath(root, M.SWEEP_LOCK))
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
    local artifact = M.layout(maki.fs.joinpath(root, name))
    local marker = kind == "directory" and maki.fs.metadata(artifact.marker)
    local meta = marker and (maki.fs.metadata(artifact.manifest_path) or marker)
    if meta and meta.mtime and meta.mtime < cutoff then
      local lock = maki.fs.try_lock(artifact.lock)
      if lock then
        local current = maki.fs.metadata(artifact.manifest_path) or marker
        if current.mtime and current.mtime < cutoff then
          local trash = maki.fs.joinpath(root, M.TRASH_PREFIX .. name)
          local already_trash = name:sub(1, #M.TRASH_PREFIX) == M.TRASH_PREFIX
          local renamed = already_trash or maki.fs.rename(artifact.dir, trash)
          if already_trash then
            trash = artifact.dir
          end
          if renamed then
            M.remove(call, trash, function()
              lock:close()
            end)
          else
            call:leave(artifact.dir)
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

--- The artifact belongs to {call} from its creation, so the call removes it after
--- a failed allocation too.
function M.allocate(call, root, env)
  local _, root_err = maki.fs.mkdir(root, { parents = true })
  if root_err then
    return nil, "maki cannot make " .. root .. ": " .. root_err
  end
  local dir, dir_err = maki.fs.mkdtemp(maki.fs.joinpath(root, ARTIFACT_TEMPLATE))
  if not dir then
    return nil, dir_err
  end
  local artifact = M.layout(dir)
  artifact.git_env = git_utils.snapshot_git_env(env, artifact.git, artifact.snapshot)
  artifact.notes = {}
  call.artifact = artifact
  local _, mark_err = maki.fs.write(artifact.marker, "")
  if mark_err then
    return nil, "maki cannot mark " .. dir .. " as an artifact: " .. mark_err
  end
  local lock, lock_err = maki.fs.try_lock(artifact.lock)
  if not lock then
    return nil, "maki cannot lock its coding artifact: " .. lock_err
  end
  call:on_released(function()
    lock:close()
  end)
  for _, sub in ipairs({ artifact.snapshot, artifact.tmp }) do
    local _, mkdir_err = maki.fs.mkdir(sub)
    if mkdir_err then
      return nil, "maki cannot make " .. sub .. ": " .. mkdir_err
    end
  end
  return artifact
end

return M
