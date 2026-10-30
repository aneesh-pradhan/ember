//! A Model Context Protocol client for stdio servers, configured like other MCP clients:
//! `{"mcpServers": {"name": {"command": "...", "args": [...], "env": {...}}}}` in
//! `.ember/mcp.json` (project) or `~/.config/ember/mcp.json` (everywhere). A project's servers only start once the user
//! trusts that exact configuration with `/mcp trust`.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
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
    /// One line per configured server, by name, for /mcp.
    status: Mutex<BTreeMap<String, String>>,
}

/// A file's `mcpServers` object; null when the file is missing or isn't JSON.
fn read_servers(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str::<Value>(&text).map(|v| v["mcpServers"].clone()).unwrap_or(Value::Null)
}

fn parse(servers: &Value) -> HashMap<String, ServerConfig> {
    serde_json::from_value(servers.clone()).unwrap_or_default()
}

fn config_dir() -> Option<PathBuf> {
    crate::config::user_env_file()?.parent().map(Path::to_path_buf)
}

/// `mcp-trust.json` in ember's config directory: for each project, the `mcpServers` the user trusted with `/mcp trust`.
fn trust_file(config: &Path) -> PathBuf {
    config.join("mcp-trust.json")
}

#[derive(Default, Serialize, Deserialize)]
struct Trusted {
    #[serde(default)]
    projects: BTreeMap<String, Value>,
}

/// Whether `file` trusts exactly these servers for the project at `root`. Any change to them needs `/mcp trust` again.
fn is_trusted(file: &Path, root: &Path, servers: &Value) -> bool {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let trusted: Trusted = serde_json::from_str(&text).unwrap_or_default();
    trusted.projects.get(&crate::permissions::key(root)) == Some(servers)
}

fn save_trust(file: &Path, root: &Path, servers: &Value) -> Result<()> {
    // Refuse to overwrite a hand-edited file that no longer parses: it holds other projects' entries too.
    let mut trusted: Trusted = match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} isn't valid JSON; fix or delete it", file.display()))?,
        Err(_) => Trusted::default(),
    };
    trusted.projects.insert(crate::permissions::key(root), servers.clone());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, serde_json::to_string_pretty(&trusted)? + "\n").with_context(|| format!("writing {}", file.display()))?;
    // The servers' env can hold tokens.
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// The project's `.ember/mcp.json` servers, and whether the user trusted them. A cloned repository must not be able to
/// start its own commands just because ember was opened in it.
fn project_servers(config: Option<&Path>, cwd: &Path) -> (Value, bool) {
    let servers = read_servers(&cwd.join(".ember").join("mcp.json"));
    let trusted = parse(&servers).is_empty() || config.is_some_and(|dir| is_trusted(&trust_file(dir), cwd, &servers));
    (servers, trusted)
}

/// The project's MCP servers that won't start until `/mcp trust`, as (name, command line).
pub fn untrusted(cwd: &Path) -> Vec<(String, String)> {
    let (servers, trusted) = project_servers(config_dir().as_deref(), cwd);
    if trusted {
        return vec![];
    }
    let mut list: Vec<_> = parse(&servers).into_iter().map(|(name, c)| (name, [vec![c.command], c.args].concat().join(" "))).collect();
    list.sort();
    list
}

/// Trust the project's current `.ember/mcp.json` and return its servers, for `Mcp::start_servers`.
pub fn trust(cwd: &Path) -> Result<Value> {
    let config = config_dir().context("couldn't find a config directory")?;
    let (servers, _) = project_servers(Some(&config), cwd);
    if parse(&servers).is_empty() {
        bail!("this project has no MCP servers in .ember/mcp.json");
    }
    save_trust(&trust_file(&config), cwd, &servers)?;
    Ok(servers)
}

impl Mcp {
    /// Start the user's servers and the project's trusted ones, and collect their tools. Failures are recorded in the
    /// status, not fatal.
    pub async fn start(self: Arc<Self>, cwd: &Path) {
        self.start_in(config_dir().as_deref(), cwd).await;
    }

    /// `start`, with ember's config directory (user servers and trust) passed in.
    async fn start_in(&self, config: Option<&Path>, cwd: &Path) {
        let mut configs = parse(&config.map_or(Value::Null, |dir| read_servers(&dir.join("mcp.json"))));
        let (project, trusted) = project_servers(config, cwd);
        for (name, server) in parse(&project) {
            if trusted {
                configs.insert(name, server);
            } else if !configs.contains_key(&name) {
                let line = format!("· {name}: not started; this project's .ember/mcp.json isn't trusted yet (/mcp trust)");
                self.status.lock().unwrap().insert(name, line);
            }
        }
        self.connect_all(configs, cwd).await;
    }

    /// Start servers from `servers` (an `mcpServers` object) that aren't running yet, e.g. after `/mcp trust`.
    pub async fn start_servers(self: Arc<Self>, servers: Value, cwd: &Path) {
        let mut configs = parse(&servers);
        configs.retain(|name, _| !self.servers.lock().unwrap().contains_key(name));
        self.connect_all(configs, cwd).await;
    }

    async fn connect_all(&self, configs: HashMap<String, ServerConfig>, cwd: &Path) {
        let mut configs: Vec<_> = configs.into_iter().collect();
        configs.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, config) in configs {
            let line = match self.connect(&name, &config, cwd).await {
                Ok(count) => format!("✓ {name}: {count} tools"),
                Err(e) => format!("✗ {name}: {e:#}"),
            };
            self.status.lock().unwrap().insert(name, line);
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
        self.status.lock().unwrap().values().cloned().collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn project_servers_start_only_once_trusted() {
        let dir = std::env::temp_dir().join(format!("ember-mcp-trust-{}", std::process::id()));
        let (config, project) = (dir.join("config"), dir.join("project"));
        std::fs::create_dir_all(project.join(".ember")).unwrap();
        let marker = dir.join("started");
        let write_config = |args: &str| {
            let servers = json!({"mcpServers": {"evil": {"command": "sh", "args": ["-c", args]}}});
            std::fs::write(project.join(".ember/mcp.json"), servers.to_string()).unwrap();
        };
        write_config(&format!("touch {}", marker.display()));

        let mcp = Mcp::default();
        mcp.start_in(Some(&config), &project).await;
        assert!(!marker.exists(), "an untrusted project server must not run");
        assert!(mcp.status()[0].contains("isn't trusted yet"), "{:?}", mcp.status());
        assert!(!project_servers(Some(&config), &project).1);

        let (servers, _) = project_servers(Some(&config), &project);
        save_trust(&trust_file(&config), &project, &servers).unwrap();
        assert!(project_servers(Some(&config), &project).1, "trusted");
        assert!(project_servers(Some(&config), &project.join(".")).1, "keyed by the canonical path");
        let mode = std::fs::metadata(trust_file(&config)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the servers' env can hold tokens");
        Mcp::default().start_in(Some(&config), &project).await;
        assert!(marker.exists(), "a trusted server starts");

        write_config("echo changed");
        assert!(!project_servers(Some(&config), &project).1, "any change needs /mcp trust again");

        std::fs::write(trust_file(&config), "not json").unwrap();
        assert!(!project_servers(Some(&config), &project).1, "an unreadable trust file trusts nothing");
        assert!(save_trust(&trust_file(&config), &project, &servers).is_err(), "and isn't overwritten");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
