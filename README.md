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

### User config directory

| Item | Path | Override |
|------|------|----------|
| Base directory | `~/.furcode` | `FURCODE_CONFIG=/custom/dir` |
| Legacy base (if exists) | `~/furcode` | — |
| Primary config | `$BASE/.furcode.toml` | — |
| Legacy JSON config | `$BASE/.config.json` | — |
| SQLite DB | `$BASE/.furcode.db` | — |
| Prompt history | `$BASE/.furcode_history` | `custom_history_path` in config |
| Credentials | `$BASE/.credentials.json` | — |
| Providers | `$BASE/provider.json` | — |
| Global agents | `$BASE/agents/` | — |
| Global skills | `$BASE/skills/` | — |
| Global commands | `$BASE/commands/` | — |
| Global AGENTS.md | `$BASE/AGENTS.md` | — |
| MCP user config | `$BASE/.mcp.json` | — |
| Self-improve learnings | `$BASE/learnings.md` | `self_improve.path` in config |
| Cache / logs / snapshots | `$BASE/cache`, `$BASE/logs`, `$BASE/snapshots` | — |

### Project-local (cwd)

| Item | Path |
|------|------|
| Agents | `.furcode/agents/*.md` |
| Skills | `.furcode/skills/<name>/SKILL.md` |
| Commands | `.furcode/commands/*.md` |
| MCP | `.mcp.json` |
| AGENTS.md | `AGENTS.md` |
| Plans | `plans/` |

### Environment variables

| Variable | Role |
|----------|------|
| `FURCODE_CONFIG` | Override base directory |
| `FURCODE_*` / `FURCODE__*` | Config overrides via `config` crate prefix |
| `FURCODE_EDITOR` | External editor for config / prompt edit |
| `FURCODE_LOG` | Tracing filter |
| `FURCODE_TRACKER` | Analytics opt-out / control |
| `FURCODE_BANNER` | Optional custom banner text |
| `_FURCODE_TERM_*` | Terminal context for suggest/cmd |

### Cloud services (disabled by default)

Remote features that need `services_url` are **off**:

```toml
services_enabled = false
services_url = ""
```

To re-enable (if you host a compatible backend):

```toml
services_enabled = true
services_url = "https://your-backend.example/"
```

### Defaults & schema

- Embedded defaults: `crates/furcode_config/.furcode.toml`
- JSON schema: `furcode.schema.json`

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
