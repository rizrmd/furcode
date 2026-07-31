# Furcode

Coding agent for the terminal.

Furcode adopts the **agent harness** (orchestration, tools, providers,
conversation/compaction, MCP) and uses a **Codex-inspired** interactive UX.
ZSH shell integration is **not** included.

## Layout

```text
furcode/
├── crates/           # Rust workspace (engine + CLI)
│   ├── furcode_*     # Harness crates
│   └── furcode_main  # Binary: `furcode`
├── templates/        # System / tool prompt templates (furcode-*.md)
├── docs/
└── …
```

## Quick start

```bash
cd furcode
cargo run -p furcode_main --bin furcode

# one-shot
cargo run -p furcode_main --bin furcode -- -p "explain the orchestrator"
```

## Config & paths

| Kind | Path |
|------|------|
| Base dir | `~/.furcode` (override: `FURCODE_CONFIG`) |
| Config | `~/.furcode/.furcode.toml` |
| Database | `~/.furcode/.furcode.db` |
| History | `~/.furcode/.furcode_history` |
| Project agents/skills/commands | `.furcode/agents`, `.furcode/skills`, `.furcode/commands` |
| Env prefix | `FURCODE_*` |

Legacy sources for `furcode config migrate`:

1. `~/furcode` → `~/.furcode`
2. `~/.forge` → `~/.furcode`

## UX sketch (Codex-inspired)

```text
furcode v0.1.0
coding agent · terminal session

  project · main
  › refactor auth middleware

● [14:32:01] Read  src/auth.rs
● [14:32:02] Patch src/auth.rs

  … markdown answer …

  project · main
  › _
                         FURCODE  12.4k  $0.08  claude-sonnet-4
```

## Explicit non-goals (for now)

- ZSH plugin / theme / doctor / rprompt
- Full Electron or IDE UI
- Renaming wire-level gRPC (`forge.v1` / `ForgeService`) used by remote context APIs

## Reference

Upstream dump: [`../forge-reference`](../forge-reference).
See also: [`docs/config-paths.md`](docs/config-paths.md), [`docs/ux-codex.md`](docs/ux-codex.md).
