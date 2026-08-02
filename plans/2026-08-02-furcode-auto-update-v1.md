# Furcode Auto Update

## Objective

Harden Furcode’s existing auto-update behavior so startup checks and self-updates are reliable across install methods and platforms, with safer preflight checks, visible failure reporting, and release metadata that matches the published artifacts. The current updater already exists, but it is still Unix-centric, silent on failure, and assumes every install should run the remote shell installer.

## Implementation Plan

- [ ] 1. Audit the current update surface and version sources by tracing `on_update`, `execute_update_command`, build-time `APP_VERSION` injection, and runtime `VERSION` to map caller paths and identify mismatch risks
- [ ] 2. Add installer-aware update routing that detects whether Furcode was installed via shell script, Homebrew, npm, or native Windows binary and routes updates accordingly instead of always executing the remote installer
- [ ] 3. Implement Windows-compatible update handling by guarding or replacing the POSIX installer path on Windows, using a graceful fallback message or native replacement flow since Windows binaries are built but the updater assumes a Unix shell
- [ ] 4. Introduce preflight checks before update execution to validate write permissions, whether the binary is on PATH or locked, and whether the current user can replace the executable, surfacing actionable errors instead of failing silently
- [ ] 5. Improve update failure observability by replacing silent success returns and swallowed errors with structured reporting or visible warnings so users know when auto-update fails
- [ ] 6. Align version metadata with release artifacts by ensuring build-time injection, runtime `VERSION`, and `update_informer` source of truth stay synchronized and reflect the same GitHub release tags
- [ ] 7. Add artifact verification for updates by validating downloaded release artifacts with checksum or provenance metadata before installing, instead of blindly executing remote installer content
- [ ] 8. Verify end-to-end update behavior by testing `furcode update --no-confirm`, `/update`, and background startup update checks across Linux, macOS, Homebrew, npm, and Windows installs

## Verification Criteria

- `furcode update --no-confirm` completes without silent failures on Unix script installs
- `/update` in interactive mode triggers the same updater path and reports errors to the user
- On Windows, the update command exits gracefully with a clear message rather than attempting `curl | sh`
- Homebrew and npm installs either skip in-app auto-update or explicitly tell the user to use the package manager
- Release tag `vX.Y.Z` is reflected in runtime `VERSION` and `update_informer` detects it correctly
- Preflight permission or lock failures produce a visible error instead of returning success

## Potential Risks and Mitigations

1. Silent update failures leave users on outdated builds
   Mitigation: Replace fail-silent returns with user-visible warnings or structured error propagation

2. Shell-script updater bypasses package managers and can corrupt managed installs
   Mitigation: Detect install method and prefer package-manager guidance or native installer binaries before running a shell installer

3. Windows update path is missing and current updater is Unix-only
   Mitigation: Guard POSIX updater behind `cfg(unix)` and provide a documented fallback or native replacement flow for Windows

4. Version drift between build script, runtime tracker, and release tags
   Mitigation: Centralize version extraction so runtime `VERSION` always reflects `APP_VERSION` or the Cargo package version tied to GitHub releases

5. Executing remote shell content poses security risk
   Mitigation: Add checksum verification or switch to native binary replacement before executing remote scripts

## Alternative Approaches

1. Keep shell installer but add guards
   Continue using the remote shell installer but wrap it with installer detection, platform checks, and permission preflights. Pros: minimal UX change; Cons: still Unix-centric and remote-script risk remains.

2. Native binary replacement updater
   Download platform-specific release assets and replace the running executable atomically. Pros: cross-platform and no shell dependency; Cons: requires more binary handling, temp-file cleanup, and restart logic.

3. Package-manager-only updates
   Disable in-app auto-update for managed installs and direct users to `brew upgrade`, `npm update`, or `winget`. Pros: respects ecosystem conventions; Cons: weaker in-app update UX for script installs.
