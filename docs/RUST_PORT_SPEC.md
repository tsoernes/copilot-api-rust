# Rust port: user-specified requirements

## Current requirements

- Create a Rust fork of <https://github.com/caozhiyuan/copilot-api>, commit incrementally, create a public GitHub fork, and push it. (User, 2026-11-20.)
- Make the repository contain only the Rust version by removing the original non-Rust project, and clearly state that this implementation is compatible with only a limited scope of the original `copilot-api`. (User, 2026-11-21.)
- Rewrite the published Git history so it contains only Rust-project history, not the original non-Rust project. (User, 2026-11-21.)
- Consider low memory usage, but do not over-optimize. (User, 2026-11-20.)
- Implement usage monitoring like the original project. (User, 2026-11-20.)
- Implement GitHub sign-in if it is not already present. (User, 2026-11-20.)
- Implement a rich terminal UI for monitoring. Start the server and dashboard together; prioritize an overview, quota, models and recent requests, with a five-second refresh. (User, 2026-11-20.)
- Refresh GitHub quota every 60 seconds and on manual refresh, while local statistics refresh every five seconds. (User, 2026-11-20.)
- Stop the server started by the dashboard when the dashboard is quit. (User, 2026-11-20.)
- The terminal request display should provide the most fine-grained token breakdown possible, rather than only a single total. The user reports that many requests appear incorrectly as cancelled; investigate and correct the classification. (User, 2026-11-20.)
- The terminal monitor should show session statistics and request, token and cost counters per minute, hour and day, globally and per session. Global usage should persist across sessions and server restarts. (User, 2026-11-20.)
- Avoid local request-size rejection so the remote Copilot API decides whether a valid oversized request is acceptable. (User, 2026-11-21.)
- (Superseded 2026-11-21) The earlier request was to reduce unnecessary large-request copies while preserving a 16 MiB request limit. The user later explicitly asked that oversized requests be left for the remote API to judge.
- The quota view should show used, allowance and remaining credit amounts, not only percentages. Make changing “Usage billed to” between personal and organization accounts and between organizations possible if supported. (User, 2026-11-20.)
- When GitHub supplies both rounded `remaining` and precise `quota_remaining`, show one consolidated Remaining value, preferring the precise reported quantity. (User, 2026-11-21.)
- If supported, provide optional automatic billing-account switching when a stream reports monthly quota exhaustion. It must be off by default, restricted to a user-selected ordered allowlist, and automatically retry the failed request once after switching. (User, 2026-11-20.)
- Display token and other count quantities using thousands/millions-style units (K, M, etc.). (User, 2026-11-20.)
- Add rolling 7-day and 30-day counters to the terminal monitor. (User, 2026-11-20.)
- The Models pane should show both per-token model unit prices and credits/AIU spent. Add a separate pane listing all Copilot-provided models and their information, with optional price sorting. Default to the highest available output-tier price first and keep missing or unverified prices visible last; offer other sort orders. (User, 2026-11-20; tier clarified 2026-11-21.)
- The available-model catalog should be filterable by model name. (User, 2026-11-21.)
- The model catalog sorting options should include long-context pricing. (User, 2026-11-21.)

## Open questions

- Should compact counts be the default with an exact-count toggle, always remain exact with suffixes, or be compact only? (Asked 2026-11-20.)

## Implementation notes (not user requirements)

- The Rust source is based on upstream commit `350dc3d992faa9f539bd9b81de14cf036477c377` (version 2.9.16), and the upstream MIT license is retained. The published history was rewritten to keep Rust-project changes and remove the original non-Rust project. The only gateway implementation in the repository is under `rust/`; non-Rust source, desktop, plugin and build projects were removed as requested. This remains a limited-scope compatibility implementation, not a claim of full parity.
- Favor streaming responses and ordinary maintainable code. Request JSON is buffered for inspection/normalization without a gateway-level size cap, so large bodies can use substantial local RAM even though Copilot decides whether to accept them. Record compatibility and testing limitations in `rust/README.md` rather than claiming upstream parity.
- Upstream includes multiple providers, desktop and plugin integrations, administration, OAuth, streaming translations and many routes. These are not automatically implemented by a Copilot-only proxy.
- Automatic payer switching and retry remain unresolved: no documented payer-selection API was found, and replay after forwarding part of a stream is not transparent. Opening GitHub settings is a manual alternative, not completion of the automatic requirement.
- Output sorting uses the highest independently verified published output price across available tiers and marks incomplete comparisons; the catalog also offers a dedicated long-context-output sort.
