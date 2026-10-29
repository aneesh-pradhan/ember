# Tuning for Nemotron on Token Factory

These choices come from measuring the models on Token Factory, not from defaults.

- **Reasoning only where it pays.** `enable_thinking: false` turns reasoning off on every Nemotron 3 model, while a
  `/no_think` system prompt is ignored, and NVIDIA's `low_effort` template switch (used by `--think low`) only
  shortens it reliably on Super (461 → 4 reasoning tokens, 4.0 s → 2.0 s on a coding question). ember turns
  reasoning off for mechanical drafting (commit messages, branch names, PR text): on a real diff that took commit
  drafts from 2.0–2.6 s to 1.0–1.5 s with 3–4× fewer output tokens and equally good messages. It stays **on**
  where accuracy matters: Nano with thinking off answered "is 91 prime?" wrongly, so the `/fix` judge keeps it, and
  Super VL describes screenshots just as accurately either way, so it keeps its default too. For the interactive
  agent, on, low and off all took about 4–6 s on an exploration task (time goes to round trips, not thinking), and
  `off` started narrating its steps into the answer, so the default is `on`.
- **Batched reads.** Nemotron Super makes one tool call per reply, even with `parallel_tool_calls` and an explicit
  hint, so every extra file costs a round trip that resends the conversation. `read_file` accepts a list of `paths`
  and the prompt tells the model to batch; in testing it read related files together and grounded answers in more
  of the code at the same cost.
- **Prompt-cache friendly.** Token Factory caches repeated prompt prefixes (4,512 of 4,533 tokens on a repeat
  request), billed at a lower rate. ember keeps the system prompt and the tool list fixed for each turn, so the
  growing conversation is re-sent as a stable prefix, and `/usage` shows how much input came from the cache (up to
  99% on warm runs).
- **Reasoning is kept between steps.** Within a task, ember sends each step's `reasoning_content` back to the model,
  so Super doesn't re-derive its plan after every tool result. On the coding benchmark below, this cut model calls
  from 8.3 to 6.2 per task, input tokens by a third and output tokens by almost half, at the same pass rate
  (14/15 vs 13/15). `EMBER_KEEP_REASONING=0` turns it off.

## Coding benchmark

`bench/run.py` gives ember a small bug or feature task on `examples/tinyledger` in print mode, then scores it with
hidden tests. The full matrix (5 tasks × 3 runs per config, Oct 28 2026, Token Factory prices) is in
`bench/results/2026-10-28-full.jsonl`:

| Config | Passed | Avg time | Model calls | Input tokens | Cached | Output tokens | $ / task |
|---|---|---|---|---|---|---|---|
| `super` (then-default) | 13/15 | 12.8 s | 8.3 | 28,229 | 75% | 1,286 | $0.0045 |
| `super-low` (`--think low`) | 14/15 | 15.5 s | 9.1 | 32,739 | 79% | 1,498 | $0.0050 |
| `super-keep` (now default) | 14/15 | 8.7 s | 6.2 | 19,003 | 64% | 710 | $0.0034 |
| `ultra` | 14/15 | 19.1 s | 9.4 | 35,685 | 0% | 1,565 | $0.0404 |
| `nano` | 9/15 | 61.3 s | 9.2 | 34,059 | 26% | 4,875 | $0.0028 |
| `lightning` | 15/15 | 19.1 s | 11.4 | 60,673 | 38% | 2,511 | $0.0036 |

What it changed:

- **`--think low` stays opt-in.** In the partial run it looked about 45% faster, but over the full matrix it was
  slightly slower and more expensive than `on`, so the interactive default stays `on`.
- **The 4,096-token think budget never binds here.** Super's output, reasoning included, averaged about 160 tokens per
  call, so the cap only guards against runaway reasoning.
- **Lightning is a strong cheap tier.** It passed every run, at about the cost of Super with kept reasoning, though it
  made more calls and used more input. It's being evaluated as the fast tier and `/fix` judge.
- **Ultra got no cache hits** (0% cached input on every run), which makes it about 9× the cost of Super per task.
  The cause is how Token Factory caches Ultra, not ember: it caches in 4,352-token blocks and never counts the last
  whole block (`cached = 4352 × (floor(prompt / 4352) − 1)`, matching every size from 7.8k to 39.7k tokens), so
  prompts under about 8.7k tokens never hit. Super caches almost the whole prefix from about 3k tokens. Ultra stays
  reserved for the two single-call `/fix` decisions, and `/model ultra` warns about the cost.
- **Almost every failure is `half_up`.** Its hidden test includes the float case `1.005 → 1.01`, which the task
  description doesn't mention. Only `Decimal(str(x))` passes it; `Decimal(x).quantize(...)` still rounds down.

The sample is small (3 runs per task), so differences of one pass are noise.
