local native = require("maki.claude_code.internal")

-- Claude Code's `stream-json` output. maki ignores all model output until an
-- init event matches the run's settings, and checks any later init event
-- again.

local Stream = {}
Stream.__index = Stream

local ASSISTANT = "assistant"
local RESULT = "result"
local CONTROL_RESPONSE = "control_response"
local INIT = "init"
local TOOL_USE = "tool_use"
-- Each step's output count is a placeholder, so only the result has the
-- real count.
local OUTPUT_FIELD = "output_tokens"
local USAGE_FIELDS = {
  { key = "input_tokens", label = "in" },
  { key = "cache_read_input_tokens", label = "cache read" },
  { key = "cache_creation_input_tokens", label = "cache write" },
  { key = OUTPUT_FIELD, label = "out" },
}
local THOUSAND = 1000
local NO_ANSWER = "Claude Code completed its run without a reply"
local PARTIAL = "at least: "
local UNREADABLE_DENIAL = "a denial that maki cannot read"
local PLAN_LIMITS_USED = "plan limits used: "
local EXTRA_USAGE = "extra usage on"

local function count(n)
  if n >= THOUSAND then
    return string.format("%.1fk", n / THOUSAND)
  end
  return tostring(n)
end

--- {expect}: `cwd`, `cli`, `worker`.
function Stream.new(expect)
  return setmetatable({
    expect = expect,
    accepted = false,
    model_checked = false,
    usage_by_message = {},
    result = nil,
    rate_limit = nil,
  }, Stream)
end

function Stream:set_model(model)
  self.model = model
  self.model_checked = false
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
---   `{ control = line }`  a handshake response
---   `{ stop = reason }`  kill the child, the run is unusable
---   `{ init = event }`   the checks found no problem in the child
---   `{ lines = {...} }`  status lines for the view
function Stream:feed(line)
  local classified, problem = native.classify(line, {
    init = self.expect,
    accepted = self.accepted,
    model = self.model,
    model_checked = self.model_checked,
  })
  if not classified then
    return { stop = problem }
  end
  local ev, kind = classified.event, classified.kind
  self.model_checked = classified.model_checked
  if kind == ASSISTANT and type(ev.message) == "table" and type(ev.message.id) == "string" and classified.usage then
    self.usage_by_message[ev.message.id] = classified.usage
  end
  if classified.problem then
    local init = kind == INIT
    if init then
      self.accepted = false
    end
    return { stop = classified.problem, init = init and ev or nil }
  end
  if kind == CONTROL_RESPONSE and type(ev.response) == "table" then
    return { control = line }
  end
  if classified.rate_limit then
    self.rate_limit = classified.rate_limit
    return nil
  end
  if kind == INIT then
    self.accepted = true
    return { init = ev }
  end
  if kind == RESULT then
    self.result = ev
    self.failure = classified.failure
    ev.usage = classified.usage
    if not self.accepted then
      return { stop = self:outcome() }
    end
    return nil
  end
  if kind ~= ASSISTANT or type(ev.message) ~= "table" or type(ev.message.content) ~= "table" then
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
  if self.failure then
    return self.failure, true
  end
  if type(r.result) ~= "string" or r.result:match("^%s*$") then
    return NO_ANSWER, true
  end
  return r.result, false
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
      local n = usage[key]
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
  local reported = self.result and self.result.usage or {}
  local totals, partial = {}, false
  for _, field in ipairs(USAGE_FIELDS) do
    local total = reported[field.key]
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
  local parts, overage = {}, false
  for _, limit in ipairs(info.limits or {}) do
    if limit.percentage then
      parts[#parts + 1] = string.format("%d%% of %s", limit.percentage, limit.label)
    elseif limit.detail then
      overage = true
    end
  end
  local line = #parts > 0 and PLAN_LIMITS_USED .. table.concat(parts, ", ") or nil
  if overage then
    line = line and line .. " · " .. EXTRA_USAGE or EXTRA_USAGE
  end
  return line
end

return Stream
