//! T3 Code's MCP endpoint (`<origin>/mcp`), for what its CLI can't do: start a thread in an existing worktree, send
//! with a request id, settle a thread. The T3 Code server's origin comes from `<T3CODE_HOME>/userdata/server-runtime.json`
//! (`~/.t3` without `T3CODE_HOME`).
//!
//! Sign-in: harnessmux registers as an OAuth client of T3 Code and approves its own sign-in with a pairing code from
//! `t3 auth pairing create` (scopes below, two minutes), with full access, as Tencent/teamai-cli does. The token lasts
//! 30 days; it is kept in `t3-credential.json` in the state directory (0600) and reused while valid. An expired token,
//! or one `/mcp` refuses, signs in again. T3 Code lists the sign-in as a session labelled `harnessmux`
//! (`t3 auth session list`; `t3 auth session revoke <id>` ends it). The pairing code and the token are never printed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::error::{Error, Result};
use crate::paths;

const SCOPES: [&str; 4] = ["orchestration:read", "orchestration:operate", "settings:write", "source-control:write"];
const CLIENT_NAME: &str = "harnessmux";
/// T3 Code sends the authorization code here; nothing listens, the code is read from the decision's answer.
const REDIRECT: &str = "http://127.0.0.1/callback";
const PROTOCOL: &str = "2025-06-18";
/// A request T3 Code doesn't answer in this time fails.
const TIMEOUT: Duration = Duration::from_secs(120);
/// A token this close to its end signs in again.
const MARGIN: u64 = 10 * 60;

/// How to run T3's CLI (`t3 auth pairing create`), and the environment that names T3 Code's data directory.
pub struct Cli {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

/// The kept sign-in. No `Debug`: it holds the token.
#[derive(Serialize, Deserialize)]
struct Credential {
    origin: String,
    access_token: String,
    /// Unix seconds.
    expires_at: u64,
}

/// A signed-in connection to T3 Code's `/mcp`.
pub struct Mcp {
    origin: String,
    token: String,
    session: Option<String>,
    next_id: u64,
}

enum Failure {
    /// `/mcp` refused the token.
    Unauthorized,
    Other(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Other(error)
    }
}

impl Mcp {
    /// Connects with the kept credential, or signs in when there is none for this server, it has expired, or `/mcp`
    /// refuses it.
    pub async fn connect(cli: &Cli) -> Result<Self> {
        let origin = origin(cli)?;
        if let Some(credential) = load_credential().filter(|c| c.origin == origin && c.expires_at > now() + MARGIN) {
            let mut mcp = Self { origin: origin.clone(), token: credential.access_token, session: None, next_id: 1 };
            match mcp.initialize().await {
                Ok(()) => return Ok(mcp),
                Err(Failure::Unauthorized) => {}
                Err(Failure::Other(error)) => return Err(error),
            }
        }
        Self::sign_in(cli, origin).await
    }

    async fn sign_in(cli: &Cli, origin: String) -> Result<Self> {
        let credential = sign_in(cli, &origin).await?;
        let mut mcp = Self { origin, token: credential.access_token.clone(), session: None, next_id: 1 };
        save_credential(&credential)?;
        match mcp.initialize().await {
            Ok(()) => Ok(mcp),
            Err(Failure::Unauthorized) => Err(Error::Agent("T3 Code refused the token it just issued to harnessmux".into())),
            Err(Failure::Other(error)) => Err(error),
        }
    }

    /// Calls a T3 Code tool and returns its structured result. A refused token signs in again once.
    pub async fn call(&mut self, cli: &Cli, tool: &str, arguments: Value) -> Result<Value> {
        let params = json!({"name": tool, "arguments": arguments});
        let reply = match self.rpc("tools/call", params.clone()).await {
            Err(Failure::Unauthorized) => {
                *self = Self::sign_in(cli, self.origin.clone()).await?;
                self.rpc("tools/call", params).await
            }
            other => other,
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(Failure::Unauthorized) => return Err(Error::Agent(format!("T3 Code refused harnessmux's new sign-in for {tool}"))),
            Err(Failure::Other(error)) => return Err(error),
        };
        if let Some(error) = reply.get("error") {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("no message");
            return Err(Error::Agent(format!("T3 Code refused {tool}: {message}")));
        }
        let result = reply.get("result").cloned().unwrap_or(Value::Null);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(Error::Agent(format!("T3 Code refused {tool}: {}", tool_text(&result))));
        }
        Ok(match result.get("structuredContent") {
            Some(content) => content.clone(),
            None => serde_json::from_str(&tool_text(&result)).unwrap_or(Value::Null),
        })
    }

    async fn initialize(&mut self) -> std::result::Result<(), Failure> {
        let params = json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")}});
        let reply = self.rpc("initialize", params).await?;
        if let Some(error) = reply.get("error") {
            return Err(Error::Agent(format!("T3 Code's MCP endpoint refused to start a session: {error}")).into());
        }
        self.post(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})).await?;
        Ok(())
    }

    async fn rpc(&mut self, method: &str, params: Value) -> std::result::Result<Value, Failure> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self.post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let body = if response.header("content-type").is_some_and(|ct| ct.starts_with("text/event-stream")) {
            // The answer is the event whose data is the JSON-RPC response to this id.
            response
                .body
                .split("\n\n")
                .filter_map(sse_data)
                .filter_map(|data| serde_json::from_str::<Value>(&data).ok())
                .find(|v| v.get("id") == Some(&json!(id)))
        } else {
            serde_json::from_str(&response.body).ok()
        };
        body.ok_or_else(|| Error::Agent(format!("T3 Code's answer to {method} is not JSON-RPC: {}", excerpt(&response.body))).into())
    }

    async fn post(&mut self, body: Value) -> std::result::Result<Response, Failure> {
        let authorization = format!("Bearer {}", self.token);
        let mut headers = vec![("accept", "application/json, text/event-stream"), ("authorization", authorization.as_str())];
        let session = self.session.clone();
        if let Some(session) = &session {
            headers.push(("mcp-session-id", session));
        }
        let response = post(&self.origin, "/mcp", "application/json", body.to_string().as_bytes(), &headers).await?;
        if response.status == 401 {
            return Err(Failure::Unauthorized);
        }
        if !(200..300).contains(&response.status) {
            return Err(Error::Agent(format!("T3 Code's MCP endpoint answered {}: {}", response.status, excerpt(&response.body))).into());
        }
        if let Some(session) = response.header("mcp-session-id") {
            self.session = Some(session.to_string());
        }
        Ok(response)
    }
}

/// The OAuth sign-in, approved with a pairing code.
async fn sign_in(cli: &Cli, origin: &str) -> Result<Credential> {
    let registration = json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [REDIRECT],
        "grant_types": ["authorization_code"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": SCOPES.join(" "),
    });
    let registered = post_json(origin, "/oauth/mcp/register", &registration, "registering harnessmux with T3 Code").await?;
    let client_id =
        registered.get("client_id").and_then(Value::as_str).ok_or_else(|| Error::Agent("T3 Code registered harnessmux without a client id".into()))?;
    let code = pairing_code(cli).await?;
    let verifier = base64url(&random::<32>()?);
    let challenge = base64url(&sha2::Sha256::digest(verifier.as_bytes()));
    let resource = format!("{origin}/mcp");
    let decision = json!({
        "authorization": {
            "response_type": "code",
            "client_id": client_id,
            "redirect_uri": REDIRECT,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "state": base64url(&random::<16>()?),
            "resource": resource,
        },
        "decision": {"_tag": "pairing-code", "access": "full-access", "code": code},
    });
    let approved = post_json(origin, "/oauth/mcp/decision", &decision, "approving harnessmux's sign-in with a pairing code").await?;
    let redirect = approved.get("redirectTo").and_then(Value::as_str).unwrap_or_default();
    let authorization_code = query_param(redirect, "code").ok_or_else(|| {
        let error = query_param(redirect, "error_description").or_else(|| query_param(redirect, "error")).unwrap_or_else(|| "no code".into());
        Error::Agent(format!("T3 Code didn't approve harnessmux's sign-in: {error}"))
    })?;
    let form = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", &authorization_code),
        ("redirect_uri", REDIRECT),
        ("client_id", client_id),
        ("code_verifier", &verifier),
        ("resource", &resource),
    ]);
    let response = post(origin, "/oauth/mcp/token", "application/x-www-form-urlencoded", form.as_bytes(), &[]).await?;
    let token: Value = answer(response, "getting harnessmux's T3 Code token")?;
    let access_token = token.get("access_token").and_then(Value::as_str).ok_or_else(|| Error::Agent("T3 Code issued no access token".into()))?;
    let expires_in = token.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
    Ok(Credential { origin: origin.to_string(), access_token: access_token.to_string(), expires_at: now() + expires_in })
}

/// A two-minute pairing code from `t3 auth pairing create`.
async fn pairing_code(cli: &Cli) -> Result<String> {
    let mut command = tokio::process::Command::new(&cli.program);
    command.args(&cli.args).args(["auth", "pairing", "create", "--json", "--ttl", "2m", "--label", CLIENT_NAME]);
    for scope in SCOPES {
        command.args(["--scope", scope]);
    }
    let output = command
        .envs(&cli.env)
        .current_dir(&cli.cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| Error::io(format!("starting {}", cli.program.display()), error))?;
    let parsed: Option<Value> = serde_json::from_slice(&output.stdout).ok();
    match parsed.as_ref().and_then(|v| v.get("credential")).and_then(Value::as_str) {
        Some(code) if output.status.success() => Ok(code.to_string()),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let why = stderr.lines().rfind(|line| !super::t3::is_noise(line) && !line.trim().is_empty()).unwrap_or("no pairing code in its output");
            Err(Error::Agent(format!("`t3 auth pairing create` failed ({}): {}", output.status, why.trim())))
        }
    }
}

/// The T3 Code server's origin from its runtime file.
fn origin(cli: &Cli) -> Result<String> {
    let home = cli
        .env
        .get("T3CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("T3CODE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from))
        .or_else(|| paths::home().map(|home| home.join(".t3")))
        .ok_or_else(|| Error::Agent("no home directory to find T3 Code's data in; set T3CODE_HOME".into()))?;
    let file = home.join("userdata/server-runtime.json");
    let text = std::fs::read_to_string(&file).map_err(|error| Error::Agent(format!("the T3 Code server doesn't run ({}: {error})", file.display())))?;
    let runtime: Value = serde_json::from_str(&text).map_err(|error| Error::Agent(format!("{}: {error}", file.display())))?;
    let origin = runtime.get("origin").and_then(Value::as_str).ok_or_else(|| Error::Agent(format!("{} names no origin", file.display())))?;
    Ok(origin.trim_end_matches('/').to_string())
}

fn credential_path() -> PathBuf {
    paths::state_dir().join("t3-credential.json")
}

fn load_credential() -> Option<Credential> {
    serde_json::from_str(&std::fs::read_to_string(credential_path()).ok()?).ok()
}

/// Writes the credential readable only by the user, replacing the old one in one step.
fn save_credential(credential: &Credential) -> Result<()> {
    let path = credential_path();
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|error| Error::io(format!("creating {}", dir.display()), error))?;
    let partial = path.with_extension(format!("json.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let text = serde_json::to_string(credential).map_err(|error| Error::Agent(format!("writing the T3 Code credential: {error}")))?;
    let write = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = options.open(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&partial, &path)
    };
    write().map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        Error::io(format!("writing {}", path.display()), error)
    })
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| Error::Agent(format!("no random bytes for the T3 Code sign-in: {error}")))?;
    Ok(bytes)
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
        }
    }
    out
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { char::from(b).to_string() } else { format!("%{b:02X}") })
            .collect()
    };
    pairs.iter().map(|(key, value)| format!("{}={}", encode(key), encode(value))).collect::<Vec<_>>().join("&")
}

/// A query parameter of a URL, percent-decoded.
fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next()?;
    let raw = query.split('&').find_map(|pair| pair.split_once('=').filter(|(key, _)| *key == name).map(|(_, value)| value))?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                Some(b) => {
                    out.push(b);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// The text of a tool result's content.
fn tool_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The data of one server-sent event.
fn sse_data(event: &str) -> Option<String> {
    let lines: Vec<&str> = event.lines().filter_map(|line| line.strip_prefix("data:")).map(|data| data.strip_prefix(' ').unwrap_or(data)).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn excerpt(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(500) {
        Some((at, _)) => format!("{}…", &body[..at]),
        None => body.to_string(),
    }
}

async fn post_json(origin: &str, path: &str, body: &Value, what: &str) -> Result<Value> {
    answer(post(origin, path, "application/json", body.to_string().as_bytes(), &[]).await?, what)
}

/// The JSON body of a 2xx answer; T3's error text otherwise.
fn answer(response: Response, what: &str) -> Result<Value> {
    if !(200..300).contains(&response.status) {
        let parsed: Option<Value> = serde_json::from_str(&response.body).ok();
        let text = parsed
            .as_ref()
            .and_then(|v| v.get("error_description").or_else(|| v.get("message")).or_else(|| v.get("error")))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .unwrap_or_else(|| excerpt(&response.body));
        return Err(Error::Agent(format!("{what}: T3 Code answered {}: {text}", response.status)));
    }
    serde_json::from_str(&response.body).map_err(|error| Error::Agent(format!("{what}: T3 Code's answer is not JSON ({error})")))
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

/// One HTTP/1.1 POST to the T3 Code server, which listens on plain HTTP on this machine.
async fn post(origin: &str, path: &str, content_type: &str, body: &[u8], headers: &[(&str, &str)]) -> Result<Response> {
    let unreachable = |why: String| Error::Agent(format!("Could not reach the T3 Code server at {origin}: {why}"));
    let host = origin
        .strip_prefix("http://")
        .ok_or_else(|| Error::Agent(format!("the T3 Code server's origin {origin} is not http://; harnessmux talks to it on this machine only")))?;
    let address = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(&address).await?;
        let mut request =
            format!("POST {path} HTTP/1.1\r\nhost: {host}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(body).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        Ok::<_, std::io::Error>(raw)
    };
    let raw = match tokio::time::timeout(TIMEOUT, exchange).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(error)) => return Err(unreachable(error.to_string())),
        Err(_) => return Err(unreachable(format!("no answer in {} seconds", TIMEOUT.as_secs()))),
    };
    parse_response(&raw).ok_or_else(|| unreachable("its answer is not HTTP".into()))
}

fn parse_response(raw: &[u8]) -> Option<Response> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers: Vec<(String, String)> =
        lines.filter_map(|line| line.split_once(':')).map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string())).collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(key, value)| key == "transfer-encoding" && value.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body)?;
    }
    Some(Response { status, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let end = raw.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&raw[..end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        raw = &raw[end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(raw.get(..size)?);
        raw = raw.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_has_no_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn the_pkce_challenge_is_rfc_7636s_example() {
        let challenge = base64url(&sha2::Sha256::digest(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn form_values_and_query_parameters_are_percent_coded() {
        assert_eq!(form_encode(&[("a b", "x/y=z"), ("c", "~ok")]), "a%20b=x%2Fy%3Dz&c=~ok");
        assert_eq!(query_param("http://127.0.0.1/callback?state=s&code=a%2Fb+c#x", "code").as_deref(), Some("a/b c"));
        assert_eq!(query_param("http://127.0.0.1/callback?error=access_denied", "code"), None);
    }

    #[test]
    fn responses_parse_with_and_without_chunks() {
        let plain = parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: s1\r\n\r\n{}").unwrap();
        assert_eq!((plain.status, plain.header("mcp-session-id"), plain.body.as_str()), (200, Some("s1"), "{}"));
        let chunked = parse_response(b"HTTP/1.1 401 Unauthorized\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!((chunked.status, chunked.body.as_str()), (401, "abcde"));
        assert_eq!(sse_data("event: message\ndata: {\"id\":1}").as_deref(), Some("{\"id\":1}"));
    }
}
