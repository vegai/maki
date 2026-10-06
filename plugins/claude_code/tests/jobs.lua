local t = require("maki.test_helpers")
local jobs = require("claude_jobs")

local case, eq, has = t.case, t.eq, t.has
local STARTUP_MS = 30000
local COMMAND = { "cat" }
local OUTPUT = "complete"
local CALLBACK_ERROR = "exit callback failed"
local SEND_ERROR = "stdin send failed"
local CLOSE_ERROR = "stdin close failed"
local CLOCK_ERROR = "the timeout clock exited with code 137"
local DIRECTORY = "/probe/hook-output"
local SUCCESS_CODE = 0
local STOPPED_CODE = 137

local function with_jobs(run)
  local state = { started = {}, stopped = {}, notices = {}, cancel = {}, closes = 0 }
  local overrides = {
    {
      maki.fn,
      "jobstart",
      function(argv, opts)
        state.started[#state.started + 1] = { argv = argv, opts = opts }
        return #state.started
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

for _, code in ipairs({ SUCCESS_CODE, STOPPED_CODE }) do
  case("a_clock_exit_stops_the_job_and_reports_code_" .. code, function()
    with_jobs(function(state)
      local call = jobs.new(STARTUP_MS, function() end)
      state.finish = function()
        state.started[1].opts.on_exit(1, code)
        eq(state.stopped[2], true)
        state.started[2].opts.on_exit(2, STOPPED_CODE)
      end
      local output, err, timed_out = call:run_quick(COMMAND, {})
      eq(output, nil)
      if code == SUCCESS_CODE then
        has(err, "did not complete in time")
        eq(timed_out, true)
      else
        has(err, CLOCK_ERROR)
        eq(timed_out, nil)
      end
    end)
  end)
end

case("a_stopped_clock_cannot_change_a_completed_job", function()
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS, function() end)
    state.finish = function()
      state.started[2].opts.on_stdout(2, OUTPUT)
      state.started[2].opts.on_exit(2, SUCCESS_CODE)
      state.started[1].opts.on_exit(1, STOPPED_CODE)
    end
    local output, err = call:run_quick(COMMAND, {})
    eq(output, OUTPUT)
    eq(err, nil)
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
        eq(state.stopped[2], true)
        state.started[2].opts.on_exit(2, SUCCESS_CODE)
      end
      local output, err = call:run_quick(COMMAND, { stdin = OUTPUT })
      eq(output, nil)
      has(err, failure == "send" and SEND_ERROR or CLOSE_ERROR)
      eq(state.closes, failure == "send" and 0 or 1)
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

t.report()
