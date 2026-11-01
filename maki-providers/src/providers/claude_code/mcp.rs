//! One request owns this loopback MCP server. The server holds tool calls until maki stops
//! Claude Code and executes the tools.
//!
//! Only clients on 127.0.0.1 with the request's bearer token can connect.

use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use futures_lite::{Stream, StreamExt, future};
use serde::Serialize;
use serde_json::{Value, json};
use smol::lock::Semaphore;
use smol::net::{TcpListener, TcpStream};
use smol::{Task, Timer};
use tracing::warn;

use super::error::Error;
use super::{MAX_EVENT_BYTES, within};

const ENDPOINT: &str = "/mcp";
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = MAX_EVENT_BYTES;
const MAX_CONNECTIONS: usize = 32;
const MAX_UNAUTHENTICATED: usize = 8;
const HEAD_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// Newest first.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER_INFO_NAME: &str = "maki";
const TOOL_USE_ID_META: &str = "claudecode/toolUseId";
const METHOD_NOT_FOUND: i64 = -32601;
const METHOD_NOT_FOUND_MESSAGE: &str = "method not found";
const POST: &str = "POST";
const INITIALIZE: &str = "initialize";
const TOOLS_LIST: &str = "tools/list";
const PING: &str = "ping";
const TOOLS_CALL: &str = "tools/call";
const BEARER_PREFIX: &str = "Bearer ";
const CONTENT_LENGTH: &str = "content-length";
const AUTHORIZATION: &str = "authorization";
const TRANSFER_ENCODING: &str = "transfer-encoding";
const CONNECTION: &str = "connection";
const CLOSE: &str = "close";
const OK: &str = "200 OK";
const ACCEPTED: &str = "202 Accepted";
const UNAVAILABLE: &str = "503 Service Unavailable";
const BAD_REQUEST: &str = "400 Bad Request";
const UNAUTHORIZED: &str = "401 Unauthorized";
const NOT_FOUND: &str = "404 Not Found";
const METHOD_NOT_ALLOWED: &str = "405 Method Not Allowed";
const LENGTH_REQUIRED: &str = "411 Length Required";
const CONTENT_TOO_LARGE: &str = "413 Content Too Large";
const HEADERS_TOO_LARGE: &str = "431 Request Header Fields Too Large";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug)]
pub(crate) enum Handoff {
    /// `tool_use_id` links Claude Code's call to the model's `tool_use`.
    Parked {
        tool_use_id: Option<String>,
        name: String,
        arguments: Value,
    },
    Invalid(Error),
}

struct Shared {
    token: String,
    tool_list: String,
    handoffs: Sender<Handoff>,
    read_limit: Duration,
    authenticated_slots: Arc<Semaphore>,
    overloaded: AtomicBool,
    invalid_notification: AtomicBool,
}

impl Shared {
    fn new(
        token: String,
        tools: Vec<McpTool>,
        handoffs: Sender<Handoff>,
        read_limit: Duration,
    ) -> Self {
        Self {
            token,
            tool_list: json!({ "tools": tools }).to_string(),
            handoffs,
            read_limit,
            authenticated_slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            overloaded: AtomicBool::new(false),
            invalid_notification: AtomicBool::new(false),
        }
    }
}

struct Connection {
    task: Task<()>,
    authenticated: Arc<AtomicBool>,
}

/// Keep connection ownership in one task so cancellation also stops held calls.
pub(crate) struct Server {
    port: u16,
    shared: Arc<Shared>,
    _accept: Task<()>,
}

impl Server {
    pub fn url(&self) -> String {
        format!("http://{}:{}{ENDPOINT}", Ipv4Addr::LOCALHOST, self.port)
    }

    pub fn authorization(&self) -> String {
        format!("{BEARER_PREFIX}{}", self.shared.token)
    }
}

pub(crate) async fn serve(
    token: String,
    tools: Vec<McpTool>,
    handoffs: Sender<Handoff>,
    read_limit: Duration,
) -> io::Result<Server> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let shared = Arc::new(Shared::new(token, tools, handoffs, read_limit));
    let server_shared = Arc::clone(&shared);
    let accept = smol::spawn(async move { accept_loop(listener.incoming(), shared).await });
    Ok(Server {
        port,
        shared: server_shared,
        _accept: accept,
    })
}

async fn accept_loop(
    mut incoming: impl Stream<Item = io::Result<TcpStream>> + Unpin,
    shared: Arc<Shared>,
) {
    let mut connections: Vec<Connection> = Vec::new();
    let mut accept_failed = false;
    loop {
        let stream = match incoming.next().await {
            Some(Ok(stream)) => {
                accept_failed = false;
                stream
            }
            // A connection that closed before the accept, or a brief shortage
            // of descriptors, must not stop the task that holds every call.
            Some(Err(error)) => {
                if !accept_failed {
                    warn!(%error, "claude-code: the handoff server cannot accept a connection");
                    accept_failed = true;
                }
                Timer::after(ACCEPT_RETRY).await;
                continue;
            }
            None => return,
        };
        connections.retain(|connection| !connection.task.is_finished());
        if connections
            .iter()
            .filter(|connection| !connection.authenticated.load(Ordering::Acquire))
            .count()
            >= MAX_UNAUTHENTICATED
            && let Some(oldest) = connections
                .iter()
                .position(|connection| !connection.authenticated.load(Ordering::Acquire))
        {
            // A local burst can evict a legitimate partial header. Keeping the oldest
            // instead lets idle peers block new attempts. Authenticated calls are never evicted.
            connections.remove(oldest);
        }
        let shared = Arc::clone(&shared);
        let authenticated = Arc::new(AtomicBool::new(false));
        let admission = Arc::clone(&authenticated);
        let task = smol::spawn(async move {
            connection(stream, shared, admission).await;
        });
        connections.push(Connection {
            task,
            authenticated,
        });
    }
}

struct Head {
    method: String,
    path: String,
    authorization: Option<String>,
    close: bool,
    length: usize,
}

enum Reply {
    Send { status: &'static str, body: String },
    Park,
}

/// Each request must arrive within the read limit, or a silent client would
/// hold its slot forever.
async fn connection(stream: TcpStream, shared: Arc<Shared>, admission: Arc<AtomicBool>) {
    let mut authenticated = None;
    let mut reader = BufReader::new(stream.clone());
    let mut writer = stream;
    let mut head_limit = shared.read_limit.min(HEAD_TIMEOUT);
    loop {
        // A rejected request's body is never read, so the connection ends.
        let head = match within(Instant::now() + head_limit, read_head(&mut reader)).await {
            Some(Ok(Some(head))) => head,
            Some(Ok(None)) | None => return,
            Some(Err(status)) => {
                let _ = writer.write_all(&response(status, "")).await;
                return;
            }
        };
        if let Err(status) = admit(&shared, &head) {
            let _ = writer.write_all(&response(status, "")).await;
            return;
        }
        if authenticated.is_none() {
            authenticated = shared.authenticated_slots.try_acquire_arc();
            if authenticated.is_none() {
                if !shared.overloaded.swap(true, Ordering::Relaxed) {
                    warn!(
                        limit = MAX_CONNECTIONS,
                        "claude-code: authenticated handoff connections are full"
                    );
                }
                let _ = writer.write_all(&response(UNAVAILABLE, "")).await;
                return;
            }
            admission.store(true, Ordering::Release);
        }
        let body = match within(
            Instant::now() + shared.read_limit,
            read_body(&mut reader, head.length),
        )
        .await
        {
            Some(Ok(body)) => body,
            Some(Err(status)) => {
                let _ = writer.write_all(&response(status, "")).await;
                return;
            }
            None => return,
        };
        head_limit = shared.read_limit;
        match answer(&shared, &body) {
            Reply::Send { status, body } => {
                if writer.write_all(&response(status, &body)).await.is_err() || head.close {
                    return;
                }
            }
            Reply::Park => future::pending::<()>().await,
        }
    }
}

/// `None` after the peer closes the connection.
async fn read_head<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<Head>, &'static str> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let read = reader
            .take((MAX_HEAD_BYTES - before) as u64 + 1)
            .read_until(b'\n', &mut head)
            .await
            .map_err(|_| BAD_REQUEST)?;
        if read == 0 {
            return if before == 0 {
                Ok(None)
            } else {
                Err(BAD_REQUEST)
            };
        }
        if head.len() > MAX_HEAD_BYTES {
            return Err(HEADERS_TOO_LARGE);
        }
        if head[before..].starts_with(b"\r\n") || head[before..].starts_with(b"\n") {
            break;
        }
    }
    let head = String::from_utf8(head).map_err(|_| BAD_REQUEST)?;
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let (Some(method), Some(path)) = (request_line.next(), request_line.next()) else {
        return Err(BAD_REQUEST);
    };
    let mut length = None;
    let mut authorization = None;
    let mut close = false;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(BAD_REQUEST)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        {
            return Err(BAD_REQUEST);
        }
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            // With two lengths, different readers could find different ends
            // of the body.
            CONTENT_LENGTH if length.is_some() => return Err(BAD_REQUEST),
            CONTENT_LENGTH => length = Some(plain_length(value).ok_or(BAD_REQUEST)?),
            AUTHORIZATION if authorization.is_some() => return Err(BAD_REQUEST),
            AUTHORIZATION => authorization = Some(value.to_owned()),
            TRANSFER_ENCODING => return Err(LENGTH_REQUIRED),
            CONNECTION => {
                close |= value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case(CLOSE));
            }
            _ => {}
        }
    }
    let length = match length {
        Some(length) => length,
        None if method == POST => return Err(LENGTH_REQUIRED),
        None => 0,
    };
    Ok(Some(Head {
        method: method.to_owned(),
        path: path.to_owned(),
        authorization,
        close,
        length,
    }))
}

/// Accepts only the digits 0 to 9, as HTTP writes a length. `parse` would
/// also accept a sign.
fn plain_length(value: &str) -> Option<usize> {
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse().ok()
    } else {
        None
    }
}

/// Runs before any body is read, token check first, so a client without the
/// token learns nothing more.
fn admit(shared: &Shared, head: &Head) -> Result<(), &'static str> {
    let presented = head
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix(BEARER_PREFIX))
        .unwrap_or_default();
    if !same_secret(presented.as_bytes(), shared.token.as_bytes()) {
        return Err(UNAUTHORIZED);
    }
    if head.path != ENDPOINT {
        return Err(NOT_FOUND);
    }
    // No server-sent stream: each answer goes in the reply to its POST.
    if head.method != POST {
        return Err(METHOD_NOT_ALLOWED);
    }
    if head.length > MAX_BODY_BYTES {
        return Err(CONTENT_TOO_LARGE);
    }
    Ok(())
}

async fn read_body<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    length: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut body = Vec::new();
    reader
        .take(length as u64)
        .read_to_end(&mut body)
        .await
        .map_err(|_| BAD_REQUEST)?;
    if body.len() == length {
        Ok(body)
    } else {
        Err(BAD_REQUEST)
    }
}

/// Compares in constant time, so response timing reveals nothing about the
/// token.
fn same_secret(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn answer(shared: &Shared, body: &[u8]) -> Reply {
    // A batch could hide a call in an array, so only one message at a time
    // is accepted.
    let Ok(mut message @ Value::Object(_)) = serde_json::from_slice::<Value>(body) else {
        return plain(BAD_REQUEST);
    };
    let Some(method) = message["method"].as_str() else {
        return plain(BAD_REQUEST);
    };
    let Some(id) = message.get("id") else {
        // A call sent as a notification expects no answer, so Claude Code
        // would continue without maki's result, and the call stops the
        // request.
        if method == TOOLS_CALL && !shared.invalid_notification.swap(true, Ordering::Relaxed) {
            let _ = shared.handoffs.send(Handoff::Invalid(Error::NoCallId));
        }
        return plain(ACCEPTED);
    };
    let result = match method {
        INITIALIZE => json!({
            "protocolVersion": protocol_version(&message["params"]["protocolVersion"]),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_INFO_NAME, "version": env!("CARGO_PKG_VERSION") },
        }),
        TOOLS_LIST => return rpc_success(id, &shared.tool_list),
        PING => json!({}),
        TOOLS_CALL => return park(shared, message["params"].take()),
        _ => return rpc_error(id, METHOD_NOT_FOUND, METHOD_NOT_FOUND_MESSAGE),
    };
    rpc_success(id, &result.to_string())
}

fn rpc_success(id: &Value, result: &str) -> Reply {
    Reply::Send {
        status: OK,
        body: format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#),
    }
}

/// Returns the client's version if maki knows it, or maki's newest. The
/// server only lists tools and takes calls, which every version has.
fn protocol_version(asked: &Value) -> &str {
    asked
        .as_str()
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .unwrap_or(PROTOCOL_VERSIONS[0])
}

/// Invalid calls are held too, because any answer, even an error, would let
/// Claude Code continue without maki.
fn park(shared: &Shared, mut params: Value) -> Reply {
    let name = params["name"].as_str().unwrap_or_default().to_owned();
    let handoff = match params
        .get_mut("arguments")
        .map(Value::take)
        .unwrap_or_default()
    {
        arguments @ Value::Object(_) => Handoff::Parked {
            tool_use_id: params["_meta"][TOOL_USE_ID_META]
                .as_str()
                .map(str::to_owned),
            name,
            arguments,
        },
        other => Handoff::Invalid(Error::NotArguments { name, input: other }),
    };
    let _ = shared.handoffs.send(handoff);
    Reply::Park
}

fn plain(status: &'static str) -> Reply {
    Reply::Send {
        status,
        body: String::new(),
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Reply {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    Reply::Send {
        status: OK,
        body: body.to_string(),
    }
}

fn response(status: &str, body: &str) -> Vec<u8> {
    let kind = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    format!(
        "HTTP/1.1 {status}\r\n{kind}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io;
    use std::net::{Ipv4Addr, Shutdown};
    use std::sync::Arc;
    use std::time::Instant;

    use flume::Receiver;
    use futures_lite::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Cursor};
    use futures_lite::{StreamExt, future, stream};
    use serde_json::{Value, json};
    use smol::net::{TcpListener, TcpStream};
    use test_case::test_case;

    use super::super::error::Error;
    use super::super::{WAIT, within};
    use super::{
        ACCEPTED, BAD_REQUEST, CONTENT_TOO_LARGE, ENDPOINT, HEADERS_TOO_LARGE, Handoff, INITIALIZE,
        LENGTH_REQUIRED, MAX_BODY_BYTES, MAX_CONNECTIONS, MAX_HEAD_BYTES, MAX_UNAUTHENTICATED,
        METHOD_NOT_ALLOWED, METHOD_NOT_FOUND, McpTool, NOT_FOUND, OK, PING, PROTOCOL_VERSIONS,
        Reply, Server, Shared, TOOL_USE_ID_META, TOOLS_CALL, TOOLS_LIST, UNAUTHORIZED, UNAVAILABLE,
        accept_loop, answer, read_head, serve,
    };

    const TOKEN: &str = "secret-token";
    const OLDER_VERSION: &str = "2025-06-18";
    const UNKNOWN_VERSION: &str = "2099-01-01";
    const TOOL: &str = "read";
    const TOOL_USE_ID: &str = "toolu_1";
    /// A body length the request announces but never sends.
    const BODY_NEVER_SENT: usize = 1000;

    #[test_case("close", true ; "close")]
    #[test_case("keep-alive, CLOSE", true ; "close_in_a_token_list")]
    #[test_case("keep-alive", false ; "keep_alive")]
    #[test_case("disclose", false ; "not_a_close_token")]
    fn connection_headers_parse_tokens(value: &str, close: bool) {
        smol::block_on(async {
            let request = head(&format!("Connection: {value}\r\nContent-Length: 0\r\n"));
            let head = read_head(&mut Cursor::new(request)).await.unwrap().unwrap();
            assert_eq!(head.close, close);
        });
    }

    fn tools() -> Vec<McpTool> {
        vec![McpTool {
            name: TOOL.into(),
            description: "Read a file".into(),
            input_schema: json!({ "type": "object" }),
        }]
    }

    fn post(body: &Value, token: &str) -> Vec<u8> {
        let body = body.to_string();
        format!(
            "POST {ENDPOINT} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    async fn bounded<T>(fut: impl Future<Output = T>) -> T {
        within(Instant::now() + WAIT, fut)
            .await
            .unwrap_or_else(|| panic!("no event occurred in {WAIT:?}"))
    }

    /// The connection sends nothing after `request`, so a cut body ends
    /// there.
    async fn connected(request: &[u8]) -> (Server, TcpStream, Receiver<Handoff>) {
        let (tx, rx) = flume::unbounded();
        let server = serve(TOKEN.into(), tools(), tx, WAIT).await.unwrap();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))
            .await
            .unwrap();
        stream.write_all(request).await.unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        (server, stream, rx)
    }

    #[test]
    fn two_requests_share_one_authenticated_connection() {
        smol::block_on(async {
            let (tx, _handoffs) = flume::unbounded();
            let server = serve(TOKEN.into(), tools(), tx, WAIT).await.unwrap();
            let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))
                .await
                .unwrap();
            let mut reader = BufReader::new(client.clone());
            for (id, method) in [(1, INITIALIZE), (2, TOOLS_LIST)] {
                client
                    .write_all(&post(
                        &json!({ "jsonrpc": "2.0", "id": id, "method": method }),
                        TOKEN,
                    ))
                    .await
                    .unwrap();
                let mut head = String::new();
                let mut length = None;
                loop {
                    head.clear();
                    bounded(reader.read_line(&mut head)).await.unwrap();
                    assert!(!head.is_empty());
                    if head == "\r\n" {
                        break;
                    }
                    if let Some(value) = head.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.unwrap()];
                bounded(reader.read_exact(&mut body)).await.unwrap();
                let reply: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(reply["id"], id);
                assert!(reply.get("result").is_some());
            }
        });
    }

    /// After an accept error (a connection closed before the accept, or a
    /// brief shortage of descriptors) the server still serves the next one.
    #[test]
    fn a_failed_accept_keeps_the_server_serving() {
        smol::block_on(async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, _handoffs) = flume::unbounded();
            let shared = Arc::new(Shared::new(TOKEN.into(), tools(), tx, WAIT));
            let _serving = smol::spawn(async move {
                let aborted = stream::once(Err(io::Error::from(io::ErrorKind::ConnectionAborted)));
                let incoming = aborted.chain(listener.incoming());
                accept_loop(incoming, shared).await;
            });
            let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            let request = post(&json!({ "jsonrpc": "2.0", "id": 1, "method": PING }), TOKEN);
            client.write_all(&request).await.unwrap();
            let mut status = String::new();
            bounded(BufReader::new(client).read_line(&mut status))
                .await
                .unwrap();
            assert!(status.contains(OK), "got: {status}");
        });
    }

    #[test]
    fn idle_connection_eviction_preserves_authenticated_calls() {
        smol::block_on(async {
            let (tx, handoffs) = flume::unbounded();
            let server = serve(TOKEN.into(), tools(), tx, WAIT).await.unwrap();
            let address = (Ipv4Addr::LOCALHOST, server.port);
            let mut held = TcpStream::connect(address).await.unwrap();
            held.write_all(&post(&json!({
                "jsonrpc": "2.0", "id": 1, "method": TOOLS_CALL,
                "params": { "name": TOOL, "arguments": {}, "_meta": { TOOL_USE_ID_META: TOOL_USE_ID } },
            }), TOKEN)).await.unwrap();
            assert!(matches!(
                bounded(handoffs.recv_async()).await.unwrap(),
                Handoff::Parked { .. }
            ));

            let mut idle = Vec::new();
            for _ in 0..MAX_UNAUTHENTICATED {
                idle.push(TcpStream::connect(address).await.unwrap());
            }
            let mut stream = TcpStream::connect(address).await.unwrap();
            let ping = json!({ "jsonrpc": "2.0", "id": 2, "method": PING });
            stream.write_all(&post(&ping, TOKEN)).await.unwrap();
            let mut status = String::new();
            bounded(BufReader::new(stream).read_line(&mut status))
                .await
                .unwrap();
            assert!(status.contains(OK), "{status:?}");

            let mut byte = [0];
            assert_eq!(bounded(idle[0].read(&mut byte)).await.unwrap(), 0);
            assert!(future::poll_once(held.read(&mut byte)).await.is_none());
            drop(server);
            assert_eq!(bounded(held.read(&mut byte)).await.unwrap(), 0);
        });
    }

    #[test]
    fn full_authenticated_connections_reply_with_unavailable_and_recover() {
        smol::block_on(async {
            let (tx, _handoffs) = flume::unbounded();
            let server = serve(TOKEN.into(), tools(), tx, WAIT).await.unwrap();
            let mut held: Vec<_> = (0..MAX_CONNECTIONS)
                .map(|_| server.shared.authenticated_slots.try_acquire_arc().unwrap())
                .collect();
            for status in [UNAVAILABLE, OK] {
                let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))
                    .await
                    .unwrap();
                client
                    .write_all(&post(
                        &json!({ "jsonrpc": "2.0", "id": 1, "method": PING }),
                        TOKEN,
                    ))
                    .await
                    .unwrap();
                let mut head = String::new();
                bounded(BufReader::new(client).read_line(&mut head))
                    .await
                    .unwrap();
                assert!(head.contains(status), "{head:?}");
                if status == UNAVAILABLE {
                    held.clear();
                }
            }
        });
    }

    /// A ping with two lengths, the second one correct.
    fn two_lengths() -> Vec<u8> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": PING }).to_string();
        format!(
            "POST {ENDPOINT} HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: \
             {BODY_NEVER_SENT}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A ping whose correct length carries a sign.
    fn signed_length() -> Vec<u8> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": PING }).to_string();
        format!(
            "POST {ENDPOINT} HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: +{}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn head(headers: &str) -> Vec<u8> {
        format!("POST {ENDPOINT} HTTP/1.1\r\n{headers}\r\n").into_bytes()
    }

    /// Also returns the handoffs the server produced by then.
    fn reply_to(request: &[u8]) -> (String, Vec<Handoff>) {
        smol::block_on(async {
            let (server, stream, handoffs) = connected(request).await;
            let mut reader = BufReader::new(stream);
            let mut reply = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                bounded(reader.read_line(&mut line)).await.unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                reply.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            bounded(reader.read_exact(&mut body)).await.unwrap();
            reply.push_str(&String::from_utf8(body).unwrap());
            drop(server);
            (reply, handoffs.try_iter().collect())
        })
    }

    /// Waits for the handoff, drops the server, and returns what the
    /// connection received before it closed.
    fn parked(request: &[u8]) -> (Vec<u8>, Handoff) {
        smol::block_on(async {
            let (server, mut stream, handoffs) = connected(request).await;
            let handoff = bounded(handoffs.recv_async()).await.unwrap();
            drop(server);
            let mut rest = Vec::new();
            bounded(stream.read_to_end(&mut rest)).await.unwrap();
            (rest, handoff)
        })
    }

    #[test]
    fn discovery_lists_the_tools() {
        let (reply, handoffs) = reply_to(&post(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": TOOLS_LIST }),
            TOKEN,
        ));
        assert!(reply.starts_with(&format!("HTTP/1.1 {OK}")), "{reply}");
        assert!(reply.contains(r#""name":"read""#), "{reply}");
        assert!(reply.contains("inputSchema"), "{reply}");
        assert!(handoffs.is_empty());
    }

    #[test]
    fn a_call_is_parked_and_never_answered() {
        let call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": TOOLS_CALL,
            "params": { "name": TOOL, "arguments": { "path": "a" }, "_meta": { TOOL_USE_ID_META: TOOL_USE_ID } },
        });
        let (rest, handoff) = parked(&post(&call, TOKEN));
        assert!(rest.is_empty(), "a held call must receive no reply");
        match handoff {
            Handoff::Parked {
                tool_use_id,
                name,
                arguments,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some(TOOL_USE_ID));
                assert_eq!(name, TOOL);
                assert_eq!(arguments, json!({ "path": "a" }));
            }
            other => panic!("this handoff is not correct: {other:?}"),
        }
    }

    #[test_case(json!({ "name": "bash", "arguments": {} }) => matches Handoff::Parked { .. } ; "an_unknown_tool")]
    #[test_case(json!({ "name": TOOL, "arguments": "rm -rf /" }) => matches Handoff::Invalid(Error::NotArguments { .. }) ; "arguments_that_are_no_object")]
    #[test_case(json!(42) => matches Handoff::Invalid(Error::NotArguments { .. }) ; "scalar_params")]
    #[test_case(json!([]) => matches Handoff::Invalid(Error::NotArguments { .. }) ; "array_params")]
    fn tool_calls_are_held_for_turn_validation(params: Value) -> Handoff {
        let call = json!({ "jsonrpc": "2.0", "id": 3, "method": TOOLS_CALL, "params": params });
        let (rest, handoff) = parked(&post(&call, TOKEN));
        assert!(
            rest.is_empty(),
            "an invalid call must also receive no reply"
        );
        handoff
    }

    #[test_case(post(&json!({ "jsonrpc": "2.0", "id": 1, "method": TOOLS_LIST }), "wrong"), UNAUTHORIZED ; "a_wrong_token")]
    #[test_case(post(&json!({ "jsonrpc": "2.0", "id": 1, "method": TOOLS_CALL, "params": { "name": TOOL, "arguments": {} } }), "wrong"), UNAUTHORIZED ; "a_call_with_a_wrong_token")]
    #[test_case(format!("GET {ENDPOINT} HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").into_bytes(), METHOD_NOT_ALLOWED ; "an_event_stream")]
    #[test_case(format!("POST /other HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\n\r\n").into_bytes(), NOT_FOUND ; "another_path")]
    #[test_case(b"POST /other HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_vec(), UNAUTHORIZED ; "another_path_without_the_token")]
    #[test_case(format!("POST {ENDPOINT} HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").into_bytes(), LENGTH_REQUIRED ; "a_chunked_body")]
    #[test_case(format!("POST {ENDPOINT} HTTP/1.1\r\n\r\n").into_bytes(), LENGTH_REQUIRED ; "a_post_without_a_length")]
    #[test_case(head(&format!("Authorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n", MAX_BODY_BYTES + 1)), CONTENT_TOO_LARGE ; "an_oversized_body")]
    #[test_case(head(&format!("Authorization: Bearer wrong\r\nContent-Length: {}\r\n", MAX_BODY_BYTES + 1)), UNAUTHORIZED ; "an_oversized_body_with_a_wrong_token")]
    #[test_case(head(&format!("Authorization: Bearer wrong\r\nContent-Length: {BODY_NEVER_SENT}\r\n")), UNAUTHORIZED ; "a_body_never_sent_with_a_wrong_token")]
    #[test_case(head(&format!("Content-Length: {BODY_NEVER_SENT}\r\n")), UNAUTHORIZED ; "no_token")]
    #[test_case(head(&format!("Authorization: Bearer {TOKEN}\r\nContent-Length: many\r\n")), BAD_REQUEST ; "a_length_that_is_no_number")]
    #[test_case(head(&format!("Authorization: Bearer wrong\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\n")), BAD_REQUEST ; "duplicate_authorization")]
    #[test_case(head(&format!("Authorization: Bearer {TOKEN}\r\nMissing colon\r\nContent-Length: 0\r\n")), BAD_REQUEST ; "missing_header_separator")]
    #[test_case(head(&format!("Authorization: Bearer {TOKEN}\r\nContent-Length : 0\r\n")), BAD_REQUEST ; "whitespace_in_header_name")]
    #[test_case(two_lengths(), BAD_REQUEST ; "a_second_length")]
    #[test_case(signed_length(), BAD_REQUEST ; "a_signed_length")]
    #[test_case(head(&format!("Authorization: Bearer {TOKEN}\r\nContent-Length: {BODY_NEVER_SENT}\r\n")), BAD_REQUEST ; "a_body_cut_short")]
    #[test_case(head(&format!("X-Padding: {}\r\n", "a".repeat(MAX_HEAD_BYTES))), HEADERS_TOO_LARGE ; "an_oversized_head")]
    #[test_case(post(&json!([{ "jsonrpc": "2.0", "id": 1, "method": TOOLS_CALL, "params": { "name": TOOL, "arguments": {} } }]), TOKEN), BAD_REQUEST ; "a_batch")]
    #[test_case(post(&json!({ "jsonrpc": "2.0", "id": 1, "method": null }), TOKEN), BAD_REQUEST ; "a_null_method")]
    #[test_case(post(&json!({ "jsonrpc": "2.0", "id": 1, "method": 42 }), TOKEN), BAD_REQUEST ; "a_numeric_method")]
    #[test_case(post(&json!({ "jsonrpc": "2.0", "id": 1 }), TOKEN), BAD_REQUEST ; "a_missing_method")]
    fn a_request_maki_will_not_take_is_refused(request: Vec<u8>, status: &str) {
        let (reply, handoffs) = reply_to(&request);
        assert!(reply.starts_with(&format!("HTTP/1.1 {status}")), "{reply}");
        assert!(handoffs.is_empty(), "no call must be held: {handoffs:?}");
    }

    #[test]
    fn an_unknown_method_is_a_protocol_error() {
        let (reply, _) = reply_to(&post(
            &json!({ "jsonrpc": "2.0", "id": "probe", "method": "server/discover" }),
            TOKEN,
        ));
        assert!(reply.contains(&METHOD_NOT_FOUND.to_string()), "{reply}");
    }

    /// The client's version if maki knows it, or else maki's newest.
    #[test_case(OLDER_VERSION => OLDER_VERSION ; "one_maki_speaks")]
    #[test_case(UNKNOWN_VERSION => PROTOCOL_VERSIONS[0] ; "one_maki_does_not")]
    fn initialize_answers_a_protocol_version_maki_speaks(asked: &str) -> String {
        let (reply, _) = reply_to(&post(
            &json!({ "jsonrpc": "2.0", "id": 0, "method": INITIALIZE, "params": { "protocolVersion": asked } }),
            TOKEN,
        ));
        let body: Value = serde_json::from_str(&reply[reply.find('{').unwrap()..]).unwrap();
        body["result"]["protocolVersion"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A call sent as a notification expects no answer, so it stops the
    /// request and never runs.
    #[test]
    fn a_call_without_an_id_ends_the_request() {
        let call = json!({ "jsonrpc": "2.0", "method": TOOLS_CALL, "params": { "name": TOOL, "arguments": {} } });
        let (reply, handoffs) = reply_to(&post(&call, TOKEN));
        assert!(reply.contains(ACCEPTED), "{reply}");
        assert!(
            matches!(handoffs.as_slice(), [Handoff::Invalid(Error::NoCallId)]),
            "{handoffs:?}"
        );
    }

    #[test]
    fn repeated_call_notifications_queue_only_one_failure() {
        const NOTIFICATIONS: usize = 128;
        let (sender, handoffs) = flume::unbounded();
        let shared = Shared::new(TOKEN.into(), tools(), sender, WAIT);
        let call = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "method": TOOLS_CALL, "params": { "name": TOOL, "arguments": {} } })).unwrap();
        for _ in 0..NOTIFICATIONS {
            assert!(matches!(
                answer(&shared, &call),
                Reply::Send {
                    status: ACCEPTED,
                    ..
                }
            ));
        }
        let handoffs: Vec<_> = handoffs.try_iter().collect();
        assert!(
            matches!(handoffs.as_slice(), [Handoff::Invalid(Error::NoCallId)]),
            "{handoffs:?}"
        );
    }
}
