//! The terminal UI. Finished output is printed into the terminal's normal scrollback;
//! only a small inline area at the bottom (live output, input, status) is redrawn.

use std::collections::BTreeMap;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentEvent, ApprovalRequest, Decision, Events};
use crate::config::{Config, Tier};
use crate::fix::{self, FixEvent, Status};
use crate::input::Input;
use crate::logo;
use crate::llm::Client;
use crate::markdown;
use crate::theme;
use crate::tools::{self, Workspace};

const VIEW_HEIGHT: u16 = 10;

pub struct App {
    config: Config,
    client: Client,
    cwd: PathBuf,
    agent: Arc<tokio::sync::Mutex<Agent>>,
    model: Arc<Mutex<String>>,
    auto: Arc<AtomicBool>,
    tx: Events,
    task: Option<JoinHandle<()>>,
    task_started: Instant,
    input: Input,
    live_text: String,
    live_reasoning: String,
    live_tool: Option<String>,
    approval: Option<ApprovalRequest>,
    fix: Option<FixView>,
    usage: BTreeMap<String, (u64, u64)>,
    pending: Vec<Line<'static>>,
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

pub async fn run(config: Config, cwd: PathBuf, auto_approve: bool, initial: Option<String>) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let client = Client::new(&config.base_url, config.api_key.clone());
    let model = Arc::new(Mutex::new(config.coder.clone()));
    let auto = Arc::new(AtomicBool::new(auto_approve));
    let agent = Agent::new(client.clone(), model.clone(), auto.clone(), Workspace::new(cwd.clone()));
    let mut app = App {
        config,
        client,
        cwd,
        agent: Arc::new(tokio::sync::Mutex::new(agent)),
        model,
        auto,
        tx,
        task: None,
        task_started: Instant::now(),
        input: Input::default(),
        live_text: String::new(),
        live_reasoning: String::new(),
        live_tool: None,
        approval: None,
        fix: None,
        usage: BTreeMap::new(),
        pending: vec![],
        frame: 0,
        quit: false,
        last_ctrl_c: None,
    };

    let mut terminal = ratatui::init_with_options(TerminalOptions { viewport: Viewport::Inline(VIEW_HEIGHT) });
    let _ = crossterm::execute!(stdout(), EnableBracketedPaste);
    app.print_header();
    if let Some(prompt) = initial {
        app.submit(prompt);
    }
    let result = app.main_loop(&mut terminal, rx).await;
    let _ = app.flush(&mut terminal);
    let _ = terminal.clear();
    let _ = crossterm::execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

impl App {
    async fn main_loop(&mut self, terminal: &mut DefaultTerminal, mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            self.flush(terminal)?;
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
                _ = tick.tick() => self.frame = self.frame.wrapping_add(1),
            }
        }
    }

    // ------------------------------------------------------------- output

    fn print(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.pending.extend(lines);
    }

    fn print_line(&mut self, line: Line<'static>) {
        self.pending.push(line);
    }

    fn flush(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let size = terminal.size()?;
        let lines = markdown::wrap(&std::mem::take(&mut self.pending), size.width);
        let chunk = size.height.saturating_sub(VIEW_HEIGHT).max(1) as usize;
        for part in lines.chunks(chunk) {
            let part = part.to_vec();
            terminal.insert_before(part.len() as u16, |buf| {
                let area = buf.area;
                Paragraph::new(part).render(area, buf);
            })?;
        }
        Ok(())
    }

    fn print_header(&mut self) {
        let cwd = self.cwd.display().to_string();
        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        let cwd = if !home.is_empty() && cwd.starts_with(&home) { cwd.replacen(&home, "~", 1) } else { cwd };
        let model = self.config.coder.clone();
        self.print(logo::banner(&model, &cwd));
        if self.config.api_key.is_none() {
            self.print_line(Line::styled(
                "  ! NEBIUS_API_KEY is not set. Add it to .env or ~/.config/ember/env, then restart.",
                theme::err(),
            ));
        }
    }

    fn print_user(&mut self, text: &str) {
        self.print_line(Line::default());
        for (i, line) in text.lines().enumerate() {
            let lead = if i == 0 { Span::styled("› ", theme::accent()) } else { Span::raw("  ") };
            self.print_line(Line::from(vec![lead, Span::styled(line.to_string(), theme::bold())]));
        }
    }

    fn print_notice(&mut self, text: impl Into<String>) {
        self.print_line(notice(text));
    }

    fn print_diff(&mut self, diff: &str, max: usize) {
        self.pending.extend(diff_lines(diff, max));
    }

    // ------------------------------------------------------------- agent events

    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(t) => self.live_text.push_str(&t),
            AgentEvent::Reasoning(r) => self.live_reasoning.push_str(&r),
            AgentEvent::Reply { content, thought_secs } => {
                if let Some(secs) = thought_secs {
                    self.print_line(Line::styled(format!("  ∴ reasoned for {secs:.1}s"), theme::dim()));
                }
                if !content.trim().is_empty() {
                    self.print_line(Line::default());
                    let rendered = markdown::render(&content);
                    for (i, line) in rendered.into_iter().enumerate() {
                        let lead = if i == 0 { Span::styled("● ", theme::accent()) } else { Span::raw("  ") };
                        let mut spans = vec![lead];
                        spans.extend(line.spans);
                        self.print_line(Line::from(spans));
                    }
                }
                self.live_text.clear();
                self.live_reasoning.clear();
            }
            AgentEvent::ToolStart { summary } => self.live_tool = Some(summary),
            AgentEvent::ToolDone { name, summary, output, ok } => {
                self.live_tool = None;
                let dot = if ok { theme::ok() } else { theme::err() };
                self.print_line(Line::from(vec![Span::styled("  ● ", dot), Span::styled(summary, theme::bold())]));
                if !ok {
                    self.print_line(Line::styled(format!("    {}", tools::one_line(&output, 200)), theme::err()));
                } else if name == "edit_file" || (name == "write_file" && output.starts_with("---")) {
                    self.print_diff(&output, 30);
                } else if name == "bash" {
                    let lines: Vec<&str> = output.lines().collect();
                    let start = lines.len().saturating_sub(8);
                    if start > 0 {
                        self.print_notice(format!("  … {start} earlier lines"));
                    }
                    for line in &lines[start..] {
                        self.print_line(Line::styled(format!("    │ {line}"), theme::dim()));
                    }
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
                    } else {
                        self.print_line(Line::styled(format!("  ◇ $ {preview}"), theme::bold()));
                    }
                }
                self.approval = Some(req);
            }
            AgentEvent::Usage { model, prompt, completion } => {
                let entry = self.usage.entry(model).or_default();
                entry.0 += prompt;
                entry.1 += completion;
            }
            AgentEvent::Notice(text) => self.print_line(Line::styled(format!("  {text}"), theme::info())),
            AgentEvent::Fix(ev) => self.on_fix(ev),
            AgentEvent::Error(err) => {
                self.print_line(Line::styled(format!("  ✗ {err}"), theme::err()));
            }
            AgentEvent::Done => {
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
        let out = &mut self.pending;
        match ev {
            FixEvent::Stage { name, status, detail } => {
                match view.stages.iter_mut().find(|(n, _)| *n == name) {
                    Some(stage) => stage.1 = status,
                    None => view.stages.push((name, status)),
                }
                view.activity.clear();
                let (mark, style) = match status {
                    Status::Running => return,
                    Status::Done => ("✓", theme::ok()),
                    Status::Failed => ("✗", theme::err()),
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
                    Span::styled("    ✗ FAILS ", theme::err().add_modifier(ratatui::style::Modifier::BOLD)),
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
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Paste(text) if self.approval.is_none() => self.input.insert(&text),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if let Some(req) = self.approval.take() {
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Decision::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::Always),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::No),
                KeyCode::Char('c') if ctrl => Some(Decision::No),
                _ => None,
            };
            match decision {
                Some(d) => {
                    let label = match d {
                        Decision::Yes => "approved",
                        Decision::Always => "approved for this session",
                        Decision::No => "declined",
                    };
                    self.print_notice(format!("  {label}"));
                    let _ = req.reply.send(d);
                }
                None => self.approval = Some(req),
            }
            return;
        }

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
            KeyCode::Esc if self.task.is_some() => self.interrupt(),
            KeyCode::BackTab => self.toggle_auto(),
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

    fn toggle_auto(&mut self) {
        let on = !self.auto.load(Ordering::Relaxed);
        self.auto.store(on, Ordering::Relaxed);
        self.print_notice(if on {
            "Auto-approve on: edits and commands run without asking (shift+tab to turn off)"
        } else {
            "Auto-approve off: ember asks before editing files or running commands"
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

    fn submit(&mut self, text: String) {
        let text = text.trim().to_string();
        if let Some(command) = text.strip_prefix('/') {
            self.command(command.to_string());
            return;
        }
        if self.task.is_some() {
            self.print_notice("ember is still working; press Esc to interrupt first");
            return;
        }
        self.print_user(&text);
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            let mut agent = agent.lock().await;
            if let Err(e) = agent.turn(text, &tx).await {
                let _ = tx.send(AgentEvent::Error(format!("{e:#}")));
            }
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
            "auto" => self.toggle_auto(),
            "model" => self.set_model(arg),
            "usage" | "cost" => self.print_usage(),
            "clear" | "new" => {
                if self.task.is_some() {
                    self.print_notice("Wait for the current task to finish (or press Esc)");
                } else {
                    let agent = self.agent.clone();
                    let reset = match agent.try_lock() {
                        Ok(mut agent) => {
                            agent.reset();
                            true
                        }
                        Err(_) => false,
                    };
                    if reset {
                        self.print_notice("Started a new conversation");
                    }
                }
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
                } else if self.task.is_some() {
                    self.print_notice("ember is still working; press Esc to interrupt first");
                } else {
                    self.print_user(&format!("/fix {arg}"));
                    self.fix = Some(FixView::default());
                    let (client, config, cwd, tx) = (self.client.clone(), self.config.clone(), self.cwd.clone(), self.tx.clone());
                    let auto = self.auto.load(Ordering::Relaxed);
                    let issue = arg.to_string();
                    self.spawn(async move {
                        if let Err(e) = fix::run(client, config, cwd, issue, auto, tx.clone()).await {
                            let _ = tx.send(AgentEvent::Error(format!("/fix stopped: {e:#}")));
                        }
                    });
                }
            }
            other => self.print_notice(format!("Unknown command /{other}. Try /help")),
        }
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
    }

    fn print_help(&mut self) {
        let rows = [
            ("/fix <bug | issue URL>", "write a failing test for the bug, fix it in parallel worktrees, verify, apply"),
            ("/model [nano|super|ultra|id]", "show or switch the Nemotron model"),
            ("/models", "list Nemotron models available to your API key"),
            ("/auto", "toggle auto-approve for edits and commands (also shift+tab)"),
            ("/usage", "token usage this session"),
            ("/clear", "start a new conversation"),
            ("/quit", "exit (or Ctrl+C twice)"),
        ];
        let mut lines = vec![Line::default()];
        for (cmd, desc) in rows {
            lines.push(Line::from(vec![Span::styled(format!("  {cmd:<30}"), theme::accent()), Span::styled(desc, theme::dim())]));
        }
        lines.push(Line::styled("  Enter sends · Alt+Enter or Ctrl+J for a new line · Esc interrupts · ↑/↓ history", theme::dim()));
        lines.push(Line::styled("  Project instructions are read from AGENTS.md or EMBER.md in the working directory.", theme::dim()));
        self.print(lines);
    }

    fn print_usage(&mut self) {
        if self.usage.is_empty() {
            self.print_notice("No model calls yet");
            return;
        }
        let rows: Vec<Line<'static>> = self
            .usage
            .iter()
            .map(|(model, (p, c))| Line::styled(format!("  {model:<45} {p:>9} in {c:>8} out"), theme::dim()))
            .collect();
        self.print(rows);
    }

    // ------------------------------------------------------------- drawing

    fn draw(&self, f: &mut Frame) {
        let [live, rule, input, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());

        let live_lines = self.live_lines(live.width);
        let skip = live_lines.len().saturating_sub(live.height as usize);
        Paragraph::new(live_lines[skip..].to_vec()).render(live, f.buffer_mut());

        let rule_style = if self.approval.is_some() { theme::accent() } else { theme::dim() };
        Paragraph::new(Line::styled("─".repeat(rule.width as usize), rule_style)).render(rule, f.buffer_mut());

        let (row, col) = self.input.cursor_pos();
        let first = row.saturating_sub(input.height as usize - 1);
        let lines: Vec<Line> = if self.input.is_empty() {
            vec![Line::from(vec![
                Span::styled("› ", theme::accent()),
                Span::styled("Ask ember to build, fix or explain something · /help", theme::dim()),
            ])]
        } else {
            self.input
                .text()
                .split('\n')
                .enumerate()
                .skip(first)
                .map(|(i, l)| {
                    let lead = if i == 0 { Span::styled("› ", theme::accent()) } else { Span::raw("  ") };
                    Line::from(vec![lead, Span::raw(l.to_string())])
                })
                .collect()
        };
        Paragraph::new(lines).render(input, f.buffer_mut());
        if self.approval.is_none() {
            let x = (input.x + 2 + col as u16).min(input.right().saturating_sub(1));
            f.set_cursor_position(Position { x, y: input.y + (row - first) as u16 });
        }

        Paragraph::new(self.status_line()).render(status, f.buffer_mut());
    }

    fn live_lines(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(req) = &self.approval {
            return vec![
                Line::from(vec![Span::styled("◇ ", theme::accent()), Span::styled(format!("Allow: {}?", req.summary), theme::bold())]),
                Line::from(vec![
                    Span::styled("  y", theme::accent()),
                    Span::raw(" yes   "),
                    Span::styled("a", theme::accent()),
                    Span::raw(" always (this session)   "),
                    Span::styled("n", theme::accent()),
                    Span::raw(" no"),
                ]),
            ];
        }
        let Some(_) = &self.task else { return vec![] };
        let spinner = theme::SPINNER[self.frame % theme::SPINNER.len()];
        let elapsed = self.task_started.elapsed().as_secs();
        let mut lines = vec![];

        if let Some(view) = &self.fix {
            let stages: Vec<Span<'static>> = ["Investigate", "Reproduce", "Plan", "Fix", "Review"]
                .iter()
                .flat_map(|name| {
                    let status = view.stages.iter().find(|(n, _)| n == name).map(|(_, s)| *s);
                    let (mark, style) = match status {
                        Some(Status::Done) => ("✓ ".to_string(), theme::ok()),
                        Some(Status::Failed) => ("✗ ".to_string(), theme::err()),
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
            lines.push(Line::styled(format!("  ∴ {}", tools::one_line(&last, width as usize - 6)), theme::dim()));
        }
        let doing = self.live_tool.clone().unwrap_or_else(|| {
            if self.live_reasoning.is_empty() || !self.live_text.is_empty() { "Working".into() } else { "Reasoning".into() }
        });
        let mut status = logo::mini(self.frame);
        status.extend([
            Span::styled(tools::one_line(&doing, width as usize / 2), theme::code()),
            Span::styled(format!("  {elapsed}s · esc to interrupt"), theme::dim()),
        ]);
        lines.push(Line::from(status));
        lines
    }

    fn status_line(&self) -> Line<'static> {
        let model = self.model.lock().unwrap().clone();
        let tier = [Tier::Fast, Tier::Coder, Tier::Reasoner].into_iter().find(|t| self.config.model(*t) == model);
        let label = tier.map_or("custom", |t| t.label());
        let tokens: u64 = self.usage.values().map(|(p, c)| p + c).sum();
        let mut spans = vec![
            Span::styled(format!(" {label} "), tier.map_or(theme::bold(), theme::tier)),
            Span::styled(model.rsplit('/').next().unwrap_or("").to_string(), theme::dim()),
        ];
        if self.auto.load(Ordering::Relaxed) {
            spans.push(Span::styled("  ⏵⏵ auto-approve", theme::accent()));
        }
        if tokens > 0 {
            spans.push(Span::styled(format!("  {:.1}k tokens", tokens as f64 / 1000.0), theme::dim()));
        }
        spans.push(Span::styled("  · shift+tab auto-approve · /help", theme::dim()));
        Line::from(spans)
    }
}

fn notice(text: impl Into<String>) -> Line<'static> {
    Line::styled(format!("  {}", text.into()), theme::dim())
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
