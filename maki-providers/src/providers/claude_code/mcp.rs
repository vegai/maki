//! One request owns this loopback MCP server. The server holds tool calls until maki stops
//! Claude Code and executes the tools.
//!
//! Only clients on 127.0.0.1 with the request's bearer token can connect.

use std::future::Future;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use futures_lite::{FutureExt, Stream, StreamExt, future};
use serde::Serialize;
use serde_json::{Value, json};
use smol::lock::Semaphore;
use smol::net::{TcpListener, TcpStream};
use smol::{Task, Timer};
use tracing::warn;

use super::error::Error;

const ENDPOINT: &str = "/mcp";
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// A `write` call carries a whole file, so the body limit is large.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;
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
    tools: Vec<McpTool>,
    handoffs: Sender<Handoff>,
    read_limit: Duration,
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
    let shared = Arc::new(Shared {
        token,
        tools,
        handoffs,
        read_limit,
    });
    let server_shared = Arc::clone(&shared);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = smol::spawn(async move { accept_loop(listener.incoming(), shared, slots).await });
    Ok(Server {
        port,
        shared: server_shared,
        _accept: accept,
    })
}

async fn accept_loop(
    mut incoming: impl Stream<Item = io::Result<TcpStream>> + Unpin,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
) {
    let mut connections: Vec<Task<()>> = Vec::new();
    loop {
        // Past the limit, a connection waits in the backlog for a slot.
        let slot = slots.acquire_arc().await;
        let stream = match incoming.next().await {
            Some(Ok(stream)) => stream,
            // A connection that closed before the accept, or a brief shortage
            // of descriptors, must not stop the task that holds every call.
            Some(Err(error)) => {
                warn!(%error, "claude-code: the handoff server cannot accept a connection");
                Timer::after(ACCEPT_RETRY).await;
                continue;
            }
            None => return,
        };
        connections.retain(|task| !task.is_finished());
        let shared = Arc::clone(&shared);
        connections.push(smol::spawn(async move {
            connection(stream, shared).await;
            drop(slot);
        }));
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
async fn connection(stream: TcpStream, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stream.clone());
    let mut writer = stream;
    loop {
        // A rejected request's body is never read, so the connection ends.
        let head = match within(shared.read_limit, read_head(&mut reader)).await {
            Some(Ok(Some(head))) => head,
            Some(Ok(None)) | None => return,
            Some(Err(status)) => {
                let _ = writer.write_all(&response(status, "")).await;
                return;
            }
        };
        let body = match admit(&shared, &head) {
            Ok(()) => match within(shared.read_limit, read_body(&mut reader, head.length)).await {
                Some(body) => body,
                None => return,
            },
            Err(status) => Err(status),
        };
        let reply = match body {
            Ok(body) => answer(&shared, &body),
            Err(status) => {
                let _ = writer.write_all(&response(status, "")).await;
                return;
            }
        };
        match reply {
            Reply::Send { status, body } => {
                if writer.write_all(&response(status, &body)).await.is_err() || head.close {
                    return;
                }
            }
            Reply::Park => future::pending::<()>().await,
        }
    }
}

async fn within<T>(limit: Duration, read: impl Future<Output = T>) -> Option<T> {
    async { Some(read.await) }
        .or(async {
            Timer::after(limit).await;
            None
        })
        .await
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
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            // With two lengths, different readers could find different ends
            // of the body.
            CONTENT_LENGTH if length.is_some() => return Err(BAD_REQUEST),
            CONTENT_LENGTH => length = Some(plain_length(value).ok_or(BAD_REQUEST)?),
            AUTHORIZATION => authorization = Some(value.to_owned()),
            TRANSFER_ENCODING => return Err(LENGTH_REQUIRED),
            CONNECTION => close = value.eq_ignore_ascii_case(CLOSE),
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
    let Ok(message @ Value::Object(_)) = serde_json::from_slice::<Value>(body) else {
        return plain(BAD_REQUEST);
    };
    let Some(method) = message["method"].as_str() else {
        return plain(ACCEPTED);
    };
    let Some(id) = message.get("id") else {
        // A call sent as a notification expects no answer, so Claude Code
        // would continue without maki's result, and the call stops the
        // request.
        if method == TOOLS_CALL {
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
        TOOLS_LIST => json!({ "tools": shared.tools }),
        PING => json!({}),
        TOOLS_CALL => return park(shared, &message["params"]),
        _ => return rpc_error(id, METHOD_NOT_FOUND, METHOD_NOT_FOUND_MESSAGE),
    };
    let body = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    Reply::Send {
        status: OK,
        body: body.to_string(),
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
fn park(shared: &Shared, params: &Value) -> Reply {
    let name = params["name"].as_str().unwrap_or_default();
    let handoff = match &params["arguments"] {
        _ if !shared.tools.iter().any(|tool| tool.name == name) => {
            Handoff::Invalid(Error::NotOffered(params["name"].clone()))
        }
        Value::Object(_) => Handoff::Parked {
            tool_use_id: params["_meta"][TOOL_USE_ID_META]
                .as_str()
                .map(str::to_owned),
            name: name.to_owned(),
            arguments: params["arguments"].clone(),
        },
        other => Handoff::Invalid(Error::NotArguments {
            name: name.to_owned(),
            input: other.clone(),
        }),
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
    use std::time::Duration;

    use flume::Receiver;
    use futures_lite::FutureExt;
    use futures_lite::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use futures_lite::{StreamExt, stream};
    use serde_json::{Value, json};
    use smol::Timer;
    use smol::lock::Semaphore;
    use smol::net::{TcpListener, TcpStream};
    use test_case::test_case;

    use super::super::error::Error;
    use super::{
        ACCEPTED, BAD_REQUEST, CONTENT_TOO_LARGE, ENDPOINT, HEADERS_TOO_LARGE, Handoff, INITIALIZE,
        LENGTH_REQUIRED, MAX_BODY_BYTES, MAX_CONNECTIONS, MAX_HEAD_BYTES, METHOD_NOT_ALLOWED,
        METHOD_NOT_FOUND, McpTool, NOT_FOUND, OK, PING, PROTOCOL_VERSIONS, Server, Shared,
        TOOL_USE_ID_META, TOOLS_CALL, TOOLS_LIST, UNAUTHORIZED, accept_loop, serve,
    };

    const TOKEN: &str = "secret-token";
    const OLDER_VERSION: &str = "2025-06-18";
    const UNKNOWN_VERSION: &str = "2099-01-01";
    const TOOL: &str = "read";
    const TOOL_USE_ID: &str = "toolu_1";
    /// A failed handoff must fail the test so the suite can continue.
    const WAIT: Duration = Duration::from_secs(5);
    /// A body length the request announces but never sends.
    const BODY_NEVER_SENT: usize = 1000;
    /// A generous deadline limits only failed test requests.
    const SHORT_READ_LIMIT: Duration = Duration::from_millis(500);

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
        async { Some(fut.await) }
            .or(async {
                Timer::after(WAIT).await;
                None
            })
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

    /// After an accept error (a connection closed before the accept, or a
    /// brief shortage of descriptors) the server still serves the next one.
    #[test]
    fn a_failed_accept_keeps_the_server_serving() {
        smol::block_on(async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, _handoffs) = flume::unbounded();
            let shared = Arc::new(Shared {
                token: TOKEN.into(),
                tools: tools(),
                handoffs: tx,
                read_limit: WAIT,
            });
            let _serving = smol::spawn(async move {
                let aborted = stream::once(Err(io::Error::from(io::ErrorKind::ConnectionAborted)));
                let incoming = aborted.chain(listener.incoming());
                accept_loop(incoming, shared, Arc::new(Semaphore::new(1))).await;
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

    /// Silent connections hold slots only until the read limit. The
    /// connection beyond the limit waits for a slot and is not dropped.
    #[test]
    fn idle_connections_give_their_slots_back() {
        smol::block_on(async {
            let (tx, _handoffs) = flume::unbounded();
            let server = serve(TOKEN.into(), tools(), tx, SHORT_READ_LIMIT)
                .await
                .unwrap();
            let address = (Ipv4Addr::LOCALHOST, server.port);
            let mut idle = Vec::new();
            for _ in 0..MAX_CONNECTIONS {
                idle.push(TcpStream::connect(address).await.unwrap());
            }
            let mut stream = TcpStream::connect(address).await.unwrap();
            let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": PING });
            stream.write_all(&post(&ping, TOKEN)).await.unwrap();

            let mut status = String::new();
            bounded(BufReader::new(stream).read_line(&mut status))
                .await
                .unwrap();
            assert!(status.contains(OK), "{status:?}");
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

    #[test_case(json!({ "name": "bash", "arguments": {} }) => matches Handoff::Invalid(Error::NotOffered(_)) ; "an_unknown_tool")]
    #[test_case(json!({ "name": TOOL, "arguments": "rm -rf /" }) => matches Handoff::Invalid(Error::NotArguments { .. }) ; "arguments_that_are_no_object")]
    fn an_invalid_call_is_reported_and_still_never_answered(params: Value) -> Handoff {
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
    #[test_case(two_lengths(), BAD_REQUEST ; "a_second_length")]
    #[test_case(signed_length(), BAD_REQUEST ; "a_signed_length")]
    #[test_case(head(&format!("Authorization: Bearer {TOKEN}\r\nContent-Length: {BODY_NEVER_SENT}\r\n")), BAD_REQUEST ; "a_body_cut_short")]
    #[test_case(head(&format!("X-Padding: {}\r\n", "a".repeat(MAX_HEAD_BYTES))), HEADERS_TOO_LARGE ; "an_oversized_head")]
    #[test_case(post(&json!([{ "jsonrpc": "2.0", "id": 1, "method": TOOLS_CALL, "params": { "name": TOOL, "arguments": {} } }]), TOKEN), BAD_REQUEST ; "a_batch")]
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
}
