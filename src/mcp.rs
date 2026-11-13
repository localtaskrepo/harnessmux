//! `harnessmux mcp`: an MCP server on stdio with tools to start, message, wait for and stop child sessions, so an agent
//! in one session can run others.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agents::AgentTable;
use crate::client;
use crate::error::{Error, Result};
use crate::event::EventBody;
use crate::follow::Follower;
use crate::ipc::Request;
use crate::profile::Profile;
use crate::registry::Registry;
use crate::session::{Session, StartOptions};

fn tools() -> Value {
    let name = json!({"type": "string", "description": "Session name"});
    json!([
        {"name": "start", "description": "Start a session of an agent (claude, codex, copilot, …) and send it a first prompt. Returns the session name and turn.",
         "inputSchema": {"type": "object", "required": ["agent", "prompt"], "properties": {
            "agent": {"type": "string"}, "prompt": {"type": "string"}, "name": name,
            "cwd": {"type": "string", "description": "Working directory; defaults to the server's"},
            "model": {"type": "string"}, "effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh", "max"]},
            "permissions": {"type": "string", "enum": ["read-only", "workspace", "full"]}}}},
        {"name": "send", "description": "Send a prompt to a session; it runs after the current turn. Returns the turn.",
         "inputSchema": {"type": "object", "required": ["name", "prompt"], "properties": {"name": name, "prompt": {"type": "string"}}}},
        {"name": "wait", "description": "Wait until a turn ends (default: the latest) and return the agent's reply and how the turn ended.",
         "inputSchema": {"type": "object", "required": ["name"], "properties": {"name": name, "turn": {"type": "integer"},
            "timeout_seconds": {"type": "integer", "description": "Default 600"}}}},
        {"name": "cancel", "description": "Cancel the running turn and drop queued prompts.", "inputSchema": {"type": "object", "required": ["name"], "properties": {"name": name}}},
        {"name": "status", "description": "A session's state, running turn and options.", "inputSchema": {"type": "object", "required": ["name"], "properties": {"name": name}}},
        {"name": "list", "description": "All sessions.", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "close", "description": "End a session for good.", "inputSchema": {"type": "object", "required": ["name"], "properties": {"name": name}}}
    ])
}

fn text_result(value: &Value) -> Value {
    json!({"content": [{"type": "text", "text": value.to_string()}], "structuredContent": value})
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| Error::Invalid(format!("missing argument {key}")))
}

async fn call(registry: &Registry, table: &AgentTable, tool: &str, args: &Value) -> Result<Value> {
    match tool {
        "start" => {
            let mut profile = Profile { model: args.get("model").and_then(Value::as_str).map(str::to_string), ..Profile::default() };
            if let Some(effort) = args.get("effort").and_then(Value::as_str) {
                profile.effort = Some(effort.parse()?);
            }
            if let Some(permissions) = args.get("permissions").and_then(Value::as_str) {
                profile.permissions = permissions.parse()?;
            }
            let cwd = match args.get("cwd").and_then(Value::as_str) {
                Some(cwd) => PathBuf::from(cwd),
                None => std::env::current_dir().map_err(|error| Error::io("current directory", error))?,
            };
            let options = StartOptions {
                name: args.get("name").and_then(Value::as_str).map(str::to_string),
                agent: arg(args, "agent")?.to_string(),
                cwd,
                profile,
                idle_ttl: Some(client::DEFAULT_IDLE_TTL),
            };
            let record = Session::prepare(registry, table, &options)?;
            client::spawn_owner(registry, &record.name, true, client::DEFAULT_IDLE_TTL).await?;
            let turn = client::send(registry, &record.name, arg(args, "prompt")?).await?;
            Ok(json!({"name": record.name, "turn": turn}))
        }
        "send" => {
            let name = arg(args, "name")?;
            let turn = client::send(registry, name, arg(args, "prompt")?).await?;
            Ok(json!({"name": name, "turn": turn}))
        }
        "wait" => {
            let name = arg(args, "name")?;
            let record = registry.load(name)?;
            let turn = args.get("turn").and_then(Value::as_u64).unwrap_or(record.turns);
            if turn == 0 {
                return Err(Error::Invalid(format!("session {name} has no turns yet")));
            }
            let timeout = Duration::from_secs(args.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(600));
            let mut follower = Follower::from_start(&registry.events_path(name));
            let mut reply = String::new();
            let end = client::wait_turn(&mut follower, turn, Some(timeout), |event| {
                if let EventBody::Message { text } = &event.body {
                    reply.push_str(text);
                }
            })
            .await?;
            let outcome = match &end.body {
                EventBody::TurnFinished { stop_reason, .. } => json!({"finished": stop_reason}),
                EventBody::TurnFailed { error } => json!({"failed": error}),
                EventBody::SessionEnded { reason } => json!({"session_ended": reason}),
                _ => Value::Null,
            };
            Ok(json!({"name": name, "turn": turn, "reply": reply, "outcome": outcome}))
        }
        "cancel" => {
            client::request(registry, arg(args, "name")?, &Request::Cancel).await?;
            Ok(json!({"cancelled": true}))
        }
        "close" => {
            client::request(registry, arg(args, "name")?, &Request::Close).await?;
            Ok(json!({"closed": true}))
        }
        "status" => {
            let name = arg(args, "name")?;
            match client::request(registry, name, &Request::Status).await {
                Ok(reply) => Ok(serde_json::to_value(reply.status).unwrap_or(Value::Null)),
                Err(_) => Ok(serde_json::to_value(registry.load(name)?).unwrap_or(Value::Null)),
            }
        }
        "list" => Ok(serde_json::to_value(registry.list(None)?).unwrap_or(Value::Null)),
        other => Err(Error::Invalid(format!("unknown tool {other}"))),
    }
}

/// Serves MCP on stdin and stdout until stdin closes.
pub async fn serve(registry: Registry, table: AgentTable) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.map_err(|error| Error::io("reading stdin", error))? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = message.get("id").cloned() else { continue }; // notifications need no answer
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match message.get("method").and_then(Value::as_str).unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "harnessmux", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let tool = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match call(&registry, &table, tool, &args).await {
                    Ok(value) => text_result(&value),
                    Err(error) => json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true}),
                })
            }
            other => Err(json!({"code": -32601, "message": format!("method not found: {other}")})),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        let mut text = reply.to_string();
        text.push('\n');
        out.write_all(text.as_bytes()).await.map_err(|error| Error::io("writing stdout", error))?;
        out.flush().await.map_err(|error| Error::io("writing stdout", error))?;
    }
    Ok(())
}
