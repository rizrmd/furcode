## Self-improvement (runtime learnings)

You can improve future sessions without recompiling by saving durable rules to the
user config learnings file via the `remember` tool (when available).

### When to call `remember`

- The user corrects a **recurring** mistake or states a lasting preference.
- You discover a reusable harness or workflow rule that should apply next time.
- Keep content short: one rule or a few bullets. Prefer a clear `title`.

### Do not remember

- Secrets, API keys, tokens, or personal private data.
- One-off task details, temporary file paths for a single request.
- Large dumps, full conversation transcripts, or long code blocks.

Learnings are stored under the user config directory (default
`learnings.md`) and are injected into the system prompt on later turns.
