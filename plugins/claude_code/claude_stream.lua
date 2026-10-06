-- Claude Code's `stream-json` output. maki ignores all model output until an
-- init event matches the run's settings, and checks any later init event
-- again.

local launch = require("claude_launch")
local RULES = require("claude_rules")

local Stream = {}
Stream.__index = Stream

local ASSISTANT = "assistant"
local USER = "user"
local RESULT = "result"
local STREAM_EVENT = "stream_event"
local SYSTEM = "system"
local CONTROL_RESPONSE = "control_response"
local RATE_LIMIT_EVENT = "rate_limit_event"
local INIT = "init"
local HOOK_STARTED = "hook_started"
local TOOL_USE = "tool_use"
local TOOL_PROGRESS = "tool_progress"
local KEEP_ALIVE = "keep_alive"
local MODEL_OUTPUT = { [ASSISTANT] = true, [USER] = true, [RESULT] = true, [STREAM_EVENT] = true }
-- Any other line means a different protocol, so the run stops.
local KNOWN_EVENTS = {
  [SYSTEM] = true,
  [CONTROL_RESPONSE] = true,
  [RATE_LIMIT_EVENT] = true,
  [TOOL_PROGRESS] = true,
  [KEEP_ALIVE] = true,
}
for kind in pairs(MODEL_OUTPUT) do
  KNOWN_EVENTS[kind] = true
end
local SHOWN_LINE_CHARS = 80
local WINDOW_NAMES = { five_hour = "5h", seven_day = "7d" }
-- Each step's output count is a placeholder, so only the result has the
-- real count.
local OUTPUT_FIELD = "output_tokens"
local USAGE_FIELDS = {
  { key = "input_tokens", label = "in" },
  { key = "cache_read_input_tokens", label = "cache read" },
  { key = "cache_creation_input_tokens", label = "cache write" },
  { key = OUTPUT_FIELD, label = "out" },
}
local PERCENT = 100
local THOUSAND = 1000
local SUCCESS = "success"
local NO_ANSWER = "Claude Code completed its run without a reply"
local PARTIAL = "at least: "
local UNREADABLE_DENIAL = "a denial that maki cannot read"
local PLAN_LIMITS_USED = "plan limits used: "
local EXTRA_USAGE = "extra usage on"

local function line_start(line)
  local cut = utf8.offset(line, SHOWN_LINE_CHARS + 1)
  return cut and line:sub(1, cut - 1) or line
end

local function init_problem(ev, expect)
  -- `none` also covers a bearer token or a cloud provider, which the child
  -- environment and the `initialize` response have already ruled out.
  if ev.apiKeySource ~= RULES.no_key_source then
    return "Claude Code started with API key source "
      .. tostring(ev.apiKeySource)
      .. " instead of the subscription login"
  end
  if ev.claude_code_version ~= expect.cli.version then
    return "Claude Code reported version "
      .. tostring(ev.claude_code_version)
      .. ", but maki checked version "
      .. expect.cli.version
      .. " before the start"
  end
  if not expect.permission_modes[ev.permissionMode] then
    return "Claude Code runs in permission mode " .. tostring(ev.permissionMode)
  end
  if not launch.is_list(ev.tools) then
    return "the init event has no tool list"
  end
  local expected, offered = launch.set_of(expect.tools), launch.set_of(ev.tools)
  for _, tool in ipairs(ev.tools) do
    if not expected[tool] then
      return "Claude Code has the tool " .. tostring(tool)
    end
  end
  for _, tool in ipairs(expect.tools) do
    if not offered[tool] then
      return "Claude Code does not have the tool " .. tool
    end
  end
  if not launch.is_list(ev.mcp_servers) then
    return "the init event has no MCP server list"
  end
  if #ev.mcp_servers > 0 then
    return "Claude Code loaded MCP servers"
  end
  local plugin_problem = launch.plugins_problem(ev.plugins)
  if plugin_problem then
    return plugin_problem
  end
  if ev.cwd ~= expect.cwd then
    return "Claude Code runs in " .. tostring(ev.cwd) .. ", and not in " .. expect.cwd
  end
  return nil
end

local function count(n)
  if n >= THOUSAND then
    return string.format("%.1fk", n / THOUSAND)
  end
  return tostring(n)
end

--- {expect}: `cwd`, `cli`, `worker`.
function Stream.new(expect)
  return setmetatable({
    expect = {
      cwd = expect.cwd,
      tools = expect.worker.tools,
      permission_modes = expect.worker.reported_modes,
      cli = expect.cli,
    },
    accepted = false,
    usage_by_message = {},
    result = nil,
    rate_limit = nil,
  }, Stream)
end

-- Relative inside the project and absolute outside it, where a run of `..`
-- would hide which file it is.
function Stream:shown_path(path)
  local rel = maki.fs.relpath(self.expect.cwd, path)
  return rel:sub(1, 2) == ".." and path or rel
end

-- Only a string is used. Anything else would raise in the job's exit
-- handler and turn a good reply into an error.
local function text_field(input, key)
  local value = input[key]
  return type(value) == "string" and value or nil
end

function Stream:tool_line(name, input)
  input = type(input) == "table" and input or {}
  local file_path, path = text_field(input, "file_path"), text_field(input, "path")
  local target = file_path and self:shown_path(file_path) or text_field(input, "pattern") or ""
  if path then
    target = target .. " in " .. self:shown_path(path)
  end
  return tostring(name) .. " " .. target
end

--- Returns what to do with one stdout line, or nil:
---   `{ control = { id, ok, response, error } }`  a handshake response
---   `{ stop = reason }`  kill the child, the run is unusable
---   `{ init = event }`   the checks found no problem in the child
---   `{ lines = {...} }`  status lines for the view
function Stream:feed(line)
  local ev = maki.json.decode(line)
  if type(ev) ~= "table" or type(ev.type) ~= "string" then
    return { stop = "Claude Code printed a line that is not an event: " .. line_start(line) }
  end
  if not KNOWN_EVENTS[ev.type] then
    return { stop = "Claude Code sent an unknown event type: " .. tostring(ev.type) }
  end
  if ev.type == ASSISTANT and type(ev.message) == "table" then
    local id, usage = ev.message.id, ev.message.usage
    if type(id) == "string" and type(usage) == "table" then
      self.usage_by_message[id] = usage
    end
  end
  if ev.type == CONTROL_RESPONSE and type(ev.response) == "table" then
    local r = ev.response
    return { control = { id = r.request_id, ok = r.subtype == SUCCESS, response = r.response, error = r.error } }
  end
  if ev.type == RATE_LIMIT_EVENT and type(ev.rate_limit_info) == "table" then
    self.rate_limit = ev.rate_limit_info
    return nil
  end
  -- Every hook is turned off, so a hook that starts means policy kept it on.
  if ev.type == SYSTEM and ev.subtype == HOOK_STARTED then
    return { stop = "Claude Code started a " .. tostring(ev.hook_event) .. " hook" }
  end
  if ev.type == SYSTEM and ev.subtype == INIT then
    local problem = init_problem(ev, self.expect)
    if problem then
      self.accepted = false
      return { stop = problem }
    end
    self.accepted = true
    return { init = ev }
  end
  if not MODEL_OUTPUT[ev.type] then
    return nil
  end
  if not self.accepted then
    return { stop = "Claude Code sent " .. ev.type .. " output before its init event" }
  end
  if ev.type == RESULT then
    self.result = ev
    return nil
  end
  if ev.type ~= ASSISTANT or type(ev.message) ~= "table" or type(ev.message.content) ~= "table" then
    return nil
  end
  local lines = {}
  for _, block in ipairs(ev.message.content) do
    if type(block) == "table" and block.type == TOOL_USE then
      lines[#lines + 1] = self:tool_line(block.name, block.input)
    end
  end
  return #lines > 0 and { lines = lines } or nil
end

function Stream:outcome()
  local r = self.result
  if r.subtype == SUCCESS and not r.is_error then
    if type(r.result) ~= "string" or r.result:match("^%s*$") then
      return NO_ANSWER, true
    end
    return r.result, false
  end
  -- A non-string would raise in the job's exit handler and the call would
  -- never finish.
  local detail = type(r.result) == "string" and r.result or ""
  if detail == "" and type(r.errors) == "table" then
    local errors = {}
    for _, entry in ipairs(r.errors) do
      errors[#errors + 1] = type(entry) == "string" and entry or maki.json.encode(entry)
    end
    detail = table.concat(errors, "; ")
  end
  local msg = "Claude Code stopped with an error (" .. tostring(r.subtype) .. ")"
  if detail ~= "" then
    msg = msg .. ": " .. detail
  end
  return msg, true
end

function Stream:denials()
  local lines = {}
  local denials = self.result and self.result.permission_denials
  for _, d in ipairs(type(denials) == "table" and denials or {}) do
    lines[#lines + 1] = type(d) == "table" and self:tool_line(d.tool_name, d.tool_input) or UNREADABLE_DENIAL
  end
  return lines
end

-- Input and cache counts over all messages, each message counted once. A
-- sum is known only when every message reports that field.
function Stream:measured()
  if next(self.usage_by_message) == nil then
    return nil
  end
  local sums = {}
  for _, field in ipairs(USAGE_FIELDS) do
    if field.key ~= OUTPUT_FIELD then
      sums[field.key] = 0
    end
  end
  for _, usage in pairs(self.usage_by_message) do
    for key, sum in pairs(sums) do
      local n = tonumber(usage[key])
      sums[key] = n and sum + n or nil
    end
  end
  return sums
end

-- A missing key means the value is unknown, and `partial` marks a field
-- that is below the run's total. The result's total replaces a sum when it
-- has one, so a result without usage keeps the counts maki already has.
function Stream:usage_totals()
  local measured = self:measured()
  if not self.result and not measured then
    return nil
  end
  local reported = self.result and type(self.result.usage) == "table" and self.result.usage or {}
  local totals, partial = {}, false
  for _, field in ipairs(USAGE_FIELDS) do
    local total = tonumber(reported[field.key])
    if not total then
      partial = true
      total = measured and measured[field.key]
    end
    totals[field.key] = total
  end
  return totals, partial
end

--- The USD value is the API list price of the tokens.
function Stream:usage_line()
  local totals, partial = self:usage_totals()
  if not totals then
    return nil
  end
  local parts = {}
  for _, field in ipairs(USAGE_FIELDS) do
    local n = totals[field.key]
    parts[#parts + 1] = n and count(n) .. " " .. field.label or field.label .. " unknown"
  end
  local cost = self.result and tonumber(self.result.total_cost_usd)
  if cost then
    parts[#parts + 1] = string.format("$%.2f at the API list price", cost)
  end
  return (partial and PARTIAL or "") .. table.concat(parts, " · ")
end

--- Extra usage is billed on top of the plan, so it is always shown.
function Stream:limits_line()
  local info = self.rate_limit
  if not info then
    return nil
  end
  local windows = type(info.unifiedWindows) == "table" and info.unifiedWindows or {}
  local names = {}
  for name in pairs(windows) do
    names[#names + 1] = name
  end
  table.sort(names)
  local parts = {}
  for _, name in ipairs(names) do
    local window = windows[name]
    local used = type(window) == "table" and tonumber(window.utilization)
    if used then
      parts[#parts + 1] = string.format("%d%% of %s", math.round(used * PERCENT), WINDOW_NAMES[name] or name)
    end
  end
  local line = #parts > 0 and PLAN_LIMITS_USED .. table.concat(parts, ", ") or nil
  if info.isUsingOverage == true then
    line = line and line .. " · " .. EXTRA_USAGE or EXTRA_USAGE
  end
  return line
end

return Stream
