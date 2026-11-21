## Summary

<!-- Describe the Rust behavior change and link related issues. -->

## Verification

- [ ] `cargo fmt --manifest-path rust/Cargo.toml --check`
- [ ] `cargo test --locked --manifest-path rust/Cargo.toml`
- [ ] `cargo clippy --locked --manifest-path rust/Cargo.toml --all-targets -- -D warnings`

## Compatibility and security

<!-- Note any scope changes, protocol behavior, request limits, credentials, or data migrations. -->
