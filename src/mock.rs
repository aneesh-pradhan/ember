//! A scripted model server for tests. It speaks Token Factory's OpenAI-compatible streaming API, plays back canned
//! replies in order, and records every request body, so tests can drive the real agent loop end to end with no
//! network or API key, and check what ember sent.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted model reply.
pub enum Reply {
    /// A finished text answer.
    Text(&'static str),
    /// Tool calls, as (name, arguments). Arguments are sent verbatim, so a test can send broken JSON.
    Tools(Vec<(&'static str, String)>),
    /// Text that stops at the output token limit (`finish_reason: "length"`).
    CutOff(&'static str),
}

/// A tool call reply with JSON arguments.
pub fn call(name: &'static str, args: Value) -> (&'static str, String) {
    (name, args.to_string())
}

pub struct MockModel {
    /// The base URL to give `Client::new`.
    pub url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockModel {
    pub async fn start(replies: Vec<Reply>) -> MockModel {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock model");
        let url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let script = Arc::new(Mutex::new(VecDeque::from(replies)));
        let recorded = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (recorded, script) = (recorded.clone(), script.clone());
                tokio::spawn(async move {
                    let Some(body) = read_request(&mut socket).await else { return };
                    recorded.lock().unwrap().push(body);
                    let reply = script.lock().unwrap().pop_front().unwrap_or(Reply::Text("MOCK: out of scripted replies"));
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                        sse(&reply)
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        MockModel { url, requests }
    }

    /// Every request body received so far, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// Read one HTTP request and return its JSON body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&buf[header_end..]).ok()
}

/// The reply as server-sent events: content split over two chunks, the finish reason, usage, and [DONE].
fn sse(reply: &Reply) -> String {
    let choice = |delta: Value, finish: Option<&str>| json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    let mut events = vec![];
    match reply {
        Reply::Text(text) | Reply::CutOff(text) => {
            let (a, b) = text.split_at(text.len() / 2);
            events.push(choice(json!({"content": a}), None));
            events.push(choice(json!({"content": b}), None));
            events.push(choice(json!({}), Some(if matches!(reply, Reply::CutOff(_)) { "length" } else { "stop" })));
        }
        Reply::Tools(calls) => {
            for (i, (name, args)) in calls.iter().enumerate() {
                events.push(choice(
                    json!({"tool_calls": [{"index": i, "id": format!("call_{i}"), "type": "function",
                                           "function": {"name": name, "arguments": args}}]}),
                    None,
                ));
            }
            events.push(choice(json!({}), Some("tool_calls")));
        }
    }
    events.push(json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10,
                                                "prompt_tokens_details": {"cached_tokens": 0}}}));
    events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}
