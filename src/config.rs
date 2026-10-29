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
    /// Mechanical writing: commit messages, branch names, PR text and /compact summaries.
    pub draft: String,
    pub fix_branches: usize,
    pub test_cmd: Option<String>,
    /// Cap on reasoning tokens per request; protects against runaway reasoning without limiting normal answers.
    pub think_budget: Option<u32>,
}

impl Config {
    pub fn load() -> Config {
        // Project .env first, then the user-level file; real env vars win over both.
        let _ = dotenvy::dotenv();
        // ~/.config/ember/env on every platform; also the platform config dir
        // (~/Library/Application Support/ember/env on macOS) for anyone who put it there.
        let platform = dirs::config_dir().map(|d| d.join("ember").join("env"));
        for path in [user_env_file(), platform].into_iter().flatten() {
            let _ = dotenvy::from_path(path);
        }
        let var = |name: &str, default: &str| env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string());
        Config {
            api_key: env::var("NEBIUS_API_KEY").ok().filter(|v| !v.is_empty()),
            // EMBER_BASE_URL points ember at any OpenAI-compatible server (e.g. Nemotron on your own Jetson).
            base_url: env::var("EMBER_BASE_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| var("NEBIUS_BASE_URL", DEFAULT_BASE_URL)),
            fast: var("EMBER_MODEL_FAST", "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B"),
            coder: var("EMBER_MODEL_CODER", "nvidia/nemotron-3-super-120b-a12b"),
            reasoner: var("EMBER_MODEL_REASONER", "nvidia/Nemotron-3-Ultra-550b-a55b"),
            draft: String::new(),
            fix_branches: var("EMBER_FIX_BRANCHES", "3").parse().unwrap_or(3).clamp(1, 4),
            test_cmd: env::var("EMBER_TEST_CMD").ok().filter(|v| !v.is_empty()),
            think_budget: None,
        }
        .with_budget()
        .with_draft_model()
    }

    /// EMBER_MODEL_DRAFT, else Lightning on Token Factory (on real diffs and sessions it wrote commit messages and
    /// /compact summaries as well as Super, about 5x cheaper), else the coder model.
    fn with_draft_model(mut self) -> Self {
        self.draft = env::var("EMBER_MODEL_DRAFT").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| {
            if self.base_url.contains("nebius.com") { "nvidia/Nemotron-3_5-Lightning".into() } else { self.coder.clone() }
        });
        self
    }

    /// EMBER_THINK_BUDGET=<tokens> or `off`. Default 4096 on Token Factory (where it is honored), none elsewhere.
    fn with_budget(mut self) -> Self {
        self.think_budget = match env::var("EMBER_THINK_BUDGET").ok().as_deref().map(str::trim) {
            Some("off" | "0" | "none") => None,
            Some(n) if !n.is_empty() => n.parse().ok(),
            _ if self.base_url.contains("nebius.com") => Some(4096),
            _ => None,
        };
        self
    }

    pub fn client(&self) -> crate::llm::Client {
        crate::llm::Client::new(&self.base_url, self.api_key.clone()).with_think_budget(self.think_budget)
    }

    /// What a call cost in USD on Token Factory, or None for other servers and models without a known price.
    pub fn cost(&self, model: &str, prompt: u64, completion: u64, cached: u64) -> Option<f64> {
        if !self.base_url.contains("nebius.com") {
            return None;
        }
        let (input, cached_input, output) = price(model)?;
        let fresh = prompt.saturating_sub(cached);
        Some((fresh as f64 * input + cached as f64 * cached_input + completion as f64 * output) / 1e6)
    }

    pub fn model(&self, tier: Tier) -> &str {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Coder => &self.coder,
            Tier::Reasoner => &self.reasoner,
        }
    }
}

/// The vision model used to look at images (Nemotron VL).
pub fn vision_model() -> String {
    env::var("EMBER_MODEL_VISION").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "nvidia/Nemotron-3.5-Super-VL".into())
}

/// Token Factory list prices in USD per 1M tokens: input, cached input, output (eu-north1, Oct 2026; the same table
/// as bench/run.py). Cached input is 5x cheaper on Super, which is why ember keeps every request a prefix extension.
fn price(model: &str) -> Option<(f64, f64, f64)> {
    match model.to_ascii_lowercase().as_str() {
        "nvidia/nemotron-3-super-120b-a12b" => Some((0.30, 0.06, 0.90)),
        "nvidia/nemotron-3-ultra-550b-a55b" => Some((1.00, 0.12, 3.00)),
        "nvidia/nvidia-nemotron-3-nano-30b-a3b" => Some((0.06, 0.012, 0.24)),
        "nvidia/nemotron-3_5-lightning" => Some((0.06, 0.03, 0.24)),
        _ => None,
    }
}

/// A dollar amount for the status bar and /usage: tenths of a cent while it's small.
pub fn format_cost(usd: f64) -> String {
    if usd < 1.0 { format!("${usd:.3}") } else { format!("${usd:.2}") }
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

/// `$XDG_CONFIG_HOME/ember/env`, defaulting to `~/.config/ember/env` (on macOS too).
pub fn user_env_file() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("ember").join("env"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: &str) -> Config {
        Config {
            api_key: None,
            base_url: base_url.into(),
            fast: String::new(),
            coder: String::new(),
            reasoner: String::new(),
            draft: String::new(),
            fix_branches: 3,
            test_cmd: None,
            think_budget: None,
        }
    }

    #[test]
    fn prices_calls_at_token_factory_rates() {
        let tf = config(DEFAULT_BASE_URL);
        // 100k input of which 80k cached, 2k output on Super: 20k × $0.30 + 80k × $0.06 + 2k × $0.90 per 1M.
        let cost = tf.cost("nvidia/nemotron-3-super-120b-a12b", 100_000, 2_000, 80_000).unwrap();
        assert!((cost - 0.0126).abs() < 1e-9, "{cost}");
        assert!(tf.cost("nvidia/Nemotron-3-Ultra-550b-a55b", 1_000_000, 0, 0).is_some_and(|c| (c - 1.0).abs() < 1e-9));
        assert_eq!(tf.cost("nvidia/Nemotron-3.5-Super-VL", 1000, 10, 0), None, "unknown price");
        assert_eq!(config("http://localhost:8000/v1/").cost("nvidia/nemotron-3-super-120b-a12b", 1000, 10, 0), None, "self-hosted");
        assert_eq!((format_cost(0.0126), format_cost(12.345)), ("$0.013".to_string(), "$12.35".to_string()));
    }
}
