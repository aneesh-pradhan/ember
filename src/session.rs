//! Saved conversations in ~/.local/share/ember/sessions, for /resume and `ember --continue`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::llm::Message;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    /// Unix seconds of the last save.
    pub updated: u64,
    pub title: String,
    pub messages: Vec<Message>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("{}-{:05x}", now(), nanos >> 12)
}

fn dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("ember").join("sessions"))
}

pub fn save(session: &Session) -> Result<()> {
    let dir = dir().context("no data directory")?;
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.json.tmp", session.id));
    std::fs::write(&tmp, serde_json::to_vec(session)?)?;
    std::fs::rename(tmp, dir.join(format!("{}.json", session.id)))?;
    Ok(())
}

/// Sessions started in `cwd`, most recent first.
pub fn list(cwd: &str) -> Vec<Session> {
    let Some(dir) = dir() else { return vec![] };
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut sessions: Vec<Session> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<Session>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|s| s.cwd == cwd)
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated));
    sessions
}

pub fn age(updated: u64) -> String {
    let secs = now().saturating_sub(updated);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
