//! ACP messages (as JSON) to harnessmux events, and the agent's config options in a simpler shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{EventBody, OUTPUT_LIMIT, PlanEntry, Usage};

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The events for one `session/update` notification's `update` object.
pub fn map_update(update: &Value) -> Vec<EventBody> {
    let kind = str_at(update, "sessionUpdate").unwrap_or_default();
    let text =
        || update.get("content").filter(|content| str_at(content, "type") == Some("text")).and_then(|content| str_at(content, "text")).map(str::to_string);
    let id = || str_at(update, "toolCallId").unwrap_or_default().to_string();
    match kind {
        "agent_message_chunk" => text().map(|text| EventBody::MessageDelta { text }).into_iter().collect(),
        "agent_thought_chunk" => text().map(|text| EventBody::ThoughtDelta { text }).into_iter().collect(),
        "tool_call" => vec![EventBody::ToolStarted {
            id: id(),
            title: str_at(update, "title").unwrap_or("tool").to_string(),
            kind: str_at(update, "kind").map(str::to_string),
            input: tool_input(update),
            locations: tool_locations(update),
        }],
        "tool_call_update" => {
            let (input, locations) = (tool_input(update), tool_locations(update));
            let title = str_at(update, "title").map(str::to_string);
            match str_at(update, "status") {
                Some(status @ ("completed" | "failed")) => {
                    // An adapter that sends the input only with the result: keep it before the result.
                    let updated = (input.is_some() || !locations.is_empty()).then(|| EventBody::ToolUpdated {
                        id: id(),
                        title: None,
                        kind: str_at(update, "kind").map(str::to_string),
                        input,
                        locations,
                    });
                    let finished =
                        EventBody::ToolFinished { id: id(), status: status.to_string(), title, output: tool_output(update).map(|text| cap_output(&text, 0)) };
                    updated.into_iter().chain([finished]).collect()
                }
                _ => {
                    let kind = str_at(update, "kind").map(str::to_string);
                    if input.is_none() && locations.is_empty() && title.is_none() && kind.is_none() {
                        Vec::new()
                    } else {
                        vec![EventBody::ToolUpdated { id: id(), title, kind, input, locations }]
                    }
                }
            }
        }
        "plan" => {
            let entries = update.get("entries").and_then(Value::as_array).into_iter().flatten();
            let entries = entries
                .filter_map(|entry| {
                    Some(PlanEntry {
                        content: str_at(entry, "content")?.to_string(),
                        status: str_at(entry, "status").unwrap_or_default().to_string(),
                        priority: str_at(entry, "priority").unwrap_or_default().to_string(),
                    })
                })
                .collect();
            vec![EventBody::Plan { entries }]
        }
        "usage_update" => {
            let cost = update.get("cost").filter(|cost| str_at(cost, "currency").is_none_or(|currency| currency == "USD"));
            let usage = Usage {
                context_used: update.get("used").and_then(Value::as_u64),
                context_size: update.get("size").and_then(Value::as_u64),
                cost_usd: cost.and_then(|cost| cost.get("amount")).and_then(Value::as_f64),
                rate_limit: update.get("_meta").and_then(|meta| meta.get("_claude/rateLimit")).and_then(crate::failure::rate_limit),
                ..Usage::default()
            };
            if usage.is_empty() { Vec::new() } else { vec![EventBody::Usage(usage)] }
        }
        "current_mode_update" => {
            str_at(update, "currentModeId").map(|value| EventBody::ConfigChanged { option: "mode".into(), value: value.into() }).into_iter().collect()
        }
        // History replayed by session/load, command lists, titles and the rest carry nothing a caller acts on;
        // they stay in the raw ACP log.
        _ => Vec::new(),
    }
}

/// A tool call's `rawInput`; none when it is empty (claude-agent-acp's first `tool_call` has `{}`).
fn tool_input(update: &Value) -> Option<Value> {
    update.get("rawInput").filter(|input| !input.is_null() && input.as_object().is_none_or(|map| !map.is_empty())).cloned()
}

/// The paths of a tool call's `locations`.
fn tool_locations(update: &Value) -> Vec<String> {
    update.get("locations").and_then(Value::as_array).into_iter().flatten().filter_map(|location| str_at(location, "path")).map(str::to_string).collect()
}

/// A finished tool's output: its text `content` items, else `rawOutput` (a string, or other JSON as compact JSON).
fn tool_output(update: &Value) -> Option<String> {
    let texts: Vec<&str> = update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| str_at(item, "type") == Some("content"))
        .filter_map(|item| item.get("content").filter(|block| str_at(block, "type") == Some("text")).and_then(|block| str_at(block, "text")))
        .collect();
    let output = if texts.is_empty() {
        match update.get("rawOutput") {
            None | Some(Value::Null) => return None,
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        }
    } else {
        texts.join("\n")
    };
    (!output.trim().is_empty()).then_some(output)
}

/// A chunk of a running command's output (codex-acp's `_meta.terminal_output_delta`).
pub fn terminal_output(update: &Value) -> Option<&str> {
    update.get("_meta").and_then(|meta| meta.get("terminal_output_delta")).and_then(|delta| str_at(delta, "data"))
}

/// `text` cut to [`OUTPUT_LIMIT`] bytes on a character boundary; a cut, or `dropped` bytes left out before, end it
/// with `… [N more bytes]`.
pub fn cap_output(text: &str, dropped: usize) -> String {
    let mut end = text.len().min(OUTPUT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let more = text.len() - end + dropped;
    if more == 0 { text.to_string() } else { format!("{}… [{more} more bytes]", &text[..end]) }
}

/// Token counts from a `session/prompt` result's `usage`.
pub fn prompt_usage(result: &Value) -> Usage {
    let Some(usage) = result.get("usage") else { return Usage::default() };
    let count = |key: &str| usage.get(key).and_then(Value::as_u64);
    Usage {
        input_tokens: count("inputTokens"),
        output_tokens: count("outputTokens"),
        cached_input_tokens: count("cachedReadTokens"),
        total_tokens: count("totalTokens"),
        ..Usage::default()
    }
}

/// Texts adapters send as an assistant message when the model call failed. A reply that is only one of these ends the
/// turn as `turn.failed`, not as a success (codex-acp 1.x sent `model_not_found` this way).
const FAILURE_TEXTS: &[&str] = &[
    "model_not_found",
    "invalid_api_key",
    "authentication required",
    "not logged in",
    "please run /login",
    "unauthorized",
    "unexpected status 4",
    "unexpected status 5",
    "usage limit",
    "rate limit reached",
    "hit your limit",
    "-hour limit",
    "weekly limit",
    "insufficient_quota",
];

/// Codex's own error messages (codex-rs `CodexErr`, codex 0.169.0) that codex-acp sends as a message chunk of their
/// own before `end_turn` instead of failing the prompt (acpx #850): server overloaded, high demand, policy-restricted or
/// flex capacity gone, retries used up (429 and 5xx), stream cut off. A reply whose last paragraph or last chunk
/// starts with one fails the turn, also when tools ran first.
const LAST_PARAGRAPH_FAILURES: &[&str] = &[
    "selected model is at capacity",
    "we're currently experiencing high demand",
    "policy-restricted capacity is unavailable",
    "flex capacity unavailable",
    "exceeded retry limit, last status:",
    "stream disconnected before completion",
];

/// The error a reply reports: a Codex error as the last paragraph of the reply or of its last chunk (the chunk
/// catches an error glued onto an earlier message without a blank line), else a short error text in a turn that
/// ran no tools.
pub fn failure_in_reply(reply: &str, last_chunk: &str, ran_tools: bool) -> Option<String> {
    let trimmed = reply.trim();
    for text in [trimmed, last_chunk] {
        let last = text.trim().rsplit("\n\n").next().unwrap_or_default().trim();
        let last_lower = last.to_lowercase().replace('\u{2019}', "'");
        if LAST_PARAGRAPH_FAILURES.iter().any(|failure| last_lower.starts_with(failure)) {
            return Some(last.to_string());
        }
    }
    if ran_tools || trimmed.is_empty() || trimmed.len() > 1_500 {
        return None;
    }
    let lower = trimmed.to_lowercase();
    // Copilot 1.0: `Error: Execution failed: Model "gpt-5.4" is not available.`
    let error_line = lower.starts_with("error:") || lower.starts_with("execution failed");
    (error_line || FAILURE_TEXTS.iter().any(|text| lower.contains(text))).then(|| trimmed.to_string())
}

/// An agent's config option (ACP `configOptions`, or the older `modes`), reduced to what a caller needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// mode, model, thought_level, … (ACP category), when the agent gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub current: String,
    /// Allowed values; empty for a boolean option (true, false).
    pub values: Vec<String>,
    /// True for the older session-modes API (set with `session/set_mode`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_mode: bool,
}

impl ConfigOption {
    pub fn is_boolean(&self) -> bool {
        self.values.is_empty() && !self.legacy_mode
    }

    pub fn allows(&self, value: &str) -> bool {
        if self.is_boolean() { matches!(value, "true" | "false") } else { self.values.iter().any(|allowed| allowed == value) }
    }
}

/// `configOptions` (and `modes`) from a `session/new`, `session/resume` or `session/load` result.
pub fn config_options(result: &Value) -> Vec<ConfigOption> {
    let mut options: Vec<ConfigOption> =
        result.get("configOptions").and_then(Value::as_array).map(|list| list.iter().filter_map(config_option).collect()).unwrap_or_default();
    if !options.iter().any(|option| option.id == "mode")
        && let Some(modes) = result.get("modes")
    {
        let values: Vec<String> =
            modes.get("availableModes").and_then(Value::as_array).into_iter().flatten().filter_map(|mode| str_at(mode, "id")).map(str::to_string).collect();
        if !values.is_empty() {
            options.push(ConfigOption {
                id: "mode".into(),
                name: "Mode".into(),
                category: Some("mode".into()),
                current: str_at(modes, "currentModeId").unwrap_or_default().to_string(),
                values,
                legacy_mode: true,
            });
        }
    }
    options
}

fn config_option(value: &Value) -> Option<ConfigOption> {
    let id = str_at(value, "id")?.to_string();
    let name = str_at(value, "name").unwrap_or(&id).to_string();
    let category = str_at(value, "category").map(str::to_string);
    match str_at(value, "type") {
        Some("boolean") => {
            let current = value.get("currentValue").and_then(Value::as_bool).unwrap_or_default().to_string();
            Some(ConfigOption { id, name, category, current, values: Vec::new(), legacy_mode: false })
        }
        _ => {
            let mut values = Vec::new();
            for entry in value.get("options").and_then(Value::as_array).into_iter().flatten() {
                match entry.get("options").and_then(Value::as_array) {
                    // A group of options.
                    Some(group) => values.extend(group.iter().filter_map(|option| str_at(option, "value")).map(str::to_string)),
                    None => values.extend(str_at(entry, "value").map(str::to_string)),
                }
            }
            let current = str_at(value, "currentValue").unwrap_or_default().to_string();
            Some(ConfigOption { id, name, category, current, values, legacy_mode: false })
        }
    }
}

/// The option with this id, else the first one in this ACP category.
pub fn find_option<'a>(options: &'a [ConfigOption], id: Option<&str>, category: &str) -> Option<&'a ConfigOption> {
    id.and_then(|id| options.iter().find(|option| option.id == id)).or_else(|| options.iter().find(|option| option.category.as_deref() == Some(category)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn updates_become_events() {
        let chunk = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hi"}});
        assert_eq!(map_update(&chunk), [EventBody::MessageDelta { text: "Hi".into() }]);
        let tool = json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read a.rs", "kind": "read", "status": "pending", "rawInput": {}});
        let started = EventBody::ToolStarted { id: "t1".into(), title: "Read a.rs".into(), kind: Some("read".into()), input: None, locations: Vec::new() };
        assert_eq!(map_update(&tool), [started]);
        let running = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"});
        assert!(map_update(&running).is_empty());
        let paths = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "locations": [{"path": "a.rs"}, {"line": 3}], "rawInput": null});
        let updated = EventBody::ToolUpdated { id: "t1".into(), title: None, kind: None, input: None, locations: vec!["a.rs".into()] };
        assert_eq!(map_update(&paths), [updated]);
        let done = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"});
        assert_eq!(map_update(&done), [EventBody::ToolFinished { id: "t1".into(), status: "failed".into(), title: None, output: None }]);
        // The input only with the result: kept as an update before it. JSON output that isn't text: compact JSON.
        let late = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed", "rawInput": {"q": 1}, "rawOutput": {"n": 2}});
        let events: [EventBody; 2] = map_update(&late).try_into().unwrap();
        let [EventBody::ToolUpdated { input, .. }, EventBody::ToolFinished { output, .. }] = events else { panic!("{events:?}") };
        assert_eq!((input, output.as_deref()), (Some(json!({"q": 1})), Some(r#"{"n":2}"#)));
        let cleared = json!({"sessionUpdate": "plan", "entries": []});
        assert_eq!(map_update(&cleared), [EventBody::Plan { entries: Vec::new() }]);
        let usage = json!({"sessionUpdate": "usage_update", "used": 10, "size": 200, "cost": {"amount": 0.5, "currency": "USD"}});
        assert_eq!(map_update(&usage), [EventBody::Usage(Usage { context_used: Some(10), context_size: Some(200), cost_usd: Some(0.5), ..Usage::default() })]);
        assert!(map_update(&json!({"sessionUpdate": "available_commands_update"})).is_empty());
    }

    #[test]
    fn long_output_is_cut_on_a_character_boundary_and_says_how_much() {
        let long = "é".repeat(OUTPUT_LIMIT);
        let cut = cap_output(&long, 0);
        assert!(cut.ends_with(&format!("… [{} more bytes]", OUTPUT_LIMIT)), "{}", &cut[cut.len() - 40..]);
        assert_eq!(cut.len(), OUTPUT_LIMIT + "… [16384 more bytes]".len());
        assert_eq!(cap_output("ok", 0), "ok");
        assert_eq!(cap_output("ok", 5), "ok… [5 more bytes]");
    }

    #[test]
    fn error_replies_fail_the_turn_but_normal_replies_do_not() {
        assert!(failure_in_reply("Error: model_not_found: The model `gpt-6.1-sol` does not exist", "", false).is_some());
        assert!(failure_in_reply("Not logged in · Please run /login", "", false).is_some());
        assert!(failure_in_reply("You've hit your 5-hour limit · resets 6pm (America/Los_Angeles)", "", false).is_some());
        assert!(failure_in_reply("Error: Execution failed: Model \"gpt-5.4\" is not available.", "", false).is_some());
        assert!(failure_in_reply("model_not_found", "", true).is_none());
        assert!(failure_in_reply("Fixed the bug in add().", "", false).is_none());
        let overloaded = "Selected model is at capacity. Please try a different model.";
        assert_eq!(failure_in_reply(&format!("Looking at the tests.\n\n{overloaded}\n\n"), "", true).as_deref(), Some(overloaded));
        assert!(failure_in_reply("We\u{2019}re currently experiencing high demand, which may cause temporary errors.", "", true).is_some());
        assert!(failure_in_reply("exceeded retry limit, last status: 429 Too Many Requests", "", false).is_some());
        assert!(failure_in_reply("The model said the server is at capacity, so I retried.\n\nDone.", "", true).is_none());
        let policy = "Policy-restricted capacity is unavailable. Please try again later.";
        assert_eq!(failure_in_reply(&format!("Running the tests.\n\n{policy}\n\n"), "", true).as_deref(), Some(policy));
        assert!(failure_in_reply("Flex capacity unavailable.", "", true).is_some());
        let glued = "Looking at the failing test.Flex capacity unavailable.\n\n";
        assert_eq!(failure_in_reply(glued, "Flex capacity unavailable.\n\n", true).as_deref(), Some("Flex capacity unavailable."));
        assert!(failure_in_reply("Flex capacity unavailable. Switched to the standard tier.\n\nDone.", "Done.", true).is_none());
    }

    #[test]
    fn config_options_take_groups_booleans_and_legacy_modes() {
        let result = json!({
            "configOptions": [
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "a",
                 "options": [{"group": "g", "name": "G", "options": [{"value": "a", "name": "A"}, {"value": "b", "name": "B"}]}]},
                {"id": "fast", "name": "Fast", "type": "boolean", "currentValue": false}
            ],
            "modes": {"currentModeId": "default", "availableModes": [{"id": "default", "name": "D"}, {"id": "plan", "name": "P"}]}
        });
        let options = config_options(&result);
        assert_eq!(options[0].values, ["a", "b"]);
        assert!(options[1].is_boolean() && options[1].allows("true"));
        assert!(options[2].legacy_mode && options[2].allows("plan"));
        assert_eq!(find_option(&options, Some("nope"), "model").map(|o| o.id.as_str()), Some("model"));
    }
}
