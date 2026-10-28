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

ember runs on **macOS** (Apple Silicon and Intel) and **Linux** (x86_64 and ARM64, any distribution).
You need `git`, plus a [Nebius Token Factory API key](https://tokenfactory.nebius.com/project/api-keys).

### macOS

1. Install ember. Open Terminal (or iTerm2, Ghostty, WezTerm…) and run:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/aneesh-pradhan/ember/main/install.sh | sh
   ```

2. Put `~/.local/bin` on your PATH. macOS uses zsh by default; the installer prints the exact line if needed:

   ```bash
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
   ```

3. Add your API key:

   ```bash
   mkdir -p ~/.config/ember
   printf 'NEBIUS_API_KEY=%s\n' 'your-key' > ~/.config/ember/env && chmod 600 ~/.config/ember/env
   ```

4. `git` comes with the Xcode Command Line Tools. If `git --version` asks you to install them, accept, or run
   `xcode-select --install`.

Notes for macOS:
- Terminal.app may not show 24-bit color, so ember switches to a 256-color palette there automatically. For the
  best colors, use iTerm2, Ghostty, WezTerm, Kitty or the VS Code terminal. Set `EMBER_COLORS=256` or
  `EMBER_COLORS=truecolor` to override.
- If you download a release archive **in a browser** instead of using the installer, macOS quarantines the
  binary. Clear the flag with `xattr -d com.apple.quarantine ./ember`.
- To build from source instead: install the Xcode Command Line Tools and Rust
  (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`, or `brew install rust`), then
  `cargo install --git https://github.com/aneesh-pradhan/ember`.

### Linux

1. Install ember:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/aneesh-pradhan/ember/main/install.sh | sh
   ```

   The Linux builds are fully static (musl), so they work on any distribution.

2. If `~/.local/bin` isn't on your PATH (the installer tells you), add it:

   ```bash
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
   ```

3. Add your API key:

   ```bash
   mkdir -p ~/.config/ember
   printf 'NEBIUS_API_KEY=%s\n' 'your-key' > ~/.config/ember/env && chmod 600 ~/.config/ember/env
   ```

To build from source on Linux: install Rust 1.80+ (via [rustup](https://rustup.rs) or your package manager)
and run `cargo install --git https://github.com/aneesh-pradhan/ember`.

### Installer options, updating and uninstalling

- The installer downloads the matching binary from [Releases](https://github.com/aneesh-pradhan/ember/releases),
  verifies its SHA-256 checksum, and installs it to `~/.local/bin`.
- `EMBER_INSTALL_DIR=/some/dir` installs elsewhere; `EMBER_VERSION=v0.2.1` pins a version.
- **Update:** run the same one-liner again. **Uninstall:** `rm ~/.local/bin/ember`.
- The key can also live in a `.env` file in the project you run ember from. Saved conversations are stored in
  `~/.local/share/ember` on Linux and `~/Library/Application Support/ember` on macOS.

## Use

```bash
cd your-project
ember                              # interactive session
ember "why does the build fail?"   # start with a prompt
ember -c                           # continue the last conversation in this directory
ember --plan                       # start in plan mode
ember -p "summarize src/"          # print mode: answer and exit (edits/commands denied unless --yes)
ember --model ultra                # pick the model tier
```

### In the prompt

| Input | |
|---|---|
| `/` | Command menu: type to filter, ↑/↓ to choose, Tab or Enter to complete |
| `@path` | Attach a file to your message; a fuzzy file picker pops up as you type |
| `!command` | Run a shell command yourself; its output joins the conversation |
| Shift+Tab | Cycle modes: **ask** before edits → **auto-accept** → **plan** (read-only, ends in a plan) |
| Esc | Interrupt ember (or clear the input) |
| Ctrl+T | Show the task list |
| Alt+Enter / Ctrl+J | New line |

ember runs full screen: the conversation scrolls above a prompt box pinned to the bottom. Scroll back with the
mouse wheel, PgUp/PgDn or Shift+↑/↓, and jump back to live output with Ctrl+End. Permission prompts appear as a
panel in place of the prompt: pick with ↑/↓ and Enter, or press 1-3 (or y/a/n). Because ember uses the mouse for
scrolling, select text with Shift-drag (Option-drag in iTerm2), or set `EMBER_MOUSE=off` to keep normal selection.

For multi-step work ember keeps a live **task list**, and the status bar shows how full the **context window**
is. Near the limit ember compacts the conversation automatically. When a command needs approval, "always" applies
to that command family (e.g. all `cargo test` runs), never to arbitrary shell. Conversations are saved
automatically, so you can pick them up later.

### Commands

| Command | |
|---|---|
| `/fix <bug or GitHub issue URL>` | Reproduce with a failing test, fix in parallel worktrees, verify, apply on approval |
| `/init` | Explore the repo and write an `AGENTS.md` for future sessions |
| `/plan`, `/auto` | Toggle plan mode / auto-accept |
| `/model [nano\|super\|ultra\|id]` | Show or switch the model; `/models` lists those on your key |
| `/resume [n]` | List saved conversations in this directory, or continue one |
| `/compact` | Summarize the conversation so far to free up context |
| `/diff` | Show uncommitted changes |
| `/todos`, `/usage` | Task list; token usage per model |
| `/clear`, `/quit` | New conversation; exit (or Ctrl+C twice) |

ember reads project instructions from `AGENTS.md` or `EMBER.md` in the working directory.

### Configuration

| Variable | Default |
|---|---|
| `NEBIUS_API_KEY` | required |
| `NEBIUS_BASE_URL` | `https://api.tokenfactory.nebius.com/v1/` |
| `EMBER_MODEL_FAST` / `EMBER_MODEL_CODER` / `EMBER_MODEL_REASONER` | the models in the table above |
| `EMBER_FIX_BRANCHES` | `3` (1 to 4) |
| `EMBER_MOUSE` | `on`; `off` keeps your terminal's normal text selection (no wheel scrolling) |
| `EMBER_COLORS` | auto; `256` or `truecolor` to override color detection |
| `EMBER_TEST_CMD` | detected: `cargo test`, `go test ./...`, `pytest`, or `npm test` |

## Try `/fix` on the demo project

`examples/tinyledger` is a small invoicing library with a real rounding bug (see its `ISSUE.md`). Its tests run
with [uv](https://docs.astral.sh/uv/): `brew install uv` on macOS, or
`curl -LsSf https://astral.sh/uv/install.sh | sh` on either OS.

```bash
git clone https://github.com/aneesh-pradhan/ember && cd ember/examples/tinyledger
EMBER_TEST_CMD="uvx pytest -q" ember
/fix Invoice totals are a cent short: 2.50 with 5% tax gives tax 0.12, should be 0.13
```

## Design

```
src/
  main.rs      CLI (interactive, print mode)
  ui.rs        Ratatui inline-viewport TUI: scrollback transcript, live area, popups, approvals, /fix progress
  session.rs   saved conversations for /resume and --continue
  logo.rs      the pocket-rig mascot
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
