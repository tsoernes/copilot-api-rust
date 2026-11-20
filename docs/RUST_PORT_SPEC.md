# Rust port: user-specified requirements

## Current requirements

- Create a Rust fork of <https://github.com/caozhiyuan/copilot-api>, commit incrementally, create a public GitHub fork, and push it. (User, 2026-11-20.)
- Consider low memory usage, but do not over-optimize. (User, 2026-11-20.)

## Implementation notes (not user requirements)

- The fork starts from upstream commit `350dc3d992faa9f539bd9b81de14cf036477c377` (version 2.9.16); the upstream MIT license and history are retained. The Rust implementation lives in `rust/` while parity is developed.
- Favor streaming responses, bounded incoming requests and ordinary maintainable code. Record compatibility and testing limitations in `rust/README.md` rather than claiming upstream parity.
- Upstream includes multiple providers, desktop and plugin integrations, administration, OAuth, streaming translations and many routes. These are not automatically implemented by a Copilot-only proxy.
