# Changelog

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
