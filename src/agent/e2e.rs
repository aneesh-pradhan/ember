//! End-to-end tests of the agent loop against a scripted model (see `crate::mock`): real turns, real tools on a
//! temporary project, and checks on exactly what ember sent to the model.

use super::*;
use serde_json::Value;

use crate::mock::{call, MockModel, Reply};

/// A fresh project directory with the given files.
fn project(files: &[(&str, &str)]) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ember-e2e-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    dir
}

fn agent(model: &MockModel, dir: &Path, mode: Mode) -> Agent {
    let client = Client::new(&model.url, Some("test-key".into()));
    Agent::new(client, Arc::new(Mutex::new("mock-model".into())), SharedMode::new(mode), Workspace::new(dir.to_path_buf()))
}

/// Run one turn, answering every permission prompt with `decision`. Returns the events, with each prompt
/// recorded as `Notice("approval: <summary>")`.
async fn turn(agent: &mut Agent, input: &str, decision: Decision) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handler = tokio::spawn(async move {
        let mut events = vec![];
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Approval(req) => {
                    events.push(AgentEvent::Notice(format!("approval: {}", req.summary)));
                    let _ = req.reply.send(decision);
                }
                other => events.push(other),
            }
        }
        events
    });
    agent.turn(input.into(), &tx).await.expect("turn failed");
    drop(tx);
    handler.await.unwrap()
}

fn messages(request: &Value) -> &Vec<Value> {
    request["messages"].as_array().expect("messages")
}

/// The tool results the model saw in a request (the trailing tool messages).
fn tool_results(request: &Value) -> Vec<String> {
    let msgs = messages(request);
    let start = msgs.iter().rposition(|m| m["role"] != "tool").map_or(0, |i| i + 1);
    msgs[start..].iter().map(|m| m["content"].as_str().unwrap_or("").to_string()).collect()
}

fn replies(events: &[AgentEvent]) -> Vec<String> {
    events.iter().filter_map(|e| match e {
        AgentEvent::Reply { content, .. } if !content.is_empty() => Some(content.clone()),
        _ => None,
    }).collect()
}

#[tokio::test]
async fn runs_tools_and_sends_results_back() {
    let dir = project(&[("notes.txt", "the answer is 42\n")]);
    let model = MockModel::start(vec![Reply::Tools(vec![call("read_file", json!({"path": "notes.txt"}))]), Reply::Text("It says 42.")]).await;
    let mut a = agent(&model, &dir, Mode::Ask);
    let events = turn(&mut a, "What do my notes say?", Decision::No).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(tool_results(&requests[1])[0].contains("the answer is 42"));
    assert_eq!(replies(&events), ["It says 42."]);
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.starts_with("approval"))), "reads need no approval");
}

/// Token Factory only credits cached tokens when everything up to the last user message matches an earlier
/// request, so within a session every request must extend the previous one: same tools, and the earlier
/// messages unchanged. Follow-ups go through ask_user rather than a new user message.
#[tokio::test]
async fn every_request_extends_the_previous_one() {
    let dir = project(&[("a.txt", "alpha\n")]);
    let model = MockModel::start(vec![
        Reply::Tools(vec![call("read_file", json!({"path": "a.txt"}))]),
        Reply::Text("First answer."),
        Reply::Tools(vec![call("grep", json!({"pattern": "alpha"}))]),
        Reply::Text("Second answer."),
    ])
    .await;
    let mut a = agent(&model, &dir, Mode::Ask).interactive();
    turn(&mut a, "first question", Decision::No).await;
    turn(&mut a, "follow-up", Decision::No).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    for pair in requests.windows(2) {
        let (prev, next) = (messages(&pair[0]), messages(&pair[1]));
        assert_eq!(pair[0]["tools"], pair[1]["tools"], "the tool list changed mid-session");
        assert_eq!(&next[..prev.len()], &prev[..], "a request rewrote earlier messages");
    }
    let last = messages(&requests[3]);
    assert_eq!(last.iter().filter(|m| m["role"] == "user").count(), 1, "the follow-up became a new user message");
    assert!(last.iter().any(|m| m["role"] == "tool" && m["content"] == "follow-up"));
}

#[tokio::test]
async fn continues_a_reply_cut_off_at_the_token_limit() {
    let dir = project(&[]);
    let model = MockModel::start(vec![Reply::CutOff("The first half of a long answ"), Reply::Text("er, finished.")]).await;
    let mut a = agent(&model, &dir, Mode::Ask);
    turn(&mut a, "explain", Decision::No).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(messages(&requests[1]).last().unwrap()["content"], CUT_OFF_NOTE);
}

#[tokio::test]
async fn explains_unknown_tools_and_broken_arguments() {
    let dir = project(&[("a.txt", "alpha\n")]);
    let model = MockModel::start(vec![
        Reply::Tools(vec![("read_files", "{}".into()), ("read_file", r#"{"path": "a.t"#.into())]),
        Reply::Text("ok"),
    ])
    .await;
    let mut a = agent(&model, &dir, Mode::Ask);
    turn(&mut a, "read it", Decision::No).await;

    let results = tool_results(&model.requests()[1]);
    assert!(results[0].contains("no tool named `read_files`") && results[0].contains("read_file"), "{results:?}");
    assert!(results[1].contains("valid JSON object"), "{results:?}");
}

#[tokio::test]
async fn edits_wait_for_approval() {
    let dir = project(&[]);
    let write = || Reply::Tools(vec![call("write_file", json!({"path": "new.txt", "content": "hello\n"}))]);
    let model = MockModel::start(vec![write(), Reply::Text("ok"), write(), Reply::Text("done")]).await;

    let mut declined = agent(&model, &dir, Mode::Ask);
    let events = turn(&mut declined, "create new.txt", Decision::No).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.starts_with("approval"))));
    assert!(!dir.join("new.txt").exists(), "a declined write still happened");

    let mut approved = agent(&model, &dir, Mode::Ask);
    turn(&mut approved, "create new.txt", Decision::Yes).await;
    assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "hello\n");
}

/// One edit_file call with several edits: one approval, every file changed, and /undo restores them all.
#[tokio::test]
async fn batched_edits_need_one_approval_and_undo_together() {
    let dir = project(&[("a.py", "x = 1\n"), ("b.py", "print(x)\n")]);
    let edits = json!({"edits": [
        {"path": "a.py", "old_string": "x = 1", "new_string": "x = 2"},
        {"path": "b.py", "old_string": "print(x)", "new_string": "print(x * 2)"}]});
    let model = MockModel::start(vec![Reply::Tools(vec![call("edit_file", edits)]), Reply::Text("Done.")]).await;
    let mut a = agent(&model, &dir, Mode::Ask);
    let events = turn(&mut a, "double it", Decision::Yes).await;

    let prompts: Vec<_> = events.iter().filter_map(|e| match e {
        AgentEvent::Notice(n) if n.starts_with("approval") => Some(n.clone()),
        _ => None,
    }).collect();
    assert_eq!(prompts, ["approval: Edit a.py, b.py (2 edits)"]);
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    assert_eq!((read("a.py"), read("b.py")), ("x = 2\n".into(), "print(x * 2)\n".into()));
    let result = &tool_results(&model.requests()[1])[0];
    assert!(result.contains("b/a.py") && result.contains("b/b.py"), "{result}");

    let (_, restored) = a.undo().expect("undo");
    assert_eq!(restored, ["a.py", "b.py"]);
    assert_eq!((read("a.py"), read("b.py")), ("x = 1\n".into(), "print(x)\n".into()));
}

/// Dropped connections, a stream that breaks mid-reply and a 503 are retried; the reply that finally arrives is
/// the only one kept. A 400 isn't retried.
#[tokio::test]
async fn retries_dropped_connections_broken_streams_and_server_errors() {
    let dir = project(&[]);
    let model = MockModel::start(vec![Reply::Drop, Reply::Broken("Half of an ans"), Reply::Status(503), Reply::Text("Full answer.")]).await;
    let mut a = agent(&model, &dir, Mode::Ask);
    let events = turn(&mut a, "hello", Decision::No).await;

    assert_eq!(model.requests().len(), 4);
    let retries = events.iter().filter(|e| matches!(e, AgentEvent::Retrying(_))).count();
    assert_eq!(retries, 3);
    assert_eq!(replies(&events), ["Full answer."]);
    assert_eq!(a.messages.last().unwrap().content, "Full answer.");

    let model = MockModel::start(vec![Reply::Status(400)]).await;
    let mut a = agent(&model, &dir, Mode::Ask);
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = a.turn("hello".into(), &tx).await.unwrap_err();
    assert!(format!("{err:#}").contains("API error 400"), "{err:#}");
    assert_eq!(model.requests().len(), 1, "a 400 was retried");
}

fn approvals(events: &[AgentEvent]) -> Vec<String> {
    events.iter().filter_map(|e| match e {
        AgentEvent::Notice(n) => n.strip_prefix("approval: ").map(String::from),
        _ => None,
    }).collect()
}

/// "Always" lasts for the session; `/permissions save` keeps it for the project in the user's config, so a new
/// session runs the same command prefix without asking (and still asks for others), until `forget`.
#[tokio::test]
async fn saved_permissions_carry_over_to_new_sessions() {
    let dir = project(&[]);
    let file = dir.join("config/permissions.json");
    let bash = |cmd: &str| Reply::Tools(vec![call("bash", json!({"command": cmd}))]);
    let model = MockModel::start(vec![
        bash("ls -a"), Reply::Text("listed"),
        bash("ls -l"), bash("pwd -L"), Reply::Text("done"),
        bash("ls -l"), Reply::Text("asked"),
    ])
    .await;

    let mut first = agent(&model, &dir, Mode::Ask).with_permissions_file(file.clone());
    assert_eq!(approvals(&turn(&mut first, "list", Decision::Always).await), ["Run `ls -a`"]);
    first.save_permissions(true).unwrap();
    assert!(std::fs::read_to_string(&file).unwrap().contains("\"ls\""));

    let mut second = agent(&model, &dir, Mode::Ask).with_permissions_file(file.clone());
    assert_eq!(approvals(&turn(&mut second, "list again", Decision::Yes).await), ["Run `pwd -L`"], "ls was saved");

    second.save_permissions(false).unwrap();
    let mut third = agent(&model, &dir, Mode::Ask).with_permissions_file(file);
    assert_eq!(approvals(&turn(&mut third, "once more", Decision::Yes).await), ["Run `ls -l`"], "forgotten");
}

/// New files are listed by absolute path at the end of the turn; edits to existing files aren't.
#[tokio::test]
async fn names_new_files_by_full_path() {
    let dir = project(&[("old.txt", "one\n")]);
    let model = MockModel::start(vec![
        Reply::Tools(vec![call("edit_file", json!({"path": "old.txt", "old_string": "one", "new_string": "two"}))]),
        Reply::Tools(vec![call("write_file", json!({"path": "site/index.html", "content": "<h1>hi</h1>\n"}))]),
        Reply::Text("Saved as index.html."),
    ])
    .await;
    let mut a = agent(&model, &dir, Mode::Auto);
    let events = turn(&mut a, "make a page", Decision::No).await;

    let notes: Vec<_> = events.iter().filter_map(|e| match e {
        AgentEvent::Notice(n) => Some(n.clone()),
        _ => None,
    }).collect();
    assert_eq!(notes, [format!("New files: {}", dir.join("site/index.html").display())]);
}

/// Several new files in one write_file call: one approval, all written, listed together, and undone together.
/// A batch with one bad path writes nothing.
#[tokio::test]
async fn batched_writes_need_one_approval_and_undo_together() {
    let dir = project(&[]);
    let files = json!({"files": [
        {"path": "data/a1.txt", "content": "A 0 0\nB 3 4\n"},
        {"path": "data/a2.txt", "content": "A 0 0\n"}]});
    let bad = json!({"files": [{"path": "ok.txt", "content": "x"}, {"path": "../escape.txt", "content": "x"}]});
    let model = MockModel::start(vec![
        Reply::Tools(vec![call("write_file", files)]),
        Reply::Tools(vec![call("write_file", bad)]),
        Reply::Text("Done."),
    ])
    .await;
    let mut a = agent(&model, &dir, Mode::Ask);
    let events = turn(&mut a, "write the data files", Decision::Yes).await;

    assert_eq!(approvals(&events), ["Write data/a1.txt, data/a2.txt (2 files)", "Write ok.txt, ../escape.txt (2 files)"]);
    assert_eq!(std::fs::read_to_string(dir.join("data/a1.txt")).unwrap(), "A 0 0\nB 3 4\n");
    let results = tool_results(&model.requests()[1]);
    assert!(results[0].contains("Created data/a1.txt (2 lines)") && results[0].contains("Created data/a2.txt"), "{results:?}");
    let bad_result = &tool_results(&model.requests()[2])[0];
    assert!(bad_result.contains("File 2 of 2") && bad_result.contains("No files were written"), "{bad_result}");
    assert!(!dir.join("ok.txt").exists(), "a failed batch wrote a file");

    let (_, restored) = a.undo().expect("undo");
    assert_eq!(restored, ["data/a1.txt", "data/a2.txt"]);
    assert!(!dir.join("data/a1.txt").exists() && !dir.join("data/a2.txt").exists());
}

/// Near the step limit the model is told to wrap up, in a tool result (so the request still extends the last one),
/// and the turn stops at the limit.
#[tokio::test]
async fn warns_before_the_step_limit_and_stops_at_it() {
    let dir = project(&[("a.txt", "alpha\n")]);
    let read = || Reply::Tools(vec![call("read_file", json!({"path": "a.txt"}))]);
    let model = MockModel::start((0..8).map(|_| read()).collect()).await;
    let mut a = agent(&model, &dir, Mode::Ask).with_max_steps(Some(7));
    let events = turn(&mut a, "keep reading", Decision::No).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 7);
    let warned: Vec<usize> = requests.iter().enumerate()
        .filter(|(_, r)| tool_results(r).iter().any(|t| t.contains("steps left in this turn")))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(warned.first(), Some(&2), "the note arrives with step 2's result, 5 steps before the limit of 7");
    for pair in requests.windows(2) {
        let (prev, next) = (messages(&pair[0]), messages(&pair[1]));
        assert_eq!(&next[..prev.len()], &prev[..], "the note rewrote an earlier message");
    }
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.starts_with("Stopped after 7 steps"))));
}

/// A fetch can send out anything the model has read, so it asks first. "Always" covers that host only.
#[tokio::test]
async fn web_fetch_asks_per_host() {
    let dir = project(&[]);
    // Port 1 refuses connections, so approved fetches fail fast without touching the network.
    let fetch = |url: &str| call("web_fetch", json!({"url": url}));
    let model = MockModel::start(vec![
        Reply::Tools(vec![fetch("http://127.0.0.1:1/a?q=secret")]), Reply::Text("declined"),
        Reply::Tools(vec![fetch("http://127.0.0.1:1/a")]), Reply::Text("fetched"),
        Reply::Tools(vec![fetch("http://127.0.0.1:1/b"), fetch("http://LOCALHOST:1/c")]), Reply::Text("done"),
    ])
    .await;
    let mut a = agent(&model, &dir, Mode::Ask);

    assert_eq!(approvals(&turn(&mut a, "look it up", Decision::No).await), ["Fetch http://127.0.0.1:1/a?q=secret"]);
    assert!(tool_results(&model.requests()[1])[0].contains("declined"), "nothing was fetched");

    assert_eq!(approvals(&turn(&mut a, "try again", Decision::Always).await), ["Fetch http://127.0.0.1:1/a"]);
    assert!(a.allowed().tools.contains("web_fetch:127.0.0.1"));
    assert!(!a.allowed().tools.contains("edit_file"), "a fetch approval doesn't allow edits");

    assert_eq!(approvals(&turn(&mut a, "and more", Decision::Yes).await), ["Fetch http://LOCALHOST:1/c"], "same host runs");
}
