use std::fmt;

/// Everything harnessmux reports as a failure. The message says what was wrong and, where there is one, how to fix it.
#[derive(Debug)]
pub enum Error {
    /// A value the caller passed is not valid (unknown agent, option value, session name).
    Invalid(String),
    /// No session with that name in the registry.
    NoSession(String),
    /// The agent's program is missing or could not be started.
    Agent(String),
    /// The agent spoke ACP but refused or failed a request.
    Protocol(String),
    /// Files, sockets and processes.
    Io(String, std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(context: impl Into<String>, error: std::io::Error) -> Self {
        Self::Io(context.into(), error)
    }

    /// The process exit code the CLI uses for this error (1 is a failed turn, 2 a cancelled one).
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Invalid(_) | Self::NoSession(_) => 64,
            Self::Agent(_) | Self::Protocol(_) => 69,
            Self::Io(..) => 74,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Agent(message) | Self::Protocol(message) => f.write_str(message),
            Self::NoSession(name) => write!(f, "no session named {name}; `harnessmux list` shows the sessions"),
            Self::Io(context, error) => write!(f, "{context}: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(_, error) => Some(error),
            _ => None,
        }
    }
}

impl From<agent_client_protocol::Error> for Error {
    fn from(error: agent_client_protocol::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}
