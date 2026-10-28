//! The interactive agent loop: stream a reply, run the tools it asks for, feed results back.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};

use crate::fix::FixEvent;
use crate::llm::{Client, Delta, Message, Request, ToolCall};
use crate::tools::{self, Workspace};

const MAX_STEPS: usize = 80;

pub type Events = mpsc::UnboundedSender<AgentEvent>;

pub enum AgentEvent {
    Text(String),
    Reasoning(String),
    /// A model reply finished streaming.
    Reply { content: String, thought_secs: Option<f32> },
    ToolStart { summary: String },
    ToolDone { name: String, summary: String, output: String, ok: bool },
    Approval(ApprovalRequest),
    Usage { model: String, prompt: u64, completion: u64 },
    Notice(String),
    Fix(FixEvent),
    Error(String),
    /// The current task (turn, /fix, ...) ended.
    Done,
}

pub struct ApprovalRequest {
    pub summary: String,
    pub preview: Option<String>,
    pub reply: oneshot::Sender<Decision>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Yes,
    Always,
    No,
}

/// Ask the UI to approve an action; dropping the request counts as "no".
pub async fn ask(tx: &Events, summary: String, preview: Option<String>) -> Decision {
    let (reply, rx) = oneshot::channel();
    if tx.send(AgentEvent::Approval(ApprovalRequest { summary, preview, reply })).is_err() {
        return Decision::No;
    }
    rx.await.unwrap_or(Decision::No)
}

pub struct Agent {
    pub client: Client,
    pub model: Arc<Mutex<String>>,
    pub auto_approve: Arc<AtomicBool>,
    pub ws: Workspace,
    messages: Vec<Message>,
    always: HashSet<String>,
}

impl Agent {
    pub fn new(client: Client, model: Arc<Mutex<String>>, auto_approve: Arc<AtomicBool>, ws: Workspace) -> Self {
        let mut agent = Agent { client, model, auto_approve, ws, messages: vec![], always: HashSet::new() };
        agent.reset();
        agent
    }

    pub fn reset(&mut self) {
        self.messages = vec![Message::system(system_prompt(&self.ws.root))];
        self.always.clear();
    }

    /// After a cancelled turn, answer any tool calls left without results so the history stays valid.
    fn repair(&mut self) {
        let Some(pos) = self.messages.iter().rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty()) else {
            return;
        };
        let answered: HashSet<String> =
            self.messages[pos + 1..].iter().filter_map(|m| m.tool_call_id.clone()).collect();
        let missing: Vec<String> = self.messages[pos]
            .tool_calls
            .iter()
            .filter(|c| !answered.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        for id in missing {
            self.messages.push(Message::tool(id, "Cancelled: the user interrupted before this ran."));
        }
    }

    pub async fn turn(&mut self, input: String, tx: &Events) -> Result<()> {
        self.repair();
        self.messages.push(Message::user(input));
        for _ in 0..MAX_STEPS {
            let model = self.model.lock().unwrap().clone();
            let request = Request {
                model: model.clone(),
                messages: self.messages.clone(),
                tools: tools::specs(false),
                max_tokens: Some(8192),
                ..Default::default()
            };
            let resp = self
                .client
                .stream(&request, |delta| {
                    let _ = tx.send(match delta {
                        Delta::Text(t) => AgentEvent::Text(t.to_string()),
                        Delta::Reasoning(r) => AgentEvent::Reasoning(r.to_string()),
                    });
                })
                .await?;
            let _ = tx.send(AgentEvent::Usage {
                model,
                prompt: resp.usage.prompt_tokens,
                completion: resp.usage.completion_tokens,
            });
            let _ = tx.send(AgentEvent::Reply {
                content: resp.content.clone(),
                thought_secs: (!resp.reasoning.is_empty()).then(|| resp.elapsed.as_secs_f32()),
            });
            self.messages.push(Message::assistant(resp.content, resp.tool_calls.clone()));
            if resp.tool_calls.is_empty() {
                return Ok(());
            }
            for call in &resp.tool_calls {
                let result = self.execute(call, tx).await;
                self.messages.push(Message::tool(call.id.clone(), result));
            }
        }
        let _ = tx.send(AgentEvent::Notice(format!("Stopped after {MAX_STEPS} steps; say \"continue\" to keep going.")));
        Ok(())
    }

    async fn execute(&mut self, call: &ToolCall, tx: &Events) -> String {
        let name = call.function.name.as_str();
        let args = call.args();
        let summary = tools::summary(name, &args);
        if tools::mutates(name) && !self.auto_approve.load(Ordering::Relaxed) && !self.always.contains(name) {
            match ask(tx, summary.clone(), tools::preview(&self.ws, name, &args)).await {
                Decision::Yes => {}
                Decision::Always => {
                    self.always.insert(name.to_string());
                }
                Decision::No => {
                    let _ = tx.send(AgentEvent::ToolDone {
                        name: name.into(),
                        summary,
                        output: "Denied by user".into(),
                        ok: false,
                    });
                    return "The user declined this action. Stop and ask them how they would like to proceed.".into();
                }
            }
        }
        let _ = tx.send(AgentEvent::ToolStart { summary: summary.clone() });
        let (output, ok) = match tools::run(&self.ws, name, &args).await {
            Ok(out) => (out, true),
            Err(err) => (format!("Error: {err}"), false),
        };
        let _ = tx.send(AgentEvent::ToolDone { name: name.into(), summary, output: output.clone(), ok });
        debug_log(name, &args, &output);
        output
    }
}

fn command_line(dir: &Path, cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).current_dir(dir).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn system_prompt(root: &Path) -> String {
    let date = command_line(root, "date", &["+%Y-%m-%d"]).unwrap_or_default();
    let git = match command_line(root, "git", &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Some(branch) => {
            let status = command_line(root, "git", &["status", "--short"]).unwrap_or_default();
            let status: String = status.lines().take(20).collect::<Vec<_>>().join("\n");
            format!("Git branch: {branch}\nGit status:\n{}", if status.is_empty() { "(clean)".into() } else { status })
        }
        None => "Not a git repository.".into(),
    };
    let instructions = ["AGENTS.md", "EMBER.md"]
        .iter()
        .find_map(|name| std::fs::read_to_string(root.join(name)).ok().map(|t| (name, t)))
        .map(|(name, text)| format!("\n\n# Project instructions ({name})\n{}", tools::truncate(&text, 12_000)))
        .unwrap_or_default();

    format!(
        "You are ember, a coding agent working in the user's terminal, powered by NVIDIA Nemotron.\n\
         You help with software engineering: understanding code, fixing bugs, adding features, refactoring, \
         and running builds and tests.\n\n\
         # How to work\n\
         - Explore before you change anything: use list_files, grep and read_file to find the relevant code. \
           Never guess file contents or paths.\n\
         - Make focused changes with edit_file, matching the existing style. Use write_file only for new files.\n\
         - After changing code, build or run the relevant tests with bash and fix what you broke.\n\
         - Mutating actions ask the user for approval. If they decline, stop and ask how to proceed.\n\
         - Do not commit, push, or delete files unless asked.\n\n\
         # How to communicate\n\
         - Be concise and direct. Use Markdown; put code, commands and paths in backticks.\n\
         - When you finish a task, briefly say what you changed and how you verified it, including anything \
           that failed or that you could not check.\n\n\
         # Environment\n\
         Working directory: {}\nOS: {}\nDate: {date}\n{git}{instructions}",
        root.display(),
        std::env::consts::OS,
    )
}

/// Append tool calls to the file named by EMBER_LOG (for debugging model behavior).
fn debug_log(name: &str, args: &serde_json::Value, output: &str) {
    let Ok(path) = std::env::var("EMBER_LOG") else { return };
    let entry = serde_json::json!({"tool": name, "args": args, "output": tools::truncate(output, 2000)});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{entry}");
    }
}
