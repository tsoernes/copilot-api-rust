# Copilot API Rust

A Rust implementation intended to be compatible with a **limited scope** of
[`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api). It is not
a complete port or a drop-in replacement: provider routing, many translations,
WebSockets, administration, plugins, and desktop features are not implemented.
See [the compatibility notes](RUST_GUIDE.md#current-scope-and-known-gaps)
before using it.

[![Overview of the Rust terminal monitor — synthetic sample data](assets/screenshots/overview.png)](docs/monitor-gallery.md)

[View the screenshot gallery of all six monitor panes](docs/monitor-gallery.md) · synthetic sample data

## What this provides from Copilot API

- GitHub Copilot device sign-in, token reuse, and token refresh.
- A basic proxy for `/models`, `/chat/completions`, `/embeddings`, and `/responses`, with `/v1` aliases, plus native `/v1/messages` pass-through. Upstream status codes and bodies are forwarded; response and SSE bodies are streamed. Model/route support depends on Copilot.
- Local usage monitoring through both an HTTP viewer and a terminal dashboard. The monitor includes request, token, model, session, quota, and catalog views; reported usage is stored locally in SQLite and cost figures are estimates, not bills.
- A model catalog fetched from Copilot’s `/models` endpoint, with name filtering, price sorting, and separately identified GitHub reference prices.

This is the basic Copilot-focused subset currently implemented; it does not provide the original project's complete feature set.

## Build and run

```fish
cargo build --locked --release
./target/release/copilot-api-rust auth
./target/release/copilot-api-rust monitor
```

The monitor starts the API server and terminal dashboard together. The server
listens on `127.0.0.1:4141` by default. GitHub device sign-in stores credentials
in the user data directory; alternatively set `COPILOT_API_GITHUB_TOKEN`.
An active GitHub Copilot entitlement is required. Protect credentials and
usage data; do not expose the service to an untrusted network.

## Why Rust, and memory use

Rust was chosen partly to explore lower runtime overhead, tighter control of
allocations, and a compiled executable that does not require a Node.js or Bun
runtime. This is a motivation, not a measured result: no comparative memory
benchmark has established that this implementation uses less RAM than the
original.

Responses are streamed instead of collected into one buffer. Request JSON is
still buffered, parsed, and normalized before forwarding. There is no
gateway-level request-size cap, so Copilot decides whether valid JSON is too
large; a large or concurrent request can still consume substantial local RAM.

For all current usage, quota, catalog, security, and compatibility details, see
the [Rust implementation guide](RUST_GUIDE.md). User requirements and scope
decisions are recorded in [the port specification](docs/RUST_PORT_SPEC.md).
