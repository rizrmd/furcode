# Incremental Dependency Update Plan

## Objective

Safely advance five dependencies—`rmcp`, `gh-workflow`, `sysinfo`, `termimad`, and `handlebars`—in small, compile-safe steps while preserving existing API usage in the workspace.

## Implementation Plan

- [ ] 1. Update workspace manifest to bump `handlebars` from `6.4.0` to `6.4.1` (`Cargo.toml:56`) since lockfile evidence shows this version already resolvable and usage is limited to basic `Handlebars::new()`, `set_strict_mode`, `register_escape_fn`, and `register_helper` APIs.
- [ ] 2. Update workspace manifest to bump `sysinfo` from `0.38.3` to `0.38.4` (`Cargo.toml:93`) because lockfile shows `0.38.4` exists and `furcode_tracker` only uses `sysinfo::System` (`crates/furcode_tracker/src/dispatch.rs:9`), a conservative patch-compatible surface.
- [ ] 3. Keep `termimad` at `0.34.1` for now (`Cargo.toml:95`) because no newer version was confirmed in local lock/metadata evidence and the crate is used for `MadSkin`, `CompoundStyle`, `LineStyle`, and `term_text` rendering (`crates/furcode_display/src/markdown.rs:5-6,30-39,59`), which makes it a higher-risk candidate if changed without verified newer semver.
- [ ] 4. Keep `gh-workflow` at `0.8.1` for now (`Cargo.toml:52`) because local evidence only shows `0.8.1`/`gh-workflow-macros 0.8.1` in the lock and no confirmed newer target; bump only after confirming a newer semver release or valid `-pre` candidate.
- [ ] 5. Move `rmcp` from `1.0.0` to a safer `1.x` minor/patch target (`Cargo.toml:130-135`) rather than the major-style `1.8.0` seen in lockfile; the crate is used across model/service/transport/auth paths (`crates/furcode_infra/src/mcp_client.rs:13-17,200,306,579` and `crates/furcode_infra/src/auth/mcp_token_storage.rs:10,101`), so a large jump needs a tighter changelog review.
- [ ] 6. Run `cargo check` across the workspace after manifest edits to validate that the selected target versions still compile without code changes.
- [ ] 7. If `cargo check` passes, run `cargo test` in the directly affected crates—`furcode_app`, `furcode_tracker`, `furcode_display`, `furcode_embed`, `furcode_services`, `furcode_infra`, and `furcode_ci`—to catch runtime-visible regressions from template, tracker, markdown, MCP, or workflow-generation behavior.

## Verification Criteria

- [ ] `cargo check -p furcode_app` succeeds with updated `handlebars`.
- [ ] `cargo check -p furcode_tracker` succeeds with updated `sysinfo`.
- [ ] `cargo check` for the full workspace succeeds after `rmcp` minor/patch target adjustment.
- [ ] `cargo test -p furcode_display` succeeds with current `termimad` unchanged.
- [ ] `cargo test -p furcode_ci` succeeds with current `gh-workflow` unchanged.
- [ ] `cargo test` in `furcode_infra` succeeds with conservative `rmcp` bump, with MCP client and auth token storage tests green.

## Potential Risks and Mitigations

1. `rmcp` auth APIs may have breaking changes even within `1.x`.
   Mitigation: Keep the bump to a conservative minor/patch range and verify auth-related paths before any feature changes.
2. `gh-workflow` may have newer versions that change macro exports or DSL surface.
   Mitigation: Hold at `0.8.1` until a specific newer version is identified and reviewed.
3. `termimad` 0.34.x may have rendering-output differences if updated later.
   Mitigation: Treat any future bump as a behavior-test change and validate markdown rendering tests first.

## Alternative Approaches

1. Conservative only: update only `handlebars` and `sysinfo` now, then revisit `rmcp`/`termimad`/`gh-workflow` later.
2. Aggressive: attempt `rmcp` `1.8.0` with accompanying code migration after confirming changelog compatibility, but this is higher risk and may break auth/service abstractions.
3. Lockfile-only first: update only `Cargo.lock` after bumping workspace versions to confirm transitive compatibility before committing manifest changes.
