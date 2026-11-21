# harnessmux

Run coding-agent CLI sessions (Claude Code, Codex, Copilot, Gemini, Cursor, Droid, OpenCode, Kilo, Antigravity, Amp
and T3 Code threads) through one interface: named sessions you can send to, follow, cancel and resume from any
process, one mapping for model, effort and permissions, and one JSONL event stream for every agent.

It speaks the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) with the official Rust SDK
([`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) 5.2). Amp has no ACP server and T3 Code is
driven through its own CLI and MCP server; both sit behind the same session interface.

It is a Rust library and a CLI. [lotar](https://github.com/localtaskrepo/lotar) links the library to run its agent
jobs; the CLI is for people, other agents and other apps.

## Install

From a release: download the archive for your platform from
[GitHub Releases](https://github.com/localtaskrepo/harnessmux/releases) and put `harnessmux` on your PATH.

From source (Rust 1.99 or newer):

```sh
git clone https://github.com/localtaskrepo/harnessmux && cd harnessmux
cargo xtask install
```

`cargo xtask install` builds `harnessmux` and installs it in `HARNESSMUX_INSTALL_DIR` (default `~/.cargo/bin`, where
`cargo install` puts it). On macOS it signs it first, identifier `net.pyxzl.harnessmux` with the hardened runtime, so
keychain and firewall grants survive a new build: with `HARNESSMUX_SIGNING_IDENTITY` (a name or SHA-1 hash from
`security find-identity -v -p codesigning`) when set, else with the maintainer's Developer ID when the keychain has it,
else ad-hoc with a warning. A set identity the keychain lacks, a failed codesign or a binary that doesn't run stops the
install and leaves the installed `harnessmux` as it was. The first signing may ask at the Mac to use the key: choose
Always Allow. `cargo install --git https://github.com/localtaskrepo/harnessmux harnessmux` and `cargo install
harnessmux` work too, without the signing.

Each agent needs its own CLI, installed and signed in (`claude`, `codex`, `copilot`, …): harnessmux uses your
existing sign-ins. Claude and Codex also need Node.js: harnessmux installs their pinned ACP adapters with `npm` into
its data directory the first time you use them.

## Use

```sh
harnessmux run claude --name fix-calc --effort high "Fix the failing test in calc.py"
harnessmux send fix-calc --wait "Now add a test for negative numbers"
harnessmux attach fix-calc            # follow it; type a line to send it
harnessmux cancel fix-calc
harnessmux list --project             # the sessions of this repository
harnessmux close fix-calc
```

| Command | What it does |
| --- | --- |
| `run <agent> [PROMPT]` | Start a session in the current directory (`--cwd`) and run the first turn. `--model`, `--effort low\|medium\|high\|xhigh\|max`, `--permissions read-only\|workspace\|full`, `--on-permission`, `--deny-command`, `--allow-tool`, `--deny-tool`, `--confine-writes`, `--option id=value`, `--mcp FILE`, `--add-dir`, `--env`, `--json`, `--no-wait`, `--record FILE`. |
| `send <name> [PROMPT]` | Queue a prompt; it runs after the current turn. Starts an owner and resumes the session when none runs. `--wait` prints the turn. |
| `attach <name>` | Print new events (`--from-start`: the whole log) and send lines typed on stdin. `attach --project` follows every session of the project. |
| `resume <name>` | Start an owner for a session nobody owns, after a crash or a reboot. |
| `cancel <name>` | Cancel the running turn and drop queued prompts. |
| `answer <name> <request> <option>` | Answer a permission request (with `--on-permission ask`). |
| `set <name> <option> <value>` | Set one of the agent's config options; `status` lists them. |
| `list`, `status <name>`, `close <name>` | Sessions; one session's state, agent session id and options; end a session. |
| `settle <name>` | t3: move the session's thread to T3 Code's Settled group when its last run finished and it doesn't wait for an answer; prints the link. |
| `capabilities [agent]` | The agents, whether they are installed, their adapters; with an agent, start it and list what it offers. |
| `mcp` | An MCP server on stdio with tools to start, send to, wait for, cancel and close child sessions. |
| `replay FILE` | An ACP agent that plays back a recording made with `run --record`, for tests. |

`--json` prints the event stream. The exit code is 0 for a finished turn, 1 for a failed one, 2 for a cancelled one, 3 for one
that waits for an answer in the agent's own app;
64 for wrong input, 69 when the agent can't be started, 74 for file and socket errors.

## Sessions

A session is a name, an agent, a working directory and the repository it belongs to (its project: the main
checkout, also when the directory is a git worktree). The registry keeps one JSON file per session and its event log
under `~/.local/state/harnessmux/sessions` (`%LOCALAPPDATA%\harnessmux` on Windows; `HARNESSMUX_HOME` moves
everything into one directory).

The process that starts a session owns it: it holds the agent's ACP connection, queues prompts, writes the log and
serves a socket (a Unix socket, a named pipe on Windows). `run` and `send` start a background owner,
`harnessmux serve <name>`, which stops the agent after 30 minutes without a turn (`--idle-ttl`). The session stays: the
next `send` starts a new owner, which resumes the agent's session with `session/resume`, or `session/load` when the
agent only has that. A wait for a turn whose owner was killed fails within seconds instead of waiting for the turn. A
program that links the library owns its sessions itself, and the CLI reaches them the same way.

## Profile mapping

| harnessmux | Claude (claude-agent-acp) | Codex (codex-acp) | Copilot |
| --- | --- | --- | --- |
| `--effort low … max` | `effort` low … max | `reasoning_effort` low … max | none (a notice) |
| `--permissions read-only` | mode `plan` | mode `read-only` | mode `#plan` |
| `--permissions workspace` | mode `acceptEdits` and its OS sandbox | mode `workspace-write` | mode `#agent` |
| `--permissions full` | mode `bypassPermissions` | mode `agent-full-access` | mode `#autopilot` |

`--model` is passed as the agent lists it; a model the agent doesn't list is refused with the valid ones. An effort
or mode the agent doesn't offer is a `notice` event, not an error. Permission requests the agent still sends follow
`--on-permission`: `deny`, `allow-reads`, `allow` or `ask` (wait for `answer`). The default is `allow-reads` for
read-only and `allow` otherwise, since the mode already limits what the agent asks for. A refusal picks the reject
option the agent table's `reject` names first (Codex: `decline`, not `cancel`, which can end the turn).

Rules decide a request before the policy: `--deny-command "git push"` refuses a command line that runs it. The line is
read as a shell reads it, so quoted words stay whole: also after `&&`, `;`, `|`, in `sh -c "…"` or `$(…)`, with the
program's options in between such as `git -C "/tmp/my repo" -c a=b push`, or through `env` or `sudo`. A command the
line builds while it runs (a program from a variable, a script piped into `sh`) isn't seen: the rules are a best-effort
layer, and only an agent's own sandbox stops a command for sure.
`--deny-tool` and `--allow-tool` match the agent's tool name (Claude's `toolName`, else the title;
`mcp__lotar__*` matches a prefix), and `--confine-writes` refuses edits outside the working directory and `--add-dir`.
At workspace, Claude's shell commands run in its OS sandbox, which blocks writes outside the session's directories,
SSH, and the git hosts github.com, gitlab.com and bitbucket.org (so no push, and no fetch from them either). They still
ask, so the rules see each one; Claude also gets
the tool lists and denied commands as its own `allowedTools` and `disallowedTools` (`Bash(git push:*)`). In the library they are `Profile::rules`.

## Agents

The agent table, [`agents.toml`](agents.toml), says how each agent starts, the pinned adapter version and the ids of
its options. It is read at run time; a file `~/.config/harnessmux/agents.toml` (`%APPDATA%\harnessmux` on Windows)
overrides single keys per agent or adds agents. A `command` of your own replaces the pinned adapter.
Agents marked `live = false` are set up from their docs; [docs/agent-cli-research.md](docs/agent-cli-research.md) collects
what those docs say about each CLI's headless mode.

```toml
[agents.codex]
adapter = { npm = "@agentclientprotocol/codex-acp", version = "2.7.0", bin = "codex-acp" }

[agents.mine]
kind = "acp"
command = ["my-agent", "--acp"]
```

| Agent | Started as | Status |
| --- | --- | --- |
| claude | `claude-agent-acp` 0.107.0 | run live: new, send, resume after the owner was killed, cancel |
| codex | `codex-acp` 2.6.0 | run live: new, send, effort |
| copilot | `copilot --acp --stdio` (1.0.111) | run live: one turn, a model error reported as `turn.failed` |
| t3 | `t3 thread start/show` and T3 Code's MCP server (T3 Code 0.1.10) | run live: new thread, a thread in an existing worktree, a later turn, a repeated launch and send, settle |
| gemini | `gemini --acp` | from its docs, not run live |
| cursor | `cursor-agent acp` | from its docs, not run live |
| droid | `droid exec --output-format acp` | from its docs, not run live |
| opencode | `opencode acp` | from its docs, not run live |
| kilo | `kilo acp` | from its docs, not run live |
| antigravity | `agy_acp_server` | from its docs, not run live |
| amp | `amp --stream-json -x` (no ACP server) | from its docs, not run live |

The adapters are young and change fast. harnessmux pins versions it was tested with (acpx's pinned codex-acp 1.x
failed with `model_not_found` for a ChatGPT account where 2.6.0 works) and bumps them in its own releases.

Some adapters send a failed model call as an ordinary reply that ends the turn. A short reply that is only an error
text (`model_not_found`, an auth failure, `Error: Execution failed: …`) ends the turn as `turn.failed`.

T3 Code takes the profile's effort and `options` as model options (`t3 thread start --option effort=high`) only with
`--provider` and `--model`; without them a notice says they are not applied. Its CLI has no child threads, no interrupt
and no permission answers: `capabilities t3` says so, and `cancel` stops waiting while the thread keeps running in T3 Code. A thread that waits for an answer ends
the turn with `waiting_for_answer` and a notice with the thread's link. When a session's owner died during a turn,
the first turn after the resume follows the thread's run to its end instead of sending its prompt, so the agent
doesn't work twice. The session's record notes each `t3 thread start` (`agent_session_asked`) until the new thread's
id is saved. A session resumed with that note and no id takes the newest thread with the session's `--title` made
since then. When `t3 thread list` can't be read (three tries), the session starts no thread, as it could be a second
one, and fails the turn; the note stays, so each later prompt and resume looks again. A `t3` that exits with an error while the thread still runs, or because the T3 Code server doesn't
answer ("Could not reach the T3 Code server"), doesn't fail the turn: the turn follows the thread's run, waits up to 60
minutes for the server and ends from the thread's state. A t3 session needs `--provider` with
`--model` (`--provider claudeAgent --model claude-haiku-5-5`), starts its thread in the session's project and can
start it in a new worktree (`--worktree main --branch agent/x`).

A t3 session whose directory is a git worktree of its project starts its thread in that worktree, with full access,
through T3 Code's MCP tool `t3_thread_launch` (`existing_worktree`), as T3's CLI can only make a new worktree;
`--worktree` is refused there. Every later turn of a t3 session goes through `t3_thread_send`. Both carry a
`clientRequestId` made from the session and the turn, so a retry after an error starts no second thread or run.
harnessmux signs in to T3 Code's MCP server once: it makes a pairing code with `t3 auth pairing create` (2 minutes,
never shown or logged) and trades it for an OAuth token, kept for 30 days in `<state>/t3-credential.json` (readable by
the user only). T3 Code lists it as session `harnessmux`; `t3 auth session revoke <id>` ends it, and the next turn
signs in again. The server's address comes from T3 Code's `userdata/server-runtime.json` (`T3CODE_HOME`, default
`~/.t3`).

## Events

One JSON object per line. Every event has `v` (format version, 1), `ts`, `session`, `project`, `seq` and, inside a
turn, `turn`; `raw` keeps the agent's own message it came from.

| type | fields |
| --- | --- |
| `session.started` | `agent`, `cwd`, `agent_session`, `how` (new, resumed, loaded), `adapter`, `link` (a T3 Code thread's) |
| `turn.started` | `prompt` |
| `message.delta`, `thought.delta` | `text` |
| `message` | `text`: the whole reply of the turn, before it ends |
| `tool.started` | `id`, `title`, `kind` |
| `tool.finished` | `id`, `status` (completed, failed) |
| `permission.requested` | `request`, `title`, `options` (`id`, `name`, `kind`) |
| `permission.answered` | `request`, `option`, `by` |
| `usage` | `context_used`, `context_size`, `cost_usd`, `rate_limit` (`status`: allowed, allowed_warning, rejected; `window`, `resets_at`, `utilization`; claude only) |
| `config.changed` | `option`, `value` |
| `notice` | `text` |
| `turn.finished` | `stop_reason` (end_turn, max_tokens, max_turn_requests, refusal, cancelled, waiting_for_answer), `usage` |
| `turn.failed` | `error`, `kind` (rate_limited, quota, auth_required, overloaded, context_exhausted, transport, other), `resets_at`, `retryable`; `raw` holds the agent's JSON-RPC error |
| `session.ended` | `reason` (closed, idle, or the error) |

New types and fields can appear within version 1; readers skip what they don't know.

A failed turn's `kind` comes from the adapter's error data where it sends some (claude-agent-acp: `data.errorKind`,
`data.rateLimit.resetsAt`, ACP's `authRequired` code; codex-acp: `data.codexErrorInfo`, with the reason in `data.message`), else from the error text ("You've hit your usage limit",
"Not logged in", "Overloaded", …). Only the error data gives `resets_at`, an RFC 3339 UTC time; a reset time in the text
is not read. `retryable` is true for rate_limited, overloaded and transport: the same prompt can succeed later (after
`resets_at`) without anyone changing anything.

## Library

```rust
use harnessmux::{AgentTable, Effort, Profile, Registry, Session, StartOptions};

let table = AgentTable::load()?;
let profile = Profile { effort: Some(Effort::High), ..Profile::default() };
let options = StartOptions { name: Some("DEV-1".into()), agent: "codex".into(), cwd: "/work/repo".into(), profile, idle_ttl: None };
let session = Session::start(Registry::default(), &table, options).await?;
let mut events = session.events();
let turn = session.send("Fix the failing test").await?;
// read events until event.turn == Some(turn) && event.body.ends_turn()
```

`Session::resume` takes over a session after a restart; `cancel`, `answer`, `set`, `status` and `close` do what the
CLI commands do. `harnessmux::client` reaches sessions another process owns. `AgentTable::insert` adds an agent or
replaces one, for example a copy of `claude` with another `command` (clear its `adapter` so the command runs).

## Recordings and tests

`run --record FILE` writes every ACP message between harnessmux and the agent to a JSONL file, and `harnessmux replay
FILE` is an ACP agent that plays it back: point an agent's `command` at `["harnessmux", "replay", "FILE"]` to run a
session without a real agent. Recordings hold your account details (emails, plans, the agent's command list): the
ones in [`tests/recordings`](tests/recordings) had those removed.

## Develop

```sh
cargo nextest run                                    # tests (recorded sessions, no agents needed)
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo deny check
```

CI runs the tests on Linux, macOS and Windows. A tag `v*` builds the release binaries and publishes the crate to
crates.io (trusted publishing, no stored token).

## License

MIT, see [LICENSE](LICENSE).
