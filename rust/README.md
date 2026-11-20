# copilot-api Rust port (early, incomplete)

This is an incremental port of [caozhiyuan/copilot-api](https://github.com/caozhiyuan/copilot-api), based on upstream commit `350dc3d992faa9f539bd9b81de14cf036477c377` (2.9.16). Original source, history, [MIT license](../LICENSE) and [notices](../NOTICE.md) are retained. The root README describes the upstream TypeScript gateway, not this implementation.

## Build and run

```fish
cargo build --locked --release --manifest-path rust/Cargo.toml
./rust/target/release/copilot-api-rust auth
./rust/target/release/copilot-api-rust serve
```

`auth` uses the GitHub device flow and saves the GitHub token with owner-only permissions on Unix. Alternatively set `COPILOT_API_GITHUB_TOKEN`; existing upstream credential files are read, and `COPILOT_API_HOME` can select the data directory. An active Copilot entitlement is required. Do not share token files or logs.

The server defaults to `127.0.0.1:4141`. Configure `COPILOT_API_KEY` to require client `Authorization: Bearer ...` or `x-api-key` and to permit a non-loopback listen address, e.g. `serve --listen 0.0.0.0:4141`. The server does **not** provide TLS: use a trusted TLS-terminating reverse proxy for remote access. The health root requires no key. Browser Origin requests are rejected; without a key, Host must be loopback. POST requests require `application/json`. Do not expose the service directly on an untrusted network.

## Current scope and known gaps

- GitHub Copilot device login, token-file reuse, short-lived token exchange and refresh. Enterprise and OpenCode OAuth are not supported and are rejected rather than sent to the wrong service.
- Basic direct Copilot proxy routes: `/models`, `/chat/completions`, `/embeddings`, `/responses` and their `/v1` aliases, plus native `/v1/messages`. Upstream HTTP status and body are forwarded; response bodies, including SSE, are streamed. Individual Copilot models may not support all of these paths.
- Requests are limited to 16 MiB and parsed as JSON. Responses are not collected into a single buffer. This is a conservative memory design, not a measured or optimized memory target.
- Upstream connections and response headers have deadlines; there is currently no response-stream inactivity timeout.
- `anthropic.rs` contains an experimental partial Chat/Anthropic translation foundation; it is **not wired into the proxy**. Native messages pass-through requires a model supporting that endpoint.
- Not yet ported: provider routing and other providers, most model selection and translation logic, Responses WebSocket/continuations, count-tokens, images, videos, search, quotas, usage, admin UI/configuration, desktop, plugins, and upstream operational features. Error handling, streaming semantics, API compatibility and model-specific behavior are not equivalent to upstream. Do not assume compatibility based only on the route names.
- Unit and mock tests cannot establish live Copilot compatibility. No live paid inference is implied by the test suite. See test output for current verification.

## Development

```fish
cargo fmt --manifest-path rust/Cargo.toml --check
cargo test --locked --manifest-path rust/Cargo.toml
cargo clippy --locked --manifest-path rust/Cargo.toml --all-targets -- -D warnings
```

User-specified requirements and baseline are recorded in [the port specification](../docs/RUST_PORT_SPEC.md).
