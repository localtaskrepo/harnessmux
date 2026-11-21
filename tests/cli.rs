#![allow(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]
//! The CLI end to end, with `harnessmux replay` playing recorded ACP sessions as the agent.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

fn cargo_bin(_: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harnessmux"))
}

struct Home {
    dir: tempfile::TempDir,
    cwd: PathBuf,
}

/// A HARNESSMUX_HOME with an agent table whose agents replay the given recordings.
fn home(agents: &[(&str, &str, &str)]) -> Home {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    let bin = cargo_bin("harnessmux");
    let mut table = String::new();
    for (name, recording, extra) in agents {
        let recording = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recordings").join(recording);
        table.push_str(&format!(
            "[agents.{name}]\nkind = \"acp\"\ncommand = [{:?}, \"replay\", {:?}]\nlive = false\n{extra}\n\n",
            bin.display().to_string(),
            recording.display().to_string()
        ));
    }
    std::fs::write(dir.path().join("config/agents.toml"), table).unwrap();
    Home { dir, cwd }
}

impl Home {
    fn cmd(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(cargo_bin("harnessmux"));
        command.args(args).env("HARNESSMUX_HOME", self.dir.path()).current_dir(&self.cwd);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let child = self.cmd(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        self.finish(child, args)
    }

    /// Waits for a command, at most 40 s; a hung one fails with the sessions' logs. Its output is read meanwhile:
    /// a full pipe would block it.
    fn finish(&self, mut child: std::process::Child, args: &[&str]) -> Output {
        fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                bytes
            })
        }
        let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
        let deadline = Instant::now() + Duration::from_secs(40);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("harnessmux {args:?} hung\n{}", self.logs());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Output { status, stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() }
    }

    /// Every session's event log and owner log.
    fn logs(&self) -> String {
        let dir = self.dir.path().join("state/sessions");
        let mut out = String::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".jsonl") || name.ends_with(".log") {
                out.push_str(&format!("== {name}\n{}\n", std::fs::read_to_string(entry.path()).unwrap_or_default()));
            }
        }
        out
    }

    fn events(&self, name: &str) -> Vec<Value> {
        let path = self.dir.path().join("state/sessions").join(format!("{name}.events.jsonl"));
        std::fs::read_to_string(path).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    fn wait_for(&self, name: &str, event_type: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(event) = self.events(name).into_iter().find(|e| e["type"] == event_type) {
                return event;
            }
            assert!(Instant::now() < deadline, "no {event_type}\n{}", self.logs());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kills session `name`'s owner and waits until the process is gone: a killed owner can still answer a request for
    /// a moment, which a test that resumes the session right away would send to it.
    #[cfg(unix)]
    fn kill_owner(&self, name: &str) {
        let status: Value = serde_json::from_slice(&self.run(&["status", name, "--json"]).stdout).unwrap();
        let pid = status["owner_pid"].as_u64().unwrap().to_string();
        assert!(std::process::Command::new("kill").args(["-9", &pid]).status().unwrap().success());
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::process::Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success() {
            assert!(Instant::now() < deadline, "owner {pid} of {name} outlived kill -9");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn close_all(&self) {
        let list = self.run(&["list", "--json"]);
        let sessions: Vec<Value> = serde_json::from_slice(&list.stdout).unwrap_or_default();
        for session in sessions {
            self.run(&["close", session["name"].as_str().unwrap()]);
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn text(output: &Output) -> String {
    format!("stdout:\n{}\nstderr:\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

const CLAUDE: &str = r#"options = { model = "model", effort = "effort", mode = "mode" }
effort = { low = "low", medium = "medium", high = "high", xhigh = "xhigh", max = "max" }
modes = { read-only = "plan", workspace = "acceptEdits", full = "bypassPermissions" }"#;

const CODEX: &str = r#"options = { model = "model", effort = "reasoning_effort", mode = "mode" }
effort = { low = "low", medium = "medium", high = "high", xhigh = "xhigh", max = "max" }
modes = { read-only = "read-only", workspace = "workspace-write", full = "agent-full-access" }"#;

#[test]
fn a_recorded_claude_session_runs_two_turns_and_remembers() {
    let home = home(&[("claude", "claude-two-turns.jsonl", CLAUDE)]);
    let first = home.run(&["run", "claude", "--name", "c1", "--model", "haiku", "--effort", "low", "Reply with exactly the word: pong"]);
    assert!(first.status.success(), "{}", text(&first));
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "pong");

    let second = home.run(&["send", "c1", "--wait", "--json", "What word did you reply with last time? One word."]);
    assert!(second.status.success(), "{}", text(&second));
    let events: Vec<Value> = String::from_utf8_lossy(&second.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(events.iter().all(|e| e["turn"] == 2 && e["v"] == 1 && e["session"] == "c1"), "{events:#?}");
    let finished = events.last().unwrap();
    assert_eq!(finished["type"], "turn.finished");
    assert_eq!(finished["stop_reason"], "end_turn");
    assert!(finished["usage"]["total_tokens"].as_u64().unwrap() > 0, "{finished}");
    assert!(events.iter().any(|e| e["type"] == "message" && e["text"] == "pong"), "{events:#?}");

    let all = home.events("c1");
    let started = &all[0];
    assert_eq!(started["type"], "session.started");
    assert_eq!(started["how"], "new");
    assert!(started["agent_session"].is_string());
    assert!(!all.iter().any(|e| e["type"] == "notice"), "model, effort and mode all applied: {all:#?}");

    let status = home.run(&["status", "c1", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let options = status["options"].as_array().unwrap();
    let current = |id: &str| options.iter().find(|o| o["id"] == id).unwrap()["current"].clone();
    assert_eq!(current("model"), "haiku");
    assert_eq!(current("effort"), "low");
    assert_eq!(current("mode"), "acceptEdits");

    let closed = home.run(&["close", "c1"]);
    assert!(closed.status.success(), "{}", text(&closed));
    let refused = home.run(&["send", "c1", "hi"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("was closed"), "{}", text(&refused));
}

#[test]
fn a_recorded_codex_session_maps_effort_onto_reasoning_effort() {
    let home = home(&[("codex", "codex-two-turns.jsonl", CODEX)]);
    let first = home.run(&["run", "codex", "--name", "x1", "--effort", "low", "Reply with exactly the word: pong"]);
    assert!(first.status.success(), "{}", text(&first));
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "pong");
    let second = home.run(&["send", "x1", "--wait", "Which word did you just reply with? One word."]);
    assert!(second.status.success(), "{}", text(&second));
    let status: Value = serde_json::from_slice(&home.run(&["status", "x1", "--json"]).stdout).unwrap();
    let options = status["options"].as_array().unwrap();
    assert_eq!(options.iter().find(|o| o["id"] == "reasoning_effort").unwrap()["current"], "low");
    assert_eq!(options.iter().find(|o| o["id"] == "mode").unwrap()["current"], "workspace-write");
}

#[test]
fn a_codex_deny_declines_instead_of_cancelling_and_an_overloaded_turn_fails() {
    let home = home(&[("codex", "codex-overloaded-after-a-deny.jsonl", CODEX)]);
    let run = home.run(&["run", "codex", "--name", "o1", "--effort", "low", "--on-permission", "deny", "Run the tests"]);
    // codex-acp lists `cancel` (ends the turn) and `decline` both as reject_once; a deny takes `decline` by id.
    let answered = home.events("o1").into_iter().find(|e| e["type"] == "permission.answered").unwrap();
    assert_eq!(answered["option"], "decline", "{answered}");
    // codex-acp sends `server_overloaded` as the turn's text and ends with end_turn (acpx #850), also after tools ran.
    assert_eq!(run.status.code(), Some(1), "{}", text(&run));
    let failed = home.events("o1").into_iter().find(|e| e["type"] == "turn.failed").unwrap();
    assert!(failed["error"].as_str().unwrap().contains("at capacity"), "{failed}");
    assert_eq!((&failed["kind"], &failed["retryable"]), (&"overloaded".into(), &true.into()), "{failed}");
}

#[test]
fn a_codex_error_glued_onto_an_earlier_message_fails_the_turn() {
    let home = home(&[("codex", "codex-error-glued-onto-a-message.jsonl", CODEX)]);
    let run = home.run(&["run", "codex", "--name", "g1", "--effort", "low", "Fix the failing test"]);
    // codex-acp sends the error as a chunk of its own; the message before it ends without a blank line.
    assert_eq!(run.status.code(), Some(1), "{}", text(&run));
    let failed = home.events("g1").into_iter().find(|e| e["type"] == "turn.failed").unwrap();
    assert_eq!(failed["error"], "Flex capacity unavailable.", "{failed}");
    assert_eq!((&failed["kind"], &failed["retryable"]), (&"overloaded".into(), &true.into()), "{failed}");
}

#[test]
fn claude_tool_calls_carry_their_command_paths_output_and_the_plan() {
    let home = home(&[("claude", "claude-tool-calls.jsonl", CLAUDE)]);
    let run = home.run(&["run", "claude", "--name", "t1", "--model", "haiku", "--effort", "low", "Run the tests, read calc.py and plan the fix"]);
    assert!(run.status.success(), "{}", text(&run));
    let events = home.events("t1");
    let of = |kind: &str, id: &str| events.iter().find(|e| e["type"] == kind && e["id"] == id).unwrap_or_else(|| panic!("no {kind} {id}: {events:#?}")).clone();
    // claude-agent-acp starts a Bash call with `rawInput: {}` and sends the command in an in-progress update.
    let started = of("tool.started", "toolu_1");
    assert_eq!((&started["title"], started.get("input")), (&"Terminal".into(), None), "{started}");
    let updated = of("tool.updated", "toolu_1");
    assert_eq!((&updated["input"]["command"], &updated["title"]), (&"cargo test".into(), &"cargo test".into()), "{updated}");
    assert_eq!(of("tool.finished", "toolu_1")["output"], "```console\ntest add ... FAILED\n```");
    let read = of("tool.started", "toolu_2");
    assert_eq!((&read["locations"], &read["input"]["file_path"]), (&serde_json::json!(["/work/repo/calc.py"]), &"/work/repo/calc.py".into()), "{read}");
    assert_eq!(of("tool.finished", "toolu_2")["output"], "def add(a, b):\n    return a - b\n");
    let plan = events.iter().find(|e| e["type"] == "plan").unwrap();
    assert_eq!(plan["entries"][0], serde_json::json!({"content": "Fix add()", "status": "in_progress", "priority": "medium"}), "{plan}");
    assert_eq!(plan["entries"].as_array().unwrap().len(), 2);
    // Without --json the CLI prints the command from the update, once.
    let tools: Vec<&str> = std::str::from_utf8(&run.stderr).unwrap().lines().filter(|l| l.starts_with("· ")).collect();
    assert_eq!(tools, ["· Terminal", "· cargo test", "· Read /work/repo/calc.py"], "{}", text(&run));
}

#[test]
fn codex_tool_calls_carry_their_command_and_the_joined_terminal_output() {
    let home = home(&[("codex", "codex-tool-calls.jsonl", CODEX)]);
    let run = home.run(&["run", "codex", "--name", "t2", "--effort", "low", "Run the tests"]);
    assert!(run.status.success(), "{}", text(&run));
    let events = home.events("t2");
    let started = events.iter().find(|e| e["type"] == "tool.started").unwrap();
    assert_eq!((&started["title"], &started["input"]["command"]), (&"cargo test".into(), &"cargo test".into()), "{started}");
    // An update with only terminal output adds no event; its output reaches `tool.finished`.
    assert!(!events.iter().any(|e| e["type"] == "tool.updated"), "{events:#?}");
    let finished = events.iter().find(|e| e["type"] == "tool.finished").unwrap();
    assert_eq!((&finished["status"], &finished["output"]), (&"failed".into(), &"running 1 test\ntest add ... FAILED\n".into()), "{finished}");
    let tools: Vec<&str> = std::str::from_utf8(&run.stderr).unwrap().lines().filter(|l| l.starts_with("· ")).collect();
    assert_eq!(tools, ["· cargo test"], "{}", text(&run));
}

#[test]
fn copilot_without_an_effort_option_gets_a_notice() {
    let home = home(&[("copilot", "copilot-one-turn.jsonl", r#"options = { model = "model", mode = "mode" }"#)]);
    let run = home.run(&["run", "copilot", "--name", "g1", "--model", "auto", "--effort", "low", "Reply with exactly the word: pong"]);
    assert!(run.status.success(), "{}", text(&run));
    let notices: Vec<String> = home.events("g1").iter().filter(|e| e["type"] == "notice").map(|e| e["text"].as_str().unwrap().to_string()).collect();
    assert!(notices.iter().any(|n| n.contains("no effort setting")), "{notices:?}");
}

#[test]
fn a_permission_request_waits_for_answer_and_an_error_reply_fails_the_turn() {
    let home = home(&[(
        "rec",
        "permission-then-error.jsonl",
        r#"options = { mode = "mode" }
modes = { workspace = "acceptEdits" }"#,
    )]);
    let run = home
        .cmd(&["run", "rec", "--name", "p", "--on-permission", "ask", "Fix add()"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let requested = home.wait_for("p", "permission.requested");
    assert_eq!(requested["request"], "p1");
    assert_eq!(requested["title"], "Edit calc.py");
    assert_eq!(requested["options"][0]["kind"], "allow_once");
    let status: Value = serde_json::from_slice(&home.run(&["status", "p", "--json"]).stdout).unwrap();
    assert_eq!(status["state"], "waiting");

    let wrong = home.run(&["answer", "p", "p1", "maybe"]);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("options: allow, reject"), "{}", text(&wrong));
    let answered = home.run(&["answer", "p", "p1", "allow"]);
    assert!(answered.status.success(), "{}", text(&answered));
    let finished = home.finish(run, &["run", "rec"]);
    assert!(finished.status.success(), "{}", text(&finished));
    assert_eq!(String::from_utf8_lossy(&finished.stdout).trim(), "Fixed add() to return a + b.", "{}\n{:#?}", text(&finished), home.events("p"));

    // The adapter sends the model error as an ordinary reply that ends the turn: harnessmux fails the turn.
    let failed = home.run(&["send", "p", "--wait", "And now?"]);
    assert_eq!(failed.status.code(), Some(1), "{}", text(&failed));
    let last = home.events("p").into_iter().rev().find(|e| e["turn"] == 2 && e["type"] == "turn.failed").unwrap();
    assert!(last["error"].as_str().unwrap().contains("model_not_found"), "{last}");
    assert_eq!((&last["kind"], &last["retryable"]), (&"other".into(), &false.into()), "{last}");
}

#[test]
fn claude_limit_and_auth_errors_fail_the_turn_with_their_kind() {
    // claude-agent-acp 0.107.0's shapes (result-end.ts, v2/outcome.ts): a usage limit fails the prompt with
    // `data.errorKind` and `data.rateLimit`, a sign-in failure with ACP's authRequired (-32000).
    let home = home(&[("rec", "claude-limit-then-auth.jsonl", "")]);
    let limited = home.run(&["run", "rec", "--name", "l", "Fix add()"]);
    assert_eq!(limited.status.code(), Some(1), "{}", text(&limited));
    assert!(String::from_utf8_lossy(&limited.stderr).contains("resets at 2026-11-14T17:00:00.000Z"), "{}", text(&limited));
    let events = home.events("l");
    let usage = events.iter().find(|e| e["type"] == "usage").unwrap();
    assert_eq!(
        usage["rate_limit"],
        serde_json::json!({"status": "rejected", "window": "five_hour", "resets_at": "2026-11-14T17:00:00.000Z", "utilization": 1.0})
    );
    let failed = events.iter().find(|e| e["type"] == "turn.failed").unwrap();
    assert_eq!(failed["error"], "You've hit your limit · resets 6pm (Europe/Berlin)");
    assert_eq!(
        (&failed["kind"], &failed["resets_at"], &failed["retryable"]),
        (&"rate_limited".into(), &"2026-11-14T17:00:00.000Z".into(), &true.into()),
        "{failed}"
    );
    assert_eq!(failed["raw"]["data"]["errorKind"], "rate_limit");

    let auth = home.run(&["send", "l", "--wait", "And now?"]);
    assert_eq!(auth.status.code(), Some(1), "{}", text(&auth));
    let failed = home.events("l").into_iter().rev().find(|e| e["turn"] == 2 && e["type"] == "turn.failed").unwrap();
    assert_eq!((&failed["kind"], &failed["retryable"]), (&"auth_required".into(), &false.into()), "{failed}");
    assert!(failed.get("resets_at").is_none(), "{failed}");
}

#[test]
fn a_recorded_codex_usage_limit_replays_as_rate_limited() {
    // codex-acp 2.6.0 sends the reason in `data.message` under the generic "Internal error" (live, 2026-11-16).
    let home = home(&[("rec", "codex-usage-limit.jsonl", "")]);
    let limited = home.run(&["run", "rec", "--name", "c", "Fix add()"]);
    assert_eq!(limited.status.code(), Some(1), "{}", text(&limited));
    let failed = home.events("c").into_iter().find(|e| e["type"] == "turn.failed").unwrap();
    assert!(failed["error"].as_str().unwrap().starts_with("You’ve hit your usage limit."), "{failed}");
    assert_eq!((&failed["kind"], &failed["retryable"]), (&"rate_limited".into(), &true.into()), "{failed}");
    assert!(failed.get("resets_at").is_none(), "{failed}");
    assert_eq!(failed["raw"]["data"]["codexErrorInfo"], "usageLimitExceeded");
}

#[test]
fn a_rule_refuses_a_request_the_policy_would_allow() {
    let home = home(&[("rec", "permission-then-error.jsonl", "options = { mode = \"mode\" }\nmodes = { workspace = \"acceptEdits\" }")]);
    let run = home.run(&["run", "rec", "--name", "r", "--deny-tool", "Edit *", "Fix add()"]);
    assert!(run.status.success(), "{}", text(&run));
    let answered = home.events("r").into_iter().find(|e| e["type"] == "permission.answered").unwrap();
    assert_eq!(answered["option"], "reject", "{answered}");
    assert_eq!(answered["by"], "rule deny_tools Edit *", "{answered}");
}

#[test]
fn list_filters_by_project_and_bad_input_names_the_valid_values() {
    let home = home(&[("claude", "claude-two-turns.jsonl", CLAUDE)]);
    let bad = home.run(&["run", "claude", "--effort", "ultra", "x"]);
    assert_eq!(bad.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("use one of: low, medium, high, xhigh, max"), "{}", text(&bad));
    let unknown = home.run(&["run", "nope", "x"]);
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("known agents:"), "{}", text(&unknown));

    let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&home.cwd).output().unwrap().status.success());
    git(&["init", "-q"]);
    let started = home.run(&["run", "claude", "--name", "in-repo", "--no-wait"]);
    assert!(started.status.success(), "{}", text(&started));
    let other = home.dir.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let elsewhere = home.run(&["run", "claude", "--name", "outside", "--cwd", other.to_str().unwrap(), "--no-wait"]);
    assert!(elsewhere.status.success(), "{}", text(&elsewhere));

    let here: Vec<Value> = serde_json::from_slice(&home.run(&["list", "--json", "--project"]).stdout).unwrap();
    assert_eq!(here.iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(), ["in-repo"]);
    let all: Vec<Value> = serde_json::from_slice(&home.run(&["list", "--json"]).stdout).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn capabilities_list_every_agent_without_starting_any() {
    let output = std::process::Command::new(cargo_bin("harnessmux"))
        .args(["capabilities", "--json"])
        .env("HARNESSMUX_HOME", tempfile::tempdir().unwrap().path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let agents: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = agents.iter().map(|a| a["agent"].as_str().unwrap()).collect();
    assert_eq!(names, ["amp", "antigravity", "claude", "codex", "copilot", "cursor", "droid", "gemini", "kilo", "opencode", "t3"]);
    let t3 = agents.iter().find(|a| a["agent"] == "t3").unwrap();
    assert_eq!(t3["offers"]["effort"], "with --provider and --model");
    assert_eq!(t3["offers"]["child_threads"], false);
}

#[test]
fn the_mcp_server_lists_its_tools() {
    let mut child = std::process::Command::new(cargo_bin("harnessmux"))
        .arg("mcp")
        .env("HARNESSMUX_HOME", tempfile::tempdir().unwrap().path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"status","arguments":{{"name":"none"}}}}}}"#).unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "harnessmux");
    let tools: Vec<&str> = replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(tools, ["start", "send", "wait", "cancel", "status", "list", "close"]);
    assert_eq!(replies[2]["result"]["isError"], true);
}

#[test]
fn start_errors_reach_the_caller_and_leave_no_session() {
    let home = home(&[("claude", "claude-two-turns.jsonl", CLAUDE)]);
    let model = home.run(&["run", "claude", "--name", "m", "--model", "gpt-9", "hi"]);
    assert_eq!(model.status.code(), Some(69), "{}", text(&model));
    let stderr = String::from_utf8_lossy(&model.stderr);
    assert!(stderr.contains("gpt-9 is not a value of claude option model; values: default, opus"), "{stderr}");

    std::fs::write(
        home.dir.path().join("config/agents.toml"),
        "[agents.ghost]\nkind = \"acp\"\ncommand = [\"no-such-agent-cli\"]\nrequires = \"no-such-agent-cli\"\n",
    )
    .unwrap();
    let missing = home.run(&["run", "ghost", "--name", "g", "hi"]);
    assert_eq!(missing.status.code(), Some(69), "{}", text(&missing));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not installed or not on PATH. Install no-such-agent-cli"), "{}", text(&missing));
    let sessions: Vec<Value> = serde_json::from_slice(&home.run(&["list", "--json"]).stdout).unwrap();
    assert!(sessions.is_empty(), "{sessions:?}");
    let socket = harnessmux::Registry::new(home.dir.path().join("state")).socket_name("g");
    assert!(cfg!(windows) || !Path::new(&socket).exists(), "{socket} left behind");
}

/// A fake `t3` CLI in `dir`: `thread start` names thread t1, prints `start-error` (if any) to stderr and exits with
/// the code in `start-exit` after `start-sleep` seconds, `thread show` prints `show.json`, `thread list` prints
/// `list.json` (or fails while `list-fail` exists), `auth pairing create` prints pairing code `pair-1` (fails while
/// `pairing-fail` exists); every call is appended to `calls`, and `thread start`'s arguments are written to `start-args`. While `down` holds a number above 0, `thread show` counts
/// it down and fails as T3's CLI does when its server is down. A [`fake_t3_server`] answers T3 Code's MCP endpoint.
#[cfg(unix)]
fn fake_t3(home: &Home) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = home.dir.path().join("t3");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("t3");
    std::fs::write(
        &script,
        r#"#!/bin/sh
dir=$(dirname "$0")
echo "$1 $2" >> "$dir/calls"
case "$1 $2" in
  "thread start") echo "$*" > "$dir/start-args"; cat > /dev/null; echo "Thread t1" >&2; cat "$dir/start-error" >&2 2>/dev/null; sleep "$(cat "$dir/start-sleep" 2>/dev/null || echo 0)"; echo "Started."; exit "$(cat "$dir/start-exit")" ;;
  "auth pairing")
    if [ -f "$dir/pairing-fail" ]; then echo "Could not reach the T3 Code server at http://127.0.0.1:3773" >&2; exit 1; fi
    echo '{"credential":"pair-1","expiresAt":"2026-01-01T00:02:00.000Z","id":"p","label":"harnessmux","scopes":[]}' ;;
  "thread list")
    if [ -f "$dir/list-fail" ]; then echo "Could not reach the T3 Code server at http://127.0.0.1:3773" >&2; exit 1; fi
    cat "$dir/list.json" ;;
  "thread show")
    down=$(cat "$dir/down" 2>/dev/null || echo 0)
    if [ "$down" -gt 0 ]; then
      echo $((down - 1)) > "$dir/down"
      echo "Could not reach the T3 Code server at http://127.0.0.1:3773" >&2
      exit 1
    fi
    cat "$dir/show.json" ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let t3_home = dir.join("t3home");
    std::fs::create_dir_all(t3_home.join("userdata")).unwrap();
    let origin = fake_t3_server(&dir, &home.cwd);
    std::fs::write(t3_home.join("userdata/server-runtime.json"), format!(r#"{{"version":1,"origin":"{origin}"}}"#)).unwrap();
    std::fs::write(
        home.dir.path().join("config/agents.toml"),
        format!(
            "[agents.t3]\nkind = \"t3\"\ncommand = [{:?}]\nlive = false\nenv = {{ T3CODE_HOME = {:?} }}\n",
            script.display().to_string(),
            t3_home.display().to_string()
        ),
    )
    .unwrap();
    dir
}

#[cfg(unix)]
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
        }
    }
    out
}

/// A fake T3 Code server for `dir`'s fake `t3`, with one project at `project`: the MCP OAuth sign-in (the pairing code
/// `pair-1`, PKCE checked) and `/mcp`. `t3_thread_launch` returns thread t1 (the same thread for a repeated
/// `clientRequestId`), or fails with the text in `launch-error`; `t3_thread_send` and the launch name run r2;
/// `t3_thread_read` pages the items in `activity.json` (without it, the assistant replies in `show.json`; from the second
/// page read on, also those in `activity-more.json`) as T3 Code does for a client outside a thread: the oldest `limit` without a position, the newest `limit` before
/// `beforePosition`, the oldest `limit` after `afterPosition`, an item's text from `textOffset` with `itemId`; texts are
/// cut at `maxCharsPerItem`. Each
/// sign-in step and tool call is appended to `calls` (`oauth token`, `mcp <tool> <arguments>`). Once `revoke` exists,
/// the next `/mcp` request finds every token revoked (and removes the file). Returns the origin.
#[cfg(unix)]
fn fake_t3_server(dir: &Path, project: &Path) -> String {
    use sha2::Digest as _;
    use std::collections::{HashMap, HashSet};
    use std::io::{BufRead as _, Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (dir, project) = (dir.to_path_buf(), project.to_path_buf());
    std::thread::spawn(move || {
        let log = |line: String| {
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("calls")).unwrap();
            writeln!(file, "{line}").unwrap();
        };
        let (mut tokens, mut codes, mut threads) = (HashSet::new(), HashMap::new(), HashMap::new());
        let mut page_reads = 0;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let (mut length, mut bearer) = (0, String::new());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (key, value) = line.split_once(':').unwrap();
                match key.to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap(),
                    "authorization" => bearer = value.trim().trim_start_matches("Bearer ").to_string(),
                    _ => {}
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let json = |status: u16, value: Value| (status, value.to_string());
            let (status, answer) = match path.as_str() {
                "/oauth/mcp/register" => {
                    log("oauth register".into());
                    json(201, serde_json::json!({"client_id": "c1"}))
                }
                "/oauth/mcp/decision" => {
                    let request: Value = serde_json::from_str(&body).unwrap();
                    let decision = &request["decision"];
                    if decision["code"] != "pair-1" || decision["access"] != "full-access" {
                        json(400, serde_json::json!({"error": "invalid_grant", "error_description": "Invalid pairing code"}))
                    } else {
                        let code = format!("ac{}", codes.len() + 1);
                        codes.insert(code.clone(), request["authorization"]["code_challenge"].as_str().unwrap().to_string());
                        json(200, serde_json::json!({"redirectTo": format!("http://127.0.0.1/callback?code={code}&state=s")}))
                    }
                }
                "/oauth/mcp/token" => {
                    let form: HashMap<&str, &str> = body.split('&').filter_map(|pair| pair.split_once('=')).collect();
                    let challenge = codes.remove(form["code"]).unwrap();
                    assert_eq!(base64url(&sha2::Sha256::digest(form["code_verifier"].as_bytes())), challenge, "PKCE");
                    let token = format!("tok-secret-{}", tokens.len() + 1);
                    tokens.insert(token.clone());
                    log("oauth token".into());
                    json(200, serde_json::json!({"access_token": token, "token_type": "Bearer", "expires_in": 2_592_000}))
                }
                "/mcp" => {
                    if std::fs::remove_file(dir.join("revoke")).is_ok() {
                        tokens.clear();
                    }
                    let request: Value = serde_json::from_str(&body).unwrap();
                    if !tokens.contains(&bearer) {
                        json(401, serde_json::json!({"error": "invalid_mcp_credential"}))
                    } else if request.get("id").is_none() {
                        (202, String::new())
                    } else if request["method"] == "initialize" {
                        json(200, serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {}}}))
                    } else {
                        let (tool, arguments) = (request["params"]["name"].as_str().unwrap(), &request["params"]["arguments"]);
                        log(format!("mcp {tool} {arguments}"));
                        let thread = |id: &str| serde_json::json!({"threadId": id, "link": format!("https://t3/{id}"), "runId": "r2", "status": "starting"});
                        let result = match tool {
                            "t3_project_list" => Ok(serde_json::json!({"projects": [{"id": "p1", "workspaceRoot": project}], "nextCursor": null})),
                            "t3_thread_launch" => match std::fs::read_to_string(dir.join("launch-error")) {
                                Ok(error) => Err(error),
                                Err(_) => {
                                    let n = threads.len() + 1;
                                    let id = threads.entry(arguments["clientRequestId"].as_str().unwrap().to_string()).or_insert_with(|| format!("t{n}"));
                                    Ok(thread(id))
                                }
                            },
                            "t3_thread_send" => Ok(thread(arguments["threadId"].as_str().unwrap())),
                            "t3_thread_read" => {
                                // The activity holds the replies `thread show` prints, unless `activity.json` says what it holds.
                                let mut items: Vec<Value> = match std::fs::read_to_string(dir.join("activity.json")) {
                                    Ok(activity) => serde_json::from_str(&activity).unwrap(),
                                    Err(_) => std::fs::read_to_string(dir.join("show.json"))
                                        .ok()
                                        .and_then(|show| serde_json::from_str::<Value>(&show).ok())
                                        .and_then(|show| show["messages"].as_array().cloned())
                                        .unwrap_or_default()
                                        .into_iter()
                                        .filter(|m| m["role"] == "assistant")
                                        .enumerate()
                                        .map(|(n, m)| serde_json::json!({"itemId": format!("m{n}"), "runId": m["runId"], "type": "assistant_message", "status": "completed", "text": m["text"]}))
                                        .collect(),
                                };
                                // The items in `activity-more.json` arrive after the first page read.
                                if arguments.get("itemId").is_none() {
                                    page_reads += 1;
                                }
                                if page_reads > 1
                                    && let Ok(more) = std::fs::read_to_string(dir.join("activity-more.json"))
                                {
                                    items.extend(serde_json::from_str::<Vec<Value>>(&more).unwrap());
                                }
                                // Positions count from 0 where an item has none.
                                let items: Vec<Value> = items
                                    .into_iter()
                                    .enumerate()
                                    .map(|(n, mut item)| {
                                        if item.get("position").is_none() {
                                            item["position"] = n.into();
                                        }
                                        item
                                    })
                                    .collect();
                                let at = |item: &Value| item["position"].as_u64().unwrap();
                                let limit = arguments["limit"].as_u64().map_or(20, |n| n as usize);
                                let max = arguments["maxCharsPerItem"].as_u64().map_or(usize::MAX, |n| n as usize);
                                if let Some(id) = arguments["itemId"].as_str() {
                                    let item = items.iter().find(|item| item["itemId"] == id).unwrap();
                                    let text: Vec<char> = item["text"].as_str().unwrap().chars().collect();
                                    let from = arguments["textOffset"].as_u64().unwrap_or(0) as usize;
                                    let to = (from + max).min(text.len());
                                    let mut part = item.clone();
                                    part["text"] = text[from..to].iter().collect::<String>().into();
                                    part["textTruncated"] = (to < text.len()).into();
                                    part["nextTextOffset"] = if to < text.len() { to.into() } else { Value::Null };
                                    Ok(serde_json::json!({"items": [part]}))
                                } else {
                                    let page: Vec<&Value> = match (arguments["beforePosition"].as_u64(), arguments["afterPosition"].as_i64()) {
                                        (Some(before), _) => {
                                            let older: Vec<&Value> = items.iter().filter(|item| at(item) < before).collect();
                                            older[older.len().saturating_sub(limit)..].to_vec()
                                        }
                                        (None, Some(after)) => items.iter().filter(|item| at(item) as i64 > after).take(limit).collect(),
                                        (None, None) => items.iter().take(limit).collect(),
                                    };
                                    let has_more = page.last().is_some_and(|last| items.iter().any(|item| at(item) > at(last)));
                                    let page: Vec<Value> = page
                                        .into_iter()
                                        .map(|item| {
                                            let mut item = item.clone();
                                            let text: Vec<char> = item["text"].as_str().unwrap_or_default().chars().collect();
                                            if text.len() > max {
                                                item["text"] = text[..max].iter().collect::<String>().into();
                                                item["textTruncated"] = true.into();
                                                item["nextTextOffset"] = max.into();
                                            }
                                            item
                                        })
                                        .collect();
                                    Ok(serde_json::json!({"items": page, "hasMore": has_more}))
                                }
                            }
                            _ => Ok(serde_json::json!({"sequence": 1})),
                        };
                        let result = match result {
                            Ok(content) => {
                                serde_json::json!({"content": [{"type": "text", "text": content.to_string()}], "structuredContent": content, "isError": false})
                            }
                            Err(error) => serde_json::json!({"content": [{"type": "text", "text": error}], "isError": true}),
                        };
                        json(200, serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
                    }
                }
                _ => (404, String::new()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    origin
}

/// The fake T3 Code server's calls of `tool`, with their arguments.
#[cfg(unix)]
fn mcp_calls(dir: &Path, tool: &str) -> Vec<Value> {
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
    let prefix = format!("mcp {tool} ");
    calls.lines().filter_map(|line| line.strip_prefix(&prefix)).map(|args| serde_json::from_str(args).unwrap()).collect()
}

#[cfg(unix)]
fn count_calls(dir: &Path, call: &str) -> usize {
    std::fs::read_to_string(dir.join("calls")).unwrap_or_default().lines().filter(|line| *line == call).count()
}

/// Makes `home.cwd` a git repository with a commit and adds the worktree `<home>/wt`; returns its path.
#[cfg(unix)]
fn git_worktree(home: &Home) -> PathBuf {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").args(args).current_dir(&home.cwd).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", text(&output));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "first"]);
    let worktree = home.dir.path().join("wt");
    git(&["worktree", "add", "-q", "-b", "agent/DEV-1", worktree.to_str().unwrap()]);
    worktree.canonicalize().unwrap()
}

#[cfg(unix)]
fn show(dir: &Path, status: &str, active: &str, pending: u64, messages: &str) {
    let json = format!(
        r#"{{"thread":{{"threadId":"t1","link":"https://t3/t1","status":"{status}","activeRunId":{active},"latestRunId":"r2","pendingRequestCount":{pending}}},"messages":{messages}}}"#
    );
    std::fs::write(dir.join("show.json"), json).unwrap();
}

#[cfg(unix)]
#[test]
fn a_t3_turn_reports_the_threads_tool_calls_thoughts_and_replies_from_its_activity() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    let long = "word ".repeat(1000);
    show(&dir, "completed", "null", 0, &serde_json::json!([{"role": "assistant", "runId": "r2", "text": long}]).to_string());
    let item = |id: &str, kind: &str, status: &str, text: &str| serde_json::json!({"itemId": id, "runId": "r2", "type": kind, "status": status, "title": null, "text": text});
    let items = [
        item("u1", "user_message", "completed", "Fix it"),
        item("th1", "reasoning", "completed", "Reading the tests."),
        item("c1", "command_execution", "failed", "$ cargo test\nerror: 1 failed"),
        item("m1", "assistant_message", "completed", &long),
        item("x1", "command_execution", "completed", "$ ls")
            .as_object()
            .cloned()
            .map(|mut other| {
                other.insert("runId".into(), "r1".into());
                Value::Object(other)
            })
            .unwrap(),
    ];
    std::fs::write(dir.join("activity.json"), Value::from(items.to_vec()).to_string()).unwrap();
    let run = home.run(&["run", "t3", "--name", "a", "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    let events = home.events("a");
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).filter(|t| *t != "session.started").collect();
    assert_eq!(types, ["turn.started", "thought.delta", "tool.started", "tool.finished", "message.delta", "message", "turn.finished"], "{events:?}");
    let tool = events.iter().find(|e| e["type"] == "tool.started").unwrap();
    assert_eq!((&tool["id"], &tool["title"], &tool["kind"]), (&"c1".into(), &"cargo test".into(), &"execute".into()), "{tool}");
    assert_eq!(tool["raw"]["text"], "$ cargo test\nerror: 1 failed", "{tool}");
    let done = events.iter().find(|e| e["type"] == "tool.finished").unwrap();
    assert_eq!((&done["id"], &done["status"]), (&"c1".into(), &"failed".into()), "{done}");
    assert_eq!(done["output"], "error: 1 failed", "{done}");
    // The reply is longer than one read gives; it is read on by its id.
    let reply = events.iter().find(|e| e["type"] == "message.delta").unwrap();
    assert_eq!(reply["text"].as_str().unwrap(), format!("{long}\n\n"));
    assert!(mcp_calls(&dir, "t3_thread_read").iter().any(|read| read["itemId"] == "m1" && read["textOffset"] == 4000));
}

#[cfg(unix)]
#[test]
fn a_t3_turn_reads_its_activity_page_by_page_without_missing_or_repeating_an_item() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Done"}]"#);
    let command = |n: usize, run: &str| serde_json::json!({"itemId": format!("c{n}"), "runId": run, "type": "command_execution", "status": "completed", "text": format!("$ step {n}")});
    // An earlier run, then more than two pages of this run's items before the first read, and more than two after it.
    let before: Vec<Value> = (0..5).map(|n| command(n, "r1")).chain((5..255).map(|n| command(n, "r2"))).collect();
    let after: Vec<Value> = (255..505).map(|n| command(n, "r2")).collect();
    std::fs::write(dir.join("activity.json"), Value::from(before).to_string()).unwrap();
    std::fs::write(dir.join("activity-more.json"), Value::from(after).to_string()).unwrap();
    let run = home.run(&["run", "t3", "--name", "p", "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    let events = home.events("p");
    let started: Vec<&str> = events.iter().filter(|e| e["type"] == "tool.started").map(|e| e["id"].as_str().unwrap()).collect();
    let expected: Vec<String> = (5..505).map(|n| format!("c{n}")).collect();
    assert_eq!(started, expected);
    assert_eq!(events.iter().filter(|e| e["type"] == "tool.finished").count(), 500);
    let reads = mcp_calls(&dir, "t3_thread_read");
    assert!(reads.iter().filter(|read| read.get("beforePosition").is_some()).count() >= 3, "{reads:?}");
    assert!(reads.iter().filter(|read| read.get("afterPosition").is_some()).count() >= 3, "{reads:?}");
}

#[cfg(unix)]
#[test]
fn a_t3_turn_whose_activity_cant_be_read_still_streams_its_replies() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    std::fs::write(dir.join("pairing-fail"), "").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Working"}]"#);
    let run = home.run(&["run", "t3", "--name", "f", "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    let events = home.events("f");
    let notices: Vec<&Value> = events.iter().filter(|e| e["type"] == "notice").collect();
    assert_eq!(notices.len(), 1, "{events:?}");
    assert!(notices[0]["text"].as_str().unwrap().contains("shows its replies but not its tool calls"), "{events:?}");
    let replies: Vec<&Value> = events.iter().filter(|e| e["type"] == "message.delta").collect();
    assert_eq!(replies.len(), 1, "{events:?}");
    assert_eq!(replies[0]["text"], "Working\n\n");
}

#[cfg(unix)]
#[test]
fn a_t3_thread_waiting_for_an_answer_ends_the_turn_with_its_link() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "2").unwrap();
    show(&dir, "running", "\"r1\"", 1, "[]");
    let run = home.run(&["run", "t3", "--name", "w", "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert_eq!(run.status.code(), Some(3), "{}", text(&run));
    let events = home.events("w");
    let started = events.iter().find(|e| e["type"] == "session.started").unwrap();
    assert_eq!(started["link"], "https://t3/t1", "{started}");
    let end = events.iter().find(|e| e["type"] == "turn.finished").unwrap();
    assert_eq!(end["stop_reason"], "waiting_for_answer", "{end}");
    assert!(events.iter().any(|e| e["type"] == "notice" && e["text"].as_str().unwrap().contains("waits for an answer; answer it in T3 Code: https://t3/t1")));
}

#[cfg(unix)]
#[test]
fn a_t3_thread_starts_at_the_profiles_effort_as_a_model_option() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Done."}]"#);
    let run = home.run(&["run", "t3", "--name", "e", "--provider", "codex", "--model", "m", "--effort", "high", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    let args = std::fs::read_to_string(dir.join("start-args")).unwrap();
    assert!(args.contains("--provider codex --model m --option effort=high"), "{args}");
    let events = home.events("e");
    assert!(!events.iter().any(|e| e["type"] == "notice" && e["text"].as_str().unwrap().contains("effort")), "{events:?}");

    let run = home.run(&["run", "t3", "--name", "n", "--effort", "high", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    assert!(!std::fs::read_to_string(dir.join("start-args")).unwrap().contains("--option"));
    let events = home.events("n");
    assert!(events.iter().any(|e| e["type"] == "notice" && e["text"].as_str().unwrap().contains("only with a provider and model")), "{events:?}");
}

#[cfg(unix)]
#[test]
fn a_t3_turn_that_loses_the_server_waits_for_it_and_ends_from_the_thread() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    // t3 exits 1 as the server stops answering; the thread finishes meanwhile (lotar DEV-467).
    std::fs::write(dir.join("start-exit"), "1").unwrap();
    std::fs::write(dir.join("start-error"), "Could not reach the T3 Code server at http://127.0.0.1:3773\n").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Done."}]"#);
    // More failed reads than a run is followed through otherwise, all with the server down.
    std::fs::write(dir.join("down"), "4").unwrap();
    let run = home.run(&["run", "t3", "--name", "u", "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    let events = home.events("u");
    let end = events.iter().find(|e| e["type"] == "turn.finished").unwrap();
    assert_eq!(end["stop_reason"], "end_turn", "{end}");
    assert!(!events.iter().any(|e| e["type"] == "turn.failed"), "{events:?}");
    assert!(events.iter().any(|e| e["type"] == "message" && e["text"] == "Done."), "{events:?}");
    assert!(events.iter().any(|e| e["type"] == "notice" && e["text"].as_str().unwrap().contains("The T3 Code server doesn't answer")), "{events:?}");
}

#[cfg(unix)]
#[test]
fn a_t3_session_whose_owner_died_mid_turn_follows_the_thread_run_instead_of_sending_again() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Started."}]"#);
    let first = home.run(&["run", "t3", "--name", "k", "--provider", "claudeAgent", "--model", "m", "Start"]);
    assert!(first.status.success(), "{}", text(&first));

    // The second turn's run keeps running; the owner dies while it follows the run.
    show(&dir, "running", "\"r2\"", 0, r#"[{"role":"assistant","runId":"r2","text":"Working."}]"#);
    let turn = home.run(&["send", "k", "Go on"]);
    assert!(turn.status.success(), "{}", text(&turn));
    home.wait_for("k", "message.delta");
    home.kill_owner("k");

    // The thread ended its run meanwhile: the resumed session reports it and sends nothing.
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Working."},{"role":"assistant","runId":"r2","text":"Finished."}]"#);
    let resumed = home.run(&["send", "k", "--wait", "Continue"]);
    assert!(resumed.status.success(), "{}", text(&resumed));
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("Finished."), "{}", text(&resumed));
    assert_eq!(mcp_calls(&dir, "t3_thread_send").len(), 1);
}

#[cfg(unix)]
#[test]
fn a_t3_thread_waiting_on_its_own_background_task_keeps_the_turn_running() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Started."}]"#);
    let first = home.run(&["run", "t3", "--name", "b", "--provider", "claudeAgent", "--model", "m", "Start"]);
    assert!(first.status.success(), "{}", text(&first));

    // The agent ended its run with t3_background_wait on a build: T3 Code resumes it when the build ends (HARN-3).
    let waiting = r#"{"thread":{"threadId":"t1","link":"https://t3/t1","status":"waiting","activeRunId":null,"latestRunId":"r2","pendingRequestCount":0,
        "blockedOnThreadWait":false,"pendingBackgroundTasks":[{"taskId":"b1"}]},"messages":[{"role":"assistant","runId":"r2","text":"Waiting for the build."}]}"#;
    std::fs::write(dir.join("show.json"), waiting).unwrap();
    let wait = home.cmd(&["send", "b", "--wait", "Build it"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    home.wait_for("b", "message.delta");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!home.events("b").iter().any(|e| e["type"] == "turn.failed"), "{:?}", home.events("b"));

    show(
        &dir,
        "completed",
        "null",
        0,
        r#"[{"role":"assistant","runId":"r2","text":"Waiting for the build."},{"role":"assistant","runId":"r2","text":"Built."}]"#,
    );
    let waited = home.finish(wait, &["send", "b", "--wait", "Build it"]);
    assert!(waited.status.success(), "{}", text(&waited));
    let events = home.events("b");
    assert!(!events.iter().any(|e| e["type"] == "turn.failed"), "{events:?}");
    assert_eq!(events.iter().rfind(|e| e["type"] == "turn.finished").unwrap()["stop_reason"], "end_turn", "{events:?}");
}

#[cfg(unix)]
#[test]
fn a_t3_run_resumed_under_a_new_run_id_is_followed_to_its_end() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Started."}]"#);
    let first = home.run(&["run", "t3", "--name", "r", "--provider", "claudeAgent", "--model", "m", "Start"]);
    assert!(first.status.success(), "{}", text(&first));

    // The sent run r2 waits on its background task; T3 Code resumes it as run r3 (HARN-4).
    let thread = |status: &str, active: &str, latest: &str, tasks: &str, messages: &str| {
        let json = format!(
            r#"{{"thread":{{"threadId":"t1","link":"https://t3/t1","status":"{status}","activeRunId":{active},"latestRunId":"{latest}","pendingRequestCount":0,"blockedOnThreadWait":false,"pendingBackgroundTasks":{tasks}}},"messages":{messages}}}"#
        );
        std::fs::write(dir.join("show.json"), json).unwrap();
    };
    let r2 = r#"{"role":"assistant","runId":"r2","text":"Waiting for the build."}"#;
    thread("waiting", "null", "r2", r#"[{"taskId":"b1"}]"#, &format!("[{r2}]"));
    let wait = home.cmd(&["send", "r", "--wait", "Build it"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    home.wait_for("r", "message.delta");
    std::thread::sleep(Duration::from_millis(500));

    let r3 = r#"{"role":"assistant","runId":"r3","text":"Testing."}"#;
    thread("running", "\"r3\"", "r3", "[]", &format!("[{r2},{r3}]"));
    std::thread::sleep(Duration::from_millis(500));
    let ended = |e: &Value| e["type"] == "turn.failed" || (e["type"] == "turn.finished" && e["turn"] == 2);
    assert!(!home.events("r").iter().any(ended), "{:?}", home.events("r"));

    thread("completed", "null", "r3", "[]", &format!(r#"[{r2},{r3},{{"role":"assistant","runId":"r3","text":"Built."}}]"#));
    let waited = home.finish(wait, &["send", "r", "--wait", "Build it"]);
    assert!(waited.status.success(), "{}", text(&waited));
    assert!(String::from_utf8_lossy(&waited.stdout).contains("Built."), "{}", text(&waited));
    let events = home.events("r");
    assert!(!events.iter().any(|e| e["type"] == "turn.failed"), "{events:?}");
    assert_eq!(events.iter().rfind(|e| e["type"] == "message").unwrap()["text"], "Built.", "{events:?}");
    assert_eq!(events.iter().rfind(|e| e["type"] == "turn.finished").unwrap()["stop_reason"], "end_turn", "{events:?}");
}

#[cfg(unix)]
#[test]
fn an_owner_that_takes_over_numbers_its_events_after_the_logs_last() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Started."}]"#);
    let first = home.run(&["run", "t3", "--name", "q", "--provider", "claudeAgent", "--model", "m", "Start"]);
    assert!(first.status.success(), "{}", text(&first));

    // The owner dies after logging an event it never saved the count of.
    show(&dir, "running", "\"r2\"", 0, r#"[{"role":"assistant","runId":"r2","text":"Working."}]"#);
    let turn = home.run(&["send", "q", "Go on"]);
    assert!(turn.status.success(), "{}", text(&turn));
    home.wait_for("q", "message.delta");
    home.kill_owner("q");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(home.dir.path().join("state/sessions/q.json")).unwrap()).unwrap();
    let logged = home.events("q").last().unwrap()["seq"].as_u64().unwrap();
    assert!(record["events"].as_u64().unwrap() < logged, "the record saved the count of every event: {record}");

    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Finished."}]"#);
    let resumed = home.run(&["send", "q", "--wait", "Continue"]);
    assert!(resumed.status.success(), "{}", text(&resumed));
    let seqs: Vec<u64> = home.events("q").iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>(), "{}", home.logs());
}

#[cfg(unix)]
#[test]
fn a_wait_whose_owner_dies_mid_turn_ends_with_an_error() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Started."}]"#);
    let first = home.run(&["run", "t3", "--name", "d", "--provider", "claudeAgent", "--model", "m", "Start"]);
    assert!(first.status.success(), "{}", text(&first));

    // The second turn's run keeps running; the owner dies while `send --wait` follows the turn.
    show(&dir, "running", "\"r2\"", 0, r#"[{"role":"assistant","runId":"r2","text":"Working."}]"#);
    let wait = home.cmd(&["send", "d", "--wait", "Go on"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    home.wait_for("d", "message.delta");
    let status: Value = serde_json::from_slice(&home.run(&["status", "d", "--json"]).stdout).unwrap();
    let pid = status["owner_pid"].as_u64().unwrap().to_string();
    assert!(std::process::Command::new("kill").args(["-9", &pid]).status().unwrap().success());

    let waited = home.finish(wait, &["send", "d", "--wait", "Go on"]);
    assert!(!waited.status.success(), "{}", text(&waited));
    assert!(String::from_utf8_lossy(&waited.stderr).contains("the owner of d stopped during turn 2"), "{}", text(&waited));
}

#[cfg(unix)]
#[test]
fn list_and_status_show_a_running_session_whose_owner_died_as_interrupted() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Started."}]"#);
    let first = home.run(&["run", "t3", "--name", "i", "--provider", "claudeAgent", "--model", "m", "Start"]);
    assert!(first.status.success(), "{}", text(&first));
    show(&dir, "running", "\"r2\"", 0, r#"[{"role":"assistant","runId":"r2","text":"Working."}]"#);
    assert!(home.run(&["send", "i", "Go on"]).status.success());
    home.wait_for("i", "message.delta");

    let listed = |home: &Home| -> (String, Value) {
        let text = String::from_utf8(home.run(&["list"]).stdout).unwrap();
        let json: Vec<Value> = serde_json::from_slice(&home.run(&["list", "--json"]).stdout).unwrap();
        (text, json.into_iter().find(|s| s["name"] == "i").unwrap())
    };
    let status = |home: &Home| -> (String, Value) {
        let text = String::from_utf8(home.run(&["status", "i"]).stdout).unwrap();
        (text, serde_json::from_slice(&home.run(&["status", "i", "--json"]).stdout).unwrap())
    };

    // A live owner: as before, no owner_alive.
    let (text, json) = listed(&home);
    assert!(text.starts_with("i\tt3\trunning\t"), "{text}");
    assert_eq!(json["state"], "running");
    assert!(json.get("owner_alive").is_none(), "{json}");
    let (text, json) = status(&home);
    assert!(text.starts_with("i: running (t3)"), "{text}");
    assert!(json.get("owner_alive").is_none(), "{json}");

    home.kill_owner("i");
    let (text, json) = listed(&home);
    assert!(text.starts_with("i\tt3\tinterrupted\t"), "{text}");
    assert_eq!(json["state"], "running", "{json}");
    assert_eq!(json["owner_alive"], false, "{json}");
    let (text, json) = status(&home);
    assert!(text.starts_with("i: interrupted (t3)"), "{text}");
    assert_eq!(json["state"], "running", "{json}");
    assert_eq!(json["owner_alive"], false, "{json}");
    // The record keeps its state for resume.
    let record: Value = serde_json::from_str(&std::fs::read_to_string(home.dir.path().join("state/sessions/i.json")).unwrap()).unwrap();
    assert_eq!(record["state"], "running", "{record}");
}

/// Session `n` with title "DEV-1 Fix" whose owner died in its first turn after `thread start` made t1 and before it
/// saved t1's id: the record notes when the thread was asked for, not its id. `list.json` lists t1 as made then.
/// Returns the record's path.
#[cfg(unix)]
fn owner_died_before_it_saved_the_thread(home: &Home, dir: &Path) -> PathBuf {
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    std::fs::write(dir.join("start-sleep"), "20").unwrap();
    show(dir, "running", "\"r2\"", 0, r#"[{"role":"assistant","runId":"r2","text":"Working."}]"#);
    let turn = home.run(&["run", "t3", "--name", "n", "--provider", "claudeAgent", "--model", "m", "--title", "DEV-1 Fix", "--no-wait", "Start"]);
    assert!(turn.status.success(), "{}", text(&turn));
    home.wait_for("n", "session.started");
    home.kill_owner("n");

    let path = home.dir.path().join("state/sessions/n.json");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["agent_session"], "t1");
    assert_eq!(record["state"], "running");
    assert!(record.get("agent_session_asked").is_none(), "cleared once the id is saved: {record}");
    let since = record["last_used"].as_str().unwrap().to_string();
    record.as_object_mut().unwrap().remove("agent_session");
    record["agent_session_asked"] = Value::String(since.clone());
    std::fs::write(&path, record.to_string()).unwrap();
    let list = format!(
        r#"[{{"threadId":"t0","title":"DEV-1 Fix","createdAt":"2000-01-01T00:00:00.000Z"}},{{"threadId":"t1","title":"DEV-1 Fix","createdAt":"{since}"}}]"#
    );
    std::fs::write(dir.join("list.json"), list).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn a_t3_session_whose_owner_died_before_it_saved_the_thread_finds_the_thread_by_its_title() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    let path = owner_died_before_it_saved_the_thread(&home, &dir);

    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Finished."}]"#);
    let resumed = home.run(&["send", "n", "--wait", "Continue"]);
    assert!(resumed.status.success(), "{}", text(&resumed));
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("Finished."), "{}", text(&resumed));
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|call| *call == "thread start").count(), 1, "{calls}");
    assert!(mcp_calls(&dir, "t3_thread_send").is_empty(), "{calls}");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["agent_session"], "t1");
}

#[cfg(unix)]
#[test]
fn a_t3_session_that_cant_look_up_the_interrupted_turns_thread_starts_no_second_one() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    owner_died_before_it_saved_the_thread(&home, &dir);
    std::fs::write(dir.join("list-fail"), "").unwrap();

    for prompt in ["Continue", "And again"] {
        let resumed = home.run(&["send", "n", "--wait", prompt]);
        assert_eq!(resumed.status.code(), Some(1), "{}", text(&resumed));
    }
    let failed: Vec<Value> = home.events("n").into_iter().filter(|e| e["type"] == "turn.failed").collect();
    assert_eq!(failed.len(), 2, "{failed:?}");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains("can't tell whether that turn made a thread") && error.contains("Could not reach the T3 Code server"), "{error}");
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|call| *call == "thread start").count(), 1, "{calls}");
    // Three tries at the resume, three more at each prompt.
    assert_eq!(calls.lines().filter(|call| *call == "thread list").count(), 9, "{calls}");

    // The failed turns left the session idle; a later owner keeps the note and still doesn't start a thread.
    let status: Value = serde_json::from_slice(&home.run(&["status", "n", "--json"]).stdout).unwrap();
    assert_eq!(status["state"], "idle", "{status}");
    home.kill_owner("n");
    let resumed = home.run(&["resume", "n"]);
    assert!(resumed.status.success(), "{}", text(&resumed));
    let path = home.dir.path().join("state/sessions/n.json");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(record.get("agent_session").is_none() && record["agent_session_asked"].is_string(), "{record}");

    // Once T3 answers, the next prompt finds t1 and follows its run instead of starting or sending.
    std::fs::remove_file(dir.join("list-fail")).unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Finished."}]"#);
    let found = home.run(&["send", "n", "--wait", "Third"]);
    assert!(found.status.success(), "{}", text(&found));
    assert!(String::from_utf8_lossy(&found.stdout).contains("Finished."), "{}", text(&found));
    let record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["agent_session"], "t1", "{record}");
    assert!(record.get("agent_session_asked").is_none(), "{record}");
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|call| *call == "thread start").count(), 1, "{calls}");
    assert!(mcp_calls(&dir, "t3_thread_send").is_empty(), "{calls}");
}

/// Every file under `root` that holds `needle`, the kept credential aside.
#[cfg(unix)]
fn files_holding(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|name| name != "t3-credential.json")
                && std::fs::read(&path).is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
            {
                found.push(path);
            }
        }
    }
    found
}

#[cfg(unix)]
#[test]
fn a_t3_session_in_a_worktree_launches_its_thread_there_and_sends_later_turns_through_mcp() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = home(&[]);
    let dir = fake_t3(&home);
    let worktree = git_worktree(&home);
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Done."}]"#);
    let wt = worktree.to_str().unwrap();
    let run =
        home.run(&["run", "t3", "--name", "w", "--cwd", wt, "--provider", "claudeAgent", "--model", "m", "--effort", "high", "--title", "DEV-1 Fix", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    assert!(String::from_utf8_lossy(&run.stdout).contains("Done."), "{}", text(&run));

    assert_eq!(count_calls(&dir, "thread start"), 0);
    let launches = mcp_calls(&dir, "t3_thread_launch");
    assert_eq!(launches.len(), 1, "{launches:?}");
    let launch = &launches[0];
    assert_eq!(launch["projectId"], "p1", "{launch}");
    assert_eq!(launch["workspaceStrategy"]["type"], "existing_worktree", "{launch}");
    assert_eq!(Path::new(launch["workspaceStrategy"]["worktreePath"].as_str().unwrap()).canonicalize().unwrap(), worktree);
    assert_eq!(launch["runtimeMode"], "full-access", "{launch}");
    assert_eq!(launch["title"], "DEV-1 Fix", "{launch}");
    let model = serde_json::json!({"instanceId": "claudeAgent", "model": "m", "options": [{"id": "effort", "value": "high"}]});
    assert_eq!(launch["modelSelection"], model, "{launch}");
    assert_eq!(launch["message"], "Fix it", "{launch}");
    let launch_id = launch["clientRequestId"].as_str().unwrap();
    assert!(launch_id.starts_with("harnessmux:w:") && launch_id.ends_with(":launch"), "{launch_id}");
    let events = home.events("w");
    let started = events.iter().find(|e| e["type"] == "session.started").unwrap();
    assert_eq!(started["link"], "https://t3/t1", "{started}");

    // The credential is kept for the user only; the token reaches no log or record.
    let credential = home.dir.path().join("state/t3-credential.json");
    assert_eq!(std::fs::metadata(&credential).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(std::fs::read_to_string(&credential).unwrap().contains("tok-secret-1"));
    assert_eq!(files_holding(&home.dir.path().join("state"), "tok-secret"), Vec::<PathBuf>::new());
    assert_eq!(files_holding(&home.dir.path().join("state"), "pair-1"), Vec::<PathBuf>::new());

    let next = home.run(&["send", "w", "--wait", "Go on"]);
    assert!(next.status.success(), "{}", text(&next));
    let sends = mcp_calls(&dir, "t3_thread_send");
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(sends[0]["threadId"], "t1");
    assert_eq!(sends[0]["message"], "Go on");
    assert_eq!(sends[0]["mode"], "auto");
    let send_id = sends[0]["clientRequestId"].as_str().unwrap();
    assert!(send_id.starts_with("harnessmux:w:") && send_id.ends_with(":2"), "{send_id}");

    // A second session signs in no more.
    let other = home.run(&["run", "t3", "--name", "x", "--cwd", wt, "--provider", "claudeAgent", "--model", "m", "Other"]);
    assert!(other.status.success(), "{}", text(&other));
    assert_eq!(count_calls(&dir, "oauth token"), 1);
    assert_eq!(mcp_calls(&dir, "t3_thread_launch")[1]["title"], "x");
}

#[cfg(unix)]
#[test]
fn a_failed_t3_run_ends_the_turn_with_the_error_from_the_threads_activity() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    let worktree = git_worktree(&home);
    show(&dir, "failed", "null", 0, "[]");
    // T3 Code keeps a run's error as an `error` item of the thread's activity; `t3 thread show` has none. The run
    // worked before it hit the limit, so its error is past the oldest 20 items.
    let mut items = vec![serde_json::json!({"type": "error", "runId": "r1", "title": "Old", "text": "an earlier run's error"})];
    items.extend((0..25).map(|n| serde_json::json!({"type": "command_execution", "runId": "r2", "text": format!("$ step {n}")})));
    items.push(serde_json::json!({"type": "error", "runId": "r2", "status": "failed", "title": "Provider error", "text": "Claude API rate limit reached. Try again later."}));
    std::fs::write(dir.join("activity.json"), serde_json::Value::from(items).to_string()).unwrap();
    let run = home.run(&["run", "t3", "--name", "w", "--cwd", worktree.to_str().unwrap(), "Fix it"]);
    assert!(!run.status.success(), "{}", text(&run));
    let events = home.events("w");
    let failed = events.iter().find(|e| e["type"] == "turn.failed").unwrap();
    let error = failed["error"].as_str().unwrap();
    assert!(error.contains("status 'failed': Provider error: Claude API rate limit reached. Try again later. (https://t3/t1)"), "{failed}");
    assert!(!error.contains("earlier run"), "{failed}");
    assert_eq!((&failed["kind"], &failed["retryable"]), (&"rate_limited".into(), &true.into()), "{failed}");
    assert_eq!(mcp_calls(&dir, "t3_thread_read")[0]["view"], "activity");
}

#[cfg(unix)]
#[test]
fn a_revoked_or_expired_t3_credential_signs_in_again() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    let worktree = git_worktree(&home);
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Done."}]"#);
    let wt = worktree.to_str().unwrap();
    let run = home.run(&["run", "t3", "--name", "r", "--cwd", wt, "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));
    assert_eq!(count_calls(&dir, "oauth token"), 1);

    std::fs::write(dir.join("revoke"), "").unwrap();
    let revoked = home.run(&["send", "r", "--wait", "Go on"]);
    assert!(revoked.status.success(), "{}", text(&revoked));
    assert_eq!(count_calls(&dir, "oauth token"), 2);

    let path = home.dir.path().join("state/t3-credential.json");
    let mut credential: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    credential["expires_at"] = Value::from(1);
    std::fs::write(&path, credential.to_string()).unwrap();
    // A running session keeps its token; a new one reads the expired credential.
    let expired = home.run(&["run", "t3", "--name", "e", "--cwd", wt, "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(expired.status.success(), "{}", text(&expired));
    assert_eq!(count_calls(&dir, "oauth token"), 3);
    assert_eq!(mcp_calls(&dir, "t3_thread_send").len(), 1);
}

#[cfg(unix)]
#[test]
fn a_t3_session_in_a_worktree_refuses_a_new_worktree() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    let worktree = git_worktree(&home);
    let run = home.run(&[
        "run",
        "t3",
        "--name",
        "b",
        "--cwd",
        worktree.to_str().unwrap(),
        "--provider",
        "claudeAgent",
        "--model",
        "m",
        "--worktree",
        "main",
        "Fix it",
    ]);
    assert!(!run.status.success(), "{}", text(&run));
    assert!(text(&run).contains("is already a git worktree of"), "{}", text(&run));
    assert!(mcp_calls(&dir, "t3_thread_launch").is_empty());
    assert_eq!(count_calls(&dir, "thread start"), 0);
}

#[cfg(unix)]
#[test]
fn a_failed_t3_launch_fails_the_turn_and_the_retry_asks_with_the_same_request_id() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    let worktree = git_worktree(&home);
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r2","text":"Done."}]"#);
    let wt = worktree.to_str().unwrap();

    std::fs::write(dir.join("pairing-fail"), "").unwrap();
    let unpaired = home.run(&["run", "t3", "--name", "f", "--cwd", wt, "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(!unpaired.status.success(), "{}", text(&unpaired));
    assert!(text(&unpaired).contains("Could not reach the T3 Code server"), "{}", text(&unpaired));
    assert!(mcp_calls(&dir, "t3_thread_launch").is_empty());
    std::fs::remove_file(dir.join("pairing-fail")).unwrap();

    std::fs::write(dir.join("launch-error"), "Project p1 has no worktree there").unwrap();
    let refused = home.run(&["send", "f", "--wait", "Fix it"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("T3 Code refused t3_thread_launch: Project p1 has no worktree there"), "{}", text(&refused));
    let record: Value = serde_json::from_str(&std::fs::read_to_string(home.dir.path().join("state/sessions/f.json")).unwrap()).unwrap();
    assert!(record.get("agent_session").is_none(), "{record}");
    std::fs::remove_file(dir.join("launch-error")).unwrap();

    let retried = home.run(&["send", "f", "--wait", "Fix it"]);
    assert!(retried.status.success(), "{}", text(&retried));
    let launches = mcp_calls(&dir, "t3_thread_launch");
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert_eq!(launches[0]["clientRequestId"], launches[1]["clientRequestId"]);
    assert_eq!(count_calls(&dir, "thread start"), 0);
}

#[cfg(unix)]
#[test]
fn settle_moves_a_finished_t3_thread_to_settled_and_keeps_one_that_needs_a_look() {
    let home = home(&[]);
    let dir = fake_t3(&home);
    std::fs::write(dir.join("start-exit"), "0").unwrap();
    show(&dir, "completed", "null", 0, r#"[{"role":"assistant","runId":"r1","text":"Done."}]"#);
    let run = home.run(&["run", "t3", "--name", "s", "--provider", "claudeAgent", "--model", "m", "Fix it"]);
    assert!(run.status.success(), "{}", text(&run));

    for (status, active, pending, why) in [
        ("completed", "null", 1, "waits for an answer"),
        ("running", "\"r2\"", 0, "still runs"),
        ("failed", "null", 0, "ended its last run with status 'failed'"),
    ] {
        show(&dir, status, active, pending, "[]");
        let kept = home.run(&["settle", "s"]);
        assert!(!kept.status.success(), "{}", text(&kept));
        assert!(text(&kept).contains(why) && text(&kept).contains("https://t3/t1"), "{}", text(&kept));
    }
    assert!(mcp_calls(&dir, "t3_thread_organize").is_empty());

    show(&dir, "completed", "null", 0, "[]");
    let settled = home.run(&["settle", "s"]);
    assert!(settled.status.success(), "{}", text(&settled));
    assert!(String::from_utf8_lossy(&settled.stdout).contains("https://t3/t1"), "{}", text(&settled));
    assert_eq!(mcp_calls(&dir, "t3_thread_organize"), vec![serde_json::json!({"threadId": "t1", "action": "settle"})]);
}
