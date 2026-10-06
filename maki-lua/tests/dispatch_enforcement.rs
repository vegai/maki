use std::sync::Arc;

use maki_agent::agent::tool_dispatch;
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolAudience, ToolContext, ToolFilter, ToolRegistry};
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use serde_json::{Value, json};
use test_case::test_case;

const HIDDEN_TOOL: &str = "hidden_write";
const CALLER: &str = "nested_caller";
const INSPECT: &str = "inspect_calls";
const BATCH: &str = "batch";
const CODE_EXECUTION: &str = "code_execution";
const UNKNOWN_TOOL: &str = "unknown tool: hidden_write";
const NAME_ERROR: &str = "NameError";
const CALL_ID: &str = "dispatch_probe";
const ZERO_INVOCATIONS: &str = "0";
const LIST_TOOLS: &str = "list_tools";
const BATCH_SOURCE: &str = include_str!("../../plugins/batch/init.lua");
const CODE_SOURCE: &str = include_str!("../../plugins/code_execution/init.lua");
const FIXTURE: &str = r#"
local calls = 0
maki.api.register_tool({
  name = "hidden_write",
  description = "records an invocation",
  audiences = { "main", "general_sub", "interpreter" },
  schema = { type = "object", properties = {} },
  handler = function() calls = calls + 1 return "ran" end,
})
maki.api.register_tool({
  name = "inspect_calls",
  description = "reads the invocation count",
  schema = { type = "object", properties = {} },
  handler = function() return tostring(calls) end,
})
maki.api.register_tool({
  name = "nested_caller",
  description = "calls the fixture through Lua",
  schema = { type = "object", properties = {} },
  handler = function(_, ctx)
    local out, err = maki.agent.call_tool(ctx, "hidden_write", {})
    return err and { llm_output = err, is_error = true } or out
  end,
})
"#;
const LIST_FIXTURE: &str = r#"
maki.api.register_tool({
  name = "list_tools",
  description = "lists the tools offered to a subagent",
  schema = { type = "object", properties = {} },
  handler = function(_, ctx)
    local tools, err = maki.agent.tools(ctx, { audience = "main" })
    if err then return { llm_output = err, is_error = true } end
    return (maki.json.encode(tools))
  end,
})
"#;

enum Origin {
    Model,
    Lua,
    Batch,
    Bridge,
    CodeExecution,
}

fn execute(ctx: &ToolContext, name: &str, input: Value) -> Result<ToolOutput, String> {
    let invocation = ctx.registry.get(name).unwrap().tool.parse(&input).unwrap();
    smol::block_on(invocation.execute(ctx)).output
}

#[test_case(Origin::Model, true ; "filtered_model_call")]
#[test_case(Origin::Lua, true ; "filtered_lua_call")]
#[test_case(Origin::Batch, true ; "filtered_batch_child")]
#[test_case(Origin::Bridge, true ; "filtered_bridge_call")]
#[test_case(Origin::CodeExecution, true ; "filtered_python_call")]
#[test_case(Origin::Model, false ; "research_model_call")]
#[test_case(Origin::Lua, false ; "research_lua_call")]
#[test_case(Origin::Batch, false ; "research_batch_child")]
#[test_case(Origin::Bridge, false ; "research_bridge_call")]
#[test_case(Origin::CodeExecution, false ; "research_python_call")]
fn an_unoffered_tool_never_runs(origin: Origin, filtered: bool) {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("dispatch_fixture", FIXTURE).unwrap();
    host.load_source(BATCH, BATCH_SOURCE).unwrap();
    host.load_source(CODE_EXECUTION, CODE_SOURCE).unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = registry;
    if filtered {
        ctx.tool_filter = Arc::new(ToolFilter::AllExcept(vec![HIDDEN_TOOL.to_owned()]));
    } else {
        ctx.audience = ToolAudience::RESEARCH_SUB;
    }
    let text = match origin {
        Origin::Model => {
            let done = smol::block_on(tool_dispatch::run(
                CALL_ID.to_owned(),
                HIDDEN_TOOL,
                &json!({}),
                &ctx,
                CallOrigin::Model,
            ));
            assert!(done.is_error);
            done.output.as_text()
        }
        Origin::Lua => execute(&ctx, CALLER, json!({})).unwrap_err(),
        Origin::Batch => execute(
            &ctx,
            BATCH,
            json!({ "tool_calls": [{ "tool": HIDDEN_TOOL, "parameters": {} }] }),
        )
        .unwrap()
        .as_text(),
        Origin::Bridge => {
            smol::block_on(interpreter_bridge::dispatch(&ctx, HIDDEN_TOOL, &json!({}))).unwrap_err()
        }
        Origin::CodeExecution => {
            execute(&ctx, CODE_EXECUTION, json!({ "code": "hidden_write()" })).unwrap_err()
        }
    };
    let expected = if matches!(origin, Origin::CodeExecution) {
        NAME_ERROR
    } else {
        UNKNOWN_TOOL
    };
    assert!(text.contains(expected), "{text}");
    assert_eq!(
        execute(&ctx, INSPECT, json!({})).unwrap().as_text(),
        ZERO_INVOCATIONS
    );
}

#[test]
fn lua_tool_definitions_respect_a_plugin_allow_list() {
    let registry = Arc::clone(ToolRegistry::global_arc());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("dispatch_fixture", FIXTURE).unwrap();
    host.load_source(LIST_TOOLS, LIST_FIXTURE).unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = registry;
    ctx.config.allowed_tools = vec![HIDDEN_TOOL.to_owned()];
    let output = execute(&ctx, LIST_TOOLS, json!({})).unwrap().as_text();
    let definitions: Value = serde_json::from_str(&output).unwrap();
    let names: Vec<&str> = definitions
        .as_array()
        .unwrap()
        .iter()
        .map(|def| def["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [HIDDEN_TOOL]);
}
