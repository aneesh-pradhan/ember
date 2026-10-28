# FailFirst

**An agent that proves a bug with a failing test before it fixes it.**

Most AI coding agents go straight from an issue to a patch, and you're left wondering whether it fixes
anything. FailFirst works the way a careful maintainer does:

1. **Reproduce first.** It reads the issue and the code, then writes a pytest test that **fails on the current
   code, for the reason the issue describes**. A Nemotron judge rejects tests that fail for the wrong reason
   (an import error, a typo, asserting the wrong thing). If it can't reproduce the bug, it stops and says so. It
   never hands you an unverified patch.
2. **Checkpoint.** The sandbox state with the failing test is saved as an immutable image.
3. **Branch.** Nemotron Ultra plans several genuinely different fix strategies. Each one is patched and tested
   in its own sandbox, in parallel, from the same checkpoint.
4. **Verify.** A branch only wins if the reproduction test now passes **and** no existing test that passed before
   now fails. The agent is not allowed to edit test files, so it can't "fix" the bug by weakening the test.
5. **Review.** Ultra compares the passing candidates, picks one, and writes the pull request description.

You get a diff containing the fix *plus* the regression test, with evidence that the test failed before and passes after.

Track: **Coding and Agentic Engineering**.

## How it uses NVIDIA Nemotron on Nebius Token Factory

Every model call goes to Nebius Token Factory's OpenAI-compatible API. FailFirst routes each step to the
smallest Nemotron model that does it well:

| Tier | Default model | Used for | Why this tier |
|---|---|---|---|
| **Nano** | `nvidia/Nemotron-3-Nano-30B-A3B` | Triage of the issue; judging whether a failing test genuinely reproduces the bug | High-volume classification calls; low latency keeps the loop fast |
| **Super** | `nvidia/Nemotron-3-Super-120b-a12b` | Investigating the codebase (tool-using loop), writing the reproduction test, writing patches in each branch | Strong coding at a cost that allows many parallel attempts |
| **Ultra** | `nvidia/Nemotron-3-Ultra-550b-a55b` | Planning distinct fix strategies; reviewing passing candidates and writing the PR | The two decisions where deeper reasoning changes the outcome |

The UI shows which tier made every call, with token counts per model.

## How it uses Nebius Sandboxes

Running model-written code needs isolation. FailFirst executes everything (dependency install, the baseline suite,
reproduction attempts, every patch) in [Nebius Sandboxes](https://docs.tokenfactory.nebius.com/sandboxes/overview)
through `contree-sdk`. Each run is a fresh microVM. The agent relies on Sandboxes' checkpoint-and-branch model:

```
python:3.12-slim ──install──▶ [env-ready] ──add repro test──▶ [bug-reproduced] ─┬─▶ branch B1: patch + tests
                                    │                                           ├─▶ branch B2: patch + tests
                                    └──▶ baseline test suite                    └─▶ branch B3: patch + tests
```

Branches are disposable runs from the same immutable image, so they can't interfere with each other, and a
failed attempt never pollutes the next one. The source code is read on the host (read-only) for fast exploration;
it is only ever *executed* inside the sandbox.

For local development without Sandboxes access, a Podman backend mimics the same model (`podman commit` for
checkpoints, disposable containers for branches, no network during test runs).

## Running it

Requirements: Python 3.12, [uv](https://docs.astral.sh/uv/), a Nebius Token Factory API key, and either
Nebius Sandboxes access or Podman.

```bash
git clone <this repo> failfirst && cd failfirst
uv sync
cp .env.example .env     # add NEBIUS_API_KEY (and NEBIUS_PROJECT_ID for Sandboxes)
uv run failfirst models  # list the Nemotron model ids your key can use
```

Web UI:

```bash
uv run failfirst serve   # http://127.0.0.1:8000
```

Command line:

```bash
# A GitHub issue
uv run failfirst run https://github.com/owner/repo/issues/123

# Or any repo (GitHub URL or local path) plus a description
uv run failfirst run --repo examples/tinyledger --description "$(cat examples/tinyledger/ISSUE.md)"
```

Each run is saved to `runs/<id>.jsonl` and can be replayed in the UI.
Setting `FAILFIRST_ALLOW_LIVE_RUNS=false` makes a public deployment replay-only.

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `NEBIUS_API_KEY` | (required) | Token Factory API key |
| `NEBIUS_PROJECT_ID` | (unset) | Enables Nebius Sandboxes; otherwise Podman is used |
| `FAILFIRST_MODEL_FAST` / `_CODER` / `_REASONER` | see table above | Model id per tier |
| `FAILFIRST_BRANCHES` | `3` | Parallel fix branches |
| `FAILFIRST_MAX_REPRO_ATTEMPTS` | `4` | Attempts at a genuine reproduction |
| `FAILFIRST_MAX_FIX_ROUNDS` | `3` | Patch/test rounds per branch |
| `GITHUB_TOKEN` | (unset) | Higher GitHub API rate limit |

### Scope

FailFirst currently targets Python projects tested with pytest that install with `pip install -e .` or a
requirements file.

## Tests

```bash
uv run pytest
```

`tests/test_pipeline_podman.py` runs the whole pipeline against the bundled `examples/tinyledger` repository in
real Podman containers, with a scripted model standing in for Nemotron, so the orchestration is tested without spending tokens.

## Project layout

```
failfirst/
  agent.py          the pipeline: triage → investigate → reproduce → plan → fix branches → review
  llm.py            Token Factory client, per-tier routing and usage accounting
  repo.py           in-memory repo snapshot: search, SEARCH/REPLACE edits, diffs; GitHub fetching
  sandbox/          Nebius Sandboxes and Podman backends behind one checkpoint/branch interface
  server.py         FastAPI app with server-sent events and run replay
  prompts.py        prompts for each step
web/                the UI (no build step)
examples/tinyledger a small library with a real rounding bug, used for the demo and tests
```

## License

MIT
