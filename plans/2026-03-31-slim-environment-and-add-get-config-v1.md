# Slim Environment & Add `get_config` to `EnvironmentInfra`

## Objective

Reduce `Environment` to only the six fields that cannot be sourced from `FurcodeConfig` — `os`, `pid`, `cwd`, `home`, `shell`, `base_path` — and expose `FurcodeConfig` directly through a new `get_config` method on `EnvironmentInfra`. All service/app code that currently accesses configuration fields through `Environment` will instead call `infra.get_config()` and read the canonical `FurcodeConfig` fields directly.

---

## Implementation Plan

### Phase 1 — Slim `Environment` in `furcode_domain`

- [ ] Task 1. **Remove all `FurcodeConfig`-sourced fields from the `Environment` struct** (`crates/furcode_domain/src/env.rs:54-198`).

  The 37 fields below the `// --- Infrastructure-derived fields ---` comment block — including `retry_config`, `max_search_lines`, `fetch_truncation_limit`, `session`, `commit`, `suggest`, `is_restricted`, `tool_supported`, `temperature`, `top_p`, `top_k`, `max_tokens`, `max_tool_failure_per_turn`, `max_requests_per_turn`, `compact`, `updates`, and all remaining mapped fields — must be deleted. Only the six runtime-derived fields remain: `os`, `pid`, `cwd`, `home`, `shell`, `base_path`.

- [ ] Task 2. **Adjust `history_path()` to accept a `custom_path` parameter** (`crates/furcode_domain/src/env.rs:267-271`).

  `history_path()` currently reads `self.custom_history_path`, which originates from `FurcodeConfig`. Change its signature to `pub fn history_path(&self, custom_path: Option<&PathBuf>) -> PathBuf` so callers pass the value sourced from `FurcodeConfig` themselves. Update all call sites accordingly.

- [ ] Task 3. **Remove `apply_op()` from `Environment`** (`crates/furcode_domain/src/env.rs:222-261`).

  This method mutates `session`, `commit`, and `suggest` — all fields being removed. The mutation logic will move to the infra layer (see Phase 3). Delete `apply_op` and its `#[cfg(test)]` unit tests that exercise it via `fixture_env`.

- [ ] Task 4. **Remove the `SessionConfig` re-export from `Environment`'s imports if it becomes unused** (`crates/furcode_domain/src/env.rs:10-13`).

  After removing `session`, `commit`, `suggest` from `Environment`, check whether `SessionConfig`, `CommitConfig`, `SuggestConfig`, `RetryConfig`, `HttpConfig`, `MaxTokens`, `Temperature`, `TopK`, `TopP`, `Update`, and `Compact` are still needed in this file. Remove any that are now unreferenced.

- [ ] Task 5. **Remove `AutoDumpFormat` from `env.rs` if it is no longer referenced there**.

  `AutoDumpFormat` was used by the `auto_dump` field being removed. If it has no other use in `env.rs`, move its definition or re-export to the appropriate module, or leave it in `furcode_domain` as a standalone type — do not silently delete it if it is still needed elsewhere in the domain.

- [ ] Task 6. **Update the `fake::Dummy` derive on `Environment`**.

  With only six fields, the `#[derive(fake::Dummy)]` annotation and any `#[dummy(...)]` attribute overrides on removed fields must be cleaned up. Verify the derive still compiles.

---

### Phase 2 — Add `type Config` and `get_config` to `EnvironmentInfra`

- [ ] Task 7. **Add the `Config` associated type to `EnvironmentInfra`** (`crates/furcode_app/src/infra.rs:21-39`).

  Add `type Config;` as an associated type on the trait. Every service bound written as `F: EnvironmentInfra` will need the concrete type resolved through the `type Config` mechanism, so consider whether a `where Self::Config: ...` bound is necessary at the trait level (e.g. `Clone` and `Send` if callers cache the value).

- [ ] Task 8. **Add `fn get_config(&self) -> Self::Config` to `EnvironmentInfra`**.

  This method returns the full `FurcodeConfig` to any consumer that holds a reference to an `EnvironmentInfra` implementor. Add it alongside the existing `get_environment`, `get_env_var`, and `get_env_vars` methods.

- [ ] Task 9. **Add `furcode_config` as a direct dependency of `furcode_app`** (`crates/furcode_app/Cargo.toml`).

  `furcode_app` now references `FurcodeConfig` in the trait definition. Add the path dependency if it does not already exist.

---

### Phase 3 — Update `FurcodeEnvironmentInfra` in `furcode_infra`

- [ ] Task 10. **Implement `type Config = FurcodeConfig` and `get_config` on `FurcodeEnvironmentInfra`** (`crates/furcode_infra/src/env.rs:393-443`).

  `get_config` reads from the existing `Arc<Mutex<Option<FurcodeConfig>>>` cache, loading from disk on the first call — identical to how `get_environment` already works. Return the `FurcodeConfig` value (cloned from cache).

- [ ] Task 11. **Simplify `to_environment()` to populate only the six runtime fields** (`crates/furcode_infra/src/env.rs:120-179`).

  Remove the entire `// --- FurcodeConfig-mapped fields ---` section. `to_environment` should only construct `os`, `pid`, `cwd`, `home`, `shell`, and `base_path`.

- [ ] Task 12. **Delete `to_furcode_config()` and all its `from_*` helper conversion functions** (`crates/furcode_infra/src/env.rs:181-354`).

  These were needed to round-trip `Environment ↔ FurcodeConfig`. With `Environment` no longer carrying config data, the round-trip path is eliminated. Remove `to_furcode_config`, `from_retry_config`, `from_http_config`, `from_tls_version`, `from_tls_backend`, `from_auto_dump_format`, `from_update_frequency`, `from_update`, and `from_compact`.

- [ ] Task 13. **Rewrite `update_environment()` to operate on `FurcodeConfig` directly** (`crates/furcode_infra/src/env.rs:417-442`).

  The new flow:
  1. Load `FurcodeConfig` from disk via `ConfigReader` (defaults + global only, as today).
  2. Apply each `ConfigOperation` directly to the `FurcodeConfig` using a new free function `apply_config_op(fc: &mut FurcodeConfig, op: ConfigOperation)` defined in this file.
  3. Call `fc.write()` to persist.
  4. Invalidate the cache.

  The `apply_config_op` function replicates the `SetProvider`, `SetModel`, `SetCommitConfig`, and `SetSuggestConfig` mutation logic that was previously in `Environment::apply_op`, but targeting `FurcodeConfig`'s `session`, `commit`, and `suggest` fields of type `Option<ModelConfig>`.

- [ ] Task 14. **Delete the round-trip identity test** (`crates/furcode_infra/src/env.rs:483-509`).

  The test `test_furcode_config_environment_identity` verified that `fc → env → fc' → env'` preserved equality. Since `to_furcode_config` is removed, this test is invalid. Remove it and add a simpler test that confirms `get_config` returns the same values as were written via `update_environment`.

- [ ] Task 15. **Implement `type Config = FurcodeConfig` and `get_config` on `FurcodeInfra`** (`crates/furcode_infra/src/furcode_infra.rs:94-113`).

  Add the delegation: `fn get_config(&self) -> FurcodeConfig { self.config_infra.get_config() }`. Also add `type Config = FurcodeConfig;`.

- [ ] Task 16. **Update `FurcodeInfra::new()` to read config through `get_config()` rather than `get_environment()`** (`crates/furcode_infra/src/furcode_infra.rs:58-91`).

  `FurcodeInfra::new()` currently calls `config_infra.get_environment()` to extract `env.parallel_file_reads` and `env.service_url` for constructing `FurcodeDirectoryReaderService` and `FurcodeGrpcClient`. Change these to read from `config_infra.get_config()` using the canonical `FurcodeConfig` field names: `max_parallel_file_reads` and `services_url` (parsed to `Url`).

---

### Phase 4 — Update `MockInfra` in tests

- [ ] Task 17. **Add a `FurcodeConfig` field to `MockInfra`** (`crates/furcode_services/src/app_config.rs:148-278`).

  `MockInfra` currently holds `Arc<Mutex<Environment>>`. Add `config: Arc<Mutex<FurcodeConfig>>` alongside it, initialised from `FurcodeConfig::default()` or a faked value in `MockInfra::new()`.

- [ ] Task 18. **Implement `type Config = FurcodeConfig` and `get_config` on `MockInfra`**.

  Return a clone of the locked `FurcodeConfig` value.

- [ ] Task 19. **Rewrite `update_environment` on `MockInfra` to operate on `FurcodeConfig`**.

  Replace the inline `Environment`-mutation logic with the same `apply_config_op` semantics (on `FurcodeConfig`), keeping `get_environment` returning a still-valid slim `Environment` built from the stored cwd/os/shell/etc.

- [ ] Task 20. **Update `MockInfra::get_environment()` to return a slim `Environment`**.

  Since `Environment` now has only six fields, construct it from static test values (`os`, `pid`, `cwd`, `home`, `shell`, `base_path`) rather than from a full fake.

- [ ] Task 21. **Update test assertions in `app_config.rs` that inspect `env.session` and `env.suggest`**.

  Tests currently call `fixture.get_environment()` and then inspect `env.session`. Change these assertions to call `fixture.get_config()` and inspect `config.session` on the returned `FurcodeConfig`. Update the field access paths to match `FurcodeConfig`'s field names (`session: Option<ModelConfig>`, `model_id: Option<String>`, etc.).

---

### Phase 5 — Update all service and app consumers

- [ ] Task 22. **Update `FurcodeAppConfigService` to read session config via `get_config()`** (`crates/furcode_services/src/app_config.rs:33-129`).

  Every call to `self.infra.get_environment()` followed by `env.session`, `env.commit`, or `env.suggest` must be replaced with `self.infra.get_config()` accessing `config.session`, `config.commit`, `config.suggest` of type `Option<ModelConfig>`. Adapt the field access to `ModelConfig`'s `provider_id: Option<String>` and `model_id: Option<String>` fields.

- [ ] Task 23. **Update `furcode_services/src/auth.rs` — `env.service_url` → `get_config().services_url`**.

  Parse `config.services_url` to `Url` at the call site as needed.

- [ ] Task 24. **Update `furcode_services/src/context_engine.rs` — `env.max_file_read_batch_size`** (two sites).

  Replace with `infra.get_config().max_file_read_batch_size`.

- [ ] Task 25. **Update `furcode_services/src/attachment.rs` — `env.max_search_result_bytes`**.

  Replace with `infra.get_config().max_search_result_bytes`.

- [ ] Task 26. **Update `furcode_app/src/tool_executor.rs` — `env.fetch_truncation_limit`**.

  Replace with `infra.get_config().max_fetch_chars`.

- [ ] Task 27. **Update `furcode_app/src/tool_registry.rs` — `env.tool_timeout` and `env.max_search_lines`**.

  Replace with `infra.get_config().tool_timeout_secs` and `infra.get_config().max_search_lines`.

- [ ] Task 28. **Update `furcode_app/src/changed_files.rs` — `env.parallel_file_reads`**.

  Replace with `infra.get_config().max_parallel_file_reads`.

- [ ] Task 29. **Update `furcode_app/src/app.rs` — `env.tool_supported`**.

  Replace with `infra.get_config().tool_supported`.

- [ ] Task 30. **Audit all remaining `get_environment()` call sites that access now-removed fields**.

  Run a codebase search for `get_environment()` followed by field access. For any that reference a field no longer on `Environment`, redirect to `get_config()`. Document any additional files not enumerated in Tasks 22–29.

- [ ] Task 31. **Add `furcode_config` as a direct dependency to any crate that now imports `FurcodeConfig` for field access** (e.g. `furcode_services`, `furcode_app`).

  For each crate modified in Tasks 22–29, verify `furcode_config` is listed in its `Cargo.toml` and add it if absent.

---

### Phase 6 — Update other infra consumers

- [ ] Task 32. **Update `FurcodeHttpInfra::new()` which currently accepts `Environment`** (`crates/furcode_infra/src/http.rs`).

  If `FurcodeHttpInfra` uses any `Environment` field that originated from `FurcodeConfig` (e.g. `env.http`, `env.retry_config`), change it to accept `FurcodeConfig` instead and read `config.http` and `config.retry` directly.

- [ ] Task 33. **Update `FurcodeCommandExecutorService::new()` which currently accepts `Environment`** (`crates/furcode_infra/src/executor.rs`).

  If it reads fields like `env.tool_timeout`, `env.shell`, or `env.cwd`, split the arguments: pass the slim `Environment` for `shell`/`cwd`, and pass the relevant `FurcodeConfig` fields for config-sourced values.

---

### Phase 7 — Remove dead code and verify

- [ ] Task 34. **Remove unused imports and dead code** across all modified files.

  After removing fields from `Environment`, many imports in `furcode_domain/src/env.rs`, `furcode_infra/src/env.rs`, and consumer files will become unused. Clean them all up.

- [ ] Task 35. **Remove the `AutoDumpFormat`-related and other now-unused conversion helpers** from `furcode_infra/src/env.rs`.

  The `to_auto_dump_format`, `to_compact`, `to_update`, `to_update_frequency`, `to_session_config`, `to_tls_version`, `to_tls_backend`, `to_http_config`, and `to_retry_config` functions may all become dead code. Remove any that are no longer called.

- [ ] Task 36. **Run `cargo insta test --accept`** to verify all tests pass and update any snapshot tests affected by the structural changes.

---

## Verification Criteria

- `Environment` contains exactly six fields: `os`, `pid`, `cwd`, `home`, `shell`, `base_path`. All path helper methods continue to compile and behave correctly.
- `EnvironmentInfra` has `type Config;` and `fn get_config(&self) -> Self::Config` as part of its public interface.
- Every `impl EnvironmentInfra` in the codebase (production: `FurcodeEnvironmentInfra`, `FurcodeInfra`; test: `MockInfra`) declares `type Config = FurcodeConfig`.
- No service or app code accesses a field on the return value of `get_environment()` that was formerly sourced from `FurcodeConfig`; all such accesses use `get_config()` instead.
- The `to_furcode_config()` function and all `from_*` conversion helpers are deleted from `furcode_infra/src/env.rs`.
- `update_environment()` operates directly on `FurcodeConfig` without involving `Environment` mutation.
- `apply_op()` no longer exists on `Environment`; mutation logic lives in a free function in `furcode_infra/src/env.rs`.
- `cargo insta test --accept` completes without errors.
- `cargo check` produces no warnings for unused imports or dead code in any modified file.

---

## Potential Risks and Mitigations

1. **`SessionConfig` vs `ModelConfig` type mismatch**  
   `Environment.session` was of type `Option<SessionConfig>` (domain type), while `FurcodeConfig.session` is `Option<ModelConfig>` (config type). Service code that destructures `SessionConfig` will need updating to work with `ModelConfig`'s field names. Additionally, `SessionConfig` may become an orphaned domain type if nothing else uses it — audit its usages before deciding to retain or remove it.  
   Mitigation: Before removing `SessionConfig`, run a workspace-wide search for all usages and migrate or remove them as part of Tasks 22 and 21.

2. **`cwd` used in `FurcodeCommandExecutorService` and `FurcodeHttpInfra`**  
   These types currently take a full `Environment` but may rely on a mix of infra fields (`cwd`, `shell`) and config fields (`tool_timeout`, `http`, `retry`). A naive split risks passing two objects where one worked before.  
   Mitigation: Tasks 32–33 explicitly address this. The slim `Environment` is passed for runtime fields; individual `FurcodeConfig` fields are passed separately, preserving the single-responsibility boundary at each constructor.

3. **`history_path()` signature change breaks all callers**  
   This is the only path helper that currently reads a `FurcodeConfig` field. Changing its signature is a mechanical but wide-reaching refactor.  
   Mitigation: Make the change in Task 2 early, then use compiler errors to find all call sites. Each call site must source `custom_history_path` from a locally available `FurcodeConfig` (obtained via `get_config()`).

4. **Crate dependency graph expansion**  
   `furcode_services` and `furcode_app` currently have no direct dependency on `furcode_config`. Adding one is architecturally significant — it means domain-level services now know about the infrastructure config format.  
   Mitigation: This is an explicit requirement of the task. Task 31 ensures all `Cargo.toml` files are updated consistently. If future work wants to reintroduce a domain abstraction layer, a `FurcodeConfig`-to-domain mapping can be re-introduced separately.

5. **`fake::Dummy` derive on slim `Environment` may fail**  
   The `fake` crate derives require all field types to implement `Dummy`. With six simple fields (`String`, `u32`, `PathBuf`, `Option<PathBuf>`), this should remain straightforward, but the `url::Url` `#[dummy(expr = ...)]` override that was on `service_url` is being removed.  
   Mitigation: Task 6 ensures the derive is re-validated after the structural change.

---

## Alternative Approaches

1. **Keep `Environment` as-is but add `get_config()` alongside `get_environment()`**: This is the minimal additive change — no consumer code is disrupted, and new code can opt into `get_config()`. Trade-off: `Environment` remains a redundant mirror of `FurcodeConfig`, the `to_environment`/`to_furcode_config` round-trip complexity is preserved, and the synchronisation risk (duplicate fields drifting out of step) persists.

2. **Embed `FurcodeConfig` as a field inside `Environment` rather than replacing the fields**: `Environment` could hold `pub config: FurcodeConfig` plus the six infra fields, allowing a gradual migration of callers. Trade-off: Transitional approach that still results in duplicated field access patterns and retains the `to_environment` mapping, just restructured. Cleaner than alternative 1 but more disruptive than the targeted plan above.
