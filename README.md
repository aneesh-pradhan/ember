# ember

**A terminal coding agent powered by NVIDIA Nemotron on Nebius Token Factory, written in Rust.**

ember lives in your terminal. Ask it to explain code, build a feature, or fix a failing build. It reads
and searches your project, edits files, and runs commands. Before any edit or command, it shows you
the diff or the command line and asks for approval. The conversation stays in your terminal's normal
scrollback, so you can scroll, search, and copy it like any other output.

Its standout feature is **`/fix`**, which never hands you a patch it hasn't proven:

```
/fix Invoice totals are a cent short when tax lands on a half cent
```

1. **Investigate.** Nemotron Super explores the code with read-only tools and reports the root cause.
2. **Reproduce.** It writes a new test that must **fail on your current code**. A Nemotron Nano judge rejects
   tests that fail for the wrong reason (import errors, typos, asserting something unrelated). If ember can't
   reproduce the bug, it stops and tells you so instead of guessing.
3. **Plan.** Nemotron Ultra proposes several genuinely different fix strategies.
4. **Branch.** Each strategy runs in its own **git worktree**, in parallel, with its own patching loop. Test files
   are read-only inside a branch, so a fix can't "pass" by weakening the test.
5. **Verify.** A branch wins only if the reproduction test passes **and** the full test suite has no new failures
   compared with a baseline run.
6. **Review & apply.** Ultra picks the best passing candidate and explains why. You see the diff (fix +
   regression test) and decide whether to apply it to your working tree.

Your working tree is never touched until you approve. Uncommitted changes are included in the experiment through
`git stash create`, which snapshots them without modifying anything.

Hackathon track: **Coding and Agentic Engineering** (Nebius × NVIDIA Global AI Hackathon).

## How it uses Nemotron on Nebius Token Factory

Every model call goes to Token Factory's OpenAI-compatible API, with streaming and native tool calling.
ember routes each kind of work to the smallest Nemotron model that does it well:

| Tier | Default model | Used for |
|---|---|---|
| **Nano** | `nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B` | `/fix` judgements: does this failing test really reproduce the bug? did the suite regress? |
| **Super** | `nvidia/nemotron-3-super-120b-a12b` | The interactive agent; `/fix` investigation, test writing, and every branch's patching loop |
| **Ultra** | `nvidia/Nemotron-3-Ultra-550b-a55b` | `/fix` strategy planning and choosing between passing candidates |

Switch the interactive model at any time with `/model nano|super|ultra` (or any model id from `/models`).
Nemotron's reasoning is streamed live in the UI (from `reasoning_content` or inline `<think>` blocks) and kept out of the transcript.

## Install

On Linux (x86_64 or ARM64, any distribution) or macOS (Apple Silicon or Intel):

```bash
curl -fsSL https://raw.githubusercontent.com/aneesh-pradhan/ember/main/install.sh | sh
```

The script downloads the matching binary from [Releases](https://github.com/aneesh-pradhan/ember/releases),
verifies its SHA-256 checksum, and installs it to `~/.local/bin`. Set `EMBER_INSTALL_DIR` to choose another
directory, or `EMBER_VERSION=v0.1.1` to pin a version. To uninstall, delete `~/.local/bin/ember`.

Or build from source (Rust 1.80+):

```bash
cargo install --git https://github.com/aneesh-pradhan/ember
```

Then put your Token Factory key in `~/.config/ember/env` (or a `.env` in the project you run it from):

```bash
mkdir -p ~/.config/ember
printf 'NEBIUS_API_KEY=%s\n' 'your-key' > ~/.config/ember/env && chmod 600 ~/.config/ember/env
```

## Use

```bash
cd your-project
ember                         # interactive session
ember "why does the build fail?"   # start with a prompt
ember -p "summarize src/" # print mode: answer and exit (edits/commands denied unless --yes)
ember --model ultra           # pick the model tier
```

| Command | |
|---|---|
| `/fix <bug or GitHub issue URL>` | Reproduce with a failing test, fix in parallel worktrees, verify, apply on approval |
| `/model [nano\|super\|ultra\|id]` | Show or switch the model |
| `/models` | List Nemotron models available to your key |
| `/auto` or Shift+Tab | Toggle auto-approve for edits and commands |
| `/usage` | Token usage per model this session |
| `/clear` | Start a new conversation |
| `/quit` | Exit (or Ctrl+C twice) |

Keys: Enter sends, Alt+Enter or Ctrl+J inserts a newline, Esc interrupts, ↑/↓ browse history.

ember reads project instructions from `AGENTS.md` or `EMBER.md` in the working directory.

### Configuration

| Variable | Default |
|---|---|
| `NEBIUS_API_KEY` | required |
| `NEBIUS_BASE_URL` | `https://api.tokenfactory.nebius.com/v1/` |
| `EMBER_MODEL_FAST` / `EMBER_MODEL_CODER` / `EMBER_MODEL_REASONER` | the models in the table above |
| `EMBER_FIX_BRANCHES` | `3` (1 to 4) |
| `EMBER_TEST_CMD` | detected: `cargo test`, `go test ./...`, `pytest`, or `npm test` |

## Try `/fix` on the demo project

`examples/tinyledger` is a small invoicing library with a real rounding bug (see its `ISSUE.md`):

```bash
cd examples/tinyledger
EMBER_TEST_CMD="uvx pytest -q" ember
/fix Invoice totals are a cent short: 2.50 with 5% tax gives tax 0.12, should be 0.13
```

## Design

```
src/
  main.rs      CLI (interactive, print mode)
  ui.rs        Ratatui inline-viewport TUI: scrollback transcript, live area, approvals, /fix progress
  agent.rs     agent loop: stream → tool calls → approval → results → repeat; cancellation-safe history
  fix.rs       /fix pipeline over git worktrees
  llm.rs       streaming OpenAI-compatible client: SSE, tool-call assembly, <think> splitting, retries
  tools.rs     read_file, list_files, grep (.gitignore-aware), edit_file, write_file, bash
  markdown.rs  Markdown rendering and word wrapping for the terminal
```

```bash
cargo test
```

## License

MIT
