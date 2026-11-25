# Rust port: user-specified requirements

## Current requirements

- Create a Rust fork of <https://github.com/caozhiyuan/copilot-api>, commit incrementally, create a public GitHub fork, and push it. (User, 2026-11-20.)
- Make the repository contain only the Rust version by removing the original non-Rust project, and clearly state that this implementation is compatible with only a limited scope of the original `copilot-api`. (User, 2026-11-21.)
- Rewrite the published Git history so it contains only Rust-project history, not the original non-Rust project. (User, 2026-11-21.)
- Place the Rust crate at the repository root instead of keeping it in a `rust/` subdirectory. (User, 2026-11-21.)
- Consider low memory usage, but do not over-optimize. (User, 2026-11-20.)
- Implement usage monitoring like the original project. (User, 2026-11-20.)
- Implement GitHub sign-in if it is not already present. (User, 2026-11-20.)
- Implement a rich terminal UI for monitoring. Start the server and dashboard together; prioritize an overview, quota, models and recent requests, with a five-second refresh. (User, 2026-11-20.)
- Put a monitor screenshot preview on the repository front page and link it to a gallery containing all monitor panes. (User, 2026-11-21.)
- Describe the basic Copilot API functionality this Rust implementation does provide in the README, and explain Rust as a memory-usage motivation. (User, 2026-11-21.)
- Refresh GitHub quota every 60 seconds and on manual refresh, while local statistics refresh every five seconds. (User, 2026-11-20.)
- Stop the server started by the dashboard when the dashboard is quit. (User, 2026-11-20.)
- Provide an automatically started user service for the API, and make `monitor` attach to an already-running instance rather than starting a duplicate. (User, 2026-11-22.)
- The terminal request display should provide the most fine-grained token breakdown possible, rather than only a single total. The user reports that many requests appear incorrectly as cancelled; investigate and correct the classification. (User, 2026-11-20.)
- Log upstream Copilot transport failures, header timeouts, and non-success HTTP statuses without logging request/response bodies or credentials. Record failed inference attempts as error outcomes even when no usage was reported, so the monitor does not omit or label them completed. (User, 2026-11-23.)
- In the terminal Requests view, show a safe provider error code and message for failed rows. Keep the complete error inline when it fits in two lines; open a scrollable centered popup on Enter for longer errors. Keep `d` as the request/token-detail toggle. Persist only bounded, sanitized error metadata, never raw request or response bodies. (User, 2026-11-25.)
- The terminal monitor should show session statistics and request, token and cost counters per minute, hour and day, globally and per session. Global usage should persist across sessions and server restarts. (User, 2026-11-20.)
- Avoid local request-size rejection so the remote Copilot API decides whether a valid oversized request is acceptable. (User, 2026-11-21.)
- (Superseded 2026-11-21) The earlier request was to reduce unnecessary large-request copies while preserving a 16 MiB request limit. The user later explicitly asked that oversized requests be left for the remote API to judge.
- The quota view should show used, allowance and remaining credit amounts, not only percentages. Make changing “Usage billed to” between personal and organization accounts and between organizations possible if supported. (User, 2026-11-20.)
- Estimate time to quota exhaustion using recent usage. Store quota-balance samples in the durable usage database so estimates can use multi-day history and survive restarts. In the monitor, show the actual sample span and count used for each ETA. Compare projected runout with the reported quota refresh in the quota text, including an estimated local time when runout is projected before refresh. Keep refresh comparison out of the boxed usage bar; add a separate boxed bar beneath it to show elapsed days in the quota period. Both bars need strong contrast between their labels and colored progress fill. (User, 2026-11-22; persistence, longer history, visible ETA basis and refresh comparison clarified 2026-11-23 and 2026-11-25; keep refresh status out of the bar and add period-days bar, 2026-11-25; emphasize label contrast and box period bar, 2026-11-25.)
- When GitHub supplies both rounded `remaining` and precise `quota_remaining`, show one consolidated Remaining value, preferring the precise reported quantity. (User, 2026-11-21.)
- If supported, provide optional automatic billing-account switching when a stream reports monthly quota exhaustion. It must be off by default, restricted to a user-selected ordered allowlist, and automatically retry the failed request once after switching. (User, 2026-11-20.)
- Display token and other count quantities using thousands/millions-style units (K, M, etc.). (User, 2026-11-20.)
- Add rolling 7-day and 30-day counters to the terminal monitor. (User, 2026-11-20.)
- Display rolling reported counts in a table with period, request, token, and estimated-cost columns. (User, 2026-11-22.)
- Draw a visible box around the quota percent-used bar. (User, 2026-11-22.)
- The Models pane should show both per-token model unit prices and credits/AIU spent. Add a separate pane listing all Copilot-provided models and their information, with optional price sorting. Default to the highest available output-tier price first and keep missing or unverified prices visible last; offer other sort orders. (User, 2026-11-20; tier clarified 2026-11-21.)
- The available-model catalog should be filterable by model name. (User, 2026-11-21.)
- The model catalog sorting options should include long-context pricing. (User, 2026-11-21.)
- Add GitHub Copilot Fusion API support. (User, 2026-11-23.)
- Automatically refresh Codex's model catalog on API startup, and provide a CLI command and terminal-monitor action for manual refresh. Build from the models served by the local gateway; preserve exact IDs, include text models even if `/responses` is absent from advertised endpoints, sort GPT models first by descending numeric version, and do not send inference requests. Store the profile under `~/.codex/model-catalogs/copilot-api/`. (User request, 2026-11-25; catalog ordering/filter preferences previously clarified 2026-11-06.)
- One-time rollout instruction: trigger the Codex catalog refresh after implementation, then restart the API service. (User, 2026-11-25.)

## Open questions

- Should compact counts be the default with an exact-count toggle, always remain exact with suffixes, or be compact only? (Asked 2026-11-20.)
- The observed GitHub web flow uses an undocumented `api.github.com/cmc_internal/api/agents/tasks` task API. A read-only models-catalog GET was verified on 2026-11-24 with the stored GitHub OAuth Bearer token, but the HAR did not preserve Authorization for task/environment routes; task-route authentication and Fusion Standard/High/Max selection remain unverified. Experimental pass-through routes are implemented with those gaps documented; do not treat them as a stable gateway contract. See [observed Fusion API notes](COPILOT_FUSION_API.md). (Investigation, 2026-11-24.)

## Implementation notes (not user requirements)

- The Rust source is based on upstream commit `350dc3d992faa9f539bd9b81de14cf036477c377` (version 2.9.16), and the upstream MIT license is retained. The published history was rewritten to keep Rust-project changes and remove the original non-Rust project. The only gateway implementation in the repository is the crate at the root; non-Rust source, desktop, plugin and build projects were removed as requested. This remains a limited-scope compatibility implementation, not a claim of full parity.
- Favor streaming responses and ordinary maintainable code. Request JSON is buffered for inspection/normalization without a gateway-level size cap, so large bodies can use substantial local RAM even though Copilot decides whether to accept them. Record compatibility and testing limitations in `RUST_GUIDE.md` rather than claiming upstream parity.
- Copilot Responses requests may contain encrypted reasoning state issued by another model/provider. The gateway removes opaque reasoning, encrypted content items, and compaction state as complete entries before forwarding, while preserving visible reasoning summaries and ordinary content. It never leaves a required encrypted field missing from a retained wrapper and never logs the removed value. This is transport compatibility normalization, not retained reasoning.
- MAI Code model entries generated without an installed Codex template are explicitly configured for Code Mode with freeform patch calls, matching the terminal tool protocol those models use. Other generated model entries retain ordinary template/fallback behavior.
- The quota endpoint reports the next reset but not the current period's start. The terminal UI estimates period progress as a monthly cycle beginning one calendar month before that reset, labels it approximate, and should not present this inferred start as provider-reported fact.
- Upstream includes multiple providers, desktop and plugin integrations, administration, OAuth, streaming translations and many routes. These are not automatically implemented by a Copilot-only proxy.
- Automatic payer switching and retry remain unresolved: no documented payer-selection API was found, and replay after forwarding part of a stream is not transparent. Opening GitHub settings is a manual alternative, not completion of the automatic requirement.
- Output sorting uses the highest independently verified published output price across available tiers and marks incomplete comparisons; the catalog also offers a dedicated long-context-output sort.
- GitHub's official [October 28, 2026 changelog](https://github.blog/changelog/2026-10-28-route-and-orchestrate-models-with-github-copilot-fusion/) describes Fusion as model routing/orchestration with Single, Cascade and Critique patterns; the [November 17, 2026 update](https://github.blog/changelog/2026-11-17-github-copilot-fusion-updates-more-availability-benchmarks-and-models/) adds client availability and model-pool updates. These posts do not define a standalone REST API contract. Do not equate Fusion with a single model ID or invent a request schema without further evidence.
