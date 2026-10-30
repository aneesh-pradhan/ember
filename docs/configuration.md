# Configuration

ember reads settings from environment variables, `~/.config/ember/env`, and a `.env` file in the project you run it
from. The key is the only required setting for Token Factory.

A project's `.env` can't set `EMBER_BASE_URL`, `NEBIUS_BASE_URL`, `EMBER_TEST_CMD` or `EMBER_LOG`: a cloned repository
could otherwise send your key and code to another server or pick the command `/fix` runs. Set those in
`~/.config/ember/env` or the environment.

## Variables

| Variable | Default |
|---|---|
| `NEBIUS_API_KEY` | required for Token Factory |
| `EMBER_BASE_URL` (or `NEBIUS_BASE_URL`) | `https://api.tokenfactory.nebius.com/v1/` |
| `EMBER_MODEL_VISION` | `nvidia/Nemotron-3.5-Super-VL` |
| `TAVILY_API_KEY` | unset; enables `web_search` |
| `EMBER_MODEL_FAST` / `EMBER_MODEL_CODER` / `EMBER_MODEL_REASONER` | the models in the [model table](../README.md#how-it-uses-nemotron-on-nebius-token-factory) |
| `EMBER_MODEL_DRAFT` | `nvidia/Nemotron-3_5-Lightning` on Token Factory, else the coder model: commit messages, branch names, PR text and `/compact` summaries |
| `EMBER_FIX_BRANCHES` | `3` (1 to 4) |
| `EMBER_THINK` | `on`; `low` or `off` for faster, shallower replies (same as `--think` / `/think`) |
| `EMBER_THINK_BUDGET` | `4096` reasoning tokens per request on Token Factory, no cap elsewhere; a number, or `off` |
| `EMBER_KEEP_REASONING` | `on`: each step's reasoning is sent back to the model for the rest of the task; `0` turns it off |
| `EMBER_NO_WEB` | unset; `1` removes the `web_search` and `web_fetch` tools (used by the SWE-bench runner) |
| `EMBER_MAX_STEPS` | `80` model calls per turn; ember asks the model to wrap up 5 steps before the limit. `--max-steps` overrides it |
| `EMBER_VERBOSE` | unset; `1` makes `ember -p "/fix …"` log every tool call its branches make (`[fix:activity]` lines on stderr) |
| `EMBER_MOUSE` | `on`; `off` keeps your terminal's normal text selection (no wheel scrolling) |
| `EMBER_HEADER` | `on`: once the startup banner scrolls away, a compact header (mini mascot, its mood, model and folder) stays pinned at the top; `off` turns it off |
| `EMBER_COLORS` | auto; `256` or `truecolor` to override color detection |
| `EMBER_THEME` | `kawaii` (pastel, sparkles, kaomoji); `classic` for the original orange look. `/theme` switches and remembers it in `~/.config/ember/theme` |
| `EMBER_TEST_CMD` | detected: `cargo test`, `go test ./...`, `pytest`, or `npm test` |

A starter file with every variable is in [`.env.example`](../.env.example).

## Run Nemotron on your own hardware

ember speaks the OpenAI-compatible API, so it can drive a model served from your own machine, such as a Jetson dev
kit or a workstation GPU. Start an OpenAI-compatible server (for example llama.cpp's `llama-server`, vLLM, or an
NVIDIA NIM) with a Nemotron model that supports tool calling, then point ember at it:

```bash
EMBER_BASE_URL=http://jetson.local:8000/v1 \
EMBER_MODEL_CODER=<model name your server reports> \
ember
```

No Nebius key is needed for a local server. Pick a model that fits the device's memory, and set
`EMBER_MODEL_FAST` / `EMBER_MODEL_REASONER` too if you use `/fix` or `/review`.
