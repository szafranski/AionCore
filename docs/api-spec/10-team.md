# 10 - 团队模式

## 概述

多 Agent 协作系统：用户创建"团队"，指定一个 Lead Agent 和若干 Teammate Agent，Lead 负责任务分解与调度，Teammate 负责执行。Agent 之间通过邮箱（Mailbox）通信，通过任务板（Task Board）追踪工作进度。每个 Agent 拥有独立的对话上下文，由 `TeammateManager` 状态机驱动轮次调度。

**源码位置**：`process/team/`、`process/bridge/teamBridge.ts`、`common/types/teamTypes.ts`

> **设计决策**：原实现使用 XML 文本协议（`<send_message>`、`<task_create>` 等 XML tag）作为 Agent 间通信的结构化输出格式，与 MCP 工具调用并存。Rust 重写时建议统一为 MCP 工具调用，废弃 XML 解析层，降低复杂度。

## 子模块划分

| 子模块 | 原始源码 | Rust 归属建议 |
|--------|---------|--------------|
| 团队生命周期管理 | `TeamSessionService.ts` | `aionui-team` |
| 会话协调器 | `TeamSession.ts` | `aionui-team` |
| Agent 调度引擎 | `TeammateManager.ts` | `aionui-team` |
| 邮箱系统 | `Mailbox.ts` | `aionui-team` |
| 任务板 | `TaskManager.ts` | `aionui-team` |
| MCP Server（团队工具） | `TeamMcpServer.ts` | `aionui-team` |
| 事件总线 | `teamEventBus.ts` | `aionui-team`（内部模块） |
| 协议适配器 | `adapters/` | `aionui-team`（Rust 重写时可废弃） |
| Prompt 模板 | `prompts/` | `aionui-team` |
| 数据持久化 | `repository/SqliteTeamRepository.ts` | `aionui-db` |
| IPC 桥接 | `bridge/teamBridge.ts` | `aionui-team`（HTTP/WS 路由） |

---

## IPC 接口

### 团队管理

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `team.create` | HTTP | `{ name: string, agents: TeamAgentInput[] }` | `TTeam` | 创建团队：为每个 agent 创建独立 conversation 并写入 DB，第一个 agent 默认为 lead |
| `team.list` | HTTP | 无 | `TTeam[]` | 获取所有团队列表（含 agents 数组） |
| `team.get` | HTTP | `{ teamId: string }` | `TTeam \| null` | 获取单个团队详情 |
| `team.remove` | HTTP | `{ teamId: string }` | `void` | 删除团队：级联删除关联的 conversations、mailbox 消息、tasks |
| `team.rename` | HTTP | `{ teamId: string, name: string }` | `void` | 重命名团队 |

### Agent 管理

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `team.add-agent` | HTTP | `{ teamId: string, agent: TeamAgentInput }` | `TeamAgent` | 向团队添加 agent，创建对应 conversation |
| `team.remove-agent` | HTTP | `{ teamId: string, slotId: string }` | `void` | 移除 agent，删除对应 conversation |
| `team.rename-agent` | HTTP | `{ teamId: string, slotId: string, name: string }` | `void` | 重命名 agent |

### 消息与会话

| 通道名 | 目标协议 | 参数 | 返回值 | 功能语义 |
|--------|---------|------|--------|---------|
| `team.send-message` | HTTP | `{ teamId: string, content: string }` | `void` | 向团队发送用户消息：写入 lead 邮箱 → 写 user bubble 到 DB → 唤醒 lead agent |
| `team.send-message-to-agent` | HTTP | `{ teamId: string, slotId: string, content: string }` | `void` | 直接向指定 agent 发送消息 |
| `team.ensure-session` | HTTP | `{ teamId: string }` | `void` | 确保团队会话已启动（懒初始化 TeamSession + MCP Server） |
| `team.stop` | HTTP | `{ teamId: string }` | `void` | 停止团队会话（关闭 MCP Server，清理运行时状态） |

### 事件推送

| 通道名 | 方向 | 载荷 | 功能语义 |
|--------|------|------|---------|
| `team.agent.status` | 服务端 → 客户端 | `ITeamAgentStatusEvent` | Agent 状态变更（idle / working / thinking 等） |
| `team.agent.spawned` | 服务端 → 客户端 | `ITeamAgentSpawnedEvent` | Lead 动态创建了新 agent |
| `team.agent.removed` | 服务端 → 客户端 | `ITeamAgentRemovedEvent` | Agent 被移除 |
| `team.agent.renamed` | 服务端 → 客户端 | `ITeamAgentRenamedEvent` | Agent 被重命名 |

> **协议映射**：事件推送通过 WebSocket 通道实现（复用 `07-realtime.md`）。

---

## MCP 工具接口（Team MCP Server）

每个团队会话启动时在 `127.0.0.1` 上监听随机端口的 TCP MCP Server，供 AI Agent 通过 MCP 协议调用团队协作工具。连接使用 `auth_token`（UUID）鉴权。

### 工具列表

| 工具名 | 可用角色 | 参数 | 功能语义 |
|--------|---------|------|---------|
| `team_send_message` | Lead + Teammate | `{ to: string, message: string }` | 向指定 agent 邮箱写入消息并唤醒。`to="*"` 广播给所有其他 agent。如果目标 agent 回复 `shutdown_approved`，则执行关闭 |
| `team_spawn_agent` | 仅 Lead | `{ name: string, role: string, backend: string }` | 动态创建新 agent（白名单：claude / codex），分配到团队 |
| `team_task_create` | Lead + Teammate | `{ subject: string, description?: string, owner?: string, blockedBy?: string[] }` | 创建任务，支持依赖关系（blockedBy） |
| `team_task_update` | Lead + Teammate | `{ taskId: string, status?: string, description?: string, owner?: string, blockedBy?: string[] }` | 更新任务状态或字段 |
| `team_task_list` | Lead + Teammate | 无 | 列出团队所有任务（含状态、依赖关系） |
| `team_members` | Lead + Teammate | 无 | 列出团队所有成员（含角色、状态） |
| `team_rename_agent` | Lead + Teammate | `{ slotId: string, newName: string }` | 重命名 agent |
| `team_shutdown_agent` | Lead | `{ slotId: string, reason?: string }` | 向目标 agent 邮箱发送 `shutdown_request`，agent 可响应 `shutdown_approved` 或 `shutdown_rejected: <reason>` |

### MCP Stdio 桥接

AI Agent（如 Claude CLI）通过 stdio MCP 协议与团队交互。实际架构为两层：

```
Claude CLI ←stdio→ teamMcpStdio.js ←TCP→ TeamMcpServer (主进程)
```

`teamMcpStdio.js` 是独立 Node.js 进程，通过环境变量接收连接参数：
- `TEAM_MCP_PORT` — TCP 端口
- `TEAM_MCP_TOKEN` — 鉴权令牌
- `TEAM_AGENT_SLOT_ID` — 当前 agent 的 slot ID

TCP 协议：4 字节大端长度头 + JSON 负载。

> **设计决策**：原实现中 stdio → TCP 的两层桥接是 Electron + Claude CLI 架构约束所致。Rust 重写后，如果 Agent 运行在同一进程内，MCP 工具可直接作为内存函数调用，无需 TCP 中转。如果 Agent 仍为外部进程（如 Claude CLI），则保留 stdio 桥接但 TCP 层可替换为 Unix Socket。

---

## 核心流程

### 团队创建流程

```
用户点击 "Create Team"
    ↓
team.create({ name, agents: [{ name, role, backend, model }, ...] })
    ↓
TeamSessionService.createTeam()
    ├─ 为每个 agent 创建 conversation（type='team'）
    ├─ 第一个 agent 标记为 lead（role='lead'）
    ├─ 将 teamId 写入 conversation.extra.teamId
    ├─ 写入 teams 表
    └─ 返回 TTeam
```

### 用户发送消息流程

```
team.send-message({ teamId, content })
    ↓
TeamSession.sendMessage(content)
    ├─ Mailbox.write(leadSlotId, 'message', content)     // 写入 lead 邮箱
    ├─ 写 user bubble 到 lead 的 conversation             // DB + IPC 推送
    └─ TeammateManager.wake(leadSlotId)                    // 唤醒 lead
         ↓
    buildPayload()
         ├─ buildRolePrompt()                              // lead prompt + 成员列表 + 工具说明
         ├─ TaskManager.list()                             // 当前任务
         └─ Mailbox.readUnread()                           // 未读消息（原子读取+标记已读）
         ↓
    agentTask.sendMessage(payload)                         // 调用 AI Agent
         ↓
    AI 流式响应 → teamEventBus.emit('responseStream')
         ↓
    等待 finish 事件
         ↓
    finalizeTurn()
         ├─ parseResponse()                                // 解析 XML action 或 MCP 工具调用
         └─ 对每个 action 执行 executeAction()
              ├─ send_message → Mailbox.write(target) + wake(target)
              ├─ task_create → TaskManager.create()
              ├─ task_update → TaskManager.update() + checkUnblocks()
              ├─ spawn_agent → TeamSessionService.addAgent()
              └─ idle_notification → 标记 agent 为 idle
                   └─ maybeWakeLeaderWhenAllIdle()          // 所有 teammate idle → 唤醒 lead
```

### Agent 调度状态机

```
                    wake()
                      ↓
    idle ──────→ working ──────→ finalizeTurn()
     ↑                               ↓
     │                    ┌── executeAction() ──┐
     │                    ↓                     ↓
     │            send_message              idle_notification
     │            → wake(target)            → 标记 idle
     │                                          ↓
     │                          maybeWakeLeaderWhenAllIdle()
     │                          （所有 teammate idle 时）
     └──────────────────────────────────────────┘
```

**防死循环机制**：
- Lead 完成轮次后如果触发 idle，不会立即被唤醒
- 只有当所有非 lead agent 都变为 idle 时，才唤醒 lead
- 每次 wake 有 60 秒超时（`WAKE_TIMEOUT_MS`），超时自动标记 idle

### Shutdown 流程

```
Lead 发起 team_shutdown_agent(slotId)
    ↓
Mailbox.write(slotId, 'shutdown_request', reason)
    ↓
wake(slotId)
    ↓
Teammate 收到 shutdown_request
    ├─ 同意 → 回复 "shutdown_approved"
    │   → Lead 收到后 removeAgent(slotId)
    └─ 拒绝 → 回复 "shutdown_rejected: <reason>"
        → Lead 收到拒绝原因，决定下一步
```

---

## 数据模型

### 团队表 `teams`

| 列名 | 类型 | 约束 | 说明 |
|------|------|------|------|
| `id` | TEXT | PK | 团队 ID（UUID） |
| `name` | TEXT | NOT NULL | 团队名称 |
| `agents` | TEXT | NOT NULL | JSON 数组：`TeamAgent[]` |
| `lead_agent_id` | TEXT | | Lead agent 的 slot ID |
| `created_at` | INTEGER | NOT NULL | 创建时间戳 |
| `updated_at` | INTEGER | NOT NULL | 更新时间戳 |

### 邮箱表 `mailbox`

| 列名 | 类型 | 约束 | 说明 |
|------|------|------|------|
| `id` | TEXT | PK | 消息 ID（UUID） |
| `team_id` | TEXT | NOT NULL | 所属团队 |
| `to_agent_id` | TEXT | NOT NULL | 接收者 agent slot ID |
| `from_agent_id` | TEXT | NOT NULL | 发送者 agent slot ID（用户消息为 `'user'`） |
| `type` | TEXT | NOT NULL | 消息类型：`'message'` / `'idle_notification'` / `'shutdown_request'` |
| `content` | TEXT | NOT NULL | 消息内容 |
| `summary` | TEXT | | 消息摘要（用于 idle notification） |
| `read` | INTEGER | NOT NULL, DEFAULT 0 | 是否已读 |
| `created_at` | INTEGER | NOT NULL | 创建时间戳 |

### 任务表 `team_tasks`

| 列名 | 类型 | 约束 | 说明 |
|------|------|------|------|
| `id` | TEXT | PK | 任务 ID（UUID） |
| `team_id` | TEXT | NOT NULL | 所属团队 |
| `subject` | TEXT | NOT NULL | 任务主题 |
| `description` | TEXT | | 任务描述 |
| `status` | TEXT | NOT NULL, DEFAULT 'pending' | 状态：`'pending'` / `'in_progress'` / `'completed'` / `'deleted'` |
| `owner` | TEXT | | 负责人 agent slot ID |
| `blocked_by` | TEXT | NOT NULL, DEFAULT '[]' | JSON 数组：被阻塞的任务 ID 列表 |
| `blocks` | TEXT | NOT NULL, DEFAULT '[]' | JSON 数组：阻塞的任务 ID 列表（反向链接） |
| `metadata` | TEXT | | JSON：扩展元数据 |
| `created_at` | INTEGER | NOT NULL | 创建时间戳 |
| `updated_at` | INTEGER | NOT NULL | 更新时间戳 |

> **设计决策**：任务 ID 支持短前缀匹配（如 `abc` 匹配 `abc12345-...`），因为 Agent 在 XML 响应中经常截断 UUID。Rust 重写时如果废弃 XML 协议改用 MCP 工具调用，可去掉前缀匹配，要求精确 ID。

---

## 共享类型

```
TeammateRole = 'lead' | 'teammate'

TeammateStatus = 'idle' | 'working' | 'thinking' | 'tool_use' | 'error'

WorkspaceMode = 'team' | 'solo'

TeamAgent {
  slotId: string                    // 唯一标识（UUID，团队内 agent 的插槽 ID）
  name: string                      // 显示名称
  role: TeammateRole                // lead 或 teammate
  conversationId: string            // 关联的独立 conversation ID
  backend: string                   // AI 后端类型（如 'acp'、'gemini'）
  model: string                     // 模型标识
  customAgentId?: string            // 自定义 agent ID（如 ACP agent ID）
  status?: TeammateStatus           // 当前运行时状态
}

TTeam {
  id: string
  name: string
  agents: TeamAgent[]
  leadAgentId?: string
  createdAt: number
  updatedAt: number
}
```

### 事件载荷

```
ITeamAgentStatusEvent {
  teamId: string
  slotId: string
  status: TeammateStatus
}

ITeamAgentSpawnedEvent {
  teamId: string
  agent: TeamAgent
}

ITeamAgentRemovedEvent {
  teamId: string
  slotId: string
}

ITeamAgentRenamedEvent {
  teamId: string
  slotId: string
  name: string
}
```

### 内部类型（Process 端）

```
MailboxMessage {
  id: string
  teamId: string
  toAgentId: string
  fromAgentId: string
  type: 'message' | 'idle_notification' | 'shutdown_request'
  content: string
  summary?: string
  read: boolean
  createdAt: number
}

TeamTask {
  id: string
  teamId: string
  subject: string
  description?: string
  status: 'pending' | 'in_progress' | 'completed' | 'deleted'
  owner?: string                     // agent slot ID
  blockedBy: string[]                // 被阻塞的任务 ID
  blocks: string[]                   // 阻塞的任务 ID
  metadata?: Record<string, unknown>
  createdAt: number
  updatedAt: number
}
```

---

## Repository 接口

```
ITeamCrudRepository {
  createTeam(team: TTeam) → void
  listTeams() → TTeam[]
  getTeam(teamId: string) → TTeam | null
  updateTeam(teamId: string, updates: Partial<TTeam>) → void
  deleteTeam(teamId: string) → void
}

IMailboxRepository {
  writeMessage(msg: MailboxMessage) → void
  readUnreadAndMark(teamId: string, toAgentId: string) → MailboxMessage[]
  getHistory(teamId: string, toAgentId: string, limit?: number) → MailboxMessage[]
  deleteByTeam(teamId: string) → void
}

ITaskRepository {
  createTask(task: TeamTask) → void
  findTaskById(teamId: string, taskId: string) → TeamTask | null   // 支持前缀匹配
  updateTask(taskId: string, updates: Partial<TeamTask>) → void
  listTasks(teamId: string) → TeamTask[]
  appendToBlocks(taskId: string, blockedTaskId: string) → void     // 事务性 JSON 数组操作
  removeFromBlockedBy(taskId: string, unblockedTaskId: string) → void
  deleteByTeam(teamId: string) → void
}

ITeamRepository = ITeamCrudRepository & IMailboxRepository & ITaskRepository
```

> **设计决策**：`readUnreadAndMark()` 必须是事务性原子操作（读取 + 标记已读在同一事务内），防止并发调度下同一消息被重复读取。Rust 实现建议使用 SQLite 的 `RETURNING` 子句或 `BEGIN IMMEDIATE` 事务。

---

## 关键常量

| 常量 | 值 | 说明 |
|------|---|------|
| `WAKE_TIMEOUT_MS` | 60000 (60s) | Agent 唤醒超时，超时自动标记 idle |
| MCP auth_token | UUID v4 | 每个 TeamSession 生成唯一令牌 |
| MCP 监听地址 | `127.0.0.1:0` | 随机端口，仅本机访问 |
| spawn_agent 白名单 | `claude`, `codex` | Lead 动态创建 agent 限制的后端类型 |
| TCP 协议 | 4 字节大端长度头 + JSON | stdio ↔ TCP 桥接的线路格式 |

---

## 与其他模块的集成

### 依赖

| 模块 | 依赖方式 |
|------|---------|
| `02-database` | 读写 `teams`、`mailbox`、`team_tasks` 表 |
| `05-conversation` | 为每个 agent 创建独立 conversation（`type='team'`），conversation 的 `extra.teamId` 标记团队归属 |
| `06-ai-agent` | 通过各 AgentManager 发送消息和接收响应。Agent 的 MCP session 中注入 `teamMcpStdioConfig`。所有 AgentManager 在 `responseStream` 事件时同步 emit 到 `teamEventBus` |
| `07-realtime` | 事件推送（`agent.status`、`agent.spawned`、`agent.removed`、`agent.renamed`）通过 WebSocket |

### 被依赖

| 模块 | 依赖方式 |
|------|---------|
| `05-conversation` | `warmup` handler 中检测 `extra.teamId`，自动启动 TeamSession |
| `06-ai-agent` | AcpAgentManager 自动审批 `aionui-team` 前缀的 MCP 工具调用（无需用户确认） |
| `14-app-lifecycle` | 应用退出时调用 `disposeAllTeamSessions()` 清理所有 TCP server |

---

## 候选公共类型

| 类型 | 说明 | 建议归属 |
|------|------|---------|
| `TeammateRole` | Agent 角色枚举 `'lead' \| 'teammate'` | `aionui-team` |
| `TeammateStatus` | Agent 运行时状态枚举 | `aionui-team` |
| `WorkspaceMode` | 工作区模式 `'team' \| 'solo'` | `aionui-common`（前端需要区分） |
| `TeamAgent` | Agent 描述结构体 | `aionui-team` |
| `TTeam` | 团队描述结构体 | `aionui-team` |

---

## 设计决策

1. **Lead / Teammate 角色模型**：Lead 负责任务分解与分发，不自己执行具体工作；Teammate 负责执行并汇报。这一模式是 Agent 协作的核心抽象，Rust 重写时保留。

2. **邮箱通信模型**：Agent 间不直接通信，而是通过 Mailbox 异步投递。每次 wake 时一次性读取所有未读消息并原子标记已读。这保证了消息不丢失且不重复处理。

3. **任务板与依赖图**：任务支持 `blockedBy` / `blocks` 双向链接。当一个任务标记为 `completed` 时，自动检查并解除下游任务的阻塞（`checkUnblocks()`）。Rust 重写时建议在 DB 层用触发器或应用层逻辑保证双向链接一致性。

4. **XML 协议 vs MCP 工具调用**：原实现中 Agent 输出 XML tag（`<send_message>`、`<task_create>` 等）作为结构化 action，再由 `xmlFallbackAdapter` 解析。同时 MCP Server 也提供了同名工具。两套机制并存增加了复杂度。Rust 重写时建议统一使用 MCP 工具调用，废弃 XML 文本解析层。

5. **Team Event Bus**：原实现中 `ipcBridge.emit()` 仅路由到 renderer，同进程内的 `TeammateManager` 收不到事件，因此引入了 `teamEventBus`（EventEmitter）。所有 AgentManager 在发送 `responseStream` 时同步 emit 到 `teamEventBus`。Rust 重写时建议使用 `tokio::sync::broadcast` 或类似的进程内通道，统一事件路由。

6. **MCP 自动审批**：AcpAgentManager 自动批准 `aionui-team` 前缀的 MCP 工具调用，无需用户确认。这是多 Agent 协作的必要条件——Agent 间通信不应阻塞在人类审批上。

7. **动态 Agent 创建**：Lead 可在运行时通过 `team_spawn_agent` 创建新 Agent。出于安全考虑，后端类型限制为白名单（目前为 `claude` / `codex`）。Rust 重写时此白名单应可配置。
