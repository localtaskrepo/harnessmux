# harnessmux: agent instructions

A Rust crate and CLI that run coding-agent CLI sessions over ACP (README.md). lotar links the crate (lotar DEV-503);
its design: lotar `docs/developers/harness-tool-design.md`.

## Commands
- Setup: Rust from `rust-toolchain.toml` (rustup picks it), `cargo install cargo-nextest cargo-deny`. Public repo: no
  local hooks; CI is the gate.
- Check: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo nextest run`,
  `cargo deny check`. One test: `cargo nextest run <name>`.
- Live check of an agent (spends its quota): `HARNESSMUX_HOME=$(mktemp -d) harnessmux run <agent> --record
  /tmp/x.jsonl "Reply with exactly the word: pong"`.
- Release: bump `version` in Cargo.toml and CHANGELOG.md, tag `vX.Y.Z`, push the tag (release.yml builds the
  binaries); `cargo publish` for the crate.

## Differences from the global standards
- Hooks: none in the repo (public, self-contained); CI runs fmt, clippy, nextest on Linux, macOS and Windows, deny.

## Rules
- Tests never start a real agent: an agent's `command` points at `harnessmux replay <recording>`. A user `command`
  replaces the built-in pinned `adapter`; without that, a test named `claude` installed the real adapter (2026-11-13).
- Recordings committed under tests/recordings are scrubbed: no `_meta`, no `auth/*` or `_*` notifications, no
  `available_commands_update` (they hold the account's email and plan, and the owner's skills), paths as /work/repo.
- Adapter versions and option ids live in agents.toml, not in code. Agents not run live stay `live = false` and say
  so in the README table.
- An adapter error sent as an ordinary reply must end as `turn.failed` (acp_map::failure_in_reply); add the text
  when a new adapter does this.
- Event format: add types and fields freely; removing or changing one raises `EVENT_VERSION`.
