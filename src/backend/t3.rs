//! T3 Code threads through T3's CLI: the first turn is `t3 thread start <project> - --wait`, later turns
//! `t3 thread send <thread> - --wait`, with the prompt on stdin; `t3 thread show --json` is polled to follow the run.
//! The thread lives in T3 Code, so a session resumes by sending to it again. T3's CLI has no effort setting, no child
//! threads, no permission answers and no interrupt.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{Command, Report, Reporter, Start, refuse};
use crate::error::{Error, Result};
use crate::event::{EventBody, StopReason, Usage};

const POLL: Duration = Duration::from_secs(2);

pub async fn run(start: Start, mut commands: mpsc::UnboundedReceiver<Command>, reporter: Reporter) -> Result<()> {
    let profile = start.record.profile.clone();
    if profile.provider.is_some() != profile.model.is_some() {
        let missing = if profile.provider.is_some() { "model" } else { "provider" };
        return Err(Error::Invalid(format!("the t3 agent takes a provider and a model together (T3: --provider with --model); set the {missing} too")));
    }
    if let Some(effort) = profile.effort {
        reporter.notice(None, format!("T3's CLI has no effort setting; effort {effort} is not applied"));
    }
    let (program, base_args) = start.spec.launch_command().await?;
    reporter.report(Report::Ready);
    let mut thread = start.record.agent_session.clone();
    if let Some(id) = &thread {
        reporter.event(None, started(&start, id, "resumed"), None);
    }
    while let Some(command) = commands.recv().await {
        let (turn, text) = match command {
            Command::Close => return Ok(()),
            Command::Prompt { turn, text } => (turn, text),
            Command::Cancel => continue,
            other => {
                refuse(other, "answer T3 Code's questions and change its settings in T3 Code; its CLI can't");
                continue;
            }
        };
        reporter.event(Some(turn), EventBody::TurnStarted { prompt: text.clone() }, None);
        let mut args = base_args.clone();
        match &thread {
            Some(id) => args.extend(["thread".into(), "send".into(), id.clone(), "-".into(), "--wait".into()]),
            None => {
                let project = start.record.project.clone().unwrap_or_else(|| start.record.cwd.clone());
                args.extend(["thread".into(), "start".into(), project.display().to_string(), "-".into()]);
                if let Some(title) = &profile.title {
                    args.extend(["--title".into(), title.clone()]);
                }
                if let (Some(provider), Some(model)) = (&profile.provider, &profile.model) {
                    args.extend(["--provider".into(), provider.clone(), "--model".into(), model.clone()]);
                }
                if let Some(base) = &profile.worktree_base {
                    args.extend(["--worktree".into(), base.clone()]);
                    if let Some(branch) = &profile.branch {
                        args.extend(["--branch".into(), branch.clone()]);
                    }
                }
                args.push("--wait".into());
            }
        }
        let mut child = tokio::process::Command::new(&program)
            .args(&args)
            .envs(start.spec.env.iter().chain(&profile.env))
            .current_dir(&start.record.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::io(format!("starting {}", program.display()), error))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await.map_err(|error| Error::io("writing the prompt to t3", error))?;
        }
        let stdout = child.stdout.take().ok_or_else(|| Error::Agent("t3: no stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| Error::Agent("t3: no stderr".into()))?;
        let reply = tokio::spawn(async move {
            let mut text = String::new();
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                text.push_str(&line);
                text.push('\n');
            }
            text
        });
        let mut errors = BufReader::new(stderr).lines();
        let mut error_text = String::new();
        let mut follow = Follow::default();
        let mut poll = tokio::time::interval(POLL);
        let mut cancelled = false;
        let status = loop {
            tokio::select! {
                line = errors.next_line() => match line {
                    Ok(Some(line)) => {
                        if let Some(id) = thread_id(&line).filter(|_| thread.is_none()) {
                            thread = Some(id.clone());
                            reporter.report(Report::AgentSession { id: id.clone(), adapter: None });
                            reporter.event(None, started(&start, &id, "new"), None);
                        } else if !is_noise(&line) && !line.trim().is_empty() {
                            error_text.push_str(&line);
                            error_text.push('\n');
                        }
                    }
                    _ => break child.wait().await.map_err(|error| Error::io("waiting for t3", error))?,
                },
                _ = poll.tick() => {
                    if let Some(id) = &thread {
                        for message in follow.poll(&program, &start, id).await {
                            reporter.event(Some(turn), EventBody::MessageDelta { text: message }, None);
                        }
                    }
                }
                command = commands.recv() => match command {
                    Some(Command::Cancel) => {
                        let link = follow.link.clone().or_else(|| thread.clone()).unwrap_or_default();
                        reporter.notice(Some(turn), format!("T3's CLI can't interrupt a thread; it keeps running in T3 Code: {link}"));
                        let _ = child.start_kill();
                        cancelled = true;
                    }
                    Some(Command::Close) | None => {
                        let _ = child.start_kill();
                        return Ok(());
                    }
                    Some(Command::Prompt { turn, text }) => reporter.notice(Some(turn), format!("dropped a prompt sent during a running t3 turn: {text}")),
                    Some(other) => refuse(other, "answer T3 Code's questions and change its settings in T3 Code; its CLI can't"),
                },
            }
        };
        let reply = reply.await.unwrap_or_default().trim().to_string();
        let link = follow.link.clone().or_else(|| thread.clone()).unwrap_or_default();
        if cancelled {
            reporter.event(Some(turn), EventBody::TurnFinished { stop_reason: StopReason::Cancelled, usage: Usage::default() }, None);
            continue;
        }
        if !reply.is_empty() {
            reporter.event(Some(turn), EventBody::Message { text: reply.clone() }, None);
        }
        let body = match status.code() {
            Some(0) => EventBody::TurnFinished { stop_reason: StopReason::EndTurn, usage: Usage::default() },
            Some(2) => EventBody::TurnFailed { error: format!("the T3 Code thread waits for an answer; answer it in T3 Code: {link}") },
            _ => EventBody::TurnFailed { error: format!("t3 exited with {status}: {}", error_text.trim()) },
        };
        reporter.event(Some(turn), body, None);
    }
    Ok(())
}

fn started(start: &Start, id: &str, how: &str) -> EventBody {
    EventBody::SessionStarted {
        agent: start.spec.name.clone(),
        cwd: start.record.cwd.display().to_string(),
        agent_session: Some(id.to_string()),
        how: how.into(),
        adapter: None,
    }
}

/// New assistant messages of the thread's current run, from `t3 thread show --json`.
#[derive(Default)]
struct Follow {
    run: Option<String>,
    seen: usize,
    link: Option<String>,
}

impl Follow {
    async fn poll(&mut self, program: &std::path::Path, start: &Start, thread: &str) -> Vec<String> {
        let output = tokio::process::Command::new(program)
            .args(["thread", "show", thread, "--json", "--messages", "20"])
            .envs(start.spec.env.iter().chain(&start.record.profile.env))
            .current_dir(&start.record.cwd)
            .stdin(Stdio::null())
            .output()
            .await;
        let Ok(output) = output else { return Vec::new() };
        let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else { return Vec::new() };
        self.take_new(&value)
    }

    fn take_new(&mut self, value: &Value) -> Vec<String> {
        let thread = value.get("thread");
        if let Some(link) = thread.and_then(|t| t.get("link")).and_then(Value::as_str) {
            self.link = Some(link.to_string());
        }
        let Some(run) = thread.and_then(|t| t.get("activeRunId").or_else(|| t.get("latestRunId"))).and_then(Value::as_str) else { return Vec::new() };
        if self.run.as_deref() != Some(run) {
            self.run = Some(run.to_string());
            self.seen = 0;
        }
        let replies: Vec<String> = value
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant") && m.get("runId").and_then(Value::as_str) == Some(run))
            .filter_map(|m| m.get("text").and_then(Value::as_str).map(str::to_string))
            .collect();
        let new = replies.iter().skip(self.seen).map(|text| format!("{text}\n\n")).collect();
        self.seen = self.seen.max(replies.len());
        new
    }
}

/// The thread id from `t3 thread start`'s `Thread <id>` line on stderr.
fn thread_id(line: &str) -> Option<String> {
    let id = line.trim().strip_prefix("Thread ")?.trim();
    (!id.is_empty() && !id.contains(char::is_whitespace)).then(|| id.to_string())
}

/// Electron's note that the t3 launcher runs Node with app defaults, printed on every run.
fn is_noise(line: &str) -> bool {
    line.contains(":ERROR:electron/") && line.contains("Node.js environment variables are disabled")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stderr_names_the_thread_and_electron_noise_is_dropped() {
        assert_eq!(thread_id("Thread abc-123\n").as_deref(), Some("abc-123"));
        assert_eq!(thread_id("Threads are busy"), None);
        assert!(is_noise("[1111/090522.026791:ERROR:electron/shell/app/node_main.cc:154] Node.js environment variables are disabled because x"));
    }

    #[test]
    fn following_a_thread_reports_each_new_reply_of_the_run_once() {
        let mut follow = Follow::default();
        let show = |messages: Value| json!({"thread": {"activeRunId": "r1", "link": "https://t3/x"}, "messages": messages});
        let first = show(json!([{"role": "user", "runId": "r1", "text": "do it"}, {"role": "assistant", "runId": "r1", "text": "On it"}]));
        assert_eq!(follow.take_new(&first), ["On it\n\n"]);
        assert!(follow.take_new(&first).is_empty());
        let second = show(
            json!([{"role": "assistant", "runId": "r0", "text": "old"}, {"role": "assistant", "runId": "r1", "text": "On it"}, {"role": "assistant", "runId": "r1", "text": "Done"}]),
        );
        assert_eq!(follow.take_new(&second), ["Done\n\n"]);
        assert_eq!(follow.link.as_deref(), Some("https://t3/x"));
    }
}
