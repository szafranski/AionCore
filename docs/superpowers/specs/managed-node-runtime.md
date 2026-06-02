# AionCore Managed Node Runtime 方案

> 说明：
> 本文档保留为最初的问题陈述与第一版草稿。
> 当前已经批准的设计基线是
> `docs/superpowers/specs/2026-06-02-managed-node-runtime-design.md`。
> 如果两份文档存在冲突，以设计稿为准。
>
> 当前实现状态摘要（2026-06-02）：
> managed Node runtime 的阶段一闭环已经基本落地，包括 runtime-first API、MCP/ACP/AionRS 执行接入、Office managed prefix 闭环、doctor/runtime probe 输出，以及移除 `node -> bun` alias 创建。
> 尚未完成的是阶段二的可靠性和体验加固项，例如 checksum、跨进程 install lock、runtime preparing 状态事件、install progress 与首次初始化提示。

## 背景

AionCore 当前已经在 `aionui-runtime` 中提供了 bundled bun runtime。启动时会把 bun 目录放进 `PATH`，并在解包时创建 `bun`、`bunx`，以及一个 `node` 别名，让部分 `#!/usr/bin/env node` 脚本可以在没有系统 Node.js 的环境中运行。

团队已经对齐：继续用 `bun` 或 `bunx` 替代 Node 生态不是合适方向。`node -> bun` 只能作为历史兼容层，不能等价于真实 Node.js。后续应移除这种隐式替代，统一使用真实 Node.js 发行包提供 `node`、`npm`、`npx`。

特别是用户导入第三方 MCP 配置时，经常会出现如下配置：

```json
{
  "command": "npx",
  "args": ["-y", "@some/mcp-server"]
}
```

这里的 `npx` 语义是使用 npm 生态的包解析、下载、缓存和 bin 启动逻辑。把它替换成 `bunx` 会改变用户配置语义；继续依赖用户本机 PATH 又会导致 AionUI 在用户没有安装 Node、npm、npx 或 Electron 启动环境 PATH 不完整时出现不稳定。

因此，AionCore 需要一套类似 Zed `node_runtime.rs` 的 managed Node runtime。它不是简单地“内置一个 node 二进制”，而是提供一套完整的 Node.js 发行环境，并在运行时按规则选择系统 Node 或 AionCore 托管 Node。这样可以在用户本机环境不可靠时，由 AionCore 使用官方 Node.js 完整发行包提供稳定的 `node`、`npm`、`npx`。

## 目标

- 为 AionCore 提供稳定、可控的 `node` / `npm` / `npx` 执行环境。
- 不修改用户导入的 MCP JSON 语义，配置层继续保存 `command: "npx"`。
- 在执行层将 `node` / `npm` / `npx` 解析到系统可用版本或 AionCore managed Node runtime。
- 移除 `node -> bun` 这类隐式替代路径，所有 `node` 语义都由真实 Node.js 提供。
- 统一 Office、MCP、Agent CLI 探测等场景的命令解析策略。
- 降低用户本机未安装 Node、PATH 不完整、nvm 未加载、npm 缺失等问题对 AionUI 功能稳定性的影响。
- 提供清晰的错误信息和 doctor 诊断能力。

## 非目标

- 不把用户配置中的 `npx` 自动改写为 `bunx`。
- 不再为 `node` 场景设计 bun fallback。`node`、`npm`、`npx` 必须来自真实 Node.js 发行包。
- 不在本方案中决定 bundled bun 是否保留给其他显式 bun 场景；如果保留，也不能参与 `node` / `npm` / `npx` 解析。
- 不要求所有 JavaScript 工具都强制使用 managed Node。绝对路径和用户显式配置应被尊重。
- 不在第一阶段解决所有内置 skill 文档中的 `node` / `npm` 示例，只先解决后端实际执行和注入链路。

## 当前现状

### 历史 bundled bun runtime

`crates/aionui-runtime` 当前负责 bundled bun：

- `resolve_bun()` 返回可用 bun。
- `bun_bin_dir()` 返回 bun 所在目录。
- 解包时会创建 `bunx`。
- 解包时会创建 `node`，指向 bun，用于兼容部分 `#!/usr/bin/env node` shebang。

新的方案中，`node -> bun` 应作为待移除的历史行为。`node` 命令不应再解析到 bun，也不应依赖 bun 来模拟 Node.js。

### 命令解析

当前 `resolve_command_path(cmd)` 的策略大致是：

```text
bun   -> bundled bun 或系统 bun
bunx  -> bundled bunx 或系统 bunx
其他  -> which(PATH)
```

这意味着 `node`、`npm`、`npx` 都仍然依赖用户本机 PATH。

新方案应把 `node`、`npm`、`npx` 从普通 PATH 命令中提升出来，统一交给真实 Node runtime 管理。

### Office 链路

`aionui-office` 当前在 `officecli` 缺失时执行：

```text
npm install -g officecli
```

并且更新检查也执行：

```text
npm outdated -g officecli
npm install -g officecli@latest
```

如果用户机器没有 `npm`，或 Electron 启动时 PATH 中没有 `npm`，这条链路会失败。

### MCP 链路

MCP 配置层会保留用户导入的 stdio 配置，例如：

```json
{
  "command": "npx",
  "args": ["-y", "@upstash/context7-mcp"]
}
```

这点是正确的。配置层不应改写用户意图。

但执行和注入链路中有多处重复的 `resolve_stdio_command`，它们本质上只是用当前 `PATH` 解析 bare command：

- MCP connection test
- ACP session MCP injection
- AionRS MCP config injection
- session snapshot MCP conversion

这些路径应该统一收口到 `aionui-runtime`。

### Agent CLI 探测

Agent registry 和 health check 会调用 `resolve_command_path` 判断 CLI 是否可用。由于 `resolve_command_path` 目前不管理 `node` / `npm` / `npx`，相关检测结果仍然受用户本机 PATH 影响。

## Zed 参考实现

Zed 已经实现了一套可参考的 Node runtime，主要文件路径如下：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs
/Users/zhoukai/Documents/github/zed/crates/zed/src/main.rs
/Users/zhoukai/Documents/github/zed/assets/settings/default.json
```

### Zed 的核心选型逻辑

Zed 在 `NodeRuntime::instance()` 中集中决定使用哪套 Node runtime。核心流程在：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:87
```

逻辑顺序是：

1. 如果用户在设置里指定了 `node.path` / `npm_path`，优先使用用户指定路径。
2. 如果允许查找系统 PATH，则等待 shell env 加载完成后查找系统 `node` 和 `npm`。
3. 系统 Node 检查通过时使用系统 Node。
4. 系统 Node 不存在、版本过低或检查失败时，如果允许下载，则安装并使用 Zed managed Node。
5. 如果所有路径都不可用，返回 `UnavailableNodeRuntime`，让上层拿到明确错误。

关键代码位置：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:112
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:135
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:150
```

这点对 AionCore 很关键：Node runtime 的选择必须集中在一个模块里完成，Office、MCP、Agent 不应各自直接 `which("node")`、`which("npm")`、`which("npx")`。

### Zed 如何检查用户系统 Node

Zed 的系统 Node 检查在 `SystemNodeRuntime::new` 和 `SystemNodeRuntime::detect`：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:793
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:830
```

检查动作包括：

- 使用 `which("node")` 查找系统 `node`。
- 使用 `which("npm")` 查找系统 `npm`。
- 执行 `node --version`。
- 解析输出中的 semver，去掉前缀 `v`。
- 要求 Node 版本不低于 `22.0.0`。
- 版本过低时返回错误，而不是勉强使用。

Zed 的版本检查逻辑等价于：

```text
node --version
parse version
if version < 22.0.0:
  reject system Node
```

关键代码位置：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:794
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:796
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:808
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:810
```

AionCore 应采用同类策略。只要系统 Node 版本过低，或者 `npm` / `npx` 缺失，就不应继续使用系统环境，而应切换到 managed Node。

### Zed 如何安装 managed Node

Zed 的 managed Node 安装逻辑在：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:538
```

关键行为：

- 固定 Node 版本：`v24.11.0`。
- 根据平台映射 OS：
  - `macos -> darwin`
  - `linux -> linux`
  - `windows -> win`
- 根据架构映射 arch：
  - `x86_64 -> x64`
  - `aarch64 -> arm64`
- 拼出官方 Node 下载 URL。
- macOS/Linux 下载 `.tar.gz`。
- Windows 下载 `.zip`。
- 解压到 Zed data dir 下的 `node` 目录。
- 解压后使用 managed Node 执行 npm 版本检查，确认 runtime 可用。

Zed URL 拼接逻辑在：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:628
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:637
```

格式如下：

```text
https://nodejs.org/dist/{version}/node-{version}-{os}-{arch}.{extension}
```

Zed 解压逻辑在：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:646
```

校验已有 managed Node 是否可用的逻辑在：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:579
```

它不是只判断文件存在，而是执行：

```text
managed-node npm-cli.js --version
```

如果执行失败，就删除旧目录并重新下载。

### Zed 如何运行 npm

Zed 没有直接执行系统 `npm`，而是通过 runtime 组装 npm command。managed Node 下，实际执行形式是：

```text
node npm-cli.js <subcommand> <args>
```

关键代码在：

```text
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:739
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:758
/Users/zhoukai/Documents/github/zed/crates/node_runtime/src/node_runtime.rs:770
```

这解决了两个问题：

- npm 的入口和 Node runtime 保持同源，不会出现系统 npm 配 managed Node 或相反的混搭。
- 可以统一注入 npm cache、userconfig、globalconfig、proxy、CA 等环境。

AionCore 的 `npm` 和 `npx` 也应保持同源：如果使用 managed Node，就使用 managed Node 发行包里的 `npm` 和 `npx`，不要混用用户系统里的工具。

### Zed 的设置入口

Zed 的默认设置说明在：

```text
/Users/zhoukai/Documents/github/zed/assets/settings/default.json:2020
```

含义是：

- 默认先找 `$PATH` 中的 `node` 和 `npm`。
- 如果系统版本足够新，就使用系统版本。
- 用户可以设置 `ignore_system_version: true` 强制使用 Zed managed Node。
- 用户也可以设置 `path` / `npm_path` 指定自己的 Node / npm。

Zed 主程序把设置转换为 `NodeBinaryOptions` 的位置在：

```text
/Users/zhoukai/Documents/github/zed/crates/zed/src/main.rs:534
```

其中 `allow_binary_download: true` 目前是固定开启。

AionCore 可以先不暴露完整设置项，但实现层需要预留类似能力：

- 是否允许系统 PATH。
- 是否允许下载 managed Node。
- 是否允许用户指定 Node/npm/npx 路径。
- 是否强制忽略系统 Node。

### AionCore 从 Zed 借鉴什么

AionCore 不应照搬 Zed 的所有接口，但应借鉴这些原则：

- Node 选择逻辑集中在 `aionui-runtime`。
- 系统 Node 必须检查版本，不满足要求就不用。
- 系统 runtime 不完整时不用，例如只有 `node` 没有 `npm` / `npx`。
- managed runtime 下载官方完整 Node.js 发行包。
- managed runtime 要执行真实命令校验，不只检查文件存在。
- npm/npx 要和 node 同源，避免混用。
- 失败要有明确错误，供 doctor、MCP connection test、Office install path 展示。

## 设计方案

### 新增 managed Node runtime

在 `crates/aionui-runtime/src` 下新增模块：

```text
node_runtime.rs
```

导出 API：

```rust
pub enum NodeTool {
    Node,
    Npm,
    Npx,
}

pub fn resolve_node_tool(tool: NodeTool) -> Result<PathBuf, NodeRuntimeError>;
pub fn resolve_runtime_command(command: &str) -> Option<PathBuf>;
pub fn node_runtime_dir() -> Option<PathBuf>;
```

`resolve_runtime_command` 作为统一命令解析入口：

```text
node / npm / npx  -> managed Node runtime 或合格的系统 Node runtime
bun / bunx        -> 显式 bun 场景，是否支持由单独策略决定
其他命令           -> 系统 PATH
绝对路径/带路径命令 -> 原样尊重
```

关键约束：`resolve_runtime_command("node")`、`resolve_runtime_command("npm")`、`resolve_runtime_command("npx")` 永远不能返回 bun 或 bunx。

### Node runtime 解析优先级

`resolve_node_tool(NodeTool::Npx)` 等 API 的优先级：

1. 用户显式 override，例如提供一套完整 runtime，或提供一个可派生完整 runtime 的主入口。
2. 系统 PATH 中的 `node` / `npm` / `npx`，且完整性和版本校验通过。
3. AionCore managed Node runtime。

### 系统 Node 检查规则

系统 Node 不能只检查 `node` 是否存在。AionCore 需要的是一套能稳定支持 MCP、Office、Agent 的 Node 工具链，因此系统 runtime 至少要满足：

- `node --version` 成功，版本满足最低要求。
- `npm --version` 成功。
- `npx --version` 成功。
- `node`、`npm`、`npx` 都来自系统 PATH 或用户显式 override，且能在 AionCore 当前进程环境下执行。

最低 Node 版本建议与当前生态兼容性对齐，例如 `>= 20` 或 `>= 22`。如果需要与 Zed 保持更接近，可以采用 `>= 22`。

建议第一版采用 `>= 22.0.0`，原因：

- Zed 当前也使用 `>= 22.0.0` 作为系统 Node 最低版本。
- MCP server 生态中很多包依赖较新的 Node runtime。
- 版本门槛过低会把问题延后到运行时，表现为更难诊断的包安装或启动失败。

检查伪代码：

```text
node_path = which("node")
npm_path = which("npm")
npx_path = which("npx")

if any missing:
  reject system runtime

node_version = run(node_path, "--version")
if node_version < 22.0.0:
  reject system runtime

if run(npm_path, "--version") fails:
  reject system runtime

if run(npx_path, "--version") fails:
  reject system runtime

accept system runtime
```

用户显式 override 的检查也应走同样逻辑，只是路径来源由 env 或配置提供，而不是 `which`。

### managed Node 检查规则

managed Node 解压后不能只检查文件存在，需要执行真实命令确认可用：

```text
{node} --version
{npm} --version
{npx} --version
```

如果使用官方包中 npm 的 JS 入口，也可以采用类似 Zed 的方式：

```text
{node} {npm-cli.js} --version
```

AionCore 需要额外确认 `npx` 可用，因为 MCP JSON 中 `command: "npx"` 是核心场景。Node 官方发行包中的 npm 包通常包含 npx 入口，但不同平台的入口文件形态不同，Windows 尤其需要处理 `.cmd`。

建议 managed runtime 安装完成后记录：

```text
node_path
npm_path
npx_path
node_version
npm_version
npx_version
source = managed
```

这些信息供 doctor、日志和错误信息使用。

### 下载官方 Node 发行包

managed Node 使用官方 Node.js 发行包，而不是只下载单个 `node` 二进制。

官方下载地址格式：

```text
https://nodejs.org/dist/{version}/node-{version}-{platform}-{arch}.{ext}
```

示例：

```text
https://nodejs.org/dist/v24.11.0/node-v24.11.0-darwin-arm64.tar.gz
https://nodejs.org/dist/v24.11.0/node-v24.11.0-darwin-x64.tar.gz
https://nodejs.org/dist/v24.11.0/node-v24.11.0-linux-x64.tar.gz
https://nodejs.org/dist/v24.11.0/node-v24.11.0-linux-arm64.tar.gz
https://nodejs.org/dist/v24.11.0/node-v24.11.0-win-x64.zip
```

平台映射：

```text
macOS   -> darwin
Linux   -> linux
Windows -> win

x86_64  -> x64
aarch64 -> arm64
```

压缩格式：

```text
macOS / Linux -> tar.gz
Windows       -> zip
```

### 安装目录

managed Node 应安装到 AionCore data dir 下，避免污染系统环境。

建议目录：

```text
{data_dir}/runtime/node/node-{version}-{os}-{arch}/
```

目录内使用官方发行包原始结构：

```text
bin/node
bin/npm
bin/npx
lib/node_modules/npm/...
```

Windows：

```text
node.exe
npm.cmd 或 npm
npx.cmd 或 npx
node_modules/npm/...
```

### 缓存和校验

第一阶段可以先做到：

- 固定版本。
- 下载后解压。
- 解压后执行 `node --version`、`npm --version`、`npx --version` 校验。
- 校验失败则删除 runtime 目录并重试一次。

第二阶段增加：

- 下载 `SHASUMS256.txt` 校验归档 sha256。
- 缓存 stamp 文件，记录版本、平台、归档 sha256、安装时间。
- 并发启动时使用锁文件，避免多个进程同时下载或解压。

## 调用点改造

### aionui-runtime

修改 `resolve_command_path`：

```text
node / npm / npx  -> managed Node runtime 或合格的系统 Node runtime
bun / bunx        -> 显式 bun 策略，不能影响 node/npm/npx
其他              -> which(PATH)
```

`CmdBuilder::new` 和 `CmdBuilder::clean_cli` 已经通过 `resolve_command_path` 解析 bare command，因此这里改造后，大部分调用点会自然受益。

同时需要移除解包阶段创建 `node -> bun` 的行为，避免 PATH 中出现由 bun 伪装的 node。

### aionui-office

将 `npm` 调用从系统 PATH 改为 managed runtime：

```text
npm install -g officecli
npm outdated -g officecli
npm install -g officecli@latest
```

短期：继续使用 npm，但由 managed Node 提供。

中期：评估优先使用 officecli 官方二进制安装方式，npm 作为 fallback。原因是 officecli 的内置 skill 文档已经推荐官方安装脚本，后端自动安装逻辑应与文档一致。

### aionui-mcp

MCP 入库和展示不改。

连接测试执行前，将 stdio command 解析为 runtime command：

```text
npx -> managed Node runtime 中的 npx
node -> managed Node runtime 中的 node
npm -> managed Node runtime 中的 npm
```

错误信息应从：

```text
Command not found: npx
```

升级为更可诊断的错误，例如：

```text
MCP server requires npx, but managed Node runtime is unavailable: download failed
```

### aionui-ai-agent

ACP 和 AionRS 在注入 MCP 到 agent session 前，都需要使用同一套解析逻辑。

现有重复的 `resolve_stdio_command` 应统一替换为 `aionui_runtime::resolve_runtime_command`。

这样用户导入的 MCP JSON 仍然是 `npx`，但传给实际 agent session 的 command 可以是 managed runtime 中的绝对路径。

### Agent registry 和 health check

`resolve_command_path` 改造后，registry 和 health check 对 `node` / `npm` / `npx` 的判断会自动使用 managed runtime。

这会让 agent 可用性判断更接近真实运行能力。

## Doctor 诊断

`aioncore doctor` 应增加 runtime 区块：

```text
Runtime:
  node:
    source: system | managed | unavailable
    path: ...
    version: ...

  npm:
    source: system | managed | unavailable
    path: ...
    version: ...

  npx:
    source: system | managed | unavailable
    path: ...
    version: ...

  bun:
    source: bundled | system | unavailable
    path: ...
    version: ...
    note: only used for explicit bun/bunx commands
```

如果 managed Node 下载失败，应显示失败原因：

- 网络错误
- 平台不支持
- 解压失败
- 校验失败
- npm/npx 缺失或不可执行

## 分阶段实施

### 阶段一：最小闭环

- 新增 `node_runtime.rs`。
- 固定 Node 版本。
- 实现下载、解压、校验。
- `resolve_command_path` 支持 `node` / `npm` / `npx`。
- 移除 `node -> bun` alias 创建逻辑。
- 确保 `resolve_command_path("node")` 不再返回 bun。
- 修改 Office 的 npm 调用自然走 managed npm。
- 替换 MCP / ACP / AionRS 重复的 stdio command 解析。
- 增加单元测试：
  - `resolve_command_path("npx")` 能返回 managed npx。
  - `resolve_command_path("node")` 返回真实 Node，不返回 bun。
  - 绝对路径不被替换。
  - MCP 配置 roundtrip 不改写 `command: "npx"`。

### 阶段二：诊断和错误体验

- `doctor` 增加 runtime 状态。
- MCP connection test 返回更明确的 runtime 错误。
- Agent health check 区分“命令缺失”和“managed runtime 准备失败”。
- 增加日志字段：runtime source、tool、path、version。

### 阶段三：完整性和安全性

- 下载 `SHASUMS256.txt` 并校验归档 sha256。
- 增加并发锁，避免多进程重复下载/解压。
- 增加缓存 stamp。
- 增加 Node runtime 清理策略，只保留当前版本和最近一个旧版本。
- 清理历史 bun node alias 的迁移残留，确保旧缓存中存在的 `node -> bun` 不会继续被使用。

### 阶段四：Office 安装策略收敛

- 优先使用 officecli 官方二进制安装。
- npm 全局安装作为 fallback。
- 避免污染用户全局 npm 环境。
- 将 officecli 安装目录纳入 AionCore data dir 管理。

## 风险与注意事项

### 不应偷换 MCP 语义

用户写 `npx`，就代表 npm 生态语义。不能自动改成 `bunx`。

同理，用户写 `node`，就代表真实 Node.js 语义。不能自动解析到 bun。

### managed Node 不能污染用户环境

不应修改用户 shell 配置，不应全局安装 Node，不应写入系统 PATH。所有路径应限定在 AionCore data dir。

### 下载失败必须可诊断

如果首次使用 MCP 触发 Node 下载失败，用户需要看到明确原因。否则会表现为 MCP 启动失败或 agent 不可用，难以定位。

### 本地开发和发行构建要区分

本地开发构建可以允许 fallback 到系统 Node，但必须能模拟“系统没有 Node”的场景进行测试。

发行构建应保证 managed Node 下载逻辑可用，或预置可用的 runtime。

## 测试策略

### 单元测试

- command resolver：
  - `node` / `npm` / `npx` 走 Node runtime。
  - `node` 不会解析到 bun。
  - 绝对路径原样返回。
  - Windows `.cmd` / `.ps1` / `.bat` fallback 保持可用。

- node runtime：
  - 平台映射正确。
  - 下载 URL 拼接正确。
  - 解压目录正确。
  - 校验失败会重试或返回明确错误。

- MCP：
  - import `command: "npx"` 后 DB 和 response 仍是 `npx`。
  - connection test 执行时解析到 runtime npx。
  - session injection 执行时解析到 runtime npx。

### 集成测试

- 构造一个临时 PATH，不包含系统 Node。
- 运行 MCP connection test，确认 `npx` 可由 managed runtime 提供。
- 运行 Office install path，确认 `npm` 不依赖系统 PATH。
- 运行 agent registry hydrate，确认 `npx` bridge 不因系统 PATH 缺失被误判不可用。

### 手工验证

- macOS arm64：无系统 Node 环境下导入 `npx` MCP。
- Linux x64：无系统 Node 环境下启动 Office preview。
- Windows x64：验证 `npm` / `npx` 可执行文件解析，尤其 `.cmd` 行为。

## 建议优先级

优先做阶段一和阶段二。它们能直接解决用户本机 Node 环境导致的 MCP、Office、Agent 不稳定问题。

阶段三用于提高可靠性和安全性。

阶段四用于减少 npm 全局安装带来的副作用，是 Office 链路的长期收敛方向。
