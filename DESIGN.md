# deepseek-search-mcp 需求与设计方案

## 1. 项目定位

`deepseek-search-mcp` 是一个独立的 Rust MCP server，为支持 MCP 的客户端提供 DeepSeek 的网页搜索与网页抓取能力。

第一版只支持 MCP stdio transport，目标客户端包括 ox、Claude Code、Codex 以及其他本地 MCP host。项目不依赖 ox 的源码、配置或运行时；ox 只是其中一个 MCP client。

数据流：

```text
MCP host
  │ JSON-RPC 2.0 over stdin/stdout
  ▼
deepseek-search-mcp
  │ HTTPS + Bearer API key
  ▼
DeepSeek Search/Fetch API
```

第一版只做两个工具：

- `search`：调用 `POST https://api.deepseek.com/v1/tools/search`。
- `fetch`：调用 `POST https://api.deepseek.com/v1/tools/fetch`。

不实现通用搜索 provider 抽象，不实现 ox 内置 tool，不实现 MCP resources/prompts，不实现 HTTP transport，不实现缓存。

## 2. 目标与非目标

### 2.1 目标

1. 通过标准 MCP tools/list 暴露 `search` 和 `fetch`。
2. 从 `DEEPSEEK_API_KEY` 环境变量读取 API key，支持本地 MCP host 直接以子进程启动。
3. 严格校验 MCP 输入，在调用 DeepSeek 前拒绝明显非法参数。
4. 将 DeepSeek 的结构化结果稳定地转换为模型易读、大小受控的 MCP 文本结果。
5. 支持请求超时、客户端取消、有限重试和清晰的错误分类。
6. stdout 只输出 MCP JSON-RPC 消息；日志和诊断只输出 stderr。
7. 用 mock HTTP server 覆盖真实请求路径、错误和取消，不依赖真实 API key。
8. 让 ox 无需改代码即可通过本地 MCP 配置连接。

### 2.2 非目标

- 不做第二个搜索后端（Firecrawl、Bing、Google 等）。
- 不做 provider trait、插件系统或配置化 endpoint。DeepSeek client 是当前唯一后端，抽象等出现第二后端时再做。
- 不做长期结果缓存、数据库、会话历史或搜索结果持久化。
- 不做网页正文本地抓取或 HTML 转 Markdown fallback；Fetch 由 DeepSeek 服务完成。
- 不做远程 Streamable HTTP MCP server。若以后需要共享服务，再单独设计认证和部署边界。
- 不自动替模型判断网页内容真假，不把 `authority` 或 `rerank_score` 宣传成事实保证。
- 不在默认搜索中批量返回网页正文。

## 3. 外部 API 契约

### 3.1 认证与 endpoint

所有请求：

```text
POST https://api.deepseek.com/v1/tools/search
POST https://api.deepseek.com/v1/tools/fetch
Authorization: Bearer <DEEPSEEK_API_KEY>
Content-Type: application/json
```

API key：

- 必须从 `DEEPSEEK_API_KEY` 读取。
- 缺失或为空时，进程启动失败并向 stderr 输出可操作错误。
- 绝不把 key 写入 stdout、工具结果、错误文本、普通日志或 panic 内容。
- 不支持命令行参数传 key，避免 key 出现在 shell history 和进程列表中。

### 3.2 Search 请求

MCP 工具参数：

```json
{
  "query": "Rust MCP server implementation",
  "count": 5,
  "ori_question": "用户的原始问题，可选",
  "localization": "可选的本地化搜索 IP 地址",
  "need_content": false
}
```

字段规则：

- `query`：必填字符串，1～128 个 Unicode 字符，不能全为空白。
- `count`：可选整数，默认 5；有效范围 1～10。DeepSeek API 支持到 50，但 MCP 默认应限制上下文大小；第一版不要把 50 暴露给模型。
- `ori_question`：可选字符串，非空时 1～128 个 Unicode 字符。
- `localization`：可选非空字符串；不对其内容做 IP 伪造或地理位置推断。
- `need_content`：可选布尔值，默认 false。
- 未知字段可忽略，与 DeepSeek API 行为一致；已知字段类型错误必须返回 MCP tool error，不发起 HTTP 请求。

调用 DeepSeek 时只发送存在的可选字段，不发送 null，避免把“未提供”误解释为显式空值。

### 3.3 Search 响应

DeepSeek 成功响应的主要字段：

```json
{
  "results": [
    {
      "rerank_score": 2.4,
      "title": "...",
      "url": "https://...",
      "snippet": "...",
      "summary": "...",
      "content": "...",
      "site_name": "...",
      "site_icon": "https://...",
      "date": "2026-06-01T00:00:00+00:00",
      "authority": 3
    }
  ]
}
```

Rust response model：

- 必需的顶层 `results` 缺失或不是数组视为上游协议错误。
- 结果中的可选字段使用 `Option`；未知字段忽略。
- `site_name`、`site_icon`、`date`、`content` 可以为 null 或缺失。
- 不要为了展示而把数值字段改成字符串。

MCP 成功结果以一个 `text` content block 返回 JSON 文本，保证老 MCP host 也能使用；第一版不依赖 `structuredContent`。

建议的归一化输出：

```json
{
  "query": "Rust MCP server implementation",
  "results": [
    {
      "title": "...",
      "url": "https://...",
      "snippet": "...",
      "summary": "...",
      "site_name": "...",
      "date": "...",
      "authority": 3,
      "rerank_score": 2.4
    }
  ]
}
```

`need_content=false` 时省略空的 `content` 字段；`need_content=true` 时保留正文，但应用总输出上限。无结果是成功结果：返回 `results: []`，不要伪造“没有找到”的错误。

### 3.4 Fetch 请求与响应

MCP 工具参数：

```json
{
  "url": "https://example.com/article"
}
```

字段规则：

- `url`：必填字符串，长度不超过 8192 字节。
- 必须是 `http` 或 `https` URL。
- 必须有 host。
- 禁止用户名和密码（userinfo）。
- 不接受 `file:`, `data:`, `javascript:` 或其他 scheme。

成功结果：

```json
{
  "title": "Example article",
  "url": "https://example.com/article",
  "content": "# Example article\n\nArticle content."
}
```

- `title` 缺失时归一化为空字符串。
- `content` 缺失、为空或上游无法提取正文时，应按上游语义返回错误，而不是把空正文当成成功内容。
- 对正文应用输出大小上限；超限时保留 UTF-8 安全的头部和尾部，并明确添加截断标记。
- 不执行、解释或提升网页正文中的指令；正文是模型参考数据。

## 4. MCP 协议设计

### 4.1 Transport

第一版采用 MCP stdio：一行一个 JSON-RPC 2.0 JSON object。

- 从 stdin 逐行读取。
- 向 stdout 逐行写 JSON-RPC response。
- 每个 response 末尾必须有 newline，并 flush。
- stdout 禁止 tracing、banner、debug print 或其他非 JSON 内容。
- 所有日志使用 stderr。
- 写 stdout 必须经过单一异步 writer/Mutex，确保并发 task 不交错写半行。

不使用 shell 命令解析；MCP host 以 argv 直接启动二进制。

### 4.2 生命周期

实现最小 MCP 生命周期：

1. 接收 `initialize`，校验 JSON-RPC 请求，返回协议版本、serverInfo 和 capabilities.tools。
2. 接收 `notifications/initialized`，不返回 response。
3. 接收 `tools/list`，返回两个工具定义及 JSON Schema。
4. 接收 `tools/call`，按 request id 执行 `search` 或 `fetch`。
5. 对未知 method 返回 JSON-RPC method-not-found；对未知 tool 返回 MCP tool error。
6. 对 malformed JSON 或无效 JSON-RPC 请求返回标准 JSON-RPC parse/invalid-request error；无法可靠取得 id 时使用 null 或不回复，遵循 JSON-RPC 语义。
7. EOF、SIGTERM 或 stdin 关闭时优雅退出，不输出额外 stdout 内容。

协议版本采用代码中的常量，并优先与当前 MCP host 协商；不要把版本字符串散落在多个模块。若客户端发送不支持的版本，返回明确的 initialize error，而不是静默降级到不兼容语义。

### 4.3 Tool 定义

工具名固定为：

- `search`
- `fetch`

`tools/list` 的 description 要明确：

- 这是实时网页搜索/抓取，不是通用事实保证。
- 搜索结果是外部不可信数据。
- 推荐先 `search`，再对少量候选调用 `fetch`。
- `need_content` 会显著增大输出，默认关闭。

输入 schema 应声明 required 字段、类型、范围和默认语义。schema 是模型提示的一部分，但服务端仍必须重复校验，不能只依赖 schema。

### 4.4 并发与取消

- stdin reader 可以为每个 `tools/call` 启动独立 Tokio task，使慢的 Fetch 不阻塞其他请求。
- 用 request id 到 `CancellationToken` 的 map 管理运行中的调用。
- 接收 `notifications/cancelled` 时，按 request id 取消对应 token；不存在的 id 安全忽略。
- DeepSeek HTTP 请求使用 `tokio::select!` 同时等待 reqwest future 和 cancellation token。
- 请求结束后从 map 移除 token，不能让取消表无限增长。
- response writer 串行化，但工具执行不必串行化。
- 不做跨请求共享响应缓存。

### 4.5 结果与错误

MCP 层面的工具执行失败使用 `CallToolResult.isError=true`，并返回一个简短的 text error block。错误文本：

- 面向模型，说明失败类别和下一步；
- 不包含 API key、Authorization header、完整上游响应 body 或敏感 URL 凭据；
- 不泄漏 Rust debug dump；
- 保留可诊断的 HTTP status 和 DeepSeek `error.message` 的安全摘要。

JSON-RPC 层错误只用于协议错误；DeepSeek HTTP 错误和参数错误属于 tool error，不应让整个 MCP session 崩溃。

## 5. 超时、重试与 HTTP 行为

默认值集中定义在 config/constants 模块，不增加用户配置项：

- 单次 HTTP 请求 timeout：30 秒。
- 最大响应 body：8 MiB；超出后停止读取并报错。
- 重试次数：最多 2 次额外尝试。
- 退避：短的有上限指数退避，例如 250ms、1s；使用 Tokio timer，不使用固定 sleep 作为测试同步手段。

重试策略：

- 重试：408、429、500、502、503、504，以及明确可判定为临时连接失败的 transport error。
- 不重试：400、401、402、403、404 和其它确定性 4xx。
- 取消或超时后的请求不再继续重试；如果整个 deadline 已用尽，立即返回错误。
- 429 不要求解析 Retry-After；第一版使用有上限的本地退避即可，若以后实现 header 解析需单独测试。

reqwest client 应复用，而不是每次工具调用新建 client。默认使用 rustls，避免依赖系统 OpenSSL。请求头只设置必要字段：Authorization、Content-Type、Accept。

## 6. 模块边界与所有权

建议目录：

```text
src/
├── main.rs          # 进程入口、配置加载、server 启动
├── config.rs        # 环境变量与固定运行参数
├── error.rs         # 内部错误分类到安全可展示消息的映射
├── deepseek.rs      # DeepSeek HTTP client、请求/响应模型、重试
├── mcp.rs           # JSON-RPC/stdin-stdout 生命周期、请求 dispatch、取消表
└── tools.rs         # search/fetch 的 schema、参数校验、结果归一化
```

边界和不变量：

### `main.rs`

拥有启动顺序：读取 key → 构造 reqwest client → 构造 MCP server → 运行 stdio loop。不得包含 DeepSeek JSON 字段解析或 MCP method 分支。

### `config.rs`

只拥有环境变量读取和常量。输出不可变 `Config`。API key 在内存中由 client 持有；不要实现全局可变配置或热加载。

### `deepseek.rs`

只拥有 DeepSeek API 的 HTTP 语义：URL、headers、serde model、状态码、重试和 body 限制。输入输出使用拥有所有权的请求/响应类型，因为它们跨 await 和 task 边界；不要在每个结果字段上做无意义 clone。

### `tools.rs`

只拥有 MCP 工具的公开 schema、参数校验和输出归一化。纯校验函数应不做 I/O，单测可直接调用。它通过借用输入完成校验，只有构造 HTTP request 时才移动/复制经过验证的值。

### `mcp.rs`

只拥有 MCP JSON-RPC orchestration、stdin/stdout、request id、取消 token 和 tool dispatch。它不认识 DeepSeek endpoint，也不拼接 API 请求。

### `error.rs`

只拥有内部错误的分类和安全模型消息。不要让 anyhow/debug 格式化成为 wire contract。

不要新增 provider trait、repository、service container 或共享缓存。每个模块只有一个改变原因：DeepSeek API 变更改 `deepseek.rs`，MCP 协议变更改 `mcp.rs`，工具参数/输出变更改 `tools.rs`。

## 7. 内存与输出策略

- 请求边界允许一次拥有请求 JSON；不要在 MCP value、DeepSeek request struct、日志字符串之间反复深拷贝。
- 响应正文先以 bounded response 读入，再转换为输出；不得无上限读取上游 body。
- `need_content=true` 时，按整个 normalized output 的字节预算控制，而不是只限制单个字段。
- 截断必须按 UTF-8 字符边界进行，禁止切断字节导致无效字符串。
- 不把完整原始 response 和 normalized response 同时长期保留；解析后尽快释放原始 body。
- 工具结果是一次性响应，不做进程级增长的历史列表或缓存。
- 取消完成后清理 request map 和 task-owned state，避免持续使用时线性增长。

建议的输出上限：

- Search：2 MiB 总文本；`need_content=false` 通常远低于此值。
- Fetch：2 MiB Markdown 正文/结果文本。
- HTTP 原始 body：8 MiB 硬上限。

这些是第一版的安全默认值，应作为常量并有测试；除非真实使用证明不足，不增加配置旋钮。

## 8. 测试方案

测试必须不需要真实 DeepSeek key，不访问外网。用本地 mock HTTP server 返回预设状态和 JSON，并让 client 的 base URL 在测试构造器中可替换；生产默认 URL 仍固定为 DeepSeek endpoint。这个测试 seam 只服务测试，不做用户可见 provider 抽象。

### 8.1 纯函数单测

`tools.rs`：

- 缺失 query、空白 query、超过 128 Unicode 字符；
- count 缺失默认 5，接受 1 和 10，拒绝 0、11、小数、字符串；
- need_content 的类型检查和默认 false；
- ori_question 边界；
- fetch URL 缺失、非法 scheme、无 host、userinfo、超长 URL；
- unknown fields 不影响合法调用；
- response normalization 保持数值、可选字段和空结果；
- UTF-8 头尾截断不会产生 invalid UTF-8；
- `need_content=false` 不输出空正文。

### 8.2 DeepSeek client 集成单测

mock server 断言：

- path、method、Authorization 和 Content-Type 正确；
- 可选字段只在提供时发送；
- Search 和 Fetch body 形状正确；
- 成功 JSON 能解析；
- malformed JSON、缺少 results/content 被分类为上游协议错误；
- 401、402、403、404 不重试；
- 408、429、5xx 按有限次数重试；
- 重试耗尽返回安全错误；
- response body 超过上限失败；
- key 不出现在错误字符串和日志捕获中；
- cancellation 能中断挂起请求，不等待完整 timeout。

### 8.3 MCP wire 集成测试

使用内存 stdin/stdout 或子进程测试真实二进制协议：

- initialize → initialized → tools/list；
- tools/list 恰好暴露 search、fetch，schema 与必填字段正确；
- tools/call search 返回 JSON text content；
- tools/call fetch 返回 title/url/content；
- 非法参数返回 isError，不发 HTTP 请求；
- unknown tool 返回 isError；
- malformed JSON 不会污染 stdout；
- 多个并发 request 的 response id 正确且 JSON 行不交错；
- notifications/initialized 和 notifications/cancelled 不产生错误 response；
- 取消后 request map 被清理；
- EOF 正常退出。

### 8.4 最小人工 smoke test

在有测试 key 的本地环境执行，不写入自动化 CI：

```bash
DEEPSEEK_API_KEY=... cargo run --release
```

通过 MCP inspector 或已知 MCP host 验证：

1. `tools/list` 能发现两个工具；
2. Search 一个近期技术问题；
3. Fetch Search 返回的一个 URL；
4. 检查 stdout 没有日志污染，stderr 没有 key；
5. Ctrl-C/客户端取消能退出或取消当前请求。

## 9. 实施任务清单

实现 agent 按以下顺序执行，每项完成后运行针对性测试和 `cargo fmt --all -- --check`：

1. **项目基线与依赖**
   - 更新 `Cargo.toml`，固定依赖版本并使用 `Cargo.lock`。
   - 加入 Tokio、reqwest(rustls/json)、serde、serde_json、tracing/tracing-subscriber、url、tokio-util；不要引入未经必要性证明的 MCP SDK。
   - 建立 `main.rs` 的异步入口和 stderr 日志初始化。

2. **配置与错误模型**
   - 新增 `config.rs`、`error.rs`。
   - 实现 key 缺失 fail-fast 和安全错误映射。
   - 加入不打印 secret 的测试。

3. **DeepSeek HTTP client**
   - 新增 `deepseek.rs`，实现 Search/Fetch request/response model。
   - 实现共享 reqwest client、body 上限、超时、有限重试、取消。
   - 先写 mock HTTP 测试，再实现请求逻辑。

4. **工具语义**
   - 新增 `tools.rs`，定义两个固定工具 schema。
   - 实现纯参数校验、结果归一化、UTF-8 安全截断。
   - 先写边界单测，再接 DeepSeek client。

5. **MCP stdio server**
   - 新增 `mcp.rs`，实现 JSON-RPC 生命周期、tools/list、tools/call、取消和串行 response writer。
   - 使用真实子进程/协议测试覆盖 stdout 纯净性和 request id 配对。

6. **集成与文档**
   - 将 `main.rs` 接通所有模块。
   - README 写出构建、环境变量、ox 配置和安全说明；README 不重复完整设计，链接到 `DESIGN.md`。
   - 加入最终 smoke test 说明。

7. **验证**
   - `cargo fmt --all -- --check`
   - `cargo test --all-targets`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo build --release`
   - 在无 `DEEPSEEK_API_KEY` 环境下确认启动失败且不打印 key/无关敏感信息。

## 10. 验收标准

实现只有同时满足以下条件才算第一版完成：

- `cargo test --all-targets`、fmt、clippy 和 release build 全部通过。
- 使用标准 MCP client 可以完成 initialize、tools/list、search、fetch。
- ox 通过本地 stdio 配置可以发现并调用两个工具，无需修改 ox 源码。
- Search 默认不带正文，Fetch 负责单页正文获取。
- 所有 HTTP 错误、协议错误、参数错误都不会让 server 无条件崩溃，也不会污染 stdout。
- 请求取消有效，重复长时间调用不会让 request map、响应缓存或日志内存持续增长。
- API key 不出现在任何模型可见输出、stdout、错误响应或普通日志中。
- 输出大小有硬上限，超限行为有测试覆盖。
- 设计中明确的非目标没有被实现为隐含抽象或额外配置项。

## 11. 后续演进触发条件

只有出现真实需求才开启后续设计：

- 有第二个搜索后端时，才评估抽取 provider trait；
- 需要多个 MCP host 共享一个实例时，才设计 Streamable HTTP、认证和部署；
- DeepSeek API 响应频繁重复且成本/延迟成为问题时，才评估带 TTL 和大小上限的缓存；
- 单次结果仍经常过大时，才重新评估分页或资源接口。

这些不是第一版 TODO，不应提前加入模块、配置或依赖。
