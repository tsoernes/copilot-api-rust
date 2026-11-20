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

## Terminal monitor

After signing in, run `./rust/target/release/copilot-api-rust monitor` in an interactive terminal. This starts its own server and dashboard together; quitting the dashboard also stops that server. Use `--listen` and `COPILOT_API_KEY` as with `serve`; the monitor connects locally to its own listener, so it accepts loopback or wildcard listen addresses. The existing `serve` command runs without a TUI. Recent server errors are shown in the dashboard instead of being printed over it.

- `1`–`4` or Tab: overview, Copilot quota, per-model usage, recent requests.
- `p`: change period; `↑`/`↓` or `j`/`k`: scroll models or requests; `←`/`→`: change request page.
- `r`: refresh local usage and quota; Space: pause/resume automatic refresh; `q` or Ctrl-C: quit.
- Usage refreshes every five seconds; quota every 60 seconds and on manual refresh. Stale/error status is shown rather than treating missing data as zero.

The monitor displays upstream-reported, persisted usage: in-flight requests may not appear until they complete or are cancelled. Quota requires a working GitHub sign-in; costs are estimates, not bills. A terminal of at least 42 columns by 10 rows is needed for the full layout.

## Usage monitoring

Open the usage-viewer address printed by `serve`. This serves the original upstream dashboard with the current server as its default endpoint. It includes period selection, charts, model/session summaries and paginated events. Enter the configured API key in the dashboard if needed. The original viewer loads JavaScript and fonts from third-party CDNs and stores entered credentials in browser local storage; use it only in a trusted browser and network.

Copilot quota is fetched on demand from GitHub using the signed-in GitHub token. The API provides `/usage`, `/usage/github-copilot`, `/token-usage`, `/token-usage/daily`, `/token-usage/events` and `/token-usage/sessions`. These endpoints require the configured API key, if any. Quota results are not cached and may contain account information. Unsupported subscription providers return 404.

Token events are stored separately from upstream TypeScript data in `~/.local/share/copilot-api/rust-usage.sqlite3` (or `$COPILOT_API_HOME/rust-usage.sqlite3`); `COPILOT_API_RUST_USAGE_DB` overrides the file path. Newly created database files are owner-only on Unix. Existing upstream databases are not imported, and foreign/unknown database schemas are rejected. The database contains models, session IDs, truncated user agents, timestamps and token counts; protect it accordingly. No automatic retention or deletion is applied.

Only usage actually reported by upstream is recorded. Streaming bytes are forwarded unchanged; completed responses are saved before EOF is delivered, and observed usage from cancelled streams is saved asynchronously. Copilot AIU cost is an estimate based on upstream-reported nano-AIU, not a billing statement. Missing usage is not estimated. To bound memory, JSON responses over 4 MiB and SSE events over 256 KiB can lack monitoring data even though forwarding continues. A response that has not finished may not yet appear in the viewer.

The original multi-provider/admin usage, key isolation, external history import, and all upstream timing/accounting edge cases are not yet implemented. This port cannot report providers or routes it does not support. See [the upstream guide](../docs/guides/en/usage.md) for the original feature set; do not assume complete parity.

## Current scope and known gaps

- GitHub Copilot device login, token-file reuse, short-lived token exchange and refresh. Enterprise and OpenCode OAuth are not supported and are rejected rather than sent to the wrong service.
- Basic direct Copilot proxy routes: `/models`, `/chat/completions`, `/embeddings`, `/responses` and their `/v1` aliases, plus native `/v1/messages`. Upstream HTTP status and body are forwarded; response bodies, including SSE, are streamed. Individual Copilot models may not support all of these paths.
- Requests are limited to 16 MiB and parsed as JSON. Responses are not collected into a single buffer. This is a conservative memory design, not a measured or optimized memory target.
- Upstream connections and response headers have deadlines; there is currently no response-stream inactivity timeout.
- `anthropic.rs` contains an experimental partial Chat/Anthropic translation foundation; it is **not wired into the proxy**. Native messages pass-through requires a model supporting that endpoint.
- Not yet ported: provider routing and other providers, most model selection and translation logic, Responses WebSocket/continuations, count-tokens, images, videos, search, admin UI/configuration, desktop, plugins, and upstream operational features. Error handling, streaming semantics, API compatibility and model-specific behavior are not equivalent to upstream. Do not assume compatibility based only on the route names.
- Unit and mock tests cannot establish live Copilot compatibility. No live paid inference is implied by the test suite. See test output for current verification.

## Development

```fish
cargo fmt --manifest-path rust/Cargo.toml --check
cargo test --locked --manifest-path rust/Cargo.toml
cargo clippy --locked --manifest-path rust/Cargo.toml --all-targets -- -D warnings
```

User-specified requirements and baseline are recorded in [the port specification](../docs/RUST_PORT_SPEC.md).
