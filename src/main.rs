mod agent;
mod commands;
mod config;
mod fix;
mod git;
mod input;
mod llm;
mod logo;
mod markdown;
mod mcp;
mod session;
mod theme;
mod tools;
mod ui;
mod vision;
mod web;

use std::io::Write;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;

use crate::agent::{Agent, AgentEvent, Decision, Mode, SharedMode};
use crate::config::{Config, Tier};
use crate::tools::Workspace;

/// ember: a terminal coding agent powered by NVIDIA Nemotron on Nebius Token Factory.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Start the session with this prompt.
    prompt: Option<String>,
    /// Print mode: answer the prompt non-interactively and exit.
    #[arg(short, long)]
    print: bool,
    /// Model tier (nano, super, ultra) or a full model id.
    #[arg(short, long)]
    model: Option<String>,
    /// Run edits and commands without asking.
    #[arg(long)]
    yes: bool,
    /// Start in plan mode (read-only investigation that ends in a plan).
    #[arg(long, conflicts_with = "yes")]
    plan: bool,
    /// Nemotron reasoning for the agent: on (default), low or off.
    #[arg(long, value_name = "LEVEL")]
    think: Option<String>,
    /// Continue the most recent conversation in this directory.
    #[arg(short = 'c', long = "continue")]
    resume: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load();
    if let Some(m) = &cli.model {
        config.coder = Tier::parse(m).map_or(m.clone(), |t| config.model(t).to_string());
    }
    let cwd = std::env::current_dir()?;
    let think = match cli.think.as_deref().or(std::env::var("EMBER_THINK").ok().as_deref()) {
        Some(t) => crate::llm::Think::parse(t).ok_or_else(|| anyhow::anyhow!("--think must be on, low or off"))?,
        None => crate::llm::Think::On,
    };
    if cli.print {
        let prompt = cli.prompt.unwrap_or_default();
        return print_mode(config, cwd, prompt, cli.yes, think).await;
    }
    let mode = if cli.yes { Mode::Auto } else if cli.plan { Mode::Plan } else { Mode::Ask };
    ui::run(config, cwd, mode, cli.prompt, cli.resume, think).await
}

/// Non-interactive mode: stream the answer to stdout. Mutating tools are denied unless --yes.
async fn print_mode(config: Config, cwd: std::path::PathBuf, prompt: String, yes: bool, think: crate::llm::Think) -> Result<()> {
    let client = config.client();
    let model = Arc::new(Mutex::new(config.coder.clone()));
    let mode = SharedMode::new(if yes { Mode::Auto } else { Mode::Ask });
    let mut agent = Agent::new(client, model, mode, Workspace::new(cwd.clone()));
    agent.mcp.clone().start(&cwd).await;
    *agent.think.lock().unwrap() = think;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let turn = tokio::spawn(async move { agent.turn(prompt, &tx).await });
    let mut out = std::io::stdout();
    let (mut steps, mut tools, mut prompt_tokens, mut cached, mut completion) = (0, 0, 0, 0, 0);
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Text(t) => {
                print!("{t}");
                out.flush()?;
            }
            AgentEvent::Reply { content, .. } if !content.is_empty() => println!(),
            AgentEvent::ToolDone { summary, ok, .. } => {
                tools += 1;
                eprintln!("[{}] {summary}", if ok { "ok" } else { "failed" })
            }
            AgentEvent::Usage { prompt, completion: c, cached: k, .. } => {
                steps += 1;
                prompt_tokens += prompt;
                completion += c;
                cached += k;
            }
            AgentEvent::Approval(req) => {
                eprintln!("[denied] {} (pass --yes to allow)", req.summary);
                let _ = req.reply.send(Decision::No);
            }
            AgentEvent::Error(e) => eprintln!("error: {e}"),
            _ => {}
        }
    }
    turn.await??;
    let pct = if prompt_tokens > 0 { cached * 100 / prompt_tokens } else { 0 };
    eprintln!("[usage] {steps} model calls, {tools} tool calls, {prompt_tokens} input tokens ({pct}% cached), {completion} output tokens");
    Ok(())
}
