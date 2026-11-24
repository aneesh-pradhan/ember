# Architecture

ember is one Rust binary. A full-screen terminal UI and a non-interactive print mode (`-p`) share one agent loop. Model calls go to an OpenAI-compatible HTTP API: Nebius Token Factory by default, or whatever `EMBER_BASE_URL` points at.

## Request path

1. `main` parses the CLI and loads `Config` from the process environment, then `~/.config/ember/env`, then a project `.env`. Real environment variables win. The config picks a model id per tier (Nano, Super, Ultra), a draft model for mechanical writing, and a vision model.
2. Interactive mode starts the Ratatui UI. Print mode streams the answer to stdout and denies edits and shell commands unless `--yes` is set.
3. The agent loop in `agent` sends the conversation to `llm`, which streams a chat completion (SSE), assembles tool calls, and splits reasoning out of the visible text. Retries cover dropped connections and 429/5xx responses.
4. When the model asks for a tool, the loop asks the UI (or print mode) for a decision. Ask mode shows the diff or the command first. Auto mode runs it. Plan mode stays read-only. An "always" approval is scoped to a command family or a tool, and `/permissions save` stores it under `~/.config/ember`, not in the repo.
5. `tools` runs the call inside the workspace: `read_file`, `list_files`, `grep` (gitignore-aware), `edit_file`, `write_file`, and `bash`. Paths that escape the workspace are refused. Results go back into the conversation, and the loop repeats until the model stops or the step limit hits.
6. The UI keeps the transcript, the prompt, permission prompts, a task list, and a context-window meter. Near 80% of the window the loop compacts the conversation. Sessions are written for `ember -c` and `/resume`.

`web_fetch` is offered whenever web tools are on. `web_search` is added only when `TAVILY_API_KEY` is set. `view_image` sends a local image through the vision model and gives the description to the coding model. MCP servers from `.ember/mcp.json` stay stopped until `/mcp trust`.

A short repo map is prepended to the prompt so later turns extend a stable prefix and can hit the provider's prompt cache. `EMBER_REPO_MAP=0` turns that off.

## `/fix`

`/fix` does not patch the working tree first. It investigates with read-only tools, writes a test that must fail on the current code, and asks the Nano tier to reject a failure that is the test's fault. The Ultra tier proposes distinct strategies. Each strategy runs in its own git worktree, with test files read-only, so a branch cannot pass by weakening the test. A branch wins only when the new test passes and the full suite has no new failures against a baseline. Ultra picks among the passers. The working tree changes only after approval. Uncommitted tracked work is snapshotted with `git stash create`, which does not modify the tree.

`/commit`, `/branch`, `/pr`, and `/review` draft text with the model and then call ordinary git. Commits go through `git commit`, so the user's hooks and signing config still apply.

## Layout

```
src/
  main.rs       CLI: interactive session, or print mode
  config.rs     env, ~/.config/ember/env, project .env; model tiers and prices
  llm.rs        streaming OpenAI-compatible client
  agent.rs      tool loop, approvals, compaction, /undo checkpoints
  ui.rs         Ratatui UI: transcript, prompt, permissions, status
  input.rs      multi-line editor
  theme.rs      palettes (kawaii, classic) and color detection
  logo.rs       mascot
  markdown.rs   terminal markdown
  animation.rs  cosmetic motion; off when CI, NO_COLOR, or EMBER_REDUCED_MOTION is set
  tools.rs      read, search, edit, write, bash
  fix.rs        /fix pipeline over git worktrees
  git.rs        status, /commit, /branch, /pr
  web.rs        web_fetch and optional Tavily web_search
  vision.rs     images through the vision model
  mcp.rs        stdio MCP client; project servers need /mcp trust
  commands.rs   custom slash commands from Markdown files
  session.rs    saved conversations
  permissions.rs
                remembered approvals, stored outside the repo
  repomap.rs    compact repo map used as a cache-stable prompt prefix
  mock.rs       local stand-in for the HTTP API, test-only
```

Tests are in the modules plus `src/agent/e2e.rs`. The end-to-end tests point the client at `mock` instead of Token Factory, so `cargo test` needs no API key and no network.
