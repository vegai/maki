-- Plugin jobs report exit after cancellation, so a slot must wait for every process.
--
-- Use a `sleep` job for timeouts. Its exit arrives even when queued calls occupy every Lua
-- slot and block Lua timers.

local M = {}

M.ERROR_PREFIX = "claude_code: "
M.LEFT_BEHIND = "maki did not remove: "
M.CLOCK = "sleep"
local STDERR_TAIL_LINES = 10
M.MS_PER_SEC = 1000

local Call = {}
Call.__index = Call

function M.clock(seconds)
  return { M.CLOCK, string.format("%.3f", seconds) }
end

function M.clock_problem(code)
  if code ~= 0 then
    return "the timeout clock exited with code " .. code
  end
end

function M.with_stderr(head, stderr_tail)
  return #stderr_tail > 0 and head .. ":\n" .. table.concat(stderr_tail, "\n") or head
end

--- {released} runs once the call has replied and all its processes exited.
function M.new(startup_ms, released)
  return setmetatable({
    alive = 0,
    -- Jobs a cancel stops. The rest must outlive the cancel, because they
    -- remove the files the call made.
    stoppable = {},
    idle = {},
    answered = false,
    left_behind = {},
    startup_ms = startup_ms,
    released = released,
  }, Call)
end

function M.keep_tail(tail, line)
  tail[#tail + 1] = line
  if #tail > STDERR_TAIL_LINES then
    table.remove(tail, 1)
  end
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
  job_opts.kill_group_on_exit = true
  job_opts.on_exit = function(id, code)
    self.alive = self.alive - 1
    local ok, err = pcall(on_exit, id, code)
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

--- {how} has `on_exit(id, code, clock_code)`, `timeout_ms`, and
--- `keep_on_cancel` for a job that a kill could interrupt. The startup limit
--- applies to such a job unless it has its own.
function Call:spawn(argv, job_opts, how)
  local limit = how.timeout_ms or how.keep_on_cancel and self.startup_ms or nil
  local id, clock, exited, clock_code
  -- The clock starts first, so a job never runs without its limit. It is
  -- not one of the call's processes, so the call need not wait for the
  -- clock that the job's exit stops.
  if limit then
    local clock_err
    clock, clock_err = maki.fn.jobstart(M.clock(limit / M.MS_PER_SEC), {
      scope = "plugin",
      env = job_opts.env,
      clear_env = true,
      on_exit = function(_, code)
        if id and not exited then
          clock_code = code
          maki.fn.jobstop(id)
        end
      end,
    })
    if not clock then
      error(clock_err, 0)
    end
  end
  local ok, started = pcall(self.start, self, argv, job_opts, function(job, code)
    exited = true
    if clock then
      maki.fn.jobstop(clock)
    end
    if how.on_exit then
      how.on_exit(job, code, clock_code)
    end
  end)
  if not ok then
    if clock then
      maki.fn.jobstop(clock)
    end
    error(started, 0)
  end
  id = started
  if not how.keep_on_cancel then
    self.stoppable[#self.stoppable + 1] = id
    -- Each job gets its own hook, because `stop` runs on a cancel only for a
    -- coding call. A job started after the cancel stops as soon as its hook
    -- is registered.
    maki.async.on_cancel(function()
      maki.fn.jobstop(id)
    end)
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

--- Calls {removed} even when `rmdir` fails, because this usually runs in
--- another job's exit. A directory a hook wrote to stays for the user.
function Call:remove_dir(dir, env, removed)
  local function finish(code)
    -- Another cleanup of the same call already removed it.
    if code ~= 0 and maki.fs.metadata(dir) then
      self:leave(dir)
    end
    if removed then
      removed()
    end
  end
  local started = pcall(self.spawn, self, { "rmdir", dir }, { env = env, clear_env = true }, {
    keep_on_cancel = true,
    on_exit = function(_, code)
      finish(code)
    end,
  })
  if not started then
    finish(nil)
  end
end

function Call:remove_dir_now(dir, env)
  maki.async.await(1, function(done)
    self:remove_dir(dir, env, done)
  end)
end

--- Returns the exit code, stdout lines, and any input or clock error.
--- The code is nil if the clock exits first. Waits for `on_exit`, because a cancel can stop a coroutine that calls `jobwait`.
--- {how} has `started(id)` returning `(ok, err)`, `on_line(id, line)`, `keep_on_cancel`,
--- and `leftover(lines)`, which names a directory to remove first.
function Call:run_to_end(argv, job_opts, how, timeout_ms)
  local lines = {}
  local input_error
  job_opts.on_stdout = function(id, line)
    lines[#lines + 1] = line
    if how.on_line then
      how.on_line(id, line)
    end
  end
  local code, clock_code = maki.async.await(1, function(done)
    local id = self:spawn(argv, job_opts, {
      keep_on_cancel = how.keep_on_cancel,
      timeout_ms = timeout_ms,
      on_exit = function(_, exit_code, clock_exit)
        local result = clock_exit == nil and exit_code or nil
        local leftover = how.leftover and how.leftover(lines)
        if leftover then
          self:remove_dir(leftover, job_opts.env, function()
            done(result, clock_exit)
          end)
        else
          done(result, clock_exit)
        end
      end,
    })
    if how.started then
      local _, err = how.started(id)
      if err then
        input_error = err
        maki.fn.jobstop(id)
      end
    end
  end)
  return code, lines, input_error or (clock_code and M.clock_problem(clock_code))
end

--- The `inherit_env` option merges `env` into maki's environment without provider keys.
--- Otherwise `env` supplies the entire child environment.
function Call:run_quick(argv, spec)
  local shown = "`" .. table.concat(argv, " ") .. "`"
  local stderr_tail = {}
  local job_opts = {
    cwd = spec.cwd,
    env = spec.env,
    clear_env = not spec.inherit_env,
    stdin = spec.stdin and "pipe" or nil,
    on_stderr = function(_, line)
      M.keep_tail(stderr_tail, line)
    end,
  }
  local how = { keep_on_cancel = spec.keep_on_cancel }
  if spec.stdin then
    how.started = function(id)
      local sent, err = maki.fn.chansend(id, spec.stdin)
      if not sent then
        return nil, err
      end
      return maki.fn.chanclose(id, "stdin")
    end
  end
  local ok, code, lines, run_error =
    pcall(self.run_to_end, self, argv, job_opts, how, spec.timeout_ms or self.startup_ms)
  if not ok or run_error then
    return nil, "maki cannot run " .. shown .. ": " .. tostring(run_error or code)
  end
  if not code then
    return nil, shown .. " did not complete in time", true
  end
  if code ~= 0 then
    return nil, M.with_stderr(shown .. " exited with code " .. code, stderr_tail)
  end
  return table.concat(lines, "\n")
end

return M
