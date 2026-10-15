local native = require("maki.claude_code.internal")

-- Everything maki decides before Claude Code starts. The claude.ai
-- subscription is the only route. Pay-per-token users have maki's anthropic
-- provider.

local jobs = require("claude_jobs")
local Stream = require("claude_stream")
local workspace = require("claude_workspace")

local M = {}

M.MODELS = { "sonnet", "opus", "haiku", "fable" }
M.STARTUP_TIMEOUT_SECS = 30
M.STARTUP_MS = M.STARTUP_TIMEOUT_SECS * jobs.MS_PER_SEC
local PROBE_DIR_TEMPLATE = "maki-claude-code.XXXXXX"
local DEFAULT_TMP_DIR = "/tmp"
M.READ_PROFILE = "read"
M.CODE_PROFILE = "code"
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
    reported_modes = { default = true, manual = true },
    sandboxed = false,
    contract = READ_CONTRACT,
  },
  code = {
    tools = { "Read", "Glob", "Grep", "Edit", "Write", "Bash" },
    permission_mode = "acceptEdits",
    reported_modes = { acceptEdits = true },
    sandboxed = true,
    contract = CODE_CONTRACT,
  },
}

--- `ipairs` skips object entries. Require a JSON array so an object cannot bypass list validation.
function M.is_list(value)
  if type(value) ~= "table" then
    return false
  end
  if next(value) == nil and getmetatable(value) == nil then
    return false
  end
  local count = 0
  for _ in pairs(value) do
    count = count + 1
  end
  for key in pairs(value) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > count then
      return false
    end
  end
  return true
end

function M.set_of(list)
  local set = {}
  for _, item in ipairs(M.is_list(list) and list or {}) do
    set[item] = true
  end
  return set
end

-- Limits for a call's `timeout` and for the option.
M.MIN_TIMEOUT_SECS = 30
M.MAX_TIMEOUT_SECS = 1800
-- The process that runs the task answers these requests, so the answers
-- cover every settings source it merged, managed policy included.
M.HANDSHAKE = native.handshake()

local EMPTY_MCP_CONFIG = native.empty_mcp_config()
local INSTRUCTIONS_HEADING = "The instructions maki loaded for this task, from the project and the user:"

-- Claude Code passes its environment to Bash, in the sandbox too (tested on
-- 2.1.284), and the sandbox cannot drop a variable.
local SHELL_TOOL = "Bash"
local SHELL_VISIBLE_LOGIN = "CLAUDE_CODE_OAUTH_TOKEN"
-- A proxy URL can carry a user and a password that Claude Code needs to
-- reach the API, so maki cannot strip them.
local PROXY_ENV = { HTTP_PROXY = true, HTTPS_PROXY = true }

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

local NO_HOME = "maki cannot find your home directory, so it cannot keep the coding worker's shell away from "
  .. "the logins and keys there. Set HOME, or use the read profile."

local function sorted_keys(t)
  local keys = {}
  for key in pairs(t) do
    keys[#keys + 1] = key
  end
  table.sort(keys)
  return keys
end

--- Returns why {worker} cannot run with {env}: its shell could read the
--- login token, or a proxy URL with a user or password in it. Names compare
--- in upper case, because `https_proxy` and the like reach the child too.
function M.shell_secret_problem(worker, env)
  if not M.set_of(worker.tools)[SHELL_TOOL] then
    return nil
  end
  for name, value in pairs(env) do
    if name:upper() == SHELL_VISIBLE_LOGIN then
      return "Claude Code passes "
        .. name
        .. " to its shell, where a coding worker can read it. Remove the variable and log in with "
        .. "`claude auth login`, or use the read profile."
    end
    local authority = PROXY_ENV[name:upper()] and value:gsub("^%a[%w+.-]*://", ""):match("^[^/]*")
    if authority and authority:find("@", 1, true) then
      return "Claude Code passes "
        .. name
        .. " to its shell, where a coding worker can read the login in its URL. Use a proxy without a "
        .. "login in the URL, or use the read profile."
    end
  end
  return nil
end

function M.denied_home_paths(extra)
  local paths = {}
  for path in (extra or ""):gmatch("[^,]+") do
    path = workspace.trim(path)
    if path:sub(1, 1) == "/" or path:sub(1, 1) == "~" or ("/" .. path .. "/"):find("/../", 1, true) then
      return nil, "a deny_read_home path must stay inside the home directory"
    end
    if path ~= "" then
      paths[#paths + 1] = path
    end
  end
  return paths
end

--- Returns the logins and keys under {home} that a coding worker's shell
--- cannot read, or nil and the reason when there is no home directory.
function M.home_credentials(home, extra, environ)
  if not home or home == "" then
    return nil, NO_HOME
  end
  local cwd, cwd_err = maki.uv.cwd()
  if not cwd then
    return nil, "maki cannot resolve relocated credential paths: " .. tostring(cwd_err)
  end
  local paths, seen = {}, {}
  local resolved_home = maki.uv.fs_realpath(home) or home
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
    local resolved = maki.uv.fs_realpath(absolute)
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
    local resolved = maki.uv.fs_realpath(maki.fs.joinpath(resolved_home, path))
    if resolved and not workspace.within(resolved, resolved_home) then
      add(resolved)
    end
  end
  local extras, paths_err = M.denied_home_paths(extra)
  if not extras then
    return nil, paths_err
  end
  for _, path in ipairs(extras) do
    deny(path)
  end
  local env = environ or {}
  local function relocated(path)
    if path and path ~= "" then
      if path:sub(1, 1) ~= "/" then
        path = maki.fs.joinpath(cwd, path)
      end
      add(path)
      local resolved = maki.uv.fs_realpath(path)
      if resolved then
        add(resolved)
      end
    end
  end
  if env.XDG_CONFIG_HOME and env.XDG_CONFIG_HOME ~= "" then
    for _, path in ipairs(HOME_CREDENTIALS) do
      local relative = path:match("^%.config/(.+)$")
      if relative then
        relocated(maki.fs.joinpath(env.XDG_CONFIG_HOME, relative))
      end
    end
  end
  if env.CARGO_HOME and env.CARGO_HOME ~= "" then
    relocated(maki.fs.joinpath(env.CARGO_HOME, "credentials"))
    relocated(maki.fs.joinpath(env.CARGO_HOME, "credentials.toml"))
  end
  relocated(env.GNUPGHOME)
  relocated(env.DOCKER_CONFIG)
  for path in (env.KUBECONFIG or ""):gmatch("[^:]+") do
    relocated(path)
  end
  return paths
end

--- Returns true if {path} is in the checkout of {spec}, which reaches past
--- the session's directory when that is a subdirectory or a worktree.
function M.in_checkout(path, spec)
  for _, dir in ipairs(spec.checkout_dirs) do
    if workspace.within(path, dir) then
      return true
    end
  end
  return spec.git_dir ~= nil and workspace.within(path, spec.git_dir)
end

local function json(value)
  return type(value) == "string" and value or maki.json.encode(value)
end

function M.resolved_model(account, requested)
  return native.resolved_model(json(account), requested)
end

function M.account_problem(init, worker)
  return native.account_problem(json(init), sorted_keys(worker.reported_modes))
end

local function deny_rules(extra)
  local rules = {}
  for i, pattern in ipairs(assert(workspace.denied_paths(extra))) do
    rules[i] = "Read(./" .. pattern .. ")"
  end
  return rules
end

--- Only maki settings and safe managed restrictions can pass. The worker shell must also
--- remain inside `confine`.
function M.policy_problem(settings, hooks, deny_read, worker, confine)
  return native.policy_problem(json(settings), json(hooks), maki.json.encode(M.settings(deny_read, worker, confine)))
end

--- The keys of {answers} are the request ids.
function M.handshake_problem(answers, deny_read, worker, confine)
  for _, request in ipairs(M.HANDSHAKE) do
    if answers[request.id] == nil then
      return "Claude Code did not send a response to the " .. request.id .. " request"
    end
  end
  return M.account_problem(answers.account, worker)
    or M.policy_problem(answers.settings, answers.hooks, deny_read, worker, confine)
end

--- `--setting-sources ""` drops the user's hooks, permissions and MCP
--- servers, and these settings turn off what Claude Code loads on its own. A
--- {worker} that runs commands gets the sandbox with {confine}'s absolute
--- paths, and Bash runs only inside it.
function M.settings(deny_read, worker, confine)
  local settings = native.base_settings()
  settings.permissions = {
    blockReadsOutsideWorkingDirectories = true,
    deny = deny_rules(deny_read),
  }
  if worker.sandboxed then
    settings.sandbox = {
      enabled = true,
      failIfUnavailable = true,
      autoAllowBashIfSandboxed = true,
      allowUnsandboxedCommands = false,
      -- A table from `decode` keeps its array mark, so `encode` writes `[]`.
      network = { allowedDomains = maki.json.decode("[]"), strictAllowlist = true },
      filesystem = { denyRead = confine.deny_read, allowRead = confine.allow_read, allowWrite = confine.allow_write },
    }
  end
  return settings
end

--- Returns how many whole seconds a call can run: {requested} or
--- {configured}, clamped to the limits.
function M.task_timeout(requested, configured)
  return math.floor(math.clamp(requested or configured, M.MIN_TIMEOUT_SECS, M.MAX_TIMEOUT_SECS))
end

function M.control_request(request)
  return maki.json.encode({
    type = "control_request",
    request_id = request.id,
    request = { subtype = request.subtype },
  }) .. "\n"
end

--- Sent on stdin, so the process list never shows the task.
--- {instructions} come first, because Claude Code loads none by itself.
function M.user_message(prompt, instructions)
  local content = prompt
  if instructions and instructions ~= "" then
    content = {
      { type = "text", text = INSTRUCTIONS_HEADING .. instructions },
      { type = "text", text = prompt },
    }
  end
  return maki.json.encode({ type = "user", message = { role = "user", content = content } }) .. "\n"
end

--- {spec} has the executable, model, deny_read and worker, plus `confine`
--- for a sandboxed worker.
function M.argv(spec)
  local worker = spec.worker
  local args = native.base_args(
    spec.model,
    table.concat(worker.tools, ","),
    worker.permission_mode,
    maki.json.encode(M.settings(spec.deny_read, worker, spec.confine))
  )
  table.insert(args, 1, spec.executable)
  local extra = {
    "--mcp-config",
    EMPTY_MCP_CONFIG,
    "--append-system-prompt",
    worker.contract,
  }
  table.move(extra, 1, #extra, #args + 1, args)
  return args
end

-- Returns why the responses stop the run, or nil, and whether all responses
-- are in.
function M.take_answer(answers, control, worker, confine, deny_read)
  local asked = false
  for _, request in ipairs(M.HANDSHAKE) do
    asked = asked or request.id == control.id
  end
  -- A second or unknown response would run the checks again, and the prompt
  -- could go out twice.
  if not asked or answers[control.id] ~= nil then
    return "Claude Code answered a " .. tostring(control.id) .. " request that maki never sent, or answered it twice",
      false
  end
  if not control.ok then
    return "Claude Code rejected the " .. tostring(control.id) .. " request: " .. tostring(control.error), false
  end
  answers[control.id] = control.json or control.response or false
  for _, request in ipairs(M.HANDSHAKE) do
    if answers[request.id] == nil then
      return nil, false
    end
  end
  return M.handshake_problem(answers, deny_read, worker, confine), true
end

function M.checked_step(stream, line, answers, spec, confine, deny_read)
  local step = stream:feed(line)
  if not step then
    return nil
  end
  if step.stop then
    return step, step.stop
  end
  if step.control then
    local problem, complete = M.take_answer(answers, step.control, spec.worker, confine, deny_read)
    return step, problem, complete
  end
  return step
end

-- A resolved TMPDIR must stay outside the project.
local function probe_dir(env, spec)
  local base, base_err = maki.uv.fs_realpath(env.TMPDIR or DEFAULT_TMP_DIR)
  if not base then
    return nil, base_err
  end
  if M.in_checkout(base, spec) then
    return nil, "the temporary directory " .. base .. " is in the project. Set TMPDIR to a different directory."
  end
  local created, err = maki.uv.fs_mkdtemp(maki.fs.joinpath(base, PROBE_DIR_TEMPLATE))
  if not created then
    return nil, err
  end
  if M.in_checkout(created, spec) or workspace.within(spec.cwd, created) then
    maki.uv.fs_rmdir(created)
    return nil, "the probe directory " .. created .. " is in the project, or contains it"
  end
  return created
end

function M.job_opts(cwd, env, stderr_tail)
  return {
    cwd = cwd,
    env = env,
    clear_env = true,
    stdin = "pipe",
    guard = true,
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
function M.probe(call, spec, argv, env, confine, deny_read)
  local dir, dir_err = probe_dir(spec.env, spec)
  if not dir then
    return dir_err
  end
  local stream = Stream.new({ cwd = dir, worker = spec.worker, cli = spec.cli })
  local answers, problem, stderr_tail = {}, nil, {}
  local ok, code, _, run_error = pcall(call.run_to_end, call, argv, M.job_opts(dir, env, stderr_tail), {
    leftover = function()
      return dir
    end,
    on_line = function(id, line)
      if problem then
        return
      end
      local step, failure = M.checked_step(stream, line, answers, spec, confine, deny_read)
      problem = failure or step and not step.control and PROBE_RUN or nil
      if problem then
        maki.fn.jobstop(id)
      end
    end,
    started = function(id)
      local requests = {}
      for _, request in ipairs(M.HANDSHAKE) do
        requests[#requests + 1] = M.control_request(request)
      end
      return jobs.feed_stdin(id, table.concat(requests))
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
  local incomplete = M.handshake_problem(answers, deny_read, spec.worker, confine)
  if incomplete then
    return jobs.with_stderr(incomplete, stderr_tail)
  end
  if run_error then
    return jobs.with_stderr("maki cannot run Claude Code checks: " .. run_error, stderr_tail)
  end
end

-- Only a coding call has a `plan`.
function M.preflight(input, ctx, call, opts, plan)
  local model = input.model or opts.model
  if not M.set_of(M.MODELS)[model] then
    return nil, "unknown model " .. tostring(model) .. ", use one of " .. table.concat(M.MODELS, ", ")
  end
  local worker = M.WORKERS[input.profile or M.READ_PROFILE]
  if not worker then
    return nil, "unknown profile " .. tostring(input.profile) .. ", use " .. M.READ_PROFILE .. " or " .. M.CODE_PROFILE
  end
  local home = maki.uv.os_homedir()
  local plan_err, home_credentials
  if worker == M.WORKERS[M.CODE_PROFILE] then
    home_credentials, plan_err = M.home_credentials(home, opts.deny_read_home, maki.uv.os_environ())
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
  local prepared, prepare_err =
    native.prepare_launch(maki.uv.os_environ(), opts.config_dir ~= "" and opts.config_dir or nil, cwd)
  if not prepared then
    return nil, prepare_err
  end
  local env, withheld = prepared.env, prepared.withheld
  local secret_err = M.shell_secret_problem(worker, env)
  if secret_err then
    return nil, secret_err
  end
  local config_dir, own_config_dir = prepared.config_dir, prepared.own_config_dir
  local local_dirs, git_dir = prepared.dirs, prepared.git_dir

  -- Keep the version job in Call so cancellation reaps it before the slot is released.
  local check, version_err = native.version_check(executable)
  if not check then
    return nil, version_err
  end
  local version = check:version()
  if not version then
    local dir, dir_err = probe_dir(env, { cwd = cwd, checkout_dirs = local_dirs, git_dir = git_dir })
    if not dir then
      return nil, dir_err
    end
    local output, output_err = call:run_quick({ executable, "--version" }, { cwd = dir, env = env })
    call:remove_dir(dir)
    if not output then
      return nil, output_err
    end
    version, version_err = check:save(output, maki.uv.os_uname().sysname)
  end
  if not version then
    return nil, version_err
  end
  local cli = { version = version }
  return {
    model = model,
    worker = worker,
    plan = plan,
    executable = executable,
    cwd = cwd,
    instructions = instructions,
    env = env,
    withheld = withheld,
    config_dir = config_dir,
    own_config_dir = own_config_dir and own_config_dir:sub(1, 1) == "/" and own_config_dir or nil,
    home_credentials = home_credentials,
    checkout_dirs = local_dirs,
    git_dir = git_dir,
    cli = cli,
  }
end

return M
