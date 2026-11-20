# Rust port: user-specified requirements

## Current requirements

- Create a Rust fork of <https://github.com/caozhiyuan/copilot-api>, commit incrementally, create a public GitHub fork, and push it. (User, 2026-11-20.)
- Consider low memory usage, but do not over-optimize. (User, 2026-11-20.)
- Implement usage monitoring like the original project. (User, 2026-11-20.)
- Implement GitHub sign-in if it is not already present. (User, 2026-11-20.)
- Implement a rich terminal UI for monitoring. Start the server and dashboard together; prioritize an overview, quota, models and recent requests, with a five-second refresh. (User, 2026-11-20.)
- Refresh GitHub quota every 60 seconds and on manual refresh, while local statistics refresh every five seconds. (User, 2026-11-20.)
- Stop the server started by the dashboard when the dashboard is quit. (User, 2026-11-20.)
- The terminal request display should provide the most fine-grained token breakdown possible, rather than only a single total. The user reports that many requests appear incorrectly as cancelled; investigate and correct the classification. (User, 2026-11-20.)

## Implementation notes (not user requirements)

- The fork starts from upstream commit `350dc3d992faa9f539bd9b81de14cf036477c377` (version 2.9.16); the upstream MIT license and history are retained. The Rust implementation lives in `rust/` while parity is developed.
- Favor streaming responses, bounded incoming requests and ordinary maintainable code. Record compatibility and testing limitations in `rust/README.md` rather than claiming upstream parity.
- Upstream includes multiple providers, desktop and plugin integrations, administration, OAuth, streaming translations and many routes. These are not automatically implemented by a Copilot-only proxy.
