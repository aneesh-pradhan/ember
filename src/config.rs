//! Settings from the environment, `~/.config/ember/env`, and a project `.env`.

use std::env;
use std::path::PathBuf;

pub const DEFAULT_BASE_URL: &str = "https://api.tokenfactory.nebius.com/v1/";

/// The three Nemotron tiers ember routes work between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    /// Fast, cheap calls: triage and yes/no judgements.
    Fast,
    /// The everyday coding model: the interactive agent and patch writing.
    Coder,
    /// Deep reasoning: planning fix strategies and reviewing candidates.
    Reasoner,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Fast => "Nano",
            Tier::Coder => "Super",
            Tier::Reasoner => "Ultra",
        }
    }

    pub fn parse(name: &str) -> Option<Tier> {
        match name.to_ascii_lowercase().as_str() {
            "nano" | "fast" => Some(Tier::Fast),
            "super" | "coder" => Some(Tier::Coder),
            "ultra" | "reasoner" => Some(Tier::Reasoner),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: Option<String>,
    pub base_url: String,
    pub fast: String,
    pub coder: String,
    pub reasoner: String,
    pub fix_branches: usize,
    pub test_cmd: Option<String>,
}

impl Config {
    pub fn load() -> Config {
        // Project .env first, then the user-level file; real env vars win over both.
        let _ = dotenvy::dotenv();
        if let Some(path) = user_env_file() {
            let _ = dotenvy::from_path(path);
        }
        let var = |name: &str, default: &str| env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string());
        Config {
            api_key: env::var("NEBIUS_API_KEY").ok().filter(|v| !v.is_empty()),
            base_url: var("NEBIUS_BASE_URL", DEFAULT_BASE_URL),
            fast: var("EMBER_MODEL_FAST", "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B"),
            coder: var("EMBER_MODEL_CODER", "nvidia/nemotron-3-super-120b-a12b"),
            reasoner: var("EMBER_MODEL_REASONER", "nvidia/Nemotron-3-Ultra-550b-a55b"),
            fix_branches: var("EMBER_FIX_BRANCHES", "3").parse().unwrap_or(3).clamp(1, 4),
            test_cmd: env::var("EMBER_TEST_CMD").ok().filter(|v| !v.is_empty()),
        }
    }

    pub fn model(&self, tier: Tier) -> &str {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Coder => &self.coder,
            Tier::Reasoner => &self.reasoner,
        }
    }
}

/// Context window size in tokens for a model id (from the Token Factory catalog; conservative default).
pub fn context_window(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("ultra") || m.contains("3_5") || m.contains("3.5") {
        1_000_000
    } else if m.contains("nemotron-3") {
        256_000
    } else {
        128_000
    }
}

pub fn user_env_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ember").join("env"))
}
