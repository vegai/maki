local t = require("maki.test_helpers")
local launch = require("claude_launch")
local Stream = require("claude_stream")
local snapshot = require("claude_snapshot")
local workspace = require("claude_workspace")

local case, eq, has = t.case, t.eq, t.has

local CWD = "/work"
local SECRET = "sk-ant-secret"
local PROMPT = "--explain the parser"
local ANSWER = "The parser lives in src/parse.rs:10."
local OK_VERSION = "2.1.284"
local LINUX = "Linux"
local CLI = assert(launch.checked_cli(OK_VERSION .. " (Claude Code)", LINUX))
local OUTSIDE = "/home/u/.ssh/config"
-- What 2.1.280 sends with every setting source off.
local BUILTIN_PLUGINS = {
  { name = "agents-md", path = "builtin", source = "agents-md@builtin" },
  { name = "telemetry", path = "builtin", source = "telemetry@builtin" },
}
local MISSING = {}
local READ = launch.WORKERS.read

local function init_event(overrides)
  local ev = {
    type = "system",
    subtype = "init",
    apiKeySource = "none",
    claude_code_version = OK_VERSION,
    permissionMode = "default",
    tools = { "Glob", "Grep", "Read" },
    mcp_servers = {},
    plugins = BUILTIN_PLUGINS,
    cwd = CWD,
  }
  for k, v in pairs(overrides or {}) do
    if v == MISSING then
      ev[k] = nil
    else
      ev[k] = v
    end
  end
  return maki.json.encode(ev)
end

local function assistant(id, content, usage)
  return maki.json.encode({ type = "assistant", message = { id = id, content = content, usage = usage } })
end

-- Returns a stream for {worker} (default: the read worker) in CWD.
local function fresh(worker)
  return Stream.new({ cwd = CWD, worker = worker or launch.WORKERS.read, cli = CLI })
end

local function started()
  local stream = fresh()
  assert(stream:feed(init_event()).init, "the checks must accept a clean init")
  return stream
end

local function contains(list, value)
  for _, v in ipairs(list) do
    if v == value then
      return true
    end
  end
  return false
end

-- ── claude_launch ──

case("child_env_passes_only_known_names_and_reports_route_ones", function()
  local env, withheld = launch.child_env({
    PATH = "/bin",
    HOME = "/home/u",
    LC_ALL = "C",
    https_proxy = "http://proxy:3128",
    CLAUDE_CODE_OAUTH_TOKEN = "subscription-token",
    CLAUDE_CODE_USE_BEDROCK = "1",
    ANTHROPIC_API_KEY = SECRET,
    ANTHROPIC_MODEL = "claude-x",
    SOME_TOOL_TOKEN = "other",
  })
  eq(env.PATH, "/bin")
  eq(env.HOME, "/home/u")
  eq(env.LC_ALL, "C")
  eq(env.https_proxy, "http://proxy:3128")
  eq(env.CLAUDE_CODE_OAUTH_TOKEN, "subscription-token")
  eq(env.ANTHROPIC_API_KEY, nil)
  eq(env.CLAUDE_CODE_USE_BEDROCK, nil)
  eq(env.ANTHROPIC_MODEL, nil)
  eq(env.SOME_TOOL_TOKEN, nil)
  eq(table.concat(withheld, ","), "ANTHROPIC_API_KEY,CLAUDE_CODE_USE_BEDROCK")
end)

for _, c in ipairs({
  {
    name = "the_code_worker_with_the_token",
    profile = "code",
    env = { CLAUDE_CODE_OAUTH_TOKEN = SECRET },
    want = "CLAUDE_CODE_OAUTH_TOKEN",
  },
  {
    name = "a_lowercase_token",
    profile = "code",
    env = { claude_code_oauth_token = SECRET },
    want = "claude_code_oauth_token",
  },
  {
    name = "a_proxy_with_a_password",
    profile = "code",
    env = { HTTPS_PROXY = "http://user:" .. SECRET .. "@proxy:3128/" },
    want = "HTTPS_PROXY",
  },
  {
    name = "a_lowercase_proxy_without_a_scheme",
    profile = "code",
    env = { http_proxy = SECRET .. "@proxy:3128" },
    want = "http_proxy",
  },
  { name = "a_proxy_without_a_login", profile = "code", env = { HTTPS_PROXY = "http://proxy:3128/a@b" }, want = nil },
  { name = "the_code_worker_without_it", profile = "code", env = { PATH = "/bin" }, want = nil },
  { name = "the_read_worker_with_it", profile = "read", env = { CLAUDE_CODE_OAUTH_TOKEN = SECRET }, want = nil },
  {
    name = "the_read_worker_with_a_proxy_password",
    profile = "read",
    env = { HTTPS_PROXY = "http://user:" .. SECRET .. "@proxy:3128" },
    want = nil,
  },
}) do
  case("shell_secret_problem_" .. c.name, function()
    local problem = launch.shell_secret_problem(launch.WORKERS[c.profile], c.env)
    if c.want then
      has(problem, c.want)
      eq(problem:find(SECRET, 1, true), nil)
    else
      eq(problem, nil)
    end
  end)
end

-- As in `.gitignore`, a pattern without a `/` matches at any depth, and one
-- with a `/` is anchored at the project.
case("denied_paths_twin_a_pattern_without_a_slash", function()
  local patterns = assert(launch.denied_paths("*.pem, certs/key.pem, ./local.txt"))
  assert(contains(patterns, "*.pem") and contains(patterns, "**/*.pem"), "*.pem must match at any depth")
  assert(contains(patterns, "certs/key.pem") and not contains(patterns, "**/certs/key.pem"), "a path stays anchored")
  assert(contains(patterns, "**/local.txt"), "a leading ./ goes")
end)

for _, pattern in ipairs({ "/etc/passwd", "../secret", "a/../b", "x)" }) do
  case("denied_paths_refuse_" .. pattern:gsub("%W", "_"), function()
    local patterns, err = launch.denied_paths(pattern)
    eq(patterns, nil)
    has(err or "", pattern)
  end)
end

-- The default Claude Code login stays out of the coding shell, even when
-- `config_dir` names another login.
case("home_credentials_include_the_default_claude_login", function()
  local paths = assert(launch.home_credentials("/home/u"))
  assert(contains(paths, "/home/u/.claude"), "~/.claude must be denied")
  assert(contains(paths, "/home/u/.ssh"), "~/.ssh must be denied")
end)

for _, home in ipairs({ "", false }) do
  case("home_credentials_without_a_home_" .. tostring(home), function()
    local paths, err = launch.home_credentials(home or nil)
    eq(paths, nil)
    has(err or "", "cannot find your home directory")
  end)
end

local function account_answer(overrides, mode)
  local account = { apiProvider = "firstParty", subscriptionType = "Claude Pro" }
  for k, v in pairs(overrides or {}) do
    account[k] = v ~= MISSING and v or nil
  end
  return { current_permission_mode = mode or "default", account = account }
end

local function has_pair(argv, flag, value)
  for i = 1, #argv - 1 do
    if argv[i] == flag and argv[i + 1] == value then
      return true
    end
  end
  return false
end

case("a_coding_worker_expects_accept_edits_and_a_reader_refuses_it", function()
  eq(launch.account_problem(account_answer(nil, "acceptEdits"), launch.WORKERS.code), nil)
  has(launch.account_problem(account_answer(nil, "acceptEdits"), READ) or "", "acceptEdits")
  has(launch.account_problem(account_answer(), launch.WORKERS.code) or "", "default")
end)

local CONFINE = {
  deny_read = { "/work", "/home/u/.claude", "/artifacts" },
  allow_read = { "/artifacts/a1/git" },
  allow_write = { "/artifacts/a1/tmp" },
}

-- The values here are literal on purpose. The coding worker gets only these
-- tools, in this mode, with no bypass, and Bash runs only in its sandbox.
case("argv_for_a_coding_worker_edits_and_runs_without_bypass", function()
  local argv = launch.argv({
    executable = "claude",
    model = "haiku",
    deny_read = "",
    worker = launch.WORKERS.code,
    confine = CONFINE,
  })
  assert(has_pair(argv, "--tools", "Read,Glob,Grep,Edit,Write,Bash"), "coding tools")
  assert(has_pair(argv, "--permission-mode", "acceptEdits"), "coding mode")
  local joined = table.concat(argv, " ")
  for _, forbidden in ipairs({ "--add-dir", "bypassPermissions", "--dangerously", "--allowedTools" }) do
    eq(joined:find(forbidden, 1, true), nil, "the argv must not contain " .. forbidden)
  end
  local settings = launch.settings("", launch.WORKERS.code, CONFINE)
  eq(settings.permissions.allow, nil)
  eq(settings.permissions.blockReadsOutsideWorkingDirectories, true)
  eq(
    maki.json.encode(settings.sandbox),
    maki.json.encode({
      enabled = true,
      failIfUnavailable = true,
      autoAllowBashIfSandboxed = true,
      allowUnsandboxedCommands = false,
      network = { allowedDomains = maki.json.decode("[]"), strictAllowlist = true },
      filesystem = { denyRead = CONFINE.deny_read, allowRead = CONFINE.allow_read, allowWrite = CONFINE.allow_write },
    })
  )
  has(maki.json.encode(settings), '"allowedDomains":[]')
  eq(launch.settings("", READ).sandbox, nil)
end)

-- Returns the sandbox the flags set, after {edit} changes the merged result.
local function merged_sandbox(edit)
  local sandbox = maki.json.decode(maki.json.encode(launch.settings("", launch.WORKERS.code, CONFINE).sandbox))
  if edit then
    edit(sandbox)
  end
  return sandbox
end

case("our_sandbox_passes", function()
  eq(launch.sandbox_problem(merged_sandbox(), CONFINE), nil)
end)

for _, c in ipairs({
  { name = "missing", edit = false, want = "outside its sandbox" },
  {
    name = "switched_off",
    edit = function(sb)
      sb.enabled = false
    end,
    want = "outside its sandbox",
  },
  {
    name = "optional_when_unavailable",
    edit = function(sb)
      sb.failIfUnavailable = nil
    end,
    want = "outside its sandbox",
  },
  {
    name = "with_an_escape_hatch",
    edit = function(sb)
      sb.allowUnsandboxedCommands = nil
    end,
    want = "outside its sandbox",
  },
  {
    name = "excluding_a_command",
    edit = function(sb)
      sb.excludedCommands = { "docker" }
    end,
    want = "outside its sandbox",
  },
  {
    name = "nested_weaker",
    edit = function(sb)
      sb.enableWeakerNestedSandbox = true
    end,
    want = "outside its sandbox",
  },
  {
    name = "allowing_a_domain",
    edit = function(sb)
      sb.network.allowedDomains = { "example.com" }
    end,
    want = "use the network",
  },
  {
    name = "prompting_for_hosts",
    edit = function(sb)
      sb.network.strictAllowlist = nil
    end,
    want = "use the network",
  },
  {
    name = "open_to_unix_sockets",
    edit = function(sb)
      sb.network.allowAllUnixSockets = true
    end,
    want = "use the network",
  },
  {
    name = "without_filesystem_rules",
    edit = function(sb)
      sb.filesystem.disabled = true
    end,
    want = "read paths that maki denies",
  },
  {
    name = "reallowing_another_read",
    edit = function(sb)
      sb.filesystem.allowRead = { "/artifacts/a1/git", "/work/src" }
    end,
    want = "read /work/src",
  },
  {
    name = "reallowing_reads_it_cannot_list",
    edit = function(sb)
      sb.filesystem.allowRead = { src = "/work/src" }
    end,
    want = "read paths given as a table",
  },
  {
    name = "short_of_a_denied_path",
    edit = function(sb)
      sb.filesystem.denyRead = { "/work" }
    end,
    want = "read /home/u/.claude",
  },
  {
    name = "writing_elsewhere",
    edit = function(sb)
      sb.filesystem.allowWrite = { "/artifacts/a1/tmp", "/work" }
    end,
    want = "write to /work",
  },
}) do
  case("a_sandbox_refused_when_" .. c.name, function()
    local sandbox = c.edit ~= false and merged_sandbox(c.edit) or nil
    has(launch.sandbox_problem(sandbox, CONFINE) or "", c.want)
  end)
end

case("a_coding_handshake_checks_the_sandbox_and_a_read_one_does_not", function()
  local settings = {
    sources = { { source = "flagSettings", settings = {} } },
    effective = launch.settings("", READ),
  }
  local hooks = { hooks = {}, policy = { allDisabled = true } }
  eq(launch.policy_problem(settings, hooks, "", READ), nil)
  has(launch.policy_problem(settings, hooks, "", launch.WORKERS.code, CONFINE) or "", "outside its sandbox")
  settings.effective = launch.settings("", launch.WORKERS.code, CONFINE)
  eq(launch.policy_problem(settings, hooks, "", launch.WORKERS.code, CONFINE), nil)
end)

local function read_argv()
  return launch.argv({ executable = "claude", model = "haiku", deny_read = "", worker = READ })
end

case("argv_for_a_reader_never_widens_its_reach", function()
  local joined = table.concat(read_argv(), " ")
  for _, forbidden in ipairs({ "--add-dir", "--allowedTools", "--allowed-tools", "bypassPermissions", "--dangerously" }) do
    eq(joined:find(forbidden, 1, true), nil, "the argv must not contain " .. forbidden)
  end
end)

-- The values here are literal rather than taken from the builder, so the
-- test notices if `claude_launch` drops one of these switches.
case("settings_switch_off_everything_claude_code_loads_on_its_own", function()
  local settings = launch.settings("", READ)
  eq(settings.disableAllHooks, true)
  eq(settings.autoMemoryEnabled, false)
  eq(settings.disableClaudeAiConnectors, true)
  eq(maki.json.encode(settings.claudeMdExcludes), '["**"]')
  eq(settings.permissions.blockReadsOutsideWorkingDirectories, true)
  eq(settings.permissions.additionalDirectories, nil)
end)

-- The switches that keep a reader's tools, permissions, settings and MCP
-- servers closed. They are literal here, so the test notices a missing one.
case("argv_carries_every_security_switch", function()
  local argv = read_argv()
  for _, c in ipairs({
    { "--tools", "Read,Glob,Grep" },
    { "--permission-mode", "manual" },
    { "--permission-prompts", "none" },
    { "--setting-sources", "" },
    { "--mcp-config", '{"mcpServers":{}}' },
  }) do
    assert(has_pair(argv, c[1], c[2]), c[1] .. " must be " .. c[2])
  end
  for _, flag in ipairs({ "--restricted", "--strict-mcp-config", "--no-session-persistence" }) do
    assert(contains(argv, flag), flag .. " missing")
  end
end)

case("instructions_come_before_the_prompt", function()
  local instructions = "\n\nProject instructions (/r/AGENTS.md):\nRun the linter."
  local content = maki.json.decode(launch.user_message(PROMPT, instructions)).message.content
  has(content[1].text, instructions)
  eq(content[2].text, PROMPT)
end)

-- Returns the `get_settings` response when only maki's flags and {policy}
-- apply, after {edit} changes the merged result.
local function settings_answer(policy, edit)
  local flags = maki.json.decode(maki.json.encode(launch.settings("", READ)))
  local sources = { { source = "flagSettings", settings = flags } }
  if policy then
    sources[#sources + 1] = { source = "policySettings", settings = policy }
  end
  local effective = maki.json.decode(maki.json.encode(flags))
  if edit then
    edit(effective, sources)
  end
  return { effective = effective, sources = sources }
end

local HOOKS_OFF = { hooks = {}, policy = { allDisabled = true, policyHookCount = 0 } }

-- `checks.rs` runs the same cases against the provider's checks, so the two
-- sides cannot drift apart. Each section's check returns the problem it
-- finds in one case, and compares any other result itself.
local RULE_CASES = require("tests.rule_cases")
local SHARED_CHECKS = {
  versions = function(c, shown)
    local cli, problem = launch.checked_cli(c.output, c.system)
    eq(cli and cli.version, c.version, shown)
    return problem
  end,
  env = function(c, shown)
    local env, withheld = launch.child_env({ [c.name] = "" })
    eq(env[c.name] and "passed" or withheld[1] == c.name and "withheld" or "dropped", c.verdict, shown)
  end,
  config_dirs = function(c, shown)
    local dir, problem = launch.config_dir(c.configured, c.home)
    eq(dir, c.dir, shown)
    return problem
  end,
  settings = function(c, shown)
    eq(table.concat(launch.settings_conflicts(c.settings), ","), table.concat(c.conflicts, ","), shown)
  end,
  policies = function(c)
    return launch.policy_problem(settings_answer(c.policy), HOOKS_OFF, "", READ)
  end,
  accounts = function(c)
    return launch.account_problem(c.init, READ)
  end,
  plugin_lists = function(c)
    return launch.plugins_problem(c.plugins)
  end,
  hooks = function(c)
    return launch.policy_problem(settings_answer(), c.hooks, "", READ)
  end,
}
for section, cases in pairs(RULE_CASES) do
  local check = assert(SHARED_CHECKS[section], "no check for the shared section " .. section)
  for i, c in ipairs(cases) do
    local shown = maki.json.encode(c)
    case("shared_" .. section .. "_" .. i, function()
      local problem = check(c, shown)
      if c.problem then
        has(problem or "", c.problem, shown)
      else
        eq(problem, nil, shown)
      end
    end)
  end
end

case("our_settings_alone_pass", function()
  eq(launch.policy_problem(settings_answer(), HOOKS_OFF, "", READ), nil)
end)

-- An empty settings error list means every setting is valid.
case("an_empty_settings_error_list_passes", function()
  local answer = settings_answer()
  answer.errors = maki.json.decode("[]")
  eq(launch.policy_problem(answer, HOOKS_OFF, "", READ), nil)
end)

case("managed_keys_that_only_restrict_pass", function()
  local policy = {
    companyAnnouncements = { "Be nice" },
    forceLoginMethod = "claudeai",
    permissions = { deny = { "Read(./vault/**)" }, disableBypassPermissionsMode = "disable" },
  }
  eq(launch.policy_problem(settings_answer(policy), HOOKS_OFF, "", READ), nil)
end)

for _, c in ipairs({
  { name = "hooks", policy = { hooks = { PreToolUse = {} } }, want = "sets hooks" },
  {
    name = "extra_roots",
    policy = { permissions = { additionalDirectories = { "/" } } },
    want = "permissions.additionalDirectories",
  },
  { name = "allow_rules", policy = { permissions = { allow = { "Read(//**)" } } }, want = "permissions.allow" },
  { name = "console_login", policy = { forceLoginMethod = "console" }, want = "forceLoginMethod" },
  { name = "unknown_key", policy = { sandbox = { enabled = false } }, want = "sets sandbox" },
  { name = "no_settings_object", policy = "anything", want = "not a settings object" },
}) do
  case("managed_policy_with_" .. c.name .. "_is_refused", function()
    has(launch.policy_problem(settings_answer(c.policy), HOOKS_OFF, "", READ) or "", c.want)
  end)
end

for _, c in ipairs({
  {
    name = "extra_readable_directories",
    edit = function(e)
      e.permissions.additionalDirectories = { "/srv" }
    end,
    want = "also read /srv",
  },
  {
    name = "outside_reads_allowed",
    edit = function(e)
      e.permissions.blockReadsOutsideWorkingDirectories = nil
    end,
    want = "outside the project",
  },
  {
    name = "a_dropped_deny_rule",
    edit = function(e)
      e.permissions.deny = {}
    end,
    want = "Read(./.env*)",
  },
  {
    name = "a_skipped_source",
    edit = function(_, sources)
      sources[#sources + 1] = { source = "userSettings", settings = {} }
    end,
    want = "userSettings",
  },
}) do
  case("merged_settings_with_" .. c.name .. "_are_refused", function()
    has(launch.policy_problem(settings_answer(nil, c.edit), HOOKS_OFF, "", READ) or "", c.want)
  end)
end

-- Claude Code leaves invalid settings out of its response, so a policy with
-- a bad entry could do more than the response shows. The message can quote
-- the value, so only the file and the key are shown.
for _, c in ipairs({
  {
    name = "a_listed_error",
    errors = {
      { file = "/etc/claude-code/managed-settings.json", path = "permissions.defaultMode", message = "bad TOKEN_42" },
    },
    want = "/etc/claude-code/managed-settings.json: permissions.defaultMode",
  },
  { name = "an_unreadable_error_list", errors = "TOKEN_42", want = "an error list that maki cannot read" },
}) do
  case("settings_with_" .. c.name .. "_are_refused", function()
    local answer = settings_answer()
    answer.errors = c.errors
    local problem = launch.policy_problem(answer, HOOKS_OFF, "", READ) or ""
    has(problem, c.want)
    assert(not problem:find("TOKEN_42", 1, true), "the error message shows the secret: " .. problem)
  end)
end

-- Values that pass a table check but carry entries maki would not read.
for _, c in ipairs({
  {
    name = "sources_as_an_object",
    reshape = function(answer)
      answer.sources = { flags = answer.sources[1] }
    end,
    want = "did not give its settings",
  },
  {
    name = "extra_roots_as_an_object",
    reshape = function(answer)
      answer.effective.permissions.additionalDirectories = { root = "/" }
    end,
    want = "can also read",
  },
}) do
  case("settings_with_" .. c.name .. "_are_refused", function()
    local answer = settings_answer()
    c.reshape(answer)
    has(launch.policy_problem(answer, HOOKS_OFF, "", READ) or "", c.want)
  end)
end

for _, c in ipairs({
  { name = "an_array", value = { "Read", "Grep" }, want = true },
  { name = "an_empty_table", value = {}, want = true },
  { name = "an_object", value = { tool = "Bash" }, want = false },
  { name = "a_decoded_array_with_a_null", value = maki.json.decode('["Read", null, "Bash"]'), want = false },
  { name = "a_string", value = "Read", want = false },
}) do
  case("is_list_" .. c.name, function()
    eq(launch.is_list(c.value), c.want)
  end)
end

local function write_file(path, content)
  maki.fs.mkdir(maki.fs.dirname(path), { parents = true })
  maki.fs.write(path, content)
end

-- Temporary directories can belong to a repository. Use a nonexistent path to exercise the
-- walk to the filesystem root.
case("local_settings_dirs_outside_a_repository", function()
  local cwd = "/no-such-dir-for-the-claude-code-spec/work"
  local dirs, err = launch.local_settings_dirs(cwd)
  eq(err, nil)
  eq(table.concat(dirs, "|"), cwd)
end)

-- Each layout is built under a fresh temporary directory, {root}, with its
-- own `.git` where the walk stops. Paths are relative to {root}.
for _, c in ipairs({
  {
    name = "in_a_repository_subdirectory",
    dirs = { "repo/.git", "repo/src/deep" },
    files = {},
    cwd = "repo/src/deep",
    want = { "repo/src/deep", "repo" },
  },
  {
    name = "in_a_worktree_with_an_absolute_gitdir",
    dirs = { "main/.git/worktrees/wt", "wt/sub" },
    files = {
      ["main/.git/worktrees/wt/commondir"] = "../..\n",
      ["wt/.git"] = "gitdir: @ROOT@/main/.git/worktrees/wt\n",
    },
    cwd = "wt/sub",
    want = { "wt/sub", "wt", "main" },
    git_dir = "main/.git",
  },
  {
    name = "in_a_worktree_with_a_relative_gitdir",
    dirs = { "main/.git/worktrees/wt", "wt" },
    files = {
      ["main/.git/worktrees/wt/commondir"] = "../..\n",
      ["wt/.git"] = "gitdir: ../main/.git/worktrees/wt\n",
    },
    cwd = "wt",
    want = { "wt", "main" },
    git_dir = "main/.git",
  },
  {
    name = "in_a_submodule",
    dirs = { "super/.git/modules/sub", "sub" },
    files = { ["sub/.git"] = "gitdir: ../super/.git/modules/sub\n" },
    cwd = "sub",
    want = { "sub" },
    git_dir = "super/.git/modules/sub",
  },
}) do
  case("local_settings_dirs_" .. c.name, function()
    local root = t.mktmpdir("cc_layout")
    for _, dir in ipairs(c.dirs) do
      maki.fs.mkdir(maki.fs.joinpath(root, dir), { parents = true })
    end
    for path, content in pairs(c.files) do
      write_file(maki.fs.joinpath(root, path), (content:gsub("@ROOT@", root)))
    end
    local dirs, err, git_dir = launch.local_settings_dirs(maki.fs.joinpath(root, c.cwd))
    t.rmtree(root)
    eq(err, nil)
    local want = {}
    for i, rel in ipairs(c.want) do
      want[i] = maki.fs.joinpath(root, rel)
    end
    eq(table.concat(dirs, "|"), table.concat(want, "|"))
    eq(git_dir, c.git_dir and maki.fs.joinpath(root, c.git_dir))
  end)
end

-- If a worktree's files are unreadable, or its primary checkout is gone,
-- that checkout's local settings cannot be checked, so the call stops.
for _, c in ipairs({
  {
    name = "an_unreadable_commondir",
    dirs = { "main/.git/worktrees/wt/commondir", "wt" },
    git = "gitdir: ../main/.git/worktrees/wt\\n",
  },
  { name = "a_git_file_that_is_not_utf8", dirs = { "wt" }, git = "gitdir: \\377\\n" },
  {
    name = "a_moved_primary_checkout",
    dirs = { "main/.git/worktrees/wt", "wt" },
    files = { ["main/.git/worktrees/wt/commondir"] = "../../../moved/.git\n" },
    git = "gitdir: ../main/.git/worktrees/wt\\n",
  },
}) do
  case("local_settings_dirs_refuse_" .. c.name, function()
    local root = t.mktmpdir("cc_layout")
    for _, dir in ipairs(c.dirs) do
      maki.fs.mkdir(maki.fs.joinpath(root, dir), { parents = true })
    end
    for path, content in pairs(c.files or {}) do
      write_file(maki.fs.joinpath(root, path), content)
    end
    -- `printf` writes bytes that a Lua string literal here cannot carry.
    local job = maki.fn.jobstart(
      { "sh", "-c", 'printf "$1" > "$2"', "sh", c.git, maki.fs.joinpath(root, "wt/.git") },
      { scope = "plugin" }
    )
    maki.fn.jobwait(job, 5000)
    local dirs, err = launch.local_settings_dirs(maki.fs.joinpath(root, "wt"))
    t.rmtree(root)
    eq(dirs, nil)
    has(err or "", "maki cannot examine it")
  end)
end

for _, c in ipairs({
  { name = "the_root_itself", path = "/work", root = "/work", want = true },
  { name = "a_child", path = "/work/sub", root = "/work", want = true },
  { name = "a_sibling_sharing_the_prefix", path = "/work2", root = "/work", want = false },
  { name = "a_parent", path = "/", root = "/work", want = false },
  { name = "anything_under_the_filesystem_root", path = "/tmp/x", root = "/", want = true },
  { name = "an_unrelated_dir", path = "/tmp/x", root = "/work", want = false },
}) do
  case("within_" .. c.name, function()
    eq(launch.within(c.path, c.root), c.want)
  end)
end

case("skipped_settings_cover_every_local_file", function()
  local paths = launch.skipped_settings("/home/u/.claude", "/r/sub", { "/r/sub", "/r", "/main" })
  eq(
    table.concat(paths, "|"),
    table.concat({
      "/home/u/.claude/settings.json",
      "/r/sub/.claude/settings.json",
      "/r/sub/.claude/settings.local.json",
      "/r/.claude/settings.local.json",
      "/main/.claude/settings.local.json",
    }, "|")
  )
end)

-- Configured paths follow the defaults, with spaces and empty entries
-- dropped and no second ./ added.
case("settings_deny_the_defaults_and_the_configured_paths", function()
  local settings = launch.settings(" config/prod.yml , ./certs/** ,, ", READ)
  eq(
    table.concat(settings.permissions.deny, " "),
    "Read(./.env*) Read(./**/.env*) Read(./secrets/**) Read(./**/secrets/**) Read(./config/prod.yml) Read(./certs/**)"
  )
end)

-- ── claude_stream ──

case("a_complete_clean_handshake_passes", function()
  local answers = { account = account_answer(), settings = settings_answer(), hooks = HOOKS_OFF }
  eq(launch.handshake_problem(answers, "", READ), nil)
end)

case("a_missing_handshake_answer_is_refused", function()
  local answers = { account = account_answer(), hooks = HOOKS_OFF }
  has(launch.handshake_problem(answers, "", READ) or "", "did not send a response to the settings request")
end)

for _, accepted in ipairs({ false, true }) do
  case("a_hook_starting_stops_the_run" .. (accepted and "_after_init" or "_before_init"), function()
    local stream = accepted and started() or fresh()
    local ev = maki.json.encode({ type = "system", subtype = "hook_started", hook_event = "SessionStart" })
    has(stream:feed(ev).stop, "started a SessionStart hook")
  end)
end

case("output_before_init_stops_the_run", function()
  local stream = fresh()
  eq(stream:feed(maki.json.encode({ type = "system", subtype = "thinking_tokens" })), nil)
  eq(stream:feed(maki.json.encode({ type = "keep_alive" })), nil)
  has(stream:feed(assistant("msg_1", {})).stop, "before its init event")
end)

for _, c in ipairs({
  { name = "not_json", line = "Update available", want = "a line that is not an event: Update available" },
  { name = "a_json_array", line = "[1]", want = "a line that is not an event: [1]" },
  { name = "an_object_without_a_type", line = '{"kind":"x"}', want = 'a line that is not an event: {"kind":"x"}' },
  { name = "an_unknown_event", line = '{"type":"surprise"}', want = "unknown event type: surprise" },
  { name = "long_and_not_ascii", line = string.rep("é", 100), want = "not an event: " .. string.rep("é", 80) },
}) do
  case("a_line_that_is_" .. c.name .. "_stops_the_run", function()
    for _, stream in ipairs({ fresh(), started() }) do
      local stop = stream:feed(c.line).stop or ""
      has(stop, c.want)
      assert(utf8.len(stop), "the stop cut a character: " .. stop)
    end
  end)
end

for _, c in ipairs({
  { name = "api_key", init = { apiKeySource = "ANTHROPIC_API_KEY" }, want = "ANTHROPIC_API_KEY" },
  { name = "missing_key_source", init = { apiKeySource = MISSING }, want = "API key source" },
  { name = "other_version", init = { claude_code_version = "2.1.285" }, want = "2.1.285" },
  { name = "bypass_mode", init = { permissionMode = "bypassPermissions" }, want = "bypassPermissions" },
  { name = "extra_tool", init = { tools = { "Glob", "Grep", "Read", "Bash" } }, want = "has the tool Bash" },
  { name = "missing_tool", init = { tools = { "Grep", "Read" } }, want = "does not have the tool Glob" },
  { name = "no_tools", init = { tools = {} }, want = "does not have the tool Read" },
  { name = "mcp_server", init = { mcp_servers = { { name = "x", status = "connected" } } }, want = "MCP" },
  {
    name = "installed_plugin",
    init = { plugins = { BUILTIN_PLUGINS[1], { name = "p", path = "/p", source = "p@market" } } },
    want = "p@market",
  },
  { name = "no_plugin_list", init = { plugins = MISSING }, want = "did not list its plugins" },
  -- A table check would pass an object, and `ipairs` would skip its entries.
  { name = "tools_as_an_object", init = { tools = { tool = "Bash" } }, want = "no tool list" },
  { name = "mcp_servers_as_an_object", init = { mcp_servers = { x = { name = "x" } } }, want = "no MCP server list" },
  {
    name = "plugins_as_an_object",
    init = { plugins = { p = { source = "p@market" } } },
    want = "did not list its plugins",
  },
  { name = "other_cwd", init = { cwd = "/elsewhere" }, want = "/elsewhere" },
  -- Without acceptEdits, every edit waits for an approval that never comes.
  {
    name = "a_code_worker_in_default_mode",
    worker = launch.WORKERS.code,
    init = { tools = launch.WORKERS.code.tools },
    want = "permission mode default",
  },
  {
    name = "a_code_worker_without_bash",
    worker = launch.WORKERS.code,
    init = { permissionMode = "acceptEdits", tools = { "Edit", "Glob", "Grep", "Read", "Write" } },
    want = "does not have the tool Bash",
  },
  {
    name = "a_code_worker_without_edit",
    worker = launch.WORKERS.code,
    init = { permissionMode = "acceptEdits", tools = { "Bash", "Glob", "Grep", "Read", "Write" } },
    want = "does not have the tool Edit",
  },
}) do
  case("init_with_" .. c.name .. "_stops_the_run", function()
    local stream = fresh(c.worker)
    has(stream:feed(init_event(c.init)).stop or "", c.want)
    eq(stream.accepted, false)
  end)
end

case("a_bad_init_after_a_good_one_still_stops", function()
  local stream = started()
  has(stream:feed(init_event({ apiKeySource = "apiKeyHelper" })).stop, "apiKeyHelper")
  eq(stream.accepted, false)
end)

case("tool_uses_become_progress_lines_relative_to_cwd", function()
  local step = started():feed(assistant("msg_1", {
    { type = "text", text = "Let me look." },
    { type = "tool_use", name = "Read", input = { file_path = CWD .. "/src/parse.rs" } },
    { type = "tool_use", name = "Grep", input = { pattern = "fn parse", path = CWD .. "/src" } },
  }))
  eq(table.concat(step.lines, "|"), "Read src/parse.rs|Grep fn parse in src")
end)

case("a_successful_result_is_the_answer", function()
  local stream = started()
  stream:feed(maki.json.encode({ type = "result", subtype = "success", is_error = false, result = ANSWER }))
  local text, is_error = stream:outcome()
  eq(text, ANSWER)
  eq(is_error, false)
end)

for _, c in ipairs({
  { name = "no_answer", result = { subtype = "success", result = " " }, want = "without a reply" },
  {
    name = "error_subtype",
    result = { subtype = "error_max_turns", errors = { "turn limit" } },
    want = "error_max_turns): turn limit",
  },
  {
    name = "error_flag",
    result = { subtype = "success", is_error = true, result = "Invalid API key" },
    want = "Invalid API key",
  },
  {
    name = "a_table_for_its_result",
    result = { subtype = "error_during_execution", result = { code = 1 }, errors = { "overloaded" } },
    want = "error_during_execution): overloaded",
  },
  {
    name = "errors_that_are_no_strings",
    result = { subtype = "error_during_execution", errors = { { code = 529 }, "retry later" } },
    want = 'error_during_execution): {"code":529}; retry later',
  },
}) do
  case("a_result_with_" .. c.name .. "_is_an_error", function()
    local stream = started()
    local ev = c.result
    ev.type = "result"
    stream:feed(maki.json.encode(ev))
    local text, is_error = stream:outcome()
    has(text, c.want)
    eq(is_error, true)
  end)
end

case("denied_reads_are_listed", function()
  local stream = started()
  stream:feed(maki.json.encode({
    type = "result",
    subtype = "success",
    result = ANSWER,
    permission_denials = {
      { tool_name = "Read", tool_input = { file_path = CWD .. "/.env" } },
      { tool_name = "Read", tool_input = { file_path = OUTSIDE } },
    },
  }))
  eq(table.concat(stream:denials(), "|"), "Read .env|Read " .. OUTSIDE)
end)

-- A field of the wrong type yields no name rather than raising, which would
-- turn a good reply into an error.
case("denials_of_the_wrong_shape_are_shown_rather_than_raised", function()
  local stream = started()
  stream:feed(maki.json.encode({
    type = "result",
    subtype = "success",
    result = ANSWER,
    permission_denials = {
      5,
      { tool_name = "Read", tool_input = { file_path = 7, pattern = {}, path = false } },
    },
  }))
  eq(table.concat(stream:denials(), "|"), "a denial that maki cannot read|Read ")
end)

-- Claude Code sends one `assistant` event for each content block of a
-- generation, and the output count of each step is a placeholder.
case("usage_without_result_counts_each_message_once_and_leaves_output_unknown", function()
  local stream = started()
  local usage =
    { input_tokens = 10, cache_read_input_tokens = 2000, cache_creation_input_tokens = 300, output_tokens = 1 }
  stream:feed(assistant("msg_1", {}, usage))
  stream:feed(assistant("msg_1", {}, usage))
  stream:feed(assistant("msg_2", {}, usage))
  eq(stream:usage_line(), "at least: 20 in · 4.0k cache read · 600 cache write · out unknown")
end)

case("a_field_one_message_lacks_stays_unknown", function()
  local stream = started()
  stream:feed(
    assistant("msg_1", {}, { input_tokens = 10, cache_read_input_tokens = 5, cache_creation_input_tokens = 1 })
  )
  stream:feed(assistant("msg_2", {}, { input_tokens = 10, cache_read_input_tokens = 5 }))
  eq(stream:usage_line(), "at least: 20 in · 10 cache read · cache write unknown · out unknown")
end)

case("a_result_reports_only_the_fields_it_has", function()
  local stream = started()
  stream:feed(
    maki.json.encode({ type = "result", subtype = "success", result = ANSWER, usage = { input_tokens = 12 } })
  )
  eq(stream:usage_line(), "at least: 12 in · cache read unknown · cache write unknown · out unknown")
end)

local MEASURED = { input_tokens = 10, cache_read_input_tokens = 2000, cache_creation_input_tokens = 300 }

case("a_result_without_usage_keeps_what_the_messages_measured", function()
  local stream = started()
  stream:feed(assistant("msg_1", {}, MEASURED))
  stream:feed(maki.json.encode({ type = "result", subtype = "success", result = ANSWER }))
  eq(stream:usage_line(), "at least: 10 in · 2.0k cache read · 300 cache write · out unknown")
end)

case("each_field_the_result_lacks_falls_back_to_the_messages", function()
  local stream = started()
  stream:feed(assistant("msg_1", {}, MEASURED))
  stream:feed(maki.json.encode({
    type = "result",
    subtype = "success",
    result = ANSWER,
    usage = { input_tokens = 12, output_tokens = 40 },
  }))
  eq(stream:usage_line(), "at least: 12 in · 2.0k cache read · 300 cache write · 40 out")
end)

case("usage_of_a_finished_run_is_its_total_at_list_price", function()
  local stream = started()
  stream:feed(maki.json.encode({
    type = "result",
    subtype = "success",
    result = ANSWER,
    total_cost_usd = 0.126,
    usage = { input_tokens = 12, output_tokens = 1500, cache_read_input_tokens = 0, cache_creation_input_tokens = 0 },
  }))
  eq(stream:usage_line(), "12 in · 0 cache read · 0 cache write · 1.5k out · $0.13 at the API list price")
end)

local WINDOWS = { five_hour = { utilization = 0.45 }, seven_day = { utilization = 0.153 } }
local EXTRA_USAGE = "extra usage on"

local function rate_limit(overage, windows)
  return maki.json.encode({
    type = "rate_limit_event",
    rate_limit_info = { status = "allowed", isUsingOverage = overage, unifiedWindows = windows },
  })
end

case("plan_limits_show_each_window", function()
  local stream = started()
  eq(stream:limits_line(), nil)
  eq(stream:feed(rate_limit(false, WINDOWS)), nil)
  eq(stream:limits_line(), "plan limits used: 45% of 5h, 15% of 7d")
end)

case("extra_usage_is_called_out", function()
  local stream = started()
  stream:feed(rate_limit(true, WINDOWS))
  eq(stream:limits_line(), "plan limits used: 45% of 5h, 15% of 7d · " .. EXTRA_USAGE)
end)

-- Extra usage is billed on top of the plan, so it is shown even when the
-- window values are missing.
for _, c in ipairs({
  { name = "without_windows", windows = nil },
  { name = "with_no_windows_listed", windows = {} },
}) do
  case("extra_usage_shows_" .. c.name, function()
    local stream = started()
    stream:feed(rate_limit(true, c.windows))
    eq(stream:limits_line(), EXTRA_USAGE)
  end)
end

case("no_usage_before_anything_was_counted", function()
  eq(started():usage_line(), nil)
end)

for _, c in ipairs({
  { name = "the_callers_own", requested = 120, want = 120 },
  { name = "the_option_when_none_is_asked", requested = nil, want = 600 },
  { name = "raised_to_the_least", requested = 5, want = launch.MIN_TIMEOUT_SECS },
  { name = "lowered_to_the_most", requested = 5000, want = launch.MAX_TIMEOUT_SECS },
  { name = "whole_seconds", requested = 90.7, want = 90 },
}) do
  case("a_task_timeout_is_" .. c.name, function()
    eq(launch.task_timeout(c.requested, 600), c.want)
  end)
end

-- ── claude_workspace ──

-- `S` means setuid without the execute bit.
for _, c in ipairs({
  { mode = "-rwxr-xr-x", want = true },
  { mode = "-rwsr-xr-x", want = true },
  { mode = "-rw-r--r--", want = false },
  { mode = "-rwSr--r--", want = false },
  { mode = "-rw-r-xr-x", want = false },
}) do
  case("owner_executes_" .. c.mode, function()
    eq(workspace.owner_executes(c.mode), c.want)
  end)
end

case("a_snapshot_leaves_out_credentials_and_claude_config", function()
  local specs = table.concat(workspace.excluded_pathspecs("config/prod.yml"), "|")
  for _, excluded in ipairs({ ".env*", "**/.env*", "secrets/**", "config/prod.yml", ".claude/**" }) do
    has(specs, ":(exclude,glob)" .. excluded)
  end
end)

-- Whatever the caller exports, git reads the artifact's repository and no
-- user or system config. Other variables pass through unchanged.
for _, c in ipairs({
  { var = "GIT_DIR", want = "/artifact/git" },
  { var = "GIT_WORK_TREE", want = "/snap" },
  { var = "GIT_CONFIG_GLOBAL", want = "/dev/null" },
  { var = "GIT_CONFIG_NOSYSTEM", want = "1" },
}) do
  case("snapshot_git_env_overrides_the_callers_" .. c.var:lower(), function()
    local env = workspace.snapshot_git_env({ HOME = "/home/u", [c.var] = "/from/caller" }, "/artifact/git", "/snap")
    eq(env[c.var], c.want)
    eq(env.HOME, "/home/u")
  end)
end

-- The values here are literal. Without any one of them, the worker's git
-- config could run a program inside maki's git commands, or unquote a
-- listing. Git reads them only before the subcommand.
case("git_commands_override_hooks_fsmonitor_and_quoting", function()
  local argv = workspace.git({ "status" })
  local joined = table.concat(argv, " ")
  for _, setting in ipairs({ "core.hooksPath=/dev/null", "core.fsmonitor=false", "core.quotePath=true", "gc.auto=0" }) do
    has(joined, "-c " .. setting)
  end
  eq(argv[#argv], "status")
end)

-- User input is normalized, so one file always has one spelling.
for _, c in ipairs({
  { name = "a_relative_file", path = "fixtures/input.json", want = "fixtures/input.json" },
  { name = "a_leading_dot", path = "./src/a.rs", want = "src/a.rs" },
  { name = "a_doubled_slash", path = "src//a.rs", want = "src/a.rs" },
  { name = "a_trailing_slash", path = "fixtures/", want = "fixtures" },
  { name = "an_absolute_path", path = "/etc/passwd", problem = "relative" },
  { name = "a_climb_out", path = "src/../../x", problem = "goes out of the project" },
  { name = "an_empty_path", path = "", problem = "not empty" },
  { name = "the_whole_project", path = "./", problem = "the full project" },
}) do
  case("relative_path_" .. c.name, function()
    local spelled, problem = workspace.relative_path(c.path)
    eq(spelled, c.want)
    if c.problem then
      has(problem or "", c.problem)
    end
  end)
end

-- Manifest paths come from git, which spells each path one way, so any
-- other spelling did not come from a collect step.
for _, c in ipairs({
  { name = "as_git_spells_it", path = "src/a.rs", want = nil },
  { name = "a_leading_dot", path = "./src/a.rs", want = "not in the format that git gives" },
  { name = "a_doubled_slash", path = "src//a.rs", want = "not in the format that git gives" },
  { name = "a_trailing_slash", path = "src/", want = "not in the format that git gives" },
  { name = "a_climb_out", path = "../a.rs", want = "goes out of the project" },
}) do
  case("manifest_path_" .. c.name, function()
    local problem = workspace.path_problem(c.path)
    if c.want then
      has(problem or "", c.want)
    else
      eq(problem, nil)
    end
  end)
end

case("a_larger_import_gets_more_time_than_the_bash_default", function()
  local one = workspace.import_timeout({ {} })
  assert(one >= 120, "less than the bash default: " .. one)
  assert(workspace.import_timeout({ {}, {}, {} }) > one, "a larger import does not receive more time")
end)

-- Each list follows a different word, so swapped lists would read wrong.
case("an_import_stopped_partway_names_what_it_applied_and_what_not", function()
  local message = snapshot.stopped_partway({ "src/lib.rs", "src/new.rs" }, { "src/old.rs" }, "", "boom")
  has(message, "It applied src/lib.rs, src/new.rs, and not src/old.rs.")
  has(message, "boom")
end)

local SHA_A = string.rep("a", 40)
local SHA_B = string.rep("b", 40)
local SHA_0 = string.rep("0", 40)

case("dependency_dirs_are_project_relative", function()
  local dirs = assert(workspace.dependency_dirs(" node_modules/ , ./web//./node_modules,"))
  eq(table.concat(dirs, "|"), "node_modules|web/node_modules")
  local dirs_none, err = workspace.dependency_dirs("../shared")
  eq(dirs_none, nil)
  has(err, "goes out of the project")
end)

case("an_exclude_rule_matches_its_dir_literally", function()
  eq(workspace.exclude_rule("node_modules"), "/node_modules/\n")
  eq(workspace.exclude_rule("#a b[1]*"), "/\\#a\\ b\\[1\\]\\*/\n")
  eq(workspace.exclude_rule("d\195\169p"), "/d\195\169p/\n")
end)

case("a_quoted_path_reads_back_byte_for_byte", function()
  eq(workspace.unquote("plain/name.rs"), "plain/name.rs")
  eq(workspace.unquote('"new\\nline"'), "new\nline")
  eq(workspace.unquote('"q\\"uote\\\\"'), 'q"uote\\')
  eq(workspace.unquote('"\\303\\244.txt"'), "\195\164.txt")
  eq(workspace.unquote('"\\\\303"'), "\\303")
end)

case("paths_for_hash_object_are_quoted_whole", function()
  eq(workspace.quoted_lines({ "a b", 'q"\\', "x\ny" }), '"a b"\n"q\\042\\134"\n"x\\012y"\n')
end)

case("a_name_that_is_not_utf8_is_found", function()
  eq(workspace.first_non_utf8({ "ok.txt", "\195\164.txt" }), nil)
  eq(workspace.first_non_utf8({ "ok.txt", "bad\255.txt" }), "bad\255.txt")
end)

case("a_shell_word_survives_quotes", function()
  eq(workspace.shell_quote("it's here"), "'it'\\''s here'")
end)

local function raw(old_mode, new_mode, old_sha, new_sha, status, path)
  return ":" .. old_mode .. " " .. new_mode .. " " .. old_sha .. " " .. new_sha .. " " .. status .. "\t" .. path .. "\n"
end

case("the_raw_diff_reads_every_change", function()
  local changes = workspace.parse_raw_diff(
    raw("100644", "100644", SHA_A, SHA_B, "M", "src/lib.rs")
      .. raw("000000", "100755", SHA_0, SHA_B, "A", "bin/run")
      .. raw("100644", "000000", SHA_A, SHA_0, "D", '"old\\tname.txt"')
  )
  eq(#changes, 3)
  eq(changes[1].path, "src/lib.rs")
  eq(changes[1].old_sha, SHA_A)
  eq(changes[2].status, "A")
  eq(changes[2].new_mode, "100755")
  eq(changes[3].path, "old\tname.txt")
end)

case("numstat_marks_binary_files", function()
  local binary = workspace.binary_paths('3\t1\tsrc/lib.rs\n-\t-\tlogo.png\n-\t-\t"\\303\\244.png"')
  eq(binary["logo.png"], true)
  eq(binary["\195\164.png"], true)
  eq(binary["src/lib.rs"], nil)
end)

local STAGED = table.concat({
  "100644 " .. SHA_A .. " 0\tsrc/lib.rs",
  "120000 " .. SHA_B .. " 0\tlink",
  "160000 " .. SHA_B .. " 0\tvendor/x",
  "100644 " .. SHA_A .. " 1\tmerging.rs",
  "100644 " .. SHA_B .. " 2\tmerging.rs",
  "100644 " .. SHA_B .. ' 0\t"sp\\303\\244ce.rs"',
}, "\n")

case("a_snapshot_takes_each_file_once_and_no_submodule", function()
  local files, submodules = workspace.tracked_paths(STAGED)
  eq(table.concat(files, "|"), "src/lib.rs|link|merging.rs|sp\195\164ce.rs")
  eq(table.concat(submodules, "|"), "vendor/x")
  eq(table.concat(workspace.listed_paths('a.rs\n"b\\nc.rs"'), "|"), "a.rs|b\nc.rs")
end)

for _, c in ipairs({
  {
    name = "text",
    change = { path = "a", status = "M", old_mode = "100644", new_mode = "100644", old_sha = SHA_A, new_sha = SHA_B },
    want = "text",
  },
  {
    name = "binary",
    change = {
      path = "logo.png",
      status = "M",
      old_mode = "100644",
      new_mode = "100644",
      old_sha = SHA_A,
      new_sha = SHA_B,
    },
    want = "binary",
  },
  {
    name = "symlink",
    change = { path = "l", status = "A", old_mode = "000000", new_mode = "120000", old_sha = SHA_0, new_sha = SHA_B },
    want = "symlink",
  },
  {
    name = "submodule",
    change = { path = "v", status = "M", old_mode = "160000", new_mode = "160000", old_sha = SHA_A, new_sha = SHA_B },
    want = "submodule",
  },
  {
    name = "type_change",
    change = { path = "t", status = "T", old_mode = "100644", new_mode = "100755", old_sha = SHA_A, new_sha = SHA_B },
    want = "type",
  },
  {
    name = "mode_only",
    change = { path = "run", status = "M", old_mode = "100644", new_mode = "100755", old_sha = SHA_A, new_sha = SHA_A },
    want = "mode",
  },
}) do
  case("a_change_of_kind_" .. c.name, function()
    eq(workspace.kind(c.change, { ["logo.png"] = true }), c.want)
  end)
end

case("conflicts_are_changes_the_checkout_no_longer_matches", function()
  local changes = {
    { path = "same", status = "M", old_sha = SHA_A, old_mode = "100644" },
    { path = "moved_on", status = "M", old_sha = SHA_A, old_mode = "100644" },
    { path = "made_executable", status = "M", old_sha = SHA_A, old_mode = "100644" },
    { path = "still_executable", status = "D", old_sha = SHA_A, old_mode = "100755" },
    { path = "new_here", status = "A", old_sha = SHA_0, old_mode = "000000" },
    { path = "new_both", status = "A", old_sha = SHA_0, old_mode = "000000" },
    { path = "gone", status = "D", old_sha = SHA_A, old_mode = "100644" },
  }
  local current = {
    same = workspace.file_state(SHA_A, false),
    moved_on = workspace.file_state(SHA_B, false),
    made_executable = workspace.file_state(SHA_A, true),
    still_executable = workspace.file_state(SHA_A, true),
    new_here = false,
    new_both = workspace.file_state(SHA_B, false),
    gone = false,
  }
  eq(table.concat(workspace.conflicts(changes, current), ","), "moved_on,made_executable,new_both,gone")
end)

-- An import puts manifest fields into the command the user approves, so a
-- change must look exactly like one from a collect step.
local function a_change(edit)
  local change = {
    path = "src/lib.rs",
    status = "M",
    old_sha = SHA_A,
    new_sha = SHA_B,
    old_mode = "100644",
    new_mode = "100644",
    kind = "text",
  }
  edit(change)
  return change
end

case("a_change_collection_writes_passes", function()
  eq(workspace.change_problem(a_change(function() end)), nil)
  eq(
    workspace.change_problem(a_change(function(c)
      c.status, c.old_sha, c.old_mode = "A", SHA_0, "000000"
    end)),
    nil
  )
end)

for _, c in ipairs({
  {
    "a_command_for_an_object_id",
    function(c)
      -- As long as a SHA-1 id, so only the hex check refuses it.
      c.new_sha = "$(touch pwned)" .. string.rep("a", 26)
    end,
    "no object id in new_sha",
  },
  {
    "a_short_object_id",
    function(c)
      c.old_sha = "abc"
    end,
    "no object id in old_sha",
  },
  {
    "a_path_out_of_the_project",
    function(c)
      c.path = "../outside.rs"
    end,
    "goes out of the project",
  },
  {
    "an_absolute_path",
    function(c)
      c.path = "/etc/passwd"
    end,
    "relative to the project",
  },
  {
    "an_unknown_status",
    function(c)
      c.status = "R"
    end,
    "unknown status",
  },
  {
    "an_unknown_mode",
    function(c)
      c.new_mode = "100777; rm -rf ~"
    end,
    "unknown new_mode",
  },
  {
    "an_unknown_kind",
    function(c)
      c.kind = "script"
    end,
    "text change, and not script",
  },
  {
    "a_git_path",
    function(c)
      c.path = ".git/hooks/pre-commit"
    end,
    "a snapshot does not copy",
  },
  {
    "a_nested_git_path",
    function(c)
      c.path = "vendor/lib/.git/config"
    end,
    "a snapshot does not copy",
  },
  {
    "a_claude_config_path",
    function(c)
      c.path = "src/.claude/settings.json"
    end,
    "a snapshot does not copy",
  },
  {
    "an_addition_with_an_old_mode",
    function(c)
      c.status, c.old_sha = "A", SHA_0
    end,
    "old_mode does not agree with its status A",
  },
  {
    "a_deletion_with_a_new_file",
    function(c)
      c.status = "D"
    end,
    "new_mode does not agree with its status D",
  },
  {
    "an_edit_from_no_object",
    function(c)
      c.old_sha = SHA_0
    end,
    "old object id that does not agree with",
  },
  {
    "a_mode_change_called_text",
    function(c)
      c.new_sha = SHA_A
    end,
    "mode change, and not text",
  },
  {
    "a_link_called_text",
    function(c)
      c.new_mode = "120000"
    end,
    "symlink change, and not text",
  },
}) do
  case("a_change_with_" .. c[1] .. "_is_refused", function()
    has(workspace.change_problem(a_change(c[2])) or "", c[3])
  end)
end

case("a_change_that_is_no_table_is_refused", function()
  has(workspace.change_problem("not a table") or "", "not a table")
end)

case("the_summary_says_what_needs_applying_by_hand", function()
  local lines = workspace.summary({
    { path = "src/lib.rs", status = "M", kind = "text" },
    { path = "logo.png", status = "A", kind = "binary" },
    { path = "link", status = "A", kind = "symlink" },
  })
  eq(lines[1], "M src/lib.rs")
  eq(lines[2], "A logo.png (binary)")
  eq(lines[3], "A link (symlink, apply manually)")
end)

-- After a failed import, the checkout shows what applied: a written file
-- has its new blob and executable bit, and a deleted file is gone. Nothing
-- else counts as applied.
case("what_landed_is_what_the_checkout_now_holds", function()
  local edit = { path = "src/lib.rs", status = "M", new_sha = SHA_B, new_mode = "100644" }
  local lost_its_bit = { path = "bin/run", status = "A", new_sha = SHA_B, new_mode = "100755" }
  local deletion = { path = "old.rs", status = "D", new_sha = SHA_0, new_mode = "000000" }
  local never_came = { path = "src/new.rs", status = "A", new_sha = SHA_B, new_mode = "100644" }
  local landed = workspace.landed({ edit, lost_its_bit, deletion, never_came }, {
    ["src/lib.rs"] = workspace.file_state(SHA_B, false),
    ["bin/run"] = workspace.file_state(SHA_B, false),
    ["old.rs"] = false,
    ["src/new.rs"] = false,
  })
  eq(#landed, 2)
  eq(landed[1].path, edit.path)
  eq(landed[2].path, deletion.path)
end)

-- A file that became a folder, or the reverse, is a change to the file plus
-- changes under its path. A name that merely shares the prefix is not under
-- it.
case("retyped_paths_finds_both_sides_of_a_file_that_became_a_folder", function()
  local retyped = workspace.retyped_paths({
    { path = "a" },
    { path = "a/b" },
    { path = "c/d/e" },
    { path = "c" },
    { path = "ab" },
    { path = "f/g" },
  })
  local found = {}
  for path in pairs(retyped) do
    found[#found + 1] = path
  end
  table.sort(found)
  eq(table.concat(found, " "), "a a/b c c/d/e")
end)

t.report()
