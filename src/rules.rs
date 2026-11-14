//! Permission rules: commands, tools and write locations a session refuses or allows, decided before the policy.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rules applied to each permission request before the policy ([`crate::Profile::policy`]). Agents whose table entry
/// has `rules_meta` also get the tool lists and commands natively, for the tool calls they run without asking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    /// Shell commands refused: a command line with one of these at the start of a command (alone, or after `&&`,
    /// `||`, `;` or `|`) is denied, e.g. `git push`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_commands: Vec<String>,
    /// Tools allowed without the policy, by the name the agent gives them; a trailing `*` matches a prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    /// Tools refused, matched like `allow_tools`; refusing wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// Refuse edits, deletes and moves of paths outside the working directory and `add_dirs`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confine_writes: bool,
}

impl Rules {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// `Some((allow, why))` when a rule decides the request (the `requestPermission` params as JSON); `None` leaves it
    /// to the policy. `roots` are the working directory and the extra directories.
    pub fn decide(&self, request: &Value, roots: &[PathBuf]) -> Option<(bool, String)> {
        let call = request.get("toolCall").unwrap_or(&Value::Null);
        if let Some(line) = command(call)
            && let Some(denied) = self.deny_commands.iter().find(|denied| runs(&line, denied))
        {
            return Some((false, format!("rule deny_commands {denied}")));
        }
        let tool = tool_name(call);
        if let Some(pattern) = tool.as_deref().and_then(|tool| self.deny_tools.iter().find(|pattern| matches(pattern, tool))) {
            return Some((false, format!("rule deny_tools {pattern}")));
        }
        if self.confine_writes && writes(call) {
            let cwd = roots.first().map(PathBuf::as_path).unwrap_or(Path::new("/"));
            if let Some(outside) =
                paths(call).into_iter().map(|path| resolve(cwd, &path)).find(|path| !roots.iter().any(|root| path.starts_with(resolve(cwd, root))))
            {
                return Some((false, format!("rule confine_writes: {} is outside the session's directories", outside.display())));
            }
        }
        if let Some(pattern) = tool.as_deref().and_then(|tool| self.allow_tools.iter().find(|pattern| matches(pattern, tool))) {
            return Some((true, format!("rule allow_tools {pattern}")));
        }
        None
    }
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Whether one of the commands in `line` starts with the words of `denied`.
fn runs(line: &str, denied: &str) -> bool {
    let denied: Vec<&str> = denied.split_whitespace().collect();
    if denied.is_empty() {
        return false;
    }
    line.split(['&', '|', ';', '\n', '(', ')']).any(|part| {
        let words: Vec<&str> = part.split_whitespace().skip_while(|word| word.contains('=') && !word.starts_with('-')).collect();
        words.len() >= denied.len() && words[..denied.len()] == denied[..]
    })
}

/// The shell command line of an execute request: `rawInput.command` (a string or an argument list).
fn command(call: &Value) -> Option<String> {
    match call.pointer("/rawInput/command")? {
        Value::String(line) => Some(line.clone()),
        Value::Array(args) => Some(args.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

/// The agent's own tool name (`_meta.<agent>.toolName`, as the Claude adapter sends it), else the title.
fn tool_name(call: &Value) -> Option<String> {
    let from_meta = call.get("_meta").and_then(Value::as_object).and_then(|meta| meta.values().find_map(|agent| agent.get("toolName")?.as_str()));
    from_meta.or_else(|| call.get("title")?.as_str()).map(str::to_string)
}

fn writes(call: &Value) -> bool {
    matches!(call.get("kind").and_then(Value::as_str), Some("edit" | "delete" | "move"))
}

/// The paths a request touches: its locations and the usual path fields of its input.
fn paths(call: &Value) -> Vec<PathBuf> {
    let locations = call.get("locations").and_then(Value::as_array).into_iter().flatten().filter_map(|location| location.get("path")?.as_str());
    let input = ["file_path", "path", "notebook_path", "destination"].into_iter().filter_map(|key| call.get("rawInput")?.get(key)?.as_str());
    locations.chain(input).map(PathBuf::from).collect()
}

/// `path` made absolute against `cwd`, with `.` and `..` removed and the longest existing ancestor's symlinks resolved.
fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    let mut absolute = PathBuf::new();
    for component in cwd.join(path).components() {
        match component {
            Component::ParentDir => {
                absolute.pop();
            }
            Component::CurDir => {}
            other => absolute.push(other),
        }
    }
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else { return absolute };
        rest.push(name.to_os_string());
        existing = parent;
    }
    let mut resolved = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
    resolved.extend(rest.iter().rev());
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Rules {
        Rules { deny_commands: vec!["git push".into()], allow_tools: vec!["mcp__lotar__*".into()], deny_tools: vec!["WebFetch".into()], confine_writes: true }
    }

    fn bash(command: &str) -> Value {
        json!({"toolCall": {"kind": "execute", "title": command, "rawInput": {"command": command}}})
    }

    #[test]
    fn denied_commands_match_any_command_of_a_line() {
        let roots = [PathBuf::from("/work/repo")];
        for line in ["git push", "git push origin main", "cd x && git push", "FOO=1 git push -f", "make; git   push"] {
            assert_eq!(rules().decide(&bash(line), &roots).map(|(allow, _)| allow), Some(false), "{line}");
        }
        for line in ["git pushd", "echo git push", "git status"] {
            assert_eq!(rules().decide(&bash(line), &roots), None, "{line}");
        }
        let args = json!({"toolCall": {"kind": "execute", "rawInput": {"command": ["git", "push"]}}});
        assert_eq!(rules().decide(&args, &roots).map(|(allow, _)| allow), Some(false));
    }

    #[test]
    fn tools_match_by_the_agents_name_and_refusing_wins() {
        let roots = [PathBuf::from("/work/repo")];
        let mcp = json!({"toolCall": {"kind": "other", "title": "lotar: list", "_meta": {"claudeCode": {"toolName": "mcp__lotar__list"}}}});
        assert_eq!(rules().decide(&mcp, &roots), Some((true, "rule allow_tools mcp__lotar__*".into())));
        let fetch = json!({"toolCall": {"kind": "fetch", "title": "WebFetch"}});
        assert_eq!(rules().decide(&fetch, &roots), Some((false, "rule deny_tools WebFetch".into())));
        assert_eq!(rules().decide(&json!({"toolCall": {"kind": "read", "title": "Read"}}), &roots), None);
    }

    #[test]
    fn writes_outside_the_directories_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let extra = dir.path().join("shared");
        std::fs::create_dir_all(&cwd).unwrap();
        let roots = [cwd.clone(), extra.clone()];
        let edit = |path: &str| json!({"toolCall": {"kind": "edit", "title": "Edit", "locations": [{"path": path}]}});
        assert_eq!(rules().decide(&edit("src/a.rs"), &roots), None);
        assert_eq!(rules().decide(&edit(&extra.join("x").display().to_string()), &roots), None);
        let (allow, why) = rules().decide(&edit("../elsewhere/a.rs"), &roots).unwrap();
        assert!(!allow && why.contains("elsewhere"), "{why}");
        let raw = json!({"toolCall": {"kind": "delete", "rawInput": {"file_path": "/etc/hosts"}}});
        assert_eq!(rules().decide(&raw, &roots).map(|(allow, _)| allow), Some(false));
        let read = json!({"toolCall": {"kind": "read", "locations": [{"path": "/etc/hosts"}]}});
        assert_eq!(rules().decide(&read, &roots), None, "reads are not confined");
    }
}
