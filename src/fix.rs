//! `/fix`: prove a bug with a failing test, then fix it in parallel, isolated branches.
//!
//! 1. Investigate the code (Nemotron Super, read-only tools).
//! 2. Write a test that must FAIL on the current code; Nemotron Nano judges that it fails for the
//!    reported reason and not because the test itself is broken.
//! 3. Plan distinct fix strategies (Nemotron Ultra).
//! 4. Each strategy gets its own git worktree and patching loop (Super). Test files are read-only there.
//! 5. A branch passes only if the repro test passes and the full suite has no new failures.
//! 6. Ultra reviews the passing candidates; the winning patch is applied to the working tree on approval.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::task::JoinSet;

use crate::agent::{ask, AgentEvent, Decision, Events};
use crate::config::{Config, Tier};
use crate::llm::{Client, FunctionCall, Message, Request, ToolCall, ToolSpec};
use crate::tools::{self, run_command, CommandOutput, Workspace};

const TEST_TIMEOUT: Duration = Duration::from_secs(900);

/// A branch hands its edit back with this tool, and the test results come back as its result. A tool result keeps
/// the branch's prompt cached on Token Factory; feedback sent as a new user message would re-bill its whole history.
const RUN_TESTS: &str = "run_tests";

fn run_tests_spec() -> ToolSpec {
    ToolSpec::new(
        RUN_TESTS,
        "Run the reproduction test and then the full test suite on your current edits. Call it when your change is \
         complete; the result says whether the fix passes or what still fails.",
        json!({"type": "object", "properties": {}}),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct Strategy {
    pub id: String,
    pub title: String,
    pub approach: String,
}

pub enum FixEvent {
    Stage { name: &'static str, status: Status, detail: String },
    /// Progress worth showing live but not keeping (e.g. "Read src/lib.rs").
    Activity(String),
    Note(String),
    Repro { attempt: usize, path: String, code: String, output: String, confirmed: bool, reason: String },
    Strategies(Vec<Strategy>),
    Branch { id: String, status: String, note: String },
    Result { winner: String, title: String, rationale: String, diff: String },
}

#[derive(Clone)]
struct Ctx {
    client: Client,
    config: Config,
    tx: Events,
}

impl Ctx {
    fn send(&self, event: FixEvent) {
        let _ = self.tx.send(AgentEvent::Fix(event));
    }

    fn stage(&self, name: &'static str, status: Status, detail: impl Into<String>) {
        self.send(FixEvent::Stage { name, status, detail: detail.into() });
    }

    async fn chat(&self, tier: Tier, request: Request) -> Result<crate::llm::Response> {
        let model = self.config.model(tier).to_string();
        let resp = self.client.stream(&Request { model: model.clone(), ..request }, |_| {}).await?;
        let _ = self.tx.send(AgentEvent::Usage {
            model,
            prompt: resp.usage.prompt_tokens,
            completion: resp.usage.completion_tokens,
            cached: resp.usage.cached_tokens,
        });
        Ok(resp)
    }

    /// Force a single structured tool call and return its arguments.
    /// Get one structured answer by forcing a tool call. If the endpoint rejects a forced tool_choice,
    /// or the model answers in prose, fall back to asking for the arguments as plain JSON.
    async fn structured(&self, tier: Tier, messages: Vec<Message>, spec: ToolSpec) -> Result<Value> {
        let name = spec.function.name.clone();
        let forced = Request {
            messages: messages.clone(),
            tools: vec![spec.clone()],
            tool_choice: Some(json!({"type": "function", "function": {"name": name}})),
            max_tokens: Some(8192),
            ..Default::default()
        };
        if let Ok(resp) = self.chat(tier, forced).await {
            if let Some(call) = resp.tool_calls.into_iter().find(|c| c.function.name == name) {
                if let Ok(v) = serde_json::from_str(&call.function.arguments) {
                    return Ok(v);
                }
            }
            if let Some(v) = extract_json(&resp.content) {
                return Ok(v);
            }
        }
        let mut messages = messages;
        messages.push(Message::user(format!(
            "Reply with only a JSON object (no prose, no code fence) matching this schema for `{name}`:\n{}",
            spec.function.parameters
        )));
        let resp = self.chat(tier, Request { messages, max_tokens: Some(8192), ..Default::default() }).await?;
        extract_json(&resp.content).with_context(|| format!("the model did not return {name} as JSON"))
    }
}

fn extract_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(&text[start..=end]).ok()
}

/// Worktrees created for this run; removed again when dropped, whatever happens.
struct Worktrees {
    top: PathBuf,
    dir: PathBuf,
    paths: Vec<PathBuf>,
}

impl Worktrees {
    fn new(top: &Path) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("ember-fix-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Worktrees { top: top.to_path_buf(), dir, paths: vec![] })
    }

    fn add(&mut self, name: &str, commit: &str) -> Result<PathBuf> {
        let path = self.dir.join(name);
        git(&self.top, &["worktree", "add", "--detach", "--quiet", path.to_str().unwrap(), commit])?;
        self.paths.push(path.clone());
        Ok(path)
    }
}

impl Drop for Worktrees {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = git(&self.top, &["worktree", "remove", "--force", path.to_str().unwrap_or_default()]);
        }
        let _ = git(&self.top, &["worktree", "prune"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git").args(args).current_dir(dir).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn detect_test_command(dir: &Path) -> Option<String> {
    let has = |name: &str| dir.join(name).exists();
    if has("Cargo.toml") {
        Some("cargo test --quiet".into())
    } else if has("go.mod") {
        Some("go test ./...".into())
    } else if has("tests/runtests.py") {
        // Django (and projects copying its layout) run tests through their own script; pytest can't load them.
        Some("python tests/runtests.py".into())
    } else if has("bin/test") && has("setup.py") {
        // SymPy's runner.
        Some("python bin/test".into())
    } else if ["pyproject.toml", "setup.py", "setup.cfg", "pytest.ini", "tox.ini"].iter().any(|f| has(f)) {
        Some("python -m pytest -q -p no:cacheprovider".into())
    } else if has("package.json") {
        Some("npm test --silent".into())
    } else {
        None
    }
}

/// Environment tweaks so tests in a worktree use the worktree's code, not an installed copy.
fn test_env_prefix(top: &Path, project: &Path) -> String {
    let mut prefix = String::new();
    if project.join("Cargo.toml").exists() {
        // Share one target dir between worktrees so each branch doesn't rebuild from scratch.
        prefix.push_str(&format!("CARGO_TARGET_DIR='{}' ", top.join("target").join("ember-fix").display()));
    }
    if detect_test_command(project).is_some_and(|c| c.contains("pytest")) {
        prefix.push_str("PYTHONPATH=\"$PWD/src:$PWD${PYTHONPATH:+:$PYTHONPATH}\" PYTHONDONTWRITEBYTECODE=1 ");
    }
    prefix
}

async fn fetch_issue(input: &str) -> Result<String> {
    let re = regex::Regex::new(r"github\.com/([^/\s]+)/([^/\s]+)/issues/(\d+)").unwrap();
    let Some(c) = re.captures(input) else { return Ok(input.to_string()) };
    let url = format!("https://api.github.com/repos/{}/{}/issues/{}", &c[1], &c[2], &c[3]);
    let mut req = reqwest::Client::new().get(&url).header("User-Agent", "ember").header("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.bearer_auth(token);
    }
    let issue: Value = req.send().await?.error_for_status()?.json().await?;
    Ok(format!("{}\n\n{}", issue["title"].as_str().unwrap_or(""), issue["body"].as_str().unwrap_or("")))
}

pub async fn run(client: Client, config: Config, cwd: PathBuf, input: String, auto: bool, tx: Events) -> Result<()> {
    let ctx = Ctx { client, config, tx };
    let top = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"]).context("/fix needs a git repository")?);
    let sub = cwd.strip_prefix(&top).unwrap_or(Path::new("")).to_path_buf();
    let test_cmd = ctx
        .config
        .test_cmd
        .clone()
        .or_else(|| detect_test_command(&cwd))
        .ok_or_else(|| anyhow!("couldn't detect a test command here; set EMBER_TEST_CMD"))?;
    // `git stash create` snapshots uncommitted tracked changes without touching the working tree.
    let base = match git(&top, &["stash", "create"])? {
        s if s.is_empty() => git(&top, &["rev-parse", "HEAD"])?,
        s => s,
    };

    if !auto {
        let summary = format!("Let /fix run `{test_cmd}` and model-written tests in temporary git worktrees");
        if ask(&ctx.tx, summary, None, None).await == Decision::No {
            bail!("cancelled");
        }
    }

    ctx.stage("Investigate", Status::Running, "");
    let issue = fetch_issue(&input).await?;
    let mut trees = Worktrees::new(&top)?;
    let in_tree = |root: &Path| root.join(&sub);
    let prefix = test_env_prefix(&top, &cwd);

    let baseline_dir = in_tree(&trees.add("baseline", &base)?);
    let baseline_cmd = format!("{prefix}{test_cmd}");
    let baseline = tokio::spawn(async move { run_command(&baseline_dir, &baseline_cmd, TEST_TIMEOUT).await });

    let report = investigate(&ctx, &cwd, &issue).await?;
    ctx.stage("Investigate", Status::Done, report["root_cause"].as_str().unwrap_or("").to_string());

    ctx.stage("Reproduce", Status::Running, "");
    let repro_root = in_tree(&trees.add("repro", &base)?);
    let repro = reproduce(&ctx, &repro_root, &issue, &report, &prefix, &test_cmd).await?;
    ctx.stage("Reproduce", Status::Done, format!("{} fails on the current code", repro.path));

    let baseline = baseline.await?;
    if !baseline.success() {
        ctx.send(FixEvent::Note("The test suite already fails before any change; branches are checked for new failures only.".into()));
    }

    ctx.stage("Plan", Status::Running, "");
    let strategies = plan(&ctx, &issue, &report, &repro).await?;
    ctx.stage("Plan", Status::Done, format!("{} strategies", strategies.len()));
    ctx.send(FixEvent::Strategies(strategies.clone()));

    ctx.stage("Fix", Status::Running, format!("{} branches in parallel", strategies.len()));
    let solved = Arc::new(AtomicBool::new(false));
    let mut set = JoinSet::new();
    for strategy in strategies {
        let root = in_tree(&trees.add(&strategy.id.to_lowercase(), &base)?);
        let branch = Branch {
            ctx: ctx.clone(),
            strategy,
            root,
            git_root: trees.paths.last().unwrap().clone(),
            base: base.clone(),
            issue: issue.clone(),
            report: report.clone(),
            repro: repro.clone(),
            prefix: prefix.clone(),
            suite: format!("{prefix}{test_cmd}"),
            baseline_ok: baseline.success(),
            baseline_output: tail(&baseline.output, 3000),
            solved: solved.clone(),
        };
        set.spawn(branch.run());
    }
    let mut passed = vec![];
    while let Some(result) = set.join_next().await {
        if let Ok(Some(candidate)) = result {
            passed.push(candidate);
        }
    }
    if passed.is_empty() {
        ctx.stage("Fix", Status::Failed, "no branch passed");
        bail!("the bug is reproduced, but no candidate fix passed verification");
    }
    ctx.stage("Fix", Status::Done, format!("{} passing candidate(s)", passed.len()));

    ctx.stage("Review", Status::Running, "");
    let (winner, title, rationale) = review(&ctx, &issue, &passed).await?;
    let chosen = passed.iter().find(|c| c.id == winner).unwrap_or(&passed[0]);
    ctx.stage("Review", Status::Done, format!("selected {}", chosen.id));
    ctx.send(FixEvent::Result {
        winner: chosen.id.clone(),
        title: title.clone(),
        rationale,
        diff: chosen.diff.clone(),
    });

    if ask(&ctx.tx, "Apply this patch to your working tree".into(), None, None).await != Decision::No {
        let patch_file = trees.dir.join("winner.patch");
        std::fs::write(&patch_file, &chosen.diff)?;
        git(&top, &["apply", "--whitespace=nowarn", patch_file.to_str().unwrap()])?;
        let _ = ctx.tx.send(AgentEvent::Notice(format!("Applied: {title}")));
    } else {
        let _ = ctx.tx.send(AgentEvent::Notice("Patch not applied.".into()));
    }
    Ok(())
}

async fn investigate(ctx: &Ctx, root: &Path, issue: &str) -> Result<Value> {
    let ws = Workspace::new(root.to_path_buf());
    let report = ToolSpec::new(
        "report",
        "Report what you found once you can explain the bug and how to trigger it.",
        json!({"type": "object", "properties": {
            "root_cause": {"type": "string", "description": "What goes wrong and where, precisely"},
            "locations": {"type": "array", "items": {"type": "string"}, "description": "file paths (and symbols) involved"},
            "how_to_trigger": {"type": "string", "description": "Public API call and inputs that expose the bug"},
            "test_conventions": {"type": "string", "description": "Where tests live, how they import the code, how to run one test"}},
            "required": ["root_cause", "locations", "how_to_trigger", "test_conventions"]}),
    );
    let mut specs = tools::specs(true);
    specs.push(report.clone());
    let mut messages = vec![
        Message::system(
            "You are investigating a bug report before anyone writes a fix. Use the tools to find the code \
             responsible, understand how to trigger the bug from a test, and learn the project's test conventions. \
             Then call `report`. Be efficient: at most about 12 tool calls.",
        ),
        Message::user(format!("BUG REPORT\n{issue}")),
    ];
    for step in 0..16 {
        let last = step == 15;
        let resp = ctx
            .chat(Tier::Coder, Request {
                messages: messages.clone(),
                tools: specs.clone(),
                tool_choice: last.then(|| json!({"type": "function", "function": {"name": "report"}})),
                max_tokens: Some(8192),
                ..Default::default()
            })
            .await?;
        messages.push(Message::assistant(resp.content.clone(), resp.tool_calls.clone()));
        if resp.tool_calls.is_empty() {
            messages.push(Message::user("Continue investigating with the tools, or call `report`."));
            continue;
        }
        for call in &resp.tool_calls {
            let args = call.args();
            if call.function.name == "report" {
                return Ok(args);
            }
            ctx.send(FixEvent::Activity(tools::summary(&call.function.name, &args)));
            let out = tools::run(&ws, &call.function.name, &args).await.unwrap_or_else(|e| format!("Error: {e}"));
            messages.push(Message::tool(call.id.clone(), out));
        }
    }
    bail!("investigation did not converge")
}

#[derive(Clone)]
struct Repro {
    path: String,
    code: String,
    command: String,
    failure: String,
}

async fn reproduce(ctx: &Ctx, root: &Path, issue: &str, report: &Value, prefix: &str, test_cmd: &str) -> Result<Repro> {
    let submit = ToolSpec::new(
        "submit_test",
        "Submit a NEW test file that reproduces the bug, and the shell command that runs only that test.",
        json!({"type": "object", "properties": {
            "path": {"type": "string", "description": "New file path, relative to the project directory, following test conventions"},
            "content": {"type": "string", "description": "Complete file content"},
            "command": {"type": "string", "description": "Shell command, run from the project directory, that runs just this test"}},
            "required": ["path", "content", "command"]}),
    );
    let mut specs = tools::specs(true);
    specs.push(submit);
    let mut messages = vec![
        Message::system(
            "You write a test that REPRODUCES a reported bug. It must FAIL on the current code because of the bug, \
             and PASS once the bug is fixed. Assert the correct (expected) behavior, not the buggy one.\n\
             First use the read-only tools to read the code under test and at least one existing test, so you call \
             the real API with the right names and signatures and import it the way existing tests do. Never guess \
             method names. Then call `submit_test`. Keep the test small and deterministic, do not mock the code under \
             test, and use a NEW file: never overwrite an existing one. Each submission is run and checked; you will \
             get the result back.",
        ),
        Message::user(format!(
            "BUG REPORT\n{issue}\n\nINVESTIGATION\n{}\n\nThe project's full test command is `{test_cmd}` (run from the \
             project directory). Base your single-test command on it.",
            serde_json::to_string_pretty(report).unwrap_or_default()
        )),
    ];
    let ws = Workspace::new(root.to_path_buf());
    let mut attempt = 0;
    for _ in 0..30 {
        let resp = ctx
            .chat(Tier::Coder, Request {
                messages: messages.clone(),
                tools: specs.clone(),
                max_tokens: Some(8192),
                ..Default::default()
            })
            .await?;
        messages.push(Message::assistant(resp.content.clone(), resp.tool_calls.clone()));
        if resp.tool_calls.is_empty() {
            messages.push(Message::user("Use the tools to check the code, then call `submit_test`."));
            continue;
        }
        for call in &resp.tool_calls {
            let args = call.args();
            if call.function.name != "submit_test" {
                ctx.send(FixEvent::Activity(tools::summary(&call.function.name, &args)));
                let out = tools::run(&ws, &call.function.name, &args).await.unwrap_or_else(|e| format!("Error: {e}"));
                messages.push(Message::tool(call.id.clone(), out));
                continue;
            }
            let path = args["path"].as_str().unwrap_or("").trim().to_string();
            let mut code = args["content"].as_str().unwrap_or("").to_string();
            if !code.is_empty() && !code.ends_with('\n') {
                code.push('\n');
            }
            let command = args["command"].as_str().unwrap_or("").to_string();
            let target = match ws.resolve(&path) {
                Ok(t) if !path.is_empty() && !code.is_empty() && !t.exists() => t,
                _ => {
                    messages.push(Message::tool(call.id.clone(), format!("`{path}` is not a valid NEW file path (or content is empty).")));
                    continue;
                }
            };
            attempt += 1;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &code)?;
            ctx.send(FixEvent::Activity(format!("running {command}")));
            let out = run_command(root, &format!("{prefix}{command}"), TEST_TIMEOUT).await;
            let (confirmed, reason) = judge(ctx, issue, &code, &out).await?;
            ctx.send(FixEvent::Repro { attempt, path: path.clone(), code: code.clone(), output: tail(&out.output, 2500), confirmed, reason: reason.clone() });
            if confirmed {
                return Ok(Repro { path, code, command, failure: tail(&out.output, 3000) });
            }
            std::fs::remove_file(&target)?;
            if attempt >= 4 {
                break;
            }
            messages.push(Message::tool(call.id.clone(), format!(
                "Result: {}\n{}\n\nRejected: {reason}\nFix the test (check the real API with the tools if needed) and submit again.",
                if out.timed_out { "timed out".to_string() } else { format!("exit code {:?}", out.code) },
                tail(&out.output, 3000)
            )));
        }
        if attempt >= 4 {
            break;
        }
    }
    ctx.stage("Reproduce", Status::Failed, "could not reproduce");
    bail!("couldn't write a test that reproduces this bug, so no fix was attempted (ember won't ship a patch it can't verify)")
}

async fn judge(ctx: &Ctx, issue: &str, code: &str, out: &CommandOutput) -> Result<(bool, String)> {
    if out.timed_out {
        return Ok((false, "the test timed out".into()));
    }
    if out.success() {
        return Ok((false, "the test PASSED on the current code, so it does not demonstrate the bug".into()));
    }
    let verdict = ToolSpec::new(
        "verdict",
        "Decide whether the failing test genuinely reproduces the bug.",
        json!({"type": "object", "properties": {
            "reproduces": {"type": "boolean"},
            "reason": {"type": "string", "description": "One or two sentences"}},
            "required": ["reproduces", "reason"]}),
    );
    let v = ctx
        .structured(Tier::Fast, vec![
            Message::system(
                "You check whether a failing test genuinely reproduces a bug report. It is genuine only if it fails \
                 BECAUSE of the reported misbehavior (wrong value, the reported error...). It is NOT genuine if it \
                 fails because the test itself is broken: compile/import errors, wrong names, missing fixtures, syntax \
                 errors, or asserting something unrelated to the report.",
            ),
            Message::user(format!("BUG REPORT\n{issue}\n\nTEST\n```\n{code}\n```\n\nOUTPUT\n{}", tail(&out.output, 4000))),
        ], verdict)
        .await?;
    Ok((v["reproduces"].as_bool().unwrap_or(false), v["reason"].as_str().unwrap_or("").to_string()))
}

async fn plan(ctx: &Ctx, issue: &str, report: &Value, repro: &Repro) -> Result<Vec<Strategy>> {
    let n = ctx.config.fix_branches;
    let spec = ToolSpec::new(
        "propose_strategies",
        "Propose distinct fix strategies.",
        json!({"type": "object", "properties": {"strategies": {"type": "array", "items": {"type": "object", "properties": {
            "title": {"type": "string"},
            "approach": {"type": "string", "description": "2-4 sentences: what to change and why"}},
            "required": ["title", "approach"]}}}, "required": ["strategies"]}),
    );
    let v = ctx
        .structured(Tier::Reasoner, vec![
            Message::system(format!(
                "You are a senior engineer. A bug has a failing reproduction test. Propose {n} genuinely DIFFERENT fix \
                 strategies (different locations, mechanisms or scope, not rewordings of one idea). Prefer fixing the \
                 root cause over special-casing the test's inputs, and avoid changing global or process-wide state."
            )),
            Message::user(format!(
                "BUG REPORT\n{issue}\n\nINVESTIGATION\n{}\n\nFAILING TEST ({})\n```\n{}\n```\n\nFAILURE\n{}",
                serde_json::to_string_pretty(report).unwrap_or_default(),
                repro.path,
                repro.code,
                repro.failure
            )),
        ], spec)
        .await?;
    let strategies: Vec<Strategy> = v["strategies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s["title"].as_str()?.to_string(), s["approach"].as_str()?.to_string())))
        .take(n)
        .enumerate()
        .map(|(i, (title, approach))| Strategy { id: format!("B{}", i + 1), title, approach })
        .collect();
    if strategies.is_empty() {
        bail!("the planner returned no strategies");
    }
    Ok(strategies)
}

struct Candidate {
    id: String,
    title: String,
    diff: String,
}

struct Branch {
    ctx: Ctx,
    strategy: Strategy,
    root: PathBuf,
    git_root: PathBuf,
    base: String,
    issue: String,
    report: Value,
    repro: Repro,
    prefix: String,
    suite: String,
    baseline_ok: bool,
    baseline_output: String,
    solved: Arc<AtomicBool>,
}

impl Branch {
    fn status(&self, status: &str, note: impl Into<String>) {
        self.ctx.send(FixEvent::Branch { id: self.strategy.id.clone(), status: status.into(), note: note.into() });
    }

    async fn run(self) -> Option<Candidate> {
        match self.attempt().await {
            Ok(result) => result,
            Err(err) => {
                self.status("error", err.to_string());
                None
            }
        }
    }

    async fn attempt(&self) -> Result<Option<Candidate>> {
        let test_path = self.root.join(&self.repro.path);
        if let Some(parent) = test_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&test_path, &self.repro.code)?;
        let ws = Workspace { root: self.root.clone(), protect_tests: true };
        let mut specs = tools::specs(false).into_iter().filter(|s| s.function.name != "bash").collect::<Vec<_>>();
        specs.push(run_tests_spec());
        let mut messages = vec![
            Message::system(
                "You fix a bug by editing source code with the tools. A reproduction test fails because of the bug; \
                 your change must make it pass without breaking other tests. Test files are read-only. Follow the \
                 strategy you are given, keep the change minimal and in the codebase's style. Read files before \
                 editing them. When your edit is complete, call run_tests; if something still fails, keep \
                 working from its result.",
            ),
            Message::user(format!(
                "BUG REPORT\n{}\n\nROOT CAUSE\n{}\n\nSTRATEGY: {}\n{}\n\nREPRODUCTION TEST ({}), run with `{}`\n```\n{}\n```\n\nCURRENT FAILURE\n{}",
                self.issue,
                self.report["root_cause"].as_str().unwrap_or(""),
                self.strategy.title,
                self.strategy.approach,
                self.repro.path,
                self.repro.command,
                self.repro.code,
                self.repro.failure
            )),
        ];
        self.status("running", self.strategy.title.clone());
        for round in 1..=3 {
            if self.solved.load(Ordering::Relaxed) {
                self.status("stopped", "another branch already passed");
                return Ok(None);
            }
            let pending = self.patch(&ws, &specs, &mut messages, round).await?;
            // Results answer the branch's run_tests call (a plain user message only if it ran out of steps).
            let feedback = |text: String| match &pending {
                Some(id) => Message::tool(id.clone(), text),
                None => Message::user(text),
            };
            self.status("testing", format!("round {round}: running reproduction test"));
            let repro = run_command(&self.root, &format!("{}{}", self.prefix, self.repro.command), TEST_TIMEOUT).await;
            if !repro.success() {
                self.status("failing", format!("round {round}: reproduction test still fails"));
                messages.push(feedback(format!(
                    "The reproduction test still fails:\n{}\nKeep working on the fix.",
                    tail(&repro.output, 3000)
                )));
                continue;
            }
            self.status("testing", format!("round {round}: repro passes; running full suite"));
            let suite = run_command(&self.root, &self.suite, TEST_TIMEOUT).await;
            if let Some(problem) = self.regression(&suite).await? {
                self.status("regressed", format!("round {round}: {problem}"));
                messages.push(feedback(format!(
                    "The reproduction test passes, but the full suite shows a regression: {problem}\n{}\nAdjust the fix.",
                    tail(&suite.output, 3000)
                )));
                continue;
            }
            self.solved.store(true, Ordering::Relaxed);
            git(&self.git_root, &["add", "-A"])?;
            let diff = git(&self.git_root, &["diff", "--cached", &self.base])? + "\n";
            self.status("passed", "repro passes, no regressions");
            return Ok(Some(Candidate { id: self.strategy.id.clone(), title: self.strategy.title.clone(), diff }));
        }
        self.status("gave_up", "no passing patch after 3 rounds");
        Ok(None)
    }

    /// Let the model edit until it calls run_tests, and return that call's id for the results. A reply without
    /// tool calls counts as run_tests too: the call is added to it, which is safe because no request has included
    /// that reply yet. None if the model used up its steps.
    async fn patch(&self, ws: &Workspace, specs: &[ToolSpec], messages: &mut Vec<Message>, round: usize) -> Result<Option<String>> {
        for _ in 0..25 {
            let resp = self
                .ctx
                .chat(Tier::Coder, Request {
                    messages: messages.clone(),
                    tools: specs.to_vec(),
                    max_tokens: Some(8192),
                    ..Default::default()
                })
                .await?;
            let mut reply = Message::assistant(resp.content.clone(), resp.tool_calls.clone());
            if resp.tool_calls.is_empty() {
                let id = format!("run_tests_{}", messages.len());
                reply.tool_calls.push(ToolCall {
                    id: id.clone(),
                    kind: "function".into(),
                    function: FunctionCall { name: RUN_TESTS.into(), arguments: "{}".into() },
                });
                messages.push(reply);
                return Ok(Some(id));
            }
            messages.push(reply);
            let mut run_tests = None;
            for call in &resp.tool_calls {
                if call.function.name == RUN_TESTS {
                    if run_tests.is_some() {
                        messages.push(Message::tool(call.id.clone(), "(Duplicate call: the tests run once for this reply.)"));
                    } else {
                        run_tests = Some(call.id.clone());
                    }
                    continue;
                }
                let args = call.args();
                self.status("running", format!("round {round}: {}", tools::summary(&call.function.name, &args)));
                let out = tools::run(ws, &call.function.name, &args).await.unwrap_or_else(|e| format!("Error: {e}"));
                messages.push(Message::tool(call.id.clone(), out));
            }
            if run_tests.is_some() {
                return Ok(run_tests);
            }
        }
        Ok(None)
    }

    /// None when the full suite shows no new failures compared with the baseline.
    async fn regression(&self, suite: &CommandOutput) -> Result<Option<String>> {
        if suite.timed_out {
            return Ok(Some("the full suite timed out".into()));
        }
        if suite.success() {
            return Ok(None);
        }
        if self.baseline_ok {
            return Ok(Some("tests that passed before now fail".into()));
        }
        let spec = ToolSpec::new(
            "compare",
            "Compare test results before and after a patch.",
            json!({"type": "object", "properties": {
                "new_failures": {"type": "boolean", "description": "true if anything fails now that passed before"},
                "explanation": {"type": "string"}}, "required": ["new_failures", "explanation"]}),
        );
        let v = self
            .ctx
            .structured(Tier::Fast, vec![
                Message::system("Compare two test-suite outputs and decide whether the second has failures the first did not."),
                Message::user(format!("BEFORE\n{}\n\nAFTER\n{}", self.baseline_output, tail(&suite.output, 3000))),
            ], spec)
            .await?;
        Ok(v["new_failures"].as_bool().unwrap_or(true).then(|| v["explanation"].as_str().unwrap_or("new failures").to_string()))
    }
}

async fn review(ctx: &Ctx, issue: &str, passed: &[Candidate]) -> Result<(String, String, String)> {
    let spec = ToolSpec::new(
        "choose",
        "Choose the patch to merge.",
        json!({"type": "object", "properties": {
            "winner": {"type": "string", "description": "Candidate id, e.g. B2"},
            "title": {"type": "string", "description": "Commit title for the chosen patch"},
            "rationale": {"type": "string", "description": "Why this one (2-3 sentences)"}},
            "required": ["winner", "title", "rationale"]}),
    );
    let candidates: String = passed
        .iter()
        .map(|c| format!("## {}: {}\n```diff\n{}\n```\n", c.id, c.title, tools::truncate(&c.diff, 12_000)))
        .collect();
    let v = ctx
        .structured(Tier::Reasoner, vec![
            Message::system(
                "Every candidate patch below makes the reproduction test pass with no new test failures. Choose the one \
                 a maintainer should merge: correct at the root cause, minimal, idiomatic, least risky for other callers. \
                 Strongly prefer local changes over ones that modify global or process-wide state (module-level \
                 configuration, global contexts, monkey-patching), and over ones that change behavior beyond the bug.",
            ),
            Message::user(format!("BUG REPORT\n{issue}\n\nCANDIDATES\n{candidates}")),
        ], spec)
        .await?;
    Ok((
        v["winner"].as_str().unwrap_or("").trim().to_string(),
        v["title"].as_str().unwrap_or("Fix bug").to_string(),
        v["rationale"].as_str().unwrap_or("").to_string(),
    ))
}

pub fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("...\n{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_project_test_runners() {
        let dir = std::env::temp_dir().join(format!("ember-detect-{}", std::process::id()));
        let detect = |files: &[&str]| {
            let _ = std::fs::remove_dir_all(&dir);
            for f in files {
                let path = dir.join(f);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "").unwrap();
            }
            detect_test_command(&dir)
        };
        assert_eq!(detect(&["setup.py", "tests/runtests.py"]).as_deref(), Some("python tests/runtests.py"));
        assert_eq!(detect(&["setup.py", "bin/test"]).as_deref(), Some("python bin/test"));
        assert_eq!(detect(&["pyproject.toml"]).as_deref(), Some("python -m pytest -q -p no:cacheprovider"));
        assert_eq!(detect(&["Cargo.toml", "tests/runtests.py"]).as_deref(), Some("cargo test --quiet"));
        assert_eq!(detect(&["README.md"]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
