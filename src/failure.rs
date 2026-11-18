//! Why a turn failed, as a kind a caller can act on: wait for a usage limit's reset, ask the person to sign in, retry.
//!
//! The adapter's error data comes first (claude-agent-acp's `data.errorKind` and `data.rateLimit`, ACP's
//! `authRequired` code); the error text is the fallback for adapters that send none.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{EventBody, format_rfc3339};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// A usage or rate limit that resets (Claude's five-hour or weekly limit, Codex's usage limit, HTTP 429).
    RateLimited,
    /// Out of credits or quota, or a billing problem: no reset to wait for.
    Quota,
    /// The agent isn't signed in, or its sign-in was refused.
    AuthRequired,
    /// The provider is overloaded or failing (HTTP 529, 5xx).
    Overloaded,
    /// The conversation or the reply no longer fits the model's window.
    ContextExhausted,
    /// The connection to the agent or the provider was lost.
    Transport,
    #[default]
    Other,
}

impl FailureKind {
    /// Whether the same prompt can succeed later without anyone changing anything (after `resets_at` when there is
    /// one).
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Overloaded | Self::Transport)
    }
}

/// A failed turn: the error text and what it means.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub error: String,
    pub kind: FailureKind,
    /// RFC 3339 UTC time when the limit resets, when the agent says.
    pub resets_at: Option<String>,
    pub retryable: bool,
}

impl Failure {
    pub fn new(error: impl Into<String>, kind: FailureKind) -> Self {
        Self { error: error.into(), kind, resets_at: None, retryable: kind.retryable() }
    }

    /// A failure known only by its text.
    pub fn from_text(error: impl Into<String>) -> Self {
        let error = error.into();
        let kind = kind_of_text(&error);
        Self::new(error, kind)
    }

    /// A JSON-RPC error (`{code, message, data}`) that failed an ACP request.
    pub fn from_acp_error(error: &Value) -> Self {
        let message = error.get("message").and_then(Value::as_str).unwrap_or_default();
        let data = error.get("data");
        // codex-acp puts the real reason in `data.message` under a generic "Internal error".
        let data_message = data.and_then(|data| data.as_str().or_else(|| data.get("message").and_then(Value::as_str)));
        let text = match data_message {
            Some(data) => data.to_string(),
            None if message.is_empty() => error.get("code").map(Value::to_string).unwrap_or_else(|| "the agent failed the turn".into()),
            None => message.to_string(),
        };
        let error_kind = data.and_then(|data| data.get("errorKind")).and_then(Value::as_str);
        let codex_info = data.and_then(|data| data.get("codexErrorInfo"));
        let kind = if error.get("code").and_then(Value::as_i64) == Some(AUTH_REQUIRED) {
            FailureKind::AuthRequired
        } else {
            error_kind.and_then(kind_of_error_kind).or_else(|| codex_info.and_then(kind_of_codex_error_info)).unwrap_or_else(|| kind_of_text(&text))
        };
        let mut failure = Self::new(text, kind);
        if kind == FailureKind::RateLimited {
            failure.resets_at = data.and_then(|data| data.get("rateLimit")).and_then(|limit| limit.get("resetsAt")).and_then(unix_time);
        }
        if let Some(retryable) = data.and_then(|data| data.get("retryable")).and_then(Value::as_bool) {
            failure.retryable = retryable;
        }
        failure
    }
}

impl From<Failure> for EventBody {
    fn from(failure: Failure) -> Self {
        Self::TurnFailed { error: failure.error, kind: failure.kind, resets_at: failure.resets_at, retryable: failure.retryable }
    }
}

/// ACP's `authRequired` error code.
const AUTH_REQUIRED: i64 = -32_000;

/// claude-agent-acp's `data.errorKind`: Claude Code's assistant message error, or the adapter's own.
fn kind_of_error_kind(error_kind: &str) -> Option<FailureKind> {
    Some(match error_kind {
        "rate_limit" => FailureKind::RateLimited,
        "billing_error" | "account_on_hold" => FailureKind::Quota,
        "authentication_failed" | "oauth_org_not_allowed" => FailureKind::AuthRequired,
        "overloaded" | "server_error" => FailureKind::Overloaded,
        "max_output_tokens" => FailureKind::ContextExhausted,
        "transport_lost" | "worker_shutdown" => FailureKind::Transport,
        "unknown" => return None,
        _ => FailureKind::Other,
    })
}

/// codex-acp's `data.codexErrorInfo`: Codex's error code, a string or `{"<code>": {"httpStatusCode": …}}`. The kinds
/// follow codex-acp 2.6.0's own categories, except that its usage limit (`quota_exhausted` there) resets at the time its
/// text names, so it is `RateLimited`.
fn kind_of_codex_error_info(info: &Value) -> Option<FailureKind> {
    let (code, details) = match info {
        Value::String(code) => (code.as_str(), None),
        Value::Object(map) => map.iter().next().map(|(code, details)| (code.as_str(), Some(details)))?,
        _ => return None,
    };
    match details.and_then(|details| details.get("httpStatusCode")).and_then(Value::as_i64) {
        Some(429) => return Some(FailureKind::RateLimited),
        Some(401) => return Some(FailureKind::AuthRequired),
        _ => {}
    }
    Some(match code {
        "usageLimitExceeded" | "rateLimitExceeded" => FailureKind::RateLimited,
        "sessionBudgetExceeded" => FailureKind::Quota,
        "unauthorized" => FailureKind::AuthRequired,
        "serverOverloaded" | "internalServerError" => FailureKind::Overloaded,
        "contextWindowExceeded" => FailureKind::ContextExhausted,
        "httpConnectionFailed" | "responseStreamConnectionFailed" | "responseStreamDisconnected" | "responseTooManyFailedAttempts" => FailureKind::Transport,
        "other" => return None,
        _ => FailureKind::Other,
    })
}

/// Texts that name a kind, lowercase, checked in this order.
const TEXTS: &[(FailureKind, &[&str])] = &[
    (
        FailureKind::AuthRequired,
        &[
            "not logged in",
            "run /login",
            "authentication required",
            "authentication_failed",
            "failed to authenticate",
            "invalid_api_key",
            "invalid api key",
            "unauthorized",
        ],
    ),
    (FailureKind::Quota, &["insufficient_quota", "exceeded your current quota", "credit balance", "out of credits", "billing"]),
    (
        FailureKind::RateLimited,
        &["usage limit", "hit your limit", "-hour limit", "weekly limit", "rate limit", "rate_limit", "too many requests", "status 429"],
    ),
    (
        FailureKind::Overloaded,
        &[
            "overloaded",
            "status 529",
            "status 503",
            "unexpected status 5",
            "internal server error",
            // Codex's own texts (acp_map::LAST_PARAGRAPH_FAILURES); a 429 retry limit is rate_limited above.
            "at capacity",
            "high demand",
            "capacity is unavailable",
            "capacity unavailable",
            "exceeded retry limit",
        ],
    ),
    (FailureKind::ContextExhausted, &["prompt is too long", "context_length_exceeded", "context window", "maximum context length"]),
    (FailureKind::Transport, &["stream disconnected before completion"]),
];

fn kind_of_text(text: &str) -> FailureKind {
    let lower = text.to_lowercase();
    TEXTS.iter().find(|(_, texts)| texts.iter().any(|t| lower.contains(t))).map_or(FailureKind::Other, |(kind, _)| *kind)
}

/// Unix seconds (or milliseconds, which some adapters send) as RFC 3339.
fn unix_time(value: &Value) -> Option<String> {
    let number = value.as_f64().filter(|n| *n > 0.0)?;
    let secs = if number > 1e12 { number / 1000.0 } else { number };
    Some(format_rfc3339(secs as u64, 0))
}

/// `_claude/rateLimit` (Claude Code's `rate_limit_event`) as a [`crate::event::RateLimit`].
pub fn rate_limit(info: &Value) -> Option<crate::event::RateLimit> {
    let status = info.get("status").and_then(Value::as_str)?.to_string();
    Some(crate::event::RateLimit {
        status,
        window: info.get("rateLimitType").and_then(Value::as_str).map(str::to_string),
        resets_at: info.get("resetsAt").and_then(unix_time),
        utilization: info.get("utilization").and_then(Value::as_f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_error_data_gives_the_kind_and_the_reset() {
        let error = json!({"code": -32603, "message": "You've hit your limit · resets 3pm", "data": {"errorKind": "rate_limit", "rateLimit": {"status": "rejected", "resetsAt": 1_794_675_600, "rateLimitType": "five_hour"}}});
        let failure = Failure::from_acp_error(&error);
        assert_eq!(
            failure,
            Failure {
                error: "You've hit your limit · resets 3pm".into(),
                kind: FailureKind::RateLimited,
                resets_at: Some("2026-11-14T17:00:00.000Z".into()),
                retryable: true
            }
        );
        // Live, 2026-11-18: the error data held only the kind, no rateLimit.
        let live = Failure::from_acp_error(
            &json!({"code": -32603, "message": "Internal error: You've hit your 5-hour limit · resets 6pm (America/Los_Angeles)", "data": {"errorKind": "rate_limit"}}),
        );
        assert_eq!((live.kind, live.resets_at, live.retryable), (FailureKind::RateLimited, None, true));
        let auth = Failure::from_acp_error(&json!({"code": -32000, "message": "Authentication required"}));
        assert_eq!((auth.kind, auth.retryable), (FailureKind::AuthRequired, false));
        let billing = Failure::from_acp_error(&json!({"code": -32603, "message": "Credit balance is too low", "data": {"errorKind": "billing_error"}}));
        assert_eq!(billing.kind, FailureKind::Quota);
        // `unknown` and no data fall back to the text; a string `data` is the text.
        assert_eq!(Failure::from_acp_error(&json!({"code": -32603, "message": "Overloaded", "data": {"errorKind": "unknown"}})).kind, FailureKind::Overloaded);
        assert_eq!(Failure::from_acp_error(&json!({"code": -32603, "message": "Internal error", "data": "boom"})).error, "boom");
        // codex-acp 2.6.0, live 2026-11-16: the reason is in `data.message`, the kind in `data.codexErrorInfo`.
        let codex = Failure::from_acp_error(
            &json!({"code": -32603, "message": "Internal error", "data": {"message": "You've hit your usage limit. Upgrade to Pro or try again at Nov 21st, 2026 9:36 PM.", "codexErrorInfo": "usageLimitExceeded"}}),
        );
        assert_eq!(
            (codex.error.as_str(), codex.kind, codex.retryable),
            ("You've hit your usage limit. Upgrade to Pro or try again at Nov 21st, 2026 9:36 PM.", FailureKind::RateLimited, true)
        );
        let codex_kind =
            |info: Value| Failure::from_acp_error(&json!({"code": -32603, "message": "Internal error", "data": {"message": "x", "codexErrorInfo": info}})).kind;
        assert_eq!(codex_kind(json!("contextWindowExceeded")), FailureKind::ContextExhausted);
        assert_eq!(codex_kind(json!({"responseStreamDisconnected": {"httpStatusCode": null}})), FailureKind::Transport);
        assert_eq!(codex_kind(json!({"httpConnectionFailed": {"httpStatusCode": 429}})), FailureKind::RateLimited);
        assert_eq!(codex_kind(json!("other")), FailureKind::Other);
        assert_eq!(codex_kind(json!("cyberPolicy")), FailureKind::Other);
        let told = Failure::from_acp_error(&json!({"code": -32603, "message": "x", "data": {"errorKind": "invalid_request", "retryable": true}}));
        assert_eq!((told.kind, told.retryable), (FailureKind::Other, true));
    }

    #[test]
    fn texts_give_a_kind() {
        let cases = [
            ("You've hit your usage limit. Upgrade to Pro or try again at Nov 15th, 2026 9:30 AM.", FailureKind::RateLimited),
            ("You've hit your limit · resets 3pm (Europe/Berlin)", FailureKind::RateLimited),
            ("You've hit your 5-hour limit · resets 6pm (America/Los_Angeles)", FailureKind::RateLimited),
            ("Claude API rate limit reached", FailureKind::RateLimited),
            ("Not logged in · Please run /login", FailureKind::AuthRequired),
            ("unexpected status 429 Too Many Requests", FailureKind::RateLimited),
            ("insufficient_quota: You exceeded your current quota", FailureKind::Quota),
            ("API Error: 529 Overloaded", FailureKind::Overloaded),
            ("Prompt is too long", FailureKind::ContextExhausted),
            ("Selected model is at capacity. Please try a different model.", FailureKind::Overloaded),
            ("We\u{2019}re currently experiencing high demand, which may cause temporary errors.", FailureKind::Overloaded),
            ("Policy-restricted capacity is unavailable. Please try again later.", FailureKind::Overloaded),
            ("Flex capacity unavailable.", FailureKind::Overloaded),
            ("exceeded retry limit, last status: 502 Bad Gateway", FailureKind::Overloaded),
            ("exceeded retry limit, last status: 429 Too Many Requests", FailureKind::RateLimited),
            ("stream disconnected before completion: error sending request", FailureKind::Transport),
            ("model_not_found", FailureKind::Other),
        ];
        for (text, kind) in cases {
            assert_eq!(Failure::from_text(text).kind, kind, "{text}");
        }
    }

    #[test]
    fn claude_rate_limit_info_becomes_a_window() {
        let info = json!({"status": "allowed_warning", "resetsAt": 1_794_675_600, "rateLimitType": "seven_day", "utilization": 0.91});
        let limit = rate_limit(&info).unwrap();
        assert_eq!(limit.window.as_deref(), Some("seven_day"));
        assert_eq!(limit.resets_at.as_deref(), Some("2026-11-14T17:00:00.000Z"));
        assert!(rate_limit(&json!({})).is_none());
    }
}
