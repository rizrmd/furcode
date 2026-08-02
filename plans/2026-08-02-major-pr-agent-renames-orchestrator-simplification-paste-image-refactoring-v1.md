# Major PR: Agent Renames, Orchestrator Simplification, and Paste/Image Refactoring

## Objective

Execute a large-scale refactoring PR that:
1. Renames built-in agent IDs: `coding` → `forge`, `planning` → `muse`, `researching` → `sage`
2. Simplifies orchestrator state management and control flow
3. Refactors paste/image/clipboard handling for maintainability

This plan breaks the work into discoverable, verifiable batches with clear ownership boundaries between rename, simplification, and refactoring work.

## Implementation Plan

### Phase 1: Agent Renames (Forge/Muse/Sage)

- [ ] Audit all hard-coded built-in agent ID strings (`coding`, `planning`, `researching`) across Rust source, templates, markdown, config, and schema files, and record every match location and its surrounding usage pattern.
- [ ] Rename built-in agent IDs in `crates/furcode_repo/src/agent.rs` default agent initialization to use `forge`, `muse`, and `sage`.
- [ ] Update built-in agent markdown files in `crates/furcode_repo/src/agents/` (`coding.md`, `planning.md`, `researching.md`) to use new IDs and internal cross-references.
- [ ] Rename slash-command aliases and display labels in `crates/furcode_main/src/model.rs` from `coding`/`planning`/`researching` to `forge`/`muse`/`sage` while preserving backward-compatible aliases where needed.
- [ ] Update TUI header/render mappings in `crates/furcode_main/src/tui/render.rs` for agent color and label lookup tables.
- [ ] Update prompt rendering in `crates/furcode_main/src/prompt.rs` to map new agent IDs to their display labels/colors.
- [ ] Update `crates/furcode_main/src/ui.rs` references that mention old agent IDs in comments, headers, or refresh logic.
- [ ] Update `crates/furcode_app/src/agent_executor.rs` chrome/label mappings for built-in agents.
- [ ] Update `crates/furcode_config/src/config.rs` documentation strings that mention `researching`/`coding`/`planning`.
- [ ] Update `furcode.schema.json` descriptions that mention old agent names.
- [ ] Update any README/docs/template files that reference old agent IDs.
- [ ] Add or update tests that assert on old agent ID strings to assert on the new names instead.

### Phase 2: Orchestrator Simplification

- [ ] Review `crates/furcode_app/src/orch.rs` and identify redundant state flags or branching that can be collapsed without changing behavior.
- [ ] Simplify turn-completion/yield logic by consolidating `is_complete`, `should_yield`, and `emitted_interrupt` handling into clearer exit conditions.
- [ ] Extract repeated tool-call lifecycle bookkeeping in `execute_tool_calls` into smaller helpers to reduce nesting and improve readability.
- [ ] Remove any dead or legacy control-flow paths in the orchestrator that are no longer exercised by tests or UI callers.
- [ ] Update `crates/furcode_app/src/orch_spec/orch_runner.rs` if needed to match any orchestrator API/behavior changes from simplification.
- [ ] Add or update tests to cover the simplified control flow and guard against regressions.

### Phase 3: Paste/Image Refactoring

- [ ] Review `crates/furcode_main/src/paste.rs` and map current responsibilities: text path wrapping, image atom registry, clipboard probing, and PNG serialization.
- [ ] Identify coupling between image-paste state and editor/UI behavior that can be separated into focused modules/types.
- [ ] Refactor image-atom registry/expansion into a dedicated abstraction with a smaller public API surface.
- [ ] Simplify clipboard probing/saving paths by consolidating platform-specific branches where behavior is identical or trivially branchable.
- [ ] Ensure `expand_image_atoms`, `wrap_pasted_text`, and related helpers remain behaviorally equivalent; preserve existing tests and add missing edge-case coverage if gaps are found.
- [ ] Update all call sites in `crates/furcode_main/src/ui.rs` and TUI modules to use the refactored paste/image API.

### Phase 4: Cross-Cutting Verification

- [ ] Run targeted crate checks/tests for affected crates (`furcode_main`, `furcode_app`, `furcode_repo`, `furcode_config`).
- [ ] Search again for any remaining stale references to old agent IDs, old orchestrator patterns, or old paste module surface names.
- [ ] Verify slash-command parsing, agent switching, TUI header rendering, and image-paste expansion still behave correctly end-to-end at a smoke-test level.

## Verification Criteria

- All built-in agent definitions load with IDs `forge`, `muse`, and `sage` and no built-in agent path still requires `coding`/`planning`/`researching` to function.
- `/forge`, `/muse`, and `/sage` slash commands resolve correctly, and backward-compatible aliases remain intentionally preserved if documented.
- TUI header and prompt chrome show the expected built-in agent labels/colors after rename.
- Orchestrator test suite passes with the simplified control flow, and observable behavior for successful turns, tool failures, interrupts, and max-request limits is unchanged.
- Paste/image tests pass, image atoms still expand to attachment paths on submit, and clipboard image paste behavior is preserved.
- No remaining authoritative references to the old built-in agent IDs exist outside intentionally preserved compatibility shims.

## Potential Risks and Mitigations

1. **Missed rename surface in non-Rust assets**
   Mitigation: After Phase 1, run a repo-wide grep-style audit for the old ID literals and review schema/docs/templates before declaring rename complete.

2. **Orchestrator simplification changing edge-case timing or retries**
   Mitigation: Keep behavior parity as the primary constraint; simplify only after mapping existing branches to tests, and add regression tests before and after the change.

3. **Paste/image refactor breaking editor integrations**
   Mitigation: Preserve public function signatures used by UI/TUI callers where possible; introduce new internal types behind unchanged wrappers first, then migrate call sites incrementally.

4. **Large PR review fatigue from mixed concerns**
   Mitigation: Use this phased plan to separate rename, orchestrator, and paste/image commits so review can be scoped to one concern at a time.

## Alternative Approaches

1. **Incremental rename with deprecation shims**
   Rename built-in IDs but keep old IDs as aliases temporarily; trade-off is more code paths and a second cleanup PR later.

2. **Orchestrator rewrite instead of simplification**
   A full rewrite could modernize structure, but carries much higher regression risk; simplification keeps diff size and risk bounded.

3. **Paste/image extraction into a separate crate**
   This would improve reuse, but adds crate/API design overhead and is better saved for a follow-up once the current refactor stabilizes.
