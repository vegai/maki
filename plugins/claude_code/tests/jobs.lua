local t = require("maki.test_helpers")
local jobs = require("claude_jobs")
local snapshot = require("claude_snapshot")

local case, eq, has = t.case, t.eq, t.has
local STARTUP_MS = 30000
local COMMAND = { "cat" }
local OUTPUT = "complete"
local CALLBACK_ERROR = "exit callback failed"
local SEND_ERROR = "stdin send failed"
local CLOSE_ERROR = "stdin close failed"
local DIRECTORY = "/probe/hook-output"
local SUCCESS_CODE = 0
local STOPPED_CODE = 137

local function with_jobs(run)
  local state = { started = {}, stopped = {}, notices = {}, cancel = {}, timers = {}, closes = 0 }
  local overrides = {
    {
      maki,
      "defer_fn",
      function(callback, _ms)
        local timer = { callback = callback, stopped = false }
        function timer:stop()
          self.stopped = true
        end
        state.timers[#state.timers + 1] = timer
        return timer
      end,
    },
    {
      maki.fn,
      "jobstart",
      function(argv, opts)
        if state.start_error then
          return nil, state.start_error
        end
        state.started[#state.started + 1] = { argv = argv, opts = opts }
        return #state.started
      end,
    },
    {
      maki.fn,
      "jobinfo",
      function()
        return state.info
      end,
    },
    {
      maki.fn,
      "jobstop",
      function(id)
        state.stopped[id] = true
      end,
    },
    {
      maki.fn,
      "chansend",
      function(_, data)
        if state.send_error then
          return nil, state.send_error
        end
        return #data
      end,
    },
    {
      maki.fn,
      "chanclose",
      function()
        state.closes = state.closes + 1
        if state.close_error then
          return nil, state.close_error
        end
        return 1
      end,
    },
    {
      maki.async,
      "on_cancel",
      function(hook)
        state.cancel[#state.cancel + 1] = hook
      end,
    },
    {
      maki.async,
      "await",
      function(_, start)
        local result
        start(function(...)
          result = table.pack(...)
        end)
        state.finish()
        assert(result, "the exit callback did not resolve the wait")
        return table.unpack(result, 1, result.n)
      end,
    },
  }
  for _, override in ipairs(overrides) do
    override.original = override[1][override[2]]
    override[1][override[2]] = override[3]
  end
  maki.set_notify_handler(function(message, level)
    state.notices[#state.notices + 1] = { message = message, level = level }
  end)
  local ok, err = pcall(run, state)
  maki.set_notify_handler(nil)
  for _, override in ipairs(overrides) do
    override[1][override[2]] = override.original
  end
  if not ok then
    error(err, 0)
  end
end

for _, answered in ipairs({ false, true }) do
  case("a_raising_exit_releases_the_slot_answered_" .. tostring(answered), function()
    with_jobs(function(state)
      local releases = 0
      local call = jobs.new(STARTUP_MS, function()
        releases = releases + 1
      end)
      call:start(COMMAND, {}, function()
        error(CALLBACK_ERROR, 0)
      end)
      if answered then
        call:answer()
      end
      local ok, err = pcall(state.started[1].opts.on_exit, 1, SUCCESS_CODE)
      eq(ok, false)
      eq(err, CALLBACK_ERROR)
      if not answered then
        call:answer()
      end
      eq(releases, 1)
      call:answer()
      eq(releases, 1)
    end)
  end)
end

case("a_native_deadline_reports_a_timeout", function()
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS, function() end)
    state.finish = function()
      eq(state.started[1].opts.timeout_ms, STARTUP_MS)
      state.started[1].opts.on_exit(1, STOPPED_CODE, "exit", true)
    end
    local output, err, timed_out = call:run_quick(COMMAND, {})
    eq(output, nil)
    has(err, "did not complete in time")
    eq(timed_out, true)
  end)
end)

case("a_completed_job_reports_its_output", function()
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS, function() end)
    state.finish = function()
      state.started[1].opts.on_stdout(1, OUTPUT)
      state.started[1].opts.on_exit(1, SUCCESS_CODE)
    end
    local output, err = call:run_quick(COMMAND, {})
    eq(output, OUTPUT)
    eq(err, nil)
  end)
end)

for _, cancel_job in ipairs({ false, true }) do
  case("a_stopped_call_allows_only_cleanup_jobs_cancel_hook_" .. tostring(cancel_job), function()
    with_jobs(function(state)
      local call = jobs.new(STARTUP_MS)
      if cancel_job then
        maki.async.on_cancel(function()
          call:stop()
        end)
      end
      state.finish = function()
        if cancel_job then
          state.cancel[1]()
        else
          call:stop()
        end
        eq(state.stopped[1], true)
        state.started[1].opts.on_exit(1, STOPPED_CODE)
      end
      call:run_quick(COMMAND, {})

      local output, err = call:run_quick(COMMAND, {})
      eq(output, nil)
      eq(#state.started, 1)
      has(err, jobs.CANCELLED)

      state.finish = function()
        state.started[2].opts.on_stdout(2, OUTPUT)
        state.started[2].opts.on_exit(2, SUCCESS_CODE)
      end
      output, err = call:run_quick(COMMAND, { keep_on_cancel = true })
      eq(output, OUTPUT)
      eq(err, nil)
      eq(#state.started, 2)
      eq(state.stopped[2], nil)
      eq(call.alive, 0)
      eq(#state.cancel, cancel_job and 1 or 0)
    end)
  end)
end

case("redirected_output_keeps_the_guard_and_explicit_timeout", function()
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS)
    local timeout = STARTUP_MS * 2
    state.finish = function()
      local opts = state.started[1].opts
      eq(opts.stdout, DIRECTORY)
      eq(opts.on_stdout, nil)
      eq(opts.guard, true)
      eq(opts.timeout_ms, timeout)
      opts.on_exit(1, SUCCESS_CODE)
    end
    local output, err = call:run_quick(COMMAND, { stdout = DIRECTORY, stdin = OUTPUT, timeout_ms = timeout })
    eq(output, "")
    eq(err, nil)
    eq(state.closes, 1)
  end)
end)

for _, failure in ipairs({ "send", "close" }) do
  case("a_stdin_" .. failure .. "_error_stops_the_job_and_wins_over_its_exit", function()
    with_jobs(function(state)
      state.close_error = CLOSE_ERROR
      if failure == "send" then
        state.send_error = SEND_ERROR
      end
      local call = jobs.new(STARTUP_MS, function() end)
      state.finish = function()
        eq(state.stopped[1], true)
        state.started[1].opts.on_exit(1, SUCCESS_CODE)
      end
      local output, err = call:run_quick(COMMAND, { stdin = OUTPUT })
      eq(output, nil)
      has(err, failure == "send" and SEND_ERROR or CLOSE_ERROR)
      eq(state.closes, failure == "send" and 0 or 1)
    end)
  end)
end

for _, exited in ipairs({ false, true }) do
  case("a_failed_stdin_write_stops_only_a_live_child_" .. tostring(exited), function()
    with_jobs(function(state)
      state.send_error = SEND_ERROR
      state.info = { exit_code = exited and SUCCESS_CODE or nil }
      local call = jobs.new(STARTUP_MS)
      state.finish = function()
        eq(state.stopped[1] == true, not exited)
        state.started[1].opts.on_stderr(1, CALLBACK_ERROR)
        state.started[1].opts.on_exit(1, SUCCESS_CODE)
      end
      local output, err = call:run_quick(COMMAND, { stdin = OUTPUT })
      eq(output, nil)
      has(err, SEND_ERROR)
      has(err, CALLBACK_ERROR)
    end)
  end)
end

case("an_answer_reports_pending_directories_once_and_later_cleanup_notifies", function()
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS, function() end)
    call:leave(DIRECTORY)
    eq(#state.notices, 0)
    call:answer()
    eq(#state.notices, 1)
    eq(state.notices[1].message, jobs.ERROR_PREFIX .. jobs.LEFT_BEHIND .. DIRECTORY)
    eq(state.notices[1].level, "warn")
    eq(#call.left_behind, 0)
    call:answer()
    eq(#state.notices, 1)
    call:leave(DIRECTORY)
    eq(#state.notices, 2)
  end)
end)

case("a_cancel_reports_a_directory_left_before_the_reply", function()
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS, function() end)
    maki.async.on_cancel(function()
      call:answer()
    end)
    call:leave(DIRECTORY)
    state.cancel[1]()
    eq(#state.notices, 1)
    has(state.notices[1].message, DIRECTORY)
    eq(#call.left_behind, 0)
  end)
end)

for _, code in ipairs({ SUCCESS_CODE, STOPPED_CODE }) do
  case("artifact_cleanup_reports_its_exit_" .. code, function()
    with_jobs(function(state)
      local releases = 0
      local call = jobs.new(STARTUP_MS, function()
        releases = releases + 1
      end)
      call:stop()
      local removed = 0
      snapshot.discard(call, { dir = DIRECTORY }, function()
        removed = removed + 1
      end)
      eq(call.alive, 1)
      eq(state.started[1].opts.guard, true)
      eq(state.stopped[1], nil)
      call:answer()
      eq(releases, 0)
      eq(#state.notices, 0)
      eq(removed, 0)
      state.started[1].opts.on_exit(1, code)
      eq(removed, 1)
      eq(releases, 1)
      eq(#state.notices, code == SUCCESS_CODE and 0 or 1)
      if code ~= SUCCESS_CODE then
        eq(state.notices[1].message, jobs.ERROR_PREFIX .. jobs.LEFT_BEHIND .. DIRECTORY)
      end
    end)
  end)
end

case("artifact_cleanup_reports_a_failed_start", function()
  with_jobs(function(state)
    state.start_error = CALLBACK_ERROR
    local call = jobs.new(STARTUP_MS)
    snapshot.discard(call, { dir = DIRECTORY })
    eq(call.alive, 0)
    eq(call.left_behind[1], DIRECTORY)
    call:answer()
    eq(#state.notices, 1)
    eq(state.notices[1].message, jobs.ERROR_PREFIX .. jobs.LEFT_BEHIND .. DIRECTORY)
  end)
end)

t.report()
