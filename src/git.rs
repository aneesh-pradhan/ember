//! Git helpers: status for the status bar, and /commit, /branch, /pr flows that draft with Nemotron
//! and act only after the user approves. Commits go through plain `git commit`, so the user's own
//! signing, sign-off and hook configuration applies; ember adds no trailers of its own.

use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::agent::{ask, AgentEvent, Decision, Events};
use crate::llm::{Client, Message, Request};

/// Run git and return stdout, or an error with stderr.
pub async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).output().await.context("could not run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Current branch and number of changed files, for the status bar.
pub fn status(dir: &Path) -> Option<(String, usize)> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "--branch"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let head = lines.next()?.trim_start_matches("## ");
    let branch = head.split("...").next().unwrap_or(head).replace("No commits yet on ", "");
    Some((branch, lines.count()))
}

/// Uncommitted changes (tracked diff plus new files), as text for a reviewer.
pub async fn review_material(dir: &Path) -> Result<String> {
    let diff = git(dir, &["diff", "HEAD", "--no-color"]).await.unwrap_or_default();
    let untracked = git(dir, &["ls-files", "--others", "--exclude-standard"]).await.unwrap_or_default();
    let mut out = String::new();
    if !diff.trim().is_empty() {
        out.push_str(&format!("```diff\n{}\n```\n", crate::tools::truncate(&diff, 120_000)));
    }
    for file in untracked.lines().take(20) {
        let content = std::fs::read_to_string(dir.join(file)).unwrap_or_default();
        if content.len() < 40_000 {
            out.push_str(&format!("\nNew file `{file}`:\n```\n{content}\n```\n"));
        } else {
            out.push_str(&format!("\nNew file `{file}` (too large to include)\n"));
        }
    }
    Ok(out)
}

async fn draft(client: &Client, model: &str, system: &str, user: String, tx: &Events) -> Result<String> {
    let resp = client
        .stream(
            &Request {
                model: model.to_string(),
                messages: vec![Message::system(system), Message::user(user)],
                max_tokens: Some(4096),
                ..Default::default()
            }
            // Mechanical drafting: measured 2.4s → 1.0s and ~25x fewer output tokens with reasoning off.
            .think(crate::llm::Think::Off),
            |_| {},
        )
        .await?;
    let _ = tx.send(AgentEvent::Usage { model: model.into(), prompt: resp.usage.prompt_tokens, completion: resp.usage.completion_tokens, cached: resp.usage.cached_tokens });
    Ok(resp.content.trim().trim_matches('`').trim().to_string())
}

fn show(tx: &Events, markdown: String) {
    let _ = tx.send(AgentEvent::Reply { content: markdown, thought_secs: None });
}

fn working(tx: &Events, what: &str) {
    let _ = tx.send(AgentEvent::ToolStart { summary: what.into() });
}

/// /commit: stage (with permission) if nothing is staged, draft a message, commit on approval.
pub async fn commit(client: Client, model: String, dir: &Path, hint: String, tx: Events) -> Result<()> {
    let mut staged = git(dir, &["diff", "--cached", "--stat"]).await?;
    if staged.trim().is_empty() {
        let changes = git(dir, &["status", "--porcelain"]).await?;
        if changes.trim().is_empty() {
            bail!("nothing to commit: the working tree is clean");
        }
        show(&tx, format!("Nothing is staged yet. Changes in the working tree:\n```\n{changes}\n```"));
        if ask(&tx, "Stage all changes (git add -A)".into(), None, None).await == Decision::No {
            bail!("cancelled; stage what you want with `!git add …` and run /commit again");
        }
        git(dir, &["add", "-A"]).await?;
        staged = git(dir, &["diff", "--cached", "--stat"]).await?;
    }
    working(&tx, "Drafting a commit message");
    let diff = git(dir, &["diff", "--cached", "--no-color"]).await?;
    let log = git(dir, &["log", "--oneline", "-10"]).await.unwrap_or_default();
    let message = draft(
        &client,
        &model,
        "You write git commit messages. Output ONLY the message: a subject line in the imperative mood of at most \
         72 characters, then a blank line, then a short body (wrapped at 72 columns) explaining what changed and why. \
         Match the style of the recent commits shown. Do not add trailers, sign-offs, co-author lines or any mention \
         of AI tools.",
        format!(
            "Recent commits:\n{log}\n\nStaged changes:\n{staged}\n\n```diff\n{}\n```{}",
            crate::tools::truncate(&diff, 60_000),
            if hint.is_empty() { String::new() } else { format!("\n\nThe author's note about this change: {hint}") }
        ),
        &tx,
    )
    .await?;
    let message = wrap_body(&message, 72);
    show(&tx, format!("**Proposed commit**\n```\n{message}\n```"));
    if ask(&tx, "Commit with this message".into(), None, None).await == Decision::No {
        bail!("commit cancelled; the changes are still staged");
    }
    working(&tx, "git commit (your signing setup may ask for a passphrase)");
    let file = std::env::temp_dir().join(format!("ember-commit-{}.txt", std::process::id()));
    std::fs::write(&file, format!("{message}\n"))?;
    let result = git(dir, &["commit", "-F", file.to_str().unwrap_or_default()]).await;
    let _ = std::fs::remove_file(&file);
    result?;
    let info = git(dir, &["log", "-1", "--format=%h %G? %s"]).await?;
    let mut parts = info.splitn(3, ' ');
    let (hash, sig, subject) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let signed = match sig {
        "G" => "signed ✓",
        "N" => "unsigned",
        "U" | "X" | "Y" => "signed (key not fully trusted locally)",
        _ => "signature not verified",
    };
    let _ = tx.send(AgentEvent::Notice(format!("Committed {hash} {subject} · {signed}")));
    Ok(())
}

/// /branch <description>: suggest a branch name and switch to it on approval.
pub async fn branch(client: Client, model: String, dir: &Path, description: String, tx: Events) -> Result<()> {
    if description.trim().is_empty() {
        bail!("usage: /branch <what the work is about>");
    }
    working(&tx, "Naming the branch");
    let name = draft(
        &client,
        &model,
        "Suggest one git branch name for the described work. Output ONLY the name: lowercase, words separated by \
         hyphens, prefixed with feat/, fix/, chore/, docs/ or refactor/ as appropriate, at most 40 characters.",
        description,
        &tx,
    )
    .await?;
    let name: String = name.lines().next().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric() || "-/_.".contains(*c)).collect();
    if name.is_empty() {
        bail!("couldn't come up with a valid branch name");
    }
    if ask(&tx, format!("Create and switch to branch `{name}`"), None, None).await == Decision::No {
        bail!("cancelled");
    }
    git(dir, &["switch", "-c", &name]).await?;
    let _ = tx.send(AgentEvent::Notice(format!("Switched to new branch {name}")));
    Ok(())
}

/// /pr: draft a pull request from the branch's commits; push and open it with `gh` on approval.
pub async fn pr(client: Client, model: String, dir: &Path, tx: Events) -> Result<()> {
    let gh = Command::new("gh").arg("--version").output().await;
    if gh.map(|o| !o.status.success()).unwrap_or(true) {
        bail!("/pr needs the GitHub CLI (`gh`), logged in with `gh auth login`");
    }
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    let base = Command::new("gh")
        .args(["repo", "view", "--json", "defaultBranchRef", "-q", ".defaultBranchRef.name"])
        .current_dir(dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "main".into());
    if branch == base || branch == "HEAD" {
        bail!("you're on {branch}; create a feature branch first (try /branch <description>)");
    }
    let range = format!("origin/{base}..HEAD");
    let commits = git(dir, &["log", "--format=- %s%n%b", &range]).await.or(git(dir, &["log", "--format=- %s", "-20"]).await)?;
    if commits.trim().is_empty() {
        bail!("no commits on {branch} that aren't on {base} yet");
    }
    let diff = git(dir, &["diff", "--stat", &format!("origin/{base}...HEAD")]).await.unwrap_or_default();
    working(&tx, "Drafting the pull request");
    let text = draft(
        &client,
        &model,
        "Write a GitHub pull request. Output a title on the first line (no prefix, at most 72 characters), a blank \
         line, then a Markdown body: a short summary, a bullet list of the changes, and a '## Testing' section of \
         unchecked checkboxes listing what a reviewer should verify. Base it only on the commits and diff given. \
         Never claim that tests were run or anything was verified, and do not mention AI tools.",
        format!("Branch {branch} into {base}.\n\nCommits:\n{commits}\n\nFiles changed:\n{diff}"),
        &tx,
    )
    .await?;
    let (title, body) = text.split_once('\n').map_or((text.as_str(), ""), |(t, b)| (t, b));
    let title = title.trim().trim_start_matches('#').trim().to_string();
    show(&tx, format!("**Proposed pull request** ({branch} → {base})\n\n**{title}**\n\n{}", body.trim()));
    if ask(&tx, format!("Push {branch} and open this pull request on GitHub"), None, None).await == Decision::No {
        bail!("cancelled; nothing was pushed");
    }
    working(&tx, "git push");
    git(dir, &["push", "-u", "origin", &branch]).await?;
    working(&tx, "gh pr create");
    let file = std::env::temp_dir().join(format!("ember-pr-{}.md", std::process::id()));
    std::fs::write(&file, body.trim())?;
    let out = Command::new("gh")
        .args(["pr", "create", "--base", &base, "--head", &branch, "--title", &title, "--body-file"])
        .arg(&file)
        .current_dir(dir)
        .output()
        .await?;
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        bail!("gh pr create failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let _ = tx.send(AgentEvent::Notice(format!("Opened {}", String::from_utf8_lossy(&out.stdout).trim())));
    Ok(())
}

/// Wrap a commit message's body at `width` columns (models don't reliably do it). The subject line, code and
/// indented lines are left alone; list items keep a hanging indent.
fn wrap_body(message: &str, width: usize) -> String {
    let mut lines = message.lines();
    let mut out = vec![lines.next().unwrap_or("").to_string()];
    for line in lines {
        if line.chars().count() <= width || line.starts_with(' ') || line.starts_with('\t') {
            out.push(line.to_string());
            continue;
        }
        let indent = if line.starts_with("- ") || line.starts_with("* ") { "  " } else { "" };
        let mut current = String::new();
        for word in line.split(' ') {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut current));
                current.push_str(indent);
            } else if !current.is_empty() && current != indent {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push(current);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_commit_bodies_but_not_subjects_or_code() {
        let long = "word ".repeat(30);
        let msg = format!("A subject line that is longer than seventy-two characters stays on one line\n\n{long}\n- {long}\n    {long}");
        let wrapped = wrap_body(&msg, 72);
        let lines: Vec<&str> = wrapped.lines().collect();
        assert!(lines[0].len() > 72, "the subject must not be wrapped");
        assert!(lines[1].is_empty());
        let (body, code): (Vec<&str>, Vec<&str>) = lines[2..].iter().partition(|l| !l.starts_with("    "));
        assert!(body.iter().all(|l| l.chars().count() <= 72), "{wrapped}");
        assert!(body.iter().any(|l| l.starts_with("- word")) && body.iter().any(|l| l.starts_with("  word")), "{wrapped}");
        assert_eq!(code.len(), 1, "indented lines stay as they are");
        assert_eq!(wrap_body("short\n\nbody", 72), "short\n\nbody");
    }
}
