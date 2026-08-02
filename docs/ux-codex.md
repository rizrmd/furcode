# Furcode UX direction (Codex-inspired)

## Goals

1. **Conversation first** — transcript is the main surface; chrome stays quiet.
2. **Simple composer** — `›` input, multi-line ok, no shell-prompt cosplay.
3. **Live tools** — keep tool titles (`● [time] Action path`) for trust.
4. **No shell host** — run as a standalone TUI process only (no zsh `:` hijack).

## Compared to the original Forge interactive UX

| Forge | Furcode |
|-------|---------|
| Large ASCII “Forge” banner | Compact `furcode vX` product line |
| Nerd-font folder/branch/agent icons | Plain text + muted separators |
| Multiline shell-like prompt | Context line + `›` composer |
| ZSH plugin encouragement box | Generic tip about slash commands |
| Right prompt with heavy symbols | Agent · tokens · cost · model |

## Compared to OpenAI Codex CLI (inspiration, not a clone)

We borrow:

- calm session header
- focus on messages over mascot art
- clear model/session status at the edge of the screen
- keyboard-first interrupt (`Esc`)

We keep the harness strengths:

- tool execution stream + markdown rendering stack
- multi-agent tools, MCP, compaction hooks
- provider/auth configuration surface

## Roadmap

- [x] Product header + `›` composer
- [x] Drop zsh integration surface
- [ ] Approval / sandbox modes closer to Codex (policy UX polish)
- [x] Frame TUI scaffold — ratatui layout + live composer
- [x] Wire chat stream into `TurnView`
- [x] Default frame TUI; remove legacy line-oriented interactive REPL
- [x] Rebrand slash command docs and default agent display name
- [x] Rename built-in agent IDs to `coding` / `planning` / `researching`
