# Copilot API Rust

这是一个 Rust 实现，旨在兼容
[`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api) 的**有限功能范围**。
它不是完整移植版，也不是可直接替换的实现：目前尚未实现 provider 路由、许多协议转换、
WebSocket、管理功能、插件和桌面应用。使用前请阅读
[兼容性说明](rust/README.md#current-scope-and-known-gaps)。

## 构建和运行

```fish
cargo build --locked --release --manifest-path rust/Cargo.toml
./rust/target/release/copilot-api-rust auth
./rust/target/release/copilot-api-rust monitor
```

监控命令会同时启动 API 服务和终端仪表板。服务默认监听 `127.0.0.1:4141`。
GitHub 设备登录凭据保存在当前用户的数据目录中；也可设置
`COPILOT_API_GITHUB_TOKEN`。需要有效的 GitHub Copilot 权限。请妥善保护凭据和用量数据，
不要将服务暴露给不可信网络。

当前 API 提供 `/models`、`/chat/completions`、`/embeddings`、`/responses` 及其 `/v1` 别名，
以及原生 `/v1/messages`。请求大小上限为 16 MiB。不同模型支持的路由可能不同。
内置 HTTP 用量页面和终端仪表板仅显示 Rust 服务实际报告的用量。

完整的用量、配额、模型目录、安全及兼容性说明请参阅
[Rust 实现指南](rust/README.md)。用户需求和范围决策记录在
[移植规格文档](docs/RUST_PORT_SPEC.md)。
