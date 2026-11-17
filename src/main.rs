use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

mod commands;

/// Run coding-agent CLI sessions (Claude, Codex, Copilot, Gemini, Kilo, …) over the Agent Client Protocol.
#[derive(Parser)]
#[command(name = "harnessmux", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct PromptArgs {
    /// The prompt; `-` reads it from stdin.
    prompt: Option<String>,
    /// Read the prompt from a file.
    #[arg(short = 'f', long = "file", conflicts_with = "prompt")]
    file: Option<PathBuf>,
}

#[derive(Args)]
struct ProjectArg {
    /// Only the sessions of this directory's project (the repository's main checkout); without a value, the current
    /// directory's.
    #[arg(long, num_args = 0..=1, default_missing_value = ".")]
    project: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a session and, with a prompt, run its first turn.
    Run(Box<commands::RunArgs>),
    /// Send a prompt to a session; it runs after the current turn. Resumes the session when no process owns it.
    Send {
        name: String,
        #[command(flatten)]
        prompt: PromptArgs,
        /// Wait for the turn and print it; the exit code is the turn's (0 finished, 1 failed, 2 cancelled).
        #[arg(long)]
        wait: bool,
        /// Print the turn's events as JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Follow a session's events; lines typed on stdin are sent as prompts. With --project and no name, follow every
    /// session of the project.
    Attach {
        name: Option<String>,
        /// Print the log from its start, not only new events.
        #[arg(long)]
        from_start: bool,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Start an owner for a session no process owns (after a crash or a reboot); the agent resumes it.
    Resume { name: String },
    /// Cancel the running turn and drop queued prompts.
    Cancel { name: String },
    /// Answer a permission request (from a permission.requested event) with one of its option ids.
    Answer { name: String, request: String, option: String },
    /// Set one of the agent's config options (see `status`).
    Set { name: String, option: String, value: String },
    /// List sessions.
    List {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Show a session's state, agent session id and options.
    Status {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// End a session for good; with --forget, also delete its record and log.
    Close {
        name: String,
        #[arg(long)]
        forget: bool,
    },
    /// t3: move the session's T3 Code thread to T3's Settled group, when its last run finished and it doesn't wait for
    /// an answer.
    Settle { name: String },
    /// Show the agents, whether they are installed and what they offer; with an agent, start it to ask.
    Capabilities {
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Serve MCP on stdio: tools to start, message, wait for and cancel child sessions.
    Mcp,
    /// Own a session in this process: hold the agent connection and serve its socket (`run` and `send` start this in
    /// the background).
    Serve {
        name: String,
        /// The session was just registered by `run`; start it instead of resuming it.
        #[arg(long)]
        new: bool,
        /// Stop the agent after this many seconds without a turn (0: never).
        #[arg(long, default_value_t = 1800)]
        idle_ttl: u64,
    },
    /// Act as an ACP agent on stdio that plays back a recorded session (`run --record`).
    Replay { file: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("harnessmux: starting the async runtime: {error}");
            return ExitCode::from(70);
        }
    };
    match runtime.block_on(commands::dispatch(cli.command)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("harnessmux: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
