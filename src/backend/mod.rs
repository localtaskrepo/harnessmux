//! The drivers behind a session: ACP for most agents, and the legacy Amp and T3 adapters. Each one takes commands
//! from the session's owner and reports events back to it.

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::acp_map::ConfigOption;
use crate::agents::{AgentKind, AgentSpec};
use crate::error::Result;
use crate::event::EventBody;
use crate::registry::SessionRecord;

pub mod acp;
pub mod amp;
pub mod t3;
mod t3_mcp;

/// What the owner asks a driver to do.
#[derive(Debug)]
pub enum Command {
    Prompt { turn: u64, text: String },
    Cancel,
    Answer { request: String, option: String, reply: oneshot::Sender<Result<()>> },
    Set { option: String, value: String, reply: oneshot::Sender<Result<()>> },
    Close,
}

/// What a driver tells the owner.
#[derive(Debug)]
pub enum Report {
    Event {
        turn: Option<u64>,
        body: EventBody,
        raw: Option<Value>,
    },
    /// The agent's config options as they are now.
    Options(Vec<ConfigOption>),
    /// The driver takes prompts now: the agent runs and the profile is applied.
    Ready,
    /// The agent's session id, once known.
    AgentSession {
        id: String,
        adapter: Option<String>,
    },
    /// The agent is asked to make its session now (`Some(time)`), or it made none (`None`); `saved` answers once the
    /// record holds it.
    AgentSessionAsked {
        since: Option<String>,
        saved: oneshot::Sender<()>,
    },
}

#[derive(Debug, Clone)]
pub struct Reporter(mpsc::UnboundedSender<Report>);

impl Reporter {
    pub fn new(tx: mpsc::UnboundedSender<Report>) -> Self {
        Self(tx)
    }

    pub fn event(&self, turn: Option<u64>, body: EventBody, raw: Option<Value>) {
        // The owner stops listening only when it shuts down, and then nothing reads events anyway.
        let _ = self.0.send(Report::Event { turn, body, raw });
    }

    pub fn notice(&self, turn: Option<u64>, text: impl Into<String>) {
        self.event(turn, EventBody::Notice { text: text.into() }, None);
    }

    pub fn report(&self, report: Report) {
        let _ = self.0.send(report);
    }
}

/// How a driver starts: a new session, or the record's agent session again.
#[derive(Debug, Clone)]
pub struct Start {
    pub spec: AgentSpec,
    pub record: SessionRecord,
    pub resume: bool,
    /// The previous owner stopped during a turn (the record was left running or waiting).
    pub interrupted: bool,
}

/// Runs the driver until the session closes or the agent goes away.
pub async fn run(start: Start, commands: mpsc::UnboundedReceiver<Command>, reporter: Reporter) -> Result<()> {
    match start.spec.kind {
        AgentKind::Acp => acp::run(start, commands, reporter).await,
        AgentKind::Amp => amp::run(start, commands, reporter).await,
        AgentKind::T3 => t3::run(start, commands, reporter).await,
    }
}

/// A command a driver can't carry out right now: its reply gets this error, other commands are ignored.
pub fn refuse(command: Command, why: &str) {
    match command {
        Command::Answer { reply, .. } | Command::Set { reply, .. } => {
            let _ = reply.send(Err(crate::error::Error::Invalid(why.to_string())));
        }
        Command::Prompt { .. } | Command::Cancel | Command::Close => {}
    }
}
