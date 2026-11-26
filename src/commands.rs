//! The CLI's commands, on top of the library.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use harnessmux::client::{self, DEFAULT_IDLE_TTL};
use harnessmux::follow::Follower;
use harnessmux::ipc::{self, Request};
use harnessmux::profile::parse_mcp_file;
use harnessmux::{
    AgentTable, Error, Event, EventBody, Profile, Registry, Result, Rules, Session, SessionRecord, SessionState, StartOptions, capabilities, event, mcp, replay,
};
use tokio::io::AsyncBufReadExt as _;

use super::{Command, ProjectArg, PromptArgs};

#[derive(Args)]
pub struct RunArgs {
    /// The agent: claude, codex, copilot, gemini, cursor, droid, opencode, kilo, antigravity, amp, t3 (see capabilities).
    agent: String,
    #[command(flatten)]
    prompt: PromptArgs,
    /// Session name (letters, digits, '.', '_', '-'); defaults to <agent>-<date>-<time>.
    #[arg(long)]
    name: Option<String>,
    /// Working directory; defaults to the current one.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// A model the agent lists (`status` and `capabilities <agent>` show them).
    #[arg(long)]
    model: Option<String>,
    /// low, medium, high, xhigh or max.
    #[arg(long)]
    effort: Option<String>,
    /// read-only, workspace (default) or full.
    #[arg(long)]
    permissions: Option<String>,
    /// What to do with the agent's permission requests: deny, allow-reads, allow, ask (wait for `answer`). Default:
    /// allow-reads for read-only, allow otherwise.
    #[arg(long)]
    on_permission: Option<String>,
    /// Refuse a shell command, e.g. "git push" (repeatable).
    #[arg(long = "deny-command", value_name = "COMMAND")]
    deny_commands: Vec<String>,
    /// Allow a tool by the agent's name for it without the policy; a trailing * matches a prefix (repeatable).
    #[arg(long = "allow-tool", value_name = "TOOL")]
    allow_tools: Vec<String>,
    /// Refuse a tool, matched like --allow-tool (repeatable).
    #[arg(long = "deny-tool", value_name = "TOOL")]
    deny_tools: Vec<String>,
    /// Refuse edits outside the working directory and --add-dir directories.
    #[arg(long)]
    confine_writes: bool,
    /// Another config option of the agent, as id=value (repeatable).
    #[arg(long = "option", value_name = "ID=VALUE")]
    options: Vec<String>,
    /// MCP servers for the session: a JSON file in the {"mcpServers": {...}} format.
    #[arg(long)]
    mcp: Option<PathBuf>,
    /// Another directory the agent may use (repeatable).
    #[arg(long = "add-dir")]
    add_dirs: Vec<PathBuf>,
    /// Environment for the agent, as NAME=VALUE (repeatable).
    #[arg(long = "env", value_name = "NAME=VALUE")]
    env: Vec<String>,
    /// t3: the T3 Code provider instance for --model (claudeAgent, codex).
    #[arg(long)]
    provider: Option<String>,
    /// t3: start the thread in a new git worktree based on this branch, tag or commit.
    #[arg(long)]
    worktree: Option<String>,
    /// t3: the branch for that worktree.
    #[arg(long)]
    branch: Option<String>,
    /// Session title (t3 thread title).
    #[arg(long)]
    title: Option<String>,
    /// t3: a JSON Schema file the turn's result must match; the result comes as a turn.output event.
    #[arg(long, value_name = "FILE")]
    output_schema: Option<PathBuf>,
    /// Print events as JSON lines.
    #[arg(long)]
    json: bool,
    /// Don't wait for the first turn; print the session name.
    #[arg(long)]
    no_wait: bool,
    /// Write every ACP message to this file (for `harnessmux replay` and tests).
    #[arg(long)]
    record: Option<PathBuf>,
    /// Stop the agent after this many seconds without a turn; the session stays resumable (0: never).
    #[arg(long, default_value_t = DEFAULT_IDLE_TTL.as_secs())]
    idle_ttl: u64,
}

fn pairs(raw: &[String], what: &str) -> Result<BTreeMap<String, String>> {
    raw.iter()
        .map(|item| {
            item.split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .ok_or_else(|| Error::Invalid(format!("{what} {item:?}: write it as name=value")))
        })
        .collect()
}

fn read_prompt(args: &PromptArgs) -> Result<Option<String>> {
    if let Some(file) = &args.file {
        return std::fs::read_to_string(file).map(Some).map_err(|error| Error::io(format!("reading {}", file.display()), error));
    }
    match args.prompt.as_deref() {
        Some("-") => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text).map_err(|error| Error::io("reading the prompt from stdin", error))?;
            Ok(Some(text))
        }
        other => Ok(other.map(str::to_string)),
    }
}

fn read_schema(file: &Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(file).map_err(|error| Error::io(format!("reading {}", file.display()), error))?;
    serde_json::from_str(&text).map_err(|error| Error::Invalid(format!("--output-schema {}: not JSON: {error}", file.display())))
}

fn profile(args: &RunArgs) -> Result<Profile> {
    let mut profile = Profile {
        model: args.model.clone(),
        effort: args.effort.as_deref().map(str::parse).transpose()?,
        permissions: args.permissions.as_deref().map(str::parse).transpose()?.unwrap_or_default(),
        on_permission: args.on_permission.as_deref().map(str::parse).transpose()?,
        rules: Rules {
            deny_commands: args.deny_commands.clone(),
            allow_tools: args.allow_tools.clone(),
            deny_tools: args.deny_tools.clone(),
            confine_writes: args.confine_writes,
        },
        options: pairs(&args.options, "--option")?,
        env: pairs(&args.env, "--env")?,
        provider: args.provider.clone(),
        worktree_base: args.worktree.clone(),
        branch: args.branch.clone(),
        title: args.title.clone(),
        output_schema: args.output_schema.as_deref().map(read_schema).transpose()?,
        ..Profile::default()
    };
    for dir in &args.add_dirs {
        profile.add_dirs.push(std::path::absolute(dir).map_err(|error| Error::io(format!("directory {}", dir.display()), error))?);
    }
    if let Some(file) = &args.mcp {
        let text = std::fs::read_to_string(file).map_err(|error| Error::io(format!("reading {}", file.display()), error))?;
        profile.mcp_servers = parse_mcp_file(&text)?;
    }
    Ok(profile)
}

fn ttl(seconds: u64) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

pub async fn dispatch(command: Command) -> Result<u8> {
    let registry = Registry::default();
    match command {
        Command::Run(args) => run(&registry, *args).await,
        Command::Send { name, prompt, wait, json } => {
            let prompt = read_prompt(&prompt)?.ok_or_else(|| Error::Invalid("send needs a prompt (an argument, - for stdin, or -f FILE)".into()))?;
            let mut follower = Follower::from_end(&client::events_path(&registry, &name)?);
            let turn = client::send(&registry, &name, &prompt).await?;
            if !wait {
                println!("{turn}");
                return Ok(0);
            }
            print_turn(&registry, &name, &mut follower, turn, json).await
        }
        Command::Attach { name, from_start, json, project } => attach(&registry, name, from_start, json, project).await,
        Command::Resume { name } => {
            client::ensure_owner(&registry, &name).await?;
            println!("{name}");
            Ok(0)
        }
        Command::Cancel { name } => simple(&registry, &name, Request::Cancel).await,
        Command::Answer { name, request, option } => simple(&registry, &name, Request::Answer { request, option }).await,
        Command::Set { name, option, value } => {
            client::ensure_owner(&registry, &name).await?;
            simple(&registry, &name, Request::Set { option, value }).await
        }
        Command::List { json, project } => list(&registry, json, project).await,
        Command::Status { name, json } => status(&registry, &name, json).await,
        Command::Close { name, forget } => close(&registry, &name, forget).await,
        Command::Settle { name } => {
            let record = registry.load(&name)?;
            let link = harnessmux::backend::t3::settle(AgentTable::load()?.get(&record.agent)?, &record).await?;
            println!("{link}");
            Ok(0)
        }
        Command::Capabilities { agent, json } => {
            let table = AgentTable::load()?;
            let value = match agent {
                Some(agent) => capabilities::probe(table.get(&agent)?).await?,
                None => serde_json::Value::Array(capabilities::all(&table)),
            };
            if json {
                println!("{value}");
            } else {
                print_capabilities(&value);
            }
            Ok(0)
        }
        Command::Mcp => {
            mcp::serve(registry, AgentTable::load()?).await?;
            Ok(0)
        }
        Command::Serve { name, new, idle_ttl } => {
            let session = Session::open(registry, &AgentTable::load()?, &name, !new, ttl(idle_ttl)).await?;
            session.wait_finished().await;
            Ok(0)
        }
        Command::Replay { file } => {
            replay::serve(replay::load(&file)?).await?;
            Ok(0)
        }
    }
}

async fn run(registry: &Registry, args: RunArgs) -> Result<u8> {
    let table = AgentTable::load()?;
    let prompt = read_prompt(&args.prompt)?;
    let cwd = match &args.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().map_err(|error| Error::io("current directory", error))?,
    };
    let options = StartOptions { name: args.name.clone(), agent: args.agent.clone(), cwd, profile: profile(&args)?, idle_ttl: ttl(args.idle_ttl) };
    let record = Session::prepare(registry, &table, &options)?;
    let name = record.name.clone();
    let mut follower = Follower::from_start(&registry.events_path(&name));
    let env: Vec<(String, String)> = match &args.record {
        Some(path) => vec![("HARNESSMUX_RECORD".into(), std::path::absolute(path).map_err(|error| Error::io("--record", error))?.display().to_string())],
        None => Vec::new(),
    };
    if let Err(error) = client::spawn_owner_with(registry, &name, true, Duration::from_secs(args.idle_ttl), &env).await {
        let _ = registry.remove(&name);
        return Err(error);
    }
    let Some(prompt) = prompt else {
        println!("{name}");
        return Ok(0);
    };
    let turn = client::request(registry, &name, &Request::Send { prompt }).await?.turn.unwrap_or(1);
    if args.no_wait {
        println!("{name}");
        return Ok(0);
    }
    if !args.json {
        eprintln!("harnessmux: session {name}");
    }
    // Session-level events (session.started, notices) come before the turn's own.
    let mut printer = Printer::new(args.json);
    let end = client::wait_turn(registry, &name, &mut follower, turn, None, |event| printer.print(event)).await?;
    Ok(printer.finish(&end))
}

async fn print_turn(registry: &Registry, name: &str, follower: &mut Follower, turn: u64, json: bool) -> Result<u8> {
    let mut printer = Printer::new(json);
    let end = client::wait_turn(registry, name, follower, turn, None, |event| printer.print(event)).await?;
    Ok(printer.finish(&end))
}

async fn simple(registry: &Registry, name: &str, request: Request) -> Result<u8> {
    registry.load(name)?;
    match ipc::request(registry, name, &request).await {
        Ok(reply) => {
            reply.into_result()?;
            Ok(0)
        }
        Err(Error::Io(..)) => Err(Error::Invalid(format!("no process owns session {name}; nothing runs (`harnessmux resume {name}` starts one)"))),
        Err(error) => Err(error),
    }
}

fn project_of(arg: &ProjectArg) -> Result<Option<PathBuf>> {
    arg.project
        .as_deref()
        .map(|dir| std::path::absolute(dir).map(|dir| client::project_filter(&dir)).map_err(|error| Error::io(format!("directory {}", dir.display()), error)))
        .transpose()
}

async fn list(registry: &Registry, json: bool, project: ProjectArg) -> Result<u8> {
    let project = project_of(&project)?;
    let mut sessions = Vec::new();
    for record in registry.list(project.as_deref())? {
        let gone = owner_gone(registry, &record).await;
        sessions.push((record, gone));
    }
    if json {
        let values: Vec<_> = sessions.iter().map(|(record, gone)| record_json(record, *gone)).collect();
        println!("{}", serde_json::to_string(&values).map_err(|error| Error::Invalid(error.to_string()))?);
        return Ok(0);
    }
    for (record, gone) in sessions {
        let project = record.project.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into());
        let state = if gone { "interrupted" } else { record.state.as_str() };
        println!("{}\t{}\t{}\t{}\t{}", record.name, record.agent, state, project, record.last_used);
    }
    Ok(0)
}

/// Whether the record says a turn runs or waits but no owner answers: it was killed before it could save its state
/// (HARN-20). The record stays as it is, as `resume` needs it.
async fn owner_gone(registry: &Registry, record: &SessionRecord) -> bool {
    matches!(record.state, SessionState::Running | SessionState::Waiting) && !ipc::owner_running(registry, &record.name).await
}

/// The record as JSON, with `"owner_alive": false` when its owner is gone.
fn record_json(record: &SessionRecord, gone: bool) -> serde_json::Value {
    let mut value = serde_json::to_value(record).unwrap_or_default();
    if let (true, Some(object)) = (gone, value.as_object_mut()) {
        object.insert("owner_alive".into(), false.into());
    }
    value
}

async fn status(registry: &Registry, name: &str, json: bool) -> Result<u8> {
    let record = registry.load(name)?;
    let value = match ipc::request(registry, name, &Request::Status).await.and_then(ipc::Reply::into_result) {
        Ok(reply) => serde_json::to_value(reply.status).unwrap_or_default(),
        Err(_) => record_json(&record, matches!(record.state, SessionState::Running | SessionState::Waiting)),
    };
    if json {
        println!("{value}");
    } else {
        let text = |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or("-").to_string();
        let state = if value.get("owner_alive") == Some(&false.into()) { "interrupted".into() } else { text("state") };
        println!("{name}: {state} ({}) in {}", text("agent"), text("cwd"));
        println!("agent session: {}", text("agent_session"));
        if let Some(options) = value.get("options").and_then(|v| v.as_array()) {
            for option in options {
                let values = option
                    .get("values")
                    .and_then(|v| v.as_array())
                    .map(|v| v.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                println!(
                    "option {} = {} [{}]",
                    option.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                    option.get("current").and_then(|v| v.as_str()).unwrap_or(""),
                    values
                );
            }
        }
    }
    Ok(0)
}

async fn close(registry: &Registry, name: &str, forget: bool) -> Result<u8> {
    let mut record = registry.load(name)?;
    match ipc::request(registry, name, &Request::Close).await {
        Ok(reply) => {
            reply.into_result()?;
        }
        Err(_) if record.state != SessionState::Ended => {
            // No owner: end it in the registry and the log.
            record.state = SessionState::Ended;
            record.events += 1;
            let event = Event {
                v: event::EVENT_VERSION,
                ts: event::now(),
                session: name.to_string(),
                project: record.project_name(),
                seq: record.events,
                turn: None,
                body: EventBody::SessionEnded { reason: "closed".into() },
                raw: None,
            };
            let path = registry.events_path(name);
            let line = serde_json::to_string(&event).map_err(|error| Error::Invalid(error.to_string()))? + "\n";
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(line.as_bytes()))
                .map_err(|error| Error::io(format!("writing {}", path.display()), error))?;
            registry.save(&record)?;
        }
        Err(_) => {}
    }
    if forget {
        registry.remove(name)?;
    }
    Ok(0)
}

async fn attach(registry: &Registry, name: Option<String>, from_start: bool, json: bool, project: ProjectArg) -> Result<u8> {
    let project = project_of(&project)?;
    let mut printer = Printer::new(json);
    let Some(name) = name else {
        let project = project.ok_or_else(|| Error::Invalid("attach needs a session name, or --project to follow a whole project".into()))?;
        return attach_project(registry, &project, from_start, &mut printer).await;
    };
    let record = registry.load(&name)?;
    if let Some(project) = project
        && record.project.as_deref() != Some(project.as_path())
    {
        return Err(Error::Invalid(format!("session {name} is not in project {}", project.display())));
    }
    let path = registry.events_path(&name);
    let mut follower = if from_start { Follower::from_start(&path) } else { Follower::from_end(&path) };
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() && line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdin_open = true;
    loop {
        tokio::select! {
            batch = follower.next() => {
                for event in batch? {
                    printer.print(&event);
                    if matches!(event.body, EventBody::SessionEnded { .. }) && registry.load(&name).map(|r| r.state == SessionState::Ended).unwrap_or(true) {
                        return Ok(0);
                    }
                }
            }
            line = line_rx.recv(), if stdin_open => match line {
                Some(line) => {
                    if let Err(error) = client::send(registry, &name, &line).await {
                        eprintln!("harnessmux: {error}");
                    }
                }
                None => stdin_open = false,
            },
        }
    }
}

async fn attach_project(registry: &Registry, project: &Path, from_start: bool, printer: &mut Printer) -> Result<u8> {
    let mut followers: BTreeMap<String, Follower> = BTreeMap::new();
    loop {
        for record in registry.list(Some(project))? {
            followers.entry(record.name.clone()).or_insert_with(|| {
                let path = registry.events_path(&record.name);
                if from_start { Follower::from_start(&path) } else { Follower::from_end(&path) }
            });
        }
        for follower in followers.values_mut() {
            for event in follower.read_new()? {
                printer.print(&event);
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Prints events: JSON lines, or the reply on stdout and the rest as short lines on stderr.
struct Printer {
    json: bool,
    mid_line: bool,
    color: bool,
    /// Turns whose reply was printed from deltas; the others print the whole `message`.
    streamed: std::collections::BTreeSet<(String, u64)>,
    /// The title printed last for each running tool, by session and tool id: a `tool.updated` prints only a new one.
    tool_titles: BTreeMap<(String, String), String>,
}

impl Printer {
    fn new(json: bool) -> Self {
        Self { json, mid_line: false, color: std::io::stderr().is_terminal(), streamed: Default::default(), tool_titles: Default::default() }
    }

    fn note(&mut self, text: &str) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
        let _ = std::io::stdout().flush();
        if self.color { eprintln!("\x1b[2m{text}\x1b[0m") } else { eprintln!("{text}") }
    }

    fn print(&mut self, event: &Event) {
        if self.json {
            if let Ok(line) = serde_json::to_string(event) {
                println!("{line}");
            }
            return;
        }
        match &event.body {
            EventBody::MessageDelta { text } => {
                if let Some(turn) = event.turn {
                    self.streamed.insert((event.session.clone(), turn));
                }
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.mid_line = !text.ends_with('\n');
            }
            EventBody::ToolStarted { id, title, .. } => {
                self.tool_titles.insert((event.session.clone(), id.clone()), title.clone());
                self.note(&format!("· {title}"));
            }
            EventBody::ToolUpdated { id, title: Some(title), .. } if !title.is_empty() => {
                if self.tool_titles.insert((event.session.clone(), id.clone()), title.clone()).as_ref() != Some(title) {
                    self.note(&format!("· {title}"));
                }
            }
            EventBody::ToolFinished { id, status, title, .. } => {
                self.tool_titles.remove(&(event.session.clone(), id.clone()));
                if status == "failed" {
                    self.note(&format!("✗ {}", title.as_deref().unwrap_or("tool failed")));
                }
            }
            EventBody::PermissionRequested { request, title, options } => {
                let ids = options.iter().map(|o| format!("{} ({})", o.id, o.kind)).collect::<Vec<_>>().join(", ");
                self.note(&format!("? {request}: {title} — harnessmux answer {} {request} <{ids}>", event.session));
            }
            EventBody::Notice { text } => self.note(&format!("note: {text}")),
            EventBody::Message { text } if !event.turn.is_some_and(|turn| self.streamed.contains(&(event.session.clone(), turn))) => {
                println!("{}", text.trim_end());
                self.mid_line = false;
            }
            EventBody::TurnStarted { prompt } => {
                let first = prompt.lines().next().unwrap_or_default();
                self.note(&format!("> {first}"));
            }
            EventBody::TurnFailed { error, resets_at, .. } => match resets_at {
                Some(at) => self.note(&format!("turn failed: {error} (resets at {at})")),
                None => self.note(&format!("turn failed: {error}")),
            },
            EventBody::TurnFinished { stop_reason, .. } => {
                if self.mid_line {
                    println!();
                    self.mid_line = false;
                }
                if *stop_reason != harnessmux::StopReason::EndTurn {
                    self.note(&format!("turn ended: {stop_reason:?}"));
                }
            }
            EventBody::SessionEnded { reason } => self.note(&format!("session ended: {reason}")),
            EventBody::Output { output } => self.note(&format!("output: {output}")),
            _ => {}
        }
    }

    fn finish(&mut self, end: &Event) -> u8 {
        if self.mid_line {
            println!();
        }
        end.body.exit_code().unwrap_or(1)
    }
}

fn print_capabilities(value: &serde_json::Value) {
    let agents = match value.as_array() {
        Some(list) => list.clone(),
        None => vec![value.clone()],
    };
    for agent in &agents {
        let text = |key: &str| agent.get(key).and_then(|v| v.as_str()).unwrap_or("-").to_string();
        let installed = agent.get("program").is_some_and(|p| !p.is_null());
        let adapter = agent
            .get("adapter")
            .filter(|a| !a.is_null())
            .map(|a| format!("{}@{}", a.get("package").and_then(|v| v.as_str()).unwrap_or("?"), a.get("version").and_then(|v| v.as_str()).unwrap_or("?")));
        let live = if agent.get("live_checked").and_then(|v| v.as_bool()).unwrap_or(false) { "run live" } else { "from docs, not run live" };
        println!(
            "{}\t{}\t{}\t{}\t{live}",
            text("agent"),
            text("kind"),
            if installed { "installed" } else { "not installed" },
            adapter.unwrap_or_else(|| "native".into())
        );
        if let Some(offers) = agent.get("offers") {
            println!("{}", serde_json::to_string_pretty(offers).unwrap_or_default());
        }
    }
}
