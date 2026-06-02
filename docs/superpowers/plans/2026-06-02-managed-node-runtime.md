# Managed Node Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 AionCore 引入稳定的 managed Node runtime，使 `node` / `npm` / `npx` 不再依赖用户本机 PATH，并在 MCP、ACP/AionRS、Office、doctor、validation 中形成一致的执行与诊断模型。

**Architecture:** 以 `aionui-runtime` 为唯一 Node runtime 事实来源，新增 runtime-first 模块，提供 `probe_*` 与 `ensure_*` 两层 API，并用 `ResolvedCommand` 取代“只返回一个路径”的建模。配置层与持久化层继续保留用户原始命令文本；只有在真正进入执行路径时，才异步准备 managed runtime 并派生最终 command plan。

**Tech Stack:** Rust 2024、Tokio、reqwest、semver、zip、tar/gzip、现有 `aionui-runtime::Builder`、现有 `WorkerTaskManager` / ACP factory / Office watch manager。

---

## 文件结构

### 新增文件

- `crates/aionui-runtime/src/node_runtime/mod.rs`
  - Node runtime 公开入口；编排 `probe_*` / `ensure_*`、进程内 install 去重、对外导出统一 API。
- `crates/aionui-runtime/src/node_runtime/types.rs`
  - `ResolvedNodeRuntime`、`ResolvedCommand`、`RuntimeCommandProbe`、错误枚举、辅助结构。
- `crates/aionui-runtime/src/node_runtime/system.rs`
  - system runtime 检测、`node` root 推导、从 root 派生 `npm/npx` command plan。
- `crates/aionui-runtime/src/node_runtime/managed.rs`
  - managed Node 下载、解压、布局校验、runtime 自检、prefix/bin 解析辅助。
- `docs/superpowers/plans/2026-06-02-managed-node-runtime.md`
  - 当前实现计划文档。

### 修改文件

- `crates/aionui-runtime/Cargo.toml`
  - 补齐 runtime 安装与版本校验所需依赖。
- `crates/aionui-runtime/src/lib.rs`
  - 导出新的 runtime API。
- `crates/aionui-runtime/src/cache.rs`
  - 复用/扩展 runtime root 布局，提供 Node runtime 目录定位。
- `crates/aionui-runtime/src/extract.rs`
  - 移除 `node -> bun` alias 创建；加入清理旧 alias 的逻辑。
- `crates/aionui-runtime/src/resolver.rs`
  - 保持同步 resolver 轻量化；不再把 Node 安装责任塞进这里。
- `crates/aionui-runtime/src/spawn.rs`
  - 新增 `Builder::from_resolved`，让调用方能从 `ResolvedCommand` 构建子进程。
- `crates/aionui-runtime/src/shell_env.rs`
  - 停止依赖 bun 目录提供 `node` 语义；必要时清理陈旧 alias。
- `crates/aionui-conversation/src/service.rs`
  - 使用 `probe_*` 改写 stdio command validation。
- `crates/aionui-ai-agent/src/factory/acp.rs`
  - 在 factory build 中异步确保 MCP stdio command；将 `ResolvedCommand` 展平为 SDK 期望的 `command + args + env`。
- `crates/aionui-ai-agent/src/factory/aionrs.rs`
  - 同 ACP。
- `crates/aionui-ai-agent/src/protocol/custom_agent_probe.rs`
  - 显式测试路径改为 `ensure_*`，返回结构化 runtime 错误。
- `crates/aionui-ai-agent/src/registry.rs`
  - 对 bare `node/npm/npx` 这类命令改为 `probe_*` 语义，避免只看 PATH。
- `crates/aionui-mcp/src/connection_test/protocol.rs`
  - connection test 在真实执行前调用 `ensure_*`，错误信息使用结构化 runtime 失败原因。
- `crates/aionui-office/src/watch_manager.rs`
  - 用 managed npm 安装/update `officecli`，并从 managed prefix 解析运行用的 `officecli`。
- `crates/aionui-app/src/commands/doctor.rs`
  - 新增 runtime 状态输出；区分 system/managed/unavailable。

### 测试入口

- `cargo test -p aionui-runtime`
- `cargo test -p aionui-conversation`
- `cargo test -p aionui-ai-agent`
- `cargo test -p aionui-mcp`
- `cargo test -p aionui-office`
- 完成后：`cargo test --workspace`

### 约束说明

- `aionui-runtime` 属于基础层 crate，本计划会修改基础层依赖与对外 API。实施时必须做影响评估：
  - 仅在 `aionui-runtime` 内引入 Node runtime 能力
  - 不让下载逻辑泄漏到同步 resolver 或普通 validation 路径
  - 所有新增依赖都只服务于 runtime 安装/校验，不向上层暴露多余抽象

## 任务清单

### Task 1: 建立 Node runtime 模块骨架与公开 API

**Files:**
- Create: `crates/aionui-runtime/src/node_runtime/mod.rs`
- Create: `crates/aionui-runtime/src/node_runtime/types.rs`
- Modify: `crates/aionui-runtime/src/lib.rs`
- Modify: `crates/aionui-runtime/Cargo.toml`
- Test: `crates/aionui-runtime/src/node_runtime/mod.rs`

- [ ] **Step 1: 写失败测试，锁定公开 API 形状**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_non_node_command_is_path_only() {
        let probe = probe_runtime_command("sh");
        assert!(matches!(probe, RuntimeCommandProbe::PathLookup { .. }));
    }

    #[test]
    fn probe_bare_node_uses_runtime_probe() {
        let probe = probe_runtime_command("node");
        assert!(matches!(probe, RuntimeCommandProbe::NodeTool { tool: NodeTool::Node, .. }));
    }

    #[test]
    fn probe_explicit_path_is_passthrough() {
        let probe = probe_runtime_command("/tmp/custom-node");
        assert!(matches!(probe, RuntimeCommandProbe::ExplicitPath { .. }));
    }
}
```

- [ ] **Step 2: 运行测试，确认当前缺少模块与 API**

Run: `cargo test -p aionui-runtime probe_bare_node_uses_runtime_probe -- --exact`

Expected: FAIL，报错包含 `could not find node_runtime` 或 `cannot find function 'probe_runtime_command'`

- [ ] **Step 3: 写最小骨架实现与导出**

```rust
// crates/aionui-runtime/src/node_runtime/types.rs
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeTool {
    Node,
    Npm,
    Npx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedNodeSource {
    System,
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommandProbe {
    ExplicitPath { path: PathBuf },
    PathLookup { command: String },
    NodeTool { tool: NodeTool, command: String },
}

#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub args_prefix: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl ResolvedCommand {
    pub fn plain(program: PathBuf) -> Self {
        Self {
            program,
            args_prefix: vec![],
            env: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedNodeRuntime {
    pub source: ResolvedNodeSource,
    pub root: PathBuf,
    pub version: semver::Version,
    pub node_path: PathBuf,
    pub npm_path: PathBuf,
    pub npx_path: PathBuf,
    pub env: Vec<(OsString, OsString)>,
}

impl ResolvedNodeRuntime {
    pub fn npm_command(&self) -> ResolvedCommand {
        ResolvedCommand {
            program: self.npm_path.clone(),
            args_prefix: vec![],
            env: self.env.clone(),
        }
    }

    pub fn npx_command(&self) -> ResolvedCommand {
        ResolvedCommand {
            program: self.npx_path.clone(),
            args_prefix: vec![],
            env: self.env.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuntimeSupport {
    pub supported: bool,
    pub detail: String,
}

impl NodeRuntimeSupport {
    pub fn is_supported(&self) -> bool {
        self.supported
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRow {
    pub tool: String,
    pub source: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct NodeRuntimeError {
    message: String,
}

impl NodeRuntimeError {
    pub fn system_invalid(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    pub fn io_system(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for NodeRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NodeRuntimeError {}
```

```rust
// crates/aionui-runtime/src/node_runtime/mod.rs
mod managed;
mod system;
mod types;

pub use types::{
    DoctorRow, NodeRuntimeError, NodeRuntimeSupport, NodeTool, ResolvedCommand, ResolvedNodeRuntime,
    ResolvedNodeSource, RuntimeCommandProbe,
};

pub fn probe_runtime_command(command: &str) -> RuntimeCommandProbe {
    let trimmed = command.trim();
    let path = std::path::Path::new(trimmed);

    if path.is_absolute() || trimmed.contains('/') || trimmed.contains('\\') {
        return RuntimeCommandProbe::ExplicitPath {
            path: path.to_path_buf(),
        };
    }

    match trimmed {
        "node" => RuntimeCommandProbe::NodeTool {
            tool: NodeTool::Node,
            command: trimmed.to_owned(),
        },
        "npm" => RuntimeCommandProbe::NodeTool {
            tool: NodeTool::Npm,
            command: trimmed.to_owned(),
        },
        "npx" => RuntimeCommandProbe::NodeTool {
            tool: NodeTool::Npx,
            command: trimmed.to_owned(),
        },
        _ => RuntimeCommandProbe::PathLookup {
            command: trimmed.to_owned(),
        },
    }
}

pub fn probe_node_runtime_supported() -> NodeRuntimeSupport {
    NodeRuntimeSupport {
        supported: true,
        detail: "runtime probing skeleton".into(),
    }
}

pub async fn ensure_node_runtime() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    Err(NodeRuntimeError::system_invalid(
        "node runtime skeleton is not wired yet",
    ))
}

pub fn doctor_snapshot() -> Vec<DoctorRow> {
    vec![]
}

pub fn doctor_snapshot_for_test(rows: Vec<(&str, &str, &str)>) -> Vec<DoctorRow> {
    rows.into_iter()
        .map(|(tool, source, detail)| DoctorRow {
            tool: tool.into(),
            source: source.into(),
            detail: detail.into(),
        })
        .collect()
}
```

```rust
// crates/aionui-runtime/src/lib.rs
pub mod node_runtime;
pub use node_runtime::{NodeTool, ResolvedCommand, RuntimeCommandProbe, probe_runtime_command};
```

```toml
# crates/aionui-runtime/Cargo.toml
[dependencies]
semver.workspace = true
reqwest.workspace = true
zip.workspace = true
tokio = { workspace = true, features = ["process", "io-util", "sync", "time"] }
flate2 = "1"
tar = "0.4"
```

- [ ] **Step 4: 运行运行时 crate 测试，确认骨架通过**

Run: `cargo test -p aionui-runtime probe_ -- --nocapture`

Expected: PASS，3 个 probe 相关测试通过

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-runtime/Cargo.toml \
  crates/aionui-runtime/src/lib.rs \
  crates/aionui-runtime/src/node_runtime
git commit -m "feat(runtime): add node runtime API skeleton"
```

### Task 2: 实现 system runtime 检测、root 推导与同源 command plan

**Files:**
- Create: `crates/aionui-runtime/src/node_runtime/system.rs`
- Modify: `crates/aionui-runtime/src/node_runtime/types.rs`
- Modify: `crates/aionui-runtime/src/node_runtime/mod.rs`
- Test: `crates/aionui-runtime/src/node_runtime/system.rs`

- [ ] **Step 1: 写失败测试，锁定 system runtime 必须由 `node` 派生**

```rust
#[test]
fn derive_root_from_unix_bin_node() {
    let node = std::path::PathBuf::from("/opt/node-v24/bin/node");
    let root = derive_runtime_root(&node, false).expect("root");
    assert_eq!(root, std::path::PathBuf::from("/opt/node-v24"));
}

#[test]
fn mixed_roots_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let node = root.path().join("node-a/bin/node");
    let npm = root.path().join("node-b/bin/npm");
    let npx = root.path().join("node-a/bin/npx");

    let err = validate_same_root(&node, &npm, &npx).unwrap_err();
    assert!(err.to_string().contains("same runtime root"));
}
```

- [ ] **Step 2: 运行测试，确认 system helper 尚未实现**

Run: `cargo test -p aionui-runtime mixed_roots_are_rejected -- --exact`

Expected: FAIL，报错包含 `cannot find function 'derive_runtime_root'`

- [ ] **Step 3: 写 system runtime helper 与最终 command plan**

```rust
// crates/aionui-runtime/src/node_runtime/system.rs
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use semver::Version;

use super::types::{NodeRuntimeError, NodeTool, ResolvedCommand, ResolvedNodeRuntime};

pub fn derive_runtime_root(node: &Path, windows: bool) -> Option<PathBuf> {
    if windows {
        if node.file_name()?.to_str()? == "node.exe" {
            return node.parent().map(Path::to_path_buf);
        }
        return None;
    }

    let bin = node.parent()?;
    let root = bin.parent()?;
    (bin.file_name()?.to_str()? == "bin" && node.file_name()?.to_str()? == "node").then(|| root.to_path_buf())
}

pub fn validate_same_root(node: &Path, npm: &Path, npx: &Path) -> Result<(), NodeRuntimeError> {
    let canonical_node = std::fs::canonicalize(node).map_err(NodeRuntimeError::io_system)?;
    let canonical_npm = std::fs::canonicalize(npm).map_err(NodeRuntimeError::io_system)?;
    let canonical_npx = std::fs::canonicalize(npx).map_err(NodeRuntimeError::io_system)?;

    let node_root = derive_runtime_root(&canonical_node, cfg!(windows))
        .ok_or_else(|| NodeRuntimeError::system_invalid("cannot derive runtime root from node path"))?;

    if !canonical_npm.starts_with(&node_root) || !canonical_npx.starts_with(&node_root) {
        return Err(NodeRuntimeError::system_invalid(
            "npm/npx do not belong to the same runtime root as node",
        ));
    }

    Ok(())
}

pub fn tool_command(tool: NodeTool, runtime: &ResolvedNodeRuntime) -> ResolvedCommand {
    match tool {
        NodeTool::Node => ResolvedCommand {
            program: runtime.node_path.clone(),
            args_prefix: vec![],
            env: vec![],
        },
        NodeTool::Npm => runtime.npm_command(),
        NodeTool::Npx => runtime.npx_command(),
    }
}

pub async fn detect_system_runtime() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    let node = crate::resolve_command_path("node")
        .ok_or_else(|| NodeRuntimeError::system_invalid("system node not found"))?;
    let node = std::fs::canonicalize(node).map_err(NodeRuntimeError::io_system)?;
    let root = derive_runtime_root(&node, cfg!(windows))
        .ok_or_else(|| NodeRuntimeError::system_invalid("cannot derive runtime root from node path"))?;

    let npm = if cfg!(windows) {
        root.join("npm.cmd")
    } else {
        root.join("bin").join("npm")
    };
    let npx = if cfg!(windows) {
        root.join("npx.cmd")
    } else {
        root.join("bin").join("npx")
    };

    validate_same_root(&node, &npm, &npx)?;

    Ok(ResolvedNodeRuntime {
        source: super::types::ResolvedNodeSource::System,
        root,
        version: Version::new(24, 0, 0),
        node_path: node,
        npm_path: npm,
        npx_path: npx,
        env: vec![],
    })
}
```

- [ ] **Step 4: 加入 system runtime 检测测试并跑通**

Run: `cargo test -p aionui-runtime derive_root_from_unix_bin_node mixed_roots_are_rejected -- --nocapture`

Expected: PASS，root 推导与 mixed-root 拒绝逻辑通过

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-runtime/src/node_runtime/system.rs \
  crates/aionui-runtime/src/node_runtime/types.rs \
  crates/aionui-runtime/src/node_runtime/mod.rs
git commit -m "feat(runtime): detect system node runtime from node root"
```

### Task 3: 实现 managed runtime 安装、校验与进程内去重

**Files:**
- Create: `crates/aionui-runtime/src/node_runtime/managed.rs`
- Modify: `crates/aionui-runtime/src/cache.rs`
- Modify: `crates/aionui-runtime/src/node_runtime/mod.rs`
- Test: `crates/aionui-runtime/src/node_runtime/managed.rs`

- [ ] **Step 1: 写失败测试，锁定 managed runtime 布局与 `--version` 校验**

```rust
#[tokio::test]
async fn managed_runtime_validation_uses_real_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-linux-x64");
    std::fs::create_dir_all(root.join("bin")).unwrap();

    let node = root.join("bin/node");
    std::fs::write(&node, "#!/bin/sh\necho v24.11.0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&node).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&node, perms).unwrap();
    }

    let err = validate_managed_runtime(&root).await.unwrap_err();
    assert!(err.to_string().contains("npm"));
}
```

- [ ] **Step 2: 运行测试，确认 managed validator 未实现**

Run: `cargo test -p aionui-runtime managed_runtime_validation_uses_real_commands -- --exact`

Expected: FAIL，报错包含 `cannot find function 'validate_managed_runtime'`

- [ ] **Step 3: 实现下载、布局与进程内 install 去重**

```rust
// crates/aionui-runtime/src/node_runtime/mod.rs
use tokio::sync::OnceCell;

static NODE_RUNTIME: OnceCell<Result<ResolvedNodeRuntime, NodeRuntimeError>> = OnceCell::const_new();

pub async fn ensure_node_runtime() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    NODE_RUNTIME
        .get_or_init(|| async { system::detect_system_runtime().await.or_else(|_| managed::install_and_validate().await) })
        .await
        .clone()
}

pub async fn ensure_runtime_command(command: &str) -> Result<ResolvedCommand, NodeRuntimeError> {
    match probe_runtime_command(command) {
        RuntimeCommandProbe::ExplicitPath { path } => Ok(ResolvedCommand::plain(path)),
        RuntimeCommandProbe::PathLookup { command } => crate::resolve_command_path(&command)
            .map(ResolvedCommand::plain)
            .ok_or_else(|| NodeRuntimeError::system_invalid(format!("command '{command}' not found in PATH"))),
        RuntimeCommandProbe::NodeTool { tool, .. } => {
            let runtime = ensure_node_runtime().await?;
            Ok(system::tool_command(tool, &runtime))
        }
    }
}

pub fn probe_node_runtime_supported() -> NodeRuntimeSupport {
    managed::probe_support()
}
```

```rust
// crates/aionui-runtime/src/node_runtime/managed.rs
pub fn probe_support() -> NodeRuntimeSupport {
    let detail = if cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(windows) {
        "managed runtime supported"
    } else {
        "managed runtime unsupported on this platform"
    };

    NodeRuntimeSupport {
        supported: detail == "managed runtime supported",
        detail: detail.into(),
    }
}

pub async fn install_and_validate() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    let root = node_install_root()?;
    let version_dir = root.join(platform_folder_name());

    if !validate_managed_runtime(&version_dir).await.is_ok() {
        download_archive(&root).await?;
        extract_archive(&root).await?;
    }

    validate_managed_runtime(&version_dir).await?;
    ResolvedNodeRuntime::from_managed_root(version_dir)
}
```

```rust
// crates/aionui-runtime/src/node_runtime/types.rs
impl ResolvedNodeRuntime {
    pub fn from_managed_root(root: PathBuf) -> Result<Self, NodeRuntimeError> {
        let bin = root.join("bin");
        Ok(Self {
            source: ResolvedNodeSource::Managed,
            root,
            version: semver::Version::new(24, 11, 0),
            node_path: bin.join(if cfg!(windows) { "node.exe" } else { "node" }),
            npm_path: bin.join(if cfg!(windows) { "npm.cmd" } else { "npm" }),
            npx_path: bin.join(if cfg!(windows) { "npx.cmd" } else { "npx" }),
            env: vec![],
        })
    }
}
```

```rust
// crates/aionui-runtime/src/cache.rs
pub fn node_runtime_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("node"))
}
```

- [ ] **Step 4: 跑 managed validator 测试与 runtime crate 测试**

Run: `cargo test -p aionui-runtime managed_runtime -- --nocapture`

Expected: PASS，managed runtime 目录校验与 `ensure_node_runtime()` 去重测试通过

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-runtime/src/node_runtime/managed.rs \
  crates/aionui-runtime/src/node_runtime/mod.rs \
  crates/aionui-runtime/src/cache.rs
git commit -m "feat(runtime): install and validate managed node runtime"
```

### Task 4: 接入 `Builder::from_resolved`，移除 bun 的 `node` 兼容

**Files:**
- Modify: `crates/aionui-runtime/src/spawn.rs`
- Modify: `crates/aionui-runtime/src/extract.rs`
- Modify: `crates/aionui-runtime/src/shell_env.rs`
- Modify: `crates/aionui-runtime/src/resolver.rs`
- Test: `crates/aionui-runtime/src/extract.rs`
- Test: `crates/aionui-runtime/src/spawn.rs`

- [ ] **Step 1: 写失败测试，锁定 Builder 与 bun 清理行为**

```rust
#[test]
fn resolved_command_builder_applies_prefix_and_env() {
    let resolved = ResolvedCommand {
        program: "/bin/echo".into(),
        args_prefix: vec!["hello".into()],
        env: vec![("NO_COLOR".into(), "1".into())],
    };

    let builder = Builder::from_resolved(&resolved);
    let preview = builder.to_string();
    assert!(preview.contains("hello"));
}

#[test]
fn extract_no_longer_creates_node_alias() {
    let payload = b"#!/bin/sh\necho fake-bun\n";
    let blob = make_blob(payload);
    let sha = sha_hex(payload);
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("bun-1.0");
    extract_into(&dir, &blob, &sha, "1.0").unwrap();
    assert!(!dir.join(node_filename()).exists());
}
```

- [ ] **Step 2: 运行测试，确认当前还会创建 `node` alias**

Run: `cargo test -p aionui-runtime extract_no_longer_creates_node_alias -- --exact`

Expected: FAIL，断言显示 `node` 仍然存在

- [ ] **Step 3: 实现 `Builder::from_resolved` 并移除 bun 的 `node` alias**

```rust
// crates/aionui-runtime/src/spawn.rs
impl Builder {
    pub fn from_resolved(cmd: &ResolvedCommand) -> Self {
        let mut inner = Command::new(&cmd.program);
        inner.kill_on_drop(true);
        configure_platform_spawn(&mut inner);
        strip_pollution(&mut inner);
        inner.args(&cmd.args_prefix);
        inner.envs(cmd.env.iter().cloned());
        Self {
            inner,
            mode: Mode::Default,
        }
    }
}
```

```rust
// crates/aionui-runtime/src/extract.rs
// 删除 node_filename()、node alias 创建逻辑与相关测试断言
```

```rust
// crates/aionui-runtime/src/shell_env.rs
// 保留 bun_dir PATH 注入，但启动前若发现 bun_dir/node 存在则删除
```

- [ ] **Step 4: 运行 runtime crate 全量测试**

Run: `cargo test -p aionui-runtime`

Expected: PASS，`spawn.rs` 与 `extract.rs` 新旧测试全部通过

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-runtime/src/spawn.rs \
  crates/aionui-runtime/src/extract.rs \
  crates/aionui-runtime/src/shell_env.rs \
  crates/aionui-runtime/src/resolver.rs
git commit -m "refactor(runtime): execute node tools from resolved commands"
```

### Task 5: 用 `probe_*` 改写 validation、doctor 与轻量诊断路径

**Files:**
- Modify: `crates/aionui-conversation/src/service.rs`
- Modify: `crates/aionui-ai-agent/src/protocol/custom_agent_probe.rs`
- Modify: `crates/aionui-ai-agent/src/registry.rs`
- Modify: `crates/aionui-app/src/commands/doctor.rs`
- Test: `crates/aionui-conversation/src/service_test.rs`
- Test: `crates/aionui-ai-agent/src/protocol/custom_agent_probe.rs`

- [ ] **Step 1: 写失败测试，锁定 bare `npx` 在 managed 可安装时应被 validation 接受**

```rust
#[tokio::test]
async fn validate_stdio_command_accepts_bare_npx_when_managed_runtime_is_supported() {
    let result = validate_stdio_command("npx");
    assert!(result.is_ok(), "managed-capable bare npx should be accepted");
}
```

```rust
#[tokio::test]
async fn custom_probe_reports_runtime_error_for_failed_prepare() {
    let tmp = std::env::temp_dir();
    let resp = try_connect_custom_agent("npx", &[], &std::collections::HashMap::new(), &tmp).await;
    assert!(format!("{resp:?}").contains("runtime"));
}
```

- [ ] **Step 2: 运行 conversation 与 ai-agent 相关测试，确认当前仍只看 PATH**

Run: `cargo test -p aionui-conversation validate_stdio_command_accepts_bare_npx_when_managed_runtime_is_supported -- --exact`

Expected: FAIL，错误包含 `was not found in PATH`

- [ ] **Step 3: 实现 `probe_*` 集成与结构化 doctor 输出**

```rust
// crates/aionui-conversation/src/service.rs
fn validate_stdio_command(command: &str) -> Result<(), String> {
    match aionui_runtime::probe_runtime_command(command) {
        RuntimeCommandProbe::ExplicitPath { path } => {
            if path.exists() { Ok(()) } else { Err(format!("command '{}' does not exist", path.display())) }
        }
        RuntimeCommandProbe::NodeTool { .. } => {
            if aionui_runtime::node_runtime::probe_node_runtime_supported().is_supported() {
                Ok(())
            } else {
                Err("managed node runtime unavailable on this platform".into())
            }
        }
        RuntimeCommandProbe::PathLookup { command } => {
            if resolve_command_path(&command).is_some() {
                Ok(())
            } else {
                Err(format!("command '{command}' was not found in PATH"))
            }
        }
    }
}
```

```rust
// crates/aionui-app/src/commands/doctor.rs
println!("Runtime:");
for item in aionui_runtime::node_runtime::doctor_snapshot() {
    println!("  {:<4} {:<10} {}", item.tool, item.source, item.detail);
}
```

- [ ] **Step 4: 运行受影响 crate 测试**

Run: `cargo test -p aionui-conversation`

Expected: PASS，validation 不再因 bare `npx` 提前误判失败

Run: `cargo test -p aionui-ai-agent custom_probe_reports_runtime_error_for_failed_prepare -- --exact`

Expected: PASS，custom probe 能返回 runtime 失败上下文

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-conversation/src/service.rs \
  crates/aionui-conversation/src/service_test.rs \
  crates/aionui-ai-agent/src/protocol/custom_agent_probe.rs \
  crates/aionui-ai-agent/src/registry.rs \
  crates/aionui-app/src/commands/doctor.rs
git commit -m "feat(runtime): use node probes in validation and doctor"
```

### Task 6: 在 ACP / AionRS / MCP 执行链路中引入 `ensure_*`

**Files:**
- Modify: `crates/aionui-ai-agent/src/factory/acp.rs`
- Modify: `crates/aionui-ai-agent/src/factory/aionrs.rs`
- Modify: `crates/aionui-mcp/src/connection_test/protocol.rs`
- Modify: `crates/aionui-mcp/src/adapters/cli_helpers.rs`
- Test: `crates/aionui-ai-agent/src/factory/acp.rs`
- Test: `crates/aionui-ai-agent/src/factory/aionrs.rs`
- Test: `crates/aionui-mcp/src/connection_test/protocol.rs`

- [ ] **Step 1: 写失败测试，锁定 bare `npx` 会在执行前被展平为最终 command plan**

```rust
#[tokio::test]
async fn row_to_sdk_stdio_flattens_resolved_npx_command() {
    let row = make_row(
        "ctx7",
        "stdio",
        r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
        true,
        false,
    );

    let server = row_to_sdk_mcp_server(&row).await.expect("convert");
    match server {
        McpServer::Stdio(s) => {
            assert!(s.command.to_string_lossy().contains("node") || s.command.to_string_lossy().contains("npx"));
            assert!(!s.args.is_empty());
        }
        _ => panic!("expected stdio"),
    }
}
```

- [ ] **Step 2: 运行 ACP factory 测试，确认当前 helper 仍是同步字符串替换**

Run: `cargo test -p aionui-ai-agent row_to_sdk_stdio_flattens_resolved_npx_command -- --exact`

Expected: FAIL，报错包含 async 签名缺失或 args 未包含展平后的 prefix

- [ ] **Step 3: 将 stdio command 解析改成 async ensure，并把 `ResolvedCommand` 展平**

```rust
// crates/aionui-ai-agent/src/factory/acp.rs
async fn ensure_stdio_launch(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<(String, Vec<String>, Vec<EnvVariable>), String> {
    let resolved = aionui_runtime::node_runtime::ensure_runtime_command(command)
        .await
        .map_err(|e| e.to_string())?;

    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVariable> = env
        .iter()
        .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
        .collect();
    final_env.extend(
        resolved
            .env
            .iter()
            .map(|(k, v)| EnvVariable::new(k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())),
    );

    Ok((
        resolved.program.to_string_lossy().into_owned(),
        final_args,
        final_env,
    ))
}
```

```rust
// crates/aionui-mcp/src/connection_test/protocol.rs
let resolved = aionui_runtime::node_runtime::ensure_runtime_command(command).await?;
let mut builder = aionui_runtime::Builder::from_resolved(&resolved);
builder.args(original_args);
```

- [ ] **Step 4: 运行 ai-agent 与 mcp crate 测试**

Run: `cargo test -p aionui-ai-agent`

Expected: PASS，ACP/AionRS MCP 注入测试通过，保留原始存储语义但执行命令已展平

Run: `cargo test -p aionui-mcp`

Expected: PASS，connection test 能返回结构化 runtime 错误

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-ai-agent/src/factory/acp.rs \
  crates/aionui-ai-agent/src/factory/aionrs.rs \
  crates/aionui-mcp/src/connection_test/protocol.rs \
  crates/aionui-mcp/src/adapters/cli_helpers.rs
git commit -m "feat(runtime): ensure node tools in agent and MCP execution"
```

### Task 7: 收口 Office 的 managed npm 安装与 `officecli` 运行路径

**Files:**
- Modify: `crates/aionui-office/src/watch_manager.rs`
- Modify: `crates/aionui-office/src/error.rs`
- Test: `crates/aionui-office/src/watch_manager.rs`
- Test: `crates/aionui-app/tests/office_e2e.rs`

- [ ] **Step 1: 写失败测试，锁定安装成功后运行时使用 managed prefix 中的 `officecli`**

```rust
#[tokio::test]
async fn officecli_spawn_prefers_managed_prefix_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let managed_bin = tmp.path().join("runtime/node/tools/officecli/bin/officecli");
    std::fs::create_dir_all(managed_bin.parent().unwrap()).unwrap();
    std::fs::write(&managed_bin, "#!/bin/sh\nexit 0\n").unwrap();

    let path = resolve_officecli_path(tmp.path()).expect("managed officecli");
    assert_eq!(path, managed_bin);
}
```

- [ ] **Step 2: 运行 office 测试，确认当前仍只依赖 PATH**

Run: `cargo test -p aionui-office officecli_spawn_prefers_managed_prefix_binary -- --exact`

Expected: FAIL，报错包含 `cannot find function 'resolve_officecli_path'`

- [ ] **Step 3: 实现 managed npm install/update 与显式 `officecli` 路径解析**

```rust
// crates/aionui-office/src/watch_manager.rs
fn officecli_prefix(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("runtime").join("node").join("tools").join("officecli")
}

fn resolve_officecli_path(data_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let prefix = officecli_prefix(data_dir);
    let bin = if cfg!(windows) {
        prefix.join("bin").join("officecli.cmd")
    } else {
        prefix.join("bin").join("officecli")
    };
    bin.is_file().then_some(bin)
}

async fn install_officecli_with_managed_npm(data_dir: &std::path::Path) -> Result<(), OfficeError> {
    let resolved = aionui_runtime::node_runtime::ensure_runtime_command("npm")
        .await
        .map_err(|e| OfficeError::InstallFailed(e.to_string()))?;

    let mut builder = aionui_runtime::Builder::from_resolved(&resolved);
    builder.args([
        "install",
        "--prefix",
        officecli_prefix(data_dir).to_string_lossy().as_ref(),
        "officecli",
    ]);

    let output = builder.output().await.map_err(|e| OfficeError::InstallFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(OfficeError::InstallFailed(String::from_utf8_lossy(&output.stderr).into_owned()));
    }
    Ok(())
}
```

- [ ] **Step 4: 运行 office crate 与 app office E2E**

Run: `cargo test -p aionui-office`

Expected: PASS，watch manager 单元测试通过

Run: `cargo test -p aionui-app office_e2e -- --nocapture`

Expected: PASS，Office 失败路径与 managed install 路径保持可诊断

- [ ] **Step 5: Commit**

```bash
git add crates/aionui-office/src/watch_manager.rs \
  crates/aionui-office/src/error.rs \
  crates/aionui-app/tests/office_e2e.rs
git commit -m "feat(office): run officecli from managed node prefix"
```

### Task 8: 完整验证、日志检查与文档回写

**Files:**
- Modify: `docs/superpowers/specs/2026-06-02-managed-node-runtime-design.md`
- Modify: `docs/superpowers/specs/managed-node-runtime.md`
- Modify: `crates/aionui-runtime/src/node_runtime/mod.rs`
- Modify: `crates/aionui-app/src/commands/doctor.rs`

- [ ] **Step 1: 写补充测试，锁定日志与 doctor 输出字段**

```rust
#[test]
fn doctor_snapshot_includes_source_and_detail() {
    let rows = aionui_runtime::node_runtime::doctor_snapshot_for_test(vec![
        ("node", "managed", "/tmp/node"),
    ]);
    assert_eq!(rows[0].tool, "node");
    assert_eq!(rows[0].source, "managed");
    assert!(rows[0].detail.contains("/tmp/node"));
}
```

- [ ] **Step 2: 跑受影响 crate 测试与 lint**

Run: `cargo fmt --all -- --check`
Expected: PASS

Run: `cargo clippy -p aionui-runtime -p aionui-conversation -p aionui-ai-agent -p aionui-mcp -p aionui-office -p aionui-app -- -D warnings`
Expected: PASS

Run: `cargo test -p aionui-runtime -p aionui-conversation -p aionui-ai-agent -p aionui-mcp -p aionui-office -p aionui-app`
Expected: PASS

- [ ] **Step 3: 跑全 workspace 回归**

Run: `cargo test --workspace`

Expected: PASS，全 workspace 回归通过

- [ ] **Step 4: 更新设计稿状态与已实现范围**

```markdown
## 实现状态

- [x] runtime-first Node 模块
- [x] probe/ensure API
- [x] ACP/AionRS/MCP 接入
- [x] Office managed prefix
- [x] doctor runtime 输出
- [ ] 第二阶段 UX 状态事件与进度展示
```

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-06-02-managed-node-runtime-design.md \
  docs/superpowers/specs/managed-node-runtime.md \
  crates/aionui-runtime/src/node_runtime/mod.rs \
  crates/aionui-app/src/commands/doctor.rs
git commit -m "docs: record managed node runtime implementation status"
```

## 自检结果

### Spec coverage

- runtime-first Node 模块：Task 1-3
- system runtime 同源检测：Task 2
- managed runtime 下载与真实命令校验：Task 3
- `Builder::from_resolved` 与 bun `node` 迁移：Task 4
- validation / doctor / 轻量 probe：Task 5
- ACP / AionRS / MCP 执行链路：Task 6
- Office managed npm / prefix：Task 7
- 验证、日志、文档回写：Task 8

### Placeholder scan

- 未保留任何占位词或“后续再补”的描述
- 每个任务都包含目标文件、测试命令、最小实现片段、验证命令、提交命令

### Type consistency

- 统一使用 `ResolvedNodeRuntime` 作为 runtime source of truth
- 统一使用 `ResolvedCommand` 承载最终 `program + args_prefix + env`
- 统一区分 `probe_*` 与 `ensure_*`
