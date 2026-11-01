local native = require("maki.claude_code.internal")

-- Everything maki decides before Claude Code starts. The claude.ai
-- subscription is the only route. Pay-per-token users have maki's anthropic
-- provider.

local jobs = require("claude_jobs")
local Stream = require("claude_stream")
local path_utils = require("claude_paths")
local confine = require("claude_confine")

local M = {}

M.MODELS = { "sonnet", "opus", "haiku", "fable" }
M.MODEL_SET = path_utils.set_of(M.MODELS)
M.STARTUP_TIMEOUT_SECS = native.startup_timeout_secs
M.STARTUP_MS = M.STARTUP_TIMEOUT_SECS * jobs.MS_PER_SEC
M.READ_PROFILE = "read"
M.CODE_PROFILE = "code"
M.NO_SESSION_DIR = "maki cannot find the session's working directory: "

local READ_CONTRACT = "You are a read-only worker for another coding agent. You can read and search the "
  .. "project, but you cannot edit files or run commands. Reply only with your results. Keep them short, with "
  .. "file:line locations. If the request needs a tool you do not have, say so clearly."
local CODE_CONTRACT = "You are a coding worker for another coding agent. You work in a private copy of the "
  .. "project. Make the task's changes there, and run the commands that check them. Its git repository is "
  .. "read-only to you: git can show and diff, but it cannot add, commit or stash. The other agent reviews your "
  .. "changes before it applies them to its own checkout, which you cannot change. Reply with a short summary "
  .. "of your changes and how you checked them. If a check could not run, say so clearly."

-- `read` searches the live checkout. `code` works in a private snapshot:
-- permissions keep its edits there, and Claude Code's OS sandbox keeps its
-- shell there with no network. The `manual` flag starts the mode that Claude
-- Code reports as `default`.
M.WORKERS = {
  read = {
    tools = { "Read", "Glob", "Grep" },
    permission_mode = "manual",
    reported_modes = { "default", "manual" },
    sandboxed = false,
    contract = READ_CONTRACT,
  },
  code = {
    tools = { "Read", "Glob", "Grep", "Edit", "Write", "Bash" },
    permission_mode = "acceptEdits",
    reported_modes = { "acceptEdits" },
    sandboxed = true,
    contract = CODE_CONTRACT,
  },
}

-- Limits for a call's `timeout` and for the option.
M.MIN_TIMEOUT_SECS = 30
M.MAX_TIMEOUT_SECS = 1800
-- The process that runs the task answers these requests, so the answers
-- cover every settings source it merged, managed policy included.
M.HANDSHAKE_INPUT = native.handshake_input

local INSTRUCTIONS_HEADING = "The instructions maki loaded for this task, from the project and the user:"

function M.worker_handshake(spec)
  return native.worker_handshake(spec.worker.reported_modes, spec.policy)
end

--- Returns how many whole seconds a call can run: {requested} or
--- {configured}, clamped to the limits.
function M.task_timeout(requested, configured)
  return math.floor(math.clamp(requested or configured, M.MIN_TIMEOUT_SECS, M.MAX_TIMEOUT_SECS))
end

--- Sent on stdin, so the process list never shows the task.
--- {instructions} come first, because Claude Code loads none by itself.
function M.user_message(prompt, instructions)
  local blocks = {}
  if instructions and instructions ~= "" then
    blocks[#blocks + 1] = INSTRUCTIONS_HEADING .. instructions
  end
  blocks[#blocks + 1] = prompt
  return native.user_message(blocks)
end

--- {spec} has the executable, model, deny and worker, plus `confine`
--- for a sandboxed worker.
function M.argv(spec)
  local worker = spec.worker
  local args = native.worker_args(spec.model, worker.tools, worker.permission_mode, spec.policy, worker.contract)
  table.insert(args, 1, spec.executable)
  return args
end

function M.checked_step(stream, line, handshake)
  local step = stream:feed(line, handshake)
  if not step then
    return nil
  end
  if step.stop then
    return step, step.stop
  end
  if step.control then
    return step, nil, step.complete
  end
  return step
end

local function probe_dir(env, spec)
  return native.private_dir(env.TMPDIR, spec.cwd, spec.denied)
end

function M.job_opts(cwd, env)
  return {
    max_line_bytes = native.max_event_bytes,
    cwd = cwd,
    env = env,
    clear_env = true,
    stdin = "pipe",
  }
end

-- A failed check must stop the probe immediately. Otherwise a stalled probe can replace the
-- refusal with a timeout.
--
-- Keep the probe outside a coding artifact. Refusal removes that artifact, but a managed
-- hook's output must remain available.
function M.probe(call, spec, argv, env)
  local dir, dir_err = probe_dir(spec.env, spec)
  if not dir then
    return dir_err
  end
  local stream = Stream.new({ cwd = dir, worker = spec.worker, cli = spec.cli })
  local handshake = M.worker_handshake(spec)
  local problem, stderr_tail = nil, {}
  local code, _, run_error = call:run_to_end(argv, M.job_opts(dir, env), {
    timeout_ms = M.STARTUP_MS,
    on_stderr_tail = function(tail)
      stderr_tail = tail
    end,
    leftover = function()
      return dir
    end,
    on_line = function(id, line)
      if problem then
        return
      end
      local step, failure = M.checked_step(stream, line, handshake)
      problem = failure or step and not step.control and native.unchecked_start() or nil
      if problem then
        maki.fn.jobstop(id)
      end
    end,
    started = function(id)
      return jobs.feed_stdin(id, M.HANDSHAKE_INPUT)
    end,
  })
  if run_error then
    return jobs.with_stderr("maki cannot run Claude Code checks: " .. run_error, stderr_tail)
  end
  if problem then
    return jobs.with_stderr(problem, stderr_tail)
  end
  local _, incomplete = handshake:probe_exit(code, M.STARTUP_TIMEOUT_SECS)
  if incomplete then
    return jobs.with_stderr(incomplete, stderr_tail)
  end
end

function M.preflight(input, ctx, call, opts)
  local model = input.model or opts.model
  local worker = M.WORKERS[input.profile or M.READ_PROFILE]
  local environ = maki.uv.os_environ()
  local home = maki.uv.os_homedir()
  local plan_err, home_credentials
  if worker == M.WORKERS[M.CODE_PROFILE] then
    home_credentials, plan_err = confine.home_credentials(home, opts.deny_read_home, environ)
    if not home_credentials then
      return nil, plan_err
    end
  elseif input.include and #input.include > 0 then
    return nil, "only the " .. M.CODE_PROFILE .. " profile can use `include`, because it works in a snapshot"
  end
  -- Resolved once here, because the probe runs from another directory.
  local executable, executable_err = native.executable(opts.executable)
  if not executable then
    return nil, executable_err
  end

  -- Under ACP, the session can work in a different checkout from maki's.
  local cwd, cwd_err = maki.agent.cwd(ctx)
  if not cwd then
    return nil, M.NO_SESSION_DIR .. tostring(cwd_err)
  end
  local instructions, instructions_err = maki.agent.instructions(ctx)
  if not instructions then
    return nil, "maki cannot load the instructions of the session: " .. tostring(instructions_err)
  end
  local prepared, prepare_err = native.prepare_launch(environ, opts.config_dir ~= "" and opts.config_dir or nil, cwd)
  if not prepared then
    return nil, prepare_err
  end
  local env, withheld = prepared.env, prepared.withheld
  local secret_err = confine.shell_secret_problem(worker, env)
  if secret_err then
    return nil, secret_err
  end
  local config_dir = prepared.config_dir

  local version, version_err =
    call:wait(native.checked_profile, executable, env, cwd, prepared.denied, M.STARTUP_TIMEOUT_SECS)
  if not version then
    return nil, version_err
  end
  local cli: { version: string } = { version = version }
  return {
    model = model,
    worker = worker,
    deny = confine.deny_rules(opts.deny_read),
    executable = executable,
    cwd = cwd,
    instructions = instructions,
    env = env,
    withheld = withheld,
    config_dir = config_dir,
    home_credentials = home_credentials,
    denied = prepared.denied,
    cli = cli,
    environ = environ,
  }
end

return M
