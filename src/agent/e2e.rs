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
