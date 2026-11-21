//! `capabilities`: which agents are installed, their adapters, and what each one offers.

use serde_json::{Value, json};

use crate::agents::{AgentKind, AgentSpec, AgentTable};
use crate::error::Result;
use crate::paths;
use crate::profile::{Effort, Permissions};

/// What the agent table says, plus whether the agent's programs are installed. Starts nothing.
pub fn summary(spec: &AgentSpec) -> Value {
    let program = spec.resolve_program();
    let requires = spec.requires.as_deref().map(|cli| json!({ "program": cli, "installed": paths::find_program(cli, &spec.paths).is_some() }));
    let adapter = spec.adapter.as_ref().map(|adapter| {
        json!({
            "package": adapter.npm,
            "version": adapter.version,
            "installed": spec.adapter_dir().is_some_and(|dir| dir.join("node_modules").exists()),
        })
    });
    let effort: Vec<&str> = Effort::ALL.iter().filter(|e| spec.effort_value(**e).is_some()).map(|e| e.as_str()).collect();
    let modes: serde_json::Map<String, Value> =
        Permissions::ALL.iter().filter_map(|p| spec.mode_value(*p).map(|mode| (p.as_str().to_string(), json!(mode)))).collect();
    let mut value = json!({
        "agent": spec.name,
        "kind": spec.kind,
        "command": spec.command,
        "program": program.map(|p| p.display().to_string()),
        "requires": requires,
        "adapter": adapter,
        "effort": effort,
        "modes": modes,
        "live_checked": spec.live,
        "docs": spec.docs,
    });
    let legacy = match spec.kind {
        AgentKind::Acp => None,
        AgentKind::Amp => Some(json!({
            "resume": false, "load": false, "cancel": true, "answer": false, "set": false, "model": false, "effort": false,
            "permissions": ["workspace", "full"], "messages_while_running": false,
            "note": "Amp has no ACP server; each turn runs `amp --stream-json -x`, later turns `amp threads continue`",
        })),
        AgentKind::T3 => Some(json!({
            "resume": true, "load": false, "cancel": false, "answer": false, "set": false, "model": "with --provider", "effort": "with --provider and --model",
            "child_threads": false, "worktree": true, "permissions": "T3 Code's own runtime mode", "messages_while_running": "queued by T3",
            "note": "T3 Code threads through `t3 thread start/send/show`; effort and options go as `--option id=value` with a provider and model; T3's CLI has no child threads, interrupt or answers",
        })),
    };
    if let (Some(legacy), Some(map)) = (legacy, value.as_object_mut()) {
        map.insert("offers".into(), legacy);
    }
    value
}

pub fn all(table: &AgentTable) -> Vec<Value> {
    table.iter().map(summary).collect()
}

/// The summary plus, for an ACP agent, what it reports when started: resume, load, fork, its config options.
pub async fn probe(spec: &AgentSpec) -> Result<Value> {
    if spec.kind != AgentKind::Acp {
        return Ok(summary(spec));
    }
    let dir = std::env::temp_dir().join(format!("harnessmux-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|error| crate::error::Error::io(format!("creating {}", dir.display()), error))?;
    let offers = crate::backend::acp::probe(spec, &dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    // After the probe: it installs a missing adapter.
    let mut value = summary(spec);
    if let Some(map) = value.as_object_mut() {
        map.insert("offers".into(), offers?);
    }
    Ok(value)
}
