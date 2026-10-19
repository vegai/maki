local native = require("maki.claude_code.internal")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local partial = require("maki.partial")
local truncate = require("maki.truncate")
local jobs = require("claude_jobs")
local launch = require("claude_launch")
local snapshot = require("claude_snapshot")
local Stream = require("claude_stream")

local DEFAULT_OUTPUT_LINES = 5
local BODY_INDENT_COLS = 4
local MIN_MD_WIDTH = 20
local ABANDONED = "The worker's snapshot stays in %s until maki removes it."
local CHECKING = "Validate the login and policy of Claude Code %s"
local ROUTE_OK = "claude.ai subscription%s on Claude Code %s, maki found no route conflict"
local ROUTE_UNCONFIRMED = "maki could not confirm the route on Claude Code %s"
local UNCHECKED_START = "Claude Code started its run before maki accepted its checks"
local ERROR_PREFIX = jobs.ERROR_PREFIX

local function body_width()
  return math.max(maki.ui.terminal_size().cols - BODY_INDENT_COLS, MIN_MD_WIDTH)
end

local function view_opts(ctx, keep)
  local tol = ctx:tool_output_lines()
  return {
    max_lines = (tol and tol.task) or DEFAULT_OUTPUT_LINES,
    keep = keep,
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  }
end

local function dim_lines(lines)
  local out = {}
  for i, line in ipairs(lines) do
    out[i] = { { line, "dim" } }
  end
  return out
end

-- `restore` rebuilds this from the saved header.
local function render(header, text, is_error, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, view_opts(ctx, "head"))
  view:set_header(dim_lines(header))
  local ok, md_lines = false, nil
  if not is_error then
    ok, md_lines = pcall(maki.ui.markdown, text, body_width())
  end
  for _, line in ipairs(ok and md_lines or maki.split(text, "\n")) do
    view:append(line)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

-- Job callbacks only record the result and hand it to the call. A body built
-- inside a job callback would lose its click handler when the callback's
-- scope ends.
local Run = {}
Run.__index = Run

function Run.new(ctx, call, spec, confine, timeout_secs, opts)
  local self = setmetatable({
    ctx = ctx,
    call = call,
    spec = spec,
    timeout_secs = timeout_secs,
    route = CHECKING:format(spec.cli.version),
    handshake = launch.worker_handshake(spec.deny, spec.worker, confine),
    stderr_tail = {},
    has_progress = false,
    -- Claude Code sends init after it reads the prompt. An earlier init means it bypassed the
    -- handshake.
    prompted = false,
    finished = false,
  }, Run)
  self.max_lines, self.max_bytes = output_limits.resolve(opts, ctx)
  local buf = maki.ui.buf()
  self.view = ToolView.new(buf, view_opts(ctx, "tail"))
  self.view:set_header(dim_lines(self:header_lines()))
  buf:on("click", function()
    self.view:toggle()
  end)
  ctx:live_buf(buf)
  return self
end

function Run:header_lines()
  local lines = { self.route }
  if #self.spec.withheld > 0 then
    lines[#lines + 1] = "not given to Claude: " .. table.concat(self.spec.withheld, ", ")
  end
  return lines
end

function Run:accounting()
  local lines = {}
  lines[#lines + 1] = self.stream:usage_line()
  lines[#lines + 1] = self.stream:limits_line()
  return lines
end

function Run:final_header()
  local lines = self:header_lines()
  for _, line in ipairs(self:accounting()) do
    lines[#lines + 1] = line
  end
  for _, denied in ipairs(self.stream:denials()) do
    lines[#lines + 1] = "denied: " .. denied
  end
  return lines
end

-- A failed call keeps no state, so its text carries the run's usage, which
-- a reloaded session then shows too.
function Run:with_spent(llm_output, is_error)
  local spent = is_error and self:accounting() or {}
  return #spent > 0 and llm_output .. "\n" .. table.concat(spent, "\n") or llm_output
end

-- {outcome} has `llm_output` in the output limits, `is_error`, and `shown`
-- for the body.
function Run:build(outcome)
  local lines = self:final_header()
  return {
    llm_output = self:with_spent(outcome.llm_output, outcome.is_error),
    is_error = outcome.is_error,
    body = render(lines, outcome.shown, outcome.is_error, self.ctx),
    state = { header = lines },
  }
end

function Run:finish(outcome)
  if self.finished then
    return
  end
  self.finished = true
  if self.clock then
    self.clock:stop()
  end
  self.resolve(outcome)
end

function Run:finish_with(text, is_error)
  self:finish({ llm_output = truncate(text, self.max_lines, self.max_bytes), is_error = is_error, shown = text })
end

-- The run ends after the process exits, so the usage it prints on the way
-- out still counts.
function Run:stop(reason)
  if self.finished or self.stopped then
    return
  end
  self.stopped = reason
  self.route = ROUTE_UNCONFIRMED:format(self.spec.cli.version)
  maki.fn.jobstop(self.job_id)
end

function Run:send(data)
  local sent, err = maki.fn.chansend(self.job_id, data)
  if not sent then
    self:stop("maki cannot write to Claude Code: " .. tostring(err))
  end
  return sent
end

function Run:on_answer()
  local model = self.handshake:resolved_model(self.spec.model)
  if not model then
    self:stop("Claude Code did not resolve the requested model " .. self.spec.model)
    return
  end
  self.stream:set_model(model)
  if self:send(launch.user_message(self.task, self.spec.instructions)) then
    self.prompted = true
    -- Keep the worker snapshot regardless of the result so the user can inspect its work.
    self.call.artifact = nil
    local closed, err = maki.fn.chanclose(self.job_id, "stdin")
    if not closed then
      self:stop("maki cannot close Claude Code stdin: " .. tostring(err))
    end
  end
end

function Run:on_line(line)
  if self.finished then
    return
  end
  local step, problem, complete = launch.checked_step(self.stream, line, self.handshake)
  if not step or self.stopped then
    return
  end
  if problem then
    if step.init then
      native.invalidate_version(self.spec.executable)
    end
    self:stop(problem)
  elseif step.control then
    if complete then
      self:on_answer()
    end
  elseif step.init and not self.prompted then
    self:stop(UNCHECKED_START)
  elseif step.init then
    self.clock:stop()
    local subscription = " (" .. self.handshake:subscription_type() .. ")"
    self.route = ROUTE_OK:format(subscription, self.spec.cli.version)
    self.view:set_header(dim_lines(self:header_lines()))
  elseif step.lines then
    if not self.has_progress then
      self.has_progress = true
      self.view:clear()
    end
    for _, progress in ipairs(step.lines) do
      self.view:append({ { progress, "dim" } })
    end
  end
end

function Run:on_exit(code)
  if self.finished then
    if self.artifact and self.prompted then
      local problem = snapshot.sanitize(self.artifact)
      if problem then
        maki.notify(ERROR_PREFIX .. "maki could not secure the retained snapshot: " .. problem, "error")
      end
    end
    return
  end
  if self.stopped then
    self:finish_with(
      jobs.with_stderr(ERROR_PREFIX .. "maki stopped Claude Code: " .. self.stopped, self.stderr_tail),
      true
    )
    return
  end
  if self.stream.result and code == 0 then
    self:finish_with(self.stream:outcome())
    return
  end
  local head = ERROR_PREFIX
    .. "Claude Code exited with code "
    .. code
    .. (self.stream.result and " after its result" or " before its result")
  self:finish_with(jobs.with_stderr(head, self.stderr_tail), true)
end

-- A reply that came before the stop stays on screen, but the run fails.
function Run:on_cancel(reason)
  if self.finished then
    return
  end
  self.finished = true
  local lines = self:final_header()
  self.view:set_header(dim_lines(lines))
  local answer = self.stream.result and truncate((self.stream:outcome()), self.max_lines, self.max_bytes) or ""
  local reply = partial.cut(self.view, answer, reason, self.timeout_secs)
  reply.llm_output = self:with_spent(reply.llm_output, true)
  if self.artifact and self.prompted then
    reply.llm_output = reply.llm_output .. "\n" .. ABANDONED:format(self.artifact.snapshot)
  end
  reply.state = { header = lines }
  self.ctx:finish(reply)
end

-- After a cancel, the hook registered here delivers the call's result.
function Run:start(argv, cwd, env, task)
  self.task = task
  self.stream = Stream.new({ cwd = cwd, worker = self.spec.worker, cli = self.spec.cli })
  local job_opts = launch.job_opts(cwd, env, self.stderr_tail)
  job_opts.on_stdout = function(_, line)
    self:on_line(line)
  end
  self.job_id = self.call:spawn(argv, job_opts, {
    on_exit = function(_, code)
      self:on_exit(code)
    end,
  })
  maki.async.on_cancel(function(reason)
    self:on_cancel(reason)
  end)
  self.clock = maki.defer_fn(function()
    if not self.stream.accepted then
      self:stop("no init event in " .. launch.STARTUP_TIMEOUT_SECS .. " s")
    end
  end, launch.STARTUP_MS)
  -- A send error would finish the call before the wait starts, so the
  -- handshake is sent from inside the wait.
  return maki.async.await(1, function(done)
    self.resolve = done
    for _, request in ipairs(launch.HANDSHAKE) do
      if not self:send(launch.control_request(request)) then
        return
      end
    end
  end)
end

Run.render = render

return Run
