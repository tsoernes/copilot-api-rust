# Copilot API Rust

这是一个 Rust 实现，旨在兼容
[`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api) 的**有限功能范围**。
它不是完整移植版，也不是可直接替换的实现：目前尚未实现 provider 路由、许多协议转换、
WebSocket、管理功能、插件和桌面应用。使用前请阅读
[兼容性说明](RUST_GUIDE.md#current-scope-and-known-gaps)。

## 构建和运行

```fish
cargo build --locked --release
./target/release/copilot-api-rust auth
./target/release/copilot-api-rust monitor
```

监控命令会同时启动 API 服务和终端仪表板。服务默认监听 `127.0.0.1:4141`。
GitHub 设备登录凭据保存在当前用户的数据目录中；也可设置
`COPILOT_API_GITHUB_TOKEN`。需要有效的 GitHub Copilot 权限。请妥善保护凭据和用量数据，
不要将服务暴露给不可信网络。

当前 API 提供 `/models`、`/chat/completions`、`/embeddings`、`/responses` 及其 `/v1` 别名，
以及原生 `/v1/messages`。请求大小上限为 16 MiB。不同模型支持的路由可能不同。
内置 HTTP 用量页面和终端仪表板仅显示 Rust 服务实际报告的用量。

完整的用量、配额、模型目录、安全及兼容性说明请参阅
[Rust 实现指南](RUST_GUIDE.md)。用户需求和范围决策记录在
[移植规格文档](docs/RUST_PORT_SPEC.md)。

## 当前提供的 Copilot 基础功能

- GitHub Copilot 设备登录、令牌复用和令牌刷新。
- 基础代理路由：`/models`、`/chat/completions`、`/embeddings`、`/responses` 及其 `/v1` 别名，并支持原生 `/v1/messages` 透传。上游状态码和响应体会转发；响应和 SSE 数据采用流式传输。具体模型支持的路由可能不同。
- HTTP 用量页面和终端仪表板；提供请求、令牌、模型、会话、配额和模型目录视图。已报告的用量保存在本地 SQLite 中，成本为估算值而非账单。配额视图会根据近期 GitHub 报告的剩余量下降趋势估算耗尽时间；重置或用量变化会使估算改变。
- 从 Copilot 的 `/models` 获取模型目录，支持按名称筛选和按价格排序；GitHub 参考价格会与实际计费区分显示。

以上是当前实现的 Copilot 基础子集，并不包含原项目的完整功能。

## 选择 Rust 与内存使用

采用 Rust 的一个动机是探索更低的运行时开销、更可控的内存分配，以及无需 Node.js 或 Bun 运行时的编译型程序。这是设计动机，不是测量结论：目前尚无对比基准证明本实现比原项目使用更少内存。

响应采用流式转发，不会整体缓冲。请求 JSON 仍需在本地完整缓冲、解析和规范化。网关不设请求体大小上限，由 Copilot 判断有效 JSON 是否过大；大型或并发请求仍可能消耗较多本地内存。
