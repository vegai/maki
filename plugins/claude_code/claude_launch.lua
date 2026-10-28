local native = require("maki.claude_code.internal")

-- Everything maki decides before Claude Code starts. The claude.ai
-- subscription is the only route. Pay-per-token users have maki's anthropic
-- provider.

local jobs = require("claude_jobs")
local Stream = require("claude_stream")
local path_utils = require("claude_paths")

local M = {}

M.MODELS = { "sonnet", "opus", "haiku", "fable" }
M.MODEL_SET = path_utils.set_of(M.MODELS)
M.STARTUP_TIMEOUT_SECS = native.startup_timeout_secs
M.STARTUP_MS = M.STARTUP_TIMEOUT_SECS * jobs.MS_PER_SEC
M.READ_PROFILE = "read"
M.CODE_PROFILE = "code"
M.DENY_HOME_OUTSIDE = "a deny_read_home path must stay inside the home directory"
M.NO_SESSION_DIR = "maki cannot find the session's working directory: "
local PROBE_RUN = "Claude Code started a run during its checks, before maki sent a prompt"

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

-- Claude Code passes its environment to Bash, in the sandbox too (tested on
-- 2.1.284), and the sandbox cannot drop a variable.
local SHELL_TOOL = "Bash"
-- Logins and keys under the home directory that the sandboxed shell cannot
-- read. Without `CLAUDE_CONFIG_DIR`, Claude Code keeps its login in
-- `.claude`, and its global state, MCP server `env` included, in
-- `.claude.json`. maki's own config can hold keys in `init.lua` and
-- `mcp.toml`.
local HOME_CREDENTIALS = {
  ".claude",
  ".claude.json",
  ".maki",
  ".config/maki",
  ".ssh",
  ".aws",
  ".gnupg",
  ".netrc",
  ".git-credentials",
  ".gitconfig",
  ".config/git",
  ".Xauthority",
  ".local/share/keyrings",
  ".password-store",
  ".config/containers/auth.json",
  ".m2/settings.xml",
  ".gradle/gradle.properties",
  ".terraform.d",
  ".azure",
  ".config/gh",
  ".config/gcloud",
  ".docker",
  ".kube",
  ".npmrc",
  ".pypirc",
  ".cargo/credentials.toml",
  ".cargo/credentials",
  ".bashrc",
  ".bash_profile",
  ".zshrc",
  ".zshenv",
  ".zprofile",
  ".profile",
  ".config/environment.d",
  ".bash_history",
  ".zsh_history",
  ".python_history",
  ".node_repl_history",
  ".mysql_history",
  ".psql_history",
  ".sqlite_history",
  ".viminfo",
  ".lesshst",
  ".config/fish",
  ".local/share/fish/fish_history",
  ".vault-token",
  ".pgpass",
  ".config/rclone",
  ".mozilla",
  ".config/google-chrome",
  ".config/chromium",
  ".config/BraveSoftware",
  ".config/microsoft-edge",
  ".var/app",
  "snap",
}

M.NO_HOME = "maki cannot find your home directory, so it cannot keep the coding worker's shell away from "
  .. "the logins and keys there. Set HOME, or use the read profile."

--- Returns why {worker} cannot run with {env}: its shell could read the
--- login token, or a proxy URL with a user or password in it. Names compare
--- in upper case, because `https_proxy` and the like reach the child too.
function M.shell_secret_problem(worker, env)
  if not path_utils.set_of(worker.tools)[SHELL_TOOL] then
    return nil
  end
  return native.shell_secret_problem(env)
end

function M.denied_home_paths(extra)
  return path_utils.comma_list(extra, function(path)
    if path:sub(1, 1) == "/" or path:sub(1, 1) == "~" or path_utils.climbs(path) then
      return nil, M.DENY_HOME_OUTSIDE
    end
    return path
  end)
end

--- Returns the logins and keys under {home} that a coding worker's shell
--- cannot read, or nil and the reason when there is no home directory.
function M.home_credentials(home, extra, environ)
  if not home or home == "" then
    return nil, M.NO_HOME
  end
  local cwd, cwd_err = maki.uv.cwd()
  if not cwd then
    return nil, "maki cannot resolve relocated credential paths: " .. tostring(cwd_err)
  end
  local paths, seen = {}, {}
  local resolved_home = maki.fs.realpath(home) or home
  local function add(path)
    if not seen[path] then
      seen[path] = true
      paths[#paths + 1] = path
    end
  end
  local function deny(path)
    add(maki.fs.joinpath(home, path))
    local absolute = maki.fs.joinpath(resolved_home, path)
    add(absolute)
    local resolved = maki.fs.realpath(absolute)
    if resolved then
      add(resolved)
    end
  end
  for _, path in ipairs(HOME_CREDENTIALS) do
    deny(path)
  end
  local state_files, state_err = native.home_state_files(home)
  if not state_files then
    return nil, "maki cannot inspect Claude Code state copies: " .. tostring(state_err)
  end
  add(maki.fs.joinpath(home, ".claude.json*"))
  add(maki.fs.joinpath(resolved_home, ".claude.json*"))
  for _, path in ipairs(state_files) do
    local resolved = maki.fs.realpath(maki.fs.joinpath(resolved_home, path))
    if resolved and not path_utils.within(resolved, resolved_home) then
      add(resolved)
    end
  end
  for _, path in ipairs(extra or {}) do
    deny(path)
  end
  local env = environ or {}
  local function relocated(path: string?)
    if path and path ~= "" then
      if path:sub(1, 1) ~= "/" then
        path = maki.fs.joinpath(cwd, path)
      end
      add(path)
      local resolved = maki.fs.realpath(path)
      if resolved then
        add(resolved)
      end
    end
  end
  for _, location in ipairs({ { "%.config", env.XDG_CONFIG_HOME }, { "%.local/share", env.XDG_DATA_HOME } }) do
    local prefix, directory = location[1], location[2]
    if directory and directory ~= "" then
      for _, path in ipairs(HOME_CREDENTIALS) do
        local relative = path:match("^" .. prefix .. "/(.+)$")
        if relative then
          relocated(maki.fs.joinpath(directory, relative))
        end
      end
    end
  end
  if env.CARGO_HOME and env.CARGO_HOME ~= "" then
    relocated(maki.fs.joinpath(env.CARGO_HOME, "credentials"))
    relocated(maki.fs.joinpath(env.CARGO_HOME, "credentials.toml"))
  end
  for _, name in ipairs({
    "GNUPGHOME",
    "DOCKER_CONFIG",
    "CLAUDE_CONFIG_DIR",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_CONFIG_FILE",
    "GH_CONFIG_DIR",
    "PASSWORD_STORE_DIR",
    "ZDOTDIR",
    "PGPASSFILE",
    "AZURE_CONFIG_DIR",
    "CLOUDSDK_CONFIG",
    "RCLONE_CONFIG",
  }) do
    relocated(env[name])
  end
  for path in (env.KUBECONFIG or ""):gmatch("[^:]+") do
    relocated(path)
  end
  return paths
end

--- Returns true if {path} is in the checkout of {spec}, which reaches past
--- the session's directory when that is a subdirectory or a worktree.
function M.in_checkout(path, spec)
  for _, dir in ipairs(spec.denied) do
    if path_utils.within(path, dir) then
      return true
    end
  end
  return false
end

function M.deny_rules(extra)
  local rules = {}
  for i, pattern in ipairs(extra) do
    rules[i] = "Read(./" .. pattern .. ")"
  end
  return rules
end

local function worker_policy(spec)
  if not spec.policy then
    spec.policy = native.worker_policy({
      deny = spec.deny,
      sandbox = spec.worker.sandboxed and assert(spec.confine, "a sandboxed worker needs confinement paths") or nil,
    })
  end
  return spec.policy
end

function M.worker_handshake(spec)
  return native.worker_handshake(spec.worker.reported_modes, worker_policy(spec))
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
  local args =
    native.worker_args(spec.model, worker.tools, worker.permission_mode, worker_policy(spec), worker.contract)
  table.insert(args, 1, spec.executable)
  return args
end

function M.checked_step(stream, line, handshake)
  local step = stream:feed(line)
  if not step then
    return nil
  end
  if step.stop then
    return step, step.stop
  end
  if step.control then
    local complete, problem = handshake:answer(step.control)
    return step, problem, complete
  end
  return step
end

local function probe_dir(env, spec)
  return native.private_dir(env.TMPDIR, spec.cwd, spec.denied)
end

function M.job_opts(cwd, env, stderr_tail)
  return {
    cwd = cwd,
    env = env,
    clear_env = true,
    stdin = "pipe",
    on_stderr = function(_, line)
      jobs.keep_tail(stderr_tail, line)
    end,
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
  local ok, code, _, run_error = pcall(call.run_to_end, call, argv, M.job_opts(dir, env, stderr_tail), {
    leftover = function()
      return dir
    end,
    on_line = function(id, line)
      if problem then
        return
      end
      local step, failure = M.checked_step(stream, line, handshake)
      problem = failure or step and not step.control and PROBE_RUN or nil
      if problem then
        maki.fn.jobstop(id)
      end
    end,
    started = function(id)
      return jobs.feed_stdin(id, M.HANDSHAKE_INPUT)
    end,
  }, M.STARTUP_MS)
  if not ok then
    call:remove_dir(dir)
    return "maki cannot start Claude Code: " .. tostring(code)
  end
  if problem then
    return jobs.with_stderr(problem, stderr_tail)
  end
  if not code then
    return "Claude Code did not send the responses to its checks in " .. M.STARTUP_TIMEOUT_SECS .. " s"
  end
  if code ~= 0 then
    return jobs.with_stderr("Claude Code exited with code " .. code .. " during its checks", stderr_tail)
  end
  local _, incomplete = handshake:finish()
  if incomplete then
    return jobs.with_stderr(incomplete, stderr_tail)
  end
  if run_error then
    return jobs.with_stderr("maki cannot run Claude Code checks: " .. run_error, stderr_tail)
  end
end

function M.preflight(input, ctx, call, opts)
  local model = input.model or opts.model
  local worker = M.WORKERS[input.profile or M.READ_PROFILE]
  local environ = maki.uv.os_environ()
  local home = maki.uv.os_homedir()
  local plan_err, home_credentials
  if worker == M.WORKERS[M.CODE_PROFILE] then
    home_credentials, plan_err = M.home_credentials(home, opts.deny_read_home, environ)
    if not home_credentials then
      return nil, plan_err
    end
  elseif input.include and #input.include > 0 then
    return nil, "only the " .. M.CODE_PROFILE .. " profile can use `include`, because it works in a snapshot"
  end
  -- Resolved once here, because the probe runs from another directory.
  local executable = maki.fn.exepath(opts.executable)
  if executable == "" then
    return nil, "Claude Code (`" .. opts.executable .. "`) is not installed, or it is not on PATH"
  end

  -- Under ACP, the session can work in a different checkout from maki's.
  local cwd, cwd_err = ctx:cwd()
  if not cwd then
    return nil, M.NO_SESSION_DIR .. tostring(cwd_err)
  end
  local instructions, instructions_err = ctx:instructions()
  if not instructions then
    return nil, "maki cannot load the instructions of the session: " .. tostring(instructions_err)
  end
  local prepared, prepare_err = native.prepare_launch(environ, opts.config_dir ~= "" and opts.config_dir or nil, cwd)
  if not prepared then
    return nil, prepare_err
  end
  local env, withheld = prepared.env, prepared.withheld
  local secret_err = M.shell_secret_problem(worker, env)
  if secret_err then
    return nil, secret_err
  end
  local config_dir = prepared.config_dir

  -- Keep the version job in Call so cancellation reaps it before the slot is released.
  local check, version_err = native.version_check(executable)
  if not check then
    return nil, version_err
  end
  local version = check:version()
  if not version then
    local dir, dir_err = probe_dir(env, { cwd = cwd, denied = prepared.denied })
    if not dir then
      return nil, dir_err
    end
    local output, output_err = call:run_quick({ executable, "--version" }, {
      cwd = dir,
      env = env,
      leftover = function()
        return dir
      end,
    })
    if not output then
      -- A failed spawn has no exit callback to remove the directory.
      call:remove_dir(dir)
      return nil, output_err
    end
    version, version_err = check:save(output)
  end
  if not version then
    return nil, version_err
  end
  local cli: { version: string } = { version = version }
  return {
    model = model,
    worker = worker,
    deny = M.deny_rules(opts.deny_read),
    executable = executable,
    cwd = cwd,
    instructions = instructions,
    env = env,
    withheld = withheld,
    config_dir = config_dir,
    home_credentials = home_credentials,
    denied = prepared.denied,
    cli = cli,
  }
end

return M
