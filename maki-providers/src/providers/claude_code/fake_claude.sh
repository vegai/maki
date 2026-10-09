#!/usr/bin/env bash
dir=@DIR@
echo $$ > "$dir/pid"
version=$(cat "$dir/version" 2>/dev/null || echo "2.1.284 (Claude Code)")
if [ "$1" = --version ]; then
  echo version >> "$dir/versions"
  if [ -e "$dir/version_error" ]; then cat "$dir/version_error" >&2; exit 1; fi
  echo "$version"; exit 0
fi
echo start >> "$dir/calls"
hooks='[]'
if [ -e "$dir/policy_hook" ]; then
  hooks='[{"event":"SessionStart","source":"policySettings","type":"command"}]'
  pwd -P >> "$dir/hook_ran_in"
fi
printf '%s\n' "$@" > "$dir/argv"
flags=; mcp=; prompt=; model=; prev=
for arg; do
  case $prev in
    --model) model=$arg ;;
    --settings) flags=$arg ;;
    --mcp-config) mcp=$arg ;;
    --system-prompt-file) prompt=$arg ;;
  esac
  prev=$arg
done
if [ -f "$prompt" ]; then
  stat -c '%a' "$prompt" "$(dirname "$prompt")" > "$dir/modes"
  printf '%s' "$prompt" > "$dir/prompt_file"
fi
cwd=$(pwd -P)
scenario=$(cat "$dir/scenario")
run=; [ -f "$mcp" ] && run=1
[ -n "$run" ] && printf '%s' "${CLAUDE_CODE_MAX_OUTPUT_TOKENS-unset}" > "$dir/max_output"
[ -n "$run" ] && printf '%s' "${DISABLE_AUTO_COMPACT-unset}" > "$dir/auto_compact"
[ -n "$run" ] && printf '%s' "${CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK-unset}" > "$dir/nonstreaming_fallback"
[ -n "$run" ] && printf '%s' "${CLAUDE_CODE_MAX_RETRIES-unset}" > "$dir/max_retries"
[ -n "$run" ] && printf '%s' "${CLAUDE_CODE_MODEL_CAPABILITIES-unset}" > "$dir/capabilities"
[ -n "$run" ] && cp "$mcp" "$dir/mcp_config"
[ -n "$run" ] && printf '%s' "${MAX_THINKING_TOKENS-unset}" > "$dir/thinking_budget"
[ -n "$run" ] && printf '%s' "${CLAUDE_CODE_DISABLE_THINKING-unset}" > "$dir/thinking_off"
line() { printf '%s\n' "$1"; }
reply() { line "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$1\",\"response\":$2}}"; }
init() { line "{\"type\":\"system\",\"subtype\":\"init\",\"apiKeySource\":\"$1\",\"claude_code_version\":\"${version%% *}\",\"permissionMode\":\"default\",\"tools\":[\"mcp__maki__read\"],\"mcp_servers\":[{\"name\":\"maki\",\"status\":\"connected\"}],\"plugins\":[],\"cwd\":\"$cwd\"}"; }
event() { line "{\"type\":\"stream_event\",\"event\":$1}"; }
block() { line "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_1\",\"content\":[$1]}}"; }
call() { printf '{"type":"tool_use","id":"%s","name":"mcp__maki__read","input":{"path":"%s"}}' "$1" "$2"; }
start_usage=',"usage":{"input_tokens":11,"cache_read_input_tokens":222,"cache_creation_input_tokens":33,"output_tokens":1}'
final_usage=',"usage":{"output_tokens":42}'
case $scenario in
  no_final_usage | stalled_result_without_usage) final_usage= ;;
esac
generation() {
  event "{\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-haiku-4-5-20251001\"$start_usage}}"
  event '{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}'
  event '{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Reading."}}'
  block '{"type":"text","text":"Reading."}'
  event '{"type":"content_block_stop","index":0}'
  if [ "$1" = tool_use ]; then
    event '{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp__maki__read","input":{}}}'
    block "$(call toolu_1 a)"
    event '{"type":"content_block_stop","index":1}'
    event '{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"mcp__maki__read","input":{}}}'
    block "$(call toolu_2 b)"
    event '{"type":"content_block_stop","index":2}'
  fi
  event "{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"$1\"}$final_usage}"
  event '{"type":"message_stop"}'
}
text_reply() { init none; generation end_turn; line '{"type":"result","subtype":"success","is_error":false,"result":"Reading."}'; }
limits() { line '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.45,"resetsAt":1790268000},"seven_day":{"utilization":0.15,"resetsAt":1790521200}}}}'; }
if [ -f "$mcp" ]; then
  port=$(sed -n 's/.*127\.0\.0\.1:\([0-9]*\).*/\1/p' "$mcp")
  token=$(sed -n 's/.*Bearer \([A-Za-z0-9_-]*\).*/\1/p' "$mcp")
  printf '%s' "$token" > "$dir/token"
fi
post() {
  exec 3<>"/dev/tcp/127.0.0.1/$port"
  printf 'POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer %s\r\nContent-Type: application/json\r\nContent-Length: %s\r\n\r\n%s' "$token" "${#1}" "$1" >&3
}
answered() { read -r status <&3; printf '%s\n' "${status%$'\r'}" >> "$dir/mcp_status"; exec 3<&-; }
connect() {
  post '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}'; answered
  post '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'; answered
}
dispatch() { post "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"read\",\"arguments\":{\"path\":\"$1\"},\"_meta\":{\"claudecode/toolUseId\":\"toolu_1\"}}}"; }
got=
while IFS= read -r msg; do
  id=$(printf '%s' "$msg" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  case "$msg" in
    *'"subtype":"initialize"'*) reply "$id" '{"current_permission_mode":"default","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro"},"models":[{"value":"default","resolvedModel":"claude-opus-5-5"},{"value":"opus","resolvedModel":"claude-opus-5-5"},{"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001"}]}' ;;
    *'"subtype":"get_settings"'*) reply "$id" "{\"effective\":$flags,\"sources\":[{\"source\":\"flagSettings\",\"settings\":$flags}]}" ;;
    *'"subtype":"set_model"'*) model=$(printf '%s' "$msg" | sed -n 's/.*"model":"\([^"]*\)".*/\1/p'); reply "$id" '{}' ;;
    *'"subtype":"get_context_usage"'*)
      if [ -e "$dir/no_windows" ]; then line "{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"$id\",\"error\":\"no\"}}"; continue; fi
      window=200000; [ "$model" = claude-opus-5-5 ] && window=1000000
      reply "$id" "{\"maxTokens\":$window,\"model\":\"$model\"}" ;;
    *'"subtype":"get_hooks_listing"'*) reply "$id" "{\"hooks\":$hooks,\"policy\":{\"allDisabled\":true}}"; [ "$scenario" = stops_reading ] && [ -n "$run" ] && exec sleep 30 ;;
    *'"type":"user"'*) printf '%s\n' "$msg" > "$dir/stdin_prompt"; got=1; break ;;
  esac
done
if [ -z "$got" ]; then [ "$scenario" = probe_runs ] && init none; exit 0; fi
case $scenario in
  batch) init none; connect; generation tool_use; dispatch a; sleep 30 ;;
  cut_line) init none; connect; generation tool_use; printf '{"type":"keep_al'; dispatch a; sleep 30 ;;
  missing_block_eof | missing_block_late)
    init none
    event "{\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-haiku-4-5-20251001\"$start_usage}}"
    event '{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"missing"}}'
    event "{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}$final_usage}"
    event '{"type":"message_stop"}'
    [ "$scenario" = missing_block_late ] && sleep 30 ;;
  status_only_error) init none; line '{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":529,"errors":["overloaded"]}' ;;
  text) text_reply ;;
  noisy_stderr) printf 'bad \377 byte\n' >&2; head -c 200000 /dev/zero | tr '\0' 'x' >&2; printf '\n' >&2; text_reply ;;
  text_then_fail) text_reply; exit 23 ;;
  text_then_linger) text_reply; exec >&-; sleep 30 ;;
  text_then_slow_stdout) text_reply; sleep 30 ;;
  text_then_api_key) text_reply; init ANTHROPIC_API_KEY ;;
  text_then_keep_alive) text_reply; line '{"type":"keep_alive"}' ;;
  text_then_limits) text_reply; limits ;;
  stalled_handoff) init none; connect; generation tool_use; while :; do line '{"type":"keep_alive"}'; sleep 0.2; done ;;
  progress_generation) init none; event "{\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-haiku-4-5-20251001\"$start_usage}}"; while :; do line '{"type":"tool_progress","tool_use_id":"toolu_1","tool_name":"mcp__maki__read","elapsed_time_seconds":1}'; sleep 0.2; done ;;
  stalled_generation) init none; event "{\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-haiku-4-5-20251001\"$start_usage}}"; while :; do line '{"type":"keep_alive"}'; sleep 0.2; done ;;
  ends_output) init none; exec >&-; sleep 30 ;;
  stalled_result | stalled_result_without_usage) init none; generation end_turn; while :; do line '{"type":"keep_alive"}'; sleep 0.2; done ;;
  overloaded) init none; line '{"type":"assistant","error":"unknown","is_api_error_message":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Overloaded"}]}}'; line '{"type":"result","subtype":"success","is_error":true,"api_error_status":529,"result":"Overloaded"}'; sleep 30 ;;
  cli_retry) init none; event "{\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-haiku-4-5-20251001\"$start_usage}}"; line '{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"retry_delay_ms":500,"error_status":529,"error":"overloaded"}'; sleep 30 ;;
  truncated) init none; generation max_tokens; line '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Output token limit hit. Resume directly."}]},"isSynthetic":true}'; sleep 30 ;;
  truncated_line) init none; printf '%s' '{"type":"assistant","message":' ;;
  text_then_garbage) text_reply; line 'Update available: run claude update' ;;
  no_final_usage) init none; connect; generation tool_use; dispatch a; sleep 30 ;;
  api_key) init ANTHROPIC_API_KEY; sleep 30 ;;
  other_arguments) init none; connect; generation tool_use; dispatch changed; sleep 30 ;;
  refused_then_quiet) init none; line '{"type":"assistant","error":"invalid_request","is_api_error_message":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Prompt is too long"}]}}'; sleep 30 ;;
  hang)
    init none
    sleep 240 </dev/null >/dev/null 2>&1 &
    echo $! > "$dir/member-pid"
    touch "$dir/hanging"
    exec sleep 240
    ;;
esac
