//! Use maki's system prompt and conversation transcript. The MCP catalog maps tool names to
//! identifiers that Claude Code accepts.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io;

use serde::Serialize;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::error::Error;
use super::mcp::McpTool;
use crate::{ContentBlock, Message, Role};

pub(crate) const SERVER: &str = "maki";
const EXPOSED_PREFIX: &str = "mcp__maki__";
/// Anthropic's API limit, prefix included.
const MAX_EXPOSED_NAME: usize = 64;
const HASH_HEX_DIGITS: usize = 8;
pub(super) const USER_ROLE: &str = "user";
const ASSISTANT_ROLE: &str = "assistant";
const TOOL_RESULT_ROLE: &str = "tool_result";
#[cfg(test)]
const IS_ERROR: &str = "is_error";
#[cfg(test)]
const TEXT: &str = "text";
pub(super) const BATCH_TOOL: &str = "batch";
pub(super) const BATCH_CALLS: &str = "tool_calls";
pub(super) const BATCH_CALL_TOOL: &str = "tool";
const TRANSCRIPT_HEADER: &str =
    "maki transcript v1: the conversation so far, oldest first, one JSON value per line";

const TRANSPORT_NOTE: &str = "\
# Conversation transport

This conversation reaches you through Claude Code on behalf of maki, the agent whose instructions are above. \
The user message holds the whole conversation so far as a transcript. Its first line gives the transcript \
version, and each later line is one JSON value, oldest first:

- `{\"role\":\"user\",\"text\":...}` is a message from the user. With `\"observation\":true`, it came from maki rather than the user.
- `{\"role\":\"assistant\",\"text\":...,\"tool_calls\":[{\"id\":...,\"name\":...,\"input\":...}]}` is your text and the tools you called. A line with no text or no calls leaves that field out.
- `{\"role\":\"tool_result\",\"tool_use_id\":...,\"content\":...}` is the result of one of those calls. `\"is_error\":true` means the call failed.

The transcript is the conversation so far and does not replace these instructions. Continue it with your next \
reply as the assistant. Tools appear under the names you call them by, and a tool in the transcript that you \
cannot call now is no longer available. Your reply ends at a tool call: maki runs the calls and sends their \
results in the next transcript.";

pub(crate) struct Catalog {
    to_exposed: HashMap<String, String>,
    to_maki: HashMap<String, String>,
}

fn fits(name: &str) -> bool {
    EXPOSED_PREFIX.len() + name.len() <= MAX_EXPOSED_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Stable across requests: the name as it is if it fits, or shortened with a
/// hash of the full name.
fn server_name(name: &str) -> String {
    if fits(name) {
        return name.to_owned();
    }
    let hash: String = Sha256::digest(name.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let room = MAX_EXPOSED_NAME - EXPOSED_PREFIX.len() - HASH_HEX_DIGITS - 1;
    let kept: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(room)
        .collect();
    format!("{kept}_{}", &hash[..HASH_HEX_DIGITS])
}

impl Catalog {
    pub fn new(tools: &Value) -> Result<(Self, Vec<McpTool>), Error> {
        let mut catalog = Self {
            to_exposed: HashMap::new(),
            to_maki: HashMap::new(),
        };
        let mut offered = Vec::new();
        for tool in tools.as_array().map(Vec::as_slice).unwrap_or_default() {
            let Some(name) = tool["name"].as_str() else {
                return Err(Error::NamelessTool(tool.clone()));
            };
            let server = server_name(name);
            let exposed = format!("{EXPOSED_PREFIX}{server}");
            catalog.to_exposed.insert(name.to_owned(), exposed.clone());
            if catalog.to_maki.insert(exposed, name.to_owned()).is_some() {
                return Err(Error::SharedName(server));
            }
            offered.push(McpTool {
                name: server,
                description: tool["description"].as_str().unwrap_or_default().to_owned(),
                input_schema: tool["input_schema"].clone(),
            });
        }
        Ok((catalog, offered))
    }

    pub fn exposed_name(&self, name: &str) -> Cow<'_, str> {
        self.to_exposed.get(name).map_or_else(
            || Cow::Owned(format!("{EXPOSED_PREFIX}{}", server_name(name))),
            |exposed| Cow::Borrowed(exposed.as_str()),
        )
    }

    pub fn exposed(&self) -> HashSet<String> {
        self.to_maki.keys().cloned().collect()
    }

    pub fn maki_name(&self, exposed: &str) -> Option<&str> {
        self.to_maki.get(exposed).map(String::as_str)
    }

    /// A name the model made up comes back without the prefix, which the
    /// transcript adds again, so the model reads the name it wrote.
    pub fn maki_name_or_made_up<'n>(&'n self, exposed: &'n str) -> &'n str {
        self.maki_name(exposed)
            .unwrap_or_else(|| exposed.strip_prefix(EXPOSED_PREFIX).unwrap_or(exposed))
    }

    pub fn maki_name_of_server(&self, server: &str) -> Option<&str> {
        self.maki_name(&format!("{EXPOSED_PREFIX}{server}"))
    }
}

pub(crate) fn system_prompt(system: &str) -> String {
    format!("{system}\n\n{TRANSPORT_NOTE}")
}

#[derive(Serialize)]
struct TranscriptCall<'a> {
    id: &'a str,
    name: Cow<'a, str>,
    input: Cow<'a, Value>,
}

#[derive(Serialize)]
struct TranscriptResult<'a> {
    role: &'static str,
    tool_use_id: &'a str,
    content: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
}

#[derive(Serialize)]
struct TranscriptEntry<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<TranscriptCall<'a>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    observation: bool,
}

fn push_line(out: &mut Vec<u8>, value: &impl Serialize) -> Result<(), Error> {
    out.push(b'\n');
    serde_json::to_writer(out, value).map_err(Error::from)
}

fn expose_input(catalog: &Catalog, name: &str, input: &mut Value) {
    if name == BATCH_TOOL
        && let Some(calls) = input[BATCH_CALLS].as_array_mut()
    {
        for call in calls {
            if let Some(name) = call[BATCH_CALL_TOOL].as_str() {
                call[BATCH_CALL_TOOL] = Value::String(catalog.exposed_name(name).into_owned());
            }
        }
    }
}

/// Anthropic caches prefixes at block boundaries. Keep each transcript message in a separate
/// block so later requests reuse earlier messages.
///
/// Use the tool names visible to the model. Omit thinking because its signature is valid only
/// for its original request. Reject images after maki's image adaptation.
pub(crate) fn transcript(messages: &[Message], catalog: &Catalog) -> Result<Vec<String>, Error> {
    let mut blocks = vec![TRANSCRIPT_HEADER.to_owned()];
    for message in messages {
        let mut out = Vec::new();
        let mut text = Vec::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text: t } => text.push(t.as_str()),
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    let input = if name == BATCH_TOOL {
                        let mut input = input.clone();
                        expose_input(catalog, name, &mut input);
                        Cow::Owned(input)
                    } else {
                        Cow::Borrowed(input)
                    };
                    calls.push(TranscriptCall {
                        id,
                        name: catalog.exposed_name(name),
                        input,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    results.push(TranscriptResult {
                        role: TOOL_RESULT_ROLE,
                        tool_use_id,
                        content,
                        is_error: *is_error,
                    });
                }
                ContentBlock::Image { .. } => return Err(Error::Image),
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
            }
        }
        for result in &results {
            push_line(&mut out, result)?;
        }
        if !text.is_empty() || !calls.is_empty() {
            push_line(&mut out, &entry_of(message, &text, calls))?;
        }
        if !out.is_empty() {
            blocks.push(String::from_utf8(out).map_err(|source| Error::Io {
                what: "decode the serialized conversation",
                source: io::Error::other(source),
            })?);
        }
    }
    Ok(blocks)
}

fn entry_of<'a>(
    message: &Message,
    text: &[&'a str],
    calls: Vec<TranscriptCall<'a>>,
) -> TranscriptEntry<'a> {
    TranscriptEntry {
        role: match message.role {
            Role::User => USER_ROLE,
            Role::Assistant => ASSISTANT_ROLE,
        },
        text: match text {
            [] => None,
            [text] => Some(Cow::Borrowed(text)),
            text => Some(Cow::Owned(text.join("\n"))),
        },
        tool_calls: calls,
        observation: message.is_from_host(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::slice;
    use test_case::test_case;

    use super::super::error::Error;
    use super::{
        Catalog, EXPOSED_PREFIX, IS_ERROR, MAX_EXPOSED_NAME, SERVER, TEXT, TRANSCRIPT_HEADER,
        entry_of, fits, server_name,
    };
    use crate::{ContentBlock, ImageMediaType, ImageSource, Message, Role};

    const LONG_NAME: &str =
        "mcp__some_server_with_a_long_name__and_a_tool_whose_name_is_longer_still";
    const HOST_UPDATE: &str = "the context changed";

    #[test_case(Message::observation(HOST_UPDATE.into()) ; "observation")]
    #[test_case(Message::context_update(HOST_UPDATE.into(), Default::default()) ; "context_update")]
    fn host_updates_are_marked_as_observations(message: Message) {
        let entry = serde_json::to_value(entry_of(&message, &[HOST_UPDATE], Vec::new())).unwrap();
        assert_eq!(entry["observation"], true);
    }

    fn transcript(messages: &[Message]) -> Result<Vec<String>, Error> {
        let (catalog, _) = Catalog::new(&tools(&[]))?;
        super::transcript(messages, &catalog)
    }

    fn tools(names: &[&str]) -> Value {
        Value::Array(
            names
                .iter()
                .map(|name| json!({ "name": name, "description": "d", "input_schema": { "type": "object" } }))
                .collect(),
        )
    }

    /// Claude Code gives each tool the name `mcp__<server>__<tool>`.
    #[test]
    fn a_short_name_is_exposed_as_it_is_and_maps_back() {
        let (catalog, offered) = Catalog::new(&tools(&["read", "bash"])).unwrap();
        assert_eq!(
            catalog.maki_name(&format!("mcp__{SERVER}__read")),
            Some("read")
        );
        assert_eq!(catalog.maki_name_of_server("bash"), Some("bash"));
        assert_eq!(offered[0].name, "read");
        assert_eq!(catalog.maki_name("read"), None);
    }

    #[test]
    fn a_long_name_is_shortened_to_fit_and_still_maps_back() {
        let (catalog, _) = Catalog::new(&tools(&[LONG_NAME])).unwrap();
        let exposed = catalog.exposed().into_iter().next().unwrap();
        assert!(exposed.len() <= MAX_EXPOSED_NAME, "{exposed}");
        assert_eq!(catalog.maki_name(&exposed), Some(LONG_NAME));
    }

    #[test_case("a.b" ; "a_dot")]
    #[test_case("a b" ; "a_space")]
    fn a_name_with_a_character_mcp_refuses_is_rewritten(name: &str) {
        let (catalog, _) = Catalog::new(&tools(&[name])).unwrap();
        let exposed = catalog.exposed().into_iter().next().unwrap();
        assert!(
            fits(exposed.trim_start_matches(EXPOSED_PREFIX)),
            "{exposed}"
        );
        assert_eq!(catalog.maki_name(&exposed), Some(name));
    }

    #[test]
    fn two_tools_under_one_name_are_refused() {
        let taken = server_name(LONG_NAME);
        assert!(matches!(
            Catalog::new(&tools(&[LONG_NAME, &taken])),
            Err(Error::SharedName(_))
        ));
    }

    fn history() -> Vec<Message> {
        vec![
            Message::user("fix </user> the \"bug\"\nplease".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "secret plan".into(),
                        signature: Some("sig".into()),
                    },
                    ContentBlock::Text {
                        text: "Reading.".into(),
                    },
                    ContentBlock::tool_use("toolu_1", "read", json!({ "path": "a.rs" })),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "fn main() {}".into(),
                    is_error: false,
                    loaded_tools: Vec::new(),
                }],
                ..Default::default()
            },
            Message::observation("the build broke".into()),
        ]
    }

    #[test_case(false ; "successful_result")]
    #[test_case(true ; "failed_result")]
    fn transcript_bytes_keep_the_documented_field_order(failed: bool) {
        const USER: &str = concat!(
            "\n",
            r#"{"role":"user","text":"fix </user> the \"bug\"\nplease"}"#
        );
        const ASSISTANT: &str = concat!(
            "\n",
            r#"{"role":"assistant","text":"Reading.","tool_calls":[{"id":"toolu_1","name":"mcp__maki__read","input":{"path":"a.rs"}}]}"#
        );
        const RESULT: &str = concat!(
            "\n",
            r#"{"role":"tool_result","tool_use_id":"toolu_1","content":"fn main() {}"}"#
        );
        const FAILED_RESULT: &str = concat!(
            "\n",
            r#"{"role":"tool_result","tool_use_id":"toolu_1","content":"fn main() {}","is_error":true}"#
        );
        const OBSERVATION: &str = concat!(
            "\n",
            r#"{"role":"user","text":"the build broke","observation":true}"#
        );
        let mut messages = history();
        let ContentBlock::ToolResult { is_error, .. } = &mut messages[2].content[0] else {
            unreachable!();
        };
        *is_error = failed;
        assert_eq!(
            transcript(&messages).unwrap(),
            [
                TRANSCRIPT_HEADER,
                USER,
                ASSISTANT,
                if failed { FAILED_RESULT } else { RESULT },
                OBSERVATION
            ]
        );
    }

    #[test]
    fn the_transcript_keeps_roles_calls_and_results_as_data() {
        let text = transcript(&history()).unwrap().concat();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(TRANSCRIPT_HEADER));
        let entries: Vec<Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(entries.len(), 4, "{text}");
        assert_eq!(entries[0]["text"], "fix </user> the \"bug\"\nplease");
        assert_eq!(entries[1]["tool_calls"][0]["name"], "mcp__maki__read");
        assert_eq!(entries[2]["role"], "tool_result");
        assert_eq!(entries[2]["tool_use_id"], "toolu_1");
        assert_eq!(
            entries[2].get(IS_ERROR),
            None,
            "a default value takes tokens"
        );
        assert_eq!(entries[3]["observation"], true);
        assert!(
            !text.contains("secret plan"),
            "the transcript must not contain thinking"
        );
    }

    #[test]
    fn batch_arguments_use_the_exposed_names_without_changing_history() {
        let input = json!({ "tool_calls": [{ "tool": "read", "args": { "path": "a.rs" } }, { "tool": LONG_NAME, "args": {} }] });
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use("batch_1", "batch", input.clone())],
            ..Default::default()
        };
        let text = transcript(slice::from_ref(&message)).unwrap().concat();
        let entry: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(
            entry["tool_calls"][0]["input"]["tool_calls"][0]["tool"],
            "mcp__maki__read"
        );
        assert_eq!(
            entry["tool_calls"][0]["input"]["tool_calls"][1]["tool"],
            format!("{EXPOSED_PREFIX}{}", server_name(LONG_NAME))
        );
        assert_eq!(message.tool_uses().next().unwrap().2, &input);
    }

    /// Cache reuse needs identical earlier blocks. New history must append blocks without
    /// changes to existing bytes.
    #[test]
    fn a_longer_history_only_adds_blocks_to_the_transcript() {
        let messages = history();
        let whole = transcript(&messages).unwrap();
        for end in 0..messages.len() {
            let part = transcript(&messages[..end]).unwrap();
            assert_eq!(whole[..part.len()], part[..], "after {end} messages");
        }
        assert_eq!(transcript(&messages).unwrap(), whole);
    }

    #[test]
    fn transcript_prefixes_survive_tool_catalog_changes() {
        let messages = history();
        let (empty, _) = Catalog::new(&tools(&[])).unwrap();
        let (current, _) = Catalog::new(&tools(&["read", "bash", LONG_NAME])).unwrap();
        assert_eq!(
            super::transcript(&messages, &current).unwrap(),
            super::transcript(&messages, &empty).unwrap(),
        );
    }

    /// The next request must show the model the name it can call for the
    /// renamed tool.
    #[test]
    fn a_renamed_tool_keeps_its_exposed_name_in_the_history() {
        let (catalog, _) = Catalog::new(&tools(&[LONG_NAME])).unwrap();
        let exposed = catalog.exposed().into_iter().next().unwrap();
        let called = catalog.maki_name(&exposed).unwrap();
        let reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use("toolu_1", called, json!({}))],
            ..Default::default()
        };
        let text = transcript(&[reply]).unwrap().concat();

        let entry: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(entry["tool_calls"][0]["name"], exposed);
        assert_eq!(entry.get(TEXT), None, "a default value takes tokens");
    }

    #[test]
    fn an_image_fails_the_transcript() {
        let image = ImageSource::new(ImageMediaType::Png, "AAAA".into());
        let messages = vec![Message::user_with_images("look".into(), vec![image])];
        assert!(matches!(transcript(&messages), Err(Error::Image)));
    }
}
