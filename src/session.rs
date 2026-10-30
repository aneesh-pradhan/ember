//! Saved conversations in ~/.local/share/ember/sessions, for /resume and `ember --continue`.

use std::path::{Path, PathBuf};
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
    // Tests drive real agent turns, which save sessions: keep those out of the user's history.
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("ember-test-sessions-{}", std::process::id())));
    }
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

/// Days an untouched session is kept: `EMBER_SESSION_DAYS`, 30 by default; 0 keeps them forever.
fn keep_days() -> u64 {
    std::env::var("EMBER_SESSION_DAYS").ok().and_then(|d| d.trim().parse().ok()).unwrap_or(30)
}

/// Delete saved sessions nobody will resume: ones untouched for `keep_days()`, and ones from scratch projects in the
/// temp directory (benchmarks, tests) that are gone. Best effort; run once at startup.
pub fn prune() {
    if let Some(dir) = dir() {
        prune_in(&dir, keep_days(), now(), &std::env::temp_dir());
    }
}

fn prune_in(dir: &Path, days: u64, now: u64, temp: &Path) -> usize {
    // Just the fields pruning needs, so the messages aren't kept.
    #[derive(Deserialize)]
    struct Meta {
        cwd: String,
        updated: u64,
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let gone = if name.starts_with('.') && name.ends_with(".json.tmp") {
            // Left behind by a save that didn't finish; a save in progress is seconds old.
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            modified.and_then(|m| m.elapsed().ok()).is_some_and(|age| age.as_secs() > 86_400)
        } else if name.ends_with(".json") {
            // A file that doesn't parse is left alone: it may be from a newer ember.
            let Some(meta) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Meta>(&b).ok()) else { continue };
            let cwd = Path::new(&meta.cwd);
            let stale = days > 0 && now.saturating_sub(meta.updated) > days * 86_400;
            stale || (cwd.starts_with(temp) && !cwd.exists())
        } else {
            false
        };
        if gone && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_old_and_scratch_sessions() {
        let root = std::env::temp_dir().join(format!("ember-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (dir, temp, kept_project) = (root.join("sessions"), root.join("tmp"), root.join("project"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(temp.join("live")).unwrap();
        std::fs::create_dir_all(&kept_project).unwrap();
        let now = 100 * 86_400;
        let write = |id: &str, cwd: &Path, updated: u64| {
            let session = Session { id: id.into(), cwd: cwd.display().to_string(), updated, title: id.into(), messages: vec![] };
            std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&session).unwrap()).unwrap();
        };
        write("recent", &kept_project, now - 86_400);
        write("old", &kept_project, now - 31 * 86_400);
        write("scratch-gone", &temp.join("bench-1"), now);
        write("scratch-live", &temp.join("live"), now);
        write("moved-project", &root.join("unmounted/drive"), now);
        std::fs::write(dir.join("newer-format.json"), "{\"v\": 2}").unwrap();
        let tmp = |name: &str, age: u64| {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_modified(SystemTime::now() - std::time::Duration::from_secs(age)).unwrap();
        };
        tmp(".stale.json.tmp", 2 * 86_400);
        tmp(".saving.json.tmp", 5);

        assert_eq!(prune_in(&dir, 30, now, &temp), 3);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, [".saving.json.tmp", "moved-project.json", "newer-format.json", "recent.json", "scratch-live.json"]);

        assert_eq!(prune_in(&dir, 0, now + 1000 * 86_400, &temp), 0, "0 days keeps everything outside the temp directory");
        std::fs::remove_dir_all(root).unwrap();
    }
}
