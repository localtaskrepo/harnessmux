//! Recorded ACP sessions: `--record FILE` writes every line between harnessmux and the agent; `harnessmux replay FILE`
//! is an ACP agent on stdio that plays such a file back. Tests (ours and callers') run sessions without a real agent.
//!
//! A recording is JSONL: `{"to":"agent","msg":{…}}` for what the client sent, `{"to":"client","msg":{…}}` for what the
//! agent sent, in wire order. Replay waits for each client message in turn (matched by method, or as a response), then
//! sends the agent's messages that follow it, with response ids mapped to the ids the client used.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// agent: the client sent it; client: the agent sent it.
    pub to: String,
    pub msg: Value,
}

/// Appends wire lines to a recording file.
#[derive(Clone)]
pub struct Recorder(Arc<Mutex<std::fs::File>>);

impl Recorder {
    pub fn create(path: &Path) -> Result<Self> {
        let file = std::fs::File::create(path).map_err(|error| Error::io(format!("creating {}", path.display()), error))?;
        Ok(Self(Arc::new(Mutex::new(file))))
    }

    pub fn line(&self, to_agent: bool, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        let entry = Entry { to: if to_agent { "agent" } else { "client" }.into(), msg };
        if let (Ok(mut file), Ok(text)) = (self.0.lock(), serde_json::to_string(&entry)) {
            let _ = writeln!(file, "{text}");
        }
    }
}

pub fn load(path: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(format!("reading {}", path.display()), error))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(n, line)| serde_json::from_str(line).map_err(|error| Error::Invalid(format!("{}:{}: {error}", path.display(), n + 1))))
        .collect()
}

fn method(msg: &Value) -> Option<&str> {
    msg.get("method").and_then(Value::as_str)
}

fn is_response(msg: &Value) -> bool {
    method(msg).is_none() && msg.get("id").is_some()
}

/// Plays `entries` back on stdin and stdout until the recording ends or the client hangs up.
pub async fn serve(entries: Vec<Entry>) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    // Recorded client request id (as JSON text) -> the id the client uses now.
    let mut ids: HashMap<String, Value> = HashMap::new();
    let mut index = 0;
    while index < entries.len() {
        let entry = &entries[index];
        if entry.to == "client" {
            let mut msg = entry.msg.clone();
            if is_response(&msg)
                && let Some(id) = msg.get("id").map(Value::to_string)
                && let Some(actual) = ids.get(&id)
            {
                msg["id"] = actual.clone();
            }
            let mut line = msg.to_string();
            line.push('\n');
            output.write_all(line.as_bytes()).await.map_err(|error| Error::io("writing to the client", error))?;
            output.flush().await.map_err(|error| Error::io("writing to the client", error))?;
            index += 1;
            continue;
        }
        // Wait for the client's next message that matches this one.
        let Some(line) = input.next_line().await.map_err(|error| Error::io("reading from the client", error))? else { return Ok(()) };
        let Ok(got) = serde_json::from_str::<Value>(&line) else { continue };
        let expected = &entry.msg;
        let matches = match (method(expected), method(&got)) {
            (Some(want), Some(have)) => want == have,
            (None, None) => is_response(&got),
            _ => false,
        };
        if !matches {
            if method(&got).is_some()
                && let Some(id) = got.get("id")
            {
                let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": format!("replay expected {}", method(expected).unwrap_or("a response"))}});
                let mut text = error.to_string();
                text.push('\n');
                output.write_all(text.as_bytes()).await.map_err(|error| Error::io("writing to the client", error))?;
                output.flush().await.map_err(|error| Error::io("writing to the client", error))?;
            }
            continue;
        }
        if method(expected).is_some()
            && let (Some(recorded), Some(actual)) = (expected.get("id"), got.get("id"))
        {
            ids.insert(recorded.to_string(), actual.clone());
        }
        index += 1;
    }
    // Keep stdin open until the client hangs up, as an agent does.
    while let Ok(Some(_)) = input.next_line().await {}
    Ok(())
}
