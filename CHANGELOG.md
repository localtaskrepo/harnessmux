# Changelog

## Unreleased

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
