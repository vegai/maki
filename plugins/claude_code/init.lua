-- Send the task only after the probe and worker pass the handshake.
--
-- Managed hooks can run at startup. Run the probe in an empty directory so a rejected hook
-- cannot write to the project.

local native = require("maki.claude_code.internal")
local output_limits = require("maki.output_limits")
local truncate = require("maki.truncate")
local jobs = require("claude_jobs")
local launch = require("claude_launch")
local snapshot = require("claude_snapshot")
local importer = require("claude_import")
local Run = require("claude_run")
local workspace = require("claude_workspace")
local git_utils = require("claude_git")
local path_utils = require("claude_paths")

local MODELS = launch.MODELS
local MS_PER_SEC = jobs.MS_PER_SEC
local STARTUP_MS = launch.STARTUP_MS
local NO_STATE_DIR = "maki cannot find its state directory as a UTF-8 path for the coding sandbox"
-- Each step of a coding snapshot, such as the copy, must finish in this time.
local SNAPSHOT_TIMEOUT_MS = 120 * MS_PER_SEC
local MAX_SHOWN_CHANGES = 50
local ARTIFACTS_SUBDIR = "claude_code"
local ARTIFACTS_LEAF = "changes"
local READ_PROFILE = launch.READ_PROFILE
local CODE_PROFILE = launch.CODE_PROFILE
local ERROR_PREFIX = jobs.ERROR_PREFIX
local STARTING = "Start Claude Code..."
local SNAPSHOTTING = "Create a snapshot of the project..."
local PREPARING = "Run the prepare command..."
-- A permission scope: "claude <profile> <model>".
local SCOPE = "claude %s %s"
local WAITING = "Wait for a free Claude Code slot..."
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
  executable = { default = native.default_executable, desc = "The Claude Code executable." },
  config_dir = {
    default = "",
    desc = "Absolute path of a Claude Code config directory for maki, in place of "
      .. "`$CLAUDE_CONFIG_DIR` or `~/.claude`. Use it to give maki a different Claude login.",
  },
  model = { default = "sonnet", desc = "Model alias for calls that do not pick one." },
  max_concurrent = {
    default = 2,
    min = 1,
    desc = "The maximum number of claude_code tool calls running at the same time.",
  },
  provider_max_concurrent = {
    default = native.default_provider_max_concurrent,
    min = native.min_provider_max_concurrent,
    desc = "The maximum number of concurrent claude-code provider generations, including subagents. "
      .. "Independent of max_concurrent. Model listing also takes one slot. Queued generations show a waiting status. Changes to this limit apply to new requests. Active and already queued requests keep their previous limit.",
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
      .. "depth. They add to the default `.env*` and `secrets` rules, and coding snapshots never contain them.",
  },
  deny_read_home = {
    default = "",
    desc = "Extra home-relative files or directories the coding shell cannot read, comma-separated. "
      .. "Common credential stores are denied by default. Claude Code also hides home paths outside the snapshot.",
  },
  dependencies = {
    default = "",
    desc = "Untracked directories a coding snapshot copies from the project, comma-separated, for example "
      .. "`node_modules`. The copy is a copy-on-write clone when the filesystem supports it.",
  },
  prepare = {
    default = "",
    desc = "Shell command to run in each coding snapshot before Claude starts, for example "
      .. "`npm ci` or `python3 -m venv --copies .venv && .venv/bin/pip install -e .`. It runs as you, outside the "
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

if not launch.MODEL_SET[opts.model] then
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
  if path_utils.climbs(opts.artifact_dir) then
    error("the `artifact_dir` option must not contain `..`")
  end
end
local dependencies, dependencies_err = path_utils.dependency_dirs(opts.dependencies)
if not dependencies then
  error("the `dependencies` option: " .. dependencies_err)
end
local _, home_deny_err = launch.denied_home_paths(opts.deny_read_home)
if home_deny_err then
  error("the `deny_read_home` option: " .. home_deny_err)
end
local _, denied_err = path_utils.denied_paths(opts.deny_read)
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

local function snapshot_plan(input)
  local include = {}
  for _, path in ipairs(input.include or {}) do
    local spelled, problem = path_utils.relative_path(path)
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
  local state_dir = maki.env.state_dir()
  if not state_dir then
    return nil, NO_STATE_DIR
  end
  local deny = table.clone(spec.denied)
  deny[#deny + 1] = spec.config_dir
  deny[#deny + 1] = maki.fs.dirname(artifact.dir)
  -- Optional paths can be nil. Use explicit inserts so later paths remain in the list.
  deny[#deny + 1] = state_dir
  deny[#deny + 1] = maki.env.config_dir()
  deny[#deny + 1] = maki.env.logs_dir()
  for _, name in ipairs({ "XDG_RUNTIME_DIR", "SSH_AUTH_SOCK" }) do
    local path = maki.uv.os_getenv(name)
    if path and path:sub(1, 1) == "/" then
      deny[#deny + 1] = path
      local resolved = maki.fs.realpath(path)
      if resolved and resolved ~= path then
        deny[#deny + 1] = resolved
      end
    end
  end
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
    err = snapshot.prepare(call, artifact, opts.prepare, env, opts.prepare_timeout_secs * MS_PER_SEC)
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
    local shown, more = path_utils.first(workspace.summary(changes), MAX_SHOWN_CHANGES)
    for _, line in ipairs(shown) do
      lines[#lines + 1] = line
    end
    if more > 0 then
      lines[#lines + 1] = "and " .. more .. " more"
    end
    lines[#lines + 1] = "Review the recorded changes with: " .. git_utils.diff_command(artifact)
    lines[#lines + 1] = "Apply them with claude_code_import, id " .. artifact.id .. "."
  end
  if artifact.prepare_err then
    lines[#lines + 1] = "the prepare command stopped with an error: " .. artifact.prepare_err
  end
  for _, note in ipairs(artifact.notes) do
    lines[#lines + 1] = "note: " .. note
  end
  return table.concat(lines, "\n")
end

local function open_artifact(call, spec, root)
  local root, root_err = snapshot.resolve_root(root, spec)
  if not root then
    return nil, "maki cannot make a coding artifact: " .. root_err
  end
  local sweep = jobs.new(SNAPSHOT_TIMEOUT_MS)
  snapshot.sweep(sweep, root, opts.artifact_ttl_hours)
  sweep:answer()
  local artifact, allocate_err = snapshot.allocate(call, root, spec.env, SNAPSHOT_TIMEOUT_MS)
  if not artifact then
    return nil, "maki cannot make a coding artifact: " .. allocate_err
  end
  return artifact
end

local function run(input, ctx, call, timeout_secs)
  local plan
  if input.profile == CODE_PROFILE then
    local plan_err
    plan, plan_err = snapshot_plan(input)
    if not plan then
      return refuse(plan_err)
    end
  end
  local spec, spec_err = launch.preflight(input, ctx, call, opts)
  if not spec then
    return refuse(spec_err)
  end
  -- The sandbox settings contain the artifact's paths, and the probe checks
  -- the same settings and environment as the run, so the artifact comes
  -- first.
  local artifact, confine
  local env = spec.env
  if plan then
    local artifact_err
    artifact, artifact_err = open_artifact(call, spec, plan.root)
    if not artifact then
      return refuse(artifact_err)
    end
    call.artifact = artifact
    local confine_err
    confine, confine_err = coding_confine(spec, artifact)
    if not confine then
      return refuse(confine_err)
    end
    env = table.clone(spec.env)
    env.TMPDIR = artifact.tmp
  end
  local argv = launch.argv({
    executable = spec.executable,
    model = spec.model,
    deny_read = spec.deny,
    worker = spec.worker,
    confine = confine,
  })
  local probe_problem = launch.probe(call, spec, argv, env, confine, spec.deny)
  if probe_problem then
    return refuse(probe_problem)
  end

  local claude = Run.new(ctx, call, spec, confine, timeout_secs, opts)
  if not artifact then
    claude.view:append({ { STARTING, "dim" } })
    return claude:build(claude:start(argv, spec.cwd, env, input.prompt))
  end
  claude.artifact = artifact
  local fill_err = fill_snapshot(call, claude.view, plan, artifact, spec.cwd, env)
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
  for _, note in ipairs(artifact.notes) do
    task = task .. "\n\nnote: " .. note
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
  local answer_lines = claude.max_lines and math.max(claude.max_lines - report_lines - 1, 1)
  local answer_bytes = claude.max_bytes and math.max(claude.max_bytes - #report - 2, 1)
  outcome.llm_output = truncate(outcome.shown, answer_lines, answer_bytes) .. "\n\n" .. report
  outcome.shown = outcome.shown .. "\n\n" .. report
  return claude:build(outcome)
end

-- Wait for all processes before artifact removal. A stopped copy can still write until its
-- process exits.
local function discard_unkept(call, removed)
  local artifact = call.artifact
  if artifact then
    call.artifact = nil
    call:when_idle(function()
      snapshot.discard(call, artifact, removed)
    end)
  elseif removed then
    removed()
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
    call:stop()
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
  maki.async.await(1, function(done)
    discard_unkept(call, function()
      done(true)
    end)
  end)
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
  return Run.render(state and state.header or {}, output, is_error, ctx)
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
    return refuse(launch.NO_SESSION_DIR .. tostring(project_err))
  end
  local result, err = importer.import(ctx, jobs.new(STARTUP_MS), {
    id = input.id,
    paths = input.paths,
    project = project,
    root = artifacts,
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
