//! T3 Code threads: the first turn is `t3 thread start <project> - --wait` with the prompt on stdin; in a session whose
//! directory is a git worktree of its project, it is T3's MCP tool `t3_thread_launch` with that worktree
//! (`existing_worktree`, full access), as T3's CLI can only make a new worktree. Later turns are `t3_thread_send`. Both
//! carry a `clientRequestId` from the session (and the turn), so a retry never makes a second thread or run; the MCP
//! sign-in is in [`super::t3_mcp`]. `t3 thread show --json` is polled to follow the run, and T3's thread activity
//! (`t3_thread_read`, view `activity`) gives the run's replies, thoughts and tool calls in order ([`Activity`]); a
//! turn whose activity can't be read streams the replies from `t3 thread show`. A run that ends neither
//! completed nor waiting fails the turn with the run's `error` items from the thread's activity (`t3_thread_read`), as
//! `t3 thread show` has no error.
//! The thread lives in T3 Code, so a session resumes by sending to it again; when the previous owner stopped during a
//! turn, the first turn after the resume follows the thread's run instead, as that turn may still run. The record notes
//! each `t3 thread start` until the new thread's id is saved; a session resumed without the id finds the thread by its
//! title, and when that lookup fails it starts no thread and fails its turns until a later resume finds it. A thread that
//! waits for an answer ends the turn with `waiting_for_answer`. A `t3` that fails while the thread still runs, or
//! because the T3 Code server doesn't answer, doesn't end the turn: it follows the thread's run, waiting for the server
//! up to [`SERVER_WAIT`]. The profile's effort and `options` go to T3 as model options (`--option id=value`), which T3
//! takes only with a provider and model. T3's CLI has no child threads, no permission answers and no interrupt.
//! [`settle`] moves a finished thread to T3's Settled group.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::t3_mcp::{Cli, Mcp};
use super::{Command, Report, Reporter, Start, refuse};
use crate::agents::AgentSpec;
use crate::error::{Error, Result};
use crate::event;
use crate::event::{EventBody, Failure, FailureKind, PlanEntry, StopReason, Usage};

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
    // A thread the session's turns started already runs where it was made.
    let worktree = if start.record.agent_session.is_none() { worktree_of(&start.record.cwd, start.record.project.as_deref()) } else { None };
    if let (Some(worktree), Some(base)) = (&worktree, &profile.worktree_base) {
        return Err(Error::Invalid(format!(
            "--worktree {base} starts the thread in a new worktree, but the session's directory {} is already a git worktree of {}; drop --worktree, or start the session in the main checkout",
            worktree.display(),
            start.record.project.as_deref().unwrap_or(Path::new("?")).display()
        )));
    }
    if profile.provider.is_none() && (profile.effort.is_some() || !profile.options.is_empty()) {
        reporter.notice(None, "T3 takes effort and model options only with a provider and model; they are not applied");
    }
    let (program, base_args) = start.spec.launch_command().await?;
    let cli = cli_of(&start, &program, &base_args);
    let mut mcp = None;
    reporter.report(Report::Ready);
    let mut thread = start.record.agent_session.clone();
    let mut follow = Follow::default();
    // Its turn may still run, or ended without reporting; the next turn follows its run instead of sending.
    let mut interrupted = start.interrupted && thread.is_some();
    // When the thread was asked for and `t3 thread list` can't tell whether T3 made it: no thread is started, as that
    // could be a second one, and each prompt looks again.
    let mut unresolved = None;
    // Records from before `agent_session_asked` only know that the owner died during a turn.
    let asked = start.record.agent_session_asked.clone().or_else(|| start.interrupted.then(|| start.record.last_used.clone()));
    if thread.is_none()
        && let Some(since) = asked
    {
        match find_thread(&program, &start, &since, &reporter).await {
            Ok(found) => {
                interrupted = found.is_some();
                thread = found;
            }
            Err(why) => unresolved = Some((since, why)),
        }
    }
    if let Some(id) = &thread {
        if let Shown::Thread(shown) = show(&program, &start, id).await {
            follow.link = link(&shown);
        }
        reporter.event(None, started(&start, id, "resumed", follow.link.clone()), None);
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
        follow.activity = Activity::default();
        if let Some((since, _)) = unresolved.take() {
            match find_thread(&program, &start, &since, &reporter).await {
                Ok(Some(id)) => {
                    if let Shown::Thread(shown) = show(&program, &start, &id).await {
                        follow.link = link(&shown);
                    }
                    reporter.event(None, started(&start, &id, "resumed", follow.link.clone()), None);
                    thread = Some(id);
                    interrupted = true;
                }
                Ok(None) => {}
                Err(why) => {
                    let error = format!(
                        "the previous owner stopped during a turn before it saved the T3 Code thread's id, and `t3 thread list` can't tell whether that turn made a thread ({why}); no thread is started, as it could be a second one. The next prompt looks again"
                    );
                    reporter.event(Some(turn), EventBody::failed(error), None);
                    unresolved = Some((since, why));
                    continue;
                }
            }
        }
        if std::mem::take(&mut interrupted)
            && let Some(id) = &thread
        {
            reporter.notice(Some(turn), "The previous owner stopped during a turn; this turn follows the T3 Code thread's run and doesn't send the prompt");
            if !follow_run(&mut mcp, &cli, &program, &start, id, turn, None, &mut follow, &mut commands, &reporter).await {
                return Ok(());
            }
            continue;
        }
        if let Some(id) = thread.clone() {
            let arguments = json!({"threadId": id, "message": text, "mode": "auto", "clientRequestId": request_id(&start, &turn.to_string())});
            match call(&mut mcp, &cli, "t3_thread_send", arguments).await {
                Ok(sent) => {
                    let run = sent.get("runId").and_then(Value::as_str);
                    if !follow_run(&mut mcp, &cli, &program, &start, &id, turn, run, &mut follow, &mut commands, &reporter).await {
                        return Ok(());
                    }
                }
                Err(error) => reporter.event(Some(turn), EventBody::failed(error.to_string()), None),
            }
            continue;
        }
        // Saved before T3 makes the thread, so an owner that dies before it saves the id leaves a trace.
        note_asked(&reporter, Some(event::now())).await;
        if let Some(worktree) = &worktree {
            match launch(&mut mcp, &cli, &start, worktree, &text).await {
                Ok(launched) => {
                    let id = launched.thread;
                    thread = Some(id.clone());
                    reporter.report(Report::AgentSession { id: id.clone(), adapter: None });
                    follow.link = launched.link;
                    reporter.event(None, started(&start, &id, "new", follow.link.clone()), None);
                    if !follow_run(&mut mcp, &cli, &program, &start, &id, turn, launched.run.as_deref(), &mut follow, &mut commands, &reporter).await {
                        return Ok(());
                    }
                }
                Err(error) => reporter.event(Some(turn), EventBody::failed(error.to_string()), None),
            }
            continue;
        }
        let mut args = base_args.clone();
        let project = start.record.project.clone().unwrap_or_else(|| start.record.cwd.clone());
        args.extend(["thread".into(), "start".into(), project.display().to_string(), "-".into()]);
        if let Some(title) = &profile.title {
            args.extend(["--title".into(), title.clone()]);
        }
        if let (Some(provider), Some(model)) = (&profile.provider, &profile.model) {
            args.extend(["--provider".into(), provider.clone(), "--model".into(), model.clone()]);
            for (id, value) in model_options(&profile) {
                args.extend(["--option".into(), format!("{id}={value}")]);
            }
        }
        if let Some(base) = &profile.worktree_base {
            args.extend(["--worktree".into(), base.clone()]);
            if let Some(branch) = &profile.branch {
                args.extend(["--branch".into(), branch.clone()]);
            }
        }
        args.push("--wait".into());
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
                            if let Some(shown) = shown {
                                stream(&mut mcp, &cli, &id, &shown, true, &mut follow, turn, &reporter).await;
                            }
                        } else if !is_noise(&line) && !line.trim().is_empty() {
                            error_text.push_str(&line);
                            error_text.push('\n');
                        }
                    }
                    _ => break child.wait().await.map_err(|error| Error::io("waiting for t3", error))?,
                },
                _ = poll.tick() => {
                    if let Some(id) = &thread && let Shown::Thread(shown) = show(&program, &start, id).await {
                        stream(&mut mcp, &cli, id, &shown, true, &mut follow, turn, &reporter).await;
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
        // What the run did since the last poll.
        if !cancelled
            && let Some(id) = &thread
            && let Shown::Thread(shown) = show(&program, &start, id).await
        {
            stream(&mut mcp, &cli, id, &shown, true, &mut follow, turn, &reporter).await;
        }
        if cancelled {
            reporter.event(Some(turn), EventBody::TurnFinished { stop_reason: StopReason::Cancelled, usage: Usage::default() }, None);
            continue;
        }
        if !matches!(status.code(), Some(0 | 2))
            && let Some(id) = &thread
            && still_going(&program, &start, id, &error_text).await
        {
            reporter.notice(Some(turn), format!("t3 exited with {status} while the thread goes on; this turn follows its run: {link}"));
            if !follow_run(&mut mcp, &cli, &program, &start, id, turn, None, &mut follow, &mut commands, &reporter).await {
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
            _ => EventBody::failed(format!("t3 exited with {status}: {}", error_text.trim())),
        };
        reporter.event(Some(turn), body, None);
    }
    Ok(())
}

/// Follow the thread's latest run (or `run`, once T3 shows it) to its end as `turn`, without sending a prompt. `false`
/// when the session closes.
#[expect(clippy::too_many_arguments, reason = "the run loop's state, passed down")]
async fn follow_run(
    mcp: &mut Option<Mcp>,
    cli: &Cli,
    program: &std::path::Path,
    start: &Start,
    thread: &str,
    turn: u64,
    run: Option<&str>,
    follow: &mut Follow,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    reporter: &Reporter,
) -> bool {
    let mut poll = tokio::time::interval(POLL);
    let mut failed_reads = 0;
    let mut unreachable_since: Option<Instant> = None;
    // Once T3 shows `run`, a later run is its resumption (after a background task) and is followed too.
    let mut current = run.is_none();
    let shown = loop {
        tokio::select! {
            _ = poll.tick() => match show(program, start, thread).await {
                Shown::Thread(shown) => {
                    failed_reads = 0;
                    unreachable_since = None;
                    let state = RunState::of(&shown);
                    current = current || state.run.as_deref() == run;
                    stream(mcp, cli, thread, &shown, current, follow, turn, reporter).await;
                    if current && (!state.running || state.waiting) {
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
                        reporter.event(Some(turn), Failure::new(format!("the T3 Code server didn't answer for {} minutes; the thread {thread} may still run", SERVER_WAIT.as_secs() / 60), FailureKind::Transport).into(), None);
                        return true;
                    }
                }
                Shown::Unreadable => {
                    failed_reads += 1;
                    if failed_reads >= READ_ATTEMPTS {
                        reporter.event(Some(turn), EventBody::failed(format!("could not read the T3 Code thread {thread} with `t3 thread show`")), None);
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
        let status = format!("the T3 Code thread's run ended with status '{}'", state.status);
        match run_errors(mcp, cli, thread, state.run.as_deref()).await {
            Ok((errors, resets_at)) if !errors.is_empty() => {
                let mut failure = Failure::from_text(format!("{status}: {errors} ({link})"));
                if failure.kind == FailureKind::RateLimited {
                    failure.resets_at = resets_at;
                }
                failure.into()
            }
            Ok(_) => EventBody::failed(format!("{status}: {link}")),
            Err(error) => {
                reporter.notice(Some(turn), format!("could not read the T3 Code thread's activity for its error: {error}"));
                EventBody::failed(format!("{status}: {link}"))
            }
        }
    };
    reporter.event(Some(turn), body, None);
    true
}

/// The error items T3 Code's thread activity (`t3_thread_read`) holds for `run`, such as the provider's rate limit,
/// as one text, and when its usage-limit notice says the limit resets; `t3 thread show` has neither. The newest items:
/// without `beforePosition`, T3 gives a client outside a thread the oldest ones, which miss the error of a run that
/// did some work first.
async fn run_errors(mcp: &mut Option<Mcp>, cli: &Cli, thread: &str, run: Option<&str>) -> Result<(String, Option<String>)> {
    let arguments = json!({"threadId": thread, "view": "activity", "limit": 20, "maxCharsPerItem": 2000, "beforePosition": NEWEST});
    let read = call(mcp, cli, "t3_thread_read", arguments).await?;
    Ok((errors_of(&read, run), resets_of(&read, run)))
}

/// `run`'s activity items of `kind`.
fn items_of<'a>(read: &'a Value, run: Option<&'a str>, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    read.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |item| item.get("type").and_then(Value::as_str) == Some(kind))
        .filter(move |item| run.is_none_or(|run| item.get("runId").and_then(Value::as_str) == Some(run)))
}

/// When the usage limit resets, from T3's own `system_notice` for it ("Claude usage limit reached. This turn is paused
/// until the 7-day limit resets in 31h 5m."): the Unix seconds its itemId ends in (`…usage-limit…:seven_day:1795536000`),
/// else the text's "resets in" from the notice's `updatedAt`.
fn resets_of(read: &Value, run: Option<&str>) -> Option<String> {
    let notice =
        items_of(read, run, "system_notice").filter(|item| item.get("itemId").and_then(Value::as_str).is_some_and(|id| id.contains("usage-limit"))).last()?;
    let from_id =
        notice.get("itemId").and_then(Value::as_str).and_then(|id| id.rsplit(':').next()).and_then(|secs| secs.parse::<u64>().ok()).filter(|secs| *secs > 0);
    let from_text = || {
        let text = notice.get("text").and_then(Value::as_str)?;
        let after = text.split("resets in ").nth(1)?;
        let mut secs = 0;
        for part in after.trim_end_matches('.').split_whitespace() {
            let unit = match part.chars().last()? {
                'd' => 86_400,
                'h' => 3_600,
                'm' => 60,
                's' => 1,
                _ => break,
            };
            let Ok(count) = part[..part.len() - 1].parse::<u64>() else { break };
            secs += count * unit;
        }
        let since = event::parse_rfc3339(notice.get("updatedAt").and_then(Value::as_str)?)?;
        (secs > 0).then_some(since + secs)
    };
    from_id.or_else(from_text).map(|secs| event::format_rfc3339(secs, 0))
}

fn errors_of(read: &Value, run: Option<&str>) -> String {
    let errors: Vec<String> = items_of(read, run, "error")
        .filter_map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
            match (field("title"), field("text")) {
                (Some(title), Some(text)) if !text.starts_with(title) => Some(format!("{title}: {text}")),
                (_, Some(text)) => Some(text.to_string()),
                (title, None) => title.map(str::to_string),
            }
        })
        .collect();
    errors.join("; ")
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

/// Look up the thread asked for at `since` and save what was found: its id, or that T3 made none. `Err` with the reason
/// when `t3 thread list` can't be read.
async fn find_thread(program: &std::path::Path, start: &Start, since: &str, reporter: &Reporter) -> std::result::Result<Option<String>, String> {
    let found = started_thread(program, start, since).await?;
    match &found {
        Some(id) => {
            reporter.report(Report::AgentSession { id: id.clone(), adapter: None });
            reporter.notice(None, format!("The thread's id wasn't saved when it was asked for; T3 Code thread {id} has the session's title and was made then"));
        }
        None => note_asked(reporter, None).await,
    }
    Ok(found)
}

/// Note in the record when the agent was asked to make the session's thread (none: it made none), and wait until it is
/// saved.
async fn note_asked(reporter: &Reporter, since: Option<String>) {
    let (saved, wait) = tokio::sync::oneshot::channel();
    reporter.report(Report::AgentSessionAsked { since, saved });
    let _ = wait.await;
}

/// The thread `t3 thread start` made after `since` for the session, from `t3 thread list --json`: the newest one with
/// the session's title created since then. None without a title; `Err` with the reason when the list can't be read
/// [`READ_ATTEMPTS`] times in a row.
async fn started_thread(program: &std::path::Path, start: &Start, since: &str) -> std::result::Result<Option<String>, String> {
    let Some(title) = start.record.profile.title.as_deref() else { return Ok(None) };
    let project = start.record.project.clone().unwrap_or_else(|| start.record.cwd.clone());
    let mut why = String::new();
    for attempt in 0..READ_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(POLL).await;
        }
        let output = tokio::process::Command::new(program)
            .args(["thread", "list", "--project"])
            .arg(&project)
            .arg("--json")
            .envs(start.spec.env.iter().chain(&start.record.profile.env))
            .current_dir(&start.record.cwd)
            .stdin(Stdio::null())
            .output()
            .await;
        why = match output {
            Err(error) => format!("starting {}: {error}", program.display()),
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let line = stderr.lines().rev().find(|line| !is_noise(line) && !line.trim().is_empty()).unwrap_or_default().trim().to_string();
                format!("t3 exited with {}{}", output.status, if line.is_empty() { String::new() } else { format!(": {line}") })
            }
            Ok(output) => match serde_json::from_slice::<Value>(&output.stdout) {
                Ok(threads) if threads.is_array() => {
                    return Ok(thread_made_since(&threads, title, since));
                }
                _ => "its output is not a JSON list of threads".to_string(),
            },
        };
    }
    Err(why)
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
    /// The run: the active one, else the latest.
    run: Option<String>,
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
        let waiting = thread.and_then(|t| t.get("pendingRequestCount")).and_then(Value::as_u64).is_some_and(|count| count > 0);
        // A run that ended waiting on its own background task or on another thread: T3 Code resumes it on its own.
        let resumes = status == "waiting"
            && (thread.and_then(|t| t.get("pendingBackgroundTasks")).and_then(Value::as_array).is_some_and(|tasks| !tasks.is_empty())
                || thread.and_then(|t| t.get("blockedOnThreadWait")).and_then(Value::as_bool) == Some(true));
        Self { run: run.map(str::to_string), running: matches!(status.as_str(), "running" | "starting") || active || resumes, waiting, status, reply }
    }
}

/// New assistant messages of the thread's current run, from `t3 thread show --json`, and the turn's [`Activity`].
#[derive(Default)]
struct Follow {
    run: Option<String>,
    seen: usize,
    link: Option<String>,
    activity: Activity,
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

/// JavaScript's largest safe integer: a read before it gets the newest items, as T3 Code needs a safe integer there.
const NEWEST: u64 = (1 << 53) - 1;
/// Items per activity read (T3 Code's most).
const PAGE: u64 = 100;
/// Pages one poll reads at most; the rest come on the next poll.
const PAGES: usize = 10;
/// Characters of an item's text per read; a reply or thought cut off here is read on by its id.
const ITEM_CHARS: u64 = 4000;
/// Reads one reply or thought takes at most.
const TEXT_READS: usize = 20;

/// The turn's run as T3 Code's thread activity (`t3_thread_read`, view `activity`) shows it: its replies, thoughts and
/// tool calls in T3's order, each sent once. When the activity can't be read, the rest of the turn streams replies from
/// `t3 thread show` ([`Follow`]).
#[derive(Default)]
struct Activity {
    run: Option<String>,
    /// The next read starts after this position; none: the newest items.
    after: Option<i64>,
    /// The run's items sent so far, and whether each has ended.
    sent: HashMap<String, bool>,
    /// The run's replies that ended in the activity: a fallback to `t3 thread show` sends the replies after them.
    replies: usize,
    /// Each to-do list's text as last sent: a list sends a `plan` each time it changes.
    plans: HashMap<String, String>,
    /// The activity couldn't be read in this turn.
    off: bool,
}

impl Activity {
    /// The run's new events since the last read, with T3's item as `raw`.
    async fn read(&mut self, mcp: &mut Option<Mcp>, cli: &Cli, thread: &str, run: &str) -> Result<Vec<(EventBody, Value)>> {
        if self.run.as_deref() != Some(run) {
            *self = Self { run: Some(run.to_string()), ..Self::default() };
        }
        let mut items = Vec::new();
        // Pages forward from the read position; the first read pages back from the newest item to the run's start.
        let (mut after, mut before) = (self.after, NEWEST);
        for _ in 0..PAGES {
            let mut arguments = json!({"threadId": thread, "view": "activity", "limit": PAGE, "maxCharsPerItem": ITEM_CHARS});
            match after {
                Some(position) => arguments["afterPosition"] = json!(position),
                None => arguments["beforePosition"] = json!(before),
            }
            let read = call(mcp, cli, "t3_thread_read", arguments).await?;
            let page = read.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let (first, last) = (page.iter().filter_map(position).min(), page.iter().filter_map(position).max());
            let older_run = page.iter().any(|item| {
                item.get("runId").and_then(Value::as_str) != Some(run)
                    && item.get("sourceThreadId").and_then(Value::as_str).is_none_or(|source| source == thread)
            });
            let full = page.len() as u64 >= PAGE;
            items.extend(page);
            match (after, first, last) {
                (Some(_), _, Some(last)) if read.get("hasMore").and_then(Value::as_bool) == Some(true) => after = i64::try_from(last).ok(),
                (None, Some(first), _) if full && !older_run && first > 0 => before = first,
                _ => break,
            }
        }
        let mut events = Vec::new();
        // An item's text cut off by the read is read on by its id, once per poll.
        let mut whole: HashMap<String, String> = HashMap::new();
        for (body, item) in self.take(items, thread, run) {
            let body = match item.get("itemId").and_then(Value::as_str) {
                Some(id) if item.get("textTruncated").and_then(Value::as_bool) == Some(true) => {
                    if !whole.contains_key(id) {
                        whole.insert(id.to_string(), full_text(mcp, cli, thread, &item).await?);
                    }
                    fill(body, &item, &whole[id])
                }
                _ => body,
            };
            events.push((body, item));
        }
        Ok(events)
    }

    /// The events of `items` (one read) not sent yet, each with its item; moves the next read's start.
    fn take(&mut self, mut items: Vec<Value>, thread: &str, run: &str) -> Vec<(EventBody, Value)> {
        items.sort_by_key(|item| position(item).unwrap_or(u64::MAX));
        let mut events = Vec::new();
        let mut oldest_open = None;
        for item in &items {
            if item.get("runId").and_then(Value::as_str) != Some(run)
                || item.get("sourceThreadId").and_then(Value::as_str).is_some_and(|source| source != thread)
            {
                continue;
            }
            let Some(id) = item.get("itemId").and_then(Value::as_str) else { continue };
            let item_text = item.get("text").and_then(Value::as_str).unwrap_or_default();
            let ended = ended(item);
            if !ended {
                oldest_open = oldest_open.or(position(item));
            }
            let sent = self.sent.get(id).copied();
            if sent == Some(true) || (sent == Some(false) && !ended) {
                continue;
            }
            match kind_of(item) {
                Kind::Skipped => {}
                Kind::Plan => {
                    if self.plans.get(id).map(String::as_str) != Some(item_text) {
                        events.push((fill(EventBody::Plan { entries: Vec::new() }, item, item_text), item.clone()));
                        self.plans.insert(id.to_string(), item_text.to_string());
                    }
                }
                Kind::Text(text) => {
                    if ended {
                        if item.get("type").and_then(Value::as_str) == Some("assistant_message") {
                            self.replies += 1;
                        }
                        if !item_text.trim().is_empty() {
                            events.push((fill(text(String::new()), item, item_text), item.clone()));
                        }
                        self.sent.insert(id.to_string(), true);
                    }
                }
                Kind::Tool { title, kind } => {
                    if sent.is_none() {
                        let started = EventBody::ToolStarted { id: id.to_string(), title: title.clone(), kind, input: None, locations: Vec::new() };
                        events.push((fill(started, item, item_text), item.clone()));
                    }
                    if ended {
                        let status = if item.get("status").and_then(Value::as_str) == Some("completed") { "completed" } else { "failed" };
                        let finished = EventBody::ToolFinished { id: id.to_string(), status: status.into(), title: Some(title), output: None };
                        events.push((fill(finished, item, item_text), item.clone()));
                    }
                    self.sent.insert(id.to_string(), ended);
                }
            }
        }
        // An item still running may change, so the next read starts at it.
        self.after = match oldest_open {
            Some(open) => i64::try_from(open).ok().map(|open| open - 1),
            None => items.iter().filter_map(position).max().and_then(|last| i64::try_from(last).ok()).or(self.after),
        };
        events
    }
}

fn position(item: &Value) -> Option<u64> {
    item.get("position").and_then(Value::as_u64)
}

/// Whether the item is done: T3 Code marks one that still runs `running`.
fn ended(item: &Value) -> bool {
    !matches!(item.get("status").and_then(Value::as_str), Some("running" | "pending" | "in_progress" | "starting"))
}

enum Kind {
    /// The prompt, T3's own notes and checkpoints, errors (which end a failed turn, [`errors_of`]) and requests to the
    /// user (a thread that waits on one ends the turn waiting, [`waiting`]).
    Skipped,
    /// A reply, a proposed plan (markdown) or a thought: the event for its text.
    Text(fn(String) -> EventBody),
    /// A to-do list: a `plan` event each time it changes.
    Plan,
    Tool {
        title: String,
        kind: Option<String>,
    },
}

fn kind_of(item: &Value) -> Kind {
    let field = |key: &str| item.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
    let first_line = || field("text").and_then(|text| text.lines().next()).map(|line| line.trim_start_matches("$ ").trim().to_string());
    let item_type = field("type").unwrap_or("tool");
    match item_type {
        "user_message" | "checkpoint" | "notification" | "error" => Kind::Skipped,
        "system_notice" | "run_interrupt_request" | "run_interrupt_result" | "compaction" | "handoff" | "fork" | "thread_created" => Kind::Skipped,
        "approval_request" | "user_input_request" | "secret_request" | "link_request" => Kind::Skipped,
        "assistant_message" | "proposed_plan" => Kind::Text(|text| EventBody::MessageDelta { text }),
        "todo_list" => Kind::Plan,
        "reasoning" => Kind::Text(|text| EventBody::ThoughtDelta { text }),
        "command_execution" => Kind::Tool { title: first_line().unwrap_or_else(|| item_type.into()), kind: Some("execute".into()) },
        "file_change" => {
            Kind::Tool { title: first_line().or_else(|| field("title").map(str::to_string)).unwrap_or_else(|| item_type.into()), kind: Some("edit".into()) }
        }
        _ => {
            // A tool's text is its call as JSON, maybe cut off; only its name is needed.
            let name = || {
                let text = field("text")?;
                let rest = &text[text.find("\"toolName\"")? + "\"toolName\"".len()..];
                let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
                Some(rest[..rest.find('"')?].to_string())
            };
            Kind::Tool { title: field("title").map(str::to_string).or_else(name).unwrap_or_else(|| item_type.into()), kind: None }
        }
    }
}

/// `body` with what `text`, the item's text, says (T3 Code's `turnItemText`): a reply's or thought's text; a tool's
/// input and paths when it starts, its output when it ends. A command's text is `$ <command>` and its output; a file
/// change's is its path, `+N -M` and the diff; a to-do list's is its explanation and a `[<status>] <step>` line per
/// step; another tool's is JSON with `toolName`, `input` and `output`.
fn fill(body: EventBody, item: &Value, text: &str) -> EventBody {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let call = || (item_type == "dynamic_tool").then(|| serde_json::from_str::<Value>(text).ok()).flatten();
    match body {
        EventBody::MessageDelta { .. } => EventBody::MessageDelta { text: format!("{text}\n\n") },
        EventBody::ThoughtDelta { .. } => EventBody::ThoughtDelta { text: format!("{text}\n\n") },
        EventBody::Plan { .. } => EventBody::Plan { entries: text.lines().filter_map(plan_entry).collect() },
        EventBody::ToolStarted { id, title, kind, .. } => {
            let input = call().and_then(|call| call.get("input").cloned()).filter(|input| !input.is_null());
            let path = (item_type == "file_change").then(|| text.lines().next().map(str::trim).filter(|path| !path.is_empty())).flatten();
            EventBody::ToolStarted { id, title, kind, input, locations: path.map(str::to_string).into_iter().collect() }
        }
        EventBody::ToolFinished { id, status, title, .. } => {
            let output = match item_type {
                "command_execution" => text.split_once('\n').map(|(_, output)| output.to_string()),
                "dynamic_tool" => call().and_then(|call| match call.get("output")? {
                    Value::Null => None,
                    Value::String(output) => Some(output.clone()),
                    output => Some(output.to_string()),
                }),
                _ => None,
            };
            EventBody::ToolFinished {
                id,
                status,
                title,
                output: output.filter(|output| !output.is_empty()).map(|output| crate::acp_map::cap_output(&output, 0)),
            }
        }
        body => body,
    }
}

/// A to-do list's step from its `[<status>] <step>` line; T3's `running` is ACP's `in_progress`. The explanation line
/// has no status and gives none.
fn plan_entry(line: &str) -> Option<PlanEntry> {
    let (status, content) = line.strip_prefix('[')?.split_once("] ")?;
    let status = if status == "running" { "in_progress" } else { status };
    Some(PlanEntry { content: content.trim().to_string(), status: status.to_string(), priority: String::new() })
}

/// The item's whole text, read on by its id where the activity read cut it off.
async fn full_text(mcp: &mut Option<Mcp>, cli: &Cli, thread: &str, item: &Value) -> Result<String> {
    let mut text = item.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut next = item.get("nextTextOffset").and_then(Value::as_u64).filter(|_| item.get("textTruncated").and_then(Value::as_bool) == Some(true));
    let id = item.get("itemId").and_then(Value::as_str).unwrap_or_default();
    for _ in 0..TEXT_READS {
        let Some(offset) = next else { break };
        let arguments = json!({"threadId": thread, "view": "activity", "itemId": id, "textOffset": offset, "maxCharsPerItem": 20_000});
        let read = call(mcp, cli, "t3_thread_read", arguments).await?;
        let Some(part) = read.get("items").and_then(Value::as_array).and_then(|items| items.first()) else { break };
        text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
        next = part.get("nextTextOffset").and_then(Value::as_u64).filter(|_| part.get("textTruncated").and_then(Value::as_bool) == Some(true));
    }
    Ok(text)
}

/// Sends what the thread did since the last poll: from its activity, else its replies from `t3 thread show`. The
/// activity is read only once `current`, the turn's run, shows.
#[expect(clippy::too_many_arguments, reason = "the run loop's state, passed down")]
async fn stream(mcp: &mut Option<Mcp>, cli: &Cli, thread: &str, shown: &Value, current: bool, follow: &mut Follow, turn: u64, reporter: &Reporter) {
    // While the activity follows the run, `t3 thread show`'s replies count from the last one that ended in it, so a
    // fallback also sends those the activity saw still running.
    if !follow.activity.off && follow.activity.run.is_some() && follow.activity.run == follow.run {
        follow.seen = follow.activity.replies;
    }
    let replies = follow.take_new(shown);
    let activity = &mut follow.activity;
    if !activity.off
        && current
        && let Some(run) = follow.run.clone()
    {
        match activity.read(mcp, cli, thread, &run).await {
            Ok(events) => {
                for (body, raw) in events {
                    reporter.event(Some(turn), body, Some(raw));
                }
                return;
            }
            Err(error) => {
                activity.off = true;
                reporter.notice(
                    Some(turn),
                    format!("could not read the T3 Code thread's activity, so this turn shows its replies but not its tool calls: {error}"),
                );
            }
        }
    }
    if follow.activity.off || follow.activity.run.is_none() {
        for text in replies {
            reporter.event(Some(turn), EventBody::MessageDelta { text }, None);
        }
    }
}

/// The thread id from `t3 thread start`'s `Thread <id>` line on stderr.
fn thread_id(line: &str) -> Option<String> {
    let id = line.trim().strip_prefix("Thread ")?.trim();
    (!id.is_empty() && !id.contains(char::is_whitespace)).then(|| id.to_string())
}

/// Electron's note that the t3 launcher runs Node with app defaults, printed on every run.
pub(super) fn is_noise(line: &str) -> bool {
    line.contains(":ERROR:electron/") && line.contains("Node.js environment variables are disabled")
}

fn cli_of(start: &Start, program: &Path, args: &[String]) -> Cli {
    let env = start.spec.env.iter().chain(&start.record.profile.env).map(|(key, value)| (key.clone(), value.clone())).collect();
    Cli { program: program.to_path_buf(), args: args.to_vec(), env, cwd: start.record.cwd.clone() }
}

/// Calls a T3 Code MCP tool, signing in on the first call.
async fn call(mcp: &mut Option<Mcp>, cli: &Cli, tool: &str, arguments: Value) -> Result<Value> {
    let connected = match mcp {
        Some(connected) => connected,
        None => mcp.insert(Mcp::connect(cli).await?),
    };
    connected.call(cli, tool, arguments).await
}

/// A request id for T3 Code: the same for the same session (`created` tells apart sessions of one name) and `what`.
fn request_id(start: &Start, what: &str) -> String {
    format!("harnessmux:{}:{}:{what}", start.record.name, start.record.created)
}

/// The worktree `cwd` is in when that is a linked worktree of `project`, not its main checkout.
fn worktree_of(cwd: &Path, project: Option<&Path>) -> Option<PathBuf> {
    let project = project?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let top = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let same = |a: &Path, b: &Path| a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b);
    (!same(&top, project)).then_some(top)
}

struct Launched {
    thread: String,
    link: Option<String>,
    run: Option<String>,
}

/// Starts the session's thread in `worktree` with `t3_thread_launch`, full access.
async fn launch(mcp: &mut Option<Mcp>, cli: &Cli, start: &Start, worktree: &Path, text: &str) -> Result<Launched> {
    let project = start.record.project.as_deref().ok_or_else(|| Error::Invalid("a t3 session in a worktree needs its repository".into()))?;
    let project_id = project_id(mcp, cli, project).await?;
    let profile = &start.record.profile;
    let mut arguments = json!({
        "title": profile.title.clone().unwrap_or_else(|| start.record.name.clone()),
        "projectId": project_id,
        "workspaceStrategy": {"type": "existing_worktree", "worktreePath": worktree.display().to_string()},
        "runtimeMode": "full-access",
        "message": text,
        "clientRequestId": request_id(start, "launch"),
    });
    if let (Some(provider), Some(model)) = (&profile.provider, &profile.model) {
        arguments["modelSelection"] = json!({"instanceId": provider, "model": model});
        let options: Vec<Value> = model_options(profile)
            .into_iter()
            .map(|(id, value)| {
                // As `t3 thread start --option` sends them.
                let value = match value.as_str() {
                    "true" => json!(true),
                    "false" => json!(false),
                    _ => json!(value),
                };
                json!({"id": id, "value": value})
            })
            .collect();
        if !options.is_empty() {
            arguments["modelSelection"]["options"] = json!(options);
        }
    }
    let launched = call(mcp, cli, "t3_thread_launch", arguments).await?;
    let text = |key: &str| launched.get(key).and_then(Value::as_str).map(str::to_string);
    let thread = text("threadId").ok_or_else(|| Error::Agent(format!("T3 Code's t3_thread_launch named no thread: {launched}")))?;
    Ok(Launched { thread, link: text("link"), run: text("runId") })
}

/// The profile's model options for T3 (`--option id=value`): its effort as `effort`, which wins over an `effort` in its
/// `options` (T3 refuses an id given twice), then the `options`.
fn model_options(profile: &crate::Profile) -> Vec<(String, String)> {
    let effort = profile.effort.map(|effort| ("effort".to_string(), effort.to_string()));
    let has_effort = effort.is_some();
    let others = profile.options.iter().filter(|(id, _)| !has_effort || id.as_str() != "effort").map(|(id, value)| (id.clone(), value.clone()));
    effort.into_iter().chain(others).collect()
}

/// The id of the T3 Code project whose workspace is `project`.
async fn project_id(mcp: &mut Option<Mcp>, cli: &Cli, project: &Path) -> Result<String> {
    let wanted = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
    let mut cursor = Value::Null;
    loop {
        let mut arguments = json!({"limit": 100});
        if !cursor.is_null() {
            arguments["cursor"] = cursor;
        }
        let page = call(mcp, cli, "t3_project_list", arguments).await?;
        let found = page
            .get("projects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|p| p.get("workspaceRoot").and_then(Value::as_str).is_some_and(|root| Path::new(root).canonicalize().is_ok_and(|root| root == wanted)));
        if let Some(id) = found.and_then(|p| p.get("id")).and_then(Value::as_str) {
            return Ok(id.to_string());
        }
        cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            return Err(Error::Agent(format!("T3 Code has no project for {}; add it in T3 Code first", project.display())));
        }
    }
}

/// Moves the session's thread to T3's Settled group (it stays reachable by its link), only when its last run finished
/// and it doesn't wait for an answer; a thread that runs, waits, failed or was interrupted stays active. Returns the
/// thread's link.
pub async fn settle(spec: &AgentSpec, record: &crate::registry::SessionRecord) -> Result<String> {
    if spec.kind != crate::agents::AgentKind::T3 {
        return Err(Error::Invalid(format!("session {} runs {}; only t3 sessions have a thread to settle", record.name, record.agent)));
    }
    let thread = record.agent_session.clone().ok_or_else(|| Error::Invalid(format!("session {} has no T3 Code thread to settle", record.name)))?;
    let (program, args) = spec.launch_command().await?;
    let start = Start { spec: spec.clone(), record: record.clone(), resume: true, interrupted: false };
    let Shown::Thread(shown) = show(&program, &start, &thread).await else {
        return Err(Error::Agent(format!("could not read the T3 Code thread {thread} with `t3 thread show`; not settled")));
    };
    let state = RunState::of(&shown);
    let link = link(&shown).unwrap_or_else(|| thread.clone());
    let refused = if state.waiting {
        Some("waits for an answer".to_string())
    } else if state.running {
        Some("still runs".to_string())
    } else if state.status != "completed" {
        Some(format!("ended its last run with status '{}'", state.status))
    } else {
        None
    };
    if let Some(why) = refused {
        return Err(Error::Invalid(format!("the T3 Code thread {why}, so it stays active: {link}")));
    }
    let cli = cli_of(&start, &program, &args);
    call(&mut None, &cli, "t3_thread_organize", json!({"threadId": thread, "action": "settle"})).await?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_limit_notice_gives_the_reset() {
        // The items of a run T3 Code paused on Claude's weekly limit (2026-11-23, lotar DEV-855), ids shortened.
        let notice = |id: &str| {
            json!({"runId": "r1", "type": "system_notice", "itemId": id, "updatedAt": "2026-11-23T08:55:39.479Z",
                "text": "Claude usage limit reached. This turn is paused until the 7-day limit resets in 31h 5m."})
        };
        let error = json!({"runId": "r1", "type": "error", "itemId": "turn-item:terminal-failure:r1", "title": "Usage limit reached",
            "text": "Claude API rate limit reached. Try again later."});
        let read = json!({"items": [notice("turn-item:provider:claudeAgent:native-item:usage-limit%3Aprovider-turn%3Ar1:seven_day:1795536000"), error]});
        assert_eq!(errors_of(&read, Some("r1")), "Usage limit reached: Claude API rate limit reached. Try again later.");
        assert_eq!(resets_of(&read, Some("r1")).as_deref(), Some("2026-11-24T16:00:00.000Z"));
        assert_eq!(resets_of(&read, Some("r2")), None);
        // Without the epoch in its id, the text's "resets in" counts from the notice.
        let read = json!({"items": [notice("turn-item:usage-limit:r1"), error]});
        assert_eq!(resets_of(&read, Some("r1")).as_deref(), Some("2026-11-24T16:00:39.000Z"));
        assert_eq!(resets_of(&json!({"items": [error]}), Some("r1")), None);
    }

    #[test]
    fn effort_and_options_become_t3_model_options_with_effort_winning() {
        let mut profile = crate::Profile { effort: Some(crate::profile::Effort::High), ..Default::default() };
        profile.options.insert("effort".into(), "low".into());
        profile.options.insert("fastMode".into(), "true".into());
        let pair = |id: &str, value: &str| (id.to_string(), value.to_string());
        assert_eq!(model_options(&profile), [pair("effort", "high"), pair("fastMode", "true")]);
        profile.effort = None;
        assert_eq!(model_options(&profile), [pair("effort", "low"), pair("fastMode", "true")]);
    }

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
    fn a_runs_activity_becomes_its_tool_calls_thoughts_and_replies_once_in_order() {
        let item = |position: u64, id: &str, kind: &str, status: &str, text: &str| json!({"position": position, "itemId": id, "runId": "r1", "type": kind, "status": status, "title": null, "text": text});
        let mut activity = Activity::default();
        let first = vec![
            item(3, "c1", "command_execution", "running", "$ cargo test\nrunning 3 tests"),
            item(1, "u1", "user_message", "completed", "Fix it"),
            item(2, "th1", "reasoning", "completed", "Looking at the tests."),
            json!({"position": 0, "itemId": "old", "runId": "r0", "type": "command_execution", "status": "completed", "text": "$ ls"}),
            json!({"position": 4, "itemId": "kid", "runId": "r1", "sourceThreadId": "child", "type": "command_execution", "status": "running", "text": "$ ls"}),
        ];
        let bodies = |events: Vec<(EventBody, Value)>| events.into_iter().map(|(body, _)| body).collect::<Vec<_>>();
        let started = |id: &str, title: &str, kind: Option<&str>, locations: &[&str]| EventBody::ToolStarted {
            id: id.into(),
            title: title.into(),
            kind: kind.map(str::to_string),
            input: None,
            locations: locations.iter().map(|path| path.to_string()).collect(),
        };
        let finished = |id: &str, status: &str, title: &str, output: Option<&str>| EventBody::ToolFinished {
            id: id.into(),
            status: status.into(),
            title: Some(title.into()),
            output: output.map(str::to_string),
        };
        assert_eq!(
            bodies(activity.take(first.clone(), "t1", "r1")),
            [EventBody::ThoughtDelta { text: "Looking at the tests.\n\n".into() }, started("c1", "cargo test", Some("execute"), &[])]
        );
        // The running command holds the next read's start, so its end isn't missed.
        assert_eq!(activity.after, Some(2));
        assert!(activity.take(first, "t1", "r1").is_empty());
        let second = vec![
            item(3, "c1", "command_execution", "completed", "$ cargo test\nok"),
            item(5, "e1", "file_change", "failed", "/work/repo/src/lib.rs\n@@ -1 +1 @@"),
            json!({"position": 6, "itemId": "d1", "runId": "r1", "type": "dynamic_tool", "status": "completed", "title": null, "text": "{\n  \"toolName\": \"mcp__t3-code__t3_thread_read\",\n  \"input\": {\"thr"}),
            item(7, "m1", "assistant_message", "running", "Done"),
        ];
        assert_eq!(
            bodies(activity.take(second.clone(), "t1", "r1")),
            [
                finished("c1", "completed", "cargo test", Some("ok")),
                started("e1", "/work/repo/src/lib.rs", Some("edit"), &["/work/repo/src/lib.rs"]),
                finished("e1", "failed", "/work/repo/src/lib.rs", None),
                // A tool's JSON cut off by the read gives no input or output here; [`Activity::read`] reads it whole.
                started("d1", "mcp__t3-code__t3_thread_read", None, &[]),
                finished("d1", "completed", "mcp__t3-code__t3_thread_read", None),
            ]
        );
        // A reply is sent once it has ended, whole.
        assert_eq!(activity.after, Some(6));
        let mut third = second;
        third[3]["status"] = json!("completed");
        assert_eq!(bodies(activity.take(third, "t1", "r1")), [EventBody::MessageDelta { text: "Done\n\n".into() }]);
        assert_eq!(activity.after, Some(7));
        assert!(activity.take(Vec::new(), "t1", "r1").is_empty());
        assert_eq!(activity.after, Some(7));
    }

    #[test]
    fn a_t3_tool_is_named_by_its_title_else_by_what_it_ran() {
        let title = |item: Value| match kind_of(&item) {
            Kind::Tool { title, .. } => title,
            _ => panic!("not a tool: {item}"),
        };
        assert_eq!(title(json!({"type": "dynamic_tool", "title": "Read /work/repo/a.rs", "text": "{\"toolName\": \"Read\"}"})), "Read /work/repo/a.rs");
        assert_eq!(title(json!({"type": "dynamic_tool", "title": null, "text": "{\"toolName\": \"Grep\", \"input\": {"})), "Grep");
        assert_eq!(title(json!({"type": "web_search", "title": null, "text": ""})), "web_search");
        // A file change is named by its path; some adapters (Pi) title it by the tool, `edit` or `write`.
        assert_eq!(title(json!({"type": "file_change", "title": "edit", "text": "/work/repo/a.rs\n+1 -0\n@@ -1 +1,2 @@"})), "/work/repo/a.rs");
        assert_eq!(title(json!({"type": "file_change", "title": "write", "text": null})), "write");
        assert!(matches!(kind_of(&json!({"type": "error", "text": "limit"})), Kind::Skipped));
    }

    #[test]
    fn t3s_notes_and_requests_send_nothing_and_its_searches_and_subagents_are_tools() {
        let notes = ["system_notice", "run_interrupt_request", "run_interrupt_result", "compaction", "handoff", "fork", "thread_created"];
        let requests = ["approval_request", "user_input_request", "secret_request", "link_request"];
        for item_type in notes.into_iter().chain(requests) {
            assert!(matches!(kind_of(&json!({"type": item_type, "text": "x"})), Kind::Skipped), "{item_type}");
        }
        for item_type in ["file_search", "web_search", "subagent", "dynamic_tool"] {
            assert!(matches!(kind_of(&json!({"type": item_type, "text": "x"})), Kind::Tool { .. }), "{item_type}");
        }
        let Kind::Text(text) = kind_of(&json!({"type": "proposed_plan", "text": "# Plan"})) else { panic!() };
        assert_eq!(text("# Plan".into()), EventBody::MessageDelta { text: "# Plan".into() });
        assert!(matches!(kind_of(&json!({"type": "todo_list", "text": "[pending] a"})), Kind::Plan));
    }

    #[test]
    fn a_t3_todo_list_is_a_plan_sent_each_time_it_changes() {
        let item = |status: &str, text: &str| json!({"position": 1, "itemId": "p1", "runId": "r1", "type": "todo_list", "status": status, "title": null, "text": text});
        let entry = |content: &str, status: &str| PlanEntry { content: content.into(), status: status.into(), priority: String::new() };
        let mut activity = Activity::default();
        let first = item("running", "Fix the tests first.\n[running] Run the tests\n[pending] Fix the failure");
        assert_eq!(
            activity.take(vec![first.clone()], "t1", "r1").into_iter().map(|(body, _)| body).collect::<Vec<_>>(),
            [EventBody::Plan { entries: vec![entry("Run the tests", "in_progress"), entry("Fix the failure", "pending")] }]
        );
        assert!(activity.take(vec![first], "t1", "r1").is_empty());
        let done = item("completed", "[completed] Run the tests\n[completed] Fix the failure");
        assert_eq!(
            activity.take(vec![done.clone()], "t1", "r1").into_iter().map(|(body, _)| body).collect::<Vec<_>>(),
            [EventBody::Plan { entries: vec![entry("Run the tests", "completed"), entry("Fix the failure", "completed")] }]
        );
        assert!(activity.take(vec![done], "t1", "r1").is_empty());
    }

    #[test]
    fn a_t3_tools_input_paths_and_output_come_from_its_text() {
        let started = EventBody::ToolStarted { id: "x".into(), title: "x".into(), kind: None, input: None, locations: Vec::new() };
        let finished = EventBody::ToolFinished { id: "x".into(), status: "completed".into(), title: None, output: None };
        let item = |kind: &str| json!({"type": kind});
        let call = json!({"toolName": "Read", "input": {"file_path": "/work/repo/a.rs"}, "output": "fn main() {}"}).to_string();
        let EventBody::ToolStarted { input, .. } = fill(started.clone(), &item("dynamic_tool"), &call) else { panic!() };
        assert_eq!(input, Some(json!({"file_path": "/work/repo/a.rs"})));
        let EventBody::ToolFinished { output, .. } = fill(finished.clone(), &item("dynamic_tool"), &call) else { panic!() };
        assert_eq!(output.as_deref(), Some("fn main() {}"));
        // Output that isn't text is its JSON.
        let call = json!({"toolName": "Grep", "input": {}, "output": {"matches": 2}}).to_string();
        let EventBody::ToolFinished { output, .. } = fill(finished.clone(), &item("dynamic_tool"), &call) else { panic!() };
        assert_eq!(output.as_deref(), Some(r#"{"matches":2}"#));
        let EventBody::ToolStarted { locations, input, .. } = fill(started, &item("file_change"), "/work/repo/a.rs\n+1 -0") else { panic!() };
        assert_eq!((locations, input), (vec!["/work/repo/a.rs".to_string()], None));
        // A command's output is what follows its `$ ` line, capped like any tool's.
        let EventBody::ToolFinished { output, .. } = fill(finished.clone(), &item("command_execution"), "$ ls") else { panic!() };
        assert_eq!(output, None);
        let long = format!("$ yes\n{}", "y\n".repeat(crate::event::OUTPUT_LIMIT));
        let EventBody::ToolFinished { output, .. } = fill(finished, &item("command_execution"), &long) else { panic!() };
        assert!(output.unwrap().ends_with(&format!("… [{} more bytes]", crate::event::OUTPUT_LIMIT)));
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
        let background = |tasks: Value, blocked: bool| {
            RunState::of(&json!({"thread": {"status": "waiting", "activeRunId": null, "latestRunId": "r2", "pendingRequestCount": 0,
                "pendingBackgroundTasks": tasks, "blockedOnThreadWait": blocked}, "messages": []}))
        };
        assert!(background(json!([{"taskId": "b1"}]), false).running);
        assert!(background(json!([]), true).running);
        let stalled = background(json!([]), false);
        assert!(!stalled.running && !stalled.waiting);
    }
}
