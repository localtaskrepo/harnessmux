# Changelog

## 0.3.0 (2026-11-21)

- Tool calls say what they did: `tool.started` gains `input` (the tool's arguments, ACP `rawInput`) and `locations`
  (paths); the new `tool.updated` carries the title, kind, input or paths an adapter sends while the tool runs
  (claude-agent-acp sends a Bash command or Read path only there, so before, Claude tool calls were just "Terminal");
  `tool.finished` gains `output`, the tool's text output cut at 16 KB (`OUTPUT_LIMIT`), for Codex joined from
  codex-acp's terminal output chunks, for Amp from its `tool_result`. A `plan` update is a `plan` event with
  `entries` (`content`, `status`, `priority`). The event format stays version 1. A library match on
  `EventBody::ToolStarted` or `EventBody::ToolFinished` without `..` needs it now, hence the minor version (HARN-14).
- `harnessmux run` and `attach` without `--json` print a running tool's new title from `tool.updated` as another
  `· <title>` line, so a Claude Bash call shows its command, not only "Terminal" (HARN-18).
- `harnessmux list` and `status` show a running or waiting session whose owner process is gone (killed before it
  saved its state) as `interrupted`; `--json` keeps the recorded `state` and adds `"owner_alive": false`. Before,
  such a session kept showing `running` (HARN-20).

## 0.2.1 (2026-11-21)

- A t3 session with a provider and model starts its thread at the profile's effort and `options`, as T3 Code's model
  options (`t3 thread start --option effort=high`, `modelSelection.options` for a thread in a worktree); an `effort`
  in `options` gives way to the profile's effort. Without a provider and model a notice says they are not applied;
  before, effort was always dropped. `capabilities t3` says `"effort": "with --provider and --model"` (HARN-8).

## 0.2.0 (2026-11-20)

- `turn.failed` says what the failure means: `kind` (rate_limited, quota, auth_required, overloaded,
  context_exhausted, transport, other), `resets_at` (when a usage limit resets) and `retryable`, and `raw` holds the
  agent's JSON-RPC error. They come from claude-agent-acp's error data (`errorKind`, `rateLimit`), codex-acp's
  (`codexErrorInfo`, whose `data.message` is now the error text, not "Internal error: {…}") and ACP's `authRequired`
  code first, else from the error text. A library match on `EventBody::TurnFailed { error }` needs `..`
  now, hence the minor version (lotar DEV-683).
- A t3 run that fails ends `turn.failed` with T3 Code's error for it (the provider's rate or usage limit, a worktree
  T3 couldn't recreate), read from the thread's activity with `t3_thread_read`, so its `kind` is typed too; before, it
  said only that the run ended with status 'failed' (HARN-2).
- `usage` carries Claude's usage-limit window as `rate_limit` (`status`, `window`, `resets_at`, `utilization`).
- The replies "You've hit your limit · resets …" and "… 5-hour limit" or "… weekly limit" fail the turn like the other
  limit texts.
- A Codex turn that ends on Codex's own error text (model at capacity, high demand, policy-restricted or flex
  capacity unavailable, retry limit, stream cut off) is `turn.failed`, also after tools ran: codex-acp sends these as
  the turn's last message and `end_turn` (acpx #850). The turn's last chunk is checked too, so an error glued onto an
  earlier message without a blank line also fails it (lotar DEV-691). Its `kind` is `overloaded`, `rate_limited` for
  a 429 retry limit and `transport` for a cut-off stream (lotar DEV-683).
- A denied permission request picks the option the agent table's new `reject` key names first. Codex gets `decline`
  ("continue without running it"), not `cancel`, which codex-acp also lists as `reject_once` and which can end the
  turn (lotar DEV-684).
- `cargo xtask install` builds and installs `harnessmux` from a clone; on macOS it signs it first (identifier
  `net.pyxzl.harnessmux`, hardened runtime) with `HARNESSMUX_SIGNING_IDENTITY` or the maintainer's Developer ID, so
  keychain and firewall grants survive a new build (lotar DEV-564). `cargo install --git` now needs the package name
  (`... harnessmux`): the repository is a workspace with the unpublished `xtask`.
- A t3 turn whose thread waits on its own background task or another thread (status `waiting`, a non-empty
  `pendingBackgroundTasks` or `blockedOnThreadWait`, no question pending) keeps running until T3 Code resumes and ends
  the run, also as a new run, instead of failing with "ended with status 'waiting'" (HARN-3).

## 0.1.10 (2026-11-17)

- A t3 session whose directory is a git worktree of its project starts its thread there, with full access, through
  T3 Code's MCP tool `t3_thread_launch` (`existing_worktree`); before, T3 made the thread in the main checkout or a new
  worktree. `--worktree` is refused in such a directory (lotar DEV-657).
- Later turns of every t3 session go through the MCP tool `t3_thread_send`. The launch and each send carry a
  `clientRequestId` made from the session and the turn, so a retry starts no second thread or run.
- harnessmux signs in to T3 Code's MCP server with a pairing code from `t3 auth pairing create` and keeps the OAuth
  token for 30 days in `<state>/t3-credential.json` (mode 0600); T3 Code lists the sign-in as session `harnessmux`.
  A revoked or expired token signs in again.
- `harnessmux settle <name>` (and `backend::t3::settle` in the library) moves a t3 session's thread to T3 Code's
  Settled group when its last run finished and it doesn't wait for an answer.
- Tested with T3 Code 0.1.10.

## 0.1.9 (2026-11-15)

- The session record notes when a t3 session asked T3 Code for its thread (`agent_session_asked`, saved before
  `t3 thread start` runs) until the thread's id is saved. Every resume without the id looks the thread up from that
  time, also after the turns that failed on a lookup left the session idle; before, only a resume of a session left
  running did, so a later one could start a second thread. A session whose lookup failed looks again at each prompt
  instead of failing every turn until another owner takes it over; a thread it finds is followed first (lotar
  DEV-510).
- A wait for a turn (`send --wait`, `run`, the MCP `wait` tool) whose owner stops without ending the turn (killed)
  fails within a few seconds, saying `harnessmux send <name>` resumes the session; before, it waited forever
  (lotar DEV-578).

## 0.1.8 (2026-11-15)

- Claude's workspace sandbox refuses network access to github.com, gitlab.com and bitbucket.org, so a workspace
  session can't push however the command is written; it can't fetch from those hosts either (lotar DEV-509).
- `deny_commands` also reads the commands in a parameter expansion (`${x:-$(git push)}`).

## 0.1.7 (2026-11-15)

- A t3 session resumed after its owner died before it saved the thread's id no longer starts a thread when
  `t3 thread list` can't be read (an error, a non-zero exit, output that isn't a list; three tries): it fails its turns
  with the reason until it is resumed again, as a new thread could be a second one (lotar DEV-510).

## 0.1.6 (2026-11-15)

- `deny_commands` reads the command line as a shell does instead of splitting it at spaces, so a quoted or escaped
  word stays one word: `git -C "/tmp/my repo" push` is refused, `git commit -m "then git push"` is not. Redirections,
  comments, subshells, `eval`, here-strings and more wrappers (`timeout`, `nice -n 10`) are read too; an argument-list
  command keeps its arguments (lotar DEV-509).

## 0.1.5 (2026-11-15)

- A t3 session whose owner died after `t3 thread start` made the thread and before the session saved its id resumes
  with that thread (the newest with the session's title made since the turn began) and follows its run, instead of
  starting a second thread (lotar DEV-510).

## 0.1.4 (2026-11-14)

- `deny_commands` also matches a command with the program's options before the subcommand (`git -C dir push`,
  `git -c a=b push`), a program path, `env`/`sudo` wrappers and `sh -c "…"`.
- Claude's workspace sandbox no longer runs shell commands without asking and no longer allows unsandboxed retries,
  so the rules see every command.

## 0.1.3 (2026-11-14)

- A t3 turn whose `t3` exits with an error while the thread still runs, or because the T3 Code server doesn't answer,
  follows the thread's run instead of failing; reads that find the server down wait up to 60 minutes for it (lotar
  DEV-467).

## 0.1.2 (2026-11-14)

- `session.started` has the agent session's `link` (a T3 Code thread's).
- A turn that waits for an answer in the agent's own app ends with stop reason `waiting_for_answer` (CLI exit code 3);
  a t3 thread waiting for an answer does this instead of failing the turn.
- A t3 session resumed after its owner died during a turn follows the thread's run to its end in its first turn
  instead of sending another prompt.
- A t3 turn reports the replies of a finished run (the thread's `activeRunId` is null then).
- Claude's workspace sessions run in its OS sandbox again (`meta` in agents.toml, sent as the session's `_meta`).
- Permission rules (`Profile::rules`, `--deny-command`, `--allow-tool`, `--deny-tool`, `--confine-writes`) decide
  requests before the policy; Claude also gets them as its own tool lists (`rules_meta`).
- `AgentTable::insert` adds an agent or replaces one, such as a built-in agent with another command (lotar DEV-512).

## 0.1.1 (2026-11-13)

- A session whose agent fails to start no longer leaves its socket file behind.

## 0.1.0 (2026-11-13)

First release.

- Sessions with names, a registry and an event log per session; the owner process serves a Unix socket (a named
  pipe on Windows) for send, cancel, answer, set, status and close.
- ACP client on agent-client-protocol 5.2: new sessions, resume (`session/resume`, else `session/load`), cancel,
  permission requests with a policy or answered by the caller, config options, MCP servers per session, extra
  directories.
- One mapping for model, effort (low … max) and permissions (read-only, workspace, full), from the agent table.
- Agents: claude (claude-agent-acp 0.107.0), codex (codex-acp 2.6.0), copilot, gemini, cursor, droid, opencode, kilo,
  antigravity over ACP; amp (stream-json) and t3 (T3 Code's thread CLI) as legacy adapters.
- Adapter errors sent as replies end as `turn.failed`.
- CLI: run, send, attach, resume, cancel, answer, set, list, status, close, capabilities, mcp, serve, replay.
- `run --record` and `replay` for recorded sessions; the tests run on them.
