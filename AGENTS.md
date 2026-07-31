# Agent Guidelines (Furcode)

This tree is **Furcode**: coding-agent harness + Codex-inspired UX.

## Source of truth

- **Engine / domain / services**: `furcode_*` crates.
- **Interactive UX**: `crates/furcode_main` — prefer Codex-style quiet chrome.
- **Reference dump**: `../forge-reference` (do not edit for features).
- **No ZSH integration**: do not reintroduce `shell-plugin`, `zsh` modules, or
  `:` prefix host shell commands.

## Error Management

- Use `anyhow::Result` for error handling in services and repositories.
- Create domain errors using `thiserror`.
- Never implement `From` for converting domain errors, manually convert them

## Writing Tests

- All tests should be written in three discrete steps:

  ```rust,ignore
  use pretty_assertions::assert_eq; // Always use pretty assertions

  fn test_foo() {
      let setup = ...; // Instantiate a fixture or setup for the test
      let actual = ...; // Execute the fixture to create an output
      let expected = ...; // Define a hand written expected result
      assert_eq!(actual, expected); // Assert that the actual result matches the expected result
  }
  ```

- Use `pretty_assertions` for better error messages.
- Test should always be written in the same file as the source code.
- Use `new`, Default and derive_setters::Setters for fixtures.

## Verification

1. Prefer crate-scoped checks:

   ```bash
   cargo check -p furcode_main
   cargo test -p furcode_main --bin furcode
   ```

2. **NEVER** run `cargo build --release` unless absolutely necessary.
3. For verification, use `cargo check`, `cargo test`, or debug `cargo build`.

## Documentation

- Always write Rust docs (`///`) for public items.
- Document parameters with `# Arguments` and errors with `# Errors` when useful.
- Do not include code examples in Rust docs.

## Service Implementation Guidelines

Follow clean-architecture rules:

- No service-to-service dependencies
- Services depend on infrastructure abstractions
- At most one generic type parameter for infrastructure
- Prefer `Arc<T>` and composed trait bounds over `Box<dyn …>`

## Git

- Prefer `Co-Authored-By: Furcode <noreply@furcode.dev>` for product commits.
