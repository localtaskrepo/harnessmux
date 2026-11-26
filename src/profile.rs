//! What a session runs with: agent, model, effort, permissions, MCP servers, extra directories and environment.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::rules::Rules;

macro_rules! named_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(raw: &str) -> Result<Self> {
                match raw {
                    $($text => Ok($name::$variant),)+
                    _ => Err(Error::Invalid(format!(
                        "{raw} is not a valid {}; use one of: {}",
                        stringify!($name).to_lowercase(),
                        Self::ALL.iter().map(|value| value.as_str()).collect::<Vec<_>>().join(", ")
                    ))),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

named_enum!(
    /// How much the agent may change: read-only, workspace (the working directory and `add_dirs`), full.
    #[derive(Default)]
    Permissions { ReadOnly = "read-only", #[default] Workspace = "workspace", Full = "full" }
);

named_enum!(
    /// Reasoning effort, mapped onto each agent's own values by the agent table.
    Effort { Low = "low", Medium = "medium", High = "high", Xhigh = "xhigh", Max = "max" }
);

named_enum!(
    /// What happens to a permission request from the agent: deny it, allow only reads, allow it, or wait for `answer`.
    PermissionPolicy { Deny = "deny", AllowReads = "allow-reads", Allow = "allow", Ask = "ask" }
);

impl Permissions {
    /// The policy used when the caller sets none: read-only allows reads; workspace and full allow, because the
    /// agent's mode (plan, accept edits, sandbox) already limits what it asks for.
    pub fn default_policy(self) -> PermissionPolicy {
        match self {
            Self::ReadOnly => PermissionPolicy::AllowReads,
            Self::Workspace | Self::Full => PermissionPolicy::Allow,
        }
    }
}

/// An MCP server handed to the agent with the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServer {
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

/// `{"mcpServers": {"name": {...}}}`, the format Claude, Cursor and others use, or the bare map.
pub fn parse_mcp_file(text: &str) -> Result<BTreeMap<String, McpServer>> {
    #[derive(Deserialize)]
    struct Wrapped {
        #[serde(rename = "mcpServers")]
        mcp_servers: BTreeMap<String, McpServer>,
    }
    serde_json::from_str::<Wrapped>(text)
        .map(|wrapped| wrapped.mcp_servers)
        .or_else(|_| serde_json::from_str::<BTreeMap<String, McpServer>>(text))
        .map_err(|error| Error::Invalid(format!("MCP server file: {error}; expected {{\"mcpServers\": {{\"name\": {{\"command\": …}} | {{\"url\": …}}}}}}")))
}

/// Everything a session starts with. lotar converts its agent profiles into this.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Model id as the agent lists it; rejected when the agent advertises models and this is not one of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default)]
    pub permissions: Permissions,
    /// Defaults to [`Permissions::default_policy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_permission: Option<PermissionPolicy>,
    /// Commands, tools and write locations decided before the policy.
    #[serde(default, skip_serializing_if = "Rules::is_empty")]
    pub rules: Rules,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServer>,
    /// Extra directories the agent may use (ACP `additionalDirectories`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_dirs: Vec<PathBuf>,
    /// Environment for the agent's process, on top of the inherited one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Other ACP config options, set after the session starts: option id to value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// t3 only: the T3 Code provider instance the model runs on (`claudeAgent`, `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// t3 only: start the thread in a new git worktree based on this branch, tag or commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_base: Option<String>,
    /// t3 only: the branch for that worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// t3 only: the linked T3 Code environment the thread runs on, by id or label (`t3 environment list`); none: the
    /// T3 Code server on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Session title (t3 thread title; ignored by agents without titles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// t3 only: a JSON Schema the turn's result must match. T3 Code has the agent submit it (`t3_submit_output`); the
    /// result comes as a `turn.output` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

impl Profile {
    pub fn policy(&self) -> PermissionPolicy {
        self.on_permission.unwrap_or_else(|| self.permissions.default_policy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_name_the_valid_values() {
        assert_eq!("read-only".parse::<Permissions>().unwrap(), Permissions::ReadOnly);
        assert_eq!("xhigh".parse::<Effort>().unwrap(), Effort::Xhigh);
        let err = "ultra".parse::<Effort>().unwrap_err().to_string();
        assert_eq!(err, "ultra is not a valid effort; use one of: low, medium, high, xhigh, max");
        assert_eq!(Permissions::ReadOnly.default_policy(), PermissionPolicy::AllowReads);
    }

    #[test]
    fn mcp_files_take_both_shapes() {
        let wrapped = parse_mcp_file(r#"{"mcpServers":{"lotar":{"command":"lotar","args":["mcp"]},"web":{"url":"http://localhost:1/mcp"}}}"#).unwrap();
        assert_eq!(wrapped["lotar"], McpServer::Stdio { command: "lotar".into(), args: vec!["mcp".into()], env: BTreeMap::new() });
        assert!(matches!(wrapped["web"], McpServer::Http { .. }));
        let bare = parse_mcp_file(r#"{"x":{"command":"x"}}"#).unwrap();
        assert_eq!(bare.len(), 1);
        assert!(parse_mcp_file("[]").is_err());
    }
}
