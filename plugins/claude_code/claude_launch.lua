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
local RULES = require("claude_rules")

--- `ipairs` skips object entries. Require a JSON array so an object cannot bypass list validation.
function M.is_list(value)
  if type(value) ~= "table" then
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
-- The minimum version supplies the necessary protocol. Each call must validate every login,
-- setting, hook, tool and plugin input.
--
-- Run the live tests in `maki-lua/tests/claude_code/qualify.rs` to qualify a new CLI version.
local MINIMUM_VERSION = table.concat(RULES.minimum_version, ".")
-- `uname` names in lower case. Path rules, symlinks and process cleanup
-- differ per OS.
local SYSTEMS = M.set_of(RULES.systems)
-- The process that runs the task answers these requests, so the answers
-- cover every settings source it merged, managed policy included.
M.HANDSHAKE = RULES.handshake

local STREAM_JSON = "stream-json"
-- How much of a JSON value a message shows, as in the provider.
local SHOWN_JSON_CHARS = 200
local EMPTY_MCP_CONFIG = '{"mcpServers":{}}'
local INSTRUCTIONS_HEADING = "The instructions maki loaded for this task, from the project and the user:"

-- Only these variables reach the child, so an exported ANTHROPIC_API_KEY
-- cannot move it off the subscription. Names compare in upper case, so
-- `https_proxy` passes too.
local PASSED_ENV = M.set_of(RULES.passed_env)
-- Claude Code passes its environment to Bash, in the sandbox too (tested on
-- 2.1.284), and the sandbox cannot drop a variable.
local SHELL_TOOL = "Bash"
local SHELL_VISIBLE_LOGIN = "CLAUDE_CODE_OAUTH_TOKEN"
-- A proxy URL can carry a user and a password that Claude Code needs to
-- reach the API, so maki cannot strip them.
local PROXY_ENV = { HTTP_PROXY = true, HTTPS_PROXY = true }

-- Variables that select a route. Every name with a prefix in
-- `route_env_prefixes` is one too, unless `harmless_env` lists it, so a new
-- cloud switch fails closed.
local ROUTE_ENV = M.set_of(RULES.route_env)
local HARMLESS_ENV = M.set_of(RULES.harmless_env)
-- Managed keys that only restrict or inform. Any other managed key can
-- reopen what the flags closed, and then maki cannot vouch for the profile.
local HARMLESS_POLICY = M.set_of(RULES.harmless_policy)
local HARMLESS_POLICY_PERMISSIONS = M.set_of(RULES.harmless_policy_permissions)

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
  ".config/gh",
  ".config/gcloud",
  ".docker",
  ".kube",
  ".npmrc",
  ".pypirc",
  ".cargo/credentials.toml",
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

local function version_parts(version)
  local major, minor, patch = (version or ""):match("^(%d+)%.(%d+)%.(%d+)$")
  return major and { tonumber(major), tonumber(minor), tonumber(patch) }
end

local function older(a, b)
  for i = 1, #a do
    if a[i] ~= b[i] then
      return a[i] < b[i]
    end
  end
  return false
end

--- Returns `{ version }` for `claude --version` output on {sysname}, or nil
--- and the reason. A first word other than `major.minor.patch` is refused.
function M.checked_cli(version_output, sysname)
  local version = (version_output or ""):match("^%s*(%S+)")
  local parts = version_parts(version)
  if not parts then
    return nil, "maki cannot read a Claude Code version from " .. string.format("%q", version_output or "")
  end
  if older(parts, RULES.minimum_version) then
    return nil, "Claude Code " .. version .. " is older than " .. MINIMUM_VERSION .. ", the oldest version maki runs"
  end
  if not SYSTEMS[(sysname or ""):lower()] then
    local systems = table.concat(RULES.systems, ", ")
    return nil, "maki runs Claude Code only on " .. systems .. ", not on " .. tostring(sysname)
  end
  return { version = version }
end

local function passed(name)
  local upper = name:upper()
  if PASSED_ENV[upper] then
    return true
  end
  for _, prefix in ipairs(RULES.passed_env_prefixes) do
    if upper:sub(1, #prefix) == prefix then
      return true
    end
  end
  return false
end

local function routes_login(name)
  if ROUTE_ENV[name] then
    return true
  end
  for _, prefix in ipairs(RULES.route_env_prefixes) do
    if name:sub(1, #prefix) == prefix then
      return not HARMLESS_ENV[name]
    end
  end
  return false
end

--- Returns the child's environment from {environ}, and the sorted names
--- held back because they can change the login.
function M.child_env(environ)
  local env, withheld = {}, {}
  for name, value in pairs(environ) do
    if passed(name) then
      env[name] = value
    elseif routes_login(name) then
      withheld[#withheld + 1] = name
    end
  end
  table.sort(withheld)
  return env, withheld
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
function M.home_credentials(home)
  if not home or home == "" then
    return nil, NO_HOME
  end
  local paths = {}
  for i, path in ipairs(HOME_CREDENTIALS) do
    paths[i] = maki.fs.joinpath(home, path)
  end
  return paths
end

--- Returns {configured} (`config_dir` or `$CLAUDE_CONFIG_DIR`), or `.claude`
--- in {home}. The checks and each child resolve it from different
--- directories, so it must be absolute.
function M.config_dir(configured, home)
  -- Claude Code treats an empty CLAUDE_CONFIG_DIR as unset.
  local dir = configured ~= "" and configured or nil
  if not dir then
    if not home or home == "" then
      return nil,
        "maki cannot find the Claude Code config directory. Set the claude_code plugin's `config_dir` option, "
          .. "CLAUDE_CONFIG_DIR or HOME."
    end
    dir = maki.fs.joinpath(home, RULES.claude_dir)
  end
  if dir:sub(1, 1) ~= "/" then
    return nil,
      "the Claude Code config directory "
        .. dir
        .. " is not an absolute path. Set the claude_code plugin's `config_dir` option, CLAUDE_CONFIG_DIR or HOME "
        .. "to an absolute path."
  end
  return dir
end

-- A worktree's `.git` file identifies its git directory. `commondir` leads to the primary
-- checkout. A submodule has its own git directory.
--
-- Unreadable git paths must stop validation because the primary checkout can contain local
-- settings. Resolve paths lexically, as Claude Code does.
local function main_checkout(git_file)
  local text, err = maki.fs.read(git_file)
  if not text then
    return nil, err
  end
  local gitdir = text:match("^gitdir:%s*(.-)%s*$")
  if not gitdir then
    return nil
  end
  gitdir = maki.fs.normalize(maki.fs.joinpath(maki.fs.dirname(git_file), gitdir))
  local commondir = maki.fs.joinpath(gitdir, "commondir")
  local meta, meta_err = maki.fs.metadata(commondir)
  if meta_err then
    return nil, meta_err
  end
  if not meta then
    return nil, nil, gitdir
  end
  local common, common_err = maki.fs.read(commondir)
  if not common then
    return nil, common_err
  end
  common = maki.fs.normalize(maki.fs.joinpath(gitdir, (common:gsub("%s+$", ""))))
  local found, found_err = maki.fs.metadata(common)
  if not found then
    return nil, found_err or (common .. " does not exist")
  end
  return maki.fs.basename(common) == ".git" and maki.fs.dirname(common) or nil, nil, common
end

--- Local settings can exist in the session directory, repository root and primary checkout.
--- Git objects can also reside outside the session directory.
function M.local_settings_dirs(cwd)
  local dirs, seen = {}, {}
  local function add(dir)
    if dir and not seen[dir] then
      seen[dir] = true
      dirs[#dirs + 1] = dir
    end
  end
  add(cwd)
  local candidates = { cwd }
  for _, parent in ipairs(maki.fs.parents(cwd)) do
    candidates[#candidates + 1] = parent
  end
  for _, dir in ipairs(candidates) do
    local git = maki.fs.joinpath(dir, ".git")
    local meta, err = maki.fs.metadata(git)
    if err then
      return nil, git .. ": maki cannot examine it (" .. err .. ")"
    end
    if meta then
      add(dir)
      if meta.is_file then
        local main, main_err, git_dir = main_checkout(git)
        if main_err then
          return nil, git .. ": maki cannot examine it (" .. main_err .. ")"
        end
        add(main)
        return dirs, nil, git_dir
      end
      break
    end
  end
  return dirs
end

local function joined(list)
  local names = {}
  for i, item in ipairs(list) do
    names[i] = tostring(item)
  end
  return table.concat(names, ", ")
end

-- As the provider shows a value from Claude Code: as JSON, cut so a huge
-- value cannot flood the reply.
local function shown(value)
  local text = maki.json.encode(value)
  local cut = utf8.offset(text, SHOWN_JSON_CHARS + 1)
  return cut and cut <= #text and text:sub(1, cut - 1) .. "…" or text
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

--- Managed settings still apply and need handshake validation. Claude Code reads project
--- settings in its working directory and local settings in each `local_dirs` entry.
function M.skipped_settings(config_dir, cwd, local_dirs)
  local paths = {
    maki.fs.joinpath(config_dir, RULES.settings_file),
    maki.fs.joinpath(cwd, RULES.claude_dir, RULES.settings_file),
  }
  for _, dir in ipairs(local_dirs) do
    paths[#paths + 1] = maki.fs.joinpath(dir, RULES.claude_dir, RULES.local_settings_file)
  end
  return paths
end

--- Returns the keys in an ignored file that are unsafe to ignore. Only the
--- names, because `env` can hold secrets.
function M.settings_conflicts(settings)
  local keys = {}
  for _, key in ipairs(RULES.route_settings) do
    if settings[key] ~= nil then
      keys[#keys + 1] = key
    end
  end
  local method = settings[RULES.login_method_key]
  if method ~= nil and method ~= RULES.subscription_login_method then
    keys[#keys + 1] = RULES.login_method_key
  end
  local env = settings[RULES.env_key]
  if type(env) == "table" then
    for _, name in ipairs(sorted_keys(env)) do
      if routes_login(name) then
        keys[#keys + 1] = RULES.env_key .. "." .. name
      end
    end
  end
  return keys
end

--- Returns nil only if {plugins} is a list of Claude Code's own plugins.
--- Claude Code ships its own plugins, and a version or a feature gate can add
--- one. The other checks cover what such a plugin could change: the tools,
--- the MCP servers, the hooks and the settings.
function M.plugins_problem(plugins)
  if not M.is_list(plugins) then
    return "Claude Code did not list its plugins"
  end
  local marker = RULES.builtin_plugin_marker
  for _, plugin in ipairs(plugins) do
    local source = type(plugin) == "table" and plugin.source or nil
    if type(source) ~= "string" or plugin.path ~= marker or source:sub(-#marker - 1) ~= "@" .. marker then
      return "Claude Code loaded the plugin "
        .. shown(source)
        .. ". maki does not run Claude Code with a plugin other than its built-in ones, "
        .. "because a plugin can change what the model sees and does. Turn the plugin off in Claude Code."
    end
  end
  return nil
end

--- Returns nil only for a claude.ai subscription login to Anthropic, with no
--- API key, in the mode {worker} needs.
function M.account_problem(init, worker)
  local account = type(init) == "table" and init.account
  if type(account) ~= "table" then
    return "Claude Code did not report its login"
  end
  if account.apiKeySource ~= nil and account.apiKeySource ~= RULES.no_key_source then
    return "Claude Code would use an API key from " .. shown(account.apiKeySource) .. " instead of the subscription"
  end
  if account.apiProvider ~= RULES.first_party then
    return "Claude Code sends requests to " .. shown(account.apiProvider) .. " instead of Anthropic"
  end
  if type(account.subscriptionType) ~= "string" or account.subscriptionType == "" then
    return "Claude Code is not logged in with a claude.ai subscription. Run `claude auth login` with your "
      .. "Claude subscription. For API billing, use maki's anthropic provider."
  end
  if not worker.reported_modes[init.current_permission_mode] then
    return "Claude Code starts in permission mode " .. shown(init.current_permission_mode)
  end
  return nil
end

local function policy_key_problem(policy)
  for _, key in ipairs(sorted_keys(policy)) do
    local value = policy[key]
    if key == RULES.login_method_key then
      if value ~= RULES.subscription_login_method then
        return key
      end
    elseif not HARMLESS_POLICY[key] then
      return key
    elseif key == RULES.permissions_key and type(value) == "table" then
      for _, sub in ipairs(sorted_keys(value)) do
        if not HARMLESS_POLICY_PERMISSIONS[sub] then
          return RULES.permissions_key .. "." .. sub
        end
      end
    end
  end
  return nil
end

--- Returns `file: key` for each setting Claude Code rejected as invalid. A
--- missing list and an empty one both mean no errors. The messages are left
--- out, because a message can quote a value.
local function invalid_settings(errors)
  if errors == nil or M.is_list(errors) and #errors == 0 then
    return nil
  end
  local found = {}
  for _, err in ipairs(type(errors) == "table" and errors or {}) do
    local entry = type(err) == "table" and err or {}
    local key = type(entry.path) == "string" and entry.path ~= "" and ": " .. entry.path or ""
    found[#found + 1] = tostring(entry.file or "a file without a name") .. key
  end
  return #found > 0 and table.concat(found, ", ") or "an error list that maki cannot read"
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
  if type(settings) ~= "table" or not M.is_list(settings.sources) or type(settings.effective) ~= "table" then
    return "Claude Code did not give its settings"
  end
  local invalid = invalid_settings(settings.errors)
  if invalid then
    return "Claude Code rejected some settings as invalid, so maki cannot tell which settings apply: " .. invalid
  end
  for _, source in ipairs(settings.sources) do
    local name = type(source) == "table" and source.source
    if name == RULES.policy_source then
      if type(source.settings) ~= "table" then
        return "your organization's Claude Code policy is not a settings object that maki can check"
      end
      local key = policy_key_problem(source.settings)
      if key then
        return "your organization's Claude Code policy sets " .. key .. ", which maki cannot confirm is safe"
      end
    elseif name ~= RULES.flag_source then
      return "Claude Code loaded " .. tostring(name) .. " settings, which it must ignore"
    end
  end
  local effective = settings.effective
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
  if type(hooks) ~= "table" or not M.is_list(hooks.hooks) or type(hooks.policy) ~= "table" then
    return "Claude Code did not list its hooks"
  end
  if hooks.policy.allDisabled ~= true then
    return "Claude Code cannot turn its hooks off"
  end
  for _, hook in ipairs(hooks.hooks) do
    if type(hook) ~= "table" or hook.disabled ~= true then
      local source = type(hook) == "table" and hook.source or nil
      return "Claude Code would run a hook from " .. shown(source) .. ", which can change the checkout"
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
