//! Streaming client for OpenAI-compatible chat completions (Nemotron on Nebius Token Factory).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content.into())
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content.into())
    }
    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Message { tool_calls, ..Self::new("assistant", content.into()) }
    }
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Message { tool_call_id: Some(id.into()), ..Self::new("tool", content.into()) }
    }
    fn new(role: &str, content: String) -> Self {
        Message { role: role.into(), content, tool_calls: vec![], tool_call_id: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// Parsed arguments; malformed JSON becomes an empty object so the tool can report the problem.
    pub fn args(&self) -> Value {
        serde_json::from_str(&self.function.arguments).unwrap_or_else(|_| json!({}))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDef,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn new(name: &str, description: &str, parameters: Value) -> Self {
        ToolSpec {
            kind: "function",
            function: FunctionDef { name: name.into(), description: description.into(), parameters },
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<Value>,
}

/// How much a Nemotron model should reason before answering.
/// Measured on Token Factory: `enable_thinking: false` turns reasoning off on every Nemotron 3 model;
/// `reasoning_effort: "low"` shortens it on Super (471 → 7 reasoning tokens on a commit message) but
/// not reliably on Ultra or VL. A `/no_think` system prompt is ignored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Think {
    /// The model's own default (reasoning on).
    #[default]
    On,
    Low,
    Off,
}

impl Think {
    pub fn parse(s: &str) -> Option<Think> {
        match s.to_ascii_lowercase().as_str() {
            "on" | "default" | "high" => Some(Think::On),
            "low" => Some(Think::Low),
            "off" | "none" => Some(Think::Off),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Think::On => "on",
            Think::Low => "low",
            Think::Off => "off",
        }
    }
}

impl Request {
    pub fn think(mut self, think: Think) -> Self {
        match think {
            Think::On => {}
            Think::Low => self.reasoning_effort = Some("low"),
            Think::Off => self.chat_template_kwargs = Some(json!({"enable_thinking": false})),
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// Prompt tokens served from Token Factory's prefix cache (billed at a lower rate).
    #[serde(default)]
    pub cached_tokens: u64,
}

impl Usage {
    /// Parse a `usage` object, picking up cached tokens from either field the API uses.
    pub fn from_json(v: &Value) -> Usage {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        Usage {
            prompt_tokens: n(&v["prompt_tokens"]),
            completion_tokens: n(&v["completion_tokens"]),
            cached_tokens: n(&v["prompt_cache_hit_tokens"]).max(n(&v["prompt_tokens_details"]["cached_tokens"])),
        }
    }
}

#[derive(Debug, Default)]
pub struct Response {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    pub elapsed: Duration,
}

pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl Client {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Client {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
            base_url: format!("{}/", base_url.trim_end_matches('/')),
            api_key,
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self.auth(self.http.get(format!("{}models", self.base_url)))?.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            bail!("API error {status}: {body}");
        }
        let mut ids: Vec<String> = body["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
            .unwrap_or_default();
        ids.sort();
        Ok(ids)
    }

    /// A non-streaming request with a caller-built body (e.g. multimodal content). Returns text and usage.
    pub async fn complete_raw(&self, body: Value) -> Result<(String, Usage)> {
        let resp = self.auth(self.http.post(format!("{}chat/completions", self.base_url)))?.json(&body).send().await?;
        let status = resp.status();
        let value: Value = resp.json().await?;
        if !status.is_success() {
            bail!("API error {status}: {value}");
        }
        let message = &value["choices"][0]["message"];
        let mut splitter = ThinkSplitter::default();
        let mut text: String = splitter
            .feed(message["content"].as_str().unwrap_or(""))
            .into_iter()
            .filter(|(visible, _)| *visible)
            .map(|(_, t)| t)
            .collect();
        let (visible, rest) = splitter.flush();
        if visible {
            text.push_str(&rest);
        }
        let usage = Usage::from_json(&value["usage"]);
        Ok((text.trim().to_string(), usage))
    }

    /// Attach the API key. Token Factory requires one; a local server (EMBER_BASE_URL) may not.
    fn auth(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        match &self.api_key {
            Some(key) => Ok(req.bearer_auth(key)),
            None if self.base_url.contains("nebius.com") => {
                Err(anyhow!("NEBIUS_API_KEY is not set (put it in .env or ~/.config/ember/env)"))
            }
            None => Ok(req),
        }
    }

    /// Stream a chat completion; `on_delta` sees visible text and reasoning as it arrives.
    pub async fn stream(&self, req: &Request, mut on_delta: impl FnMut(Delta)) -> Result<Response> {
        let mut body = serde_json::to_value(req)?;
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});

        let mut attempt = 0;
        let resp = loop {
            let resp = self
                .auth(self.http.post(format!("{}chat/completions", self.base_url)))?
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if status.is_success() {
                break resp;
            }
            let text = resp.text().await.unwrap_or_default();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                attempt += 1;
                continue;
            }
            bail!("API error {status}: {}", text.trim());
        };

        let started = Instant::now();
        let mut out = Response::default();
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        let mut splitter = ThinkSplitter::default();
        let mut pending = Vec::<u8>::new();
        let mut stream = resp.bytes_stream();

        let mut handle = |kind_is_text: bool, text: &str, out: &mut Response| {
            if text.is_empty() {
                return;
            }
            if kind_is_text {
                out.content.push_str(text);
                on_delta(Delta::Text(text));
            } else {
                out.reasoning.push_str(text);
                on_delta(Delta::Reasoning(text));
            }
        };

        'outer: while let Some(chunk) = stream.next().await {
            pending.extend_from_slice(&chunk?);
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim().strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                if v["usage"].is_object() {
                    out.usage = Usage::from_json(&v["usage"]);
                }
                let Some(choices) = v["choices"].as_array() else { continue };
                for choice in choices {
                    let delta = &choice["delta"];
                    if let Some(r) = delta["reasoning_content"].as_str() {
                        handle(false, r, &mut out);
                    }
                    if let Some(c) = delta["content"].as_str() {
                        for (is_text, piece) in splitter.feed(c) {
                            handle(is_text, &piece, &mut out);
                        }
                    }
                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            let idx = tc["index"].as_u64().unwrap_or(calls.len() as u64);
                            let call = calls.entry(idx).or_insert_with(|| ToolCall {
                                id: String::new(),
                                kind: "function".into(),
                                function: FunctionCall::default(),
                            });
                            if let Some(id) = tc["id"].as_str() {
                                call.id = id.to_string();
                            }
                            if let Some(name) = tc["function"]["name"].as_str() {
                                call.function.name.push_str(name);
                            }
                            if let Some(args) = tc["function"]["arguments"].as_str() {
                                call.function.arguments.push_str(args);
                            }
                        }
                    }
                    if let Some(reason) = choice["finish_reason"].as_str() {
                        out.finish_reason = Some(reason.to_string());
                    }
                }
            }
        }
        let (is_text, rest) = splitter.flush();
        handle(is_text, &rest, &mut out);

        for (i, (_, mut call)) in calls.into_iter().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{}_{i}", started.elapsed().as_nanos());
            }
            if call.function.arguments.trim().is_empty() {
                call.function.arguments = "{}".into();
            }
            out.tool_calls.push(call);
        }
        out.content = out.content.trim().to_string();
        out.reasoning = out.reasoning.trim().to_string();
        out.elapsed = started.elapsed();
        Ok(out)
    }
}

/// Splits inline `<think>…</think>` reasoning out of visible text, even when tags straddle chunks.
#[derive(Default)]
struct ThinkSplitter {
    buf: String,
    thinking: bool,
}

impl ThinkSplitter {
    fn feed(&mut self, text: &str) -> Vec<(bool, String)> {
        self.buf.push_str(text);
        let mut parts = vec![];
        loop {
            let tag = if self.thinking { "</think>" } else { "<think>" };
            if let Some(i) = self.buf.find(tag) {
                parts.push((!self.thinking, self.buf[..i].to_string()));
                self.buf.drain(..i + tag.len());
                self.thinking = !self.thinking;
                continue;
            }
            // Keep back a possible partial tag at the end.
            let keep = (1..tag.len())
                .rev()
                .find(|&n| self.buf.ends_with(&tag[..n]))
                .unwrap_or(0);
            let cut = self.buf.len() - keep;
            parts.push((!self.thinking, self.buf[..cut].to_string()));
            self.buf.drain(..cut);
            return parts;
        }
    }

    fn flush(&mut self) -> (bool, String) {
        (!self.thinking, std::mem::take(&mut self.buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&str]) -> (String, String) {
        let mut s = ThinkSplitter::default();
        let (mut text, mut think) = (String::new(), String::new());
        let mut push = |parts: Vec<(bool, String)>| {
            for (is_text, p) in parts {
                if is_text { text.push_str(&p) } else { think.push_str(&p) }
            }
        };
        for c in chunks {
            push(s.feed(c));
        }
        push(vec![s.flush()]);
        (text, think)
    }

    #[test]
    fn splits_think_tags_across_chunks() {
        assert_eq!(collect(&["<thi", "nk>plan</th", "ink>Hello"]), ("Hello".into(), "plan".into()));
        assert_eq!(collect(&["no tags at all"]), ("no tags at all".into(), "".into()));
        assert_eq!(collect(&["a < b and ", "<think>x</think>c"]), ("a < b and c".into(), "x".into()));
    }
}
