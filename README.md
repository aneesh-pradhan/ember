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
| **Ultra** | `nvidia/Nemotron-3-Ultra-550b-a55b` | `/fix` strategy planning and choosing between passing candidates; `/review` |
| **Super VL** | `nvidia/Nemotron-3.5-Super-VL` | ember's eyes: reads screenshots and diagrams you attach, and images the agent opens with `view_image` |

Switch the interactive model at any time with `/model nano|super|ultra` (or any model id from `/models`), and its
reasoning with `/think on|low|off` (or `--think`).
Nemotron's reasoning is streamed live in the UI (from `reasoning_content` or inline `<think>` blocks) and kept out of the transcript.

### Tuned for Nemotron on Token Factory

These choices come from measuring the models on Token Factory, not from defaults:

- **Reasoning only where it pays.** `enable_thinking: false` turns reasoning off on every Nemotron 3 model, while a
  `/no_think` system prompt is ignored, and `reasoning_effort: "low"` only shortens it reliably on Super. ember turns
  reasoning off for mechanical drafting (commit messages, branch names, PR text): on a real diff that took commit
  drafts from 2.0–2.6 s to 1.0–1.5 s with 3–4× fewer output tokens and equally good messages. It stays **on**
  where accuracy matters: Nano with thinking off answered "is 91 prime?" wrongly, so the `/fix` judge keeps it, and
  Super VL describes screenshots just as accurately either way, so it keeps its default too. For the interactive
  agent, on, low and off all took about 4–6 s on an exploration task (time goes to round trips, not thinking), and
  `off` started narrating its steps into the answer, so the default is `on`.
- **Batched reads.** Nemotron Super makes one tool call per reply, even with `parallel_tool_calls` and an explicit
  hint, so every extra file costs a round trip that resends the conversation. `read_file` accepts a list of `paths`
  and the prompt tells the model to batch; in testing it read related files together and grounded answers in
  more of the code at the same cost.
- **Prompt-cache friendly.** Token Factory caches repeated prompt prefixes (4,512 of 4,533 tokens on a repeat
  request), billed at a lower rate. ember keeps the system prompt and the tool list fixed for each turn, so the
  growing conversation is re-sent as a stable prefix, and `/usage` shows how much input came from the cache
  (up to 99% on warm runs).

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
- `EMBER_INSTALL_DIR=/some/dir` installs elsewhere; `EMBER_VERSION=v0.5.0` pins a version.
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
| `@path` | Attach a file (or image) to your message; a fuzzy file picker pops up as you type, Esc closes it |
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
| `/review` | Nemotron Ultra reviews your uncommitted changes, most severe findings first |
| `/commit [note]` | Draft a commit message from your changes; stage and commit only after you approve |
| `/branch <description>` | Create and switch to a well-named branch for the work |
| `/pr` | Draft a pull request from the branch's commits; push and open it with `gh` after you approve |
| `/resolve` | Work through merge conflicts, then run the tests |
| `/undo` | Restore every file ember changed in its last turn (shell commands aren't undone) |
| `/init` | Explore the repo and write an `AGENTS.md` for future sessions |
| `/plan`, `/auto` | Toggle plan mode / auto-accept |
| `/model [nano\|super\|ultra\|id]` | Show or switch the model; `/models` lists those on your key |
| `/think [on\|low\|off]` | Set how much Nemotron reasons before acting |
| `/resume [n]` | List saved conversations in this directory, or continue one |
| `/compact` | Summarize the conversation so far to free up context |
| `/mcp` | Show connected MCP servers and their tools |
| `/diff`, `/todos`, `/usage` | Uncommitted changes; task list; token usage per model, including prompt-cache hits |
| `/clear`, `/quit` | New conversation; exit (or Ctrl+C twice) |

ember reads project instructions from `AGENTS.md` or `EMBER.md` in the working directory.

### Images, web and tools

- **Images:** mention a screenshot with `@shot.png`, or drag an image file into the terminal. Nemotron Super VL
  describes it (transcribing any text exactly) for the coding model. The agent can also look at images in your
  repo with `view_image`.
- **Web:** the agent can read pages with `web_fetch`. Set `TAVILY_API_KEY` ([tavily.com](https://tavily.com)) to also give it
  `web_search`, and to have `web_fetch` use Tavily Extract for cleaner pages.
- **MCP servers:** add stdio servers to `.ember/mcp.json` (per project) or `~/.config/ember/mcp.json`, in the
  usual format. Each tool call asks for permission (or "always" for that tool):

  ```json
  {"mcpServers": {"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                             "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "..."}}}}
  ```

### Custom commands

Save a prompt as Markdown in `.ember/commands/<name>.md` (shared with your team through the repo) or
`~/.config/ember/commands/<name>.md` (just for you), and it becomes `/<name>`. `$ARGUMENTS` is replaced with
whatever you type after the command, and `@file` mentions work inside it:

```markdown
---
description: Explain a file to a beginner
---
Explain $ARGUMENTS to someone new to this codebase, in five bullet points.
```

### Run Nemotron on your own hardware

ember speaks the OpenAI-compatible API, so it can drive a model served from your own machine, such as a
Jetson dev kit or a workstation GPU. Start an OpenAI-compatible server (for example llama.cpp's `llama-server`,
vLLM, or an NVIDIA NIM) with a Nemotron model that supports tool calling, then point ember at it:

```bash
EMBER_BASE_URL=http://jetson.local:8000/v1 \
EMBER_MODEL_CODER=<model name your server reports> \
ember
```

No Nebius key is needed for a local server. Pick a model that fits the device's memory, and set
`EMBER_MODEL_FAST` / `EMBER_MODEL_REASONER` too if you use `/fix` or `/review`.

### Configuration

| Variable | Default |
|---|---|
| `NEBIUS_API_KEY` | required for Token Factory |
| `EMBER_BASE_URL` (or `NEBIUS_BASE_URL`) | `https://api.tokenfactory.nebius.com/v1/` |
| `EMBER_MODEL_VISION` | `nvidia/Nemotron-3.5-Super-VL` |
| `TAVILY_API_KEY` | unset; enables `web_search` |
| `EMBER_MODEL_FAST` / `EMBER_MODEL_CODER` / `EMBER_MODEL_REASONER` | the models in the table above |
| `EMBER_FIX_BRANCHES` | `3` (1 to 4) |
| `EMBER_THINK` | `on`; `low` or `off` for faster, shallower replies (same as `--think` / `/think`) |
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
  ui.rs        full-screen Ratatui TUI: conversation, prompt box, suggestions, permission panel, status line
  agent.rs     agent loop: stream → tool calls → approval → results → repeat; checkpoints for /undo
  fix.rs       /fix pipeline over git worktrees
  git.rs       /commit, /branch, /pr and git status
  llm.rs       streaming OpenAI-compatible client: SSE, tool-call assembly, <think> splitting, retries
  tools.rs     read_file, list_files, grep (.gitignore-aware), edit_file, write_file, bash
  web.rs       web_search (Tavily) and web_fetch
  vision.rs    images through Nemotron VL
  mcp.rs       MCP client for stdio servers
  commands.rs  custom slash commands
  session.rs   saved conversations for /resume and --continue
  markdown.rs  Markdown rendering and word wrapping for the terminal
  logo.rs      the chibi mascot
```

```bash
cargo test
```

## License

MIT
