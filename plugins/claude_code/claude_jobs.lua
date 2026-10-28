-- Plugin jobs report exit after cancellation, so a slot must wait for every process.
local truncate = require("maki.truncate")
local M = {}

M.ERROR_PREFIX = "claude_code: "
M.LEFT_BEHIND = "maki did not remove: "
M.CANCELLED = "cancelled"
local STDIN_CHUNK_BYTES = 64 * 1024
local STDIN_RETRY_MS = 10
local STDERR_TAIL_LINES = 10
local STDERR_LINE_BYTES = 4096
M.ERROR_BYTES = STDERR_TAIL_LINES * STDERR_LINE_BYTES
M.MS_PER_SEC = 1000

local Call = {}
Call.__index = Call

function M.with_stderr(head, stderr_tail)
  return #stderr_tail > 0 and head .. ":\n" .. table.concat(stderr_tail, "\n") or head
end

--- {released} runs once the call has replied and all its processes exited.
function M.new(startup_ms, released)
  return setmetatable({
    alive = 0,
    -- Jobs a cancel stops. The rest must outlive the cancel, because they
    -- remove the files the call made.
    stopped = false,
    stoppable = {},
    idle = {},
    answered = false,
    left_behind = {},
    startup_ms = startup_ms,
    released = released,
  }, Call)
end

function M.keep_tail(tail, line)
  tail[#tail + 1] = truncate(line, nil, STDERR_LINE_BYTES)
  if #tail > STDERR_TAIL_LINES then
    table.remove(tail, 1)
  end
end

function M.feed_stdin(id, data)
  local limits = maki.fn.chansend_limits
  local chunk_bytes = math.min(STDIN_CHUNK_BYTES, limits.max_bytes)
  for offset = 1, #data, chunk_bytes do
    local chunk = data:sub(offset, offset + chunk_bytes - 1)
    while true do
      local sent, err = maki.fn.chansend(id, chunk)
      if sent then
        break
      end
      if err ~= limits.full_error then
        return nil, err
      end
      maki.async.sleep(STDIN_RETRY_MS)
    end
  end
  return maki.fn.chanclose(id, "stdin")
end

function Call:settle()
  if self.alive == 0 then
    local idle = self.idle
    self.idle = {}
    for _, run in ipairs(idle) do
      run()
    end
  end
  local released = self.released
  if self.answered and self.alive == 0 and released then
    self.released = nil
    released()
  end
end

function Call:answer()
  self.answered = true
  local left_behind = self.left_behind
  self.left_behind = {}
  for _, dir in ipairs(left_behind) do
    self:leave(dir)
  end
  self:settle()
end

function Call:start(argv, job_opts, on_exit)
  job_opts.scope = "plugin"
  job_opts.guard = true
  job_opts.on_exit = function(id, code, _, timed_out)
    self.alive = self.alive - 1
    local ok, err = pcall(on_exit, id, code, timed_out)
    self:settle()
    if not ok then
      error(err, 0)
    end
  end
  local id, err = maki.fn.jobstart(argv, job_opts)
  if not id then
    error(err, 0)
  end
  self.alive = self.alive + 1
  return id
end

--- {how} has `on_exit(id, code, timed_out)`, `timeout_ms`, and
--- `keep_on_cancel` for a job that a kill could interrupt. The startup limit
--- applies to such a job unless it has its own.
function Call:spawn(argv, job_opts, how)
  if self.stopped and not how.keep_on_cancel then
    error(M.CANCELLED, 0)
  end
  local limit = how.timeout_ms or how.keep_on_cancel and self.startup_ms or nil
  job_opts.timeout_ms = limit
  local id = self:start(argv, job_opts, how.on_exit)
  if not how.keep_on_cancel then
    self.stoppable[#self.stoppable + 1] = id
  end
  return id
end

function Call:when_idle(run)
  if self.alive == 0 then
    run()
  else
    self.idle[#self.idle + 1] = run
  end
end

function Call:stop()
  self.stopped = true
  for _, id in ipairs(self.stoppable) do
    maki.fn.jobstop(id)
  end
end

--- The message goes in the reply, or in a notification once the reply is
--- sent.
function Call:leave(dir)
  if self.answered then
    maki.notify(M.ERROR_PREFIX .. M.LEFT_BEHIND .. dir, "warn")
  else
    self.left_behind[#self.left_behind + 1] = dir
  end
end

function Call:remove_dir(dir, removed)
  local ok = maki.fs.rm(dir)
  if not ok and maki.fs.metadata(dir) then
    self:leave(dir)
  end
  if removed then
    removed()
  end
end

--- Returns the exit code, stdout lines, and any input error.
--- The code is nil after a timeout. Waits for `on_exit`, because a cancel can stop a coroutine that calls `jobwait`.
--- {how} has `started(id)` returning `(ok, err)`, `on_line(id, line)`, `keep_on_cancel`,
--- and `leftover(lines)`, which names a directory to remove first.
function Call:run_to_end(argv, job_opts, how, timeout_ms)
  local lines = {}
  local input_error
  if job_opts.stdout == nil then
    job_opts.on_stdout = function(id, line)
      lines[#lines + 1] = line
      if how.on_line then
        how.on_line(id, line)
      end
    end
  end
  local code = maki.async.await(1, function(done)
    local id = self:spawn(argv, job_opts, {
      keep_on_cancel = how.keep_on_cancel,
      timeout_ms = timeout_ms,
      on_exit = function(_, exit_code, timed_out)
        local result = not timed_out and exit_code or nil
        local leftover = how.leftover and how.leftover(lines)
        if leftover then
          self:remove_dir(leftover, function()
            done(result, timed_out)
          end)
        else
          done(result, timed_out)
        end
      end,
    })
    if how.started then
      local _, err = how.started(id)
      if err then
        input_error = err
        local info = maki.fn.jobinfo(id)
        if not info or info.exit_code == nil then
          maki.fn.jobstop(id)
        end
      end
    end
  end)
  return code, lines, input_error
end

--- The `inherit_env` option merges `env` into maki's environment without provider keys.
--- Otherwise `env` supplies the entire child environment.
--- The `display` option replaces the joined argv in error messages.
--- `leftover` returns a directory to remove after exit, including cancellation.
function Call:run_quick(argv, spec)
  local shown = "`" .. (spec.display or table.concat(argv, " ")) .. "`"
  local stderr_tail = {}
  local job_opts = {
    cwd = spec.cwd,
    env = spec.env,
    clear_env = not spec.inherit_env,
    stdin = spec.stdin and "pipe" or nil,
    stdout = spec.stdout,
    on_stderr = function(_, line)
      M.keep_tail(stderr_tail, line)
    end,
  }
  local how = { keep_on_cancel = spec.keep_on_cancel, leftover = spec.leftover }
  if spec.stdin then
    how.started = function(id)
      return M.feed_stdin(id, spec.stdin)
    end
  end
  local ok, code, lines, run_error =
    pcall(self.run_to_end, self, argv, job_opts, how, spec.timeout_ms or self.startup_ms)
  if not ok then
    return nil, M.with_stderr("maki cannot run " .. shown .. ": " .. tostring(code), stderr_tail)
  end
  if not code then
    return nil, shown .. " did not complete in time", true
  end
  if code ~= 0 then
    return nil, M.with_stderr(shown .. " exited with code " .. code, stderr_tail)
  end
  if run_error then
    return nil, M.with_stderr("maki cannot run " .. shown .. ": " .. run_error, stderr_tail)
  end
  return table.concat(lines, "\n")
end

return M
