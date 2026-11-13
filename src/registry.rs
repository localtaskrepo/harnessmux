//! The session registry: one JSON file per session under `<state dir>/sessions`, next to its event log.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::event;
use crate::paths;
use crate::profile::Profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// No turn runs; the next `send` starts one (and an owner, when none runs).
    Idle,
    Running,
    /// A permission request waits for `answer`.
    Waiting,
    /// Closed: `send` and `resume` refuse it.
    Ended,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub name: String,
    pub agent: String,
    pub cwd: PathBuf,
    /// The repository the session belongs to: the main checkout of `cwd`'s git repository, also for a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    /// The agent's session id (ACP), thread id (T3, Amp); set once the agent created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<String>,
    /// The adapter the session was created with (`@agentclientprotocol/codex-acp@2.6.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default)]
    pub profile: Profile,
    pub created: String,
    pub last_used: String,
    pub state: SessionState,
    /// The process holding the agent connection, while one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    /// Turns started so far; the next turn gets `turns + 1`.
    #[serde(default)]
    pub turns: u64,
    /// Events written so far; the next event gets `events + 1`.
    #[serde(default)]
    pub events: u64,
}

impl SessionRecord {
    pub fn new(name: &str, agent: &str, cwd: &Path, profile: Profile) -> Result<Self> {
        validate_name(name)?;
        let now = event::now();
        Ok(Self {
            name: name.to_string(),
            agent: agent.to_string(),
            cwd: cwd.to_path_buf(),
            project: project_of(cwd),
            agent_session: None,
            adapter: None,
            profile,
            created: now.clone(),
            last_used: now,
            state: SessionState::Idle,
            owner_pid: None,
            turns: 0,
            events: 0,
        })
    }

    pub fn project_name(&self) -> Option<String> {
        self.project.as_ref().map(|project| project.display().to_string())
    }
}

/// Session names are file names and socket names: letters, digits, `.`, `_` and `-`, at most 64, not starting with `.`.
pub fn validate_name(name: &str) -> Result<()> {
    let valid =
        !name.is_empty() && name.len() <= 64 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid { Ok(()) } else { Err(Error::Invalid(format!("session name {name:?}: use 1 to 64 letters, digits, '.', '_' or '-', not starting with '.'"))) }
}

/// The main checkout of the git repository `cwd` is in, from git's common dir, so a worktree maps to its repository.
pub fn project_of(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    // A bare repository is its own project.
    Some(if common.file_name().is_some_and(|name| name == ".git") { common.parent()?.to_path_buf() } else { common })
}

/// The registry under a state directory (normally [`paths::state_dir`]).
#[derive(Debug, Clone)]
pub struct Registry {
    root: PathBuf,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(paths::state_dir())
    }
}

impl Registry {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { root: state_dir }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn record_path(&self, name: &str) -> PathBuf {
        self.sessions_dir().join(format!("{name}.json"))
    }

    pub fn events_path(&self, name: &str) -> PathBuf {
        self.sessions_dir().join(format!("{name}.events.jsonl"))
    }

    /// The owner's socket: a file in `<state dir>/sockets` on Unix, a named pipe on Windows. Unix socket paths hold
    /// about 100 bytes, so a deep state directory gets `/tmp/harnessmux-<hash>/` instead.
    pub fn socket_name(&self, name: &str) -> String {
        // The state directory keeps two homes (tests, users) apart where names are shared.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.root.to_string_lossy().bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        if cfg!(windows) {
            return format!("harnessmux-{hash:016x}-{name}");
        }
        let path = self.root.join("sockets").join(format!("{name}.sock")).display().to_string();
        if path.len() <= 100 { path } else { format!("/tmp/harnessmux-{hash:016x}/{name}.sock") }
    }

    pub fn exists(&self, name: &str) -> bool {
        self.record_path(name).exists()
    }

    pub fn load(&self, name: &str) -> Result<SessionRecord> {
        validate_name(name)?;
        let path = self.record_path(name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(Error::NoSession(name.to_string())),
            Err(error) => return Err(Error::io(format!("reading {}", path.display()), error)),
        };
        serde_json::from_str(&text).map_err(|error| Error::Invalid(format!("{}: {error}", path.display())))
    }

    pub fn save(&self, record: &SessionRecord) -> Result<()> {
        let dir = self.sessions_dir();
        std::fs::create_dir_all(&dir).map_err(|error| Error::io(format!("creating {}", dir.display()), error))?;
        let path = self.record_path(&record.name);
        let tmp = dir.join(format!(".{}.{}.tmp", record.name, std::process::id()));
        let text = serde_json::to_string_pretty(record).map_err(|error| Error::Invalid(error.to_string()))?;
        std::fs::write(&tmp, text + "\n").map_err(|error| Error::io(format!("writing {}", tmp.display()), error))?;
        std::fs::rename(&tmp, &path).map_err(|error| Error::io(format!("writing {}", path.display()), error))
    }

    /// Every session, oldest first; with a project, only that project's sessions.
    pub fn list(&self, project: Option<&Path>) -> Result<Vec<SessionRecord>> {
        let dir = self.sessions_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(format!("reading {}", dir.display()), error)),
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().to_string();
            let Some(name) = file.strip_suffix(".json") else { continue };
            if name.starts_with('.') {
                continue;
            }
            let record = self.load(name)?;
            if project.is_none_or(|project| record.project.as_deref() == Some(project)) {
                records.push(record);
            }
        }
        records.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
        Ok(records)
    }

    /// Deletes the record and the event log.
    pub fn remove(&self, name: &str) -> Result<()> {
        for path in [self.record_path(name), self.events_path(name)] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io(format!("removing {}", path.display()), error)),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_list_by_project() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().to_path_buf());
        let mut a = SessionRecord::new("DEV-1", "claude", Path::new("/w/a"), Profile::default()).unwrap();
        a.project = Some(PathBuf::from("/repo/a"));
        let mut b = SessionRecord::new("b", "codex", Path::new("/w/b"), Profile::default()).unwrap();
        b.project = None;
        registry.save(&a).unwrap();
        registry.save(&b).unwrap();
        assert_eq!(registry.load("DEV-1").unwrap(), a);
        assert_eq!(registry.list(None).unwrap().len(), 2);
        let only_a = registry.list(Some(Path::new("/repo/a"))).unwrap();
        assert_eq!(only_a.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["DEV-1"]);
        registry.remove("DEV-1").unwrap();
        assert!(matches!(registry.load("DEV-1"), Err(Error::NoSession(_))));
    }

    #[test]
    fn names_are_safe_file_names() {
        for good in ["DEV-504", "a.b_c", "x"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", ".hidden", "a/b", "a b", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_worktree_belongs_to_its_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = |args: &[&str], cwd: &Path| {
            let status = std::process::Command::new("git").args(args).current_dir(cwd).output().unwrap();
            assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
        };
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "-q", "-b", "main"], &repo);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "x"], &repo);
        git(&["worktree", "add", "-q", "../wt", "-b", "wt"], &repo);
        let root = repo.canonicalize().unwrap();
        assert_eq!(project_of(&repo).map(|p| p.canonicalize().unwrap()), Some(root.clone()));
        assert_eq!(project_of(&dir.path().join("wt")).map(|p| p.canonicalize().unwrap()), Some(root));
        assert_eq!(project_of(dir.path()), None);
    }
}
