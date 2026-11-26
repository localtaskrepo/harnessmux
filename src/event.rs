//! The event stream: one JSON object per line, the same for every agent and adapter version.
//!
//! Version 1. New event types and new fields may be added within a version; readers skip what they don't know.
//! Removing or changing a field raises [`EVENT_VERSION`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::failure::{Failure, FailureKind};

pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Event format version ([`EVENT_VERSION`]).
    pub v: u32,
    /// RFC 3339 UTC time.
    pub ts: String,
    /// Session name.
    pub session: String,
    /// The repository the session belongs to (its main checkout, also for a worktree); none outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Position in the session's log, from 1.
    pub seq: u64,
    /// The turn this event belongs to; none for session-level events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(flatten)]
    pub body: EventBody,
    /// The agent's own message this event came from (an ACP message, an Amp or T3 JSON line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Context window in use and its size, when the agent reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// The account's usage-limit window, when the agent reports it (claude-agent-acp's `_claude/rateLimit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    /// allowed, allowed_warning or rejected.
    pub status: String,
    /// The limit's window as the agent names it (Claude: five_hour, seven_day, overage, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// RFC 3339 UTC time when the window resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// How much of the window is used, 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// allow_once, allow_always, reject_once or reject_always.
    pub kind: String,
}

/// The most bytes of a tool's output an event carries.
pub const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    /// pending, in_progress or completed.
    #[serde(default)]
    pub status: String,
    /// high, medium or low.
    #[serde(default)]
    pub priority: String,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    /// The agent stopped to wait for an answer the caller can't give through harnessmux (a T3 Code thread's
    /// question); the agent's own session takes it.
    WaitingForAnswer,
}

impl StopReason {
    /// The CLI's exit code for a turn that ended this way: 0, 2 when cancelled, 3 when it waits for an answer.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Cancelled => 2,
            Self::WaitingForAnswer => 3,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventBody {
    #[serde(rename = "session.started")]
    SessionStarted {
        agent: String,
        cwd: String,
        /// The agent's own session id (ACP session id, Amp thread, T3 thread id).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_session: Option<String>,
        /// new, resumed (session/resume) or loaded (session/load).
        how: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adapter: Option<String>,
        /// Where a person opens the agent's session (a T3 Code thread's link).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
    },
    #[serde(rename = "turn.started")]
    TurnStarted { prompt: String },
    #[serde(rename = "message.delta")]
    MessageDelta { text: String },
    #[serde(rename = "thought.delta")]
    ThoughtDelta { text: String },
    /// The agent's whole reply in this turn, sent before `turn.finished`.
    #[serde(rename = "message")]
    Message { text: String },
    #[serde(rename = "tool.started")]
    ToolStarted {
        id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// The tool's arguments as the agent sends them (ACP `rawInput`): `{"command": …}`, `{"file_path": …}`. Some
        /// agents (Claude) send them only in a later `tool.updated`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
        /// Paths the tool reads or changes.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<String>,
    },
    /// A running tool's title, kind, input or paths, once the agent knows them; only the fields this update carries.
    /// Each update holds all input known so far.
    #[serde(rename = "tool.updated")]
    ToolUpdated {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<String>,
    },
    #[serde(rename = "tool.finished")]
    ToolFinished {
        id: String,
        /// completed or failed.
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The tool's text output, at most [`OUTPUT_LIMIT`] bytes (a longer one ends `… [N more bytes]`); `raw`
        /// holds all of it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// The agent's plan (its to-do list), whole each time; no entries when it cleared the plan.
    #[serde(rename = "plan")]
    Plan { entries: Vec<PlanEntry> },
    #[serde(rename = "permission.requested")]
    PermissionRequested { request: String, title: String, options: Vec<PermissionOption> },
    #[serde(rename = "permission.answered")]
    PermissionAnswered {
        request: String,
        /// The chosen option id; none when the request was cancelled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option: Option<String>,
        /// policy or caller.
        by: String,
    },
    #[serde(rename = "usage")]
    Usage(Usage),
    #[serde(rename = "config.changed")]
    ConfigChanged { option: String, value: String },
    /// Something the caller should know that is not an error: an option the agent doesn't offer, a queued prompt.
    #[serde(rename = "notice")]
    Notice { text: String },
    /// The turn's result, as the profile's `output_schema` asked for (t3: what the agent submitted with
    /// `t3_submit_output`). Comes before `turn.finished`.
    #[serde(rename = "turn.output")]
    Output { output: serde_json::Value },
    /// A milestone of the agent's own session (t3: T3 Code's MCP events): `thread.run_settled` with the run and its
    /// outcome (completed, failed, interrupted, cancelled), `thread.needs_input` with the request and its kind
    /// (user_input, approval).
    #[serde(rename = "milestone")]
    Milestone {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    #[serde(rename = "turn.finished")]
    TurnFinished {
        stop_reason: StopReason,
        #[serde(default, skip_serializing_if = "Usage::is_empty")]
        usage: Usage,
    },
    /// The turn failed. `raw` holds the agent's error when it sent one (an ACP JSON-RPC error).
    #[serde(rename = "turn.failed")]
    TurnFailed {
        error: String,
        /// What the failure means ([`FailureKind`]): from the agent's error data, else from the text.
        #[serde(default)]
        kind: FailureKind,
        /// RFC 3339 UTC time when the usage limit resets, when the agent says (rate_limited only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<String>,
        /// Whether the same prompt can succeed later without anyone changing anything (after `resets_at`).
        #[serde(default)]
        retryable: bool,
    },
    #[serde(rename = "session.ended")]
    SessionEnded { reason: String },
}

impl EventBody {
    /// A `turn.failed` known only by its text; the kind comes from the text.
    pub fn failed(error: impl Into<String>) -> Self {
        Failure::from_text(error).into()
    }

    /// Whether this event ends a turn.
    pub fn ends_turn(&self) -> bool {
        matches!(self, Self::TurnFinished { .. } | Self::TurnFailed { .. })
    }

    /// The CLI's exit code for a turn-ending event.
    pub fn exit_code(&self) -> Option<u8> {
        match self {
            Self::TurnFinished { stop_reason, .. } => Some(stop_reason.exit_code()),
            Self::TurnFailed { .. } => Some(1),
            _ => None,
        }
    }
}

/// `2026-11-13T22:45:12.815Z` for now.
pub fn now() -> String {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format_rfc3339(since.as_secs(), since.subsec_millis())
}

pub(crate) fn format_rfc3339(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

/// Unix seconds of a UTC RFC 3339 time such as `2026-11-23T08:55:39.479Z` (the inverse of [`format_rfc3339`]).
pub(crate) fn parse_rfc3339(time: &str) -> Option<u64> {
    let (date, clock) = time.strip_suffix('Z')?.split_once('T')?;
    let number = |text: &str| text.parse::<i64>().ok();
    let mut date = date.splitn(3, '-').map(number);
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut clock = clock.splitn(3, ':');
    let (hour, minute) = (number(clock.next()?)?, number(clock.next()?)?);
    let second = number(clock.next()?.split('.').next()?)?;
    // Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm).
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339("2026-11-13T22:45:12.815Z"), Some(1_794_609_912));
        assert_eq!(parse_rfc3339("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-13 22:45:12"), None);
    }

    #[test]
    fn times_are_rfc3339() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(1_794_609_912, 815), "2026-11-13T22:45:12.815Z");
        assert_eq!(format_rfc3339(951_782_400, 1), "2000-02-29T00:00:00.001Z");
    }

    #[test]
    fn events_are_flat_json_with_a_type() {
        let event = Event {
            v: EVENT_VERSION,
            ts: "t".into(),
            session: "s".into(),
            project: None,
            seq: 3,
            turn: Some(1),
            body: EventBody::TurnFinished { stop_reason: StopReason::EndTurn, usage: Usage { output_tokens: Some(5), ..Usage::default() } },
            raw: None,
        };
        let line = serde_json::to_string(&event).unwrap();
        assert_eq!(line, r#"{"v":1,"ts":"t","session":"s","seq":3,"turn":1,"type":"turn.finished","stop_reason":"end_turn","usage":{"output_tokens":5}}"#);
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
        assert_eq!(event.body.exit_code(), Some(0));
    }
}
