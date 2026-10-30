//! Test-only raw-TCP mock HTTP server: serves scripted responses in request
//! order and records every request (method, path, headers) for assertions.
//! Supports keep-alive, mid-body connection drops (stream errors), and
//! connection drops without any response (network errors). Loopback only.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub enum MockAction {
    /// A complete response; the connection stays open for the next request.
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// A response whose declared Content-Length exceeds the bytes actually
    /// sent; the connection is then dropped, so the client's body stream
    /// errors mid-way.
    PartialThenDrop {
        status: u16,
        headers: Vec<(String, String)>,
        prefix: Vec<u8>,
        declared_len: u64,
    },
    /// No response at all: the connection is dropped, so the client sees a
    /// network error on send/receive.
    Drop,
}

impl MockAction {
    pub fn ok_json(body: serde_json::Value) -> MockAction {
        MockAction::Respond {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: body.to_string().into_bytes(),
        }
    }

    pub fn status(status: u16) -> MockAction {
        MockAction::Respond {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    pub fn with_headers(status: u16, headers: &[(&str, &str)]) -> MockAction {
        MockAction::Respond {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: vec![],
        }
    }

    pub fn with_json_body(
        status: u16,
        headers: &[(&str, &str)],
        body: serde_json::Value,
    ) -> MockAction {
        MockAction::Respond {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_string().into_bytes(),
        }
    }

    pub fn bytes(body: &[u8]) -> MockAction {
        MockAction::Respond {
            status: 200,
            headers: vec![],
            body: body.to_vec(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct MockServer {
    pub addr: SocketAddr,
    actions: Arc<Mutex<VecDeque<MockAction>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockServer {
    pub async fn start(actions: Vec<MockAction>) -> MockServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = MockServer {
            addr,
            actions: Arc::new(Mutex::new(actions.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let actions = server.actions.clone();
        let requests = server.requests.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let actions = actions.clone();
                let requests = requests.clone();
                tokio::spawn(async move {
                    serve_conn(sock, actions, requests).await;
                });
            }
        });
        server
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Append a scripted action (useful when the action needs this server's URL).
    pub fn push(&self, action: MockAction) {
        self.actions.lock().unwrap().push_back(action);
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn serve_conn(
    mut sock: TcpStream,
    actions: Arc<Mutex<VecDeque<MockAction>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    loop {
        let req = match read_request(&mut sock).await {
            Some(r) => r,
            None => return,
        };
        requests.lock().unwrap().push(req);
        let action = actions.lock().unwrap().pop_front();
        match action {
            Some(MockAction::Respond {
                status,
                headers,
                body,
            }) => {
                if write_response(&mut sock, status, &headers, &body, true)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some(MockAction::PartialThenDrop {
                status,
                headers,
                prefix,
                declared_len,
            }) => {
                let mut headers = headers;
                headers.push(("content-length".to_string(), declared_len.to_string()));
                let _ = write_response(&mut sock, status, &headers, &prefix, false).await;
                return; // drop the socket mid-body
            }
            Some(MockAction::Drop) => return, // drop without responding
            None => {
                // Queue exhausted: fail loudly so tests notice extra requests.
                let _ = write_response(
                    &mut sock,
                    500,
                    &[("content-type".to_string(), "text/plain".to_string())],
                    b"mock queue exhausted",
                    false,
                )
                .await;
                return;
            }
        }
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn read_request(sock: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 1024 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    // Consume any request body so the next read starts on a request boundary.
    let body_start = header_end + 4;
    let mut remaining = content_length.saturating_sub(buf.len().saturating_sub(body_start));
    while remaining > 0 {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        remaining = remaining.saturating_sub(n);
    }
    Some(RecordedRequest {
        method,
        path,
        headers,
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

async fn write_response(
    sock: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    keep_alive: bool,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    let mut has_len = false;
    let mut has_conn = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_len = true;
        }
        if k.eq_ignore_ascii_case("connection") {
            has_conn = true;
        }
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    let no_body = status == 204 || status == 304 || (100..200).contains(&status);
    if !has_len && !no_body {
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    if !has_conn {
        head.push_str(if keep_alive {
            "connection: keep-alive\r\n"
        } else {
            "connection: close\r\n"
        });
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await?;
    if !body.is_empty() && !no_body {
        sock.write_all(body).await?;
    }
    sock.flush().await?;
    Ok(())
}
