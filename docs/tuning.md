# Tuning for Nemotron on Token Factory

These choices come from measuring the models on Token Factory, not from defaults.

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
  and the prompt tells the model to batch; in testing it read related files together and grounded answers in more
  of the code at the same cost.
- **Prompt-cache friendly.** Token Factory caches repeated prompt prefixes (4,512 of 4,533 tokens on a repeat
  request), billed at a lower rate. ember keeps the system prompt and the tool list fixed for each turn, so the
  growing conversation is re-sent as a stable prefix, and `/usage` shows how much input came from the cache (up to
  99% on warm runs).
