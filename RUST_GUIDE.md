# Copilot API Rust

This Rust project is intended to be compatible with a **limited scope** of
[caozhiyuan/copilot-api](https://github.com/caozhiyuan/copilot-api), based on
upstream commit `350dc3d992faa9f539bd9b81de14cf036477c377` (2.9.16). The original
non-Rust implementation is not part of this repository's current source tree.
This is not a complete port or a drop-in replacement; see
[Current scope and known gaps](#current-scope-and-known-gaps). The upstream
[MIT license](LICENSE) and relevant [notices](NOTICE.md) are retained.

## Build and run

```fish
cargo build --locked --release
./target/release/copilot-api-rust auth
./target/release/copilot-api-rust serve
```

`auth` uses the GitHub device flow and saves the GitHub token with owner-only permissions on Unix. Alternatively set `COPILOT_API_GITHUB_TOKEN`; existing upstream credential files are read, and `COPILOT_API_HOME` can select the data directory. An active Copilot entitlement is required. Do not share token files or logs.

The server defaults to `127.0.0.1:4141`. Configure `COPILOT_API_KEY` to require client `Authorization: Bearer ...` or `x-api-key` and to permit a non-loopback listen address, e.g. `serve --listen 0.0.0.0:4141`. The server does **not** provide TLS: use a trusted TLS-terminating reverse proxy for remote access. The health root requires no key. Browser Origin requests are rejected; without a key, Host must be loopback. Core inference POST routes require `application/json`. Do not expose the service directly on an untrusted network.

## Experimental Fusion routes

The gateway exposes read-only `GET /experimental/fusion/models`, `GET /experimental/fusion/tasks`, task detail/events, and environment detail/connect routes, plus the explicit side-effecting `POST /experimental/fusion/tasks`. Every route requires a configured `COPILOT_API_KEY`; client credentials and cookies are never forwarded. The proxy uses fixed GitHub hosts/paths and the stored GitHub OAuth Bearer token with the observed `copilot-web` / `copilot-cloud-web` integration headers. Task creation accepts a JSON object and forwards its bytes with the observed `text/plain;charset=UTF-8` content type. Responses preserve upstream status and stream; bodies and credentials are not logged.

This is an experimental pass-through for undocumented internal endpoints, not a supported Copilot contract. The model-catalog GET authentication was verified read-only. Task-route authentication, including task creation, remains unverified; no live task-create POST was sent. Fusion Standard/High/Max selection was not present in the observed body, so the proxy does not invent or inject an effort value. See [the observed Fusion API notes](docs/COPILOT_FUSION_API.md).

## systemd user service

The repository includes [`systemd/copilot-api-rust.service`](systemd/copilot-api-rust.service). To install and enable it for this checkout, run these commands from the repository root:

```fish
mkdir -p $HOME/.local/bin $HOME/.config/systemd/user
ln -sfn (pwd)/target/release/copilot-api-rust $HOME/.local/bin/copilot-api-rust
install -m 0644 systemd/copilot-api-rust.service $HOME/.config/systemd/user/copilot-api-rust.service
systemctl --user daemon-reload
systemctl --user enable --now copilot-api-rust.service
systemctl --user status copilot-api-rust.service
```

The user service starts the API at login and restarts it after failures. It runs as your user, binds to loopback by default, and logs to the user journal. To stop and disable automatic startup, run `systemctl --user disable --now copilot-api-rust.service`. To inspect logs, use `journalctl --user -u copilot-api-rust.service`.

If the service uses `COPILOT_API_KEY`, place it in `$HOME/.config/copilot-api-rust/environment` with owner-only permissions and provide the same key to the `monitor` command. The service file reads that environment file if it exists. No key is required for the default loopback-only setup.

## Terminal monitor

After signing in, run `./target/release/copilot-api-rust monitor` in an interactive terminal. If a matching server is already listening at the requested address, the dashboard attaches to it without starting another listener. Quitting an attached dashboard leaves the external server running. If no server exists, `monitor` starts its own server and stops it when the dashboard exits. Use `--listen` and `COPILOT_API_KEY` as with `serve`; the monitor uses loopback to reach its server. The `serve` command runs without a TUI. Recent server errors are shown in the dashboard instead of being printed over it.

- `1`–`6` or Tab: overview, Copilot quota, per-model usage, recent requests, sessions and the available-model catalog.
- `p`: change period; `↑`/`↓` or `j`/`k`: select/scroll models, requests or sessions; `←`/`→`: change request/session page. In Requests, `d` opens or closes the selected request's detailed token breakdown; scroll within it with `↑`/`↓`.
- `r`: refresh local usage and quota; Space: pause/resume automatic refresh; `u`: toggle compact K/M/B/T counts and exact counts; `q` or Ctrl-C: quit. `~` marks a rounded compact value. This toggle does not change quota balances or estimated currency amounts.
- In Quota, `b` then `y` requests opening GitHub's [Copilot settings](https://github.com/settings/copilot/features), where you can choose an available payer. This only opens the page; the gateway does not change billing settings.
- In Models, `d` opens the selected model's reference unit prices and reported AIU spend. In Catalog, `/` filters model name, ID, or vendor; type text, Enter to keep, Backspace to edit, or Esc to clear. `s` cycles highest-output, long-context-output, default input, cache-read, cache-write and name sorting; `o` reverses direction; `↑`/`↓` or `j`/`k` selects a model, Home/End jumps, and PgUp/PgDn scrolls its details. By default, the highest available published output-tier reference price sorts first. Input and cache sorts use the default tier; long-context output sorts by its published long-context quote; unknown prices sort last. A partial-tier marker indicates that another reported tier lacks a published reference price.
- Usage refreshes every five seconds; quota every 60 seconds and on manual refresh. Stale/error status is shown rather than treating missing data as zero.

The monitor displays persisted request events and upstream-reported usage. In-flight requests may not appear until they complete, are cancelled, or fail. Overview presents rolling 1-minute, 1-hour, 24-hour, 7-day and 30-day request, token and estimated-cost counts in a table; the selected session shows the same windows scoped to that session. Failed inference attempts can appear as error events with zero token/cost values when no usage was reported; request counts include these events, while token and cost totals never invent missing usage. The rows persist across restarts in the Rust usage database. Usage from the original gateway database is not imported. Session grouping uses client-provided IDs when available; otherwise each request uses its own ID. Earlier rows saved without a session ID cannot be reconstructed reliably.

Token subcounts are shown only when the provider reported them; older rows lack newly captured details. These subcounts can overlap and should not be added together. The terminal stores only bounded numeric token subcounts, not raw request or response content. Protocol completion observed upstream is recorded as completed even if the client closes before HTTP EOF; this does not prove delivery to the client. A reported incomplete response is shown separately. Existing cancellation labels remain unchanged and may have been classified incorrectly before this fix.

Quota requires a working GitHub sign-in. Copilot estimated cost converts the provider-reported AIU amount and does not apply an independent per-model price table; it is not a bill. A terminal of at least 42 columns by 10 rows is needed for the full layout, and some detailed panels require more space.

For GitHub organization/enterprise usage, [GitHub describes AI credits as model- and token-dependent](https://docs.github.com/en/copilot/concepts/billing-and-usage/organizations-and-enterprises/billing). The quota panel shows the provider's separately reported allowance and remaining values; for token-based billing their unit is marked unconfirmed. When there is enough terminal space, the used-percent meter is shown inside a box. The runout estimate uses GitHub-reported balance samples persisted in the Rust usage database, with at most one latest sample per five-minute bucket and a trailing seven-day lookback. When an ETA is available, the monitor shows the actual span and number of samples used for that estimate; it does not imply the full seven days were available. An estimate requires at least three samples spanning ten minutes and a real decrease; otherwise it reports that it is collecting samples, or that there is no drawdown. Samples are recorded when quota is fetched (the monitor refreshes it every 60 seconds); there is no separate polling loop, so estimates may need time to build. The history survives server and monitor restarts, but resets and top-ups clear or start a separate trend. An account/payer change can be indistinguishable if GitHub reports the same quota-window metadata, so treat an ETA as stale after switching. The extrapolation assumes the observed rate continues and can be invalidated by resets, top-ups, or changing usage. The Copilot quota snapshot can differ from GitHub's billing page, so check the latter for authoritative billing information.

The catalog is fetched from the signed-in Copilot account's `/models` endpoint at startup, every 60 seconds and on `r`; it retains entries even when prices or capabilities are missing. Unit prices labeled **GitHub reference credits / 1M** come from a separately bundled [public pricing snapshot](https://docs.github.com/en/copilot/reference/copilot-billing/models-and-pricing), retrieved 2026-11-20; one AI credit is $0.01 under that reference. Reference matching uses exact provider name and vendor, supported tier/threshold and date checks. It is not a guarantee of your account's actual charges. Dated promotions expire conservatively; ambiguous or outdated (over 30 days) references become unavailable. Raw `/models` price values are separately labeled unverified and are never treated as credits or used for price sorting. The selected-period AIU spend shown beside them is provider-reported and may be partial; it is not a unit price.

The checked-in price reference is a dated snapshot from GitHub's public documentation. It is separate from `/models` data, and raw model prices are not assumed to be credits.

Automatic payer switching and retry are **not implemented**: GitHub's documented API does not expose a payer-selection operation, and switching after part of a streamed response has reached the client cannot be transparently replayed. GitHub's settings page is the authoritative place to inspect or change the payer. Its billing usage can differ from the gateway's provider quota snapshot, so the latter must not be treated as an authoritative billing balance.

## HTTP usage monitoring

Open the usage-viewer address printed by `serve`. The Rust server embeds a static usage dashboard with the current server as its default endpoint. It includes period selection, charts, model/session summaries and paginated events. Enter the configured API key in the dashboard if needed. The viewer loads JavaScript and fonts from third-party CDNs and stores entered credentials in browser local storage; use it only in a trusted browser and network.

Copilot quota is fetched on demand from GitHub using the signed-in GitHub token. The API provides `/usage`, `/usage/github-copilot`, `/token-usage`, `/token-usage/daily`, `/token-usage/events` and `/token-usage/sessions`. These endpoints require the configured API key, if any. Current quota responses are not cached; only numeric balances and window metadata are persisted for runout estimates. Quota responses may contain account information. Unsupported subscription providers return 404.

Upstream transport failures, response-header timeouts, and non-success statuses are written to the service journal with a local request ID, endpoint, elapsed time, and safe error class/status. Transport error URLs are stripped; request/response bodies and credentials are not logged.

Token events are stored separately from the original gateway's data in `~/.local/share/copilot-api/rust-usage.sqlite3` (or `$COPILOT_API_HOME/rust-usage.sqlite3`); `COPILOT_API_RUST_USAGE_DB` overrides the file path. Newly created database files are owner-only on Unix. Existing upstream databases are not imported, and foreign/unknown database schemas are rejected. Additive migrations add a nullable token-details column and a quota-snapshot table to existing Rust databases without changing historical rows or the schema version; older Rust binaries can still read and write request usage. The database contains models, session IDs, truncated user agents, timestamps, token counts, and numeric quota balances keyed by quota-window metadata; protect it accordingly. Request usage events have no automatic retention. Quota snapshots older than seven days are pruned the next time quota is fetched.

Token counts and AIU values are recorded only when reported upstream. Failed inference attempts can also be recorded as zero-token error events so the monitor shows failures; they do not imply any usage or cost. Streaming bytes are forwarded unchanged; completed responses are saved before EOF is delivered, and observed usage from cancelled streams is saved asynchronously. Copilot AIU cost is an estimate based on upstream-reported nano-AIU, not a billing statement. Missing usage is not estimated. To bound memory, JSON responses over 4 MiB and SSE events over 256 KiB can lack monitoring data even though forwarding continues. A response that has not finished may not yet appear in the viewer.

The original multi-provider/admin usage, key isolation, external history import, and all upstream timing/accounting edge cases are not yet implemented. This port cannot report providers or routes it does not support. See the [upstream project](https://github.com/caozhiyuan/copilot-api) for its feature set; do not assume complete parity.

## Current scope and known gaps

- GitHub Copilot device login, token-file reuse, short-lived token exchange and refresh. Enterprise and OpenCode OAuth are not supported and are rejected rather than sent to the wrong service.
- Basic direct Copilot proxy routes: `/models`, `/chat/completions`, `/embeddings`, `/responses` and their `/v1` aliases, plus native `/v1/messages`. Upstream HTTP status and body are forwarded; response bodies, including SSE, are streamed. Individual Copilot models may not support all of these paths.
- Experimental Fusion pass-through routes are under `/experimental/fusion/`; their internal upstream task API is undocumented, and task auth and effort-selection semantics remain unverified. Only the read-only models-catalog authentication was verified. Task creation is an explicit POST that can start remote coding-agent work.
- Requests are parsed as JSON and forwarded without a gateway-level size cap, so Copilot decides whether an otherwise valid payload is too large. Parsing/normalization buffers the request in local memory; unusually large requests can use substantial RAM. Responses are streamed and are not collected into a single buffer.
- Upstream connections and response headers have deadlines; there is currently no response-stream inactivity timeout.
- `anthropic.rs` contains an experimental partial Chat/Anthropic translation foundation; it is **not wired into the proxy**. Native messages pass-through requires a model supporting that endpoint.
- Not yet ported: provider routing and other providers, most model selection and translation logic, Responses WebSocket/continuations, count-tokens, images, videos, search, admin UI/configuration, desktop, plugins, and upstream operational features. Error handling, streaming semantics, API compatibility and model-specific behavior are not equivalent to upstream. Do not assume compatibility based only on the route names.
- Unit and mock tests cannot establish live Copilot compatibility. No live paid inference is implied by the test suite. See test output for current verification.

## Development

```fish
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

User-specified requirements and baseline are recorded in [the port specification](docs/RUST_PORT_SPEC.md).
