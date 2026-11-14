local native = require("maki.claude_code.internal")
-- Plugin jobs report exit after cancellation, so a slot must wait for every process.
local M = {}

M.ERROR_PREFIX = "claude_code: "
M.LEFT_BEHIND = "maki did not remove: "
M.CANCELLED = "cancelled"
M.ERROR_BYTES = native.error_bytes
M.MS_PER_SEC = 1000

local Call = {}
Call.__index = Call

function M.with_stderr(head, stderr_tail)
  return #stderr_tail > 0 and head .. ":\n" .. table.concat(stderr_tail, "\n") or head
end

--- {released} runs once the call has replied and all its processes exited.
function M.new(released)
  return setmetatable({
    alive = 0,
    -- Jobs a cancel stops. The rest must outlive the cancel, because they
    -- remove the files the call made.
    stopped = false,
    stoppable = {},
    idle = {},
    answered = false,
    left_behind = {},
    release_hooks = released and { released } or {},
  }, Call)
end

--- Returns close status, an error, and whether the complete input was queued.
--- A close error can follow a complete prompt, so callers may need to keep its artifact.
function M.feed_stdin(id, data)
  local sent, err = maki.fn.chansend(id, data)
  if not sent then
    return nil, err, false
  end
  local closed, close_err = maki.fn.chanclose(id, "stdin")
  return closed, close_err, true
end

function Call:settle()
  if self.alive == 0 then
    local idle = self.idle
    self.idle = {}
    for _, run in ipairs(idle) do
      run()
    end
  end
  if self.answered and self.alive == 0 then
    local hooks = self.release_hooks
    self.release_hooks = {}
    for _, hook in ipairs(hooks) do
      hook()
    end
  end
end

function Call:wait(operation, ...)
  self.alive = self.alive + 1
  local args = table.pack(...)
  local result = maki.async.await(1, function(done)
    maki.async.spawn(function()
      local completed = table.pack(pcall(operation, table.unpack(args, 1, args.n)))
      self.alive = self.alive - 1
      self:settle()
      done(completed)
    end)
  end)
  if not result[1] then
    error(result[2], 0)
  end
  return table.unpack(result, 2, result.n)
end

function Call:on_released(hook)
  self.release_hooks[#self.release_hooks + 1] = hook
end

function Call:keep_artifact()
  self.artifact_kept = true
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
  job_opts.on_exit = function(id, code, _, timed_out, stderr_tail)
    local ok, err = pcall(on_exit, id, code, timed_out, stderr_tail or {})
    self.alive = self.alive - 1
    self:settle()
    if not ok then
      error(err, 0)
    end
  end
  local id, err = maki.fn.jobstart(argv, job_opts)
  if not id then
    return nil, err
  end
  self.alive = self.alive + 1
  return id
end

--- {how} has `on_exit(id, code, timed_out, stderr_tail)`, `timeout_ms`, and
--- `keep_on_cancel` for a job that must finish cleanup after cancellation.
function Call:spawn(argv, job_opts, how)
  if self.stopped and not how.keep_on_cancel then
    return nil, M.CANCELLED
  end
  job_opts.timeout_ms = how.timeout_ms
  local id, err = self:start(argv, job_opts, how.on_exit)
  if not id then
    return nil, err
  end
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

--- Returns the exit code, stdout lines, and any input error.
--- The code is nil after a timeout. Waits for `on_exit`, because a cancel can stop a coroutine that calls `jobwait`.
--- {how} has `started(id)` returning `(ok, err)`, `keep_on_cancel`, `timeout_ms`, and `on_stderr_tail`.
function Call:run_to_end(argv, job_opts, how)
  local lines = {}
  local input_error
  if job_opts.stdout == nil then
    job_opts.on_stdout = function(_, line)
      lines[#lines + 1] = line
    end
  end
  local code = maki.async.await(1, function(done)
    local function finished(_, exit_code, timed_out, stderr_tail)
      if how.on_stderr_tail then
        how.on_stderr_tail(stderr_tail or {})
      end
      local result = not timed_out and exit_code or nil
      done(result, timed_out)
    end
    local id, err = self:spawn(argv, job_opts, {
      keep_on_cancel = how.keep_on_cancel,
      timeout_ms = how.timeout_ms,
      on_exit = finished,
    })
    if not id then
      input_error = err
      finished()
      return
    end
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
function Call:run_lines(argv, spec)
  local shown = "`" .. (spec.display or table.concat(argv, " ")) .. "`"
  local stderr_tail = {}
  local job_opts = {
    cwd = spec.cwd,
    env = spec.env,
    clear_env = not spec.inherit_env,
    stdin = spec.stdin and "pipe" or nil,
    stdout = spec.stdout,
  }
  local how = {
    keep_on_cancel = spec.keep_on_cancel,
    timeout_ms = assert(spec.timeout_ms, "a command needs an explicit timeout_ms"),
    on_stderr_tail = function(tail)
      stderr_tail = tail
    end,
  }
  if spec.stdin then
    how.started = function(id)
      return M.feed_stdin(id, spec.stdin)
    end
  end
  local code, lines, run_error = self:run_to_end(argv, job_opts, how)
  if run_error then
    return nil, M.with_stderr("maki cannot run " .. shown .. ": " .. run_error, stderr_tail)
  end
  if not code then
    return nil, shown .. " did not complete in time", true
  end
  if code ~= 0 then
    return nil, M.with_stderr(shown .. " exited with code " .. code, stderr_tail)
  end
  return lines
end

return M
