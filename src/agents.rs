//! The agent table (`agents.toml`): how each agent starts and which ids its options have.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::paths;
use crate::profile::{Effort, Permissions};
use crate::rules::Rules;

/// The table shipped with this release.
pub const BUILTIN: &str = include_str!("../agents.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// An ACP server on stdio.
    Acp,
    /// Amp's headless `--stream-json` mode.
    Amp,
    /// T3 Code threads through `t3 thread`.
    T3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adapter {
    pub npm: String,
    pub version: String,
    pub bin: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// Where an agent takes [`Rules`] natively in a session's `_meta`: dotted paths of the tool lists and the tool pattern
/// a denied command becomes (`{}` is the command).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_tools: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_tools: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    #[serde(default, skip_deserializing)]
    pub name: String,
    pub kind: AgentKind,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Adapter>,
    /// The CLI the agent needs besides the adapter (for sign-in), checked by `capabilities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub docs: String,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub options: OptionIds,
    #[serde(default)]
    pub effort: BTreeMap<String, String>,
    #[serde(default)]
    pub modes: BTreeMap<String, String>,
    /// harnessmux permissions -> the `_meta` sent when a session opens (Claude's sandbox at workspace).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_meta: Option<RulesMeta>,
}

impl AgentSpec {
    pub fn effort_value(&self, effort: Effort) -> Option<&str> {
        self.effort.get(effort.as_str()).map(String::as_str)
    }

    pub fn mode_value(&self, permissions: Permissions) -> Option<&str> {
        self.modes.get(permissions.as_str()).map(String::as_str)
    }

    /// The `_meta` a session opens with: the table's `meta` for the permissions, with the rules' tool lists and
    /// denied commands added where `rules_meta` puts them.
    pub fn session_meta(&self, permissions: Permissions, rules: &Rules) -> Option<serde_json::Map<String, Value>> {
        let mut meta = self.meta.get(permissions.as_str()).cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if let Some(native) = &self.rules_meta {
            if let Some(path) = &native.allow_tools {
                append(&mut meta, path, rules.allow_tools.iter().cloned());
            }
            if let Some(path) = &native.deny_tools {
                let commands = native.command.iter().flat_map(|pattern| rules.deny_commands.iter().map(move |command| pattern.replace("{}", command)));
                append(&mut meta, path, rules.deny_tools.iter().cloned().chain(commands));
            }
        }
        match meta {
            Value::Object(map) if !map.is_empty() => Some(map),
            _ => None,
        }
    }

    /// Where the pinned adapter is installed: `<data dir>/adapters/<bin>-<version>`.
    pub fn adapter_dir(&self) -> Option<PathBuf> {
        self.adapter.as_ref().map(|adapter| paths::data_dir().join("adapters").join(format!("{}-{}", adapter.bin, adapter.version)))
    }

    fn adapter_program(&self) -> Option<PathBuf> {
        let adapter = self.adapter.as_ref()?;
        let bin = if cfg!(windows) { format!("{}.cmd", adapter.bin) } else { adapter.bin.clone() };
        Some(self.adapter_dir()?.join("node_modules").join(".bin").join(bin))
    }

    /// The program to start: the installed pinned adapter, else the command's program on PATH or in `paths`.
    pub fn resolve_program(&self) -> Option<PathBuf> {
        if let Some(program) = self.adapter_program().filter(|program| program.exists()) {
            return Some(program);
        }
        paths::find_program(self.command.first()?, &self.paths)
    }

    /// The program and its arguments. An agent with an adapter runs the pinned version, installed on first use; the
    /// command's program on PATH is used only when npm is missing.
    pub async fn launch_command(&self) -> Result<(PathBuf, Vec<String>)> {
        let args = self.command.iter().skip(1).cloned().collect();
        if let Some(adapter) = &self.adapter {
            if self.adapter_program().is_some_and(|program| !program.exists()) && paths::find_program("npm", &[]).is_some() {
                install_adapter(self, adapter).await?;
            }
            if let Some(program) = self.adapter_program().filter(|program| program.exists()) {
                return Ok((program, args));
            }
        }
        let program = self.command.first().ok_or_else(|| Error::Invalid(format!("agent {} has an empty command in the agent table", self.name)))?;
        let found = paths::find_program(program, &self.paths).ok_or_else(|| {
            let hint = match (&self.adapter, &self.requires) {
                (Some(adapter), _) => format!(" Installing its adapter {}@{} needs npm (Node.js).", adapter.npm, adapter.version),
                (None, Some(cli)) => format!(" Install {cli} and sign in."),
                (None, None) => String::new(),
            };
            Error::Agent(format!("{program} for agent {} is not installed or not on PATH.{hint} See {}", self.name, self.docs))
        })?;
        Ok((found, args))
    }
}

/// Appends `items` to the list at the dotted `path` of `value`, creating the objects and the list on the way.
fn append(value: &mut Value, path: &str, items: impl Iterator<Item = String>) {
    let items: Vec<Value> = items.map(Value::String).collect();
    if items.is_empty() {
        return;
    }
    let mut at = value;
    for key in path.split('.') {
        if !at.is_object() {
            *at = Value::Object(serde_json::Map::new());
        }
        at = at.as_object_mut().expect("made an object above").entry(key).or_insert(Value::Null);
    }
    match at {
        Value::Array(list) => list.extend(items),
        other => *other = Value::Array(items),
    }
}

async fn install_adapter(spec: &AgentSpec, adapter: &Adapter) -> Result<()> {
    let dir = spec.adapter_dir().ok_or_else(|| Error::Invalid(format!("agent {} has no adapter", spec.name)))?;
    let npm = paths::find_program("npm", &[]).ok_or_else(|| Error::Agent(format!("installing {}@{} needs npm (Node.js)", adapter.npm, adapter.version)))?;
    std::fs::create_dir_all(&dir).map_err(|error| Error::io(format!("creating {}", dir.display()), error))?;
    eprintln!("harnessmux: installing {}@{} into {}", adapter.npm, adapter.version, dir.display());
    let output = tokio::process::Command::new(npm)
        .args(["install", "--no-save", "--no-fund", "--no-audit", "--loglevel", "error", "--prefix"])
        .arg(&dir)
        .arg(format!("{}@{}", adapter.npm, adapter.version))
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|error| Error::io("running npm install", error))?;
    if !output.status.success() {
        return Err(Error::Agent(format!("npm install {}@{} failed: {}", adapter.npm, adapter.version, String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TableFile {
    #[serde(default)]
    agents: BTreeMap<String, toml::Table>,
}

/// All agents by name.
#[derive(Debug, Clone)]
pub struct AgentTable {
    agents: BTreeMap<String, AgentSpec>,
}

impl AgentTable {
    /// The built-in table with the user's file merged over it (per agent, per key).
    pub fn load() -> Result<Self> {
        let user = paths::config_dir().join("agents.toml");
        let overrides = match std::fs::read_to_string(&user) {
            Ok(text) => Some((user, text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::io(format!("reading {}", user.display()), error)),
        };
        Self::parse(BUILTIN, overrides.as_ref().map(|(path, text)| (path.display().to_string(), text.as_str())))
    }

    pub fn parse(builtin: &str, overrides: Option<(String, &str)>) -> Result<Self> {
        let parse = |origin: &str, text: &str| toml::from_str::<TableFile>(text).map_err(|error| Error::Invalid(format!("{origin}: {error}")));
        let mut merged = parse("built-in agents.toml", builtin)?.agents;
        if let Some((origin, text)) = overrides {
            for (name, table) in parse(&origin, text)?.agents {
                let entry = merged.entry(name).or_default();
                // A command of the user's own replaces the built-in pinned adapter.
                if table.contains_key("command") && !table.contains_key("adapter") {
                    entry.remove("adapter");
                }
                entry.extend(table);
            }
        }
        let mut agents = BTreeMap::new();
        for (name, table) in merged {
            let mut spec: AgentSpec = toml::Value::Table(table).try_into().map_err(|error| Error::Invalid(format!("agent {name} in agents.toml: {error}")))?;
            if spec.command.is_empty() {
                return Err(Error::Invalid(format!("agent {name} in agents.toml needs a command")));
            }
            spec.name = name.clone();
            agents.insert(name, spec);
        }
        Ok(Self { agents })
    }

    /// Adds an agent or replaces the one of the same name, such as a copy of a built-in agent with another command. A
    /// command of one's own replaces the pinned adapter only when the caller clears `adapter`, as in `agents.toml`.
    pub fn insert(&mut self, spec: AgentSpec) -> Result<()> {
        if spec.name.is_empty() {
            return Err(Error::Invalid("an agent needs a name".into()));
        }
        if spec.command.is_empty() {
            return Err(Error::Invalid(format!("agent {} needs a command", spec.name)));
        }
        self.agents.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&AgentSpec> {
        self.agents.get(name).ok_or_else(|| Error::Invalid(format!("unknown agent {name}; known agents: {}", self.names().join(", "))))
    }

    pub fn names(&self) -> Vec<&str> {
        self.agents.keys().map(String::as_str).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &AgentSpec> {
        self.agents.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_table_has_every_agent_lotar_runs() {
        let table = AgentTable::parse(BUILTIN, None).unwrap();
        for name in ["claude", "codex", "copilot", "gemini", "cursor", "amp", "droid", "opencode", "kilo", "antigravity", "t3"] {
            assert!(table.get(name).is_ok(), "{name} missing");
        }
        let codex = table.get("codex").unwrap();
        assert_eq!(codex.adapter.as_ref().unwrap().version, "2.6.0");
        assert_eq!(codex.effort_value(Effort::Max), Some("max"));
        assert_eq!(codex.mode_value(Permissions::Workspace), Some("workspace-write"));
        assert_eq!(table.get("amp").unwrap().kind, AgentKind::Amp);
    }

    #[test]
    fn a_user_file_overrides_single_keys_and_adds_agents() {
        let user = r#"
[agents.codex]
adapter = { npm = "@agentclientprotocol/codex-acp", version = "2.7.0", bin = "codex-acp" }

[agents.mine]
kind = "acp"
command = ["my-agent", "--acp"]
"#;
        let table = AgentTable::parse(BUILTIN, Some(("user".into(), user))).unwrap();
        let codex = table.get("codex").unwrap();
        assert_eq!(codex.adapter.as_ref().unwrap().version, "2.7.0");
        assert_eq!(codex.options.effort.as_deref(), Some("reasoning_effort"));
        assert_eq!(table.get("mine").unwrap().command, ["my-agent", "--acp"]);
        let own = AgentTable::parse(BUILTIN, Some(("user".into(), "[agents.claude]\ncommand = [\"/opt/claude-agent-acp\"]"))).unwrap();
        assert_eq!(own.get("claude").unwrap().adapter, None, "a command of one's own runs instead of the pinned adapter");
        assert_eq!(own.get("claude").unwrap().options.effort.as_deref(), Some("effort"));
        let err = table.get("nope").unwrap_err().to_string();
        assert!(err.contains("known agents: amp, antigravity"), "{err}");
    }

    #[test]
    fn claude_opens_workspace_sessions_sandboxed_and_takes_rules_natively() {
        let table = AgentTable::parse(BUILTIN, None).unwrap();
        let claude = table.get("claude").unwrap();
        let rules =
            Rules { deny_commands: vec!["git push".into()], allow_tools: vec!["mcp__lotar".into()], deny_tools: vec!["WebFetch".into()], confine_writes: true };
        let meta = Value::Object(claude.session_meta(Permissions::Workspace, &rules).unwrap());
        let options = &meta["claudeCode"]["options"];
        assert_eq!(options["settings"]["sandbox"], serde_json::json!({"enabled": true, "autoAllowBashIfSandboxed": true}));
        assert_eq!(options["allowedTools"], serde_json::json!(["mcp__lotar"]));
        assert_eq!(options["disallowedTools"], serde_json::json!(["WebFetch", "Bash(git push:*)"]));
        let full = Value::Object(claude.session_meta(Permissions::Full, &Rules::default()).unwrap_or_default());
        assert_eq!(full.get("claudeCode"), None, "full permissions run unsandboxed");
        assert_eq!(table.get("codex").unwrap().session_meta(Permissions::Workspace, &rules), None);
    }

    #[test]
    fn insert_adds_or_replaces_one_agent() {
        let mut table = AgentTable::parse(BUILTIN, None).unwrap();
        let mut claude = table.get("claude").unwrap().clone();
        claude.command = vec!["/opt/claude-agent-acp".into(), "--debug".into()];
        claude.adapter = None;
        table.insert(claude).unwrap();
        let claude = table.get("claude").unwrap();
        assert_eq!(claude.command, ["/opt/claude-agent-acp", "--debug"]);
        assert_eq!(claude.adapter, None);
        assert_eq!(claude.options.effort.as_deref(), Some("effort"), "the other keys stay");

        let mut mine = table.get("codex").unwrap().clone();
        mine.name = "mine".into();
        table.insert(mine.clone()).unwrap();
        assert!(table.names().contains(&"mine"));

        mine.command.clear();
        let err = table.insert(mine).unwrap_err().to_string();
        assert!(err.contains("agent mine needs a command"), "{err}");
    }

    #[test]
    fn a_broken_user_file_names_itself() {
        let err = AgentTable::parse(BUILTIN, Some(("/home/u/agents.toml".into(), "[agents.x]\nkind = 3"))).unwrap_err().to_string();
        assert!(err.contains("agent x"), "{err}");
    }
}
