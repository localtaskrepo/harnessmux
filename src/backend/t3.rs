//! T3 Code threads through T3's CLI: the first turn is `t3 thread start <project> - --wait`, later turns
//! `t3 thread send <thread> - --wait`, with the prompt on stdin; `t3 thread show --json` is polled to follow the run.
//! The thread lives in T3 Code, so a session resumes by sending to it again; when the previous owner stopped during a
//! turn, the first turn after the resume follows the thread's run instead, as that turn may still run; when it died
//! before it saved the new thread's id, the resume finds the thread by the session's title. A thread that
//! waits for an answer ends the turn with `waiting_for_answer`. A `t3` that fails while the thread still runs, or
//! because the T3 Code server doesn't answer, doesn't end the turn: it follows the thread's run, waiting for the server
//! up to [`SERVER_WAIT`]. T3's CLI has no effort setting, no child threads, no permission answers and no interrupt.

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{Command, Report, Reporter, Start, refuse};
use crate::error::{Error, Result};
use crate::event::{EventBody, StopReason, Usage};

const POLL: Duration = Duration::from_secs(2);
/// Failed `t3 thread show` reads in a row before following a run gives up.
const READ_ATTEMPTS: u32 = 3;
/// How long following a run waits for a T3 Code server that doesn't answer (lotar DEV-467).
const SERVER_WAIT: Duration = Duration::from_secs(60 * 60);
/// What T3's CLI prints when its server doesn't answer.
const UNREACHABLE: &str = "Could not reach the T3 Code server";

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
    if thread.is_none()
        && start.interrupted
        && let Some(id) = started_thread(&program, &start).await
    {
        reporter.report(Report::AgentSession { id: id.clone(), adapter: None });
        reporter.notice(
            None,
            format!("The previous owner stopped before it saved the thread's id; T3 Code thread {id} has the session's title and was made in its turn"),
        );
        thread = Some(id);
    }
    let mut follow = Follow::default();
    if let Some(id) = &thread {
        if let Shown::Thread(shown) = show(&program, &start, id).await {
            follow.link = link(&shown);
        }
        reporter.event(None, started(&start, id, "resumed", follow.link.clone()), None);
    }
    let mut interrupted = start.interrupted && thread.is_some();
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
        if std::mem::take(&mut interrupted)
            && let Some(id) = &thread
        {
            reporter.notice(Some(turn), "The previous owner stopped during a turn; this turn follows the T3 Code thread's run and doesn't send the prompt");
            if !follow_run(&program, &start, id, turn, &mut follow, &mut commands, &reporter).await {
                return Ok(());
            }
            continue;
        }
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
        let mut poll = tokio::time::interval(POLL);
        let mut cancelled = false;
        let status = loop {
            tokio::select! {
                line = errors.next_line() => match line {
                    Ok(Some(line)) => {
                        if let Some(id) = thread_id(&line).filter(|_| thread.is_none()) {
                            thread = Some(id.clone());
                            reporter.report(Report::AgentSession { id: id.clone(), adapter: None });
                            let shown = show(&program, &start, &id).await.thread();
                            follow.link = shown.as_ref().and_then(link);
                            reporter.event(None, started(&start, &id, "new", follow.link.clone()), None);
                            for message in shown.map(|shown| follow.take_new(&shown)).unwrap_or_default() {
                                reporter.event(Some(turn), EventBody::MessageDelta { text: message }, None);
                            }
                        } else if !is_noise(&line) && !line.trim().is_empty() {
                            error_text.push_str(&line);
                            error_text.push('\n');
                        }
                    }
                    _ => break child.wait().await.map_err(|error| Error::io("waiting for t3", error))?,
                },
                _ = poll.tick() => {
                    if let Some(shown) = match &thread { Some(id) => show(&program, &start, id).await.thread(), None => None } {
                        for message in follow.take_new(&shown) {
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
        if !matches!(status.code(), Some(0 | 2))
            && let Some(id) = &thread
            && still_going(&program, &start, id, &error_text).await
        {
            reporter.notice(Some(turn), format!("t3 exited with {status} while the thread goes on; this turn follows its run: {link}"));
            if !follow_run(&program, &start, id, turn, &mut follow, &mut commands, &reporter).await {
                return Ok(());
            }
            continue;
        }
        if !reply.is_empty() {
            reporter.event(Some(turn), EventBody::Message { text: reply.clone() }, None);
        }
        let body = match status.code() {
            Some(0) => finished(StopReason::EndTurn),
            Some(2) => waiting(&reporter, turn, &link),
            _ => EventBody::TurnFailed { error: format!("t3 exited with {status}: {}", error_text.trim()) },
        };
        reporter.event(Some(turn), body, None);
    }
    Ok(())
}

/// Follow the thread's latest run to its end as `turn`, without sending a prompt. `false` when the session closes.
async fn follow_run(
    program: &std::path::Path,
    start: &Start,
    thread: &str,
    turn: u64,
    follow: &mut Follow,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    reporter: &Reporter,
) -> bool {
    let mut poll = tokio::time::interval(POLL);
    let mut failed_reads = 0;
    let mut unreachable_since: Option<Instant> = None;
    let shown = loop {
        tokio::select! {
            _ = poll.tick() => match show(program, start, thread).await {
                Shown::Thread(shown) => {
                    failed_reads = 0;
                    unreachable_since = None;
                    for message in follow.take_new(&shown) {
                        reporter.event(Some(turn), EventBody::MessageDelta { text: message }, None);
                    }
                    let state = RunState::of(&shown);
                    if !state.running || state.waiting {
                        break shown;
                    }
                }
                Shown::Unreachable => {
                    let since = *unreachable_since.get_or_insert_with(|| {
                        let link = follow.link.clone().unwrap_or_else(|| thread.to_string());
                        reporter.notice(Some(turn), format!("The T3 Code server doesn't answer; waiting up to {} minutes for it to follow the thread: {link}", SERVER_WAIT.as_secs() / 60));
                        Instant::now()
                    });
                    if since.elapsed() >= SERVER_WAIT {
                        reporter.event(Some(turn), EventBody::TurnFailed { error: format!("the T3 Code server didn't answer for {} minutes; the thread {thread} may still run", SERVER_WAIT.as_secs() / 60) }, None);
                        return true;
                    }
                }
                Shown::Unreadable => {
                    failed_reads += 1;
                    if failed_reads >= READ_ATTEMPTS {
                        reporter.event(Some(turn), EventBody::TurnFailed { error: format!("could not read the T3 Code thread {thread} with `t3 thread show`") }, None);
                        return true;
                    }
                }
            },
            command = commands.recv() => match command {
                Some(Command::Cancel) => {
                    let link = follow.link.clone().unwrap_or_else(|| thread.to_string());
                    reporter.notice(Some(turn), format!("T3's CLI can't interrupt a thread; it keeps running in T3 Code: {link}"));
                    reporter.event(Some(turn), finished(StopReason::Cancelled), None);
                    return true;
                }
                Some(Command::Close) | None => return false,
                Some(Command::Prompt { turn, text }) => reporter.notice(Some(turn), format!("dropped a prompt sent during a running t3 turn: {text}")),
                Some(other) => refuse(other, "answer T3 Code's questions and change its settings in T3 Code; its CLI can't"),
            },
        }
    };
    let state = RunState::of(&shown);
    let link = follow.link.clone().unwrap_or_else(|| thread.to_string());
    if let Some(reply) = state.reply {
        reporter.event(Some(turn), EventBody::Message { text: reply }, None);
    }
    let body = if state.waiting {
        waiting(reporter, turn, &link)
    } else if state.status == "completed" {
        finished(StopReason::EndTurn)
    } else {
        EventBody::TurnFailed { error: format!("the T3 Code thread's run ended with status '{}': {link}", state.status) }
    };
    reporter.event(Some(turn), body, None);
    true
}

fn finished(stop_reason: StopReason) -> EventBody {
    EventBody::TurnFinished { stop_reason, usage: Usage::default() }
}

fn waiting(reporter: &Reporter, turn: u64, link: &str) -> EventBody {
    reporter.notice(Some(turn), format!("The T3 Code thread waits for an answer; answer it in T3 Code: {link}"));
    finished(StopReason::WaitingForAnswer)
}

fn started(start: &Start, id: &str, how: &str, link: Option<String>) -> EventBody {
    EventBody::SessionStarted {
        agent: start.spec.name.clone(),
        cwd: start.record.cwd.display().to_string(),
        agent_session: Some(id.to_string()),
        how: how.into(),
        adapter: None,
        link,
    }
}

/// Whether a turn whose `t3` failed goes on in T3 Code: the server didn't answer, or the thread still runs.
async fn still_going(program: &std::path::Path, start: &Start, thread: &str, error_text: &str) -> bool {
    error_text.contains(UNREACHABLE)
        || match show(program, start, thread).await {
            Shown::Thread(shown) => RunState::of(&shown).running,
            Shown::Unreachable => true,
            Shown::Unreadable => false,
        }
}

/// A `t3 thread show --json` read.
enum Shown {
    Thread(Value),
    /// The T3 Code server didn't answer.
    Unreachable,
    Unreadable,
}

impl Shown {
    fn thread(self) -> Option<Value> {
        match self {
            Self::Thread(shown) => Some(shown),
            Self::Unreachable | Self::Unreadable => None,
        }
    }
}

async fn show(program: &std::path::Path, start: &Start, thread: &str) -> Shown {
    let Ok(output) = tokio::process::Command::new(program)
        .args(["thread", "show", thread, "--json", "--messages", "20"])
        .envs(start.spec.env.iter().chain(&start.record.profile.env))
        .current_dir(&start.record.cwd)
        .stdin(Stdio::null())
        .output()
        .await
    else {
        return Shown::Unreadable;
    };
    match serde_json::from_slice(&output.stdout) {
        Ok(shown) => Shown::Thread(shown),
        _ if String::from_utf8_lossy(&output.stderr).contains(UNREACHABLE) => Shown::Unreachable,
        _ => Shown::Unreadable,
    }
}

/// The thread `t3 thread start` made for the session's interrupted turn, from `t3 thread list --json`: the newest
/// one with the session's title created since that turn began (`last_used`). None without a title.
async fn started_thread(program: &std::path::Path, start: &Start) -> Option<String> {
    let title = start.record.profile.title.as_deref()?;
    let project = start.record.project.clone().unwrap_or_else(|| start.record.cwd.clone());
    let output = tokio::process::Command::new(program)
        .args(["thread", "list", "--project"])
        .arg(&project)
        .arg("--json")
        .envs(start.spec.env.iter().chain(&start.record.profile.env))
        .current_dir(&start.record.cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    let threads = serde_json::from_slice(&output.stdout).ok()?;
    thread_made_since(&threads, title, &start.record.last_used)
}

fn thread_made_since(threads: &Value, title: &str, since: &str) -> Option<String> {
    threads
        .as_array()?
        .iter()
        .filter(|t| t.get("title").and_then(Value::as_str) == Some(title))
        .filter_map(|t| Some((t.get("createdAt")?.as_str()?, t.get("threadId")?.as_str()?)))
        // Both are RFC 3339 UTC times with milliseconds, so they order as strings.
        .filter(|(created, _)| *created >= since)
        .max()
        .map(|(_, id)| id.to_string())
}

fn link(shown: &Value) -> Option<String> {
    shown.get("thread").and_then(|t| t.get("link")).and_then(Value::as_str).map(str::to_string)
}

/// How the thread's latest run stands, from `t3 thread show --json`.
struct RunState {
    running: bool,
    /// T3 Code waits for an answer to one of the thread's questions.
    waiting: bool,
    /// The thread's status (running, completed, …).
    status: String,
    /// The run's last reply.
    reply: Option<String>,
}

impl RunState {
    fn of(shown: &Value) -> Self {
        let thread = shown.get("thread");
        let status = thread.and_then(|t| t.get("status")).and_then(Value::as_str).unwrap_or_default().to_string();
        let active = thread.and_then(|t| t.get("activeRunId")).is_some_and(|id| !id.is_null());
        let run = thread.and_then(|t| t.get("activeRunId").filter(|id| !id.is_null()).or_else(|| t.get("latestRunId"))).and_then(Value::as_str);
        let reply = shown
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant") && (run.is_none() || m.get("runId").and_then(Value::as_str) == run))
            .and_then(|m| m.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        Self {
            running: status == "running" || active,
            waiting: thread.and_then(|t| t.get("pendingRequestCount")).and_then(Value::as_u64).is_some_and(|count| count > 0),
            status,
            reply,
        }
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
    fn take_new(&mut self, value: &Value) -> Vec<String> {
        let thread = value.get("thread");
        if let Some(link) = link(value) {
            self.link = Some(link);
        }
        let Some(run) = thread.and_then(|t| t.get("activeRunId").filter(|id| !id.is_null()).or_else(|| t.get("latestRunId"))).and_then(Value::as_str) else {
            return Vec::new();
        };
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

    #[test]
    fn the_interrupted_turns_thread_is_the_newest_with_the_title_made_since_it_began() {
        let threads = json!([
            {"threadId": "old", "title": "DEV-1 Fix", "createdAt": "2026-11-14T09:00:00.000Z"},
            {"threadId": "other", "title": "DEV-2 Other", "createdAt": "2026-11-14T10:00:02.000Z"},
            {"threadId": "t1", "title": "DEV-1 Fix", "createdAt": "2026-11-14T10:00:01.000Z"}
        ]);
        assert_eq!(thread_made_since(&threads, "DEV-1 Fix", "2026-11-14T10:00:00.000Z").as_deref(), Some("t1"));
        assert_eq!(thread_made_since(&threads, "DEV-1 Fix", "2026-11-14T11:00:00.000Z"), None);
    }

    #[test]
    fn a_run_waits_for_an_answer_or_ends_with_its_last_reply() {
        let waiting = RunState::of(&json!({"thread": {"status": "running", "activeRunId": "r1", "pendingRequestCount": 1}, "messages": []}));
        assert!(waiting.running && waiting.waiting);
        let done = RunState::of(&json!({"thread": {"status": "completed", "activeRunId": null, "latestRunId": "r2", "pendingRequestCount": 0}, "messages": [
            {"role": "assistant", "runId": "r2", "text": "Done"}, {"role": "assistant", "runId": "r1", "text": "old"}
        ]}));
        assert!(!done.running && !done.waiting);
        assert_eq!((done.status.as_str(), done.reply.as_deref()), ("completed", Some("Done")));
    }
}
