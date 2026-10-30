//! The interactive agent loop: stream a reply, run the tools it asks for, feed results back.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::config::{context_window, vision_model};
use crate::fix::FixEvent;
use crate::llm::{Client, Delta, FunctionCall, Message, Request, Think, ToolCall, ToolSpec};
use crate::mcp::Mcp;
use crate::session::{self, Session};
use crate::tools::{self, Workspace};

/// Model calls per turn, unless EMBER_MAX_STEPS or `--max-steps` says otherwise.
const DEFAULT_MAX_STEPS: usize = 80;
/// How many steps before the limit the model is told to wrap up, so a long task ends with a summary of what's done
/// and what's left instead of stopping mid-edit.
const WRAP_UP_STEPS: usize = 5;
/// Compact automatically once the conversation fills this share of the context window.
const AUTO_COMPACT_AT: f64 = 0.8;
const COMPACT_ACK: &str = "Got it. I'll continue from that summary.";

pub type Events = mpsc::UnboundedSender<AgentEvent>;

/// How much the agent may do on its own. Cycled with Shift+Tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Ask before every edit and command.
    Ask,
    /// Run edits and commands without asking.
    Auto,
    /// Read-only: investigate and propose a plan.
    Plan,
}

impl Mode {
    pub fn next(self) -> Mode {
        match self {
            Mode::Ask => Mode::Auto,
            Mode::Auto => Mode::Plan,
            Mode::Plan => Mode::Ask,
        }
    }
}

/// The current mode, shared between the UI and a running turn.
#[derive(Clone)]
pub struct SharedMode(Arc<AtomicU8>);

impl SharedMode {
    pub fn new(mode: Mode) -> Self {
        let shared = SharedMode(Arc::new(AtomicU8::new(0)));
        shared.set(mode);
        shared
    }
    pub fn get(&self) -> Mode {
        match self.0.load(Ordering::Relaxed) {
            1 => Mode::Auto,
            2 => Mode::Plan,
            _ => Mode::Ask,
        }
    }
    pub fn set(&self, mode: Mode) {
        self.0.store(mode as u8, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Todo {
    pub content: String,
    #[serde(default)]
    pub status: String,
}

pub enum AgentEvent {
    Text(String),
    Reasoning(String),
    /// A model request failed and is being retried: drop what streamed so far.
    Retrying(String),
    /// A model reply finished streaming.
    Reply { content: String, thought_secs: Option<f32> },
    ToolStart { summary: String },
    ToolDone { name: String, summary: String, output: String, ok: bool },
    Approval(ApprovalRequest),
    Usage { model: String, prompt: u64, completion: u64, cached: u64 },
    /// How full the main conversation's context window is.
    Context { used: u64, window: u64 },
    Todos(Vec<Todo>),
    Notice(String),
    Fix(FixEvent),
    Error(String),
    /// The current task (turn, /fix, ...) ended.
    Done,
}

pub struct ApprovalRequest {
    pub summary: String,
    pub preview: Option<String>,
    /// What "always" would allow (e.g. "`cargo test` commands"), or None if it isn't offered.
    pub always: Option<String>,
    pub reply: oneshot::Sender<Decision>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Yes,
    Always,
    No,
}

/// Ask the UI to approve an action; dropping the request counts as "no".
pub async fn ask(tx: &Events, summary: String, preview: Option<String>, always: Option<String>) -> Decision {
    let (reply, rx) = oneshot::channel();
    if tx.send(AgentEvent::Approval(ApprovalRequest { summary, preview, always, reply })).is_err() {
        return Decision::No;
    }
    rx.await.unwrap_or(Decision::No)
}

/// The host a `web_fetch` call reads from, lowercased; None for a URL that doesn't parse or has no host.
fn fetch_host(args: &serde_json::Value) -> Option<String> {
    let url = reqwest::Url::parse(args["url"].as_str()?.trim()).ok()?;
    url.host_str().map(str::to_ascii_lowercase)
}

/// The part of a shell command that "always allow" applies to: the program, plus its subcommand
/// for tools like cargo or git. Commands that chain or redirect are never auto-approved.
pub fn command_prefix(command: &str) -> Option<String> {
    if command.contains(['&', ';', '|', '>', '<', '`', '$', '\n']) {
        return None;
    }
    let mut words = command.split_whitespace();
    let first = words.next()?;
    match words.next() {
        Some(second) if !second.starts_with('-') && second.chars().all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c)) => {
            Some(format!("{first} {second}"))
        }
        _ => Some(first.to_string()),
    }
}

/// The tool a reply hands the turn back with. Follow-up messages are sent as its result instead of as a new user
/// message: on Token Factory a new user message misses the prompt cache for the whole conversation (and Nemotron's
/// chat template re-renders earlier replies once a newer user message exists), while a tool result only extends
/// the prompt, so the history stays cached.
const ASK_USER: &str = "ask_user";

/// Sent when a reply stops at the output token limit without a tool call, so the model finishes instead of
/// leaving the turn half-answered.
const CUT_OFF_NOTE: &str = "(Your last reply was cut off at the output token limit. Continue from where it stopped, \
                            more concisely.)";
const MAX_CONTINUATIONS: usize = 2;

/// Why ember can't run a tool call as sent, worded for the model: an unknown tool, or arguments that aren't a JSON
/// object (often because the reply hit the token limit partway through the call). Plan mode explains its own refusals.
fn check_call(call: &ToolCall, specs: &[ToolSpec], plan: bool, cut_off: bool) -> Option<String> {
    let name = call.function.name.as_str();
    if !plan && !specs.iter().any(|s| s.function.name == name) {
        let names: Vec<&str> = specs.iter().map(|s| s.function.name.as_str()).collect();
        return Some(format!("Error: there is no tool named `{name}`. Available tools: {}.", names.join(", ")));
    }
    if !serde_json::from_str::<serde_json::Value>(&call.function.arguments).is_ok_and(|v| v.is_object()) {
        let why = if cut_off { " Your reply hit the output token limit partway through this call." } else { "" };
        return Some(format!(
            "Error: the arguments for `{name}` aren't a valid JSON object.{why} Send the call again, and split large \
             edits or file writes into smaller calls."
        ));
    }
    None
}

fn ask_user_spec() -> ToolSpec {
    ToolSpec::new(
        ASK_USER,
        "Ask the user a question and wait for their answer, when you can't continue without their input. Their next \
         message comes back as this tool's result.",
        json!({"type": "object", "properties": {"question": {"type": "string"}}}),
    )
}

fn view_image_spec() -> ToolSpec {
    ToolSpec::new(
        "view_image",
        "Look at an image file in the workspace (screenshot, diagram, mockup) using a vision model. Returns a \
         detailed description including any text in it.",
        json!({"type": "object", "properties": {
            "path": {"type": "string"},
            "question": {"type": "string", "description": "What you need to know from the image"}},
            "required": ["path"]}),
    )
}

fn todo_spec() -> ToolSpec {
    ToolSpec::new(
        "update_todos",
        "Create or update your task list for multi-step work. Send the complete list every time. Keep exactly one \
         item in_progress while working, and mark items completed as soon as they are done. Skip it for simple \
         one-step requests.",
        json!({"type": "object", "properties": {"todos": {"type": "array", "items": {"type": "object", "properties": {
            "content": {"type": "string", "description": "Short imperative description, e.g. 'Add tests for format_money'"},
            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}},
            "required": ["content", "status"]}}}, "required": ["todos"]}),
    )
}

/// File contents from before a turn's edits, so the turn can be undone.
struct Checkpoint {
    label: String,
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

pub struct Agent {
    pub client: Client,
    /// Tools from MCP servers; filled in the background after startup.
    pub mcp: Arc<Mcp>,
    pub model: Arc<Mutex<String>>,
    /// Reasoning level for the interactive agent (/think).
    pub think: Arc<Mutex<Think>>,
    pub mode: SharedMode,
    pub ws: Workspace,
    messages: Vec<Message>,
    always_tools: HashSet<String>,
    always_commands: HashSet<String>,
    session_id: String,
    title: String,
    last_prompt_tokens: u64,
    checkpoints: Vec<Checkpoint>,
    /// Send each step's reasoning back to the model within the task (EMBER_KEEP_REASONING=0 turns it off).
    keep_reasoning: bool,
    /// Offer `ask_user` and send follow-ups as its result (the full-screen UI; print mode has a single turn).
    interactive: bool,
    /// The model for /compact summaries; the current model when unset.
    draft_model: Option<String>,
    /// Model calls allowed per turn.
    max_steps: usize,
    /// Where approvals are saved per project (`/permissions save`); None when there's no config directory.
    permissions_file: Option<PathBuf>,
}

impl Agent {
    pub fn new(client: Client, model: Arc<Mutex<String>>, mode: SharedMode, ws: Workspace) -> Self {
        let mut agent = Agent {
            client,
            mcp: Arc::new(Mcp::default()),
            model,
            think: Arc::new(Mutex::new(Think::On)),
            mode,
            ws,
            messages: vec![],
            always_tools: HashSet::new(),
            always_commands: HashSet::new(),
            session_id: String::new(),
            title: String::new(),
            last_prompt_tokens: 0,
            checkpoints: vec![],
            keep_reasoning: !matches!(std::env::var("EMBER_KEEP_REASONING").as_deref(), Ok("0" | "false" | "off")),
            interactive: false,
            draft_model: None,
            max_steps: std::env::var("EMBER_MAX_STEPS").ok().and_then(|n| n.parse().ok()).unwrap_or(DEFAULT_MAX_STEPS).max(1),
            permissions_file: crate::permissions::file(),
        };
        agent.reset();
        agent
    }

    pub fn reset(&mut self) {
        self.messages = vec![Message::system(system_prompt(&self.ws.root))];
        let saved = self.permissions_file.as_deref().map(|f| crate::permissions::load_from(f, &self.ws.root)).unwrap_or_default();
        self.always_tools = saved.tools.into_iter().collect();
        self.always_commands = saved.commands.into_iter().collect();
        self.session_id = session::new_id();
        self.title.clear();
        self.last_prompt_tokens = 0;
    }

    /// Continue a saved conversation, with a fresh system prompt for the current state of the repo.
    pub fn resume(&mut self, saved: Session) -> usize {
        self.reset();
        self.session_id = saved.id;
        self.title = saved.title;
        self.messages.extend(saved.messages.into_iter().filter(|m| m.role != "system"));
        self.messages.len() - 1
    }

    /// For the full-screen UI: offer `ask_user` and send follow-up messages as its result.
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    pub fn with_max_steps(mut self, steps: Option<usize>) -> Self {
        if let Some(steps) = steps {
            self.max_steps = steps.max(1);
        }
        self
    }

    pub fn with_draft_model(mut self, model: String) -> Self {
        self.draft_model = Some(model).filter(|m| !m.is_empty());
        self
    }

    /// What runs without asking right now: approvals from this session plus the project's saved ones.
    pub fn allowed(&self) -> crate::permissions::Allowed {
        crate::permissions::Allowed {
            commands: self.always_commands.iter().cloned().collect(),
            tools: self.always_tools.iter().cloned().collect(),
        }
    }

    /// Keep this session's approvals for the project (`/permissions save`), or forget the saved ones and this
    /// session's (`/permissions forget`). Returns the file they're saved in.
    pub fn save_permissions(&mut self, keep: bool) -> Result<PathBuf> {
        if !keep {
            self.always_commands.clear();
            self.always_tools.clear();
        }
        let file = self.permissions_file.clone().context("couldn't find a config directory")?;
        crate::permissions::save_to(&file, &self.ws.root, &self.allowed())?;
        Ok(file)
    }

    #[cfg(test)]
    fn with_permissions_file(mut self, file: PathBuf) -> Self {
        self.permissions_file = Some(file);
        self.reset();
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The last assistant reply with text, for showing where a resumed session left off.
    pub fn last_reply(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.trim().is_empty() && m.content != COMPACT_ACK)
            .map(|m| m.content.clone())
    }

    fn save(&self) {
        if self.messages.len() <= 1 {
            return;
        }
        let _ = session::save(&Session {
            id: self.session_id.clone(),
            cwd: self.ws.root.display().to_string(),
            updated: session::now(),
            title: self.title.clone(),
            messages: self.messages.clone(),
        });
    }

    /// After a cancelled turn, answer any tool calls left without results so the history stays valid.
    fn repair(&mut self) {
        self.answer_missing(true);
    }

    /// Like `repair`, but leaves an open `ask_user` call for `push_input` to answer when `include_ask` is false.
    fn answer_missing(&mut self, include_ask: bool) {
        let Some(pos) = self.messages.iter().rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty()) else {
            return;
        };
        let answered: HashSet<String> =
            self.messages[pos + 1..].iter().filter_map(|m| m.tool_call_id.clone()).collect();
        let missing: Vec<(String, bool)> = self.messages[pos]
            .tool_calls
            .iter()
            .filter(|c| !answered.contains(&c.id) && (include_ask || c.function.name != ASK_USER))
            .map(|c| (c.id.clone(), c.function.name == ASK_USER))
            .collect();
        for (id, ask) in missing {
            let text = if ask { "(The user moved on without answering.)" } else { "Cancelled: the user interrupted before this ran." };
            self.messages.push(Message::tool(id, text));
        }
    }

    /// Add the user's message. In the UI, after the first turn it answers the last reply's `ask_user` call
    /// (adding one to a plain text reply, which no request has included yet), so the prompt only grows.
    fn push_input(&mut self, content: String) {
        if self.interactive {
            if let Some(id) = self.open_ask_user() {
                self.messages.push(Message::tool(id, content));
                return;
            }
        }
        self.messages.push(Message::user(content));
    }

    /// The id of an unanswered `ask_user` call on the last reply, adding one if the turn ended with plain text.
    fn open_ask_user(&mut self) -> Option<String> {
        let pos = self.messages.iter().rposition(|m| m.role == "assistant")?;
        if self.messages[pos + 1..].iter().any(|m| m.role != "tool") {
            return None;
        }
        let answered: HashSet<String> =
            self.messages[pos + 1..].iter().filter_map(|m| m.tool_call_id.clone()).collect();
        let reply = &mut self.messages[pos];
        if let Some(call) = reply.tool_calls.iter().find(|c| c.function.name == ASK_USER && !answered.contains(&c.id)) {
            return Some(call.id.clone());
        }
        if !reply.tool_calls.is_empty() {
            return None;
        }
        let id = format!("ask_user_{pos}");
        reply.tool_calls.push(ToolCall {
            id: id.clone(),
            kind: "function".into(),
            function: FunctionCall { name: ASK_USER.into(), arguments: "{}".into() },
        });
        Some(id)
    }

    /// Record a command the user ran themselves with `!`, so the model can see it.
    pub fn note_shell(&mut self, command: &str, output: &str) {
        self.repair();
        self.messages.push(Message::user(format!(
            "(I ran this in my shell; no reply needed yet.)\n$ {command}\n```\n{}\n```",
            tools::truncate(output, 20_000)
        )));
        self.save();
    }

    pub async fn turn(&mut self, input: String, tx: &Events) -> Result<()> {
        self.answer_missing(!self.interactive);
        let model = self.model.lock().unwrap().clone();
        if self.last_prompt_tokens as f64 > AUTO_COMPACT_AT * context_window(&model) as f64 {
            let _ = tx.send(AgentEvent::Notice("Context is nearly full; compacting the conversation first…".into()));
            self.compact(tx).await?;
        }
        if self.title.is_empty() {
            self.title = tools::one_line(&input, 80);
        }
        let plan = self.mode.get() == Mode::Plan;
        let content = if plan {
            format!(
                "{input}\n\n[Plan mode is on: you may only read files and update your task list. Investigate as \
                 needed, then reply with a concise, numbered implementation plan (files to change, approach, how to \
                 verify). Do not try to edit files or run commands.]"
            )
        } else {
            input
        };
        self.checkpoints.push(Checkpoint { label: self.title_for(&content), files: vec![] });
        self.push_input(content);
        let result = self.run_steps(tx, plan).await;
        // Say where new files went: the model often names them only relative to a working directory the user
        // may not have in mind (it would rather write "saved as index.html" than give the path).
        if let Some(checkpoint) = self.checkpoints.last() {
            let created: Vec<String> = checkpoint
                .files
                .iter()
                .filter(|(path, before)| before.is_none() && path.is_file())
                .map(|(path, _)| path.display().to_string())
                .collect();
            if !created.is_empty() {
                let _ = tx.send(AgentEvent::Notice(format!("New files: {}", created.join(", "))));
            }
        }
        if self.checkpoints.last().is_some_and(|c| c.files.is_empty()) {
            self.checkpoints.pop();
        }
        if self.checkpoints.len() > 20 {
            self.checkpoints.remove(0);
        }
        self.save();
        result
    }

    async fn run_steps(&mut self, tx: &Events, plan: bool) -> Result<()> {
        // The tool list is fixed for the whole turn: changing it mid-turn would change the prompt prefix
        // and miss Token Factory's prompt cache. MCP servers that finish starting later join next turn.
        let mut specs = tools::specs(plan);
        specs.push(todo_spec());
        specs.push(view_image_spec());
        if self.interactive {
            specs.push(ask_user_spec());
        }
        if !plan {
            specs.extend(self.mcp.specs());
        }
        let mut continuations = 0;
        for step in 1..=self.max_steps {
            let model = self.model.lock().unwrap().clone();
            let think = *self.think.lock().unwrap();
            let request = Request {
                model: model.clone(),
                messages: self.messages.clone(),
                tools: specs.clone(),
                max_tokens: Some(8192),
                ..Default::default()
            }
            .think(think);
            let resp = self
                .client
                .stream(&request, |delta| {
                    let _ = tx.send(match delta {
                        Delta::Text(t) => AgentEvent::Text(t.to_string()),
                        Delta::Reasoning(r) => AgentEvent::Reasoning(r.to_string()),
                        Delta::Retry(why) => AgentEvent::Retrying(why.to_string()),
                    });
                })
                .await?;
            self.last_prompt_tokens = resp.usage.prompt_tokens + resp.usage.completion_tokens;
            let _ = tx.send(AgentEvent::Usage {
                model: model.clone(),
                prompt: resp.usage.prompt_tokens,
                completion: resp.usage.completion_tokens,
                cached: resp.usage.cached_tokens,
            });
            let _ = tx.send(AgentEvent::Context { used: self.last_prompt_tokens, window: context_window(&model) });
            let _ = tx.send(AgentEvent::Reply {
                content: resp.content.clone(),
                thought_secs: (!resp.reasoning.is_empty()).then_some(resp.elapsed.as_secs_f32()),
            });
            let mut reply = Message::assistant(resp.content, resp.tool_calls.clone());
            if self.keep_reasoning && !resp.reasoning.is_empty() {
                reply.reasoning_content = Some(resp.reasoning.clone());
            }
            self.messages.push(reply);
            let cut_off = resp.finish_reason.as_deref() == Some("length");
            if resp.tool_calls.is_empty() {
                if cut_off && continuations < MAX_CONTINUATIONS {
                    continuations += 1;
                    self.messages.push(Message::user(CUT_OFF_NOTE));
                    continue;
                }
                return Ok(());
            }
            let mut asked = false;
            for call in &resp.tool_calls {
                if let Some(error) = check_call(call, &specs, plan, cut_off) {
                    self.messages.push(Message::tool(call.id.clone(), error));
                    continue;
                }
                if call.function.name == ASK_USER {
                    // Answered by the user's next message (see push_input).
                    let question = call.args()["question"].as_str().unwrap_or("").trim().to_string();
                    if !question.is_empty() {
                        let _ = tx.send(AgentEvent::Reply { content: question, thought_secs: None });
                    }
                    asked = true;
                    continue;
                }
                let result = self.execute(call, tx, plan).await;
                self.messages.push(Message::tool(call.id.clone(), result));
            }
            if asked {
                return Ok(());
            }
            // Appended to this step's last tool result rather than sent as a new user message, so the request
            // still only extends the previous one (see ASK_USER).
            if self.max_steps - step == WRAP_UP_STEPS {
                if let Some(last) = self.messages.last_mut().filter(|m| m.role == "tool") {
                    last.content.push_str(&format!(
                        "\n\n(ember: {WRAP_UP_STEPS} steps left in this turn. Finish the change you're making, then reply \
                         with a summary of what's done and what still needs doing.)"
                    ));
                }
            }
        }
        let steps = self.max_steps;
        let _ = tx.send(AgentEvent::Notice(format!("Stopped after {steps} steps; say \"continue\" to keep going.")));
        Ok(())
    }

    fn title_for(&self, input: &str) -> String {
        tools::one_line(input, 60)
    }

    /// Remember each file's contents before its first edit in this turn (every file in a batched edit).
    fn snapshot(&mut self, name: &str, args: &serde_json::Value) {
        let paths: Vec<String> = match name {
            "edit_file" => tools::edit_list(args).into_iter().map(|e| e.path).collect(),
            "write_file" => tools::write_list(args).into_iter().map(|f| f.path).collect(),
            _ => return,
        };
        let Some(checkpoint) = self.checkpoints.last_mut() else { return };
        for path in paths.iter().filter_map(|p| self.ws.resolve(p).ok()) {
            if !checkpoint.files.iter().any(|(p, _)| *p == path) {
                checkpoint.files.push((path.clone(), std::fs::read(&path).ok()));
            }
        }
    }

    /// Restore the files changed by the most recent turn that changed files.
    /// Returns that turn's label and the files restored. Shell commands are not undone.
    pub fn undo(&mut self) -> Option<(String, Vec<String>)> {
        let checkpoint = self.checkpoints.pop()?;
        let mut restored = vec![];
        for (path, original) in checkpoint.files.iter().rev() {
            let ok = match original {
                Some(bytes) => std::fs::write(path, bytes).is_ok(),
                // Snapshotted before a write that then failed: nothing to undo.
                None if !path.exists() => continue,
                None => std::fs::remove_file(path).is_ok(),
            };
            if ok {
                restored.push(self.ws.rel(path));
            }
        }
        restored.reverse();
        self.repair();
        self.messages.push(Message::user(format!(
            "(I undid the file changes from my request \"{}\": {} are back to how they were before. Shell commands \
             were not undone.)",
            checkpoint.label,
            restored.join(", ")
        )));
        self.save();
        Some((checkpoint.label, restored))
    }

    async fn execute(&mut self, call: &ToolCall, tx: &Events, plan: bool) -> String {
        let name = call.function.name.as_str();
        let args = call.args();

        if name == "update_todos" {
            return match serde_json::from_value::<Vec<Todo>>(args["todos"].clone()) {
                Ok(todos) => {
                    let _ = tx.send(AgentEvent::Todos(todos));
                    "Task list updated.".into()
                }
                Err(e) => format!("Error: invalid todos: {e}"),
            };
        }
        if name == "view_image" {
            let summary = tools::summary(name, &args);
            let _ = tx.send(AgentEvent::ToolStart { summary: summary.clone() });
            let question = args["question"].as_str().unwrap_or("Describe the image.");
            let (output, ok) = match self.ws.resolve(args["path"].as_str().unwrap_or("")) {
                Ok(path) => match self.look_at(&path, question, tx).await {
                    Ok(text) => (text, true),
                    Err(e) => (format!("Error: {e:#}"), false),
                },
                Err(e) => (format!("Error: {e}"), false),
            };
            let _ = tx.send(AgentEvent::ToolDone { name: name.into(), summary, output: output.clone(), ok });
            return output;
        }
        if plan && tools::mutates(name) {
            return "Plan mode is on, so this action is not available. Finish your plan instead.".into();
        }

        if let Some(rest) = name.strip_prefix("mcp__") {
            let summary = rest.replacen("__", ": ", 1);
            if plan {
                return "Plan mode is on, so MCP tools are not available. Finish your plan instead.".into();
            }
            if self.mode.get() != Mode::Auto && !self.always_tools.contains(name) {
                let preview = Some(serde_json::to_string_pretty(&args).unwrap_or_default());
                match ask(tx, format!("Call MCP tool {summary}"), preview, Some(format!("`{summary}` calls"))).await {
                    Decision::Yes => {}
                    Decision::Always => {
                        self.always_tools.insert(name.to_string());
                    }
                    Decision::No => {
                        let _ = tx.send(AgentEvent::ToolDone { name: name.into(), summary, output: "Denied by user".into(), ok: false });
                        return "The user declined this action. Stop and ask them how they would like to proceed.".into();
                    }
                }
            }
            let _ = tx.send(AgentEvent::ToolStart { summary: summary.clone() });
            let (output, ok) = match self.mcp.call(name, args.clone()).await {
                Ok(out) => (out, true),
                Err(e) => (format!("Error: {e:#}"), false),
            };
            let _ = tx.send(AgentEvent::ToolDone { name: name.into(), summary, output: output.clone(), ok });
            debug_log(name, &args, &output);
            return output;
        }
        let summary = tools::summary(name, &args);
        let command = args["command"].as_str().unwrap_or("");
        let prefix = (name == "bash").then(|| command_prefix(command)).flatten();
        // A fetched URL can carry anything the model has read, so fetches ask first; "always" covers one host.
        let fetch_key = (name == "web_fetch").then(|| fetch_host(&args)).flatten().map(|h| format!("web_fetch:{h}"));
        let pre_approved = match (&prefix, &fetch_key) {
            (Some(p), _) => self.always_commands.contains(p),
            (_, Some(k)) => self.always_tools.contains(k),
            _ => !matches!(name, "bash" | "web_fetch") && self.always_tools.contains(name),
        };
        if (tools::mutates(name) || name == "web_fetch") && self.mode.get() != Mode::Auto && !pre_approved {
            let always = match name {
                "bash" => prefix.as_ref().map(|p| format!("`{p}` commands")),
                "web_fetch" => fetch_key.as_ref().map(|k| format!("fetches from {}", &k["web_fetch:".len()..])),
                _ => Some("file edits".to_string()),
            };
            match ask(tx, summary.clone(), tools::preview(&self.ws, name, &args), always).await {
                Decision::Yes => {}
                Decision::Always => {
                    if let Some(p) = prefix {
                        self.always_commands.insert(p);
                    } else if let Some(k) = fetch_key {
                        self.always_tools.insert(k);
                    } else if !matches!(name, "bash" | "web_fetch") {
                        self.always_tools.extend(["edit_file".to_string(), "write_file".to_string()]);
                    }
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
        self.snapshot(name, &args);
        let _ = tx.send(AgentEvent::ToolStart { summary: summary.clone() });
        let (output, ok) = match tools::run(&self.ws, name, &args).await {
            Ok(out) => (out, true),
            Err(err) => (format!("Error: {err}"), false),
        };
        let _ = tx.send(AgentEvent::ToolDone { name: name.into(), summary, output: output.clone(), ok });
        debug_log(name, &args, &output);
        output
    }

    async fn look_at(&self, path: &Path, request: &str, tx: &Events) -> Result<String> {
        let model = vision_model();
        let (text, usage) = crate::vision::describe(&self.client, &model, path, request).await?;
        let _ = tx.send(AgentEvent::Usage { model, prompt: usage.prompt_tokens, completion: usage.completion_tokens, cached: usage.cached_tokens });
        Ok(text)
    }

    /// Describe images the user attached, as text blocks to append to their message.
    pub async fn describe_images(&self, images: &[PathBuf], request: &str, tx: &Events) -> String {
        let mut out = String::new();
        for path in images {
            let rel = self.ws.rel(path);
            let summary = format!("Look at {rel}");
            let _ = tx.send(AgentEvent::ToolStart { summary: summary.clone() });
            match self.look_at(path, request, tx).await {
                Ok(text) => {
                    let _ = tx.send(AgentEvent::ToolDone { name: "view_image".into(), summary, output: text.clone(), ok: true });
                    out.push_str(&format!("\n\n<image path=\"{rel}\" described-by=\"Nemotron VL\">\n{text}\n</image>"));
                }
                Err(e) => {
                    let _ = tx.send(AgentEvent::ToolDone { name: "view_image".into(), summary, output: format!("{e:#}"), ok: false });
                }
            }
        }
        out
    }

    /// Review uncommitted changes with `model` (the reasoning tier). The review joins the conversation,
    /// so the user can follow up with "fix the first issue".
    pub async fn review(&mut self, model: &str, tx: &Events) -> Result<()> {
        let material = crate::git::review_material(&self.ws.root).await?;
        if material.trim().is_empty() {
            let _ = tx.send(AgentEvent::Notice("No uncommitted changes to review.".into()));
            return Ok(());
        }
        self.repair();
        self.messages.push(Message::user(format!(
            "Review my uncommitted changes as a careful senior engineer. Report, most severe first: bugs and \
             correctness problems, security issues, missing error handling or tests, then smaller suggestions. \
             Number the findings, cite file and line where you can, and be concrete. Don't restate what the diff \
             does. If it looks good, say so briefly.\n\n{material}"
        )));
        let resp = self
            .client
            .stream(
                &Request { model: model.to_string(), messages: self.messages.clone(), max_tokens: Some(8192), ..Default::default() },
                |delta| {
                    let _ = tx.send(match delta {
                        Delta::Text(t) => AgentEvent::Text(t.to_string()),
                        Delta::Reasoning(r) => AgentEvent::Reasoning(r.to_string()),
                        Delta::Retry(why) => AgentEvent::Retrying(why.to_string()),
                    });
                },
            )
            .await?;
        let _ = tx.send(AgentEvent::Usage { model: model.into(), prompt: resp.usage.prompt_tokens, completion: resp.usage.completion_tokens, cached: resp.usage.cached_tokens });
        let _ = tx.send(AgentEvent::Reply { content: resp.content.clone(), thought_secs: None });
        self.messages.push(Message::assistant(resp.content, vec![]));
        self.save();
        Ok(())
    }

    /// Replace the conversation so far with a summary, to free up context.
    pub async fn compact(&mut self, tx: &Events) -> Result<()> {
        self.repair();
        let history = &self.messages[1..];
        if history.len() < 2 {
            let _ = tx.send(AgentEvent::Notice("Nothing to compact yet.".into()));
            return Ok(());
        }
        let mut transcript = String::new();
        for m in history {
            let body = match m.role.as_str() {
                "tool" => tools::truncate(&m.content, 1500),
                _ => m.content.clone(),
            };
            transcript.push_str(&format!("[{}] {body}\n", m.role));
            for call in &m.tool_calls {
                transcript.push_str(&format!("[tool call] {} {}\n", call.function.name, tools::one_line(&call.function.arguments, 300)));
            }
        }
        let transcript = crate::fix::tail(&transcript, 200_000);
        let model = self.draft_model.clone().unwrap_or_else(|| self.model.lock().unwrap().clone());
        let resp = self
            .client
            .stream(
                &Request {
                    model: model.clone(),
                    messages: vec![
                        Message::system(
                            "Summarize this coding session so it can be continued without the full history. Include: the \
                             user's goals and requests, decisions made, files read or changed and their current state, \
                             commands run and their results, and anything still unfinished. Be specific with paths, \
                             names and commands. Use concise Markdown.",
                        ),
                        Message::user(transcript),
                    ],
                    max_tokens: Some(4096),
                    ..Default::default()
                }
                .think(Think::Low),
                |_| {},
            )
            .await?;
        let _ = tx.send(AgentEvent::Usage { model, prompt: resp.usage.prompt_tokens, completion: resp.usage.completion_tokens, cached: resp.usage.cached_tokens });
        let count = history.len();
        self.messages.truncate(1);
        self.messages.push(Message::user(format!(
            "(Earlier conversation was compacted to save context. Summary:)\n\n{}",
            resp.content
        )));
        self.messages.push(Message::assistant(COMPACT_ACK, vec![]));
        self.last_prompt_tokens = 0;
        self.save();
        let window = context_window(&self.model.lock().unwrap());
        let _ = tx.send(AgentEvent::Context { used: 0, window });
        let _ = tx.send(AgentEvent::Notice(format!("Compacted {count} messages into a summary.")));
        Ok(())
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
    // Saves the model several round trips of guessing how this project runs its tests.
    let tests = std::env::var("EMBER_TEST_CMD")
        .ok()
        .filter(|c| !c.trim().is_empty())
        .or_else(|| crate::fix::detect_test_command(root))
        .map(|cmd| format!("\nTest command: `{cmd}` (add a test file, module or path to run only part of the suite)"))
        .unwrap_or_default();
    let map = crate::repomap::build(root)
        .map(|m| format!("\n\n# Project map (files and top-level definitions; read files for details)\n{m}"))
        .unwrap_or_default();
    let instructions = ["AGENTS.md", "EMBER.md"]
        .iter()
        .find_map(|name| std::fs::read_to_string(root.join(name)).ok().map(|t| (name, t)))
        .map(|(name, text)| format!("\n\n# Project instructions ({name})\n{}", tools::truncate(&text, 12_000)))
        .unwrap_or_default();
    let web = if crate::web::enabled() {
        "\n- Use web_fetch (and web_search when available) for documentation or errors you don't know; cite URLs you used."
    } else {
        ""
    };

    format!(
        "You are ember, a coding agent working in the user's terminal, powered by NVIDIA Nemotron.\n\
         You help with software engineering: understanding code, fixing bugs, adding features, refactoring, \
         and running builds and tests.\n\n\
         # How to work\n\
         - Explore before you change anything: use list_files, grep and read_file to find the relevant code. \
           Never guess file contents or paths.\n\
         - You make one tool call per reply, so batch: read every file you need in a single read_file call with \
           `paths`, combine related searches into one grep pattern with `|`, and make every planned change in one \
           edit_file call with `edits`.\n\
         - For tasks with three or more steps, keep a task list with update_todos and update it as you go.\n\
         - Make focused changes with edit_file, matching the existing style. Use write_file only for new files.\n\
         - For a bug report, reproduce the problem before changing code: write a minimal example that shows the \
           wrong behavior, even when the report describes another project's files or setup. Don't conclude there \
           is nothing to fix until a reproduction shows the behavior is already correct.\n\
         - After changing code, build or run the relevant tests with bash and fix what you broke.\n\
         - Mutating actions ask the user for approval. If they decline, stop and ask how to proceed.\n\
         - Do not commit, push, or delete files unless asked.\n\
         - Files the user mentions with @path are attached to their message; images arrive as descriptions written \
           by a vision model, and you can look at other images with view_image.{web}\n\n\
         # How to communicate\n\
         - Be concise and direct. Use Markdown; put code, commands and paths in backticks.\n\
         - When you finish a task, briefly say what you changed and how you verified it, including anything \
           that failed or that you could not check.\n\n\
         # Environment\n\
         Working directory: {}\nOS: {}\nDate: {date}{tests}\n{git}{map}{instructions}",
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

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(interactive: bool) -> Agent {
        let model = Arc::new(Mutex::new("nvidia/nemotron-3-super-120b-a12b".to_string()));
        let agent = Agent::new(Client::new("http://localhost", None), model, SharedMode::new(Mode::Ask), Workspace::new(std::env::temp_dir()));
        if interactive { agent.interactive() } else { agent }
    }

    #[test]
    fn follow_ups_answer_ask_user_instead_of_adding_user_messages() {
        let mut a = agent(true);
        a.push_input("first".into());
        assert_eq!(a.messages.last().unwrap().role, "user");

        // A plain reply gets an ask_user call, and the follow-up becomes its result.
        a.messages.push(Message::assistant("done", vec![]));
        a.push_input("second".into());
        let [.., reply, answer] = &a.messages[..] else { panic!() };
        assert_eq!(reply.tool_calls[0].function.name, ASK_USER);
        assert_eq!((answer.role.as_str(), answer.content.as_str()), ("tool", "second"));
        assert_eq!(answer.tool_call_id.as_ref(), Some(&reply.tool_calls[0].id));
        assert_eq!(a.messages.iter().filter(|m| m.role == "user").count(), 1);

        // A model-made ask_user call alongside other tools is answered after their results.
        let call = |id: &str, name: &str| ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: name.into(), arguments: "{}".into() } };
        a.messages.push(Message::assistant("", vec![call("c1", "read_file"), call("c2", ASK_USER)]));
        a.messages.push(Message::tool("c1", "file text"));
        a.answer_missing(false);
        a.push_input("third".into());
        assert_eq!(a.messages.last().unwrap().tool_call_id.as_deref(), Some("c2"));
    }

    #[test]
    fn check_call_explains_unknown_tools_and_broken_arguments() {
        let specs = vec![view_image_spec(), todo_spec()];
        let call = |name: &str, args: &str| ToolCall { id: "c".into(), kind: "function".into(), function: FunctionCall { name: name.into(), arguments: args.into() } };
        assert_eq!(check_call(&call("view_image", r#"{"path": "a.png"}"#), &specs, false, false), None);
        let unknown = check_call(&call("read_files", "{}"), &specs, false, false).unwrap();
        assert!(unknown.contains("no tool named `read_files`") && unknown.contains("view_image, update_todos"));
        assert!(check_call(&call("read_files", "{}"), &specs, true, false).is_none(), "plan mode explains its own refusals");
        let cut = check_call(&call("view_image", r#"{"path": "a.p"#), &specs, false, true).unwrap();
        assert!(cut.contains("valid JSON object") && cut.contains("token limit"));
        assert!(check_call(&call("view_image", "[1]"), &specs, false, false).unwrap().contains("valid JSON object"));
    }

    #[test]
    fn print_mode_keeps_user_messages_and_repair_closes_ask_user() {
        let mut a = agent(false);
        a.push_input("first".into());
        a.messages.push(Message::assistant("done", vec![]));
        a.push_input("second".into());
        assert_eq!(a.messages.last().unwrap().role, "user");

        let mut b = agent(true);
        b.push_input("first".into());
        let ask = ToolCall { id: "q".into(), kind: "function".into(), function: FunctionCall { name: ASK_USER.into(), arguments: "{}".into() } };
        b.messages.push(Message::assistant("", vec![ask]));
        b.repair();
        assert_eq!(b.messages.last().unwrap().tool_call_id.as_deref(), Some("q"));
    }

    #[test]
    fn command_prefixes() {
        assert_eq!(command_prefix("cargo test --quiet").as_deref(), Some("cargo test"));
        assert_eq!(command_prefix("ls -la").as_deref(), Some("ls"));
        assert_eq!(command_prefix("uvx pytest -q").as_deref(), Some("uvx pytest"));
        assert_eq!(command_prefix("cargo test && rm -rf /"), None);
        assert_eq!(command_prefix("cat x > y"), None);
    }
}
