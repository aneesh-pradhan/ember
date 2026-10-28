mod agent;
mod config;
mod fix;
mod input;
mod llm;
mod logo;
mod markdown;
mod theme;
mod tools;
mod ui;

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;

use crate::agent::{Agent, AgentEvent, Decision};
use crate::config::{Config, Tier};
use crate::llm::Client;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load();
    if let Some(m) = &cli.model {
        config.coder = Tier::parse(m).map_or(m.clone(), |t| config.model(t).to_string());
    }
    let cwd = std::env::current_dir()?;
    if cli.print {
        let prompt = cli.prompt.unwrap_or_default();
        return print_mode(config, cwd, prompt, cli.yes).await;
    }
    ui::run(config, cwd, cli.yes, cli.prompt).await
}

/// Non-interactive mode: stream the answer to stdout. Mutating tools are denied unless --yes.
async fn print_mode(config: Config, cwd: std::path::PathBuf, prompt: String, yes: bool) -> Result<()> {
    let client = Client::new(&config.base_url, config.api_key.clone());
    let model = Arc::new(Mutex::new(config.coder.clone()));
    let mut agent = Agent::new(client, model, Arc::new(AtomicBool::new(yes)), Workspace::new(cwd));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let turn = tokio::spawn(async move { agent.turn(prompt, &tx).await });
    let mut out = std::io::stdout();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Text(t) => {
                print!("{t}");
                out.flush()?;
            }
            AgentEvent::Reply { content, .. } if !content.is_empty() => println!(),
            AgentEvent::ToolDone { summary, ok, .. } => eprintln!("[{}] {summary}", if ok { "ok" } else { "failed" }),
            AgentEvent::Approval(req) => {
                eprintln!("[denied] {} (pass --yes to allow)", req.summary);
                let _ = req.reply.send(Decision::No);
            }
            AgentEvent::Error(e) => eprintln!("error: {e}"),
            _ => {}
        }
    }
    turn.await??;
    Ok(())
}
