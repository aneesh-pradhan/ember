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
            "Read a text file. Each line is shown as `<line number><TAB><content>`; the number and tab are not part of the file. Use offset/limit to page through large files.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace root"},
                "offset": {"type": "integer", "description": "1-based first line (default 1)"},
                "limit": {"type": "integer", "description": "Max lines (default 400)"}},
                "required": ["path"]}),
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
    if !read_only {
        specs.extend([
            ToolSpec::new(
                "edit_file",
                "Replace an exact snippet in a file. old_string must match the file exactly (including indentation) \
                 and be unique unless replace_all is true. Read the file first.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to replace"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"}},
                    "required": ["path", "old_string", "new_string"]}),
            ),
            ToolSpec::new(
                "write_file",
                "Create a file or overwrite it entirely. Prefer edit_file for changes to existing files.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}},
                    "required": ["path", "content"]}),
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
        "read_file" => format!("Read {}", s(args, "path")),
        "list_files" => format!("List {} {}", if s(args, "path").is_empty() { "." } else { s(args, "path") }, s(args, "glob")).trim_end().to_string(),
        "grep" => format!("Search /{}/ {}", s(args, "pattern"), s(args, "glob")).trim_end().to_string(),
        "edit_file" => format!("Edit {}", s(args, "path")),
        "write_file" => format!("Write {}", s(args, "path")),
        "bash" => format!("Run `{}`", one_line(s(args, "command"), 100)),
        other => other.to_string(),
    }
}

/// What a mutating call would do: a diff for edits and writes, the command for bash.
pub fn preview(ws: &Workspace, name: &str, args: &Value) -> Option<String> {
    match name {
        "edit_file" => {
            let path = ws.resolve(s(args, "path")).ok()?;
            let old = std::fs::read_to_string(&path).ok()?;
            let new = apply_edit(&old, s(args, "old_string"), s(args, "new_string"), args["replace_all"].as_bool().unwrap_or(false)).ok()?;
            Some(diff(&ws.rel(&path), &old, &new))
        }
        "write_file" => {
            let path = ws.resolve(s(args, "path")).ok()?;
            let old = std::fs::read_to_string(&path).unwrap_or_default();
            Some(diff(&ws.rel(&path), &old, s(args, "content")))
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
        "bash" => {
            let timeout = args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 900);
            let out = run_command(&ws.root, s(args, "command"), Duration::from_secs(timeout)).await;
            Ok(out.render())
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn read_file(ws: &Workspace, args: &Value) -> Result<String, String> {
    let path = ws.resolve(s(args, "path"))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", s(args, "path")))?;
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
        return Err(format!("old_string matches {count} places; include more surrounding lines or set replace_all"));
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

fn edit_file(ws: &Workspace, args: &Value) -> Result<String, String> {
    let rel = s(args, "path");
    let path = ws.resolve(rel)?;
    check_writable(ws, &ws.rel(&path))?;
    let old = std::fs::read_to_string(&path).map_err(|e| format!("{rel}: {e}"))?;
    let new = apply_edit(&old, s(args, "old_string"), s(args, "new_string"), args["replace_all"].as_bool().unwrap_or(false))?;
    std::fs::write(&path, &new).map_err(|e| e.to_string())?;
    Ok(diff(&ws.rel(&path), &old, &new))
}

fn write_file(ws: &Workspace, args: &Value) -> Result<String, String> {
    let rel = s(args, "path");
    let path = ws.resolve(rel)?;
    check_writable(ws, &ws.rel(&path))?;
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = s(args, "content");
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(if old.is_empty() {
        format!("Created {} ({} lines)", ws.rel(&path), content.lines().count())
    } else {
        diff(&ws.rel(&path), &old, content)
    })
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
pub async fn run_command(dir: &Path, command: &str, timeout: Duration) -> CommandOutput {
    let child = Command::new("bash")
        .arg("-c")
        .arg(format!("exec 2>&1; {command}"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return CommandOutput { code: None, output: format!("failed to start: {e}"), timed_out: false },
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = Vec::new();
    let result = tokio::time::timeout(timeout, async {
        let _ = stdout.read_to_end(&mut buf).await;
        child.wait().await
    })
    .await;
    match result {
        Ok(status) => CommandOutput {
            code: status.ok().and_then(|s| s.code()),
            output: String::from_utf8_lossy(&buf).into_owned(),
            timed_out: false,
        },
        Err(_) => {
            let _ = child.kill().await;
            CommandOutput { code: None, output: String::from_utf8_lossy(&buf).into_owned(), timed_out: true }
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

    #[test]
    fn edit_exact_and_whitespace_tolerant() {
        assert_eq!(apply_edit("a\nb\n", "b", "c", false).unwrap(), "a\nc\n");
        assert_eq!(apply_edit("x = 1   \ny = 2\n", "x = 1\ny = 2\n", "z\n", false).unwrap(), "z\n");
        assert!(apply_edit("a\na\n", "a", "b", false).unwrap_err().contains("2 places"));
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
    }

    #[test]
    fn detects_test_paths() {
        assert!(is_test_path("tests/test_x.py") && is_test_path("pkg/foo_test.go") && is_test_path("src/a.spec.ts"));
        assert!(!is_test_path("src/main.rs") && !is_test_path("src/attest.rs"));
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*.rs", "main.rs") && !glob_matches("*.rs", "main.rsx"));
    }
}
