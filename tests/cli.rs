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
    assert_eq!(t3["offers"]["effort"], false);
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
}
