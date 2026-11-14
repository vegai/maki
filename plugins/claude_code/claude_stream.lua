local native = require("maki.claude_code.internal")

-- Claude Code's `stream-json` output. maki ignores all model output until an
-- init event matches the run's settings, and checks any later init event
-- again.

local Stream = {}
Stream.__index = Stream

local ASSISTANT = "assistant"
local RESULT = "result"
local INIT = "init"
local USAGE_FIELDS = {
  { key = "input_tokens", label = "in" },
  { key = "cache_read_input_tokens", label = "cache read" },
  { key = "cache_creation_input_tokens", label = "cache write" },
  { key = "output_tokens", label = "out" },
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

--- {expect}: `cwd`, `version`, `policy`, and optional `executable`.
function Stream.new(expect)
  return setmetatable({
    expect = expect,
    native = native.worker_stream(expect),
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

--- Returns a control completion, stop reason, init event or progress lines; nil means no update.
function Stream:feed(line, handshake)
  local classified, problem = self.native:feed(line, handshake)
  if not classified then
    return { stop = problem }
  end
  local ev, kind = classified.event, classified.kind
  if classified.problem then
    return { stop = classified.problem }
  end
  if classified.control then
    return { control = true, complete = classified.complete }
  end
  if classified.rate_limit then
    self.rate_limit = classified.rate_limit
    return nil
  end
  if kind == INIT then
    return { init = true }
  end
  if kind == RESULT then
    self.result = ev
    self.failure = classified.failure
    if not self.native:accepted() then
      return { stop = self:outcome() }
    end
    return nil
  end
  if kind ~= ASSISTANT or type(ev.message) ~= "table" or type(ev.message.content) ~= "table" then
    return nil
  end
  local lines = {}
  for _, block in ipairs(ev.message.content) do
    lines[#lines + 1] = self:tool_line(block.name, block.input)
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

--- The USD value is the API list price of the tokens.
function Stream:usage_line()
  local totals, partial = self.native:usage_totals()
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
