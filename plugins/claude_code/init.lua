-- Send the task only after the probe and worker pass the handshake.
--
-- Managed hooks can run at startup. Run the probe in an empty directory so a rejected hook
-- cannot write to the project.

local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local partial = require("maki.partial")
local truncate = require("maki.truncate")
local jobs = require("claude_jobs")
local launch = require("claude_launch")
local snapshot = require("claude_snapshot")
local Stream = require("claude_stream")
local workspace = require("claude_workspace")

local MODELS = { "sonnet", "opus", "haiku", "fable" }
local PROBE_DIR_TEMPLATE = "maki-claude-code.XXXXXXXX"
local DEFAULT_TMP_DIR = "/tmp"
local MS_PER_SEC = jobs.MS_PER_SEC
-- A stalled startup must not occupy a request slot indefinitely. Apply this limit to helpers
-- without their own timeout.
local STARTUP_TIMEOUT_SECS = 30
local STARTUP_MS = STARTUP_TIMEOUT_SECS * MS_PER_SEC
-- Each step of a coding snapshot, such as the copy, must finish in this time.
local SNAPSHOT_TIMEOUT_MS = 120 * MS_PER_SEC
local DEFAULT_OUTPUT_LINES = 5
local BODY_INDENT_COLS = 4
local MIN_MD_WIDTH = 20
local MAX_SHOWN_CHANGES = 50
local ARTIFACTS_SUBDIR = "claude_code"
local ARTIFACTS_LEAF = "changes"
local READ_PROFILE = "read"
local CODE_PROFILE = "code"
local ERROR_PREFIX = jobs.ERROR_PREFIX
local STARTING = "Start Claude Code..."
local SNAPSHOTTING = "Create a snapshot of the project..."
local PREPARING = "Run the prepare command..."
local ABANDONED = "The worker's snapshot stays in %s until maki removes it."
local CHECKING = "Validate the login and policy of Claude Code %s"
local NO_SESSION_DIR = "maki cannot find the session's working directory: "
-- A permission scope: "claude <profile> <model>".
local SCOPE = "claude %s %s"
local WAITING = "Wait for a free Claude Code slot..."
local ROUTE_OK = "claude.ai subscription%s on Claude Code %s, maki found no route conflict"
local ROUTE_UNCONFIRMED = "maki could not confirm the route on Claude Code %s"
local PROBE_RUN = "Claude Code started a run during its checks, before maki sent a prompt"
local UNCHECKED_START = "Claude Code started its run before maki accepted its checks"
local PREPARE_FAILED = "maki could not prepare the dependencies (%s). Report any check that needs them as unavailable."
local UNSETTLED = "Some dependencies changed while maki copied them (%s). They can contain parts of two versions. "
  .. "Mention this for every check that uses them."

local description = [[Send a task to Claude through the user's Claude subscription.

Claude cannot see this conversation. Put all necessary task data in `prompt`. Give file paths instead of file contents.
- `profile = "read"` (default): Claude reads, globs and greps the project. Ask for short results with file:line locations.
- `profile = "code"`: Claude edits files and runs commands in a private project snapshot. It returns changes as an artifact. Review them before claude_code_import changes the checkout. List necessary untracked files in `include`.
- The user must open the tool view to see the reply. Relay the parts necessary for the main task.]]

local import_description = [[Apply a claude_code artifact's changes to the checkout with one approved bash command.

If a target changed since the snapshot, the import stops before any checkout change. A later conflict can stop import partway. The error lists applied changes. A retry preserves earlier backups and applies changes that remain safe. Use `paths` to select files.]]

local opts = maki.api.register_options(output_limits.extend({
  executable = { default = "claude", desc = "The Claude Code executable." },
  config_dir = {
    default = "",
    desc = "Absolute path of a Claude Code config directory for maki, in place of "
      .. "`$CLAUDE_CONFIG_DIR` or `~/.claude`. Use it to give maki a different Claude login.",
  },
  model = { default = "sonnet", desc = "Model alias for calls that do not pick one." },
  max_concurrent = {
    default = 2,
    min = 1,
    desc = "The maximum number of plugin calls that run Claude Code at the same time. The claude-code "
      .. "provider has its own limit with the same value, so up to twice this many can run together.",
  },
  timeout_secs = {
    default = 600,
    min = launch.MIN_TIMEOUT_SECS,
    desc = "Stop a task after this many seconds, at most "
      .. launch.MAX_TIMEOUT_SECS
      .. ". The snapshot and `prepare` of a coding task count, and time spent waiting for a free slot does not. "
      .. "A call's `timeout` parameter overrides it, within the same limits.",
  },
  deny_read = {
    default = "",
    desc = "Extra paths Claude cannot read, relative to the session's directory and comma-separated, for "
      .. "example `config/prod.yml,certs/**`. As in `.gitignore`, a pattern without a `/` matches at any "
      .. "depth. They add to the default `.env*` and `secrets/` rules, and coding snapshots never contain them.",
  },
  dependencies = {
    default = "",
    desc = "Untracked directories a coding snapshot copies from the project, comma-separated, for example "
      .. "`node_modules`. The copy is a copy-on-write clone when the filesystem supports it.",
  },
  prepare = {
    default = "",
    desc = "Shell command to run in each coding snapshot before Claude starts, for example "
      .. "`npm ci` or `python -m venv .venv && .venv/bin/pip install -e .`. It runs as you, outside the "
      .. "sandbox and with network access, so the worker can use what it fetches.",
  },
  prepare_timeout_secs = {
    default = 300,
    min = 1,
    desc = "Stop the `prepare` command after this many seconds.",
  },
  artifact_dir = {
    default = "",
    desc = "Absolute path of the directory for coding snapshots and their changes. Defaults to "
      .. "`claude_code/changes` in the maki state directory. On the project's filesystem, maki can clone the "
      .. "dependencies with copy-on-write. Imports of replacements or deletions need hard links on the checkout filesystem. "
      .. "Expiry removes only artifacts that maki created there.",
  },
  artifact_ttl_hours = {
    default = 24,
    min = 1,
    desc = "Remove a coding artifact after this many hours without a write.",
  },
}))
if opts.timeout_secs > launch.MAX_TIMEOUT_SECS then
  error("the maximum value of the `timeout_secs` option is " .. launch.MAX_TIMEOUT_SECS)
end

local valid_model = {}
for _, model in ipairs(MODELS) do
  valid_model[model] = true
end
if not valid_model[opts.model] then
  error("the `model` option must be one of " .. table.concat(MODELS, ", ") .. ", not " .. tostring(opts.model))
end
if opts.config_dir ~= "" and opts.config_dir:sub(1, 1) ~= "/" then
  error("the `config_dir` option must be an absolute path, not " .. opts.config_dir)
end
if opts.artifact_dir ~= "" then
  if opts.artifact_dir:sub(1, 1) ~= "/" then
    error("the `artifact_dir` option must be an absolute path")
  end
  -- A `..` after a missing directory has no meaning until that directory
  -- exists.
  if workspace.climbs(opts.artifact_dir) then
    error("the `artifact_dir` option must not contain `..`")
  end
end
local dependencies, dependencies_err = workspace.dependency_dirs(opts.dependencies)
if not dependencies then
  error("the `dependencies` option: " .. dependencies_err)
end
local _, denied_err = launch.denied_paths(opts.deny_read)
if denied_err then
  error("the `deny_read` option: " .. denied_err)
end

local function artifacts_root()
  if opts.artifact_dir ~= "" then
    return opts.artifact_dir
  end
  local state_dir = maki.env.state_dir()
  if not state_dir then
    return nil, "maki cannot find its state directory, which contains the coding artifacts"
  end
  return maki.fs.joinpath(state_dir, ARTIFACTS_SUBDIR, ARTIFACTS_LEAF)
end

local semaphore = maki.async.semaphore(opts.max_concurrent)
-- Only for the message of a queued call. A call is counted when it arrives,
-- so the count does not depend on when `acquire` resumes.
local callers = 0

local function refuse(msg)
  return { llm_output = ERROR_PREFIX .. msg, is_error = true }
end

-- An unreadable file could hold an API key helper, so it stops the call.
local function settings_conflicts(paths)
  local found = {}
  for _, path in ipairs(paths) do
    local meta, meta_err = maki.fs.metadata(path)
    if meta_err then
      found[#found + 1] = path .. ": maki cannot examine it (" .. meta_err .. ")"
    elseif meta then
      local text, read_err = maki.fs.read(path)
      local settings, decode_err
      if text then
        settings, decode_err = maki.json.decode(text)
      end
      if not text then
        found[#found + 1] = path .. ": maki cannot read it (" .. read_err .. ")"
      elseif decode_err then
        found[#found + 1] = path .. ": " .. decode_err
      -- maki.json.decode marks a JSON array with a metatable, an object with
      -- none, so an empty array and an empty object stay apart.
      elseif type(settings) ~= "table" or getmetatable(settings) ~= nil then
        found[#found + 1] = path .. ": is not a JSON object"
      else
        for _, key in ipairs(launch.settings_conflicts(settings)) do
          found[#found + 1] = path .. ": " .. key
        end
      end
    end
  end
  return found
end

local function trimmed(text)
  local line = text and launch.trim(text)
  return line ~= "" and line or nil
end

-- Returns why the responses stop the run, or nil, and whether all responses
-- are in.
local function take_answer(answers, control, worker, confine)
  local asked = false
  for _, request in ipairs(launch.HANDSHAKE) do
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
  answers[control.id] = control.response or false
  for _, request in ipairs(launch.HANDSHAKE) do
    if answers[request.id] == nil then
      return nil, false
    end
  end
  return launch.handshake_problem(answers, opts.deny_read, worker, confine), true
end

-- `TMPDIR` is the user's, so its target is checked first. A cancel never
-- stops `mktemp`, which could leave a directory behind with no known name.
local function probe_dir(env, spec, call)
  local base_out, base_err = call:run_quick({ "pwd", "-P" }, { env = env, cwd = env.TMPDIR or DEFAULT_TMP_DIR })
  local base = trimmed(base_out)
  if not base then
    return nil, base_err or "maki cannot resolve the temporary directory"
  end
  if launch.in_checkout(base, spec) then
    return nil, "the temporary directory " .. base .. " is in the project. Set TMPDIR to a different directory."
  end
  local created
  local unnamed = "`mktemp -d` gave no directory name, and it can have left a directory in " .. base
  local argv = { "mktemp", "-d", maki.fs.joinpath(base, PROBE_DIR_TEMPLATE) }
  local ok, code = pcall(call.run_to_end, call, argv, { env = env, clear_env = true }, {
    keep_on_cancel = true,
    leftover = function(lines)
      created = trimmed(lines[1])
      if call.answered and not created then
        maki.notify(ERROR_PREFIX .. unnamed, "warn")
      end
      return call.answered and created or nil
    end,
  })
  if not ok then
    return nil, "maki cannot run `mktemp`: " .. tostring(code)
  end
  if code ~= 0 or not created then
    return nil, unnamed
  end
  if launch.in_checkout(created, spec) or launch.within(spec.cwd, created) then
    call:remove_dir_now(created, env)
    return nil, "the probe directory " .. created .. " is in the project, or contains it"
  end
  return created
end

local function claude_job_opts(cwd, env, stderr_tail)
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
local function probe(call, spec, argv, env, confine)
  local dir, dir_err = probe_dir(spec.env, spec, call)
  if not dir then
    return dir_err
  end
  local stream = Stream.new({ cwd = dir, worker = spec.worker, cli = spec.cli })
  local answers, problem, stderr_tail = {}, nil, {}
  local ok, code, _, run_error = pcall(call.run_to_end, call, argv, claude_job_opts(dir, env, stderr_tail), {
    leftover = function()
      return dir
    end,
    on_line = function(id, line)
      if problem then
        return
      end
      local step = stream:feed(line)
      if step and step.stop then
        problem = step.stop
      elseif step and step.control then
        problem = take_answer(answers, step.control, spec.worker, confine)
      elseif step then
        problem = PROBE_RUN
      end
      if problem then
        maki.fn.jobstop(id)
      end
    end,
    started = function(id)
      for _, request in ipairs(launch.HANDSHAKE) do
        local sent, err = maki.fn.chansend(id, launch.control_request(request))
        if not sent then
          return nil, err
        end
      end
      return maki.fn.chanclose(id, "stdin")
    end,
  }, STARTUP_MS)
  if not ok then
    call:remove_dir_now(dir, env)
    return "maki cannot start Claude Code: " .. tostring(code)
  end
  if run_error then
    return "maki cannot run Claude Code checks: " .. run_error
  end
  if problem then
    return problem
  end
  if not code then
    return "Claude Code did not send the responses to its checks in " .. STARTUP_TIMEOUT_SECS .. " s"
  end
  if code ~= 0 then
    return jobs.with_stderr("Claude Code exited with code " .. code .. " during its checks", stderr_tail)
  end
  return launch.handshake_problem(answers, opts.deny_read, spec.worker, confine)
end

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

local function snapshot_plan(input)
  local include = {}
  for _, path in ipairs(input.include or {}) do
    local spelled, problem = workspace.relative_path(path)
    if not spelled then
      return nil, problem
    end
    include[#include + 1] = spelled
  end
  local root, root_err = artifacts_root()
  if not root then
    return nil, root_err
  end
  return { include = include, dependencies = dependencies, root = root }
end

-- Deny every checkout alias, git directory, login path and credential path. This includes
-- maki state, config and other artifacts.
--
-- Each sandbox command mounts empty filesystems over denied directories, including artifacts
-- created after startup. Restore write access only to the snapshot and temporary directory.
-- Keep the git directory read-only.
--
-- A read-only mount of the entire artifact would also block snapshot writes. The sandbox
-- skips nonexistent denied paths.
local function coding_confine(spec, artifact)
  local deny = table.clone(spec.checkout_dirs)
  deny[#deny + 1] = spec.config_dir
  deny[#deny + 1] = maki.fs.dirname(artifact.dir)
  -- Optional paths can be nil. Use explicit inserts so later paths remain in the list.
  deny[#deny + 1] = spec.git_dir
  deny[#deny + 1] = maki.env.state_dir()
  deny[#deny + 1] = maki.env.config_dir()
  deny[#deny + 1] = spec.own_config_dir
  table.move(spec.home_credentials, 1, #spec.home_credentials, #deny + 1, deny)
  return { deny_read = deny, allow_read = { artifact.git }, allow_write = { artifact.tmp } }
end

local function fill_snapshot(call, view, plan, artifact, cwd, env)
  view:append({ { SNAPSHOTTING, "dim" } })
  local err = snapshot.fill(call, artifact, {
    cwd = cwd,
    env = env,
    deny_read = opts.deny_read,
    include = plan.include,
    dependencies = plan.dependencies,
  })
  if err then
    return "maki cannot make a snapshot of the project: " .. err
  end
  if opts.prepare ~= "" then
    view:append({ { PREPARING, "dim" } })
    artifact.prepare_err, err =
      snapshot.prepare(call, artifact, opts.prepare, env, opts.prepare_timeout_secs * MS_PER_SEC)
    if err then
      return "maki cannot record the prepared snapshot: " .. err
    end
  end
  view:append({ { STARTING, "dim" } })
  return nil
end

-- The changes stay in the artifact after a failed run too, so the user can
-- look at them.
local function change_report(call, artifact, project)
  local changes, err = snapshot.collect(call, artifact, project)
  local lines = {}
  if not changes then
    lines[1] = "maki could not read the changes of the worker in " .. artifact.snapshot .. ": " .. err
  elseif #changes == 0 then
    lines[1] = "The worker changed no files."
  else
    lines[1] = "Changes in artifact " .. artifact.id .. ", not yet in the checkout:"
    local shown, more = workspace.first(workspace.summary(changes), MAX_SHOWN_CHANGES)
    for _, line in ipairs(shown) do
      lines[#lines + 1] = line
    end
    if more > 0 then
      lines[#lines + 1] = "and " .. more .. " more"
    end
    lines[#lines + 1] = "Examine them in "
      .. artifact.snapshot
      .. ", then apply them with claude_code_import, id "
      .. artifact.id
      .. "."
  end
  if artifact.prepare_err then
    lines[#lines + 1] = "the prepare command stopped with an error: " .. artifact.prepare_err
  end
  for _, note in ipairs(artifact.notes) do
    lines[#lines + 1] = "note: " .. note
  end
  return table.concat(lines, "\n")
end

-- Only a coding call has a `plan`.
local function preflight(input, ctx, call)
  local model = input.model or opts.model
  if not valid_model[model] then
    return nil, "unknown model " .. tostring(model) .. ", use one of " .. table.concat(MODELS, ", ")
  end
  local worker = launch.WORKERS[input.profile or READ_PROFILE]
  if not worker then
    return nil, "unknown profile " .. tostring(input.profile) .. ", use " .. READ_PROFILE .. " or " .. CODE_PROFILE
  end
  local home = maki.uv.os_homedir()
  local plan, plan_err, home_credentials
  if worker == launch.WORKERS[CODE_PROFILE] then
    home_credentials, plan_err = launch.home_credentials(home)
    if not home_credentials then
      return nil, plan_err
    end
    plan, plan_err = snapshot_plan(input)
    if not plan then
      return nil, plan_err
    end
  elseif input.include and #input.include > 0 then
    return nil, "only the " .. CODE_PROFILE .. " profile can use `include`, because it works in a snapshot"
  end
  -- Resolved once here, because the probe runs from another directory.
  local executable = maki.fn.exepath(opts.executable)
  if executable == "" then
    return nil, "Claude Code (`" .. opts.executable .. "`) is not installed, or it is not on PATH"
  end

  -- Under ACP, the session can work in a different checkout from maki's.
  local cwd, cwd_err = ctx:cwd()
  if not cwd then
    return nil, NO_SESSION_DIR .. tostring(cwd_err)
  end
  local instructions, instructions_err = ctx:instructions()
  if not instructions then
    return nil, "maki cannot load the instructions of the session: " .. tostring(instructions_err)
  end
  local env, withheld = launch.child_env(maki.uv.os_environ())
  local secret_err = launch.shell_secret_problem(worker, env)
  if secret_err then
    return nil, secret_err
  end
  -- maki's own login can be another one than `config_dir` picks, and the
  -- coding shell must not read either.
  local own_config_dir = env.CLAUDE_CONFIG_DIR
  if opts.config_dir ~= "" then
    env.CLAUDE_CONFIG_DIR = opts.config_dir
  end
  local config_dir, config_dir_err = launch.config_dir(env.CLAUDE_CONFIG_DIR, home)
  if not config_dir then
    return nil, config_dir_err
  end
  local local_dirs, dirs_err, git_dir = launch.local_settings_dirs(cwd)
  if not local_dirs then
    return nil, dirs_err
  end
  local conflicts = settings_conflicts(launch.skipped_settings(config_dir, cwd, local_dirs))
  if #conflicts > 0 then
    return nil,
      "maki ignores these Claude Code settings, and it cannot ignore them safely:\n" .. table.concat(conflicts, "\n")
  end

  local version, version_err = call:run_quick({ executable, "--version" }, { env = env })
  local cli, cli_err = launch.checked_cli(version, maki.uv.os_uname().sysname)
  if not cli then
    return nil, version_err or cli_err
  end
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

local function open_artifact(call, spec)
  local root, root_err = snapshot.resolve_root(call, spec.plan.root, spec)
  if not root then
    return nil, "maki cannot make a coding artifact: " .. root_err
  end
  snapshot.sweep(root, opts.artifact_ttl_hours)
  local artifact, allocate_err = snapshot.allocate(call, root, spec.env, SNAPSHOT_TIMEOUT_MS)
  if not artifact then
    return nil, "maki cannot make a coding artifact: " .. allocate_err
  end
  return artifact
end

-- Job callbacks only record the result and hand it to the call. A body built
-- inside a job callback would lose its click handler when the callback's
-- scope ends.
local Run = {}
Run.__index = Run

function Run.new(ctx, call, spec, confine, timeout_secs)
  local self = setmetatable({
    ctx = ctx,
    call = call,
    spec = spec,
    confine = confine,
    timeout_secs = timeout_secs,
    route = CHECKING:format(spec.cli.version),
    answers = {},
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
    maki.fn.jobstop(self.clock)
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

function Run:on_answer(control)
  local problem, complete = take_answer(self.answers, control, self.spec.worker, self.confine)
  if problem then
    self:stop(problem)
    return
  elseif not complete then
    return
  end
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
  local step = self.stream:feed(line)
  if not step or self.stopped then
    return
  end
  if step.control then
    self:on_answer(step.control)
  elseif step.stop then
    self:stop(step.stop)
  elseif step.init and not self.prompted then
    self:stop(UNCHECKED_START)
  elseif step.init then
    maki.fn.jobstop(self.clock)
    local subscription = " (" .. self.answers.account.account.subscriptionType .. ")"
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
    return
  end
  if self.stopped then
    self:finish_with(ERROR_PREFIX .. "maki stopped Claude Code: " .. self.stopped, true)
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
  local job_opts = claude_job_opts(cwd, env, self.stderr_tail)
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
  -- A clock job, for the reason claude_jobs.lua gives.
  local startup = jobs.clock(STARTUP_TIMEOUT_SECS)
  self.clock = self.call:spawn(startup, { env = self.spec.env, clear_env = true }, {
    on_exit = function(_, code)
      if not self.stream.accepted then
        self:stop(jobs.clock_problem(code) or "no init event in " .. STARTUP_TIMEOUT_SECS .. " s")
      end
    end,
  })
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

local function run(input, ctx, call, timeout_secs)
  local spec, spec_err = preflight(input, ctx, call)
  if not spec then
    return refuse(spec_err)
  end
  -- The sandbox settings contain the artifact's paths, and the probe checks
  -- the same settings and environment as the run, so the artifact comes
  -- first.
  local artifact, confine
  local env = spec.env
  if spec.plan then
    local artifact_err
    artifact, artifact_err = open_artifact(call, spec)
    if not artifact then
      return refuse(artifact_err)
    end
    call.artifact = artifact
    confine = coding_confine(spec, artifact)
    env = table.clone(spec.env)
    env.TMPDIR = artifact.tmp
  end
  local argv = launch.argv({
    executable = spec.executable,
    model = spec.model,
    deny_read = opts.deny_read,
    worker = spec.worker,
    confine = confine,
  })
  local probe_problem = probe(call, spec, argv, env, confine)
  if probe_problem then
    return refuse(probe_problem)
  end

  local claude = Run.new(ctx, call, spec, confine, timeout_secs)
  if not artifact then
    claude.view:append({ { STARTING, "dim" } })
    return claude:build(claude:start(argv, spec.cwd, env, input.prompt))
  end
  claude.artifact = artifact
  local fill_err = fill_snapshot(call, claude.view, spec.plan, artifact, spec.cwd, spec.env)
  if fill_err then
    return refuse(fill_err)
  end
  local task = input.prompt
  if artifact.prepare_err then
    task = task .. "\n\n" .. PREPARE_FAILED:format(artifact.prepare_err)
  end
  if artifact.unsettled then
    task = task .. "\n\n" .. UNSETTLED:format(artifact.unsettled)
  end
  local outcome = claude:start(argv, artifact.snapshot, env, task)
  if not claude.prompted then
    return claude:build(outcome)
  end
  local report = change_report(call, artifact, spec.cwd)
  call.artifact = nil
  -- The report identifies the artifact necessary for import. Shorten the reply first so
  -- output limits cannot remove the report.
  local report_lines = select(2, report:gsub("\n", "")) + 1
  local answer_lines = math.max(claude.max_lines - report_lines - 1, 1)
  local answer_bytes = math.max(claude.max_bytes - #report - 2, 1)
  outcome.llm_output = truncate(outcome.shown, answer_lines, answer_bytes) .. "\n\n" .. report
  outcome.shown = outcome.shown .. "\n\n" .. report
  return claude:build(outcome)
end

-- Wait for all processes before artifact removal. A stopped copy can still write until its
-- process exits.
local function discard_unkept(call)
  local artifact = call.artifact
  if artifact then
    call.artifact = nil
    call:stop()
    call:when_idle(function()
      snapshot.discard(artifact)
    end)
  end
end

local function handler(input, ctx)
  if type(input.prompt) ~= "string" or input.prompt:match("^%s*$") then
    return refuse("the call must have a prompt")
  end

  local timeout_secs = launch.task_timeout(input.timeout, opts.timeout_secs)

  callers = callers + 1
  local permit
  local call = jobs.new(STARTUP_MS, function()
    callers = callers - 1
    if permit then
      permit:release()
    end
  end)
  -- A cancel stops this coroutine where it waits, so the hook finishes the
  -- call.
  maki.async.on_cancel(function()
    call:answer()
    discard_unkept(call)
  end)
  if callers > opts.max_concurrent then
    local buf = maki.ui.buf()
    buf:line({ { WAITING, "dim" } })
    ctx:live_buf(buf)
  end
  permit = semaphore:acquire()
  -- Start the timeout only after slot acquisition.
  ctx:set_deadline(timeout_secs)

  local ok, reply = pcall(run, input, ctx, call, timeout_secs)
  if not ok then
    call:stop()
  end
  discard_unkept(call)
  if ok and #call.left_behind > 0 then
    reply.llm_output = reply.llm_output .. "\n" .. jobs.LEFT_BEHIND .. table.concat(call.left_behind, ", ")
    call.left_behind = {}
  end
  call:answer()
  if not ok then
    error(reply, 0)
  end
  return reply
end

local function header(input)
  local model = input.model or opts.model
  local profile = input.profile == CODE_PROFILE and ", " .. CODE_PROFILE or ""
  return (input.description or "Claude Code") .. " (" .. model .. profile .. ")"
end

local function restore(_input, output, is_error, ctx)
  local state = ctx:state()
  return render(state and state.header or {}, output, is_error, ctx)
end

maki.api.register_tool({
  name = "claude_code",
  description = description,
  kind = "execute",
  audiences = { "main" },
  schema = {
    type = "object",
    required = { "prompt" },
    additionalProperties = false,
    properties = {
      prompt = { type = "string", description = "The full task for Claude, with all the data that it must have" },
      description = { type = "string", description = "A short name for the task (3 to 5 words)" },
      model = { type = "string", enum = MODELS, description = "The Claude model. The default is " .. opts.model .. "." },
      timeout = {
        type = "integer",
        minimum = launch.MIN_TIMEOUT_SECS,
        maximum = launch.MAX_TIMEOUT_SECS,
        description = "The timeout in seconds. The default is " .. opts.timeout_secs .. ".",
      },
      profile = {
        type = "string",
        enum = { READ_PROFILE, CODE_PROFILE },
        description = "The permissions of Claude. The default is " .. READ_PROFILE .. ".",
      },
      include = {
        type = "array",
        items = { type = "string" },
        description = "Untracked project paths that a " .. CODE_PROFILE .. " task uses, for example test fixtures",
      },
    },
  },
  permission = "run",
  permission_scopes = function(input)
    local scope = SCOPE:format(input.profile or READ_PROFILE, input.model or opts.model)
    return { scopes = { scope }, force_prompt = false }
  end,
  handler = handler,
  header = header,
  restore = restore,
})

local function import_handler(input, ctx)
  local artifacts, artifacts_err = artifacts_root()
  if not artifacts then
    return refuse(artifacts_err)
  end
  local project, project_err = ctx:cwd()
  if not project then
    return refuse(NO_SESSION_DIR .. tostring(project_err))
  end
  local result, err = snapshot.import(ctx, jobs.new(STARTUP_MS), {
    id = input.id,
    paths = input.paths,
    project = project,
    root = artifacts,
    env = (launch.child_env(maki.uv.os_environ())),
  })
  if not result then
    return refuse(err)
  end
  local lines = {}
  for _, section in ipairs({
    { "Imported:", result.applied },
    { "Not imported:", result.skipped },
  }) do
    if #section[2] > 0 then
      lines[#lines + 1] = section[1]
      for _, line in ipairs(section[2]) do
        lines[#lines + 1] = line
      end
    end
  end
  if result.originals then
    lines[#lines + 1] = "The previous versions stay in "
      .. result.originals
      .. " and "
      .. result.displaced
      .. " until maki removes the artifact."
  end
  if result.unrecorded then
    lines[#lines + 1] = result.unrecorded
  end
  if result.failure then
    lines[#lines + 1] = "Every change landed, but the command failed afterwards:\n" .. result.failure
  end
  return #lines > 0 and table.concat(lines, "\n") or "There are no changes to import."
end

maki.api.register_tool({
  name = "claude_code_import",
  description = import_description,
  kind = "edit",
  audiences = { "main" },
  schema = {
    type = "object",
    required = { "id" },
    additionalProperties = false,
    properties = {
      id = { type = "string", description = "The artifact id from a claude_code reply" },
      paths = {
        type = "array",
        items = { type = "string" },
        description = "Import only these changed paths, as the reply shows them",
      },
    },
  },
  handler = import_handler,
})
