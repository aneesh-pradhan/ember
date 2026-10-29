//! The full-screen terminal UI: a scrollable conversation, a bordered prompt pinned to the bottom,
//! suggestions and permission prompts beneath it, and a status line. Every frame is drawn from state.

use std::collections::BTreeMap;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentEvent, ApprovalRequest, Decision, Events, Mode, SharedMode, Todo};
use crate::config::{Config, Tier};
use crate::fix::{self, FixEvent, Status};
use crate::input::Input;
use crate::llm::Client;
use crate::session;
use crate::tools::{self, Workspace};
use crate::{logo, markdown, theme};

const MAX_ATTACHMENT: u64 = 200_000;

/// Slash commands: name, description, whether it takes an argument.
const COMMANDS: &[(&str, &str, bool)] = &[
    ("fix", "prove a bug with a failing test, fix it in parallel worktrees, verify, apply", true),
    ("init", "explore this repo and write an AGENTS.md for future sessions", false),
    ("plan", "toggle plan mode: read-only investigation that ends in a plan", false),
    ("auto", "toggle auto-accept for edits and commands", false),
    ("model", "show or switch the Nemotron model: nano, super, ultra, or an id", true),
    ("models", "list the Nemotron models available to your API key", false),
    ("resume", "list saved conversations here; /resume <n> continues one", true),
    ("undo", "restore the files ember changed in its last turn", false),
    ("review", "have Nemotron Ultra review your uncommitted changes", false),
    ("commit", "draft a commit message from your changes and commit on approval", true),
    ("branch", "create and switch to a well-named branch for the described work", true),
    ("pr", "draft a pull request; push and open it with gh on approval", false),
    ("resolve", "work through merge conflicts in the repository", false),
    ("compact", "summarize the conversation so far to free up context", false),
    ("clear", "start a new conversation", false),
    ("diff", "show uncommitted changes (git diff)", false),
    ("todos", "show the current task list", false),
    ("think", "set Nemotron's reasoning for the agent: on, low or off", true),
    ("theme", "switch ember's look: kawaii (pastel, sparkles, kaomoji) or classic", true),
    ("mcp", "show connected MCP servers and their tools", false),
    ("usage", "token usage per model this session", false),
    ("help", "show commands and keys", false),
    ("quit", "exit ember", false),
];

const INIT_PROMPT: &str = "Explore this repository and write an AGENTS.md at its root to guide future coding sessions. \
Cover: what the project is; how the code is organized (key directories and files); exact commands to build, run, test \
and lint; code conventions you observe; and anything surprising or easy to get wrong. Keep it under about 80 lines and \
specific to this repo. If AGENTS.md already exists, improve it rather than discarding useful content.";

pub struct App {
    config: Config,
    client: Client,
    cwd: PathBuf,
    agent: Arc<tokio::sync::Mutex<Agent>>,
    model: Arc<Mutex<String>>,
    mode: SharedMode,
    tx: Events,
    task: Option<JoinHandle<()>>,
    task_started: Instant,
    input: Input,
    live_text: String,
    live_reasoning: String,
    live_tool: Option<String>,
    /// The mascot's reaction to the last tool result (happy after passing tests, sad after a failure), and when.
    reaction: Option<(theme::Mood, Instant)>,
    approval: Option<ApprovalRequest>,
    fix: Option<FixView>,
    todos: Vec<Todo>,
    context: Option<(u64, u64)>,
    git: Option<(String, usize)>,
    files: Option<Vec<String>>,
    popup_sel: usize,
    popup_token: String,
    /// Suggestions closed with Esc stay closed until the token being completed changes.
    popup_dismissed: bool,
    /// Per model: prompt tokens, completion tokens, cached prompt tokens.
    usage: BTreeMap<String, (u64, u64, u64)>,
    /// Everything printed so far, unwrapped.
    transcript: Vec<Line<'static>>,
    /// `transcript` wrapped to `wrap_width`, extended incrementally.
    wrapped: Vec<Line<'static>>,
    wrapped_from: usize,
    wrap_width: u16,
    /// Lines scrolled up from the bottom; 0 follows new output.
    scroll: usize,
    body_height: usize,
    approval_sel: usize,
    frame: usize,
    quit: bool,
    last_ctrl_c: Option<Instant>,
}

#[derive(Default)]
struct FixView {
    stages: Vec<(&'static str, Status)>,
    branches: BTreeMap<String, (String, String, String)>,
    activity: String,
}

#[derive(PartialEq, Eq)]
enum PopupKind {
    Command,
    File,
}

struct Popup {
    kind: PopupKind,
    /// Byte offset in the input where the completed token starts.
    start: usize,
    items: Vec<(String, String)>,
}

pub async fn run(
    config: Config,
    cwd: PathBuf,
    mode: Mode,
    initial: Option<String>,
    resume_last: bool,
    think: crate::llm::Think,
) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let client = config.client();
    let model = Arc::new(Mutex::new(config.coder.clone()));
    let mode = SharedMode::new(mode);
    let agent = Agent::new(client.clone(), model.clone(), mode.clone(), Workspace::new(cwd.clone())).interactive();
    *agent.think.lock().unwrap() = think;
    let (mcp, mcp_cwd) = (agent.mcp.clone(), cwd.clone());
    tokio::spawn(async move { mcp.start(&mcp_cwd).await });
    let mut app = App {
        config,
        client,
        cwd,
        agent: Arc::new(tokio::sync::Mutex::new(agent)),
        model,
        mode,
        tx,
        task: None,
        task_started: Instant::now(),
        input: Input::default(),
        live_text: String::new(),
        live_reasoning: String::new(),
        live_tool: None,
        reaction: None,
        approval: None,
        fix: None,
        todos: vec![],
        context: None,
        git: None,
        files: None,
        popup_sel: 0,
        popup_token: String::new(),
        popup_dismissed: false,
        usage: BTreeMap::new(),
        transcript: vec![],
        wrapped: vec![],
        wrapped_from: 0,
        wrap_width: 0,
        scroll: 0,
        body_height: 0,
        approval_sel: 0,
        frame: 0,
        quit: false,
        last_ctrl_c: None,
    };

    let mut terminal = ratatui::init();
    // Mouse capture gives wheel scrolling; EMBER_MOUSE=off keeps the terminal's own click-and-drag selection.
    let mouse = !matches!(std::env::var("EMBER_MOUSE").as_deref(), Ok("off" | "0" | "false"));
    let _ = crossterm::execute!(stdout(), EnableBracketedPaste);
    if mouse {
        let _ = crossterm::execute!(stdout(), EnableMouseCapture);
    }
    app.print_header();
    if resume_last {
        app.resume("1");
    }
    if let Some(prompt) = initial {
        app.submit(prompt);
    }
    let result = app.main_loop(&mut terminal, rx).await;
    let _ = crossterm::execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    if !app.usage.is_empty() {
        println!("ember: conversation saved. Continue it with `ember -c`.");
    }
    result
}

impl App {
    async fn main_loop(&mut self, terminal: &mut DefaultTerminal, mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            terminal.draw(|f| self.draw(f))?;
            if self.quit {
                return Ok(());
            }
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(ev)) => self.on_terminal(ev),
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                },
                Some(msg) = rx.recv() => self.on_agent(msg),
                _ = tick.tick() => {
                    self.frame = self.frame.wrapping_add(1);
                    if self.frame % 30 == 1 {
                        self.git = crate::git::status(&self.cwd);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------- output

    fn print(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.transcript.extend(lines);
    }

    fn print_line(&mut self, line: Line<'static>) {
        self.transcript.push(line);
    }

    fn short_cwd(&self) -> String {
        let cwd = self.cwd.display().to_string();
        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        let cwd = if !home.is_empty() && cwd.starts_with(&home) { cwd.replacen(&home, "~", 1) } else { cwd };
        // Keep the banner on one line per row: elide the middle of long paths.
        let chars: Vec<char> = cwd.chars().collect();
        if chars.len() <= 48 {
            return cwd;
        }
        format!("…{}", chars[chars.len() - 47..].iter().collect::<String>())
    }

    fn print_header(&mut self) {
        let model = self.config.coder.clone();
        let cwd = self.short_cwd();
        self.print(logo::banner(&model, &cwd));
        if self.config.api_key.is_none() && self.config.base_url.contains("nebius.com") {
            self.print_line(Line::styled(
                "  ! NEBIUS_API_KEY is not set. Add it to .env or ~/.config/ember/env, then restart.",
                theme::err(),
            ));
        }
    }

    fn print_user(&mut self, text: &str) {
        self.print_line(Line::default());
        for (i, line) in text.lines().enumerate() {
            let lead = if i == 0 { Span::styled(theme::glyphs().prompt, theme::accent()) } else { Span::raw("  ") };
            self.print_line(Line::from(vec![lead, Span::styled(line.to_string(), theme::bold())]));
        }
    }

    fn print_notice(&mut self, text: impl Into<String>) {
        self.print_line(notice(text));
    }

    fn print_diff(&mut self, diff: &str, max: usize) {
        self.transcript.extend(diff_lines(diff, max));
    }

    fn print_reply(&mut self, content: &str) {
        self.print_line(Line::default());
        for (i, line) in markdown::render(content).into_iter().enumerate() {
            let lead = if i == 0 { Span::styled(theme::glyphs().reply, theme::accent()) } else { Span::raw("  ") };
            let mut spans = vec![lead];
            spans.extend(line.spans);
            self.print_line(Line::from(spans));
        }
    }

    fn print_todos(&mut self) {
        if self.todos.is_empty() {
            self.print_notice("No task list yet. ember makes one for multi-step work.");
            return;
        }
        let lines = todo_lines(&self.todos);
        let lead = format!("  {}", theme::glyphs().reply);
        self.print_line(Line::from(vec![Span::styled(lead, theme::accent()), Span::styled("Task list", theme::bold())]));
        self.print(lines);
    }

    // ------------------------------------------------------------- agent events

    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(t) => self.live_text.push_str(&t),
            AgentEvent::Reasoning(r) => self.live_reasoning.push_str(&r),
            AgentEvent::Reply { content, thought_secs } => {
                if let Some(secs) = thought_secs.filter(|s| *s >= 2.0) {
                    self.print_line(Line::styled(format!("  {} reasoned for {secs:.1}s", theme::glyphs().reasoning), theme::dim()));
                }
                if !content.trim().is_empty() {
                    self.print_reply(&content);
                }
                self.live_text.clear();
                self.live_reasoning.clear();
            }
            AgentEvent::ToolStart { summary } => self.live_tool = Some(summary),
            AgentEvent::ToolDone { name, summary, output, ok } => {
                self.live_tool = None;
                if !ok {
                    self.reaction = Some((theme::Mood::Sad, Instant::now()));
                } else if name == "bash" && theme::is_test_run(&summary) {
                    self.reaction = Some((theme::Mood::Happy, Instant::now()));
                }
                let (mark, dot) = if ok { (theme::glyphs().tool_ok, theme::ok()) } else { (theme::glyphs().tool_err, theme::err()) };
                self.print_line(Line::from(vec![Span::styled(format!("  {mark} "), dot), Span::styled(summary, theme::bold())]));
                if name == "bash" {
                    let lines: Vec<&str> = output.lines().collect();
                    let start = lines.len().saturating_sub(10);
                    if start > 0 {
                        self.print_notice(format!("  … {start} earlier lines"));
                    }
                    let style = if ok { theme::dim() } else { theme::err() };
                    for line in &lines[start..] {
                        self.print_line(Line::styled(format!("    │ {line}"), style));
                    }
                } else if !ok {
                    self.print_line(Line::styled(format!("    {}", tools::one_line(&output, 200)), theme::err()));
                } else if name == "view_image" {
                    for line in output.lines().filter(|l| !l.trim().is_empty()).take(4) {
                        self.print_line(Line::styled(format!("    │ {}", tools::one_line(line, 140)), theme::dim()));
                    }
                } else if name == "edit_file" || (name == "write_file" && output.starts_with("---")) {
                    self.print_diff(&output, 30);
                } else {
                    let n = output.lines().count();
                    self.print_notice(format!("  └ {n} line{}", if n == 1 { "" } else { "s" }));
                }
            }
            AgentEvent::Approval(req) => {
                if let Some(preview) = &req.preview {
                    if preview.starts_with("---") {
                        self.print_line(Line::styled(format!("  ◇ {}", req.summary), theme::bold()));
                        self.print_diff(preview, 60);
                    } else if preview.starts_with('{') {
                        self.print_line(Line::styled(format!("  ◇ {}", req.summary), theme::bold()));
                        for line in preview.lines().take(20) {
                            self.print_line(Line::styled(format!("    │ {line}"), theme::dim()));
                        }
                    } else {
                        self.print_line(Line::styled(format!("  ◇ $ {preview}"), theme::bold()));
                    }
                }
                self.approval = Some(req);
                self.approval_sel = 0;
                self.live_tool = None;
            }
            AgentEvent::Usage { model, prompt, completion, cached } => {
                let entry = self.usage.entry(model).or_default();
                entry.0 += prompt;
                entry.1 += completion;
                entry.2 += cached;
            }
            AgentEvent::Context { used, window } => self.context = Some((used, window)),
            AgentEvent::Todos(todos) => {
                self.todos = todos;
                self.print_todos();
            }
            AgentEvent::Notice(text) => self.print_line(Line::styled(format!("  {text}"), theme::info())),
            AgentEvent::Fix(ev) => self.on_fix(ev),
            AgentEvent::Error(err) => {
                self.reaction = Some((theme::Mood::Sad, Instant::now()));
                let face = theme::face(theme::Mood::Sad).map(|f| format!("  {f}")).unwrap_or_default();
                self.print_line(Line::styled(format!("  {} {err}{face}", theme::glyphs().failed), theme::err()));
            }
            AgentEvent::Done => {
                self.git = crate::git::status(&self.cwd);
                if !self.live_text.trim().is_empty() {
                    let text = std::mem::take(&mut self.live_text);
                    self.print(markdown::render(&text));
                }
                self.task = None;
                self.fix = None;
                self.live_tool = None;
                self.live_reasoning.clear();
            }
        }
    }

    fn on_fix(&mut self, ev: FixEvent) {
        let view = self.fix.get_or_insert_with(FixView::default);
        let out = &mut self.transcript;
        match ev {
            FixEvent::Stage { name, status, detail } => {
                match view.stages.iter_mut().find(|(n, _)| *n == name) {
                    Some(stage) => stage.1 = status,
                    None => view.stages.push((name, status)),
                }
                view.activity.clear();
                let (mark, style) = match status {
                    Status::Running => return,
                    Status::Done => (theme::glyphs().done, theme::ok()),
                    Status::Failed => (theme::glyphs().failed, theme::err()),
                };
                out.push(Line::from(vec![
                    Span::styled(format!("  {mark} "), style),
                    Span::styled(format!("{name:<12}"), theme::bold()),
                    Span::styled(tools::one_line(&detail, 160), theme::dim()),
                ]));
            }
            FixEvent::Activity(text) => view.activity = text,
            FixEvent::Note(text) => out.push(notice(text)),
            FixEvent::Repro { attempt, path, code, output, confirmed, reason } => {
                if !confirmed {
                    out.push(notice(format!("  attempt {attempt} rejected: {reason}")));
                    return;
                }
                out.push(Line::from(vec![
                    Span::styled(format!("    {} FAILS ", theme::glyphs().failed), theme::err().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{path} on the current code: {reason}"), theme::dim()),
                ]));
                for line in code.lines().take(30) {
                    out.push(Line::styled(format!("    │ {line}"), theme::code()));
                }
                let tail: Vec<&str> = output.lines().collect();
                for line in &tail[tail.len().saturating_sub(8)..] {
                    out.push(Line::styled(format!("    │ {line}"), theme::err()));
                }
            }
            FixEvent::Strategies(list) => {
                for s in list {
                    view.branches.insert(s.id.clone(), (s.title.clone(), "pending".into(), String::new()));
                    out.push(Line::from(vec![
                        Span::styled(format!("    {} ", s.id), theme::accent()),
                        Span::styled(s.title, theme::bold()),
                        Span::styled(format!(" — {}", tools::one_line(&s.approach, 140)), theme::dim()),
                    ]));
                }
            }
            FixEvent::Branch { id, status, note } => {
                let entry = view.branches.entry(id.clone()).or_insert_with(|| (String::new(), String::new(), String::new()));
                entry.1 = status.clone();
                entry.2 = note.clone();
                let terminal = matches!(status.as_str(), "passed" | "gave_up" | "stopped" | "error");
                if terminal {
                    let style = if status == "passed" { theme::ok() } else { theme::dim() };
                    out.push(Line::from(vec![
                        Span::styled(format!("    {id} "), theme::accent()),
                        Span::styled(format!("{:<8}", status.replace('_', " ")), style),
                        Span::styled(note, theme::dim()),
                    ]));
                }
            }
            FixEvent::Result { winner, title, rationale, diff } => {
                out.extend([
                    Line::default(),
                    Line::from(vec![
                        Span::styled("  ◆ ", theme::accent()),
                        Span::styled(format!("Fix ready ({winner}): "), theme::bold()),
                        Span::styled(title, theme::bold()),
                    ]),
                    Line::styled(format!("    {rationale}"), theme::dim()),
                ]);
                out.extend(diff_lines(&diff, 200));
            }
        }
    }

    // ------------------------------------------------------------- input

    fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.on_key(key);
                self.sync_popup();
            }
            Event::Paste(text) if self.approval.is_none() => self.input.insert(&text),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll += 3,
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
                _ => {}
            },
            _ => {}
        }
    }

    /// Reset the popup selection whenever the token being completed changes.
    fn sync_popup(&mut self) {
        let (_, word) = self.input.current_word();
        let token = if self.input.text().starts_with('/') { self.input.text().to_string() } else { word.to_string() };
        if token.starts_with('@') && self.files.is_none() {
            self.files = Some(tools::project_files(&self.cwd, 20_000));
        }
        if token != self.popup_token {
            self.popup_token = token;
            self.popup_sel = 0;
            self.popup_dismissed = false;
        }
    }

    fn on_approval_key(&mut self, key: KeyEvent, req: ApprovalRequest) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let options = approval_options(&req);
        let decision = match key.code {
            KeyCode::Up => {
                self.approval_sel = (self.approval_sel + options.len() - 1) % options.len();
                None
            }
            KeyCode::Down => {
                self.approval_sel = (self.approval_sel + 1) % options.len();
                None
            }
            KeyCode::Enter => Some(options[self.approval_sel.min(options.len() - 1)].0),
            KeyCode::Char(c @ '1'..='3') => options.get(c as usize - '1' as usize).map(|o| o.0),
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Yes),
            KeyCode::Char('a') | KeyCode::Char('A') if req.always.is_some() => Some(Decision::Always),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::No),
            KeyCode::Char('c') if ctrl => Some(Decision::No),
            _ => None,
        };
        match decision {
            Some(d) => {
                let label = match d {
                    Decision::Yes => "approved".to_string(),
                    Decision::Always => format!("approved; will allow {} for the rest of this session", req.always.as_deref().unwrap_or("this")),
                    Decision::No => "declined".to_string(),
                };
                self.print_notice(format!("  {label}"));
                let _ = req.reply.send(d);
            }
            None => self.approval = Some(req),
        }
    }

    /// Popup navigation. Returns true when the key was handled.
    fn on_popup_key(&mut self, key: KeyEvent) -> bool {
        let Some(popup) = self.popup() else { return false };
        let n = popup.items.len();
        match key.code {
            KeyCode::Up => self.popup_sel = (self.popup_sel + n - 1) % n,
            KeyCode::Down => self.popup_sel = (self.popup_sel + 1) % n,
            KeyCode::Tab => self.accept(&popup, false),
            KeyCode::Esc => self.popup_dismissed = true,
            KeyCode::Enter if key.modifiers.is_empty() => {
                let (label, _) = &popup.items[self.popup_sel.min(n - 1)];
                if popup.kind == PopupKind::Command && label == self.input.text() {
                    return false; // already complete: let Enter run it
                }
                self.accept(&popup, true);
            }
            _ => return false,
        }
        true
    }

    fn accept(&mut self, popup: &Popup, enter: bool) {
        let (label, _) = &popup.items[self.popup_sel.min(popup.items.len() - 1)];
        match popup.kind {
            PopupKind::Command => {
                let is_custom = !COMMANDS.iter().any(|(name, _, _)| label[1..] == **name);
                let takes_arg = is_custom || COMMANDS.iter().any(|(name, _, arg)| *arg && label[1..] == **name);
                if takes_arg {
                    self.input.set(&format!("{label} "));
                } else if enter {
                    self.input.clear();
                    self.submit(label.clone());
                } else {
                    self.input.set(label);
                }
            }
            PopupKind::File => self.input.replace_before_cursor(popup.start, &format!("@{label} ")),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if let Some(req) = self.approval.take() {
            self.on_approval_key(key, req);
            return;
        }
        if self.on_popup_key(key) {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.task.is_some() {
                    self.interrupt();
                } else if !self.input.is_empty() {
                    self.input.clear();
                } else if self.last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_millis(1500)) {
                    self.quit = true;
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.print_notice("Press Ctrl+C again to exit");
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
            KeyCode::PageUp => self.scroll += self.body_height.saturating_sub(2).max(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.body_height.saturating_sub(2).max(1)),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll += 1,
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::End if ctrl => self.scroll = 0,
            KeyCode::Esc if self.task.is_some() => self.interrupt(),
            KeyCode::Esc => self.input.clear(),
            KeyCode::BackTab => self.set_mode(self.mode.get().next()),
            KeyCode::Enter if alt => self.input.insert("\n"),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Enter => {
                let text = self.input.take();
                if !text.trim().is_empty() {
                    self.submit(text);
                }
            }
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.kill_line(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('t') if ctrl => self.print_todos(),
            KeyCode::Backspace if alt => self.input.delete_word(),
            KeyCode::Char(c) if !ctrl => self.input.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up if !self.input.is_multiline() => self.input.history_prev(),
            KeyCode::Down if !self.input.is_multiline() => self.input.history_next(),
            _ => {}
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode.set(mode);
        self.print_notice(match mode {
            Mode::Ask => "Ask mode: ember asks before editing files or running commands",
            Mode::Auto => "Auto-accept mode: edits and commands run without asking (shift+tab to change)",
            Mode::Plan => "Plan mode: ember only reads and investigates, then proposes a plan (shift+tab to change)",
        });
    }

    fn interrupt(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(req) = self.approval.take() {
            let _ = req.reply.send(Decision::No);
        }
        if !self.live_text.trim().is_empty() {
            let text = std::mem::take(&mut self.live_text);
            self.print(markdown::render(&text));
        }
        self.live_text.clear();
        self.live_reasoning.clear();
        self.live_tool = None;
        self.fix = None;
        self.print_line(Line::styled("  ⎯ interrupted", theme::err()));
    }

    fn busy(&mut self) -> bool {
        if self.task.is_some() {
            self.print_notice("ember is still working; press Esc to interrupt first");
        }
        self.task.is_some()
    }

    fn submit(&mut self, text: String) {
        self.scroll = 0;
        let text = text.trim().to_string();
        if let Some(command) = text.strip_prefix('/') {
            self.command(command.to_string());
            return;
        }
        if let Some(command) = text.strip_prefix('!') {
            self.shell(command.trim().to_string());
            return;
        }
        if self.busy() {
            return;
        }
        self.print_user(&text);
        let (prompt, attached, images) = self.attach_mentions(&text);
        for name in attached {
            self.print_notice(format!("  + attached {name}"));
        }
        self.start_turn_with(prompt, images);
    }

    fn start_turn(&mut self, prompt: String) {
        self.start_turn_with(prompt, vec![]);
    }

    fn start_turn_with(&mut self, prompt: String, images: Vec<PathBuf>) {
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            let mut agent = agent.lock().await;
            let described = agent.describe_images(&images, &prompt, &tx).await;
            if let Err(e) = agent.turn(prompt + &described, &tx).await {
                let _ = tx.send(AgentEvent::Error(format!("{e:#}")));
            }
        });
    }

    /// Attach files mentioned as @path (and image paths dropped into the terminal). Text files are inlined;
    /// images are returned separately for the vision model to describe.
    fn attach_mentions(&self, text: &str) -> (String, Vec<String>, Vec<PathBuf>) {
        let ws = Workspace::new(self.cwd.clone());
        let mut prompt = text.to_string();
        let mut attached: Vec<String> = vec![];
        let mut images: Vec<PathBuf> = vec![];
        for word in text.split_whitespace() {
            let mention = word.strip_prefix('@');
            let raw = mention.unwrap_or(word).trim_matches(['\'', '"']).trim_end_matches(['.', ',', ')', ':', ';', '?', '!']);
            let raw = raw.replace("\\ ", " ");
            // Text attachments must be inside the project; images may come from anywhere (e.g. a dragged screenshot).
            let inside = ws.resolve(&raw).ok();
            let Some(abs) = inside.clone().or_else(|| Some(PathBuf::from(&raw)).filter(|p| p.is_absolute())) else { continue };
            if !abs.is_file() || attached.iter().any(|a| a.starts_with(raw.as_str())) {
                continue;
            }
            if crate::vision::is_image(&abs) {
                images.push(abs);
                attached.push(format!("{raw} (image, read by Nemotron VL)"));
                continue;
            }
            if mention.is_none() || inside.is_none() || abs.metadata().map_or(true, |m| m.len() > MAX_ATTACHMENT) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&abs) else { continue };
            prompt.push_str(&format!("\n\n<file path=\"{raw}\">\n{content}\n</file>"));
            attached.push(format!("{raw} ({} lines)", content.lines().count()));
        }
        (prompt, attached, images)
    }

    /// `!command`: run it directly in the user's shell and add the output to the conversation.
    fn shell(&mut self, command: String) {
        if command.is_empty() || self.busy() {
            return;
        }
        self.print_line(Line::default());
        self.print_line(Line::from(vec![Span::styled("! ", theme::code()), Span::styled(command.clone(), theme::bold())]));
        let (agent, tx, cwd) = (self.agent.clone(), self.tx.clone(), self.cwd.clone());
        self.spawn(async move {
            let _ = tx.send(AgentEvent::ToolStart { summary: format!("$ {command}") });
            let out = tools::run_command(&cwd, &command, Duration::from_secs(600)).await;
            let rendered = out.render();
            let _ = tx.send(AgentEvent::ToolDone { name: "bash".into(), summary: format!("$ {command}"), output: rendered.clone(), ok: out.success() });
            agent.lock().await.note_shell(&command, &rendered);
        });
    }

    fn spawn(&mut self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let tx = self.tx.clone();
        self.task_started = Instant::now();
        self.task = Some(tokio::spawn(async move {
            fut.await;
            let _ = tx.send(AgentEvent::Done);
        }));
    }

    fn command(&mut self, line: String) {
        let (name, arg) = line.split_once(char::is_whitespace).map_or((line.as_str(), ""), |(n, a)| (n, a.trim()));
        match name {
            "help" | "?" => self.print_help(),
            "quit" | "exit" | "q" => self.quit = true,
            "auto" => self.set_mode(if self.mode.get() == Mode::Auto { Mode::Ask } else { Mode::Auto }),
            "plan" => self.set_mode(if self.mode.get() == Mode::Plan { Mode::Ask } else { Mode::Plan }),
            "model" => self.set_model(arg),
            "think" => {
                let agent = self.agent.clone();
                let Ok(agent) = agent.try_lock() else {
                    self.print_notice("ember is busy; try /think again when it's idle");
                    return;
                };
                let current = *agent.think.lock().unwrap();
                match crate::llm::Think::parse(arg) {
                    Some(level) => {
                        *agent.think.lock().unwrap() = level;
                        drop(agent);
                        self.print_notice(match level {
                            crate::llm::Think::On => "Reasoning on: Nemotron thinks before acting (best for hard tasks)",
                            crate::llm::Think::Low => "Reasoning low: shorter thinking, faster replies",
                            crate::llm::Think::Off => "Reasoning off: fastest; best for simple, mechanical requests",
                        });
                    }
                    None => {
                        drop(agent);
                        self.print_notice(format!("Reasoning is {}. Use /think on, /think low or /think off.", current.label()));
                    }
                }
            }
            "theme" => self.set_theme(arg),
            "usage" | "cost" => self.print_usage(),
            "todos" => self.print_todos(),
            "diff" => self.print_git_diff(),
            "resume" => self.resume(arg),
            "mcp" => {
                let agent = self.agent.clone();
                let Ok(agent) = agent.try_lock() else {
                    self.print_notice("ember is busy; try /mcp again in a moment");
                    return;
                };
                let status = agent.mcp.status();
                let tools: Vec<String> = agent.mcp.specs().into_iter().map(|t| t.function.name).collect();
                drop(agent);
                if status.is_empty() {
                    self.print_notice("No MCP servers configured. Add them to .ember/mcp.json or ~/.config/ember/mcp.json:");
                    self.print_notice(r#"  {"mcpServers": {"name": {"command": "npx", "args": ["-y", "some-mcp-server"]}}}"#);
                    return;
                }
                self.print_line(Line::default());
                for line in status {
                    let style = if line.starts_with('✓') { theme::ok() } else { theme::err() };
                    self.print_line(Line::styled(format!("  {line}"), style));
                }
                for t in tools {
                    self.print_notice(format!("    {t}"));
                }
            }
            "review" => {
                if self.busy() {
                    return;
                }
                self.print_user("/review");
                let (agent, tx, model) = (self.agent.clone(), self.tx.clone(), self.config.reasoner.clone());
                self.spawn(async move {
                    let _ = tx.send(AgentEvent::ToolStart { summary: "Reviewing your changes with Nemotron Ultra".into() });
                    if let Err(e) = agent.lock().await.review(&model, &tx).await {
                        let _ = tx.send(AgentEvent::Error(format!("review failed: {e:#}")));
                    }
                });
            }
            "commit" | "branch" | "pr" => {
                if self.busy() {
                    return;
                }
                self.print_user(&format!("/{line}"));
                let (client, tx, cwd, model) = (self.client.clone(), self.tx.clone(), self.cwd.clone(), self.config.coder.clone());
                let (kind, arg) = (name.to_string(), arg.to_string());
                self.spawn(async move {
                    let result = match kind.as_str() {
                        "commit" => crate::git::commit(client, model, &cwd, arg, tx.clone()).await,
                        "branch" => crate::git::branch(client, model, &cwd, arg, tx.clone()).await,
                        _ => crate::git::pr(client, model, &cwd, tx.clone()).await,
                    };
                    if let Err(e) = result {
                        let _ = tx.send(AgentEvent::Error(format!("/{kind}: {e:#}")));
                    }
                });
            }
            "resolve" => {
                if self.busy() {
                    return;
                }
                let cwd = self.cwd.clone();
                let files = std::process::Command::new("git")
                    .args(["diff", "--name-only", "--diff-filter=U"])
                    .current_dir(&cwd)
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                if files.is_empty() {
                    self.print_notice("No merge conflicts to resolve.");
                    return;
                }
                self.print_user("/resolve");
                self.start_turn(format!(
                    "These files have merge conflicts:\n{files}\n\nFor each one, read it, understand what both sides \
                     intended (use `git log` on the conflicting branches if helpful), and edit the file to a correct \
                     merged version with no conflict markers left. Then build or run the tests, and summarize how you \
                     resolved each conflict. Do not commit; leave staging the result to me."
                ));
            }
            "undo" => {
                if self.busy() {
                    return;
                }
                let agent = self.agent.clone();
                let undone = match agent.try_lock() {
                    Ok(mut agent) => agent.undo(),
                    Err(_) => return,
                };
                match undone {
                    Some((label, files)) => {
                        self.print_line(Line::default());
                        self.print_line(Line::from(vec![
                            Span::styled("  ↶ Undid ", theme::accent()),
                            Span::styled(format!("\"{label}\""), theme::bold()),
                        ]));
                        for f in files {
                            self.print_notice(format!("  restored {f}"));
                        }
                        self.print_notice("  (shell commands from that turn were not undone)");
                    }
                    None => self.print_notice("Nothing to undo: ember hasn't changed any files in this conversation."),
                }
            }
            "clear" | "new" => {
                if self.busy() {
                    return;
                }
                let agent = self.agent.clone();
                let reset = match agent.try_lock() {
                    Ok(mut agent) => {
                        agent.reset();
                        true
                    }
                    Err(_) => false,
                };
                if reset {
                    self.todos.clear();
                    self.context = None;
                    self.print_notice("Started a new conversation");
                }
            }
            "compact" => {
                if self.busy() {
                    return;
                }
                self.print_user("/compact");
                let (agent, tx) = (self.agent.clone(), self.tx.clone());
                self.spawn(async move {
                    let _ = tx.send(AgentEvent::ToolStart { summary: "Summarizing the conversation".into() });
                    if let Err(e) = agent.lock().await.compact(&tx).await {
                        let _ = tx.send(AgentEvent::Error(format!("compact failed: {e:#}")));
                    }
                });
            }
            "init" => {
                if self.busy() {
                    return;
                }
                self.print_user("/init");
                self.start_turn(INIT_PROMPT.to_string());
            }
            "models" => {
                let client = self.client.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let msg = match client.list_models().await {
                        Ok(ids) => {
                            let nemotron: Vec<String> = ids.into_iter().filter(|m| m.to_lowercase().contains("nemotron")).collect();
                            format!("Nemotron models on your key:\n  {}", nemotron.join("\n  "))
                        }
                        Err(e) => format!("Couldn't list models: {e:#}"),
                    };
                    let _ = tx.send(AgentEvent::Notice(msg));
                });
            }
            "fix" => {
                if arg.is_empty() {
                    self.print_notice("Usage: /fix <bug description or GitHub issue URL>");
                } else if !self.busy() {
                    self.print_user(&format!("/fix {arg}"));
                    self.fix = Some(FixView::default());
                    let (client, config, cwd, tx) = (self.client.clone(), self.config.clone(), self.cwd.clone(), self.tx.clone());
                    let auto = self.mode.get() == Mode::Auto;
                    let issue = arg.to_string();
                    self.spawn(async move {
                        if let Err(e) = fix::run(client, config, cwd, issue, auto, tx.clone()).await {
                            let _ = tx.send(AgentEvent::Error(format!("/fix stopped: {e:#}")));
                        }
                    });
                }
            }
            other => match crate::commands::load(&self.cwd).into_iter().find(|c| c.name == other) {
                Some(custom) => {
                    if self.busy() {
                        return;
                    }
                    let prompt = custom.expand(arg);
                    self.print_user(&format!("/{line}"));
                    let (prompt, attached, images) = self.attach_mentions(&prompt);
                    for name in attached {
                        self.print_notice(format!("  + attached {name}"));
                    }
                    self.start_turn_with(prompt, images);
                }
                None => self.print_notice(format!("Unknown command /{other}. Try /help")),
            },
        }
    }

    /// `/resume` lists saved conversations for this directory; `/resume <n>` continues one.
    fn resume(&mut self, arg: &str) {
        if self.busy() {
            return;
        }
        let agent = self.agent.clone();
        let Ok(mut agent) = agent.try_lock() else { return };
        let current = agent.session_id().to_string();
        let sessions: Vec<_> = session::list(&self.cwd.display().to_string()).into_iter().filter(|s| s.id != current).take(10).collect();
        if sessions.is_empty() {
            self.print_notice("No saved conversations in this directory yet.");
            return;
        }
        if arg.is_empty() {
            let mut lines = vec![Line::default(), Line::styled("  Saved conversations here (/resume <n> to continue):", theme::bold())];
            for (i, s) in sessions.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:>2}. ", i + 1), theme::accent()),
                    Span::raw(tools::one_line(&s.title, 70)),
                    Span::styled(format!("  {} · {} messages", session::age(s.updated), s.messages.len()), theme::dim()),
                ]));
            }
            self.print(lines);
            return;
        }
        let Some(chosen) = arg.parse::<usize>().ok().and_then(|n| sessions.get(n.wrapping_sub(1)).cloned()) else {
            self.print_notice(format!("No conversation number {arg}; try /resume to list them"));
            return;
        };
        let title = chosen.title.clone();
        let count = agent.resume(chosen);
        let last = agent.last_reply();
        drop(agent);
        self.todos.clear();
        self.print_line(Line::default());
        self.print_line(Line::from(vec![
            Span::styled("  ↺ Resumed: ", theme::accent()),
            Span::styled(title, theme::bold()),
            Span::styled(format!("  ({count} messages)"), theme::dim()),
        ]));
        if let Some(reply) = last {
            self.print_reply(&reply);
        }
    }

    fn print_git_diff(&mut self) {
        let run = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&self.cwd).output().ok().filter(|o| o.status.success())
        };
        let Some(stat) = run(&["diff", "--stat", "HEAD"]) else {
            self.print_notice("Not a git repository (or no commits yet)");
            return;
        };
        let stat = String::from_utf8_lossy(&stat.stdout).to_string();
        if stat.trim().is_empty() {
            self.print_notice("No uncommitted changes");
            return;
        }
        let diff = run(&["diff", "HEAD"]).map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        self.print_line(Line::default());
        for line in stat.lines() {
            self.print_line(Line::styled(format!("  {line}"), theme::bold()));
        }
        self.print_diff(&diff, 300);
    }

    fn set_model(&mut self, arg: &str) {
        if arg.is_empty() {
            let current = self.model.lock().unwrap().clone();
            let mut lines = vec![Line::styled(format!("  Model: {current}"), theme::bold())];
            for tier in [Tier::Fast, Tier::Coder, Tier::Reasoner] {
                lines.push(Line::from(vec![
                    Span::styled(format!("    /model {:<6}", tier.label().to_lowercase()), theme::tier(tier)),
                    Span::styled(self.config.model(tier).to_string(), theme::dim()),
                ]));
            }
            lines.push(Line::styled("    /model <id>   any model id from /models", theme::dim()));
            self.print(lines);
            return;
        }
        let id = Tier::parse(arg).map_or(arg.to_string(), |t| self.config.model(t).to_string());
        *self.model.lock().unwrap() = id.clone();
        self.print_notice(format!("Now using {id}"));
        // Token Factory caches Ultra in 4,352-token blocks and never counts the last one, so short agent steps
        // get no cache hits (0% on ember's benchmark, about 9x Super's cost per task).
        if id.to_ascii_lowercase().contains("ultra") && self.config.base_url.contains("nebius.com") {
            self.print_notice("Ultra gets few prompt-cache hits on Token Factory, so multi-step tasks cost about 9x Super");
        }
    }

    /// `/theme` shows the current look; `/theme kawaii|classic` switches and remembers it.
    fn set_theme(&mut self, arg: &str) {
        let Some(choice) = theme::Theme::parse(arg) else {
            self.print_notice(format!("Theme is {}. Use /theme kawaii or /theme classic.", theme::current().label()));
            return;
        };
        theme::set(choice);
        let saved = theme::save(choice).map_or_else(|e| format!(" (couldn't save it: {e})"), |_| String::new());
        let flourish = theme::face(theme::Mood::Happy).map(|f| format!(" {f}")).unwrap_or_default();
        self.print_notice(format!("Theme: {}{flourish}{saved}", choice.label()));
    }

    /// The mascot's mood for the working indicator: a recent reaction wins for a few seconds, then what it's doing.
    fn mood(&self, elapsed: u64) -> theme::Mood {
        match self.reaction {
            Some((mood, at)) if at.elapsed() < Duration::from_secs(4) => mood,
            _ if self.live_tool.is_some() => theme::Mood::Busy,
            _ if !self.live_reasoning.is_empty() && self.live_text.is_empty() => theme::Mood::Thinking,
            _ if elapsed >= 60 => theme::Mood::Sweaty,
            _ => theme::Mood::Working,
        }
    }

    fn print_help(&mut self) {
        let mut lines = vec![Line::default(), Line::styled("  Commands", theme::bold())];
        for (name, desc, arg) in COMMANDS {
            let label = if *arg { format!("/{name} …") } else { format!("/{name}") };
            lines.push(Line::from(vec![Span::styled(format!("    {label:<12}"), theme::accent()), Span::styled(*desc, theme::dim())]));
        }
        let custom = crate::commands::load(&self.cwd);
        if !custom.is_empty() {
            lines.push(Line::styled("  Custom commands", theme::bold()));
            for c in custom {
                lines.push(Line::from(vec![
                    Span::styled(format!("    /{:<11}", c.name), theme::accent()),
                    Span::styled(c.description, theme::dim()),
                ]));
            }
        }
        lines.push(Line::styled("  Add your own: .ember/commands/<name>.md in a project, or ~/.config/ember/commands/.", theme::dim()));
        lines.push(Line::styled("  Input", theme::bold()));
        for (key, desc) in [
            ("@path", "attach a file to your message (Tab completes)"),
            ("!cmd", "run a shell command yourself; its output joins the conversation"),
            ("shift+tab", "cycle modes: ask → auto-accept → plan"),
            ("esc", "interrupt ember, or clear the input"),
            ("ctrl+t", "show the task list"),
            ("alt+enter", "new line (also ctrl+j)"),
            ("↑ / ↓", "history, or move through suggestions"),
        ] {
            lines.push(Line::from(vec![Span::styled(format!("    {key:<12}"), theme::accent()), Span::styled(desc, theme::dim())]));
        }
        lines.push(Line::styled("  Project instructions are read from AGENTS.md or EMBER.md (create one with /init).", theme::dim()));
        self.print(lines);
    }

    fn print_usage(&mut self) {
        if self.usage.is_empty() {
            self.print_notice("No model calls yet");
            return;
        }
        let mut rows = vec![Line::styled(format!("  {:<42} {:>9} {:>8} {:>9}", "model", "input", "cached", "output"), theme::bold())];
        for (model, (p, c, cached)) in &self.usage {
            let pct = if *p > 0 { format!("{}%", cached * 100 / p) } else { "-".into() };
            rows.push(Line::styled(format!("  {model:<42} {p:>9} {pct:>8} {c:>9}"), theme::dim()));
        }
        rows.push(Line::styled(
            "  Cached input is served from Token Factory's prompt cache at a lower price.",
            theme::dim(),
        ));
        self.print(rows);
    }

    // ------------------------------------------------------------- drawing

    fn popup(&self) -> Option<Popup> {
        if self.task.is_some() || self.approval.is_some() || self.popup_dismissed {
            return None;
        }
        let text = self.input.text();
        if let Some(query) = text.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) {
            let mut all: Vec<(String, String)> =
                COMMANDS.iter().map(|(name, desc, _)| (name.to_string(), desc.to_string())).collect();
            for custom in crate::commands::load(&self.cwd) {
                if !all.iter().any(|(n, _)| *n == custom.name) {
                    let scope = if custom.project { "project" } else { "custom" };
                    all.push((custom.name, format!("({scope}) {}", custom.description)));
                }
            }
            let pick = |m: &dyn Fn(&str) -> bool| -> Vec<(String, String)> {
                all.iter().filter(|(n, _)| m(n)).map(|(n, d)| (format!("/{n}"), d.clone())).collect()
            };
            let mut items = pick(&|n: &str| n.starts_with(query));
            if items.is_empty() {
                items = pick(&|n: &str| n.contains(query));
            }
            return (!items.is_empty()).then_some(Popup { kind: PopupKind::Command, start: 0, items });
        }
        let (start, word) = self.input.current_word();
        let query = word.strip_prefix('@')?;
        let files = self.files.as_ref()?;
        let mut scored: Vec<(i64, &String)> = files.iter().filter_map(|f| fuzzy_score(query, f).map(|s| (s, f))).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let items: Vec<(String, String)> = scored.into_iter().take(8).map(|(_, f)| (f.clone(), String::new())).collect();
        (!items.is_empty()).then_some(Popup { kind: PopupKind::File, start, items })
    }

    fn draw(&mut self, f: &mut Frame) {
        let popup = self.popup();
        let prompt_height = match &self.approval {
            Some(req) => 4 + approval_options(req).len() as u16,
            None => self.input.text().split('\n').count().clamp(1, 6) as u16 + 2,
        };
        let footer_height = popup.as_ref().map_or(1, |p| p.items.len().min(8) as u16);
        let [body, prompt, footer] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(prompt_height), Constraint::Length(footer_height)])
                .areas(f.area());

        self.render_body(f, body);
        if self.approval.is_some() {
            self.render_approval(f, prompt);
        } else {
            self.render_input(f, prompt);
        }
        match popup {
            Some(p) => Paragraph::new(self.popup_lines(&p, footer.height as usize, footer.width)).render(footer, f.buffer_mut()),
            None => self.render_status(f, footer),
        }
        theme::adapt(f.buffer_mut());
    }

    /// The conversation, wrapped to the screen, bottom-following unless scrolled up.
    fn render_body(&mut self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(1).max(10);
        if width != self.wrap_width {
            self.wrap_width = width;
            self.wrapped.clear();
            self.wrapped_from = 0;
        }
        if self.wrapped_from < self.transcript.len() {
            let new = markdown::wrap(&self.transcript[self.wrapped_from..], width);
            if self.scroll > 0 {
                self.scroll += new.len(); // keep the view still while scrolled back
            }
            self.wrapped.extend(new);
            self.wrapped_from = self.transcript.len();
        }
        let live = markdown::wrap(&self.live_lines(width), width);
        let gap = usize::from(!live.is_empty());
        let total = self.wrapped.len() + gap + live.len();
        let height = area.height as usize;
        self.body_height = height;
        self.scroll = self.scroll.min(total.saturating_sub(height));
        let end = total - self.scroll;
        let lines: Vec<Line> = (end.saturating_sub(height)..end)
            .map(|i| match i.checked_sub(self.wrapped.len()) {
                None => self.wrapped[i].clone(),
                Some(j) if j < gap => Line::default(),
                Some(j) => live[j - gap].clone(),
            })
            .collect();
        Paragraph::new(lines).render(area, f.buffer_mut());
        if self.scroll > 0 {
            let note = format!(" ↓ {} more lines · PgDn or ctrl+end ", self.scroll);
            let w = (note.chars().count() as u16).min(area.width);
            let spot = Rect { x: area.right() - w, y: area.bottom() - 1, width: w, height: 1 };
            Paragraph::new(Line::styled(note, theme::accent().add_modifier(Modifier::REVERSED))).render(spot, f.buffer_mut());
        }
    }

    fn render_input(&self, f: &mut Frame, area: Rect) {
        let border = match self.mode.get() {
            Mode::Plan => theme::info(),
            Mode::Auto => theme::accent(),
            Mode::Ask => theme::dim(),
        };
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(border).padding(Padding::horizontal(1));
        let inner = block.inner(area);
        block.render(area, f.buffer_mut());

        let (row, col) = self.input.cursor_pos();
        let first = row.saturating_sub((inner.height as usize).saturating_sub(1));
        let lines: Vec<Line> = if self.input.is_empty() {
            let hint = match self.mode.get() {
                Mode::Plan => "Describe what you want; ember will investigate and propose a plan",
                _ => "Ask ember to build, fix or explain something",
            };
            vec![Line::from(vec![Span::styled(theme::glyphs().prompt, theme::accent()), Span::styled(hint, theme::dim())])]
        } else {
            let shell = self.input.text().starts_with('!');
            self.input
                .text()
                .split('\n')
                .enumerate()
                .skip(first)
                .map(|(i, l)| {
                    let lead = match (i, shell) {
                        (0, true) => Span::styled("! ", theme::code()),
                        (0, false) => Span::styled(theme::glyphs().prompt, theme::accent()),
                        _ => Span::raw("  "),
                    };
                    Line::from(vec![lead, Span::raw(l.to_string())])
                })
                .collect()
        };
        Paragraph::new(lines).render(inner, f.buffer_mut());
        let x = (inner.x + 2 + col as u16).min(inner.right().saturating_sub(1));
        f.set_cursor_position(Position { x, y: inner.y + (row - first) as u16 });
    }

    fn render_approval(&self, f: &mut Frame, area: Rect) {
        let Some(req) = &self.approval else { return };
        let mut block = Block::bordered().border_type(BorderType::Rounded).border_style(theme::accent()).padding(Padding::horizontal(1));
        block = match theme::current() {
            // A little retro OS window: title bar text on the left, window buttons on the right.
            theme::Theme::Kawaii => block
                .title(Span::styled(" ♡ permission? ♡ ", theme::accent().add_modifier(Modifier::BOLD)))
                .title_top(Line::styled(" _ □ ✕ ", theme::dim()).right_aligned()),
            theme::Theme::Classic => block.title(Span::styled(" Permission ", theme::accent().add_modifier(Modifier::BOLD))),
        };
        let inner = block.inner(area);
        block.render(area, f.buffer_mut());
        let options = approval_options(req);
        let sel = self.approval_sel.min(options.len() - 1);
        let mut lines = vec![
            Line::styled(format!("{}?", req.summary), theme::bold()),
            Line::default(),
        ];
        for (i, (_, label)) in options.iter().enumerate() {
            let selected = i == sel;
            lines.push(Line::from(vec![
                Span::styled(if selected { theme::glyphs().select } else { "  " }, theme::accent()),
                Span::styled(
                    format!("{}. {label}", i + 1),
                    if selected { theme::accent().add_modifier(Modifier::BOLD) } else { ratatui::style::Style::default() },
                ),
            ]));
        }
        Paragraph::new(lines).render(inner, f.buffer_mut());
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        let model = self.model.lock().unwrap().clone();
        let tier = [Tier::Fast, Tier::Coder, Tier::Reasoner].into_iter().find(|t| self.config.model(*t) == model);
        let mut right = vec![];
        if let Some((used, window)) = self.context.filter(|(u, _)| *u > 0) {
            let pct = (used as f64 / window as f64 * 100.0).round() as u64;
            let style = if pct >= 80 { theme::err() } else if pct >= 50 { theme::code() } else { theme::dim() };
            right.push(Span::styled(format!("ctx {pct}%  "), style));
        }
        let tokens: u64 = self.usage.values().map(|(p, c, _)| p + c).sum();
        if tokens > 0 {
            right.push(Span::styled(format!("{:.1}k tok  ", tokens as f64 / 1000.0), theme::dim()));
        }
        if let Ok(agent) = self.agent.try_lock() {
            let think = *agent.think.lock().unwrap();
            if think != crate::llm::Think::On {
                right.push(Span::styled(format!("think {}  ", think.label()), theme::code()));
            }
        }
        right.push(Span::styled(tier.map_or("custom", |t| t.label()).to_string(), tier.map_or(theme::bold(), theme::tier)));
        right.push(Span::styled(format!(" {} ", model.rsplit('/').next().unwrap_or("")), theme::dim()));
        let right = Line::from(right);

        let mut left = match self.mode.get() {
            Mode::Auto => vec![Span::styled(" ⏵⏵ auto-accept on", theme::accent())],
            Mode::Plan => vec![Span::styled(" ⏸ plan mode on", theme::info())],
            Mode::Ask => vec![Span::styled(" ? /help", theme::dim())],
        };
        if let Some((branch, changed)) = &self.git {
            left.push(Span::styled(format!(" · ⎇ {branch}"), theme::code()));
            if *changed > 0 {
                left.push(Span::styled(format!(" ±{changed}"), theme::dim()));
            }
        }
        let hints = " · shift+tab mode · / commands · @ files · ! shell";
        let room = (area.width as usize).saturating_sub(right.width() + 2);
        if Line::from(left.clone()).width() + hints.chars().count() <= room {
            left.push(Span::styled(hints, theme::dim()));
        }
        Paragraph::new(Line::from(left)).render(area, f.buffer_mut());
        Paragraph::new(right).alignment(Alignment::Right).render(area, f.buffer_mut());
    }

    fn popup_lines(&self, popup: &Popup, height: usize, width: u16) -> Vec<Line<'static>> {
        let sel = self.popup_sel.min(popup.items.len() - 1);
        let first = sel.saturating_sub(height.saturating_sub(1));
        popup
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
            .map(|(i, (label, detail))| {
                let selected = i == sel;
                let lead = if selected { Span::styled(theme::glyphs().select, theme::accent()) } else { Span::raw("  ") };
                let style = if selected { theme::accent().add_modifier(Modifier::BOLD) } else { ratatui::style::Style::default() };
                let mut spans = vec![lead, Span::styled(format!("{label:<12}"), style)];
                if !detail.is_empty() {
                    spans.push(Span::styled(format!("  {}", tools::one_line(detail, width.saturating_sub(18) as usize)), theme::dim()));
                }
                Line::from(spans)
            })
            .collect()
    }

    fn live_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(_) = &self.task else { return vec![] };
        let spinner = theme::spinner(self.frame);
        let elapsed = self.task_started.elapsed().as_secs();
        let mut lines = vec![];

        if let Some(view) = &self.fix {
            let stages: Vec<Span<'static>> = ["Investigate", "Reproduce", "Plan", "Fix", "Review"]
                .iter()
                .flat_map(|name| {
                    let status = view.stages.iter().find(|(n, _)| n == name).map(|(_, s)| *s);
                    let (mark, style) = match status {
                        Some(Status::Done) => (format!("{} ", theme::glyphs().done), theme::ok()),
                        Some(Status::Failed) => (format!("{} ", theme::glyphs().failed), theme::err()),
                        Some(Status::Running) => (format!("{spinner} "), theme::accent()),
                        None => ("· ".to_string(), theme::dim()),
                    };
                    [Span::styled(mark, style), Span::styled(format!("{name}   "), style)]
                })
                .collect();
            lines.push(Line::from(stages));
            for (id, (title, status, note)) in &view.branches {
                let style = match status.as_str() {
                    "passed" => theme::ok(),
                    "failing" | "regressed" | "error" | "gave_up" => theme::err(),
                    _ => theme::dim(),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {id} "), theme::accent()),
                    Span::styled(format!("{:<9}", status.replace('_', " ")), style),
                    Span::styled(tools::one_line(if note.is_empty() { title } else { note }, width.saturating_sub(16) as usize), theme::dim()),
                ]));
            }
            if !view.activity.is_empty() {
                lines.push(Line::styled(format!("  {}", tools::one_line(&view.activity, width as usize - 4)), theme::dim()));
            }
            return lines;
        }

        if !self.live_text.is_empty() {
            lines.extend(markdown::wrap(&markdown::render(&self.live_text), width));
        } else if !self.live_reasoning.is_empty() {
            let last = self.live_reasoning.lines().last().unwrap_or("").to_string();
            lines.push(Line::styled(format!("  {} {}", theme::glyphs().reasoning, tools::one_line(&last, width as usize - 6)), theme::dim()));
        }
        if let Some(current) = self.todos.iter().find(|t| t.status == "in_progress") {
            let done = self.todos.iter().filter(|t| t.status == "completed").count();
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", theme::glyphs().todo_active), theme::accent()),
                Span::raw(tools::one_line(&current.content, width as usize - 16)),
                Span::styled(format!("  {done}/{}", self.todos.len()), theme::dim()),
            ]));
        }
        let doing = self.live_tool.clone().unwrap_or_else(|| {
            if self.live_reasoning.is_empty() || !self.live_text.is_empty() { "Working".into() } else { "Reasoning".into() }
        });
        let mut status = logo::mini(self.frame, self.mood(elapsed));
        status.extend([
            Span::styled(tools::one_line(&doing, width as usize / 2), theme::code()),
            Span::styled(format!("  {elapsed}s · esc to interrupt"), theme::dim()),
        ]);
        lines.push(Line::from(status));
        lines
    }
}

/// The choices on a permission prompt, in display order.
fn approval_options(req: &ApprovalRequest) -> Vec<(Decision, String)> {
    let mut options = vec![(Decision::Yes, "Yes".to_string())];
    if let Some(always) = &req.always {
        options.push((Decision::Always, format!("Yes, and allow {always} for the rest of this session")));
    }
    options.push((Decision::No, "No, and tell ember what to do instead".to_string()));
    options
}

fn notice(text: impl Into<String>) -> Line<'static> {
    Line::styled(format!("  {}", text.into()), theme::dim())
}

fn todo_lines(todos: &[Todo]) -> Vec<Line<'static>> {
    todos
        .iter()
        .map(|t| {
            let (mark, style) = match t.status.as_str() {
                "completed" => (theme::glyphs().todo_done, theme::ok()),
                "in_progress" => (theme::glyphs().todo_active, theme::accent().add_modifier(Modifier::BOLD)),
                _ => (theme::glyphs().todo, theme::dim()),
            };
            let text_style = match t.status.as_str() {
                "completed" => theme::dim().add_modifier(Modifier::CROSSED_OUT),
                "in_progress" => theme::bold(),
                _ => ratatui::style::Style::default(),
            };
            Line::from(vec![Span::styled(format!("    {mark} "), style), Span::styled(t.content.clone(), text_style)])
        })
        .collect()
}

fn diff_lines(diff: &str, max: usize) -> Vec<Line<'static>> {
    let lines: Vec<&str> = diff
        .lines()
        .filter(|l| !["---", "+++", "index ", "new file mode", "deleted file mode"].iter().any(|p| l.starts_with(p)))
        .collect();
    let mut out: Vec<Line<'static>> =
        lines.iter().take(max).map(|line| Line::styled(format!("    {line}"), theme::diff_line(line))).collect();
    if lines.len() > max {
        out.push(notice(format!("  … {} more lines", lines.len() - max)));
    }
    out
}

/// Rank a path for an @-mention query: substring matches (especially in the file name) beat
/// scattered subsequence matches, and shorter paths win ties.
fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let len = candidate.len() as i64;
    if query.is_empty() {
        return Some(-len);
    }
    let q = query.to_lowercase();
    let c = candidate.to_lowercase();
    if let Some(pos) = c.find(&q) {
        let name = c.rsplit('/').next().unwrap_or(&c);
        let bonus = if name.starts_with(&q) { 800 } else if name.contains(&q) { 500 } else { 0 };
        return Some(2000 + bonus - pos as i64 - len);
    }
    let mut chars = c.chars();
    let mut gaps = 0;
    for qc in q.chars() {
        loop {
            match chars.next() {
                Some(cc) if cc == qc => break,
                Some(_) => gaps += 1,
                None => return None,
            }
        }
    }
    Some(-gaps - len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_file_name_substrings() {
        let files = ["src/ui.rs", "src/tools.rs", "tests/test_ui_helpers.rs", "README.md"];
        let best = |q: &str| files.iter().filter_map(|f| fuzzy_score(q, f).map(|s| (s, *f))).max().map(|(_, f)| f);
        assert_eq!(best("ui"), Some("src/ui.rs"));
        assert_eq!(best("tls"), Some("src/tools.rs"));
        assert_eq!(best("zzz"), None);
    }
}
