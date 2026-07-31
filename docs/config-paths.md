# Furcode config & path map

## Home base

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
| Cache / logs / snapshots | `$BASE/cache`, `$BASE/logs`, `$BASE/snapshots` | — |

## Project-local (cwd)

| Item | Path |
|------|------|
| Agents | `.furcode/agents/*.md` |
| Skills | `.furcode/skills/<name>/SKILL.md` |
| Commands | `.furcode/commands/*.md` |
| MCP | `.mcp.json` |
| AGENTS.md | `AGENTS.md` |
| Plans | `plans/` |

## Environment variables

| Variable | Role |
|----------|------|
| `FURCODE_CONFIG` | Override base directory |
| `FURCODE_*` / `FURCODE__*` | Config overrides via `config` crate prefix |
| `FURCODE_EDITOR` | External editor for config / prompt edit |
| `FURCODE_LOG` | Tracing filter |
| `FURCODE_TRACKER` | Analytics opt-out / control |
| `FURCODE_BANNER` | Optional custom banner text |
| `_FURCODE_TERM_*` | Terminal context for suggest/cmd |

## Cloud services (disabled by default)

Remote features that need `services_url` are **off**:

| Feature | Status |
|---------|--------|
| Semantic search (`SemSearch`) | Disabled |
| Workspace sync / init / status / query | Disabled |
| Account usage / user-info API | Disabled |
| Provider `furcode_services` | Not listed in built-ins |

Config:

```toml
services_enabled = false
services_url = ""
```

To re-enable (if you host a compatible backend):

```toml
services_enabled = true
services_url = "https://your-backend.example/"
```

## Defaults file

Embedded defaults: `crates/furcode_config/.furcode.toml`

Schema: `furcode.schema.json`
