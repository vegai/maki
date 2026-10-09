local native = require("maki.claude_code.internal")

-- Everything maki decides before Claude Code starts. The claude.ai
-- subscription is the only route. Pay-per-token users have maki's anthropic
-- provider.

local M = {}

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

local STREAM_JSON = "stream-json"
local EMPTY_MCP_CONFIG = '{"mcpServers":{}}'
local INSTRUCTIONS_HEADING = "The instructions maki loaded for this task, from the project and the user:"

-- Claude Code passes its environment to Bash, in the sandbox too (tested on
-- 2.1.284), and the sandbox cannot drop a variable.
local SHELL_TOOL = "Bash"
local SHELL_VISIBLE_LOGIN = "CLAUDE_CODE_OAUTH_TOKEN"
-- A proxy URL can carry a user and a password that Claude Code needs to
-- reach the API, so maki cannot strip them.
local PROXY_ENV = { HTTP_PROXY = true, HTTPS_PROXY = true }

local DEFAULT_DENY = { ".env*", "**/.env*", "secrets/**", "**/secrets/**" }
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
  ".config/git/credentials",
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
  ".bash_history",
  ".zsh_history",
  ".local/share/fish/fish_history",
  ".vault-token",
  ".pgpass",
  ".config/rclone",
  ".mozilla",
  ".config/google-chrome",
  ".config/chromium",
  ".config/BraveSoftware",
  ".config/microsoft-edge",
  ".var/app/org.mozilla.firefox",
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

--- Returns the logins and keys under {home} that a coding worker's shell
--- cannot read, or nil and the reason when there is no home directory.
function M.home_credentials(home, extra)
  if not home or home == "" then
    return nil, NO_HOME
  end
  local paths = {}
  for i, path in ipairs(HOME_CREDENTIALS) do
    paths[i] = maki.fs.joinpath(home, path)
  end
  for path in (extra or ""):gmatch("[^,]+") do
    path = M.trim(path)
    if path:sub(1, 1) == "/" or ("/" .. path .. "/"):find("/../", 1, true) then
      return nil, "a deny_read_home path must stay inside the home directory"
    end
    if path ~= "" then
      paths[#paths + 1] = maki.fs.joinpath(home, path)
    end
  end
  return paths
end

function M.local_settings_dirs(cwd)
  local paths, err = native.local_settings_dirs(cwd)
  if not paths then
    return nil, err
  end
  return paths.dirs, nil, paths.git_dir
end

local function joined(list)
  local names = {}
  for i, item in ipairs(list) do
    names[i] = tostring(item)
  end
  return table.concat(names, ", ")
end

function M.trim(text)
  return text:match("^%s*(.-)%s*$")
end

--- Returns true if {path} is {root} or inside it. Both must be resolved.
function M.within(path, root)
  return root == "/" or path == root or path:sub(1, #root + 1) == root .. "/"
end

--- Returns true if {path} is in the checkout of {spec}, which reaches past
--- the session's directory when that is a subdirectory or a worktree.
function M.in_checkout(path, spec)
  for _, dir in ipairs(spec.checkout_dirs) do
    if M.within(path, dir) then
      return true
    end
  end
  return spec.git_dir ~= nil and M.within(path, spec.git_dir)
end

local function json(value)
  return type(value) == "string" and value or maki.json.encode(value)
end

function M.plugins_problem(plugins)
  return native.plugins_problem(maki.json.encode(plugins))
end

function M.resolved_model(account, requested)
  return native.resolved_model(json(account), requested)
end

function M.account_problem(init, worker)
  return native.account_problem(json(init), sorted_keys(worker.reported_modes))
end

local function empty(list)
  return list == nil or (M.is_list(list) and #list == 0)
end

-- Returns the first entry of {list} that is not in {ours}. A value that is
-- not a list could hold anything, so it is returned too.
local function foreign_entry(list, ours)
  if list ~= nil and not M.is_list(list) then
    return "paths given as a " .. type(list)
  end
  local allowed = M.set_of(ours)
  for _, path in ipairs(list or {}) do
    if not allowed[path] then
      return tostring(path)
    end
  end
end

--- Returns nil only if every shell command runs in the sandbox with no
--- network. A command cannot read `confine.deny_read`, apart from the paths
--- that `confine.allow_read` reopens, and writes only to its working
--- directory and `confine.allow_write`.
function M.sandbox_problem(sandbox, confine)
  if
    type(sandbox) ~= "table"
    or sandbox.enabled ~= true
    or sandbox.failIfUnavailable ~= true
    or sandbox.allowUnsandboxedCommands ~= false
    or not empty(sandbox.excludedCommands)
    or sandbox.enableWeakerNestedSandbox
  then
    return "Claude Code can run shell commands outside its sandbox"
  end
  local network = type(sandbox.network) == "table" and sandbox.network or {}
  if network.strictAllowlist ~= true or not empty(network.allowedDomains) or network.allowAllUnixSockets then
    return "Claude Code's sandbox lets shell commands use the network"
  end
  local filesystem = type(sandbox.filesystem) == "table" and sandbox.filesystem or {}
  if filesystem.disabled then
    return "Claude Code's sandbox lets shell commands read paths that maki denies"
  end
  local reopened = foreign_entry(filesystem.allowRead, confine.allow_read)
  if reopened then
    return "Claude Code's sandbox lets shell commands read " .. reopened
  end
  local denied = M.set_of(filesystem.denyRead)
  for _, path in ipairs(confine.deny_read) do
    if not denied[path] then
      return "Claude Code's sandbox lets shell commands read " .. path
    end
  end
  local writable = foreign_entry(filesystem.allowWrite, confine.allow_write)
  if writable then
    return "Claude Code's sandbox lets shell commands write to " .. writable
  end
  return nil
end

--- Only maki settings and safe managed restrictions can pass. The worker shell must also
--- remain inside `confine`.
local function deny_rules(extra)
  local rules = {}
  for i, pattern in ipairs(assert(M.denied_paths(extra))) do
    rules[i] = "Read(./" .. pattern .. ")"
  end
  return rules
end

function M.policy_problem(settings, hooks, deny_read, worker, confine)
  local problem, effective = native.policy_problem(json(settings), json(hooks))
  if problem then
    return problem
  end
  local permissions = type(effective.permissions) == "table" and effective.permissions or {}
  if permissions.blockReadsOutsideWorkingDirectories ~= true then
    return "Claude Code does not block reads outside the project"
  end
  local extra = permissions.additionalDirectories
  if extra ~= nil and not (M.is_list(extra) and #extra == 0) then
    return "Claude Code can also read "
      .. (M.is_list(extra) and joined(extra) or "directories given as a " .. type(extra))
  end
  local denied = {}
  for _, rule in ipairs(type(permissions.deny) == "table" and permissions.deny or {}) do
    denied[rule] = true
  end
  for _, rule in ipairs(deny_rules(deny_read)) do
    if not denied[rule] then
      return "Claude Code does not apply the deny rule " .. rule
    end
  end
  if worker.sandboxed then
    local problem = M.sandbox_problem(effective.sandbox, confine)
    if problem then
      return problem
    end
  end

  return nil
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

--- Returns the project-relative patterns: the defaults, then the
--- comma-separated patterns in {extra}, or nil and why one cannot be used.
--- As in `.gitignore`, a pattern without a `/` matches at any depth, so it
--- gets a `**/` twin.
function M.denied_paths(extra)
  local patterns = {}
  for _, pattern in ipairs(DEFAULT_DENY) do
    patterns[#patterns + 1] = pattern
  end
  for pattern in (extra or ""):gmatch("[^,]+") do
    pattern = M.trim(pattern):gsub("^%./", "")
    if pattern:sub(1, 1) == "/" or (("/" .. pattern .. "/"):find("/../", 1, true)) or pattern:find(")", 1, true) then
      return nil, pattern .. " must be relative to the project, without `..` or `)`"
    end
    if pattern ~= "" then
      patterns[#patterns + 1] = pattern
      if not pattern:find("/", 1, true) then
        patterns[#patterns + 1] = "**/" .. pattern
      end
    end
  end
  return patterns
end

--- `--setting-sources ""` drops the user's hooks, permissions and MCP
--- servers, and these settings turn off what Claude Code loads on its own. A
--- {worker} that runs commands gets the sandbox with {confine}'s absolute
--- paths, and Bash runs only inside it.
function M.settings(deny_read, worker, confine)
  local settings = {
    disableAllHooks = true,
    autoMemoryEnabled = false,
    claudeMdExcludes = { "**" },
    disableClaudeAiConnectors = true,
    permissions = {
      blockReadsOutsideWorkingDirectories = true,
      deny = deny_rules(deny_read),
    },
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
  return {
    spec.executable,
    "--print",
    "--input-format",
    STREAM_JSON,
    "--output-format",
    STREAM_JSON,
    "--verbose",
    "--no-session-persistence",
    "--model",
    spec.model,
    "--tools",
    table.concat(worker.tools, ","),
    "--restricted",
    "--permission-mode",
    worker.permission_mode,
    "--permission-prompts",
    "none",
    "--setting-sources",
    "",
    "--settings",
    (maki.json.encode(M.settings(spec.deny_read, worker, spec.confine))),
    "--strict-mcp-config",
    "--mcp-config",
    EMPTY_MCP_CONFIG,
    "--append-system-prompt",
    worker.contract,
  }
end

return M
