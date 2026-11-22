#![allow(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]
//! [`Session::resume`] through the library, with `harnessmux replay` as the agent: a refused `session/resume` or
//! `session/load` is [`Error::NotResumable`], any other refusal while the session starts is [`Error::Protocol`].

use harnessmux::{AgentTable, Error, Profile, Registry, Session, SessionRecord};
use serde_json::{Value, json};

const INIT_ID: &str = "init";
const OPEN_ID: &str = "open";

/// The agent's answer to `initialize`, able to resume (`session/resume`) or only to load (`session/load`) sessions.
fn initialized(resume: bool) -> Value {
    let capabilities = if resume { json!({"sessionCapabilities": {"resume": {}}}) } else { json!({"loadSession": true}) };
    json!({"jsonrpc": "2.0", "id": INIT_ID, "result": {"protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": []}})
}

fn refused(id: &str, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32002, "message": message}})
}

/// Resumes session `s` (agent session `a-1`) of an agent that replays `recording` (pairs of what harnessmux sends and
/// what the agent answers).
async fn resume(recording: &[(&str, Value)]) -> Result<Session, Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.jsonl");
    let mut lines = String::new();
    for (method, answer) in recording {
        let id = answer["id"].clone();
        lines.push_str(&json!({"to": "agent", "msg": {"jsonrpc": "2.0", "id": id, "method": method}}).to_string());
        lines.push('\n');
        lines.push_str(&json!({"to": "client", "msg": answer}).to_string());
        lines.push('\n');
    }
    std::fs::write(&path, lines).unwrap();
    let bin = env!("CARGO_BIN_EXE_harnessmux");
    let table = format!("[agents.fake]\nkind = \"acp\"\ncommand = [{bin:?}, \"replay\", {:?}]\nlive = false\n", path.display().to_string());
    let table = AgentTable::parse("", Some(("test".into(), &table))).unwrap();
    let registry = Registry::new(dir.path().join("state"));
    let mut record = SessionRecord::new("s", "fake", dir.path(), Profile::default()).unwrap();
    record.agent_session = Some("a-1".into());
    registry.save(&record).unwrap();
    let result = Session::resume(registry, &table, "s", None).await;
    if let Ok(session) = &result {
        let _ = session.close().await;
    }
    result
}

#[tokio::test]
async fn a_refused_session_resume_is_not_resumable() {
    let error = resume(&[("initialize", initialized(true)), ("session/resume", refused(OPEN_ID, "Resource not found: a-1"))]).await.err().unwrap();
    assert!(matches!(&error, Error::NotResumable(message) if message.contains("Resource not found: a-1")), "{error:?}");
}

#[tokio::test]
async fn a_refused_session_load_is_not_resumable() {
    let error = resume(&[("initialize", initialized(false)), ("session/load", refused(OPEN_ID, "Resource not found: a-1"))]).await.err().unwrap();
    assert!(matches!(&error, Error::NotResumable(message) if message.contains("Resource not found: a-1")), "{error:?}");
}

#[tokio::test]
async fn a_refused_initialize_on_resume_is_a_protocol_error() {
    let error = resume(&[("initialize", refused(INIT_ID, "Authentication required"))]).await.err().unwrap();
    assert!(matches!(&error, Error::Protocol(message) if message.contains("Authentication required")), "{error:?}");
}
