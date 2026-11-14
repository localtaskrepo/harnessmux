//! Amp has no ACP server: each turn runs `amp --stream-json -x <prompt>` (later turns `amp threads continue <thread>`),
//! and its Claude-shaped JSON lines become events. Set up from Amp's docs; not run live.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use super::{Command, Report, Reporter, Start, refuse};
use crate::error::{Error, Result};
use crate::event::{EventBody, StopReason, Usage};
use crate::profile::Permissions;

pub async fn run(start: Start, mut commands: mpsc::UnboundedReceiver<Command>, reporter: Reporter) -> Result<()> {
    let profile = &start.record.profile;
    if profile.permissions == Permissions::ReadOnly {
        return Err(Error::Invalid("amp has no read-only mode; use workspace or full permissions".into()));
    }
    if profile.model.is_some() || profile.effort.is_some() {
        reporter.notice(None, "amp picks its model through its own modes; model and effort are not applied");
    }
    let (program, base_args) = start.spec.launch_command().await?;
    reporter.report(Report::Ready);
    let mut thread = start.record.agent_session.clone();
    reporter.event(
        None,
        EventBody::SessionStarted {
            agent: start.spec.name.clone(),
            cwd: start.record.cwd.display().to_string(),
            agent_session: thread.clone(),
            how: if thread.is_some() { "resumed".into() } else { "new".into() },
            adapter: None,
            link: None,
        },
        None,
    );
    while let Some(command) = commands.recv().await {
        let (turn, text) = match command {
            Command::Close => return Ok(()),
            Command::Prompt { turn, text } => (turn, text),
            Command::Cancel => continue,
            other => {
                refuse(other, "amp takes no answers and has no options to set");
                continue;
            }
        };
        reporter.event(Some(turn), EventBody::TurnStarted { prompt: text.clone() }, None);
        let mut args = base_args.clone();
        if let Some(id) = &thread {
            args.extend(["threads".into(), "continue".into(), id.clone()]);
        }
        if profile.permissions == Permissions::Full {
            args.push("--dangerously-allow-all".into());
        }
        args.extend(["--stream-json".into(), "-x".into(), text]);
        let mut child = tokio::process::Command::new(&program)
            .args(&args)
            .envs(start.spec.env.iter().chain(&profile.env))
            .current_dir(&start.record.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::io(format!("starting {}", program.display()), error))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Agent("amp: no stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| Error::Agent("amp: no stderr".into()))?;
        let errors = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut text = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                text.push_str(&line);
                text.push('\n');
            }
            text
        });
        let mut lines = BufReader::new(stdout).lines();
        let mut ended = false;
        let mut cancelled = false;
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
                        if let Some(id) = value.get("session_id").and_then(Value::as_str).filter(|_| thread.is_none()) {
                            thread = Some(id.to_string());
                            reporter.report(Report::AgentSession { id: id.to_string(), adapter: None });
                        }
                        for body in map_line(&value) {
                            ended |= body.ends_turn();
                            reporter.event(Some(turn), body, Some(value.clone()));
                        }
                    }
                    _ => break,
                },
                command = commands.recv() => match command {
                    Some(Command::Cancel) => {
                        let _ = child.start_kill();
                        cancelled = true;
                    }
                    Some(Command::Prompt { turn, text }) => {
                        // The owner queues prompts and sends the next one only after this turn ends.
                        reporter.notice(Some(turn), format!("dropped a prompt sent during a running amp turn: {text}"));
                    }
                    Some(Command::Close) | None => {
                        let _ = child.start_kill();
                        return Ok(());
                    }
                    Some(other) => refuse(other, "amp takes no answers and has no options to set"),
                },
            }
        }
        let status = child.wait().await.map_err(|error| Error::io("waiting for amp", error))?;
        let stderr = errors.await.unwrap_or_default();
        if cancelled {
            reporter.event(Some(turn), EventBody::TurnFinished { stop_reason: StopReason::Cancelled, usage: Usage::default() }, None);
        } else if !ended {
            let error = format!("amp exited with {status} before finishing the turn: {}", stderr.trim());
            reporter.event(Some(turn), EventBody::TurnFailed { error }, None);
        }
    }
    Ok(())
}

/// Amp's stream follows Claude Code's stream-json: system, assistant, user and result lines.
fn map_line(value: &Value) -> Vec<EventBody> {
    let blocks = || value.get("message").and_then(|m| m.get("content")).and_then(Value::as_array).cloned().unwrap_or_default();
    let field = |block: &Value, key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    match value.get("type").and_then(Value::as_str) {
        Some("assistant") => blocks()
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => Some(EventBody::MessageDelta { text: field(block, "text") }),
                Some("tool_use") => Some(EventBody::ToolStarted { id: field(block, "id"), title: field(block, "name"), kind: None }),
                _ => None,
            })
            .collect(),
        Some("user") => blocks()
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|block| {
                let failed = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                EventBody::ToolFinished { id: field(block, "tool_use_id"), status: if failed { "failed" } else { "completed" }.into(), title: None }
            })
            .collect(),
        Some("result") => {
            let failed =
                value.get("is_error").and_then(Value::as_bool).unwrap_or(false) || value.get("subtype").and_then(Value::as_str).is_some_and(|s| s != "success");
            let reply = field(value, "result");
            let usage = value.get("usage");
            let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
            let mut events = Vec::new();
            if !reply.is_empty() {
                events.push(EventBody::Message { text: reply.clone() });
            }
            events.push(if failed {
                EventBody::TurnFailed { error: if reply.is_empty() { "amp reported an error".into() } else { reply } }
            } else {
                EventBody::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: count("input_tokens"),
                        output_tokens: count("output_tokens"),
                        cached_input_tokens: count("cache_read_input_tokens"),
                        ..Usage::default()
                    },
                }
            });
            events
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amp's documented stream (ampcode.com manual, stream-json), as lotar replayed it before harnessmux.
    const DOCS_STREAM: &str = include_str!("../../tests/recordings/amp-docs.jsonl");

    #[test]
    fn the_documented_stream_becomes_events() {
        let events: Vec<EventBody> = DOCS_STREAM.lines().flat_map(|line| map_line(&serde_json::from_str(line).unwrap())).collect();
        assert!(events.contains(&EventBody::ToolStarted { id: "toolu_example_1".into(), title: "Read".into(), kind: None }));
        assert!(events.contains(&EventBody::MessageDelta { text: "Fixed add() to return a + b.".into() }));
        let last = events.last().unwrap();
        assert!(matches!(last, EventBody::TurnFinished { stop_reason: StopReason::EndTurn, usage } if usage.input_tokens == Some(2700)), "{last:?}");
    }
}
