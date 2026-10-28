//! A Model Context Protocol client for stdio servers, configured like other MCP clients:
//! `{"mcpServers": {"name": {"command": "...", "args": [...], "env": {...}}}}` in
//! `.ember/mcp.json` (project) or `~/.config/ember/mcp.json` (everywhere).

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::llm::ToolSpec;

const PROTOCOL: &str = "2025-06-18";

#[derive(Clone, Debug, Deserialize)]
struct ServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl Tool {
    /// The name the model sees, e.g. `mcp__github__create_issue`.
    pub fn qualified(&self) -> String {
        format!("mcp__{}__{}", self.server, self.name)
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

struct Server {
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Child,
}

impl Server {
    async fn spawn(name: &str, config: &ServerConfig, cwd: &Path) -> Result<Server> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .envs(&config.env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start MCP server {name} ({})", config.command))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stdin = Arc::new(AsyncMutex::new(stdin));
        let server = Server { stdin: stdin.clone(), pending: pending.clone(), next_id: AtomicU64::new(1), _child: child };

        // Reader: route responses to their waiters; answer server requests (ping); ignore notifications.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                if msg.get("method").is_some() {
                    if let Some(id) = msg.get("id").cloned() {
                        let reply = if msg["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported"}})
                        };
                        let mut w = stdin.lock().await;
                        let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = w.flush().await;
                    }
                    continue;
                }
                let Some(id) = msg["id"].as_u64() else { continue };
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                    let result = match msg.get("error") {
                        Some(err) => Err(anyhow!("{}", err["message"].as_str().unwrap_or("MCP error"))),
                        None => Ok(msg["result"].clone()),
                    };
                    let _ = tx.send(result);
                }
            }
            // The server exited: fail anything still waiting.
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("MCP server exited")));
            }
        });
        Ok(server)
    }

    async fn send(&self, message: Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{message}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("MCP server dropped the request"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("MCP request {method} timed out")
            }
        }
    }
}

#[derive(Default)]
pub struct Mcp {
    servers: Mutex<HashMap<String, Arc<Server>>>,
    tools: Mutex<Vec<Tool>>,
    /// One line per configured server, for /mcp.
    status: Mutex<Vec<String>>,
}

fn read_configs(cwd: &Path) -> Vec<(String, ServerConfig)> {
    let mut found: HashMap<String, ServerConfig> = HashMap::new();
    let global = crate::config::user_env_file().and_then(|f| f.parent().map(|d| d.join("mcp.json")));
    for path in [global, Some(cwd.join(".ember").join("mcp.json"))].into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Ok(servers) = serde_json::from_value::<HashMap<String, ServerConfig>>(v["mcpServers"].clone()) {
            found.extend(servers);
        }
    }
    let mut list: Vec<_> = found.into_iter().collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list
}

impl Mcp {
    /// Start every configured server and collect its tools. Failures are recorded in the status, not fatal.
    pub async fn start(self: Arc<Self>, cwd: &Path) {
        for (name, config) in read_configs(cwd) {
            let line = match self.connect(&name, &config, cwd).await {
                Ok(count) => format!("✓ {name}: {count} tools"),
                Err(e) => format!("✗ {name}: {e:#}"),
            };
            self.status.lock().unwrap().push(line);
        }
    }

    async fn connect(&self, name: &str, config: &ServerConfig, cwd: &Path) -> Result<usize> {
        let server = Server::spawn(name, config, cwd).await?;
        let init = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "ember", "version": env!("CARGO_PKG_VERSION")},
        });
        server.request("initialize", init, Duration::from_secs(30)).await.context("initialize failed")?;
        server.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
            let page = server.request("tools/list", params, Duration::from_secs(30)).await?;
            for t in page["tools"].as_array().into_iter().flatten() {
                let Some(tool_name) = t["name"].as_str() else { continue };
                tools.push(Tool {
                    server: name.to_string(),
                    name: tool_name.to_string(),
                    description: t["description"].as_str().unwrap_or("").to_string(),
                    schema: t.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
                });
            }
            cursor = page["nextCursor"].as_str().map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        let count = tools.len();
        self.tools.lock().unwrap().extend(tools);
        self.servers.lock().unwrap().insert(name.to_string(), Arc::new(server));
        Ok(count)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .map(|t| ToolSpec::new(&t.qualified(), &format!("[MCP: {}] {}", t.server, t.description), t.schema.clone()))
            .collect()
    }

    pub fn status(&self) -> Vec<String> {
        self.status.lock().unwrap().clone()
    }

    pub async fn call(&self, qualified: &str, args: Value) -> Result<String> {
        let tool = self
            .tools
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.qualified() == qualified)
            .cloned()
            .ok_or_else(|| anyhow!("unknown MCP tool {qualified}"))?;
        let server = self.servers.lock().unwrap().get(&tool.server).cloned().ok_or_else(|| anyhow!("server {} is not running", tool.server))?;
        let result = server
            .request("tools/call", json!({"name": tool.name, "arguments": args}), Duration::from_secs(180))
            .await?;
        let mut out = String::new();
        for item in result["content"].as_array().into_iter().flatten() {
            match item["type"].as_str() {
                Some("text") => out.push_str(item["text"].as_str().unwrap_or("")),
                Some(other) => out.push_str(&format!("[{other} content omitted]")),
                None => {}
            }
            out.push('\n');
        }
        if result["isError"].as_bool() == Some(true) {
            bail!("{}", out.trim());
        }
        Ok(crate::tools::truncate(out.trim(), 30_000))
    }
}
