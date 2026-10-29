//! Commands and tools the user allowed without asking, saved per project with `/permissions save`.
//!
//! They live in the user's config (`~/.config/ember/permissions.json`, keyed by the project's path), never in the
//! repository: a cloned repo must not be able to pre-approve its own commands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What runs without a permission prompt: command prefixes (see `agent::command_prefix`) and tool names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowed {
    #[serde(default)]
    pub commands: BTreeSet<String>,
    #[serde(default)]
    pub tools: BTreeSet<String>,
}

impl Allowed {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.tools.is_empty()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    projects: BTreeMap<String, Allowed>,
}

/// `$XDG_CONFIG_HOME/ember/permissions.json`, next to the `env` file.
pub fn file() -> Option<PathBuf> {
    crate::config::user_env_file()?.parent().map(|dir| dir.join("permissions.json"))
}

fn key(root: &Path) -> String {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).display().to_string()
}

fn read(file: &Path) -> Saved {
    std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// What `file` holds for the project at `root`; nothing when there's no file or it can't be read.
pub fn load_from(file: &Path, root: &Path) -> Allowed {
    read(file).projects.remove(&key(root)).unwrap_or_default()
}

/// Save `allowed` for the project at `root`, replacing what was saved before; an empty set removes the project.
pub fn save_to(file: &Path, root: &Path, allowed: &Allowed) -> Result<()> {
    // Refuse to overwrite a hand-edited file that no longer parses: it holds other projects' settings too.
    let mut saved: Saved = match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} isn't valid JSON; fix or delete it", file.display()))?,
        Err(_) => Saved::default(),
    };
    if allowed.is_empty() {
        saved.projects.remove(&key(root));
    } else {
        saved.projects.insert(key(root), allowed.clone());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, serde_json::to_string_pretty(&saved)? + "\n").with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_per_project_and_forgets() {
        let dir = std::env::temp_dir().join(format!("ember-perms-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let file = dir.join("config/permissions.json");
        assert_eq!(load_from(&file, &a), Allowed::default(), "no file yet");

        let mut allowed = Allowed::default();
        allowed.commands.insert("cargo test".into());
        allowed.tools.insert("edit_file".into());
        save_to(&file, &a, &allowed).unwrap();
        let mut other = Allowed::default();
        other.commands.insert("npm test".into());
        save_to(&file, &b, &other).unwrap();

        assert_eq!(load_from(&file, &a), allowed);
        assert_eq!(load_from(&file, &a.join(".")), allowed, "keyed by the canonical path");
        save_to(&file, &a, &Allowed::default()).unwrap();
        assert_eq!(load_from(&file, &a), Allowed::default());
        assert_eq!(load_from(&file, &b), other, "other projects are kept");

        std::fs::write(&file, "not json").unwrap();
        assert_eq!(load_from(&file, &b), Allowed::default(), "an unreadable file allows nothing");
        assert!(save_to(&file, &a, &allowed).is_err(), "and isn't overwritten");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
