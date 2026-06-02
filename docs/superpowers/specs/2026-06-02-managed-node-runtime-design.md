# Managed Node Runtime 设计稿

状态：已批准的设计基线

关联草稿：`docs/superpowers/specs/managed-node-runtime.md`

## 设计目的

定义 AionCore 的稳定 Node.js runtime 策略，使 MCP、Office、Agent 相关链路不再依赖用户本机环境里的 `node` / `npm` / `npx`。

这份文档替代原始草稿中的 API 假设。原始草稿对产品问题的判断是正确的，但第一版 API 设计过于以 resolver 为中心，不适合当前代码结构。

## 问题总结

AionCore 当前混合了三种不同的假设：

1. 一个同步的命令查找 API：`resolve_command_path`，调用方默认它廉价且无副作用。
2. 一个进程级 PATH 增强步骤：当前会优先放入 bundled bun 目录。
3. 多条执行链路实际需要的是完整 Node toolchain，而不是单纯“命令名到路径”的解析。

这些假设对 `bun` / `bunx` 还勉强成立，但对 managed Node 不成立：

- 选择 Node 可能涉及版本检查、文件系统 IO、归档解压和网络下载
- `npm` 和 `npx` 不能安全地建模成“另一个可执行文件路径”
- validation 和 doctor 需要结构化失败原因，而不是 yes/no
- 有些场景需要延迟安装能力，有些场景只需要无副作用 probe

## 目标

- 为 `node`、`npm`、`npx` 提供稳定的 Node toolchain。
- 保持用户 MCP JSON 语义不变；存储层和 API 层继续保留 `command: "npx"`。
- 将 Node runtime 选择逻辑集中到 `aionui-runtime`。
- 默认保证 `node`、`npm`、`npx` 同源。
- 将 npm 的 cache/config/prefix 都收口到 AionCore 的 `data_dir` 下。
- 为 doctor、MCP connection test、agent probe、validation 提供结构化诊断。
- 从执行链路中移除 `node -> bun` 兼容路径。

## 非目标

- 第一阶段不重做全部 JavaScript package manager 支持。
- 第一阶段不把所有 JavaScript 工具都变成 managed tools。
- 不修改用户 shell 配置，不写系统级 PATH。
- 不把已存储的 MCP 命令从 `npx` 改写成其他 launcher。

## 当前代码约束

### 约束 1：`resolve_command_path` 是同步 API

今天 `resolve_command_path` 被这些地方使用：

- `aionui-runtime::Builder`
- MCP / agent 的命令解析辅助逻辑
- conversation 的 stdio command validation
- custom agent probe 第一步

这些调用方都默认它只是一个廉价的本地查找。如果把它改成“必要时下载 managed Node”，那 validation 和 builder 代码会悄悄引入网络与安装副作用。

### 约束 2：Builder 构造过程是同步的

`Builder::new()` 和 `Builder::clean_cli()` 在构造时就解析 program，并暴露同步 `spawn()` 和异步 `output()`。这不是“确保 Node runtime 已经存在”的正确层次。

### 约束 3：有些命令需要 command plan，而不是一个路径

对于 managed runtime，`npm` 和 `npx` 更准确的模型是：

```text
program = managed node
argv prefix = npm-cli.js / npx-cli.js
env delta = managed PATH + npm cache/config/prefix
```

单个 `PathBuf` 无法正确表达这个语义。

## 外部方案观察

### Zed

Zed 通过异步 runtime 对象解析 Node，而不是通过同步 path lookup。它在 `NodeRuntime::instance()` 中决定使用 system Node 还是 managed Node，缓存 runtime 实例，并用 unavailable runtime object 表达失败，而不是返回 `None`。它运行 npm 时使用 runtime 自己组装的命令，同时把 cache/config/PATH 状态都注入到 runtime 目录下。这是最接近 AionCore 的参考模型。

### JetBrains IDE

JetBrains 将 Node runtime 选择和 package manager 选择拆开建模。它的文档明确说明：当选中的 Node runtime 变化时，IDE 中 alias 到的 package manager 路径会切换到该 Node runtime 对应版本，但仍允许用户显式配置自定义 package manager 路径。也就是说，默认策略是同源，只在显式 override 时允许打破同源。

### Volta / asdf / nvm 类工具

Volta 将 Node 相关工具看作一个 toolchain，通过 shim 和 pinned engine 上下文来管理。asdf 和 nvm 类工具也不是分别解析 `node`、`npm`、`npx`，而是基于安装目录或 shim 层切换整套 Node runtime。市场上的主流做法明显更偏向“先选 runtime/toolchain，再从中派生命令”，而不是为三个工具分别找来源。

## 备选方案

### 方案 A：保留 `resolve_command_path`，并让它自动下载

优点：

- 表面改动最小

缺点：

- 在 validation 和 builder 代码里隐藏了网络与安装副作用
- 仍然无法正确表达 managed `npm` / `npx` 的命令形态
- `Option<PathBuf>` 无法表达 install failed 这类诊断状态

结论：不采用

### 方案 B：异步 runtime 对象 + 结构化 command plan

优点：

- 符合问题本身的真实形态
- 只在执行链路中做 install-on-demand
- 支持结构化诊断
- 默认保持 `node` / `npm` / `npx` 同源

缺点：

- 需要修改当前依赖同步 builder 解析的调用点

结论：推荐

### 方案 C：只走 PATH shim 思路

优点：

- 心智模型简单
- Node 版本管理器用户容易理解

缺点：

- 在 Electron/backend 启动模型里，进程级全局 PATH 状态难以推理
- 诊断能力弱
- 历史 bun cache 污染风险高
- 像 `officecli` 这类 managed package 的安装与运行路径不容易收口

结论：不适合作为 AionCore backend 方案

## 推荐架构

### 1. 引入 runtime-first 模型

在 `crates/aionui-runtime/src/node_runtime.rs` 中新增 Node runtime 模块，并以 runtime object 作为 source of truth：

```rust
pub enum NodeRuntimeSource {
    ExplicitOverride,
    System,
    Managed,
}

pub struct ResolvedNodeRuntime {
    pub source: NodeRuntimeSource,
    pub node_path: PathBuf,
    pub npm_mode: NodePackageManagerMode,
    pub npx_mode: NodePackageManagerMode,
    pub node_version: semver::Version,
    pub npm_version: semver::Version,
    pub npx_version: semver::Version,
    pub root_dir: PathBuf,
}

pub enum NodePackageManagerMode {
    Executable(PathBuf),
    NodeEntrypoint {
        node_path: PathBuf,
        entrypoint: PathBuf,
    },
}
```

`ResolvedNodeRuntime` 是 `node`、`npm`、`npx` 的统一事实来源。

### 2. 拆分 probe 与 ensure

新增两层 API：

```rust
pub fn probe_runtime_command(command: &str) -> RuntimeCommandProbe;

pub async fn ensure_runtime_command(command: &str) -> Result<ResolvedCommand, RuntimeCommandError>;

pub async fn ensure_node_runtime() -> Result<ResolvedNodeRuntime, NodeRuntimeError>;
```

规则：

- `probe_*` 无副作用
- `ensure_*` 允许安装/下载 managed Node
- 只有执行链路调用 `ensure_*`
- validation、doctor summary、availability check 从 `probe_*` 开始

### 3. 用 command plan 替代 path-only 解析

引入：

```rust
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub args_prefix: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub source: ResolvedCommandSource,
}
```

行为：

- bare `node` 解析为真实 Node executable
- bare `npm` 和 `npx` 可以解析成：
  - 完整 system runtime 下的直接 executable path
  - managed runtime 下的 `node + npm-cli.js` / `node + npx-cli.js`
- 非 Node 命令仍然走普通 command lookup
- 绝对路径和显式相对路径一律原样尊重

这样就去掉了把 managed `npm` / `npx` 错误建模成 `PathBuf` 的问题。

### 4. 默认保持同源

默认 runtime 选择顺序按 runtime 维度进行，而不是按单个工具进行：

1. 显式 runtime override
2. 完整且有效的 system runtime
3. managed runtime

第一阶段不暴露按单个工具粒度拆分的 override 入口。

如果确实要支持 override，优先提供以下任一形式：

- 一个显式描述完整 runtime 的配置对象
- 一个完整 runtime 根目录的 override
- 一个 `node` 主入口，再从其所属 runtime 派生 npm/npx

原因：

- 避免 mixed provenance
- 更符合 Zed、JetBrains、Volta 类方案
- 让诊断结果保持一致

是否支持 per-tool override 以后再看真实产品需求，不作为第一阶段默认设计，也不应在当前文档中暗示已经存在这类环境变量。

### 5. managed npm 状态全部落在 `data_dir`

managed runtime 目录布局：

```text
{data_dir}/runtime/node/
  node-v24.11.0-{os}-{arch}/
    bin/node
    bin/npm
    bin/npx
    lib/node_modules/npm/...
    cache/
    blank_user_npmrc
    blank_global_npmrc
    tools/
      officecli/
```

规则：

- npm cache 放在 managed runtime 下
- managed npm 的 user/global config 使用 runtime 目录下的 blank 文件
- managed global install 必须带显式 `--prefix`
- managed npm 状态不应泄漏到用户 home 或系统级 npm 目录

### 6. 停止依赖 PATH mutation 提供 Node 语义

`enhance_process_path()` 不应再让 bun-backed `node` 变得可发现。

第一阶段必须完成的迁移要求：

- 停止创建 `node -> bun`
- 停止把旧 bundled bun cache 里的 `node` 当作有效 Node
- 主动忽略或删除现有 bun runtime 目录里的陈旧 `node` alias

这件事不能留到后续阶段，因为旧的 PATH-prepended bun 目录会继续 shadow 真正的 Node 行为。

## Runtime 解析规则

### system runtime 接受条件

system runtime 的判断应采用“先选 runtime，再派生命令”的模型，而不是把 `node`、`npm`、`npx` 当成三个独立 PATH 命令分别判断。

第一阶段建议采用保守且可诊断的接受规则：

1. `which(node)` 成功。
2. 对 `node` 的结果做 `canonicalize`，并能稳定推出一个 runtime root。
3. 从这个 runtime root 派生出 `npm` command plan 与 `npx` command plan。
4. 对以下三条最终命令做真实执行校验：
   - `node --version`
   - 派生出的 npm command plan + `--version`
   - 派生出的 npx command plan + `--version`
5. 三条命令都满足：
   - 进程可启动
   - 退出码为 `0`
   - stdout 可解析为版本号
6. `node` 版本 `>= 22.0.0`

只要任一项失败，就拒绝整个 system runtime，并切换到 managed runtime。

这里的关键点是：

- `npm` 和 `npx` 不再是独立 source of truth
- `node` 是 system runtime 的主入口
- `npm` / `npx` 必须由该 `node` 所属 runtime 派生出来

这比“三次独立 `which`”更符合 runtime/toolchain 模型，也更接近外部成熟方案的默认思路。

### system runtime root 推导

第一阶段只接受能够稳定推出 root 的布局。

推荐规则：

- Unix：
  - 如果 `node` 是 `<root>/bin/node`，则 runtime root 为 `<root>`
- Windows：
  - 如果 `node` 是 `<root>/node.exe`，则 runtime root 为 `<root>`

如果 `node` 的真实路径无法稳定推出明确 root，则不要继续猜测 PATH 上的 `npm` / `npx`，而是直接拒绝 system runtime。

### system npm / npx 派生规则

从 runtime root 派生，而不是再次把 PATH 作为事实来源。

第一阶段建议：

- Unix 优先尝试：
  - `<root>/bin/npm`
  - `<root>/bin/npx`
- Windows 优先尝试：
  - `<root>/npm.cmd`
  - `<root>/npx.cmd`

如果平台上的 wrapper 形式不稳定，允许退回到 Node entrypoint 方案：

- `node <root>/lib/node_modules/npm/bin/npm-cli.js`
- `node <root>/lib/node_modules/npm/bin/npx-cli.js`

最终是否接受，不由“文件是否存在”决定，而由最终 command plan 的真实执行结果决定。

### 关于 shim / version manager 环境

像 Volta、asdf、nvm 这类环境，本质上也是通过 shim 或安装目录来管理整套 toolchain。

第一阶段应保守处理：

- 如果 shim 最终 `canonicalize` 后仍能稳定落到一套明确 runtime root，则可以接受
- 如果无法稳定推出 runtime root，就拒绝 system runtime，回退 managed runtime

这样做的目标不是兼容所有奇异 PATH 组合，而是在自动选择 system runtime 时优先保证稳定性和可诊断性。

### managed runtime 接受条件

managed runtime 只有在以下条件都满足时才算有效：

- 安装目录结构存在
- `node --version` 成功，且 stdout 可解析为 semver
- managed npm 的最终 command plan 执行 `--version` 成功
- managed npx 的最终 command plan 执行 `--version` 成功

校验必须执行真实命令，不能只检查文件是否存在。

这里的“成功”有精确定义，不是“文件存在”或“看起来能用”，而是：

- 使用未来真实执行时同一套 command plan
- 能成功启动进程
- 退出码为 `0`
- stdout 可解析为版本号

推荐的校验方式：

```text
{managed-node} --version
{managed-npm-command-plan} --version
{managed-npx-command-plan} --version
```

其中：

- managed npm command plan 可能是 `bin/npm --version`
- 也可能是 `node npm-cli.js --version`
- managed npx command plan 可能是 `bin/npx --version`
- 也可能是 `node npx-cli.js --version`

具体采用哪一种，不由文档预设，而由 runtime 实现根据平台和所选模式生成。

这类检查的目标是确认“managed runtime 本身可启动且 wiring 正确”，包括：

- `node`、`npm`、`npx` 之间的同源关系正确
- PATH/env 注入正确
- wrapper / shim / entrypoint 解析正确
- Windows 下 `.cmd` 或脚本入口可正常启动

它不负责证明业务场景一定成功。例如：

- `npx --version` 成功，不代表后续下载某个 MCP package 一定成功
- `npm --version` 成功，不代表 `officecli` 安装一定成功

后者应由 connection test、Office install test、集成测试分别验证。

### 下载/安装策略

第一阶段：

- 固定 Node 版本
- 下载官方 Node 发行包
- 解压到 managed runtime 目录
- 创建 cache/config 目录
- 校验 runtime
- 校验失败时删除并重试一次

第二阶段：

- 用 `SHASUMS256.txt` 做 SHA256 校验
- 加 install lock，防止多进程并发安装
- 增加旧版本清理策略

## 调用点集成

### `aionui-runtime::Builder`

不要让 `Builder::new()` 隐式下载 Node。

应新增一个用于预解析命令的入口：

```rust
impl Builder {
    pub fn from_resolved(cmd: &ResolvedCommand) -> Self;
}
```

`from_resolved()` 负责应用：

- `program`
- `args_prefix`
- env delta

现有 `Builder::new()` 保留给廉价的同步解析和非 Node 场景。

### 阻塞模型与调用点约束

managed Node 方案不只是“命令如何解析”的变化，还包括“下载/安装会在什么时机发生，以及哪些调用点允许等待”。

这一点需要明确，否则很容易误解成：

- 创建 conversation 时就会卡在下载 Node
- 任意 validation 都可能触发网络请求
- 首次发消息会阻塞 HTTP handler 很久

本设计要求不同调用点采用不同的阻塞策略。

#### 1. 永远不允许触发下载安装的调用点

这些调用点只能使用 `probe_*`，不得调用 `ensure_*`：

- conversation create
- MCP 配置保存前 validation
- 普通表单校验
- doctor 的快速环境摘要
- agent registry 的 availability 计算

原因：

- 它们的职责是“判断当前状态”或“在落库前做同步校验”
- 不应在用户无感知时触发下载
- 不应把网络副作用塞进同步或准同步路径

这些路径可以返回：

- 当前 system runtime 是否完整
- 当前平台是否允许 managed runtime
- 当前如果立刻执行，是否会走 managed runtime

但不能主动开始 install。

#### 2. 明确允许等待下载安装的调用点

这些调用点可以调用 `ensure_*`，因为它们的语义本来就是“为真实执行做准备”或“立即验证真实执行能力”：

- `POST /api/conversations/{id}/warmup`
- 首次 `send_message` 时的 task build
- MCP connection test
- custom agent try-connect
- Office install/update

这些路径允许等待 managed runtime 准备完成，并在失败时返回结构化错误。

#### 3. conversation create 不应被阻塞

当前 `ConversationService::create()` 只负责：

- 创建 conversation row
- 选择/创建 workspace
- 持久化初始 extra

它不会构建 agent task，也不会启动 CLI。

因此在 managed Node 方案下，conversation create 仍应保持这一属性：

- 只做 `probe_*`
- 不做 `ensure_*`
- 不触发 Node 下载

这是一条强约束。

#### 4. warmup 可以阻塞，并且应该阻塞到 ready

当前 `warmup` 路径会调用 `get_or_build_task()`，而 ACP factory 在 build 末尾又会显式执行 `warmup_session()`。

这意味着 `warmup` 的现有语义本来就是：

- 返回成功时，agent task 已经构建完成
- ACP `session/new` 或 resume 已完成
- 调用方可以把“warmup 成功”理解为“ready”

因此在 managed Node 方案下：

- 如果 warmup 首次触发 managed Node install，允许它等待下载/解压/校验完成
- `warmup` 返回成功时，Node runtime 也应被视为 ready
- 如果 install 失败，`warmup` 应直接返回结构化错误

也就是说，`warmup` 是显式 readiness endpoint，允许承担首次安装成本。

#### 5. `send_message` 不应阻塞 HTTP 返回

当前 `send_message` 已经采用“HTTP 先返回，agent task build 在后台 task 中执行”的模型：

- 用户消息先入库
- HTTP handler 返回 `202`
- 后台 `tokio::spawn` 中调用 `get_or_build_task()`
- task build 完成后才真正向 agent 发送消息

因此在 managed Node 方案下，首次发消息触发的 Node 下载不会阻塞 HTTP handler 本身。

它会影响的是：

- agent task 何时 ready
- 第一轮消息何时真正开始被 agent 处理

不会影响的是：

- 用户消息入库
- HTTP `202 Accepted` 的及时返回

这条链路的要求是：

1. `send_message` 主线程不调用 `ensure_*`
2. `get_or_build_task()` 内部允许 await managed runtime 准备
3. 如果准备失败，沿用当前 send failure tip / stream failure 路径反馈错误

也就是说：

- 首次发消息可以“延后开始真正执行”
- 但不能“卡住请求本身”

#### 6. MCP connection test / custom agent try-connect 可以等待

这些接口的语义是“立即验证真实执行能力”，所以允许等待 install。

在这些路径里：

- `probe_*` 不够
- 必须使用 `ensure_*`
- 如果 install 失败，应把失败原因直接返回给用户

它们本来就是显式测试动作，因此允许比普通保存校验更重。

### 并发与去重

managed Node 引入异步 install 后，还必须定义并发去重策略。

#### 1. conversation 级 task build 去重

当前 `WorkerTaskManager::get_or_build_task()` 已经通过 `OnceCell` 保证同一个 conversation 的 task build 只会跑一次。

这意味着对于同一 conversation：

- `/warmup`
- 首次 `send_message`

如果并发触发，只会等待同一个 build future，不会重复构建多个 agent task。

#### 2. runtime 级 install 去重

还需要新增一个更高层的“Node runtime install 去重”，否则不同 conversation 并发首次触发 managed Node 时会重复下载。

第一阶段建议：

- 进程内使用 async `OnceCell` / `Mutex` 保护 `ensure_node_runtime()`
- 同一进程内，多个调用方共享同一个 install future

第二阶段再增加：

- 跨进程 lock file

防止多个 backend 进程或 doctor/服务端并发安装同一 runtime。

### 推荐的调用点映射

第一阶段建议按下表执行：

- conversation create：`probe_*`，绝不 install
- validation/save：`probe_*`，绝不 install
- doctor：默认 `probe_*`
- task build（ACP/AionRS/首次 send/warmup）：`ensure_*`
- MCP connection test：`ensure_*`
- custom agent try-connect：`ensure_*`
- Office install/update：`ensure_*`

### ACP / AionRS 的具体集成位置

对于 ACP / AionRS，真正适合触发 `ensure_*` 的位置不是 conversation create，而是 agent factory build 阶段。

原因：

- factory 本身已经是 async
- `get_or_build_task()` 已经允许等待 CLI spawn、握手和 warmup
- MCP 注入的 command 解析当前就在 ACP/AionRS factory 内完成

因此建议：

1. conversation row / session snapshot 中继续保留原始 bare command，例如 `npx`
2. ACP/AionRS factory 在把 MCP server 转换为最终 `CommandSpec` 或 SDK config 时，调用 `ensure_runtime_command()`
3. 只有 factory build 真正开始时，才允许触发 Node install

这保证了：

- 配置层与持久化层语义稳定
- task build 层拥有完整的异步能力
- 下载成本只发生在真正“需要执行”的时刻

### 用户体验与可观察性

第一阶段至少应保证：

- `send_message` 不阻塞 HTTP 返回
- `warmup` 成功即 ready
- install 失败能进入现有 send failure / connection test error 路径
- 日志能区分“task build 中等待 managed runtime”与“CLI 已启动但握手失败”

如果后续要进一步优化体验，可以增加：

- 首次 build 超过阈值时的“正在准备 Node 环境”状态事件
- install progress 的前端提示

但这些属于增强项，不是第一阶段闭环所必需。

### MCP 执行链路

执行期的 stdio launcher 应这样工作：

1. 保持存储中的 command 文本不变
2. 对 bare `node` / `npm` / `npx` 调用 `ensure_runtime_command()`
3. 用 `ResolvedCommand` 构造子进程

适用范围：

- MCP connection test
- ACP session injection
- AionRS MCP injection

### MCP / conversation validation

当前 validation 在 bare command 不在 PATH 时直接报错，这对 managed Node 来说过严。

新的 validation 规则：

- 显式路径必须立刻存在
- bare `node` / `npm` / `npx` 在以下任一条件下应视为合法：
  - 当前 system runtime 有效
  - 当前平台允许 managed runtime，即使尚未安装
- 错误信息必须区分：
  - unsupported platform
  - managed runtime disabled
  - explicit path missing
  - system runtime incomplete

这样就不会因为系统里没有 Node、但 managed runtime 可用，而在 validation 阶段提前误报失败。

### Office

Office 有两个需要分开的问题：

1. 用什么 npm 去 install/update `officecli`
2. 用什么 executable 去真正运行 `officecli`

第一阶段 Office 方案：

- 用 managed npm 把 `officecli` 安装到 data dir 下的 managed prefix
- 运行时显式从这个 managed prefix 里解析 `officecli`
- 不依赖 ambient PATH 在安装后再次找到 `officecli`

这样才能补上当前链路中的漏洞：`npm install -g officecli` 成功，并不等于后续一定能解析到 `officecli`。

### Agent probe 与 doctor

Agent/custom probe 和 doctor 应改成结构化结果：

- `Available`
- `AvailableViaManagedInstall`
- `Unavailable(reason)`

doctor 输出应展示：

- source
- 实际选中的 program/entrypoint
- version
- unavailable 时的具体失败原因

## 错误模型

将“找不到路径”升级为结构化错误。

```rust
pub enum NodeRuntimeError {
    UnsupportedPlatform { os: String, arch: String },
    ExplicitRuntimeInvalid { details: String },
    SystemRuntimeInvalid { details: String },
    ManagedDownloadFailed { details: String },
    ManagedExtractFailed { details: String },
    ManagedValidationFailed { details: String },
    ManagedDisabled,
}

pub enum RuntimeCommandError {
    Node(NodeRuntimeError),
    CommandNotFound { command: String },
}
```

约束：

- command validation UI 不能再把所有错误都压成 “not found in PATH”
- execution error 必须保留 runtime install 上下文
- doctor 直接展示结构化失败原因

## 日志策略

这是关键路径，而且 runtime install / resolution 很难观察，因此需要有针对性的日志。

增加：

- `info`：runtime source 被选中
- `info`：managed install 开始 / 完成
- `warn`：system runtime 被拒绝，或 managed validation 失败
- `debug`：更细的 probe 决策过程

禁止记录：

- 除高层 subcommand 名称外的 npm command payload
- 用户 MCP command 的 package 参数
- token、registry credential、command env value、文件内容

## 测试策略

### 单元测试

- runtime source selection
- managed `npm` / `npx` 的 command-plan 构造
- absolute path passthrough
- system runtime 缺 `npx` 时被拒绝
- stale bun `node` alias 被忽略
- Office managed prefix 的解析逻辑

### 集成测试

- 没有 system Node，但允许 managed runtime：
  - bare `npx` 的 MCP connection test 成功
- 没有 system Node：
  - conversation validation 在 managed runtime 可安装时接受 bare `npx`
- Office install：
  - managed npm 把 `officecli` 安装到 data dir
  - spawn 使用该明确路径
- doctor：
  - 能显示 `managed`、`system` 和结构化失败状态

### 手工验证

- macOS arm64
- Linux x64
- Windows x64，重点验证 `.cmd` 行为

## 分阶段实施

### 阶段一：修正架构并建立最小稳定闭环

- 新增 runtime-first 的 Node 模块
- 新增结构化 probe/ensure API
- 新增 `ResolvedCommand`
- 停止 bun-backed `node`
- 接入 MCP、ACP、AionRS 执行链路
- 接入 Office 的 managed install 与 managed executable 解析
- 将 managed npm 的 cache/config/prefix 收口到 data dir
- 增加 doctor 的 runtime 状态输出

### 阶段二：可靠性加固

- archive checksum verification
- install lock
- retry classification
- old-version cleanup

### 阶段三：可选产品能力

- 用户可配置的 runtime policy
- 显式完整 runtime override
- 若未来出现真实需求，再评估 custom package-manager override

## 决策总结

正确的抽象单位不是“帮我解析一个 `node` / `npm` / `npx` 路径”。

正确的抽象应该是：

- 先解析一个 Node runtime
- 再从这个 runtime 派生命令执行计划
- validation 和诊断使用无副作用 probe API
- 真正执行时使用 async ensure API

这就是 AionCore 应该实现的 managed Node runtime 设计。
