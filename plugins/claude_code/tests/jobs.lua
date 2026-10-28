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
local STDIN_LIMIT = 16
local STDIN_FULL = "fixture stdin full"
local STDIN_OVERSIZED = "fixture oversized write"
local DIRECTORY = "/probe/hook-output"
local SUCCESS_CODE = 0
local STOPPED_CODE = 137
local SWEEP_LOCK = ".sweep.lock"

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
      "chansend_limits",
      function()
        return { max_bytes = STDIN_LIMIT, full_error = STDIN_FULL, oversized_error = STDIN_OVERSIZED }
      end,
    },
    {
      maki.async,
      "sleep",
      function()
        state.waited = true
        state.full = false
      end,
    },
    {
      maki.fn,
      "chansend",
      function(_, data)
        if state.full then
          return nil, STDIN_FULL
        end
        if #data > STDIN_LIMIT then
          return nil, STDIN_OVERSIZED
        end
        state.input = (state.input or "") .. data
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

case("stderr_tail_caps_each_line_and_preserves_utf8", function()
  local tail = {}
  local long = string.rep("é", jobs.ERROR_BYTES)
  jobs.keep_tail(tail, long)
  assert(#tail[1] < #long)
  assert(utf8.len(tail[1]))
  has(tail[1], "truncated")
  for _ = 1, jobs.ERROR_BYTES do
    jobs.keep_tail(tail, OUTPUT)
  end
  eq(#tail, 10)
  eq(tail[1], OUTPUT)
end)

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

case("stdin_larger_than_the_queue_is_chunked_and_retried_without_losing_bytes", function()
  with_jobs(function(state)
    state.full = true
    local input = string.rep("a\0b\n", STDIN_LIMIT)
    local ok, err = jobs.feed_stdin(1, input)
    eq(ok, 1)
    eq(err, nil)
    eq(state.input, input)
    eq(state.waited, true)
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

case("a_sweep_keeps_an_active_coding_artifact_until_its_call_settles", function()
  local root = t.mktmpdir("cc_active_artifact")
  with_jobs(function(state)
    local call = jobs.new(STARTUP_MS)
    local artifact = assert(snapshot.allocate(call, root, {}, STARTUP_MS))
    local sweep = jobs.new(STARTUP_MS)
    snapshot.sweep(sweep, root, -1)
    eq(#state.started, 0)
    sweep:answer()
    call:answer()
    snapshot.sweep(jobs.new(STARTUP_MS), root, -1)
    eq(#state.started, 1)
    state.started[1].opts.on_exit(1, SUCCESS_CODE)
  end)
  t.rmtree(root)
end)

case("a_sweep_lock_error_is_reported", function()
  local root = t.mktmpdir("cc_sweep_error")
  local lock_path = maki.fs.joinpath(root, SWEEP_LOCK)
  assert(maki.fs.mkdir(lock_path))
  with_jobs(function(state)
    local _, lock_err = maki.fs.try_lock(lock_path)
    assert(lock_err)
    snapshot.sweep(jobs.new(STARTUP_MS), root, -1)
    eq(#state.started, 0)
    eq(#state.notices, 1)
    eq(state.notices[1].level, "warn")
    eq(state.notices[1].message, snapshot.CANNOT_SWEEP .. root .. ": " .. lock_err)
  end)
  t.rmtree(root)
end)

case("a_missing_artifact_root_needs_no_cleanup", function()
  local parent = t.mktmpdir("cc_sweep_missing")
  local root = maki.fs.joinpath(parent, "missing")
  with_jobs(function(state)
    snapshot.sweep(jobs.new(STARTUP_MS), root, -1)
    eq(#state.started, 0)
    eq(#state.notices, 0)
    eq(maki.fs.metadata(root), nil)
  end)
  t.rmtree(parent)
end)

for _, trashed in ipairs({ false, true }) do
  case("one_sweep_owns_a_tree_until_deletion_finishes_" .. tostring(trashed), function()
    local root = t.mktmpdir("cc_sweep")
    local artifact = maki.fs.joinpath(root, trashed and ".trash-old" or "old")
    assert(maki.fs.mkdir(artifact))
    assert(maki.fs.write(maki.fs.joinpath(artifact, ".maki-claude-code-artifact"), ""))
    with_jobs(function(state)
      local first, second = jobs.new(STARTUP_MS), jobs.new(STARTUP_MS)
      snapshot.sweep(first, root, -1)
      eq(#state.started, 1)
      -- The deletion command unlinks the artifact lock before exiting.
      local deleting = state.started[1].argv[5]
      assert(maki.fs.rm(maki.fs.joinpath(deleting, "import.lock")))
      snapshot.sweep(second, root, -1)
      eq(#state.started, 1)
      eq(#second.left_behind, 0)
      eq(#state.notices, 0)
      state.started[1].opts.on_exit(1, SUCCESS_CODE)
      snapshot.sweep(second, root, -1)
      eq(#state.started, 2)
      state.started[2].opts.on_exit(2, SUCCESS_CODE)
    end)
    t.rmtree(root)
  end)
end

t.report()
