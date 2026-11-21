//! Run coding-agent CLI sessions over the Agent Client Protocol (ACP).
//!
//! A [`Session`] is a name plus a working directory and an agent from the [`AgentTable`]. The process that starts or
//! resumes it owns it: it holds the agent's ACP connection, queues prompts, writes the [`Event`] log and serves a local
//! socket, so other processes (`harnessmux send`, `attach`, `cancel`) reach it. One [`Profile`] maps model, effort and
//! permissions onto each agent's own options.
//!
//! ```no_run
//! # async fn run() -> harnessmux::Result<()> {
//! use harnessmux::{AgentTable, Profile, Registry, Session, StartOptions};
//!
//! let table = AgentTable::load()?;
//! let options = StartOptions {
//!     name: Some("DEV-1".into()),
//!     agent: "claude".into(),
//!     cwd: "/work/repo".into(),
//!     profile: Profile { effort: Some(harnessmux::Effort::High), ..Profile::default() },
//!     idle_ttl: None,
//! };
//! let session = Session::start(Registry::default(), &table, options).await?;
//! let mut events = session.events();
//! let turn = session.send("Fix the failing test").await?;
//! while let Ok(event) = events.recv().await {
//!     if event.turn == Some(turn) && event.body.ends_turn() {
//!         break;
//!     }
//! }
//! session.close().await?;
//! # Ok(())
//! # }
//! ```

pub mod acp_map;
pub mod agents;
pub mod backend;
pub mod capabilities;
pub mod client;
pub mod error;
pub mod event;
pub mod failure;
pub mod follow;
pub mod ipc;
pub mod mcp;
pub mod paths;
pub mod profile;
pub mod registry;
pub mod replay;
pub mod rules;
pub mod session;

pub use agents::{AgentKind, AgentSpec, AgentTable};
pub use error::{Error, Result};
pub use event::{Event, EventBody, Failure, FailureKind, OUTPUT_LIMIT, PlanEntry, RateLimit, StopReason, Usage};
pub use profile::{Effort, McpServer, PermissionPolicy, Permissions, Profile};
pub use registry::{Registry, SessionRecord, SessionState};
pub use rules::Rules;
pub use session::{Session, StartOptions, Status};
