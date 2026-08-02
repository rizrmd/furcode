# Dead Code Warnings Under CI Strictness Plan

## Objective

Remove or justify all dead-code and related unused-code warnings so the workspace remains green under CI's strict linting posture, while preserving intentional dead-code paths and keeping the generated CI workflows consistent.

## Context

- CI enforces `RUSTFLAGS: '-Dwarnings'` in `.github/workflows/ci.yml:19-20`.
- The autofix workflow also denies warnings via `.github/workflows/autofix.yml:19-20` and `crates/furcode_ci/src/jobs/lint.rs:34-44`.
- The repo already contains broad `#[allow(dead_code)]` and `#[allow(unused)]` suppressions that can mask real cleanup work, notably:
  - `crates/furcode_json_repair/src/schema_coercion.rs:496-679`
  - `crates/furcode_main/src/porcelain.rs:32-57`
  - `crates/furcode_domain/src/merge.rs:18-54`
  - `crates/furcode_infra/src/auth/http/anthropic.rs:10-13`
  - `crates/furcode_infra/src/auth/mcp_token_storage.rs:69`
  - `crates/furcode_main/src/ui.rs:194`
  - `crates/furcode_repo/src/provider/anthropic.rs:929`
  - `crates/furcode_repo/src/provider/openai_responses/response.rs:33-67`
  - `crates/furcode_tracker/src/log.rs:63`
  - `crates/furcode_markdown_stream/src/renderer.rs:59-65`
  - `crates/furcode_services/src/tool_services/fs_patch.rs:66`
  - `crates/furcode_app/src/dto/google/request.rs:905`
  - `crates/furcode_app/src/dto/openai/request.rs:914`
  - `crates/furcode_app/src/dto/anthropic/transforms/remove_output_format.rs:52`
  - `crates/furcode_app/src/dto/anthropic/transforms/enforce_schema.rs:71-78`
  - `crates/furcode_domain/src/tools/definition/tool_definition.rs:87-94`

## Implementation Plan

- [ ] Audit current warning surface under strict linting
  - [ ] Run cargo/rustc checks with denied warnings to collect the exact warning set and categorize them by crate/file.
  - [ ] Separate true dead code from intentionally unused code paths and test-only/snapshot-only helpers.

- [ ] Remove or narrow blanket `#[allow(...)]` suppressions
  - [ ] Replace crate/file/module-wide `#[allow(unused)]` with narrower allowances only where behavior must be preserved.
  - [ ] Replace broad `#[allow(dead_code)]` blocks with `#[allow(dead_code)]` only on the specific item that needs it, if the item is intentionally unreferenced.

- [ ] Clean up intentional dead-code candidates
  - [ ] For items kept for snapshots, tests, feature flags, or future work, add explicit `#[allow(dead_code)]` or `#[cfg(...)]` gating directly on the item.
  - [ ] For items no longer needed, remove the code or convert to test-only definitions.

- [ ] Resolve unused imports, variables, and assignments
  - [ ] Use `cargo clippy` and compiler warnings to identify concrete unused symbols under `-Dwarnings`.
  - [ ] Remove unused imports and variables, or prefix unused bindings with `_` where appropriate.
  - [ ] Convert placeholder test functions from broad `#[allow(unused)]` to minimal test stubs or `#[cfg(test)]` gated helpers.

- [ ] Keep CI workflow generation consistent
  - [ ] Verify generated workflow files still declare `RUSTFLAGS: '-Dwarnings'` after any changes to workflow generation code.
  - [ ] Ensure the release path that appends extra `RUSTFLAGS` does not unintentionally override warning denial.

- [ ] Validate with workspace-wide checks
  - [ ] Run workspace-wide check and clippy passes using the same strict flags as CI.
  - [ ] Re-run targeted checks for each touched crate to catch regressions.

## Verification Criteria

- `cargo check --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` both complete without dead-code or unused-code warnings.
- `.github/workflows/ci.yml` and `.github/workflows/autofix.yml` still contain `RUSTFLAGS: '-Dwarnings'` or equivalent generated values.
- No crate-wide or file-wide `#[allow(unused)]` remains without an explicit documented justification adjacent to it.
- Every remaining `#[allow(dead_code)]` is attached to a specific item, not a broad enclosing block unless documented.

## Potential Risks and Mitigations

1. **Risk: Removing suppression exposes non-dead warnings unrelated to dead code.**
   Mitigation: Scope cleanup per crate/file and validate after each change instead of one big sweep.

2. **Risk: Test-only or snapshot-only helpers are accidentally deleted.**
   Mitigation: Audit usages in `tests/`, `benches/`, and snapshot outputs before removal, and prefer `#[cfg(test)]` or explicit `#[allow(dead_code)]` over deletion.

3. **Risk: CI workflow regeneration diverges from the intended strict warning policy.**
   Mitigation: Update the workflow generator source of truth, then regenerate committed workflow files and verify both contain the same warning policy.

4. **Risk: Release matrix adds conflicting `RUSTFLAGS` and drops warning denial.**
   Mitigation: Inspect release build steps that append flags and preserve denial by appending rather than replacing the warning-related flag when possible.

## Alternative Approaches

1. **Minimal surgical cleanup:** Keep existing broad `#[allow(dead_code)]` blocks, remove only true dead code unrelated to those blocks. This is lower risk but leaves technical debt in place.
2. **Batch refactor by crate:** Tackle one crate at a time, fixing warnings and removing broad allows in isolation. This improves reviewability but increases cycle time.
3. **Feature-gated cleanup:** Move speculative code behind Cargo features so unused paths are excluded unless explicitly enabled. This keeps code available while removing it from default warning sets.
