local native = require("maki.claude_code.internal")

-- Everything maki decides before Claude Code starts. The claude.ai
-- subscription is the only route. Pay-per-token users have maki's anthropic
-- provider.

local jobs = require("claude_jobs")
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

-- Limits for a call's `timeout` and for the option.
M.MIN_TIMEOUT_SECS = 30
M.MAX_TIMEOUT_SECS = 1800
-- The process that runs the task answers these requests, so the answers
-- cover every settings source it merged, managed policy included.
M.HANDSHAKE_INPUT = native.handshake_input

local INSTRUCTIONS_HEADING = "The instructions maki loaded for this task, from the project and the user:"

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

function M.job_opts(cwd, env)
  return {
    max_line_bytes = native.max_event_bytes,
    stop_on_line_limit = true,
    cwd = cwd,
    env = env,
    clear_env = true,
    stdin = "pipe",
  }
end

function M.preflight(input, ctx, call, opts)
  local model = input.model or opts.model
  local worker = input.profile or M.READ_PROFILE
  local environ = maki.uv.os_environ()
  local home = maki.uv.os_homedir()
  local plan_err, home_credentials
  if worker == M.CODE_PROFILE then
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
  local secret_err = worker == M.CODE_PROFILE and native.shell_secret_problem(env) or nil
  if secret_err then
    return nil, secret_err
  end
  local config_dir = prepared.config_dir

  local version, version_err = call:wait(native.checked_profile, ctx, executable, prepared, cwd, M.STARTUP_TIMEOUT_SECS)
  if not version then
    return nil, version_err
  end
  return {
    model = model,
    deny = confine.deny_rules(opts.deny_read),
    executable = executable,
    cwd = cwd,
    instructions = instructions,
    env = env,
    withheld = withheld,
    config_dir = config_dir,
    home_credentials = home_credentials,
    denied = prepared.denied,
    temp_dir = prepared.temp_dir,
    version = version,
    environ = environ,
  }
end

return M
