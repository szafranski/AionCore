# 12 - MCP 协议

## 概述

MCP（Model Context Protocol）服务器配置管理与多 Agent CLI 同步系统：管理用户配置的 MCP 服务器列表（增删改查），将配置同步到各 AI Agent CLI（Claude、Gemini、Qwen 等），支持连接测试、OAuth 认证，以及通过 ACP session 注入内置 MCP 服务器。

**源码位置**：`process/services/mcpServices/`、`process/bridge/mcpBridge.ts`、`process/agent/acp/mcpSessionConfig.ts`、`process/resources/builtinMcp/`

> **设计决策**：原实现中 AionUi 自身**不直接运行 MCP Client 处理 tool call**，而是作为"MCP 配置管理平台"，将配置分发到各 AI Agent CLI（如 Claude CLI、Gemini CLI 等），由 CLI 负责 spawn MCP server 进程并处理工具调用。Rust 重写时保留此架构——后端负责配置管理和分发，tool call 执行由 ACP 后端处理。

## 子模块划分

| 子模块 | 原始源码 | Rust 归属建议 |
|--------|---------|--------------|
| 核心服务（配置管理 + Agent 分发） | `McpService.ts` | `aionui-mcp` |
| 协议层（连接测试 + Agent 抽象） | `McpProtocol.ts` + 9 个 Agent 实现 | `aionui-mcp` |
| OAuth 服务 | `McpOAuthService.ts` | `aionui-mcp` |
| ACP Session MCP 注入 | `mcpSessionConfig.ts` | `aionui-mcp`（与 `aionui-ai-agent` 协作） |
| 内置 MCP Server（图片生成） | `builtinMcp/imageGenServer.ts` | `aionui-mcp`（独立二进制或嵌入进程） |
| 内置 MCP Server（团队模式） | `teamMcp/teamMcpStdio.ts` | `aionui-team`（见 `10-team.md`） |
| IPC 桥接 | `mcpBridge.ts` | `aionui-mcp`（HTTP 路由） |

---

## IPC 接口

### 配置查询

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `mcp.get-agent-configs` | HTTP | 无 | `DetectedMcpServer[]` | 扫描所有已安装的 Agent CLI，读取各自已配置的 MCP 服务器列表。用于前端展示"哪些 Agent 已安装了哪些 MCP 服务器" |

### 连接测试

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `mcp.test-connection` | HTTP | `{ server: IMcpServer }` | `McpConnectionTestResult` | 测试 MCP 服务器连接：创建临时 Client → connect → listTools → close。返回工具列表或错误信息。对 SSE/HTTP 类型会先探测 OAuth 认证需求 |

### Agent 同步

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `mcp.sync-to-agents` | HTTP | `{ servers: IMcpServer[] }` | `McpSyncResult` | 将指定 MCP 服务器配置同步到所有已安装的 Agent CLI。每个 Agent 的同步策略不同（CLI 命令 / 直接写配置文件） |
| `mcp.remove-from-agents` | HTTP | `{ serverNames: string[] }` | `McpSyncResult` | 从所有 Agent CLI 中移除指定名称的 MCP 服务器配置 |

### OAuth 认证

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `mcp.check-oauth-status` | HTTP | `{ server: IMcpServer }` | `{ authenticated: boolean }` | 检查 MCP 服务器的 OAuth 认证状态（是否有有效 token） |
| `mcp.login-oauth` | HTTP | `{ server: IMcpServer }` | `{ success: boolean, error?: string }` | 执行 OAuth 登录流程：启动本地回调服务器 → 打开浏览器授权 → 获取 token → 存储 |
| `mcp.logout-oauth` | HTTP | `{ server: IMcpServer }` | `void` | 删除存储的 OAuth token |
| `mcp.get-authenticated-servers` | HTTP | 无 | `string[]` | 获取所有已通过 OAuth 认证的服务器 URL 列表 |

---

## 核心流程

### MCP 服务器配置 CRUD

MCP 服务器配置存储在 `ConfigStorage('mcp.config')`（`IMcpServer[]`），CRUD 操作在 Renderer 层直接操作存储，不经过 IPC：

```
[增] handleAddMcpServer(server)
    ├─ 同名检查：已存在则更新，否则追加
    ├─ 生成 ID: "mcp_<timestamp>"
    ├─ 记录 originalJson（原始 JSON 文本）
    └─ 写入 ConfigStorage

[改] handleEditMcpServer(id, updates)
    ├─ 按 id 定位
    ├─ 合并更新，更新 updatedAt
    └─ 写入 ConfigStorage

[删] handleDeleteMcpServer(id)
    ├─ 从列表移除
    ├─ 若 enabled=true → 调用 mcp.remove-from-agents 从各 CLI 清理
    └─ 写入 ConfigStorage

[启用/禁用] handleToggleMcpServer(id)
    ├─ 翻转 enabled 状态
    ├─ enabled=true → 调用 mcp.sync-to-agents 同步到各 CLI
    ├─ enabled=false → 调用 mcp.remove-from-agents 从各 CLI 移除
    └─ 写入 ConfigStorage

[批量导入] handleBatchImportMcpServers(servers)
    ├─ 逐个处理：同名则更新，不存在则追加
    └─ 写入 ConfigStorage
```

> **设计决策**：原实现中 CRUD 在 Renderer 层直接操作 ConfigStorage，不经过主进程 IPC。Rust 重写时建议统一为 REST API，配置持久化到 DB（或配置文件），所有操作通过后端接口完成。

### 连接测试流程

```
用户点击"测试连接"
    ↓
mcp.test-connection({ server })
    ↓
McpService.testConnection(server)
    ↓
根据 transport.type 创建对应 Transport:
    ├─ stdio → StdioClientTransport({ command, args, env })
    ├─ sse → 先 fetch(url) 探测 401 → SSEClientTransport({ url, headers })
    ├─ http → 先 POST initialize 探测认证 → StreamableHTTPClientTransport
    └─ streamable_http → StreamableHTTPClientTransport({ url, headers })
    ↓
new Client({ name: 'aionui-mcp-test', version }) → client.connect(transport)
    ↓
client.listTools() → 提取 { name, description, _meta }
    ↓
finally: client.close()
    ↓
返回 McpConnectionTestResult:
    ├─ 成功: { success: true, tools: [...] }
    ├─ 需要认证: { success: false, needsAuth: true, authMethod, wwwAuthenticate }
    └─ 错误: { success: false, error: string }
```

特殊错误处理：
- `ENOENT`：命令不存在
- `EACCES`：权限不足
- `ENOTEMPTY`：npm 缓存损坏，自动执行 `npm cache clean --force` 后重试一次

### Agent 同步流程

```
用户启用 MCP 服务器 / 点击"同步到所有 Agent"
    ↓
mcp.sync-to-agents({ servers })
    ↓
McpService.syncToAgents(servers)
    ↓ withServiceLock() — 串行化，防止并发子进程资源耗尽
    ↓
遍历 9 个 Agent (Claude/Gemini/Aionui/Qwen/iFlow/CodeBuddy/Codex/Opencode/Aionrs):
    ├─ 每个 Agent: withLock() — per-agent 串行化
    ├─ agent.detectExisting() → 读取当前已配置的 MCP 服务器
    ├─ 对比需要同步的 servers：
    │   ├─ 已存在且相同 → 跳过
    │   ├─ 已存在但不同 → 先 remove 再 install
    │   └─ 不存在 → install
    └─ agent.installServer(server) → 执行 CLI 命令或写配置文件
    ↓
聚合结果: McpSyncResult { success, results: [{ agent, success, error? }] }
```

### ACP Session MCP 注入流程

```
AcpAgent 创建新 session
    ↓
loadBuiltinSessionMcpServers()
    ↓
从 ProcessConfig.get('mcp.config') 读取启用的 MCP 服务器
    ↓
parseAcpMcpCapabilities(response) → 判断 ACP 后端支持的 transport 类型
    ↓
buildBuiltinAcpSessionMcpServers(servers, capabilities)
    ├─ 过滤：仅保留 ACP 后端支持的 transport 类型
    ├─ 格式转换：IMcpServer → AcpSessionMcpServer
    │   ├─ stdio: { type: 'stdio', name, command, args, env: [{name, value}] }
    │   └─ http/sse: { type, name, url, headers: [{name, value}] }
    ├─ 注入内置 MCP（如 imageGenServer）
    └─ 注入 Team MCP（如果在团队模式中）
    ↓
AcpConnection.newSession({ mcpServers: AcpSessionMcpServer[] })
    → CLI 负责 spawn MCP server 进程并处理后续 tool call
```

### OAuth 登录流程

```
用户点击"OAuth 登录"
    ↓
mcp.login-oauth({ server })
    ↓
McpOAuthService.login(serverUrl)
    ├─ 创建 MCPOAuthProvider({ serverUrl })
    ├─ 启动本地 HTTP 回调服务器（随机端口）
    ├─ 构建授权 URL → 打开系统浏览器
    ├─ 等待 OAuth 回调 → 获取 authorization code
    ├─ 用 code 换取 access_token + refresh_token
    ├─ 存储 token 到 MCPOAuthTokenStorage
    └─ 关闭回调服务器
    ↓
返回 { success: true }
```

---

## 数据模型

### MCP 服务器配置

```
IMcpServer {
  id: string                     // 格式 "mcp_<timestamp>"
  name: string                   // 服务器名称（同步到 Agent CLI 时用此名称）
  description?: string
  enabled: boolean               // 控制是否同步到 Agent CLI
  transport: IMcpServerTransport // 传输方式（四选一）
  tools?: IMcpTool[]             // 连接测试后填充的工具列表
  status?: McpServerStatus       // 连接状态
  lastConnected?: number         // 上次连接成功时间戳
  createdAt: number
  updatedAt: number
  originalJson: string           // 原始 JSON 文本（用于编辑时还原）
  builtin?: boolean              // true = 内置服务器（隐藏编辑/删除按钮）
}
```

### 传输方式（四选一）

```
IMcpServerTransport =
  | { type: 'stdio', command: string, args?: string[], env?: Record<string, string> }
  | { type: 'sse', url: string, headers?: Record<string, string> }
  | { type: 'http', url: string, headers?: Record<string, string> }
  | { type: 'streamable_http', url: string, headers?: Record<string, string> }
```

> **设计决策**：原实现中 `http` 和 `streamable_http` 是两个独立类型，但 MCP SDK 中 `StreamableHTTPClientTransport` 同时处理这两种。Rust 重写时建议合并为 `http`（即 streamable HTTP），因为这是 MCP 规范中 HTTP transport 的标准实现。`sse` 保留为向后兼容。

### MCP 工具描述

```
IMcpTool {
  name: string
  description?: string
  inputSchema?: unknown          // JSON Schema
  _meta?: Record<string, unknown>
}
```

### 连接测试结果

```
McpConnectionTestResult {
  success: boolean
  tools?: IMcpTool[]             // 成功时返回可用工具列表
  error?: string                 // 失败时的错误信息
  needsAuth?: boolean            // 是否需要认证
  authMethod?: 'oauth' | 'basic' // 认证方式
  wwwAuthenticate?: string       // HTTP WWW-Authenticate 头（用于 OAuth 发现）
}
```

### Agent 同步结果

```
McpSyncResult {
  success: boolean               // 所有 Agent 是否都成功
  results: Array<{
    agent: string                // Agent 标识（如 'claude', 'gemini'）
    success: boolean
    error?: string
  }>
}
```

### Agent 检测结果

```
DetectedMcpServer {
  source: McpSource              // Agent 标识
  servers: IMcpServer[]          // 该 Agent 已配置的 MCP 服务器列表
}
```

### MCP 来源标识

```
McpSource = 'claude' | 'gemini' | 'qwen' | 'iflow' | 'codex'
           | 'codebuddy' | 'opencode' | 'aionrs' | 'nanobot' | 'aionui'
```

### ACP Session MCP 配置格式

```
AcpSessionMcpServer =
  | AcpSessionMcpServerStdio
  | AcpSessionMcpServerHttpLike

AcpSessionMcpServerStdio {
  type?: 'stdio'                 // 默认值，可省略
  name: string
  command: string
  args: string[]
  env: Array<{ name: string, value: string }>
}

AcpSessionMcpServerHttpLike {
  type: 'http' | 'sse'
  name: string
  url: string
  headers?: Array<{ name: string, value: string }>
}
```

### ACP MCP 能力声明

```
AcpMcpCapabilities {
  stdio: boolean
  http: boolean
  sse: boolean
}
```

### 服务器状态枚举

```
McpServerStatus = 'connected' | 'disconnected' | 'error' | 'testing'
```

---

## Agent 适配器

核心抽象：`AbstractMcpAgent` 定义统一接口，每个 Agent 实现具体的安装/读取/删除策略。

### 抽象接口

```
AbstractMcpAgent {
  source: McpSource
  isInstalled() → boolean                           // 检测 CLI 是否已安装
  detectExisting() → IMcpServer[]                    // 读取已配置的 MCP 服务器
  installServer(server: IMcpServer) → void           // 安装 MCP 服务器配置
  removeServer(serverName: string) → void            // 移除 MCP 服务器配置
  testMcpConnection(server: IMcpServer) → McpConnectionTestResult  // 连接测试（基类实现）
}
```

### 各 Agent 安装策略

| Agent | 检测方式 | 读取配置 | 安装方式 | 移除方式 |
|-------|---------|---------|---------|---------|
| Claude | `which claude` | `claude mcp list` → 解析文本 | `claude mcp add-json -s user` / `claude mcp add -s user --transport` | `claude mcp remove -s {user\|local\|project}` |
| Gemini | `which gemini` | `gemini mcp list` → 解析文本 | `gemini mcp add -s user` | `gemini mcp remove -s {user\|project}` |
| Aionui | 始终可用 | `ProcessConfig.get('mcp.config')` | 合并写入 ProcessConfig | 跳过（配置由前端管理） |
| Qwen | `which qwen` | `qwen mcp list` → 解析文本 | `qwen mcp add -s user` | `qwen mcp remove -s {user\|project}` + 回退到直接改 `~/.qwen/client_config.json` |
| iFlow | `which iflow` | `iflow mcp list` → 解析文本 | `iflow mcp add --transport {type} -s user` | `iflow mcp remove -s {user\|project}` |
| CodeBuddy | `which codebuddy` | 解析 `~/.codebuddy/mcp.json` | `codebuddy mcp add -s user` / `add-json` | `codebuddy mcp remove -s {user\|local\|project}` |
| Codex | `which codex` | `codex mcp list --json` → JSON 解析 | `codex mcp add [--url]` | `codex mcp remove` |
| Opencode | 检查 `~/.config/opencode/` | 直接读写 `opencode.json` | 写 `mcp` 字段（JSON） | 从 `mcp` 字段删除 |
| Aionrs | `which aionrs` | 读取 TOML config（`aionrs --config-path`） | 写 `[mcp.servers.*]` TOML | 从 TOML 删除对应 key |

> **设计决策**：原实现中每个 Agent 的配置读写方式差异很大（CLI 命令 / JSON 文件 / TOML 文件），需要逐个适配。Rust 重写时建议保持此 Agent 适配器模式（Strategy Pattern），每个 Agent 实现 `McpAgentAdapter` trait。新增 Agent 只需添加一个 adapter 实现。

---

## 内置 MCP Server

### 图片生成 Server（`imageGenServer.ts`）

以 stdio 子进程方式运行，由 ACP CLI 在 `session/new` 时 spawn。

| 工具名 | 参数 | 功能 |
|--------|------|------|
| `aionui_image_generation` | `{ prompt: string, model?: string, size?: string, quality?: string, n?: number }` | 调用图片生成 API。从环境变量读取配置（`AIONUI_IMG_MODEL`、`AIONUI_IMG_API_URL`、`AIONUI_IMG_API_KEY` 等） |

环境变量：

| 变量 | 说明 |
|------|------|
| `AIONUI_IMG_MODEL` | 图片生成模型名称 |
| `AIONUI_IMG_API_URL` | API 端点 URL |
| `AIONUI_IMG_API_KEY` | API 密钥 |
| `AIONUI_IMG_SIZE` | 默认图片尺寸 |
| `AIONUI_IMG_QUALITY` | 默认图片质量 |
| `AIONUI_IMG_STYLE` | 默认图片风格 |

### 团队 MCP Server（`teamMcpStdio.ts`）

以 stdio 子进程方式运行，通过 TCP 与主进程通信。详见 `10-team.md`。

---

## 并发控制

### 服务级锁（`withServiceLock`）

`McpService` 通过 Promise 链实现串行化锁，确保以下三个重型操作不并发执行：
- `getAgentMcpConfigs`（扫描所有 Agent CLI）
- `syncMcpToAgents`（向所有 Agent 同步配置）
- `removeMcpFromAgents`（从所有 Agent 移除配置）

原因：这些操作都可能 spawn 多个子进程（CLI 命令），并发执行会耗尽系统资源。

### Agent 级锁（`withLock`）

每个 `AbstractMcpAgent` 实例有独立的 `operationQueue`，确保同一 Agent 的操作串行执行（避免同一 CLI 的并发写入冲突）。

> **设计决策**：Rust 重写时建议使用 `tokio::sync::Mutex` 或 `tokio::sync::Semaphore` 实现类似的串行化。服务级锁可用全局 Mutex，Agent 级锁可用 per-agent Mutex。

---

## 配置持久化

### 存储位置

| 数据 | 存储方式 | 说明 |
|------|---------|------|
| MCP 服务器列表 | `ConfigStorage('mcp.config')` → JSON 文件 | `IMcpServer[]` |
| Agent 安装状态缓存 | `ConfigStorage('mcp.agentInstallStatus')` → JSON 文件 | `Record<serverName, McpSource[]>` |
| OAuth Token | `MCPOAuthTokenStorage`（@office-ai/aioncli-core） | 按 serverUrl 存储 |

> **设计决策**：原实现中 MCP 配置存储在 Electron ConfigStorage（JSON 文件）。Rust 重写时建议迁移到 SQLite（`mcp_servers` 表），与其他数据统一管理。Agent 安装状态为缓存数据，可存 Redis 或内存。

---

## 与其他模块的集成

### 依赖

| 模块 | 依赖方式 |
|------|---------|
| `04-system-settings` | 读取 ConfigStorage 中的 MCP 配置 |
| `06-ai-agent` | ACP session 创建时注入 MCP 服务器配置（`loadBuiltinSessionMcpServers`） |
| `10-team` | 团队模式下注入 Team MCP Server（`buildTeamMcpServer`） |

### 被依赖

| 模块 | 依赖方式 |
|------|---------|
| `06-ai-agent` | AcpAgent `session/new` 和 `session/load` 时调用 MCP 模块获取要注入的服务器列表 |
| `13-extension` | 扩展系统通过 `extensions.getMcpServers` 贡献额外 MCP 服务器，在前端合并到配置列表 |

---

## 外部依赖

| 库 | 用途 | Rust 替代建议 |
|----|------|--------------|
| `@modelcontextprotocol/sdk` | MCP Client/Server SDK（连接测试、内置 Server） | `rmcp` crate 或直接实现 MCP 协议 |
| `@office-ai/aioncli-core` | OAuth Provider + Token Storage | 自行实现 OAuth 2.0 PKCE 流程 |
| `smol-toml` | Aionrs TOML 配置读写 | `toml` crate |
| `strip-json-comments` | Opencode JSONC 配置读写 | `json_comments` crate 或手动 strip |
| `zod` | 内置 MCP Server 的工具参数校验 | `serde` + `jsonschema` crate |

---

## 设计决策

1. **配置管理平台而非执行层**：AionUi 后端不直接运行 MCP Client 执行 tool call，而是管理 MCP 服务器配置并分发到各 AI Agent CLI。这避免了在后端重复实现 MCP Client 逻辑，且各 CLI 对 MCP 的支持更加成熟和完整。Rust 重写时保留此架构。

2. **Agent 适配器模式**：9 个 Agent 的配置格式和管理方式各不相同（CLI 命令、JSON 文件、TOML 文件）。采用策略模式（Strategy Pattern）统一抽象，新增 Agent 只需实现一个 adapter。Rust 重写时用 `McpAgentAdapter` trait 实现。

3. **串行化锁**：Agent CLI 操作（spawn 子进程）不应并发执行。服务级锁防止跨 Agent 的并发操作，Agent 级锁防止同一 Agent 的并发写入。Rust 中用 `Mutex` 实现。

4. **Transport 类型合并**：原实现区分 `http` 和 `streamable_http`，但 MCP 规范中 HTTP transport 统一为 Streamable HTTP。建议 Rust 重写时合并为 `http` 类型，`sse` 作为 legacy 保留。

5. **连接测试的一次性 Client**：测试连接时创建临时 MCP Client，connect → listTools → close，不保持长连接。Rust 重写时同样采用此策略。

6. **OAuth 流程**：对 SSE/HTTP 类型的 MCP 服务器，先探测是否需要 OAuth 认证（HTTP 401 + WWW-Authenticate 头），然后引导用户完成 OAuth PKCE 流程。Token 存储在本地，自动刷新。Rust 重写时使用 `oauth2` crate。

7. **内置 MCP Server 作为独立进程**：图片生成和团队工具作为独立的 stdio MCP Server，由 ACP CLI spawn。这确保了与第三方 MCP Server 的一致性——对 AI 来说，内置和外部 MCP Server 使用方式完全相同。Rust 重写时可编译为独立二进制，或嵌入到后端进程中通过 in-process MCP Server 实现。

8. **配置存储迁移建议**：原实现用 Electron ConfigStorage（JSON 文件），Rust 重写时建议迁移到 SQLite `mcp_servers` 表，统一数据管理，支持事务和并发控制。

---

## 候选公共类型

| 类型 | 说明 | 建议归属 |
|------|------|---------|
| `IMcpServer` | MCP 服务器完整配置 | `aionui-mcp`（导出供前端和其他模块使用） |
| `IMcpServerTransport` | 传输方式枚举（stdio / sse / http） | `aionui-mcp` |
| `IMcpTool` | MCP 工具描述 | `aionui-mcp` |
| `McpSource` | Agent 来源标识枚举 | `aionui-common`（多模块共用） |
| `McpConnectionTestResult` | 连接测试结果 | `aionui-api-types` |
| `McpSyncResult` | Agent 同步结果 | `aionui-api-types` |
| `AcpSessionMcpServer` | ACP session 注入格式 | `aionui-ai-agent`（与 ACP 协议绑定） |
| `AcpMcpCapabilities` | ACP 后端 MCP 能力声明 | `aionui-ai-agent` |
