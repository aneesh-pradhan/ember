//! User-defined slash commands: Markdown prompt templates in `.ember/commands/` (per project)
//! and `~/.config/ember/commands/` (everywhere). `$ARGUMENTS` is replaced with what follows the command.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Custom {
    pub name: String,
    pub description: String,
    pub template: String,
    pub project: bool,
}

impl Custom {
    pub fn expand(&self, args: &str) -> String {
        if self.template.contains("$ARGUMENTS") {
            self.template.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.template.clone()
        } else {
            format!("{}\n\n{args}", self.template)
        }
    }
}

fn parse(name: &str, text: &str, project: bool) -> Custom {
    let (front, body) = match text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) {
        Some((front, body)) => (front, body.trim_start_matches(['-', '\n'])),
        None => ("", text),
    };
    let description = front
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().trim_matches('"').to_string())
        .or_else(|| body.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).map(String::from))
        .unwrap_or_default();
    Custom { name: name.to_string(), description, template: body.trim().to_string(), project }
}

fn load_dir(dir: &Path, project: bool, into: &mut BTreeMap<String, Custom>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if let (true, Ok(text)) = (valid, std::fs::read_to_string(&path)) {
                into.insert(name.to_string(), parse(name, &text, project));
            }
        }
    }
}

/// Custom commands for `cwd`; project commands override global ones with the same name.
pub fn load(cwd: &Path) -> Vec<Custom> {
    let mut all = BTreeMap::new();
    if let Some(global) = crate::config::user_env_file().and_then(|f| f.parent().map(|d| d.join("commands"))) {
        load_dir(&global, false, &mut all);
    }
    load_dir(&cwd.join(".ember").join("commands"), true, &mut all);
    all.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_front_matter_and_expands_arguments() {
        let c = parse("explain", "---\ndescription: Explain a file simply\n---\nExplain $ARGUMENTS to a beginner.", true);
        assert_eq!(c.description, "Explain a file simply");
        assert_eq!(c.expand("src/main.rs"), "Explain src/main.rs to a beginner.");
        let d = parse("tidy", "# Tidy imports\nSort and dedupe imports.", false);
        assert_eq!(d.description, "Tidy imports");
        assert_eq!(d.expand("in lib.rs"), "# Tidy imports\nSort and dedupe imports.\n\nin lib.rs");
    }
}
