# deepseek-search-mcp Rust implementation plan

## 分析与方案

### 现状与目标

- 仓库当前只有 Rust 2024 `Cargo.toml`、占位 `src/main.rs`、`.gitignore` 和 `DESIGN.md`，没有既有实现或提交。
- 按 `DESIGN.md` 实现一个独立的 stdio MCP server，固定暴露 `search` 和 `fetch` 两个工具，并调用 DeepSeek Search/Fetch API。
- 第一版不加入 provider trait、缓存、数据库、HTTP MCP transport、MCP SDK 或用户可配置 endpoint。

### 模块边界

- `src/main.rs`: 启动顺序、stderr tracing 初始化、组装依赖；不解析 DeepSeek JSON 或分支处理 MCP method。
- `src/config.rs`: 读取 `DEEPSEEK_API_KEY` 和固定运行常量，返回不可变 `Config`；key 只交给 HTTP client 持有。
- `src/error.rs`: 内部错误分类及安全的模型可见消息，禁止泄漏 key、Authorization、完整上游 body 或 debug dump。
- `src/deepseek.rs`: 共享 reqwest client、DeepSeek endpoint、请求/响应 serde 模型、bounded body、timeout、有限重试和取消；生产 endpoint 固定，测试仅通过构造器注入本地 mock URL。
- `src/tools.rs`: 工具 schema、纯参数校验、search/fetch 输出归一化和 UTF-8 安全截断；校验借用输入，跨 await 的请求类型拥有必要数据。
- `src/mcp.rs`: 一行一个 JSON-RPC 的 stdin/stdout 生命周期、initialize、tools/list、tools/call、notifications、request cancellation map 和串行 response writer；不认识 DeepSeek endpoint。

### 关键决策与不变量

1. 直接使用 `tokio`、`reqwest`（rustls/json）、`serde`、`serde_json`、`tracing`、`tracing-subscriber`、`url`、`tokio-util`；不引入 MCP SDK，避免锁定尚不需要的协议抽象。
2. HTTP client 用自有的不可变共享 reqwest `Client`，`DeepSeekClient` 可廉价 clone（内部共享 client/key），不在每次调用创建 client，也不为每条结果做无意义 clone。
3. 上游响应先以不超过 8 MiB 的 bounded bytes 读取，再解析；重试只针对 408/429/5xx 和明确临时 transport error，最多两次额外尝试，取消/超时不重试。
4. MCP tool 参数错误和上游失败都作为 `CallToolResult.isError=true`，协议错误才使用 JSON-RPC error response。stdout 只写完整、带换行且 flush 的 JSON；日志只写 stderr。
5. 每个 tools/call 拥有一个 `CancellationToken`，收到 `notifications/cancelled` 后取消对应任务；任务结束无论成功失败都移除 map 项。response writer 是唯一 stdout 写入者。
6. search 默认不返回正文，`need_content=true` 时限制整个 normalized search 输出到 2 MiB；fetch 输出限制到 2 MiB。所有截断按 UTF-8 字符边界完成并带明确标记。
7. URL 校验只允许 http/https、有 host、无 userinfo，最大 8192 字节；search 字符串按 Unicode 字符数校验，count 限制 1..=10。

### 测试策略

- 先为纯校验/归一化逻辑写单测，再接 HTTP 和 MCP orchestration。
- DeepSeek client 使用本地 mock HTTP server（测试依赖 `wiremock`），覆盖请求形状、认证、可选字段、响应协议、错误状态、重试、body 上限和取消；不访问外网、不需要真实 key。
- MCP 使用内存输入输出驱动 server，覆盖生命周期、schema、工具调用、非法参数不发 HTTP、并发 response id/行完整性、通知无 response、取消清理和 EOF。
- 最终执行 `cargo fmt --all -- --check`、`cargo test --all-targets`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo build --release`，并做无 key 启动检查。

### 拒绝的替代方案

- 不做通用 `Provider` trait：当前只有 DeepSeek 后端，trait 只会增加间接层和测试负担。
- 不使用第三方 MCP SDK：需要的 stdio JSON-RPC 面积小，直接实现可精确控制 stdout、取消和错误语义。
- 不使用缓存或持久化：设计明确要求一次性结果，缓存会引入漂移和额外生命周期。
- 不把 endpoint 做成用户配置：生产 endpoint 固定，测试 seam 只存在于代码构造器中。
- 不把 API key 放命令行或日志上下文：只从环境读取并在错误/响应中完全隐藏。

## 实施任务

### 任务 1：项目基线、配置和错误模型

文件：`Cargo.toml`、`Cargo.lock`、`src/main.rs`、`src/config.rs`、`src/error.rs`。

- 固定依赖版本，启用 tokio full、reqwest rustls/json、serde derive、serde_json、tracing、tracing-subscriber、url、tokio-util；测试使用 wiremock。
- 建立 Rust 2024 异步入口和 stderr tracing；配置读取 key，缺失或空白时 fail-fast，错误消息不包含 secret。
- 定义贯穿 HTTP、工具和 MCP 的错误分类及安全消息接口。
- 添加配置和错误安全单测。

验证：`cargo test --all-targets`、`cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`。

### 任务 2：DeepSeek HTTP client

文件：`src/deepseek.rs`，必要时补充 `src/config.rs`/`src/error.rs` 的最小接口。

- 先写 mock HTTP 测试，再实现 search/fetch request/response model。
- 实现必要 headers、共享 client、bounded response、timeout、有限重试、取消 select 和安全上游错误分类。
- 保证测试 endpoint 可替换但生产默认 URL 固定；只发送已提供的可选 search 字段。

验证：DeepSeek 模块测试、fmt、clippy。

### 任务 3：工具语义和输出归一化

文件：`src/tools.rs`。

- 先写边界单测，再实现 search/fetch 参数校验、两个固定工具 schema、结果归一化和 UTF-8 安全截断。
- 保持数值字段为数值，省略 search 空 content，fetch 空正文分类为上游协议/内容错误。
- 暴露给 MCP 的接口保持最小，不复制完整原始响应。

验证：工具模块测试、fmt、clippy。

### 任务 4：MCP stdio server

文件：`src/mcp.rs`，并接通 `src/main.rs`。

- 实现 JSON-RPC 解析/错误、initialize、notifications/initialized、tools/list、tools/call、unknown method/tool。
- 实现每调用独立 task、request id 到 cancellation token 的清理 map、串行 stdout writer 和 flush。
- 添加内存 wire 集成测试，覆盖协议顺序、错误、不发 HTTP、并发 id、通知、取消和 EOF。

验证：MCP 测试、fmt、clippy。

### 任务 5：集成文档和最终验证

文件：`README.md`，必要时调整前述源码测试。

- 编写构建、环境变量、ox stdio 配置、安全说明和人工 smoke test；链接 `DESIGN.md`，不重复完整设计。
- 运行完整 fmt、test、clippy 和 release build；确认无 key 启动失败、stdout 无污染。
- 完成全量 diff 的通用审查和 Rust 专项审查，修复所有问题后再落地。
