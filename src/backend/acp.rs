//! The ACP driver: starts the agent's ACP server, creates or resumes the session, applies the profile and runs turns.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest, McpServer as AcpMcpServer, NewSessionRequest,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionId, SessionNotification, SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent, ToolKind,
};
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, LineDirection};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Command, Report, Reporter, Start};
use crate::acp_map::{self, ConfigOption};
use crate::agents::AgentSpec;
use crate::error::{Error, Result};
use crate::event::{EventBody, Failure, OUTPUT_LIMIT, PermissionOption, StopReason};
use crate::profile::{McpServer, PermissionPolicy, Profile};
use crate::replay::Recorder;

type PromptFuture = Pin<Box<dyn Future<Output = std::result::Result<PromptResponse, agent_client_protocol::Error>> + Send>>;

/// The last lines the agent wrote to stderr, for error messages when it dies.
#[derive(Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: &str) {
        if let Ok(mut lines) = self.0.lock() {
            if lines.len() == 20 {
                lines.pop_front();
            }
            lines.push_back(line.to_string());
        }
    }

    fn text(&self) -> String {
        self.0.lock().map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }
}

pub async fn run(start: Start, commands: mpsc::UnboundedReceiver<Command>, reporter: Reporter) -> Result<()> {
    let (program, args) = start.spec.launch_command().await?;
    let env = start.spec.env.iter().chain(&start.record.profile.env).map(|(k, v)| (k.clone(), v.clone()));
    let stderr = StderrTail::default();
    let tail = stderr.clone();
    let recorder = match std::env::var_os("HARNESSMUX_RECORD").filter(|path| !path.is_empty()) {
        Some(path) => Some(Recorder::create(std::path::Path::new(&path))?),
        None => None,
    };
    let agent = AcpAgent::new(AcpAgentConfig::new(&program).args(args).envs(env)).with_debug(move |line, direction| match direction {
        LineDirection::Stderr => tail.push(line),
        LineDirection::Stdin => recorder.iter().for_each(|r| r.line(true, line)),
        LineDirection::Stdout => recorder.iter().for_each(|r| r.line(false, line)),
    });

    let (notify_tx, notify_rx) = mpsc::unbounded_channel::<Value>();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel::<(RequestPermissionRequest, Responder<RequestPermissionResponse>)>();
    let driver = Driver { start, reporter: reporter.clone(), commands, notify_rx, permission_rx };
    let driver_error: Arc<Mutex<Option<Error>>> = Arc::default();
    let own_error = driver_error.clone();
    let result = Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let Ok(value) = serde_json::to_value(&notification) {
                    let _ = notify_tx.send(value);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                // The driver answers it; held here, the dispatch loop would stop until then.
                let _ = permission_tx.send((request, responder));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |cx: ConnectionTo<Agent>| {
            driver.drive(cx).await.map_err(|error| {
                let text = error.to_string();
                if let Ok(mut slot) = own_error.lock() {
                    *slot = Some(error);
                }
                agent_client_protocol::Error::internal_error().data(text)
            })
        })
        .await;
    // harnessmux's own errors (a model the agent doesn't list) come back as they are, without the agent's stderr.
    if let Some(error) = driver_error.lock().ok().and_then(|mut slot| slot.take()) {
        return Err(error);
    }
    result.map_err(|error| {
        let tail = stderr.text();
        let detail = describe(&error);
        if tail.is_empty() {
            Error::Agent(format!("{}: {detail}", program.display()))
        } else {
            Error::Agent(format!("{}: {detail}\n{tail}", program.display()))
        }
    })
}

/// The SDK error's message plus its data (where the driver's own errors go).
fn describe(error: &agent_client_protocol::Error) -> String {
    match serde_json::to_value(error).ok().and_then(|value| value.get("data").and_then(Value::as_str).map(str::to_string)) {
        Some(data) => data,
        None => error.to_string(),
    }
}

/// A failed ACP request as a `turn.failed` and its raw JSON-RPC error.
fn turn_failed(error: &agent_client_protocol::Error) -> (EventBody, Option<Value>) {
    match serde_json::to_value(error) {
        Ok(raw) => (Failure::from_acp_error(&raw).into(), Some(raw)),
        Err(_) => (EventBody::failed(error.to_string()), None),
    }
}

struct Driver {
    start: Start,
    reporter: Reporter,
    commands: mpsc::UnboundedReceiver<Command>,
    notify_rx: mpsc::UnboundedReceiver<Value>,
    permission_rx: mpsc::UnboundedReceiver<(RequestPermissionRequest, Responder<RequestPermissionResponse>)>,
}

struct Pending {
    responder: Responder<RequestPermissionResponse>,
    options: Vec<PermissionOption>,
    turn: Option<u64>,
}

/// What a running turn has seen so far.
struct Turn {
    id: u64,
    reply: String,
    /// The latest message chunk: codex-acp sends an error as a chunk of its own.
    last_chunk: String,
    ran_tools: bool,
    /// Each running tool's terminal output so far (codex-acp sends it in chunks, not as the result), at most
    /// [`OUTPUT_LIMIT`] bytes, and how many bytes more it had.
    terminal: HashMap<String, (String, usize)>,
    future: PromptFuture,
}

impl Driver {
    async fn drive(mut self, cx: ConnectionTo<Agent>) -> Result<()> {
        let init: InitializeResponse = cx
            .send_request(InitializeRequest::new(ProtocolVersion::V1).client_info(Implementation::new("harnessmux", env!("CARGO_PKG_VERSION"))))
            .block_task()
            .await?;
        let (session_id, mut options) = self.open_session(&cx, &init).await?;
        let profile = self.start.record.profile.clone();
        self.apply_profile(&cx, &session_id, &mut options, &profile).await?;
        self.reporter.report(Report::Options(options.clone()));
        self.reporter.report(Report::Ready);

        let mut turn: Option<Turn> = None;
        let mut queued: VecDeque<(u64, String)> = VecDeque::new();
        let mut pending: BTreeMap<String, Pending> = BTreeMap::new();
        let mut next_request = 0_u64;
        loop {
            if turn.is_none()
                && let Some((id, text)) = queued.pop_front()
            {
                turn = Some(self.begin_turn(&cx, &session_id, id, text));
            }
            tokio::select! {
                biased;
                Some(notification) = self.notify_rx.recv() => self.on_notification(&notification, turn.as_mut(), &mut options),
                command = self.commands.recv() => match command {
                    None | Some(Command::Close) => {
                        for (_, request) in std::mem::take(&mut pending) {
                            let _ = request.responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
                        }
                        return Ok(());
                    }
                    Some(Command::Prompt { turn: id, text }) => queued.push_back((id, text)),
                    Some(Command::Cancel) => {
                        if turn.is_some() {
                            cx.send_notification(CancelNotification::new(session_id.clone()))?;
                            // ACP: the client answers open permission requests with cancelled.
                            for (id, request) in std::mem::take(&mut pending) {
                                let _ = request.responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
                                self.reporter.event(request.turn, EventBody::PermissionAnswered { request: id, option: None, by: "cancel".into() }, None);
                            }
                        }
                    }
                    Some(Command::Answer { request, option, reply }) => {
                        let result = match pending.remove(&request) {
                            None => Err(Error::Invalid(format!("no open permission request {request}; open: {}", pending.keys().cloned().collect::<Vec<_>>().join(", ")))),
                            Some(open) if !open.options.iter().any(|o| o.id == option) => {
                                let ids = open.options.iter().map(|o| o.id.clone()).collect::<Vec<_>>().join(", ");
                                pending.insert(request.clone(), open);
                                Err(Error::Invalid(format!("{option} is not an option of request {request}; options: {ids}")))
                            }
                            Some(open) => {
                                let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.clone()));
                                let sent = open.responder.respond(RequestPermissionResponse::new(outcome)).map_err(Error::from);
                                self.reporter.event(open.turn, EventBody::PermissionAnswered { request, option: Some(option), by: "caller".into() }, None);
                                sent
                            }
                        };
                        let _ = reply.send(result);
                    }
                    Some(Command::Set { option, value, reply }) => {
                        let result = self.set_option(&cx, &session_id, &mut options, &option, &value).await;
                        if result.is_ok() {
                            self.reporter.event(turn.as_ref().map(|t| t.id), EventBody::ConfigChanged { option, value }, None);
                            self.reporter.report(Report::Options(options.clone()));
                        }
                        let _ = reply.send(result);
                    }
                },
                Some((request, responder)) = self.permission_rx.recv() => {
                    next_request += 1;
                    let id = format!("p{next_request}");
                    let turn_id = turn.as_ref().map(|t| t.id);
                    if let Some(open) = self.decide(id.clone(), turn_id, request, responder) {
                        pending.insert(id, open);
                    }
                }
                done = async { turn.as_mut().map(|t| &mut t.future).expect("guarded by the select precondition").await }, if turn.is_some() => {
                    let mut finished = turn.take().expect("the branch runs only with a turn");
                    // The SDK hands notifications to the channel before it resolves the prompt that follows them on
                    // the wire; those still in the channel belong to this turn.
                    while let Ok(notification) = self.notify_rx.try_recv() {
                        self.on_notification(&notification, Some(&mut finished), &mut options);
                    }
                    pending.retain(|_, open| open.turn != Some(finished.id));
                    self.end_turn(finished, done);
                }
            }
        }
    }

    fn on_notification(&self, notification: &Value, mut turn: Option<&mut Turn>, options: &mut Vec<ConfigOption>) {
        let turn_id = turn.as_ref().map(|t| t.id);
        let update = notification.get("update").cloned().unwrap_or(Value::Null);
        if let (Some(current), Some(chunk)) = (turn.as_deref_mut(), acp_map::terminal_output(&update)) {
            let id = update.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_string();
            let (text, dropped) = current.terminal.entry(id).or_default();
            let mut room = OUTPUT_LIMIT.saturating_sub(text.len()).min(chunk.len());
            while !chunk.is_char_boundary(room) {
                room -= 1;
            }
            text.push_str(&chunk[..room]);
            *dropped += chunk.len() - room;
        }
        for mut body in acp_map::map_update(&update) {
            if let Some(current) = turn.as_deref_mut() {
                match &mut body {
                    EventBody::MessageDelta { text } => {
                        current.reply.push_str(text);
                        current.last_chunk.clone_from(text);
                    }
                    EventBody::ToolStarted { .. } => current.ran_tools = true,
                    EventBody::ToolFinished { id, output, .. } => {
                        if let Some((text, dropped)) = current.terminal.remove(id.as_str())
                            && output.is_none()
                            && !text.trim().is_empty()
                        {
                            *output = Some(acp_map::cap_output(&text, dropped));
                        }
                    }
                    _ => {}
                }
            }
            self.reporter.event(turn_id, body, Some(notification.clone()));
        }
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("config_option_update") {
            *options = acp_map::config_options(&update);
            self.reporter.report(Report::Options(options.clone()));
        }
    }

    async fn open_session(&self, cx: &ConnectionTo<Agent>, init: &InitializeResponse) -> Result<(SessionId, Vec<ConfigOption>)> {
        let record = &self.start.record;
        let cwd = record.cwd.clone();
        let add_dirs = record.profile.add_dirs.clone();
        let mcp = acp_mcp_servers(&record.profile)?;
        let meta = self.start.spec.session_meta(record.profile.permissions, &record.profile.rules);
        let caps = &init.agent_capabilities;
        let adapter = self.start.spec.adapter.as_ref().map(|a| format!("{}@{}", a.npm, a.version));
        let agent_name = init.agent_info.as_ref().map(|info| format!("{} {}", info.name, info.version));
        let adapter_label = adapter.clone().or(agent_name);

        if self.start.resume
            && let Some(previous) = record.agent_session.clone()
        {
            if caps.session_capabilities.resume.is_some() {
                let response = cx
                    .send_request(
                        ResumeSessionRequest::new(previous.clone(), cwd.clone())
                            .additional_directories(add_dirs.clone())
                            .mcp_servers(mcp.clone())
                            .meta(meta.clone()),
                    )
                    .block_task()
                    .await?;
                return Ok(self.started(previous, "resumed", adapter_label, &json_of(&response)));
            }
            if caps.load_session {
                let response = cx
                    .send_request(
                        LoadSessionRequest::new(previous.clone(), cwd.clone())
                            .additional_directories(add_dirs.clone())
                            .mcp_servers(mcp.clone())
                            .meta(meta.clone()),
                    )
                    .block_task()
                    .await?;
                // session/load replays the history as updates; the log already has it.
                return Ok(self.started(previous, "loaded", adapter_label, &json_of(&response)));
            }
            self.reporter
                .notice(None, format!("{} can neither resume nor load sessions; this is a new session without the earlier turns", self.start.spec.name));
        }
        let response = cx.send_request(NewSessionRequest::new(cwd).additional_directories(add_dirs).mcp_servers(mcp).meta(meta)).block_task().await?;
        let id = response.session_id.to_string();
        Ok(self.started(id, "new", adapter_label, &json_of(&response)))
    }

    fn started(&self, id: String, how: &str, adapter: Option<String>, response: &Value) -> (SessionId, Vec<ConfigOption>) {
        let record = &self.start.record;
        self.reporter.report(Report::AgentSession { id: id.clone(), adapter: adapter.clone() });
        self.reporter.event(
            None,
            EventBody::SessionStarted {
                agent: self.start.spec.name.clone(),
                cwd: record.cwd.display().to_string(),
                agent_session: Some(id.clone()),
                how: how.into(),
                adapter,
                link: None,
            },
            Some(response.clone()),
        );
        (SessionId::new(id), acp_map::config_options(response))
    }

    /// Model, effort, mode and other options from the profile. A model the agent doesn't list is an error; an effort
    /// or mode it doesn't offer is a notice.
    async fn apply_profile(&self, cx: &ConnectionTo<Agent>, session: &SessionId, options: &mut Vec<ConfigOption>, profile: &Profile) -> Result<()> {
        let spec = &self.start.spec;
        if let Some(model) = &profile.model {
            match acp_map::find_option(options, spec.options.model.as_deref(), "model").map(|o| o.id.clone()) {
                Some(id) => self.set_option(cx, session, options, &id, model).await?,
                None => self.reporter.notice(None, format!("{} offers no model option; the model {model} is not applied", spec.name)),
            }
        }
        if let Some(effort) = profile.effort {
            let option = acp_map::find_option(options, spec.options.effort.as_deref(), "thought_level").map(|o| o.id.clone());
            match (option, spec.effort_value(effort)) {
                (Some(id), Some(value)) => {
                    let value = value.to_string();
                    if let Err(error) = self.set_option(cx, session, options, &id, &value).await {
                        self.reporter.notice(None, format!("effort {effort} not applied: {error}"));
                    }
                }
                _ => self.reporter.notice(None, format!("{} offers no effort setting; effort {effort} is not applied", spec.name)),
            }
        }
        let mode = spec.mode_value(profile.permissions).map(str::to_string);
        let option = acp_map::find_option(options, spec.options.mode.as_deref(), "mode").map(|o| o.id.clone());
        match (option, mode) {
            (Some(id), Some(value)) => {
                if let Err(error) = self.set_option(cx, session, options, &id, &value).await {
                    self.reporter.notice(None, format!("permissions {} (mode {value}) not applied: {error}", profile.permissions));
                }
            }
            (_, None) => self.reporter.notice(
                None,
                format!("{} has no mode for {} permissions; permission requests follow the {} policy", spec.name, profile.permissions, profile.policy()),
            ),
            (None, Some(value)) => self.reporter.notice(None, format!("{} offers no mode option; mode {value} is not applied", spec.name)),
        }
        for (id, value) in &profile.options {
            self.set_option(cx, session, options, id, value).await?;
        }
        Ok(())
    }

    async fn set_option(&self, cx: &ConnectionTo<Agent>, session: &SessionId, options: &mut Vec<ConfigOption>, id: &str, value: &str) -> Result<()> {
        let names = || options.iter().map(|o| o.id.clone()).collect::<Vec<_>>().join(", ");
        let option =
            options.iter().find(|o| o.id == id).ok_or_else(|| Error::Invalid(format!("{} has no option {id}; options: {}", self.start.spec.name, names())))?;
        if !option.allows(value) {
            let allowed = if option.is_boolean() { "true, false".to_string() } else { option.values.join(", ") };
            return Err(Error::Invalid(format!("{value} is not a value of {} option {id}; values: {allowed}", self.start.spec.name)));
        }
        if option.legacy_mode {
            cx.send_request(SetSessionModeRequest::new(session.clone(), value.to_string())).block_task().await?;
            if let Some(mode) = options.iter_mut().find(|o| o.id == id) {
                mode.current = value.to_string();
            }
            return Ok(());
        }
        let request = if option.is_boolean() {
            SetSessionConfigOptionRequest::new(session.clone(), id.to_string(), value == "true")
        } else {
            SetSessionConfigOptionRequest::new(session.clone(), id.to_string(), value)
        };
        let response = cx.send_request(request).block_task().await?;
        let legacy: Vec<ConfigOption> = options.iter().filter(|o| o.legacy_mode).cloned().collect();
        *options = acp_map::config_options(&json!({ "configOptions": json_of(&response).get("configOptions").cloned().unwrap_or(Value::Null) }));
        options.extend(legacy);
        Ok(())
    }

    fn begin_turn(&self, cx: &ConnectionTo<Agent>, session: &SessionId, id: u64, text: String) -> Turn {
        self.reporter.event(Some(id), EventBody::TurnStarted { prompt: text.clone() }, None);
        let cx = cx.clone();
        let request = PromptRequest::new(session.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
        let future: PromptFuture = Box::pin(async move { cx.send_request(request).block_task().await });
        Turn { id, reply: String::new(), last_chunk: String::new(), ran_tools: false, terminal: HashMap::new(), future }
    }

    fn end_turn(&self, turn: Turn, done: std::result::Result<PromptResponse, agent_client_protocol::Error>) {
        let id = Some(turn.id);
        if !turn.reply.is_empty() {
            self.reporter.event(id, EventBody::Message { text: turn.reply.clone() }, None);
        }
        match done {
            Err(error) => {
                let (body, raw) = turn_failed(&error);
                self.reporter.event(id, body, raw);
            }
            Ok(response) => {
                let raw = json_of(&response);
                let stop_reason: StopReason = raw.get("stopReason").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or(StopReason::EndTurn);
                match acp_map::failure_in_reply(&turn.reply, &turn.last_chunk, turn.ran_tools) {
                    Some(error) if stop_reason != StopReason::Cancelled => self.reporter.event(id, EventBody::failed(error), Some(raw)),
                    _ => self.reporter.event(id, EventBody::TurnFinished { stop_reason, usage: acp_map::prompt_usage(&raw) }, Some(raw)),
                }
            }
        }
    }

    /// Answers a permission request by the policy, or keeps it open for `answer` (policy ask).
    fn decide(&self, id: String, turn: Option<u64>, request: RequestPermissionRequest, responder: Responder<RequestPermissionResponse>) -> Option<Pending> {
        let options: Vec<PermissionOption> = request
            .options
            .iter()
            .map(|o| PermissionOption {
                id: o.option_id.to_string(),
                name: o.name.clone(),
                kind: serde_json::to_value(o.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            })
            .collect();
        let title = request.tool_call.fields.title.clone().unwrap_or_else(|| "tool call".into());
        let raw = serde_json::to_value(&request).ok();
        self.reporter.event(turn, EventBody::PermissionRequested { request: id.clone(), title, options: options.clone() }, raw.clone());
        let record = &self.start.record;
        let policy = record.profile.policy();
        let roots: Vec<PathBuf> = std::iter::once(record.cwd.clone()).chain(record.profile.add_dirs.iter().cloned()).collect();
        let reads = matches!(request.tool_call.fields.kind, Some(ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Fetch));
        let (allow, by) = match raw.as_ref().and_then(|raw| record.profile.rules.decide(raw, &roots)) {
            Some(decided) => decided,
            None => match policy {
                PermissionPolicy::Ask => return Some(Pending { responder, options, turn }),
                PermissionPolicy::Allow => (true, format!("policy {policy}")),
                PermissionPolicy::AllowReads => (reads, format!("policy {policy}")),
                PermissionPolicy::Deny => (false, format!("policy {policy}")),
            },
        };
        let kinds: &[PermissionOptionKind] = if allow {
            &[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]
        } else {
            &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
        };
        let preferred = if allow {
            None
        } else {
            self.start.spec.reject.iter().find_map(|id| request.options.iter().find(|o| kinds.contains(&o.kind) && o.option_id.0.as_ref() == id))
        };
        let chosen = preferred.or_else(|| kinds.iter().find_map(|kind| request.options.iter().find(|o| o.kind == *kind))).map(|o| o.option_id.to_string());
        let outcome = match &chosen {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.clone())),
            None => RequestPermissionOutcome::Cancelled,
        };
        let _ = responder.respond(RequestPermissionResponse::new(outcome));
        self.reporter.event(turn, EventBody::PermissionAnswered { request: id, option: chosen, by }, None);
        None
    }
}

fn json_of(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The profile's MCP servers in ACP's shape (`env` and `headers` as name/value lists).
fn acp_mcp_servers(profile: &Profile) -> Result<Vec<AcpMcpServer>> {
    let pairs = |map: &BTreeMap<String, String>| map.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>();
    profile
        .mcp_servers
        .iter()
        .map(|(name, server)| {
            let value = match server {
                McpServer::Stdio { command, args, env } => json!({ "name": name, "command": command, "args": args, "env": pairs(env) }),
                McpServer::Http { url, headers } => json!({ "type": "http", "name": name, "url": url, "headers": pairs(headers) }),
            };
            serde_json::from_value(value).map_err(|error| Error::Invalid(format!("MCP server {name}: {error}")))
        })
        .collect()
}

/// For `capabilities`: start the agent, open a session in `cwd` and report what it offers.
pub async fn probe(spec: &AgentSpec, cwd: &std::path::Path) -> Result<Value> {
    let (program, args) = spec.launch_command().await?;
    let stderr = StderrTail::default();
    let tail = stderr.clone();
    let agent = AcpAgent::new(AcpAgentConfig::new(&program).args(args).envs(spec.env.clone())).with_debug(move |line, direction| {
        if matches!(direction, LineDirection::Stderr) {
            tail.push(line);
        }
    });
    let cwd = cwd.to_path_buf();
    let result = Client
        .v1()
        .connect_with(agent, async move |cx: ConnectionTo<Agent>| {
            let init: InitializeResponse = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1).client_info(Implementation::new("harnessmux", env!("CARGO_PKG_VERSION"))))
                .block_task()
                .await?;
            let session = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?;
            Ok((json_of(&init), json_of(&session)))
        })
        .await;
    let (init, session) = result.map_err(|error| Error::Agent(format!("{}: {error}\n{}", program.display(), stderr.text())))?;
    let caps = init.get("agentCapabilities").cloned().unwrap_or(Value::Null);
    let session_caps = caps.get("sessionCapabilities").cloned().unwrap_or(Value::Null);
    let has = |key: &str| session_caps.get(key).is_some_and(|v| !v.is_null());
    Ok(json!({
        "agent_info": init.get("agentInfo"),
        "resume": has("resume"),
        "load": caps.get("loadSession").and_then(Value::as_bool).unwrap_or(false),
        "list": has("list"),
        "fork": has("fork"),
        "close": has("close"),
        "additional_directories": has("additionalDirectories"),
        "mcp": caps.get("mcpCapabilities"),
        "prompt": caps.get("promptCapabilities"),
        "auth_methods": init.get("authMethods"),
        "options": acp_map::config_options(&session),
    }))
}
