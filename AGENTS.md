# Repository guidance

## Scope

- The implementation is the Rust gateway under `rust/`; the embedded usage viewer is a runtime asset.
- Preserve the explicitly limited compatibility scope. Do not claim full parity with upstream `copilot-api`.
- Update `docs/RUST_PORT_SPEC.md` when the user changes requirements. Keep user requirements distinct from implementation choices and open questions.

## Build and verification

Run from the repository root:

```fish
cargo fmt --manifest-path rust/Cargo.toml --check
cargo test --locked --manifest-path rust/Cargo.toml
cargo clippy --locked --manifest-path rust/Cargo.toml --all-targets -- -D warnings
```

Run focused Rust tests while iterating, then run the full suite before committing behavior changes. For documentation-only edits, inspect links and the diff; Rust tests are not required.

## Rust conventions and security

- Follow `rustfmt`, use explicit error handling, and keep request/response behavior covered by tests.
- Avoid logging request bodies, authorization headers, GitHub tokens, or other credentials.
- Keep network access, input sizes, and persistence bounded; document compatibility and memory tradeoffs honestly.
- Never commit tokens, local credentials, build output, or user data.

## Commits

Use short imperative Conventional Commit subjects, such as `feat(monitor): add catalog sorting`.
