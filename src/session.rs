//! A session's owner: holds the agent connection, queues prompts, writes the event log and serves the socket.
//!
//! The process that starts or resumes a session owns it (lotar's process for lotar's jobs, `harnessmux serve` for the
//! CLI). Other processes reach it through [`crate::ipc`].

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::acp_map::ConfigOption;
use crate::agents::AgentTable;
use crate::backend::{self, Command, Report, Reporter};
use crate::error::{Error, Result};
use crate::event::{self, EVENT_VERSION, Event, EventBody};
use crate::ipc::{self, Reply, Request};
use crate::profile::Profile;
use crate::registry::{Registry, SessionRecord, SessionState};

/// What a new session starts with.
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// Defaults to `<agent>-<time>`.
    pub name: Option<String>,
    pub agent: String,
    pub cwd: PathBuf,
    pub profile: Profile,
    /// Close the agent after this long without a turn; the session stays resumable. None keeps it open.
    pub idle_ttl: Option<Duration>,
}

/// A session's state as `status` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub agent: String,
    pub state: SessionState,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The running turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    pub queued: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ConfigOption>,
    pub events: PathBuf,
}

enum Ask {
    Send(String, oneshot::Sender<Result<u64>>),
    Cancel(oneshot::Sender<Result<()>>),
    Answer(String, String, oneshot::Sender<Result<()>>),
    Set(String, String, oneshot::Sender<Result<()>>),
    Status(oneshot::Sender<Status>),
    Close(oneshot::Sender<Result<()>>),
}

/// A session owned by this process. Dropping it leaves the owner running until [`Session::close`], the idle TTL or
/// the end of the process.
#[derive(Debug, Clone)]
pub struct Session {
    name: String,
    asks: mpsc::UnboundedSender<Ask>,
    events: broadcast::Sender<Event>,
    finished: tokio::sync::watch::Receiver<bool>,
}

impl Session {
    /// Starts a new session.
    pub async fn start(registry: Registry, table: &AgentTable, options: StartOptions) -> Result<Self> {
        let record = Self::prepare(&registry, table, &options)?;
        Self::own(registry, table, record, false, options.idle_ttl).await
    }

    /// Registers a new session without starting it; [`Session::open`] with `resume: false` starts it (the CLI does
    /// that in a `harnessmux serve` process).
    pub fn prepare(registry: &Registry, table: &AgentTable, options: &StartOptions) -> Result<SessionRecord> {
        let spec = table.get(&options.agent)?;
        let name = options.name.clone().unwrap_or_else(|| default_name(&options.agent));
        if let Ok(existing) = registry.load(&name)
            && existing.state != SessionState::Ended
        {
            return Err(Error::Invalid(format!("session {name} exists; use `harnessmux send {name}`, or close it first")));
        }
        let cwd = std::path::absolute(&options.cwd).map_err(|error| Error::io(format!("working directory {}", options.cwd.display()), error))?;
        if !cwd.is_dir() {
            return Err(Error::Invalid(format!("working directory {} does not exist", cwd.display())));
        }
        let record = SessionRecord::new(&name, &spec.name, &cwd, options.profile.clone())?;
        registry.remove(&name)?;
        registry.save(&record)?;
        Ok(record)
    }

    /// Takes over a session no process owns: the agent resumes it (`session/resume`, else `session/load`).
    pub async fn resume(registry: Registry, table: &AgentTable, name: &str, idle_ttl: Option<Duration>) -> Result<Self> {
        Self::open(registry, table, name, true, idle_ttl).await
    }

    /// Owns a registered session: a prepared one (`resume: false`) or one to resume.
    pub async fn open(registry: Registry, table: &AgentTable, name: &str, resume: bool, idle_ttl: Option<Duration>) -> Result<Self> {
        let record = registry.load(name)?;
        if record.state == SessionState::Ended {
            return Err(Error::Invalid(format!("session {name} was closed; start a new one with `harnessmux run`")));
        }
        Self::own(registry, table, record, resume, idle_ttl).await
    }

    async fn own(registry: Registry, table: &AgentTable, mut record: SessionRecord, resume: bool, idle_ttl: Option<Duration>) -> Result<Self> {
        let spec = table.get(&record.agent)?.clone();
        let mut listener = ipc::listen(&registry, &record.name).await?;
        record.owner_pid = Some(std::process::id());
        record.state = SessionState::Idle;
        registry.save(&record)?;

        let (asks_tx, asks_rx) = mpsc::unbounded_channel();
        let (events_tx, _) = broadcast::channel(1024);
        let (finished_tx, finished_rx) = tokio::sync::watch::channel(false);
        let session = Self { name: record.name.clone(), asks: asks_tx.clone(), events: events_tx.clone(), finished: finished_rx };

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (report_tx, mut report_rx) = mpsc::unbounded_channel();
        let start = backend::Start { spec, record: record.clone(), resume };
        let mut driver = tokio::spawn(backend::run(start, command_rx, Reporter::new(report_tx)));

        let mut owner = Owner {
            log: EventLog::open(&registry, &record)?,
            registry,
            record,
            events: events_tx,
            commands: command_tx,
            running: None,
            queue: VecDeque::new(),
            waiting: 0,
            options: Vec::new(),
            idle_ttl,
            closing: Vec::new(),
        };
        // Until the agent runs with the profile applied, nobody is served: a start error goes to the caller.
        loop {
            tokio::select! {
                Some(report) = report_rx.recv() => {
                    if matches!(report, Report::Ready) {
                        break;
                    }
                    owner.on_report(report);
                }
                result = &mut driver => {
                    while let Ok(report) = report_rx.try_recv() {
                        owner.on_report(report);
                    }
                    let error = match result {
                        Ok(Ok(())) => Error::Agent(format!("{} stopped before the session started", owner.record.agent)),
                        Ok(Err(error)) => error,
                        Err(error) => Error::Agent(format!("the driver stopped: {error}")),
                    };
                    owner.emit(None, EventBody::SessionEnded { reason: error.to_string() }, None);
                    owner.record.owner_pid = None;
                    owner.save();
                    drop(listener);
                    if !cfg!(windows) {
                        let _ = std::fs::remove_file(owner.registry.socket_name(&owner.record.name));
                    }
                    return Err(error);
                }
            }
        }
        let server_asks = asks_tx;
        let server = tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                let asks = server_asks.clone();
                tokio::spawn(ipc::serve_one(stream, move |request| answer(asks, request)));
            }
        });
        tokio::spawn(async move {
            owner.run(asks_rx, report_rx, driver).await;
            server.abort();
            let _ = finished_tx.send(true);
        });
        Ok(session)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Ask) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.asks.send(make(tx)).map_err(|_| Error::Invalid(format!("session {} has ended", self.name)))?;
        rx.await.map_err(|_| Error::Invalid(format!("session {} has ended", self.name)))
    }

    /// Queues a prompt; returns its turn number.
    pub async fn send(&self, prompt: impl Into<String>) -> Result<u64> {
        let prompt = prompt.into();
        self.ask(|tx| Ask::Send(prompt, tx)).await?
    }

    pub async fn cancel(&self) -> Result<()> {
        self.ask(Ask::Cancel).await?
    }

    /// Answers a permission request (`permission.requested`) with one of its option ids.
    pub async fn answer(&self, request: impl Into<String>, option: impl Into<String>) -> Result<()> {
        let (request, option) = (request.into(), option.into());
        self.ask(|tx| Ask::Answer(request, option, tx)).await?
    }

    /// Sets one of the agent's config options (`capabilities` lists them).
    pub async fn set(&self, option: impl Into<String>, value: impl Into<String>) -> Result<()> {
        let (option, value) = (option.into(), value.into());
        self.ask(|tx| Ask::Set(option, value, tx)).await?
    }

    pub async fn status(&self) -> Result<Status> {
        self.ask(Ask::Status).await
    }

    /// Ends the session for good: the agent stops, `send` and `resume` refuse it.
    pub async fn close(&self) -> Result<()> {
        self.ask(Ask::Close).await??;
        self.wait_finished().await;
        Ok(())
    }

    /// Events from now on. The log file has them all ([`crate::follow`]).
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Waits until the owner has stopped (closed, idle TTL, or the agent went away).
    pub async fn wait_finished(&self) {
        let mut finished = self.finished.clone();
        let _ = finished.wait_for(|done| *done).await;
    }
}

fn default_name(agent: &str) -> String {
    let ts = event::now();
    // 2026-11-13T22:45:12.815Z -> 20261113-224512
    let digits: String = ts.chars().take(19).filter(char::is_ascii_digit).collect();
    format!("{agent}-{}-{}", &digits[..8.min(digits.len())], &digits[8.min(digits.len())..])
}

async fn answer(asks: mpsc::UnboundedSender<Ask>, request: Request) -> Reply {
    async fn ask<T>(asks: &mpsc::UnboundedSender<Ask>, make: impl FnOnce(oneshot::Sender<T>) -> Ask) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        asks.send(make(tx)).map_err(|_| Error::Invalid("the session has ended".into()))?;
        rx.await.map_err(|_| Error::Invalid("the session has ended".into()))
    }
    match request {
        Request::Send { prompt } => Reply::from_result(ask(&asks, |tx| Ask::Send(prompt, tx)).await.and_then(|r| r), |reply, turn| reply.turn = Some(turn)),
        Request::Cancel => Reply::from_result(ask(&asks, Ask::Cancel).await.and_then(|r| r), |_, ()| {}),
        Request::Answer { request, option } => Reply::from_result(ask(&asks, |tx| Ask::Answer(request, option, tx)).await.and_then(|r| r), |_, ()| {}),
        Request::Set { option, value } => Reply::from_result(ask(&asks, |tx| Ask::Set(option, value, tx)).await.and_then(|r| r), |_, ()| {}),
        Request::Status => Reply::from_result(ask(&asks, Ask::Status).await, |reply, status| reply.status = Some(status)),
        Request::Close => Reply::from_result(ask(&asks, Ask::Close).await.and_then(|r| r), |_, ()| {}),
    }
}

/// The session's append-only JSONL log.
struct EventLog {
    file: std::fs::File,
    path: PathBuf,
}

impl EventLog {
    fn open(registry: &Registry, record: &SessionRecord) -> Result<Self> {
        let path = registry.events_path(&record.name);
        let file =
            std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|error| Error::io(format!("opening {}", path.display()), error))?;
        Ok(Self { file, path })
    }

    fn append(&mut self, event: &Event) {
        if let Ok(mut line) = serde_json::to_string(event) {
            line.push('\n');
            if let Err(error) = self.file.write_all(line.as_bytes()) {
                eprintln!("harnessmux: writing {}: {error}", self.path.display());
            }
        }
    }
}

struct Owner {
    registry: Registry,
    record: SessionRecord,
    log: EventLog,
    events: broadcast::Sender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    running: Option<u64>,
    queue: VecDeque<(u64, String)>,
    /// Open permission requests.
    waiting: usize,
    options: Vec<ConfigOption>,
    idle_ttl: Option<Duration>,
    closing: Vec<oneshot::Sender<Result<()>>>,
}

impl Owner {
    async fn run(
        mut self,
        mut asks: mpsc::UnboundedReceiver<Ask>,
        mut reports: mpsc::UnboundedReceiver<Report>,
        mut driver: tokio::task::JoinHandle<Result<()>>,
    ) {
        let far = Duration::from_secs(365 * 24 * 3600);
        let outcome: std::result::Result<(), String> = loop {
            let idle = self.running.is_none() && self.queue.is_empty() && self.waiting == 0 && self.closing.is_empty();
            let ttl = self.idle_ttl.filter(|_| idle).unwrap_or(far);
            tokio::select! {
                Some(ask) = asks.recv() => self.on_ask(ask),
                Some(report) = reports.recv() => self.on_report(report),
                result = &mut driver => {
                    // Reports sent before the driver ended come first.
                    while let Ok(report) = reports.try_recv() {
                        self.on_report(report);
                    }
                    break match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(error) => Err(format!("the driver stopped: {error}")),
                    };
                }
                () = tokio::time::sleep(ttl), if self.idle_ttl.is_some() && idle => {
                    let _ = self.commands.send(Command::Close);
                    self.emit(None, EventBody::Notice { text: format!("idle for {}s; the agent stops and the session stays resumable", ttl.as_secs()) }, None);
                    self.idle_ttl = None;
                }
            }
        };
        let closed = !self.closing.is_empty();
        if let Err(error) = &outcome
            && let Some(turn) = self.running.take()
        {
            self.emit(Some(turn), EventBody::TurnFailed { error: error.clone() }, None);
        }
        let reason = match (&outcome, closed) {
            (_, true) => "closed".to_string(),
            (Ok(()), false) => "idle".to_string(),
            (Err(error), false) => error.clone(),
        };
        self.emit(None, EventBody::SessionEnded { reason }, None);
        self.record.state = if closed { SessionState::Ended } else { SessionState::Idle };
        self.record.owner_pid = None;
        self.save();
        if !cfg!(windows) {
            let _ = std::fs::remove_file(self.registry.socket_name(&self.record.name));
        }
        for reply in self.closing.drain(..) {
            let _ = reply.send(Ok(()));
        }
        // Prompts nobody will run.
        while let Ok(ask) = asks.try_recv() {
            if let Ask::Send(_, reply) = ask {
                let _ = reply.send(Err(Error::Invalid(format!("session {} has ended", self.record.name))));
            }
        }
    }

    fn save(&self) {
        if let Err(error) = self.registry.save(&self.record) {
            eprintln!("harnessmux: {error}");
        }
    }

    fn emit(&mut self, turn: Option<u64>, body: EventBody, raw: Option<serde_json::Value>) {
        self.record.events += 1;
        let event = Event {
            v: EVENT_VERSION,
            ts: event::now(),
            session: self.record.name.clone(),
            project: self.record.project_name(),
            seq: self.record.events,
            turn,
            body,
            raw,
        };
        self.log.append(&event);
        let _ = self.events.send(event);
    }

    fn set_state(&mut self) {
        let state = if self.waiting > 0 {
            SessionState::Waiting
        } else if self.running.is_some() {
            SessionState::Running
        } else {
            SessionState::Idle
        };
        if state != self.record.state {
            self.record.state = state;
        }
        self.record.last_used = event::now();
        self.save();
    }

    fn next_turn(&mut self) {
        if self.running.is_some() || !self.closing.is_empty() {
            return;
        }
        if let Some((turn, text)) = self.queue.pop_front() {
            self.running = Some(turn);
            let _ = self.commands.send(Command::Prompt { turn, text });
        }
        self.set_state();
    }

    fn status(&self) -> Status {
        Status {
            name: self.record.name.clone(),
            agent: self.record.agent.clone(),
            state: self.record.state,
            cwd: self.record.cwd.clone(),
            project: self.record.project.clone(),
            agent_session: self.record.agent_session.clone(),
            adapter: self.record.adapter.clone(),
            turn: self.running,
            queued: self.queue.len(),
            owner_pid: self.record.owner_pid,
            options: self.options.clone(),
            events: self.registry.events_path(&self.record.name),
        }
    }

    fn on_ask(&mut self, ask: Ask) {
        let closed = || Err(Error::Invalid("the session is closing".into()));
        match ask {
            Ask::Send(text, reply) => {
                if !self.closing.is_empty() {
                    let _ = reply.send(closed());
                    return;
                }
                self.record.turns += 1;
                let turn = self.record.turns;
                if let Some(running) = self.running {
                    self.emit(Some(turn), EventBody::Notice { text: format!("queued behind turn {running}") }, None);
                }
                self.queue.push_back((turn, text));
                let _ = reply.send(Ok(turn));
                self.next_turn();
            }
            Ask::Cancel(reply) => {
                self.queue.clear();
                let _ = self.commands.send(Command::Cancel);
                let _ = reply.send(Ok(()));
            }
            Ask::Answer(request, option, reply) => {
                let _ = self.commands.send(Command::Answer { request, option, reply });
            }
            Ask::Set(option, value, reply) => {
                let _ = self.commands.send(Command::Set { option, value, reply });
            }
            Ask::Status(reply) => {
                let _ = reply.send(self.status());
            }
            Ask::Close(reply) => {
                self.queue.clear();
                if self.running.is_some() {
                    let _ = self.commands.send(Command::Cancel);
                }
                let _ = self.commands.send(Command::Close);
                self.closing.push(reply);
            }
        }
    }

    fn on_report(&mut self, report: Report) {
        match report {
            Report::Options(options) => self.options = options,
            Report::Ready => {}
            Report::AgentSession { id, adapter } => {
                self.record.agent_session = Some(id);
                if adapter.is_some() {
                    self.record.adapter = adapter;
                }
                self.save();
            }
            Report::Event { turn, body, raw } => {
                match &body {
                    EventBody::PermissionRequested { .. } => self.waiting += 1,
                    EventBody::PermissionAnswered { .. } => self.waiting = self.waiting.saturating_sub(1),
                    _ => {}
                }
                let ends = body.ends_turn();
                let changes_state = ends || matches!(body, EventBody::PermissionRequested { .. } | EventBody::PermissionAnswered { .. });
                self.emit(turn, body, raw);
                if ends && turn.is_some() && turn == self.running {
                    self.running = None;
                    self.waiting = 0;
                    self.next_turn();
                } else if changes_state {
                    self.set_state();
                }
            }
        }
    }
}
