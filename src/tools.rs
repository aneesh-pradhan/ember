//! The actions the model can take in a workspace: read, search, edit, write, and run commands.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Value};
use similar::TextDiff;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::llm::ToolSpec;

const MAX_OUTPUT: usize = 30_000;

/// Where tools operate, and what they may touch.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    /// When set, edits to test files are refused (used by /fix so a patch can't weaken the test).
    pub protect_tests: bool,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        Workspace { root, protect_tests: false }
    }

    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        // Nemotron sometimes sends two paths joined by a newline as one; writing that creates a directory whose
        // name ends in a newline. Refuse rather than guess, so the model sends one path per call.
        if path.chars().any(char::is_control) {
            return Err(format!("{path:?} contains a newline or control character; send one plain path"));
        }
        let path = if path.is_empty() { "." } else { path };
        let joined = if Path::new(path).is_absolute() { PathBuf::from(path) } else { self.root.join(path) };
        let mut clean = PathBuf::new();
        for comp in joined.components() {
            match comp {
                Component::ParentDir => {
                    clean.pop();
                }
                Component::CurDir => {}
                other => clean.push(other),
            }
        }
        if !clean.starts_with(&self.root) {
            return Err(format!("{path} is outside the workspace"));
        }
        Ok(clean)
    }

    pub fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned()
    }
}

pub fn is_test_path(path: &str) -> bool {
    let p = Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = p.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
        || name == "conftest.py"
        || p.components().any(|c| matches!(c.as_os_str().to_str(), Some("test" | "tests" | "__tests__" | "testing")))
}

/// Tools that change files or run commands need the user's approval.
pub fn mutates(name: &str) -> bool {
    matches!(name, "edit_file" | "write_file" | "bash")
}

pub fn specs(read_only: bool) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec::new(
            "read_file",
            "Read text files. Each line is shown as `<line number><TAB><content>`; the number and tab are not part of \
             the file. To read several files, list them all in `paths` in ONE call: that is much faster than one call \
             per file. Use offset/limit to page through a single large file.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "One file, relative to the workspace root"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Several files to read at once (up to 10)"},
                "offset": {"type": "integer", "description": "1-based first line (default 1)"},
                "limit": {"type": "integer", "description": "Max lines (default 400)"}}}),
        ),
        ToolSpec::new(
            "list_files",
            "List files under a directory recursively, respecting .gitignore. Optional glob such as *.rs.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory (default .)"},
                "glob": {"type": "string", "description": "File name pattern"}}}),
        ),
        ToolSpec::new(
            "grep",
            "Search file contents with a regular expression. Returns path:line: text for each match.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Regular expression (Rust regex syntax)"},
                "path": {"type": "string", "description": "Directory or file (default .)"},
                "glob": {"type": "string", "description": "Only files whose name matches, e.g. *.py"}},
                "required": ["pattern"]}),
        ),
    ];
    if crate::web::enabled() {
        specs.push(ToolSpec::new(
            "web_fetch",
            "Fetch a web page (documentation, an issue, a changelog) and return its readable text.",
            json!({"type": "object", "properties": {"url": {"type": "string", "description": "http(s) URL"}}, "required": ["url"]}),
        ));
    }
    if crate::web::search_available() {
        specs.push(ToolSpec::new(
            "web_search",
            "Search the web (via Tavily) for current information: library docs, error messages, release notes, APIs. \
             Returns a short summary and the top results with URLs; follow up with web_fetch to read one.",
            json!({"type": "object", "properties": {
                "query": {"type": "string"},
                "topic": {"type": "string", "enum": ["general", "news", "finance"]},
                "max_results": {"type": "integer", "description": "1-10, default 5"}},
                "required": ["query"]}),
        ));
    }
    if !read_only {
        specs.extend([
            ToolSpec::new(
                "edit_file",
                "Replace exact snippets in files. Each old_string must match the file exactly (including \
                 indentation) and be unique unless replace_all is true. Read files first. Send one edit with path, \
                 old_string and new_string, or several with `edits`: put every change you have planned, across \
                 any files, in one call (a top-level path applies to edits that don't name one). Edits to the same file apply in order, and if any edit fails, none are \
                 applied.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to replace"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"},
                    "edits": {"type": "array", "description": "Several edits applied together", "items": {
                        "type": "object", "properties": {
                            "path": {"type": "string"},
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}},
                        "required": ["path", "old_string", "new_string"]}}}}),
            ),
            ToolSpec::new(
                "write_file",
                "Create files or overwrite them entirely. Prefer edit_file for changes to existing files. Send one \
                 file with path and content, or several with `files`: put every new file you have planned (sources, \
                 tests, data files) in one call.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "files": {"type": "array", "description": "Several files written together", "items": {
                        "type": "object", "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}},
                        "required": ["path", "content"]}}}}),
            ),
            ToolSpec::new(
                "bash",
                "Run a shell command in the workspace root (bash -c). Use it to build, run tests, and inspect git. \
                 Output is truncated if long. Avoid interactive commands.",
                json!({"type": "object", "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "description": "Default 120, max 900"}},
                    "required": ["command"]}),
            ),
        ]);
    }
    specs
}

fn s<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().unwrap_or("")
}

/// A one-line description of a call, for the transcript.
pub fn summary(name: &str, args: &Value) -> String {
    match name {
        "read_file" => format!("Read {}", read_paths(args).join(", ")),
        "list_files" => format!("List {} {}", if s(args, "path").is_empty() { "." } else { s(args, "path") }, s(args, "glob")).trim_end().to_string(),
        "grep" => format!("Search /{}/ {}", s(args, "pattern"), s(args, "glob")).trim_end().to_string(),
        "edit_file" => {
            let edits = edit_list(args);
            let mut paths: Vec<&str> = vec![];
            for e in &edits {
                if !paths.contains(&e.path.as_str()) {
                    paths.push(&e.path);
                }
            }
            let count = if edits.len() > 1 { format!(" ({} edits)", edits.len()) } else { String::new() };
            format!("Edit {}{count}", paths.join(", "))
        }
        "write_file" => {
            let files = write_list(args);
            let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            let count = if files.len() > 1 { format!(" ({} files)", files.len()) } else { String::new() };
            format!("Write {}{count}", paths.join(", "))
        }
        "bash" => format!("Run `{}`", one_line(s(args, "command"), 100)),
        "web_search" => format!("Web search \"{}\"", one_line(s(args, "query"), 80)),
        "web_fetch" => format!("Fetch {}", one_line(s(args, "url"), 100)),
        "view_image" => format!("Look at {}", s(args, "path")),
        other => other.to_string(),
    }
}

/// What a mutating call would do: a diff for edits and writes, the command for bash.
pub fn preview(ws: &Workspace, name: &str, args: &Value) -> Option<String> {
    match name {
        "edit_file" => {
            let changes = plan_edits(ws, &edit_list(args)).ok()?;
            Some(changes.iter().map(|c| diff(&ws.rel(&c.path), &c.old, &c.new)).collect())
        }
        "write_file" => {
            let mut out = String::new();
            for file in write_list(args) {
                let path = ws.resolve(&file.path).ok()?;
                let old = std::fs::read_to_string(&path).unwrap_or_default();
                out.push_str(&diff(&ws.rel(&path), &old, &file.content));
            }
            Some(out)
        }
        "bash" => Some(s(args, "command").to_string()),
        _ => None,
    }
}

pub fn diff(path: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// Run a tool. Errors are returned as text for the model to read and recover from.
pub async fn run(ws: &Workspace, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "read_file" => read_file(ws, args),
        "list_files" => list_files(ws, args),
        "grep" => grep(ws, args),
        "edit_file" => edit_file(ws, args),
        "write_file" => write_file(ws, args),
        "web_search" | "web_fetch" if !crate::web::enabled() => Err("web tools are turned off (EMBER_NO_WEB)".into()),
        "web_search" => crate::web::search(args).await,
        "web_fetch" => crate::web::fetch(args).await,
        "bash" => {
            let timeout = args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 900);
            let out = run_command(&ws.root, s(args, "command"), Duration::from_secs(timeout)).await;
            Ok(out.render())
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// The files a read_file call asks for: `paths` (batched) and/or `path`.
fn read_paths(args: &Value) -> Vec<String> {
    let mut paths: Vec<String> = args["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(String::from))
        .collect();
    if let Some(p) = args["path"].as_str().filter(|p| !p.is_empty()) {
        if !paths.iter().any(|x| x == p) {
            paths.insert(0, p.to_string());
        }
    }
    paths.truncate(10);
    paths
}

fn read_file(ws: &Workspace, args: &Value) -> Result<String, String> {
    let paths = read_paths(args);
    match paths.len() {
        0 => Err("give `path` or `paths`".into()),
        1 => read_one(ws, &paths[0], args),
        _ => {
            // Batched: each file with a header; a missing file is reported inline rather than failing the call.
            let budget = MAX_OUTPUT / paths.len();
            let mut out = String::new();
            for path in &paths {
                let body = read_one(ws, path, &json!({"limit": args["limit"]})).unwrap_or_else(|e| format!("Error: {e}\n"));
                out.push_str(&format!("==> {path} <==\n{}\n", truncate(&body, budget)));
            }
            Ok(out)
        }
    }
}

fn read_one(ws: &Workspace, rel: &str, args: &Value) -> Result<String, String> {
    let path = ws.resolve(rel)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{rel}: {e}"))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().filter(|&l| l > 0).unwrap_or(400) as usize;
    if start > lines.len().max(1) {
        return Ok(format!("(file has {} lines)", lines.len()));
    }
    let end = (start - 1 + limit).min(lines.len());
    let mut out = String::new();
    for (i, line) in lines[start - 1..end].iter().enumerate() {
        out.push_str(&format!("{:>6}\t{line}\n", start + i));
    }
    if end < lines.len() {
        out.push_str(&format!("... ({} more lines; continue with offset={})\n", lines.len() - end, end + 1));
    }
    Ok(truncate(&out, MAX_OUTPUT))
}

fn walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root).hidden(true).git_ignore(true).parents(true).build()
}

/// Project files relative to `root`, respecting .gitignore (for @-mention completion).
pub fn project_files(root: &Path, limit: usize) -> Vec<String> {
    let mut files: Vec<String> = walker(root)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().into_owned())
        .take(limit)
        .collect();
    files.sort();
    files
}

fn glob_matches(glob: &str, name: &str) -> bool {
    if glob.is_empty() {
        return true;
    }
    let pattern = format!("^{}$", regex::escape(glob).replace(r"\*", ".*").replace(r"\?", "."));
    Regex::new(&pattern).map(|re| re.is_match(name)).unwrap_or(false)
}

fn list_files(ws: &Workspace, args: &Value) -> Result<String, String> {
    let root = ws.resolve(s(args, "path"))?;
    let glob = s(args, "glob");
    let mut files: Vec<String> = walker(&root)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter(|e| glob_matches(glob, &e.file_name().to_string_lossy()))
        .map(|e| ws.rel(e.path()))
        .take(2000)
        .collect();
    files.sort();
    Ok(if files.is_empty() { "no files".into() } else { truncate(&files.join("\n"), MAX_OUTPUT) })
}

fn grep(ws: &Workspace, args: &Value) -> Result<String, String> {
    let pattern = s(args, "pattern");
    let re = Regex::new(pattern).or_else(|_| Regex::new(&regex::escape(pattern))).map_err(|e| e.to_string())?;
    let root = ws.resolve(s(args, "path"))?;
    let glob = s(args, "glob");
    let mut hits = vec![];
    'files: for entry in walker(&root).filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) || !glob_matches(glob, &entry.file_name().to_string_lossy()) {
            continue;
        }
        if entry.metadata().map(|m| m.len() > 1 << 20).unwrap_or(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!("{}:{}: {}", ws.rel(entry.path()), i + 1, one_line(line.trim(), 200)));
                if hits.len() >= 200 {
                    break 'files;
                }
            }
        }
    }
    Ok(if hits.is_empty() { "no matches".into() } else { truncate(&hits.join("\n"), MAX_OUTPUT) })
}

fn check_writable(ws: &Workspace, rel: &str) -> Result<(), String> {
    if ws.protect_tests && is_test_path(rel) {
        return Err(format!("{rel} is a test file; tests are read-only while fixing. Change the source code instead."));
    }
    Ok(())
}

/// Replace `old` with `new`, tolerating trailing-whitespace differences when there is no exact match.
/// A file that ended with a newline keeps it.
pub fn apply_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    let mut out = replace(text, old, new, replace_all)?;
    if text.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn replace(text: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string is empty; use write_file to create files".into());
    }
    let count = text.matches(old).count();
    if count == 1 || (count > 1 && replace_all) {
        return Ok(if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) });
    }
    if count > 1 {
        // Say where, so the model can pick lines that make it unique without reading the file again.
        let lines: Vec<String> = text.match_indices(old).take(8).map(|(at, _)| (text[..at].matches('\n').count() + 1).to_string()).collect();
        let more = if count > lines.len() { ", …" } else { "" };
        return Err(format!(
            "old_string matches {count} places (lines {}{more}); include more surrounding lines to make it unique, or set replace_all",
            lines.join(", ")
        ));
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let needle: Vec<&str> = old.trim_matches('\n').lines().map(str::trim_end).collect();
    if needle.is_empty() || needle.len() > lines.len() {
        return Err("old_string not found; read the file and copy the exact lines".into());
    }
    let matches: Vec<usize> = (0..lines.len().saturating_sub(needle.len() - 1))
        .filter(|&i| lines[i..i + needle.len()].iter().map(|l| l.trim_end()).eq(needle.iter().copied()))
        .collect();
    if matches.len() == 1 {
        let i = matches[0];
        let last = lines[i + needle.len() - 1];
        let mut out: String = lines[..i].concat();
        out.push_str(new.trim_end_matches('\n'));
        if last.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&lines[i + needle.len()..].concat());
        return Ok(out);
    }
    if let Some(out) = replace_reindented(&lines, &needle, new) {
        return Ok(out);
    }
    Err("old_string not found; read the file and copy the exact lines (without the line-number prefix)".into())
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Match `needle` where its lines are shifted by a consistent number of spaces (a common slip when
/// copying from numbered output), and shift `new` back by the same amounts. The first line may carry its
/// own shift, since copying usually starts after the indentation of the first line.
fn replace_reindented(lines: &[&str], needle: &[&str], new: &str) -> Option<String> {
    let shifts_at = |i: usize| -> Option<(isize, Option<isize>)> {
        let mut first = None;
        let mut rest: Option<isize> = None;
        for (k, (file_line, want)) in lines[i..i + needle.len()].iter().zip(needle).enumerate() {
            let (f, w) = (file_line.trim_end(), want.trim_end());
            if f.trim().is_empty() && w.trim().is_empty() {
                continue;
            }
            if f.trim_start() != w.trim_start() {
                return None;
            }
            let delta = indent_of(w) as isize - indent_of(f) as isize;
            if k == 0 {
                first = Some(delta);
            } else if *rest.get_or_insert(delta) != delta {
                return None;
            }
        }
        Some((first?, rest))
    };
    let found: Vec<(usize, (isize, Option<isize>))> =
        (0..=lines.len() - needle.len()).filter_map(|i| shifts_at(i).map(|d| (i, d))).collect();
    let [(i, (first, rest))] = found[..] else { return None };
    let shift = |line: &str, delta: isize| -> String {
        if delta > 0 {
            line[(delta as usize).min(indent_of(line))..].to_string()
        } else {
            format!("{}{line}", " ".repeat((-delta) as usize))
        }
    };
    let reindented: Vec<String> = new
        .trim_end_matches('\n')
        .lines()
        .enumerate()
        .map(|(k, l)| shift(l, if k == 0 { first } else { rest.unwrap_or(first) }))
        .collect();
    let last = lines[i + needle.len() - 1];
    let mut out: String = lines[..i].concat();
    out.push_str(&reindented.join("\n"));
    if last.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&lines[i + needle.len()..].concat());
    Some(out)
}

/// One replacement requested of edit_file.
pub struct Edit {
    pub path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
}

/// The edits in an edit_file call: the `edits` list, or the single path/old_string/new_string form. A top-level
/// `path` applies to every edit that doesn't name its own (Nemotron sends that shape for several edits to one file).
pub fn edit_list(args: &Value) -> Vec<Edit> {
    let one = |v: &Value| Edit {
        path: Some(s(v, "path")).filter(|p| !p.is_empty()).unwrap_or(s(args, "path")).to_string(),
        old_string: s(v, "old_string").to_string(),
        new_string: s(v, "new_string").to_string(),
        replace_all: v["replace_all"].as_bool().unwrap_or(false),
    };
    match args["edits"].as_array().filter(|e| !e.is_empty()) {
        Some(edits) => edits.iter().map(one).collect(),
        None => vec![one(args)],
    }
}

/// A file's contents before and after a set of edits.
struct Change {
    path: PathBuf,
    old: String,
    new: String,
}

/// Apply edits in memory, in order (later edits to a file see earlier ones), so nothing is written unless every
/// edit applies. Files keep the order they were first mentioned in.
fn plan_edits(ws: &Workspace, edits: &[Edit]) -> Result<Vec<Change>, String> {
    let mut changes: Vec<Change> = vec![];
    for (i, edit) in edits.iter().enumerate() {
        let which = if edits.len() > 1 { format!("Edit {} of {} ({}): ", i + 1, edits.len(), edit.path) } else { String::new() };
        let fail = |e: String| {
            let rest = if edits.len() > 1 { " No files were changed." } else { "" };
            format!("{which}{e}{rest}")
        };
        if edit.path.trim().is_empty() {
            return Err(fail("no path given for this edit".into()));
        }
        let path = ws.resolve(&edit.path).map_err(fail)?;
        check_writable(ws, &ws.rel(&path)).map_err(fail)?;
        let pos = match changes.iter().position(|c| c.path == path) {
            Some(pos) => pos,
            None => {
                let old = std::fs::read_to_string(&path).map_err(|e| fail(format!("{}: {e}", edit.path)))?;
                changes.push(Change { path, new: old.clone(), old });
                changes.len() - 1
            }
        };
        changes[pos].new = apply_edit(&changes[pos].new, &edit.old_string, &edit.new_string, edit.replace_all).map_err(fail)?;
    }
    Ok(changes)
}

fn edit_file(ws: &Workspace, args: &Value) -> Result<String, String> {
    let changes = plan_edits(ws, &edit_list(args))?;
    for c in &changes {
        std::fs::write(&c.path, &c.new).map_err(|e| format!("{}: {e}", ws.rel(&c.path)))?;
    }
    Ok(changes.iter().map(|c| diff(&ws.rel(&c.path), &c.old, &c.new)).collect())
}

/// One file requested of write_file.
pub struct FileWrite {
    pub path: String,
    content: String,
}

/// The files in a write_file call: the `files` list, or the single path/content form.
pub fn write_list(args: &Value) -> Vec<FileWrite> {
    let one = |v: &Value| FileWrite { path: s(v, "path").to_string(), content: s(v, "content").to_string() };
    match args["files"].as_array().filter(|f| !f.is_empty()) {
        Some(files) => files.iter().map(one).collect(),
        None => vec![one(args)],
    }
}

/// Write every file, or none: all paths are checked before anything is written.
fn write_file(ws: &Workspace, args: &Value) -> Result<String, String> {
    let files = write_list(args);
    let mut planned = vec![];
    for (i, file) in files.iter().enumerate() {
        let which = if files.len() > 1 { format!("File {} of {}: ", i + 1, files.len()) } else { String::new() };
        let rest = if files.len() > 1 { " No files were written." } else { "" };
        if file.path.trim().is_empty() {
            return Err(format!("{which}no path given{rest}"));
        }
        let path = ws.resolve(&file.path).map_err(|e| format!("{which}{e}{rest}"))?;
        check_writable(ws, &ws.rel(&path)).map_err(|e| format!("{which}{e}{rest}"))?;
        planned.push((path, file));
    }
    let mut out = vec![];
    for (path, file) in planned {
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, &file.content).map_err(|e| format!("{}: {e}", ws.rel(&path)))?;
        out.push(if old.is_empty() {
            format!("Created {} ({} lines)", ws.rel(&path), file.content.lines().count())
        } else {
            diff(&ws.rel(&path), &old, &file.content)
        });
    }
    Ok(out.join("\n"))
}

#[derive(Debug)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub output: String,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn render(&self) -> String {
        let status = if self.timed_out {
            "timed out".to_string()
        } else {
            format!("exit code {}", self.code.map_or("?".into(), |c| c.to_string()))
        };
        format!("{}\n[{status}]", truncate(self.output.trim_end(), MAX_OUTPUT))
    }
}

/// Run `bash -c command` in `dir`, capturing stdout and stderr together.
///
/// The command gets its own process group. On a timeout, or when the caller stops waiting (an interrupted turn), the
/// whole group is killed, so test runners, servers and other children don't outlive it. Background processes the
/// command starts and leaves running (`server &`) are left alone when it finishes normally.
pub async fn run_command(dir: &Path, command: &str, timeout: Duration) -> CommandOutput {
    let child = Command::new("bash")
        .arg("-c")
        .arg(format!("exec 2>&1; {command}"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return CommandOutput { code: None, output: format!("failed to start: {e}"), timed_out: false },
    };
    let mut group = ProcessGroup(child.id());
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = Vec::new();
    let result = tokio::time::timeout(timeout, async {
        let mut reading = std::pin::pin!(read_all(&mut stdout, &mut buf));
        tokio::select! {
            _ = &mut reading => child.wait().await,
            status = child.wait() => {
                // bash is done, but something it started in the background may still hold the output open.
                let _ = tokio::time::timeout(Duration::from_millis(200), reading).await;
                status
            }
        }
    })
    .await;
    match result {
        Ok(status) => {
            group.0 = None;
            CommandOutput {
                code: status.ok().and_then(|s| s.code()),
                output: String::from_utf8_lossy(&buf).into_owned(),
                timed_out: false,
            }
        }
        Err(_) => {
            drop(group);
            let _ = child.kill().await;
            CommandOutput { code: None, output: String::from_utf8_lossy(&buf).into_owned(), timed_out: true }
        }
    }
}

/// Read until end of file. Cancel-safe: `buf` keeps everything read before the future is dropped.
async fn read_all(stdout: &mut tokio::process::ChildStdout, buf: &mut Vec<u8>) {
    let mut chunk = [0u8; 8192];
    while let Ok(n @ 1..) = stdout.read(&mut chunk).await {
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Kills a command's process group when dropped, unless its id was taken out first.
struct ProcessGroup(Option<u32>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(id) = self.0.and_then(|id| i32::try_from(id).ok()) {
            // SAFETY: killpg only sends a signal. The group id is the child's pid, and a pid isn't handed out again
            // while it's still in use as a process group id.
            unsafe {
                libc::killpg(id, libc::SIGKILL);
            }
        }
    }
}

pub fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let head = floor_char(s, limit * 2 / 3);
    let tail = ceil_char(s, s.len() - (limit - head));
    format!("{}\n\n... ({} bytes omitted) ...\n\n{}", &s[..head], tail - head, &s[tail..])
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

pub fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let more = s.lines().nth(1).is_some();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else if more {
        format!("{line} …")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a process is gone (or a zombie waiting to be reaped).
    fn exited(pid: &str) -> bool {
        let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
        !out.status.success() || String::from_utf8_lossy(&out.stdout).trim().starts_with('Z')
    }

    #[tokio::test]
    async fn timeouts_kill_the_whole_process_group() {
        let dir = std::env::temp_dir().join(format!("ember-pgroup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run_command(&dir, "sleep 30 & echo $! > child.pid; wait", Duration::from_millis(500)).await;
        assert!(out.timed_out);
        let pid = std::fs::read_to_string(dir.join("child.pid")).unwrap();
        let start = std::time::Instant::now();
        while !exited(pid.trim()) {
            assert!(start.elapsed() < Duration::from_secs(5), "the command's child {pid} outlived the timeout");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn background_processes_dont_hold_the_command_open() {
        let start = std::time::Instant::now();
        let out = run_command(&std::env::temp_dir(), "sleep 3 & echo started", Duration::from_secs(20)).await;
        assert!(!out.timed_out);
        assert_eq!((out.code, out.output.trim()), (Some(0), "started"));
        assert!(start.elapsed() < Duration::from_secs(2), "waited {:?} for the background sleep", start.elapsed());
    }

    #[test]
    fn edit_exact_and_whitespace_tolerant() {
        assert_eq!(apply_edit("a\nb\n", "b", "c", false).unwrap(), "a\nc\n");
        assert_eq!(apply_edit("x = 1   \ny = 2\n", "x = 1\ny = 2\n", "z\n", false).unwrap(), "z\n");
        assert!(apply_edit("a\na\n", "a", "b", false).unwrap_err().contains("2 places (lines 1, 2)"));
        assert_eq!(apply_edit("a\na\n", "a", "b", true).unwrap(), "b\nb\n");
        assert!(apply_edit("a\n", "zzz", "b", false).is_err());
        assert_eq!(apply_edit("x\ny\n", "y\n", "z", false).unwrap(), "x\nz\n");
        // Every line shifted by two extra spaces, as when copied from numbered output.
        let src = "def f():\n    a = 1\n    return a\n";
        let got = apply_edit(src, "  def f():\n      a = 1\n      return a", "  def f():\n      a = 2\n      return a", false);
        assert_eq!(got.unwrap(), "def f():\n    a = 2\n    return a\n");
        // The case Nemotron produced: first line exact, following lines two spaces too deep.
        let got = apply_edit(src, "def f():\n      a = 1\n      return a", "def f(b):\n      a = b\n      return a", false);
        assert_eq!(got.unwrap(), "def f(b):\n    a = b\n    return a\n");
    }

    #[test]
    fn workspace_refuses_escapes() {
        let ws = Workspace::new(PathBuf::from("/tmp/proj"));
        assert!(ws.resolve("../etc/passwd").is_err());
        assert_eq!(ws.resolve("src/../lib.rs").unwrap(), PathBuf::from("/tmp/proj/lib.rs"));
        // Seen live: two paths joined by a newline, which created a directory named "c6.txt\n".
        assert!(ws.resolve("data/c6.txt\n/data/c6.txt").unwrap_err().contains("newline"));
        assert!(ws.resolve("a.txt\t").is_err());
    }

    #[test]
    fn detects_test_paths() {
        assert!(is_test_path("tests/test_x.py") && is_test_path("pkg/foo_test.go") && is_test_path("src/a.spec.ts"));
        assert!(!is_test_path("src/main.rs") && !is_test_path("src/attest.rs"));
    }

    #[test]
    fn batched_reads() {
        let dir = std::env::temp_dir().join(format!("ember-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
        let ws = Workspace::new(dir.clone());
        let out = read_file(&ws, &json!({"paths": ["a.txt", "b.txt", "missing.txt"]})).unwrap();
        assert!(out.contains("==> a.txt <==") && out.contains("alpha") && out.contains("beta"));
        assert!(out.contains("==> missing.txt <==\nError:"));
        assert_eq!(read_file(&ws, &json!({"path": "a.txt"})).unwrap(), "     1\talpha\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batched_edits_apply_together_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("ember-edits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reset = || {
            std::fs::write(dir.join("a.py"), "x = 1\ny = 2\n").unwrap();
            std::fs::write(dir.join("b.py"), "print(x)\n").unwrap();
        };
        let ws = Workspace::new(dir.clone());
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();

        // Several files, and two edits to one file applied in order.
        reset();
        let args = json!({"edits": [
            {"path": "a.py", "old_string": "x = 1", "new_string": "x = 10"},
            {"path": "b.py", "old_string": "print(x)", "new_string": "print(x, y)"},
            {"path": "a.py", "old_string": "x = 10\ny = 2", "new_string": "x = 10\ny = 20"}]});
        assert_eq!(summary("edit_file", &args), "Edit a.py, b.py (3 edits)");
        let out = edit_file(&ws, &args).unwrap();
        assert!(out.contains("b/a.py") && out.contains("b/b.py"));
        assert_eq!((read("a.py"), read("b.py")), ("x = 10\ny = 20\n".into(), "print(x, y)\n".into()));

        // One failing edit leaves every file untouched and says which edit failed.
        reset();
        let err = edit_file(&ws, &json!({"edits": [
            {"path": "a.py", "old_string": "x = 1", "new_string": "x = 10"},
            {"path": "b.py", "old_string": "nope", "new_string": "z"}]})).unwrap_err();
        assert!(err.starts_with("Edit 2 of 2 (b.py): ") && err.ends_with("No files were changed."), "{err}");
        assert_eq!((read("a.py"), read("b.py")), ("x = 1\ny = 2\n".into(), "print(x)\n".into()));

        // A top-level path covers edits that don't name one; an edit with no path at all is a clear error.
        reset();
        edit_file(&ws, &json!({"path": "a.py", "edits": [
            {"old_string": "x = 1", "new_string": "x = 5"},
            {"path": "b.py", "old_string": "print(x)", "new_string": "print(-x)"}]})).unwrap();
        assert_eq!((read("a.py"), read("b.py")), ("x = 5\ny = 2\n".into(), "print(-x)\n".into()));
        let err = edit_file(&ws, &json!({"edits": [{"old_string": "a", "new_string": "b"}]})).unwrap_err();
        assert!(err.contains("no path given"), "{err}");

        // The single-edit form still works.
        reset();
        assert!(edit_file(&ws, &json!({"path": "a.py", "old_string": "y = 2", "new_string": "y = 3"})).is_ok());
        assert_eq!(read("a.py"), "x = 1\ny = 3\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*.rs", "main.rs") && !glob_matches("*.rs", "main.rsx"));
    }
}
