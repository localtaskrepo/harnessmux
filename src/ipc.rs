//! The owner's socket (a Unix socket, a named pipe on Windows): one JSON request per connection, one JSON reply.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::{Error, Result};
use crate::registry::Registry;
use crate::session::Status;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Send { prompt: String },
    Cancel,
    Answer { request: String, option: String },
    Set { option: String, value: String },
    Status,
    Close,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// send: the turn the prompt runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

impl Reply {
    pub fn ok() -> Self {
        Self { ok: true, error: None, turn: None, status: None }
    }

    pub fn from_result<T>(result: Result<T>, fill: impl FnOnce(&mut Self, T)) -> Self {
        match result {
            Ok(value) => {
                let mut reply = Self::ok();
                fill(&mut reply, value);
                reply
            }
            Err(error) => Self { ok: false, error: Some(error.to_string()), turn: None, status: None },
        }
    }

    pub fn into_result(self) -> Result<Self> {
        if self.ok { Ok(self) } else { Err(Error::Invalid(self.error.unwrap_or_else(|| "the session's owner refused the request".into()))) }
    }
}

/// A connection to or from an owner.
pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}
pub type Stream = Box<dyn Conn>;

/// The owner's end of the socket.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    pipe: String,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl Listener {
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            Ok(Box::new(stream))
        }
        #[cfg(windows)]
        {
            self.next.connect().await?;
            // A new instance for the next client before this one is handed off.
            let fresh = tokio::net::windows::named_pipe::ServerOptions::new().create(&self.pipe)?;
            Ok(Box::new(std::mem::replace(&mut self.next, fresh)))
        }
    }
}

fn owned_elsewhere(session: &str) -> Error {
    Error::Invalid(format!("session {session} already has an owner; use `harnessmux send {session}`"))
}

/// Binds the session's socket. A socket left by an owner that died is replaced; a live one means another process owns
/// the session.
pub async fn listen(registry: &Registry, session: &str) -> Result<Listener> {
    let raw = registry.socket_name(session);
    #[cfg(unix)]
    {
        let path = std::path::Path::new(&raw);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| Error::io(format!("creating {}", dir.display()), error))?;
        }
        if path.exists() {
            if connect(registry, session).await.is_ok() {
                return Err(owned_elsewhere(session));
            }
            let _ = std::fs::remove_file(path);
        }
        match tokio::net::UnixListener::bind(path) {
            Ok(inner) => Ok(Listener { inner }),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Err(owned_elsewhere(session)),
            Err(error) => Err(Error::io(format!("listening on {raw}"), error)),
        }
    }
    #[cfg(windows)]
    {
        let pipe = format!(r"\\.\pipe\{raw}");
        match tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(&pipe) {
            Ok(next) => Ok(Listener { pipe, next }),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(owned_elsewhere(session)),
            Err(error) => Err(Error::io(format!("listening on {pipe}"), error)),
        }
    }
}

async fn connect(registry: &Registry, session: &str) -> Result<Stream> {
    let raw = registry.socket_name(session);
    let fail = |error| Error::io(format!("connecting to the owner of {session}"), error);
    #[cfg(unix)]
    {
        Ok(Box::new(tokio::net::UnixStream::connect(&raw).await.map_err(fail)?))
    }
    #[cfg(windows)]
    {
        const ERROR_PIPE_BUSY: i32 = 231;
        let pipe = format!(r"\\.\pipe\{raw}");
        for _ in 0..50 {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe) {
                Ok(client) => return Ok(Box::new(client)),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                Err(error) => return Err(fail(error)),
            }
        }
        Err(fail(std::io::Error::other("the pipe stayed busy")))
    }
}

/// How long an owner may take to answer; it answers from its event loop, so this is generous.
const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Sends one request to the session's owner. Fails with [`Error::Io`] when no owner runs.
pub async fn request(registry: &Registry, session: &str, request: &Request) -> Result<Reply> {
    match tokio::time::timeout(ANSWER_TIMEOUT, exchange(registry, session, request)).await {
        Ok(reply) => reply,
        Err(_) => Err(Error::Agent(format!("the owner of {session} did not answer within {} s", ANSWER_TIMEOUT.as_secs()))),
    }
}

async fn exchange(registry: &Registry, session: &str, request: &Request) -> Result<Reply> {
    let stream = connect(registry, session).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut line = serde_json::to_string(request).map_err(|error| Error::Invalid(error.to_string()))?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await.map_err(|error| Error::io("writing to the session's owner", error))?;
    let mut answer = String::new();
    BufReader::new(reader).read_line(&mut answer).await.map_err(|error| Error::io("reading from the session's owner", error))?;
    serde_json::from_str(&answer).map_err(|error| Error::Protocol(format!("the session's owner answered {answer:?}: {error}")))
}

/// Whether an owner answers on the session's socket.
pub async fn owner_running(registry: &Registry, session: &str) -> bool {
    request(registry, session, &Request::Status).await.is_ok()
}

/// Reads one request from an accepted connection and writes the reply `answer` gives.
pub async fn serve_one<F, Fut>(stream: Stream, answer: F)
where
    F: FnOnce(Request) -> Fut,
    Fut: std::future::Future<Output = Reply>,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut line = String::new();
    if BufReader::new(reader).read_line(&mut line).await.is_err() {
        return;
    }
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(request) => answer(request).await,
        Err(error) => Reply { ok: false, error: Some(format!("bad request {line:?}: {error}")), turn: None, status: None },
    };
    if let Ok(mut text) = serde_json::to_string(&reply) {
        text.push('\n');
        let _ = writer.write_all(text.as_bytes()).await;
        let _ = writer.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_json() {
        let line = serde_json::to_string(&Request::Answer { request: "p1".into(), option: "allow".into() }).unwrap();
        assert_eq!(line, r#"{"op":"answer","request":"p1","option":"allow"}"#);
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"status"}"#).unwrap(), Request::Status);
    }
}
