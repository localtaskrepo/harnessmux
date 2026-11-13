//! Working with sessions another process owns: start an owner when none runs, send to it, wait for a turn.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::event::{Event, EventBody};
use crate::follow::Follower;
use crate::ipc::{self, Reply, Request};
use crate::registry::{Registry, SessionState};

/// How long `harnessmux serve` waits without a turn before it stops the agent (the session stays resumable).
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// Starts `harnessmux serve <name>` in the background, detached from this terminal, and waits for its socket.
pub async fn spawn_owner(registry: &Registry, name: &str, new: bool, idle_ttl: Duration) -> Result<()> {
    spawn_owner_with(registry, name, new, idle_ttl, &[]).await
}

/// [`spawn_owner`] with extra environment for the owner (`HARNESSMUX_RECORD`).
pub async fn spawn_owner_with(registry: &Registry, name: &str, new: bool, idle_ttl: Duration, env: &[(String, String)]) -> Result<()> {
    let exe = std::env::current_exe().map_err(|error| Error::io("finding the harnessmux binary", error))?;
    let log_path = registry.record_path(name).with_extension("owner.log");
    let log = std::fs::File::create(&log_path).map_err(|error| Error::io(format!("creating {}", log_path.display()), error))?;
    let mut command = std::process::Command::new(exe);
    command.arg("serve").arg(name).arg("--idle-ttl").arg(idle_ttl.as_secs().to_string());
    if new {
        command.arg("--new");
    }
    command.envs(env.iter().cloned());
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    detach(&mut command);
    let mut child = command.spawn().map_err(|error| Error::io("starting harnessmux serve", error))?;
    // Long enough for a first `npm install` of an adapter.
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if ipc::owner_running(registry, name).await {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(Error::Agent(format!("the owner of {name} stopped ({status}): {}", text.trim())));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::Agent(format!("the owner of {name} did not answer within 5 minutes; see {}", log_path.display())))
}

fn detach(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        // Windows hands every inheritable handle to a child. Inherited, this process's stdout and stderr pipes would
        // stay open while the owner runs, and a caller reading `harnessmux run`'s output would wait for the owner.
        for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle and SetHandleInformation take no pointers; an invalid or null handle makes the
            // call fail, which leaves the flag as it was.
            unsafe {
                SetHandleInformation(GetStdHandle(std_handle), HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Makes sure an owner runs, resuming the session when none does.
pub async fn ensure_owner(registry: &Registry, name: &str) -> Result<()> {
    let record = registry.load(name)?;
    if record.state == SessionState::Ended {
        return Err(Error::Invalid(format!("session {name} was closed; start a new one with `harnessmux run`")));
    }
    if ipc::owner_running(registry, name).await {
        return Ok(());
    }
    spawn_owner(registry, name, false, DEFAULT_IDLE_TTL).await
}

pub async fn request(registry: &Registry, name: &str, request: &Request) -> Result<Reply> {
    ipc::request(registry, name, request).await?.into_result()
}

/// Sends a prompt, resuming the session first when no owner runs. Returns the turn.
pub async fn send(registry: &Registry, name: &str, prompt: &str) -> Result<u64> {
    ensure_owner(registry, name).await?;
    let reply = request(registry, name, &Request::Send { prompt: prompt.to_string() }).await?;
    reply.turn.ok_or_else(|| Error::Protocol("the owner accepted the prompt but named no turn".into()))
}

/// Calls `each` for every event of `turn` (from `follower`'s position) until the turn ends; returns the ending event.
/// Also ends when the session ends before the turn does.
pub async fn wait_turn(follower: &mut Follower, turn: u64, timeout: Option<Duration>, mut each: impl FnMut(&Event)) -> Result<Event> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        let batch = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline.into(), follower.next()).await {
                Ok(batch) => batch?,
                Err(_) => return Err(Error::Invalid(format!("turn {turn} did not end in time"))),
            },
            None => follower.next().await?,
        };
        for event in batch {
            let mine = event.turn == Some(turn);
            if mine {
                each(&event);
            }
            if (mine && event.body.ends_turn()) || matches!(event.body, EventBody::SessionEnded { .. }) {
                return Ok(event);
            }
        }
    }
}

/// The events log of a session.
pub fn events_path(registry: &Registry, name: &str) -> Result<std::path::PathBuf> {
    registry.load(name)?;
    Ok(registry.events_path(name))
}

/// The project a directory belongs to, for `--project`: the repository's main checkout, else the directory itself.
pub fn project_filter(dir: &Path) -> std::path::PathBuf {
    crate::registry::project_of(dir).unwrap_or_else(|| dir.to_path_buf())
}
