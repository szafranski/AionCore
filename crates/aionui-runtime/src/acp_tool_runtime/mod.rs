mod types;

use std::error::Error as StdError;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::Builder;
use crate::cache;
use crate::http_client;
use crate::managed_resources;
use crate::node_runtime::DoctorRow;
use crate::node_runtime::ensure_node_runtime_with_reporter;

pub use types::{
    ManagedAcpToolError, ManagedAcpToolFailureKind, ManagedAcpToolId, ManagedAcpToolProgress,
    ManagedAcpToolProgressPhase, ManagedAcpToolProgressReporter, ManagedAcpToolSupport, ResolvedManagedAcpTool,
    SharedManagedAcpToolProgressReporter,
};

const MANAGED_ACP_TOOL_CDN_BASE: &str = "https://static.aionui.com/managed/acp";
const MANAGED_ACP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MANAGED_ACP_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const MANAGED_ACP_DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MANAGED_ACP_PROGRESS_STEP_BYTES: u64 = 5 * 1024 * 1024;
const MANAGED_ACP_DOWNLOAD_ATTEMPTS: usize = 2;

static INSTALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

#[derive(Debug, Clone, Copy)]
struct PlatformSpec {
    manifest_key: &'static str,
    archive_ext: &'static str,
    npm_os: &'static str,
    npm_cpu: &'static str,
}

#[derive(Debug, Deserialize)]
struct ManagedAcpManifest {
    tool: String,
    version: String,
    artifacts: std::collections::BTreeMap<String, ManagedAcpArtifact>,
}

#[derive(Debug, Deserialize)]
struct ManagedAcpArtifact {
    url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct RemoteSource {
    label: &'static str,
    url: String,
}

#[derive(Debug, Serialize)]
struct DevPackageJson<'a> {
    name: &'a str,
    private: bool,
}

#[derive(Debug, Deserialize)]
struct InstalledPackageJson {
    name: String,
    #[serde(default)]
    bin: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct LocalArtifactManifestWrite {
    entrypoint: String,
    path_entries: Vec<String>,
}

pub async fn ensure_managed_acp_tool(tool: ManagedAcpToolId) -> Result<ResolvedManagedAcpTool, ManagedAcpToolError> {
    ensure_managed_acp_tool_with_reporter(tool, None).await
}

pub async fn ensure_managed_acp_tool_with_reporter(
    tool: ManagedAcpToolId,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<ResolvedManagedAcpTool, ManagedAcpToolError> {
    let spec = platform_spec()?;
    let root = tool_root(tool, spec)?;

    if let Ok(installed) = validate_tool_root(tool, &root, reporter) {
        return Ok(installed);
    }

    let lock = INSTALL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    let _guard = lock.lock().await;

    if let Ok(installed) = validate_tool_root(tool, &root, reporter) {
        return Ok(installed);
    }

    if let Some(installed) = activate_local_tool_source(tool, spec, &root, reporter)? {
        return Ok(installed);
    }

    if maybe_prepare_dev_local_tool_source(tool, spec, reporter).await?
        && let Some(installed) = activate_local_tool_source(tool, spec, &root, reporter)?
    {
        return Ok(installed);
    }

    install_tool_with_retry(tool, spec, &root, reporter).await?;
    validate_tool_root(tool, &root, reporter)
}

pub fn probe_managed_acp_tool_supported(tool: ManagedAcpToolId) -> ManagedAcpToolSupport {
    match platform_spec() {
        Ok(spec) => match tool_root(tool, spec) {
            Ok(root) => ManagedAcpToolSupport {
                supported: true,
                detail: format!(
                    "managed {} artifact supported under {}",
                    tool.display_name(),
                    root.display()
                ),
            },
            Err(error) => ManagedAcpToolSupport {
                supported: false,
                detail: error.to_string(),
            },
        },
        Err(error) => ManagedAcpToolSupport {
            supported: false,
            detail: error.to_string(),
        },
    }
}

pub fn doctor_snapshot() -> Vec<DoctorRow> {
    [ManagedAcpToolId::CodexAcp, ManagedAcpToolId::ClaudeAgentAcp]
        .into_iter()
        .map(doctor_row)
        .collect()
}

fn doctor_row(tool: ManagedAcpToolId) -> DoctorRow {
    match platform_spec() {
        Ok(spec) => match tool_root(tool, spec) {
            Ok(root) if !root.exists() => {
                if let Some(source_root) =
                    managed_resources::acp_tool_sources(tool.slug(), tool.version(), spec.manifest_key)
                        .into_iter()
                        .next()
                        .map(|source| source.root)
                {
                    return DoctorRow {
                        tool: tool.slug().into(),
                        source: "local".into(),
                        detail: source_root.display().to_string(),
                    };
                }
                DoctorRow {
                    tool: tool.slug().into(),
                    source: "managed".into(),
                    detail: format!("not installed (expected under {})", root.display()),
                }
            }
            Ok(root) => match validate_tool_root(tool, &root, None) {
                Ok(resolved) => DoctorRow {
                    tool: tool.slug().into(),
                    source: "managed".into(),
                    detail: resolved.entrypoint.display().to_string(),
                },
                Err(error) if root.exists() => DoctorRow {
                    tool: tool.slug().into(),
                    source: "managed".into(),
                    detail: format!("{} (root: {})", error, root.display()),
                },
                Err(_) => DoctorRow {
                    tool: tool.slug().into(),
                    source: "managed".into(),
                    detail: format!("not installed (expected under {})", root.display()),
                },
            },
            Err(error) => DoctorRow {
                tool: tool.slug().into(),
                source: "unavailable".into(),
                detail: error.to_string(),
            },
        },
        Err(error) => DoctorRow {
            tool: tool.slug().into(),
            source: "unavailable".into(),
            detail: error.to_string(),
        },
    }
}

#[derive(Debug)]
struct InstallLockGuard {
    file: fs::File,
}

impl InstallLockGuard {
    fn acquire(path: &Path, reporter: Option<&dyn ManagedAcpToolProgressReporter>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            emit_progress(
                reporter,
                ManagedAcpToolProgress::waiting_for_lock(
                    "waiting for another process to finish preparing the managed ACP tool",
                ),
            );
            FileExt::lock_exclusive(&file)?;
        }
        Ok(Self { file })
    }
}

impl Drop for InstallLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

async fn install_tool_with_retry(
    tool: ManagedAcpToolId,
    spec: PlatformSpec,
    root: &Path,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<(), ManagedAcpToolError> {
    let mut last_error = None;
    for attempt in 1..=MANAGED_ACP_DOWNLOAD_ATTEMPTS {
        match install_tool(tool, spec, root, reporter).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt < MANAGED_ACP_DOWNLOAD_ATTEMPTS => {
                warn!(
                    tool = tool.slug(),
                    attempt,
                    max_attempts = MANAGED_ACP_DOWNLOAD_ATTEMPTS,
                    error = %error,
                    "managed ACP tool install attempt failed; retrying"
                );
                last_error = Some(error);
            }
            Err(error) => return Err(install_error(error, reporter)),
        }
    }
    Err(last_error
        .map(|error| install_error(error, reporter))
        .unwrap_or_else(|| ManagedAcpToolError::invalid("managed ACP tool install failed")))
}

async fn install_tool(
    tool: ManagedAcpToolId,
    spec: PlatformSpec,
    root: &Path,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<(), ManagedAcpToolError> {
    let client = build_http_client()?;
    let manifest = fetch_manifest(&client, tool).await?;
    let artifact = manifest.artifacts.get(spec.manifest_key).ok_or_else(|| {
        ManagedAcpToolError::invalid(format!(
            "managed ACP manifest missing platform {} for {}",
            spec.manifest_key,
            tool.slug()
        ))
    })?;

    let _lock = InstallLockGuard::acquire(&install_lock_path(root), reporter).map_err(ManagedAcpToolError::io)?;

    if root.exists() {
        let _ = fs::remove_dir_all(root);
    }
    fs::create_dir_all(root).map_err(ManagedAcpToolError::io)?;

    let archive_path = archive_download_path(root, spec);
    let _ = fs::remove_file(&archive_path);

    download_artifact_with_fallback(tool, spec, artifact, &client, &archive_path, reporter).await?;

    emit_progress(
        reporter,
        ManagedAcpToolProgress::extracting(format!("extracting managed {} artifact", tool.display_name())),
    );
    match spec.archive_ext {
        "tar.zst" => extract_tar_zst(&archive_path, root)?,
        "zip" => extract_zip(&archive_path, root)?,
        ext => {
            return Err(ManagedAcpToolError::invalid(format!(
                "unsupported ACP archive extension: {ext}"
            )));
        }
    }
    let _ = fs::remove_file(&archive_path);

    validate_tool_root(tool, root, reporter)?;
    Ok(())
}

fn activate_local_tool_source(
    tool: ManagedAcpToolId,
    spec: PlatformSpec,
    root: &Path,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<Option<ResolvedManagedAcpTool>, ManagedAcpToolError> {
    for source in managed_resources::acp_tool_sources(tool.slug(), tool.version(), spec.manifest_key) {
        emit_progress(
            reporter,
            ManagedAcpToolProgress::extracting(format!(
                "activating managed {} artifact from {}",
                tool.display_name(),
                source.root.display()
            )),
        );

        if let Err(error) = managed_resources::materialize_directory(&source.root, root) {
            warn!(
                tool = tool.slug(),
                version = tool.version(),
                source_root = %source.root.display(),
                target_root = %root.display(),
                error = %error,
                "failed to activate local managed ACP tool source"
            );
            continue;
        }

        match validate_tool_root(tool, root, reporter) {
            Ok(resolved) => {
                info!(
                    tool = tool.slug(),
                    version = tool.version(),
                    source_root = %source.root.display(),
                    target_root = %root.display(),
                    "managed ACP tool activated from local resources"
                );
                return Ok(Some(resolved));
            }
            Err(error) => {
                warn!(
                    tool = tool.slug(),
                    version = tool.version(),
                    source_root = %source.root.display(),
                    target_root = %root.display(),
                    error = %error,
                    "local managed ACP tool source failed validation"
                );
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    Ok(None)
}

async fn maybe_prepare_dev_local_tool_source(
    tool: ManagedAcpToolId,
    spec: PlatformSpec,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<bool, ManagedAcpToolError> {
    if !managed_resources::should_auto_prepare_dev_local() {
        return Ok(false);
    }

    let dev_root = managed_resources::ensure_dev_local_root().map_err(ManagedAcpToolError::io)?;
    let target_root = dev_root
        .join("acp")
        .join(tool.slug())
        .join(tool.version())
        .join(spec.manifest_key);

    if target_root.is_dir() {
        return Ok(false);
    }

    emit_progress(
        reporter,
        ManagedAcpToolProgress::extracting(format!(
            "preparing managed {} artifact for local development",
            tool.display_name()
        )),
    );
    info!(
        tool = tool.slug(),
        version = tool.version(),
        platform = spec.manifest_key,
        target_root = %target_root.display(),
        "preparing managed ACP tool into dev-local resources"
    );

    let node_runtime = ensure_node_runtime_with_reporter(None)
        .await
        .map_err(|error| ManagedAcpToolError::invalid(format!("prepare managed Node runtime: {error}")))?;
    let node_dir_name = node_runtime
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ManagedAcpToolError::invalid("managed Node runtime root missing directory name"))?;
    let _ = managed_resources::export_node_runtime_to_dev_local(&node_runtime.root, node_dir_name)
        .map_err(ManagedAcpToolError::io)?;

    let staging_root = dev_prepare_staging_root(tool, spec, &dev_root);
    if staging_root.exists() {
        let _ = fs::remove_dir_all(&staging_root);
    }
    fs::create_dir_all(&staging_root).map_err(ManagedAcpToolError::io)?;

    let result = prepare_dev_local_tool_source(tool, spec, &node_runtime, &staging_root).await;
    if let Err(error) = fs::remove_dir_all(&staging_root)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            tool = tool.slug(),
            version = tool.version(),
            staging_root = %staging_root.display(),
            error = %error,
            "failed to clean up managed ACP dev preparation staging directory"
        );
    }

    result?;
    Ok(true)
}

async fn prepare_dev_local_tool_source(
    tool: ManagedAcpToolId,
    spec: PlatformSpec,
    node_runtime: &crate::ResolvedNodeRuntime,
    staging_root: &Path,
) -> Result<(), ManagedAcpToolError> {
    let project_dir = staging_root.join("project");
    let npm_cache_dir = staging_root.join("npm-cache");
    fs::create_dir_all(&project_dir).map_err(ManagedAcpToolError::io)?;
    fs::create_dir_all(&npm_cache_dir).map_err(ManagedAcpToolError::io)?;

    write_dev_package_json(&project_dir)?;
    run_npm_prepare_step(
        node_runtime,
        &project_dir,
        &npm_cache_dir,
        [
            "install",
            "--package-lock-only",
            "--ignore-scripts",
            "--include=optional",
            "--fund=false",
            "--audit=false",
            "--save-exact",
            "--os",
            spec.npm_os,
            "--cpu",
            spec.npm_cpu,
            &format!("{}@{}", tool.package_name(), tool.version()),
        ],
        "generate managed ACP dev lockfile",
    )
    .await?;
    run_npm_prepare_step(
        node_runtime,
        &project_dir,
        &npm_cache_dir,
        [
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--include=optional",
            "--fund=false",
            "--audit=false",
            "--os",
            spec.npm_os,
            "--cpu",
            spec.npm_cpu,
        ],
        "install managed ACP dev artifact",
    )
    .await?;

    let manifest = build_local_artifact_manifest(tool, &project_dir)?;
    validate_bridge_entrypoint(&project_dir, &manifest)?;
    validate_platform_binary(tool, &project_dir, spec)?;

    let manifest_path = project_dir.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| ManagedAcpToolError::invalid(format!("serialize local ACP manifest: {error}")))?,
    )
    .map_err(ManagedAcpToolError::io)?;

    let target_root =
        managed_resources::export_acp_tool_to_dev_local(&project_dir, tool.slug(), tool.version(), spec.manifest_key)
            .map_err(ManagedAcpToolError::io)?;
    info!(
        tool = tool.slug(),
        version = tool.version(),
        platform = spec.manifest_key,
        target_root = %target_root.display(),
        "prepared managed ACP tool under dev-local resources"
    );
    Ok(())
}

async fn run_npm_prepare_step<const N: usize>(
    node_runtime: &crate::ResolvedNodeRuntime,
    project_dir: &Path,
    npm_cache_dir: &Path,
    args: [&str; N],
    label: &str,
) -> Result<(), ManagedAcpToolError> {
    let mut builder = Builder::from_resolved(&node_runtime.npm_command());
    builder
        .current_dir(project_dir)
        .env("npm_config_cache", npm_cache_dir)
        .args(args);
    let output = builder.output().await.map_err(ManagedAcpToolError::io)?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let detail = if stderr.is_empty() {
        stdout
    } else if stdout.is_empty() {
        stderr
    } else {
        format!("{stderr}; stdout: {stdout}")
    };
    Err(ManagedAcpToolError::invalid(format!(
        "{label} failed with exit code {:?}: {detail}",
        output.status.code()
    )))
}

fn write_dev_package_json(project_dir: &Path) -> Result<(), ManagedAcpToolError> {
    let package_json = DevPackageJson {
        name: "aionui-managed-acp-dev",
        private: true,
    };
    fs::write(
        project_dir.join("package.json"),
        serde_json::to_vec_pretty(&package_json)
            .map_err(|error| ManagedAcpToolError::invalid(format!("serialize dev package.json: {error}")))?,
    )
    .map_err(ManagedAcpToolError::io)
}

fn build_local_artifact_manifest(
    tool: ManagedAcpToolId,
    project_dir: &Path,
) -> Result<LocalArtifactManifestWrite, ManagedAcpToolError> {
    let package_segments = package_path_segments(tool.package_name());
    let package_json_path = package_json_path(project_dir, tool.package_name());
    let contents = fs::read_to_string(&package_json_path).map_err(ManagedAcpToolError::io)?;
    let package_json: InstalledPackageJson = serde_json::from_str(&contents).map_err(|error| {
        ManagedAcpToolError::invalid(format!(
            "parse installed package manifest failed for {}: {error}",
            package_json_path.display()
        ))
    })?;
    let entrypoint_rel = resolve_package_bin_entry(&package_json.name, &package_json.bin)?;

    let mut entrypoint = PathBuf::from("node_modules");
    for segment in &package_segments {
        entrypoint.push(segment);
    }
    entrypoint.push(entrypoint_rel);

    Ok(LocalArtifactManifestWrite {
        entrypoint: normalize_slashes(&entrypoint),
        path_entries: vec!["node_modules/.bin".into()],
    })
}

fn validate_bridge_entrypoint(
    project_dir: &Path,
    manifest: &LocalArtifactManifestWrite,
) -> Result<(), ManagedAcpToolError> {
    let entrypoint = project_dir.join(&manifest.entrypoint);
    if !entrypoint.is_file() {
        return Err(ManagedAcpToolError::invalid(format!(
            "resolved managed ACP entrypoint missing: {}",
            entrypoint.display()
        )));
    }
    Ok(())
}

fn validate_platform_binary(
    tool: ManagedAcpToolId,
    project_dir: &Path,
    spec: PlatformSpec,
) -> Result<(), ManagedAcpToolError> {
    let expected = match tool {
        ManagedAcpToolId::CodexAcp => {
            let mut path = project_dir
                .join("node_modules")
                .join(format!("@zed-industries/codex-acp-{}", spec.manifest_key))
                .join("bin")
                .join("codex-acp");
            if spec.manifest_key.starts_with("win32-") {
                path.set_extension("exe");
            }
            path
        }
        ManagedAcpToolId::ClaudeAgentAcp => {
            let mut path = project_dir
                .join("node_modules")
                .join(format!("@anthropic-ai/claude-agent-sdk-{}", spec.manifest_key))
                .join("claude");
            if spec.manifest_key.starts_with("win32-") {
                path.set_extension("exe");
            }
            path
        }
    };

    if expected.is_file() {
        Ok(())
    } else {
        Err(ManagedAcpToolError::invalid(format!(
            "expected managed {} platform binary missing: {}",
            tool.display_name(),
            expected.display()
        )))
    }
}

fn package_json_path(project_dir: &Path, package_name: &str) -> PathBuf {
    let mut path = project_dir.join("node_modules");
    for segment in package_path_segments(package_name) {
        path.push(segment);
    }
    path.join("package.json")
}

fn package_path_segments(package_name: &str) -> Vec<&str> {
    package_name.split('/').collect()
}

fn resolve_package_bin_entry(package_name: &str, bin_field: &serde_json::Value) -> Result<String, ManagedAcpToolError> {
    match bin_field {
        serde_json::Value::String(value) if !value.is_empty() => Ok(value.clone()),
        serde_json::Value::Object(entries) => {
            let short_name = package_name
                .rsplit('/')
                .next()
                .ok_or_else(|| ManagedAcpToolError::invalid("package name missing short name"))?;
            for key in [package_name, short_name] {
                if let Some(serde_json::Value::String(value)) = entries.get(key)
                    && !value.is_empty()
                {
                    return Ok(value.clone());
                }
            }
            entries
                .values()
                .find_map(|value| match value {
                    serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
                    _ => None,
                })
                .ok_or_else(|| {
                    ManagedAcpToolError::invalid(format!("package {package_name} does not expose a usable bin entry"))
                })
        }
        _ => Err(ManagedAcpToolError::invalid(format!(
            "package {package_name} does not expose a usable bin entry"
        ))),
    }
}

fn normalize_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn dev_prepare_staging_root(tool: ManagedAcpToolId, spec: PlatformSpec, dev_root: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    dev_root.join(".staging").join(format!(
        "{}-{}-{}-{}",
        tool.slug(),
        tool.version(),
        spec.manifest_key,
        nonce
    ))
}

fn validate_tool_root(
    tool: ManagedAcpToolId,
    root: &Path,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<ResolvedManagedAcpTool, ManagedAcpToolError> {
    emit_progress(
        reporter,
        ManagedAcpToolProgress::validating(format!(
            "validating managed {} artifact under {}",
            tool.display_name(),
            root.display()
        )),
    );
    let manifest = read_local_manifest(root)?;
    let entrypoint = root.join(&manifest.entrypoint);
    if !entrypoint.is_file() {
        return Err(ManagedAcpToolError::invalid(format!(
            "managed ACP entrypoint missing: {}",
            entrypoint.display()
        )));
    }

    let env_path_entries = manifest
        .path_entries
        .into_iter()
        .map(|entry| root.join(entry))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();

    let resolved = ResolvedManagedAcpTool {
        id: tool,
        version: tool.version().to_owned(),
        root: root.to_path_buf(),
        entrypoint,
        env_path_entries,
    };
    emit_progress(
        reporter,
        ManagedAcpToolProgress::ready(format!("managed {} artifact is ready", tool.display_name())),
    );
    Ok(resolved)
}

fn platform_spec() -> Result<PlatformSpec, ManagedAcpToolError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(PlatformSpec {
            manifest_key: "darwin-arm64",
            archive_ext: "tar.zst",
            npm_os: "darwin",
            npm_cpu: "arm64",
        }),
        ("macos", "x86_64") => Ok(PlatformSpec {
            manifest_key: "darwin-x64",
            archive_ext: "tar.zst",
            npm_os: "darwin",
            npm_cpu: "x64",
        }),
        ("linux", "aarch64") => Ok(PlatformSpec {
            manifest_key: "linux-arm64",
            archive_ext: "tar.zst",
            npm_os: "linux",
            npm_cpu: "arm64",
        }),
        ("linux", "x86_64") => Ok(PlatformSpec {
            manifest_key: "linux-x64",
            archive_ext: "tar.zst",
            npm_os: "linux",
            npm_cpu: "x64",
        }),
        ("windows", "x86_64") => Ok(PlatformSpec {
            manifest_key: "win32-x64",
            archive_ext: "zip",
            npm_os: "win32",
            npm_cpu: "x64",
        }),
        ("windows", "aarch64") => Ok(PlatformSpec {
            manifest_key: "win32-arm64",
            archive_ext: "zip",
            npm_os: "win32",
            npm_cpu: "arm64",
        }),
        (os, arch) => Err(ManagedAcpToolError::unsupported_platform(format!(
            "managed ACP tool unsupported on {os}/{arch}"
        ))),
    }
}

fn tool_root(tool: ManagedAcpToolId, spec: PlatformSpec) -> Result<PathBuf, ManagedAcpToolError> {
    cache::managed_acp_tool_root()
        .map(|root| root.join(tool.slug()).join(tool.version()).join(spec.manifest_key))
        .ok_or_else(|| ManagedAcpToolError::invalid("runtime cache dir unavailable"))
}

#[derive(Debug, Deserialize)]
struct LocalArtifactManifest {
    entrypoint: String,
    #[serde(default)]
    path_entries: Vec<String>,
}

fn read_local_manifest(root: &Path) -> Result<LocalArtifactManifest, ManagedAcpToolError> {
    let path = root.join("manifest.json");
    let contents = fs::read_to_string(&path).map_err(ManagedAcpToolError::io)?;
    serde_json::from_str(&contents).map_err(|error| {
        ManagedAcpToolError::invalid(format!(
            "parse local ACP manifest failed for {}: {error}",
            path.display()
        ))
    })
}

async fn fetch_manifest(
    client: &reqwest::Client,
    tool: ManagedAcpToolId,
) -> Result<ManagedAcpManifest, ManagedAcpToolError> {
    let sources = manifest_sources(tool);
    let mut last_error = None;

    for (index, source) in sources.iter().enumerate() {
        match fetch_manifest_from_source(client, tool, source).await {
            Ok(manifest) => {
                if index > 0 {
                    info!(
                        tool = tool.slug(),
                        version = tool.version(),
                        source = source.label,
                        url = %source.url,
                        "managed ACP manifest fallback source selected"
                    );
                }
                return Ok(manifest);
            }
            Err(error) if index + 1 < sources.len() => {
                warn!(
                    tool = tool.slug(),
                    version = tool.version(),
                    source = source.label,
                    url = %source.url,
                    error = %error,
                    "managed ACP manifest source failed; trying fallback"
                );
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| ManagedAcpToolError::invalid("managed ACP manifest fetch failed")))
}

fn manifest_url(tool: ManagedAcpToolId) -> String {
    format!(
        "{}/{}/{}/manifest.json",
        MANAGED_ACP_TOOL_CDN_BASE,
        tool.slug(),
        tool.version()
    )
}

fn manifest_sources(tool: ManagedAcpToolId) -> Vec<RemoteSource> {
    vec![RemoteSource {
        label: "cdn",
        url: manifest_url(tool),
    }]
}

async fn fetch_manifest_from_source(
    client: &reqwest::Client,
    tool: ManagedAcpToolId,
    source: &RemoteSource,
) -> Result<ManagedAcpManifest, ManagedAcpToolError> {
    let response = client
        .get(source.url.clone())
        .send()
        .await
        .map_err(|error| reqwest_error("fetch managed ACP manifest", &source.url, &error))?;
    let manifest = error_for_status_with_context(response, "fetch managed ACP manifest", &source.url)
        .await?
        .json::<ManagedAcpManifest>()
        .await
        .map_err(|error| {
            ManagedAcpToolError::invalid(format!("parse managed ACP manifest failed for {}: {error}", source.url))
        })?;
    validate_remote_manifest(tool, &manifest)?;
    Ok(manifest)
}

fn validate_remote_manifest(tool: ManagedAcpToolId, manifest: &ManagedAcpManifest) -> Result<(), ManagedAcpToolError> {
    if manifest.tool != tool.slug() {
        return Err(ManagedAcpToolError::invalid(format!(
            "managed ACP manifest tool mismatch: expected {}, got {}",
            tool.slug(),
            manifest.tool
        )));
    }
    if manifest.version != tool.version() {
        return Err(ManagedAcpToolError::invalid(format!(
            "managed ACP manifest version mismatch: expected {}, got {}",
            tool.version(),
            manifest.version
        )));
    }
    Ok(())
}

fn archive_download_path(root: &Path, spec: PlatformSpec) -> PathBuf {
    root.join(format!("artifact.{}", spec.archive_ext))
}

fn install_lock_path(root: &Path) -> PathBuf {
    root.join(".install.lock")
}

fn build_http_client() -> Result<reqwest::Client, ManagedAcpToolError> {
    http_client::build_http_client(MANAGED_ACP_CONNECT_TIMEOUT, MANAGED_ACP_DOWNLOAD_TIMEOUT)
        .map_err(ManagedAcpToolError::invalid)
}

async fn stream_archive_to_file(
    mut response: reqwest::Response,
    archive_path: &Path,
    url: &str,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<(), ManagedAcpToolError> {
    let mut writer = fs::File::create(archive_path).map_err(ManagedAcpToolError::io)?;
    let total_bytes = response.content_length();
    let mut downloaded_bytes = 0_u64;
    let mut next_report_threshold = MANAGED_ACP_PROGRESS_STEP_BYTES;

    loop {
        let chunk = tokio::time::timeout(MANAGED_ACP_DOWNLOAD_IDLE_TIMEOUT, response.chunk())
            .await
            .map_err(|_| timeout_error("read ACP archive body", url, MANAGED_ACP_DOWNLOAD_IDLE_TIMEOUT, None))?
            .map_err(|error| reqwest_error("read ACP archive body", url, &error))?;
        let Some(chunk) = chunk else {
            break;
        };

        writer.write_all(&chunk).map_err(ManagedAcpToolError::io)?;
        downloaded_bytes += chunk.len() as u64;

        if downloaded_bytes == chunk.len() as u64 || downloaded_bytes >= next_report_threshold {
            emit_progress(
                reporter,
                ManagedAcpToolProgress::downloading(download_progress_message(url, downloaded_bytes, total_bytes)),
            );
            while downloaded_bytes >= next_report_threshold {
                next_report_threshold += MANAGED_ACP_PROGRESS_STEP_BYTES;
            }
        }
    }

    writer.flush().map_err(ManagedAcpToolError::io)?;
    Ok(())
}

async fn download_artifact_with_fallback(
    tool: ManagedAcpToolId,
    spec: PlatformSpec,
    artifact: &ManagedAcpArtifact,
    client: &reqwest::Client,
    archive_path: &Path,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<(), ManagedAcpToolError> {
    let sources = artifact_sources(tool, spec, artifact);
    let mut last_error = None;

    for (index, source) in sources.iter().enumerate() {
        emit_progress(
            reporter,
            ManagedAcpToolProgress::downloading(format!(
                "downloading managed {} artifact from {}",
                tool.display_name(),
                source.url
            )),
        );
        info!(
            tool = tool.slug(),
            version = tool.version(),
            platform = spec.manifest_key,
            source = source.label,
            url = %source.url,
            "managed ACP tool download source selected"
        );

        let _ = fs::remove_file(archive_path);
        match download_artifact_from_source(client, &source.url, archive_path, reporter).await {
            Ok(()) => {
                emit_progress(
                    reporter,
                    ManagedAcpToolProgress::validating(format!(
                        "verifying managed {} artifact checksum",
                        tool.display_name()
                    )),
                );
                match verify_archive_checksum(archive_path, &artifact.sha256) {
                    Ok(()) => return Ok(()),
                    Err(error) if index + 1 < sources.len() => {
                        warn!(
                            tool = tool.slug(),
                            version = tool.version(),
                            platform = spec.manifest_key,
                            source = source.label,
                            url = %source.url,
                            error = %error,
                            "managed ACP artifact source failed checksum validation; trying fallback"
                        );
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) if index + 1 < sources.len() => {
                warn!(
                    tool = tool.slug(),
                    version = tool.version(),
                    platform = spec.manifest_key,
                    source = source.label,
                    url = %source.url,
                    error = %error,
                    "managed ACP artifact source failed; trying fallback"
                );
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| ManagedAcpToolError::invalid("managed ACP artifact download failed")))
}

async fn download_artifact_from_source(
    client: &reqwest::Client,
    url: &str,
    archive_path: &Path,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> Result<(), ManagedAcpToolError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| reqwest_error("download ACP tool archive", url, &error))?;
    let response = error_for_status_with_context(response, "download ACP tool archive", url).await?;
    stream_archive_to_file(response, archive_path, url, reporter).await
}

fn artifact_sources(tool: ManagedAcpToolId, spec: PlatformSpec, artifact: &ManagedAcpArtifact) -> Vec<RemoteSource> {
    let _ = (tool, spec);
    vec![RemoteSource {
        label: "manifest",
        url: artifact.url.clone(),
    }]
}

fn verify_archive_checksum(path: &Path, expected_sha256: &str) -> Result<(), ManagedAcpToolError> {
    let bytes = fs::read(path).map_err(ManagedAcpToolError::io)?;
    let actual = hex::encode(Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(ManagedAcpToolError::invalid(format!(
            "managed ACP archive checksum mismatch for {}: expected {expected_sha256}, got {actual}",
            path.display()
        )));
    }
    Ok(())
}

fn extract_tar_zst(archive_path: &Path, root: &Path) -> Result<(), ManagedAcpToolError> {
    let archive_file = fs::File::open(archive_path).map_err(ManagedAcpToolError::io)?;
    let decoder = zstd::Decoder::new(archive_file)
        .map_err(|error| ManagedAcpToolError::invalid(format!("open zstd archive failed: {error}")))?;
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(root)
        .map_err(|error| ManagedAcpToolError::invalid(format!("extract tar.zst failed: {error}")))
}

fn extract_zip(archive_path: &Path, root: &Path) -> Result<(), ManagedAcpToolError> {
    let archive_file = fs::File::open(archive_path).map_err(ManagedAcpToolError::io)?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .map_err(|error| ManagedAcpToolError::invalid(format!("open zip failed: {error}")))?;

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| ManagedAcpToolError::invalid(format!("read zip entry failed: {error}")))?;
        let Some(relative_path) = file.enclosed_name().map(|path| path.to_path_buf()) else {
            continue;
        };
        let output_path = root.join(relative_path);
        if file.is_dir() {
            fs::create_dir_all(&output_path).map_err(ManagedAcpToolError::io)?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(ManagedAcpToolError::io)?;
        }
        let mut writer = fs::File::create(&output_path).map_err(ManagedAcpToolError::io)?;
        std::io::copy(&mut file, &mut writer).map_err(ManagedAcpToolError::io)?;
        writer.flush().map_err(ManagedAcpToolError::io)?;
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = writer.metadata().map_err(ManagedAcpToolError::io)?.permissions();
            perms.set_mode(mode);
            fs::set_permissions(&output_path, perms).map_err(ManagedAcpToolError::io)?;
        }
    }
    Ok(())
}

fn emit_progress(reporter: Option<&dyn ManagedAcpToolProgressReporter>, update: ManagedAcpToolProgress) {
    if let Some(reporter) = reporter {
        reporter.report(update);
    }
}

fn reqwest_error(stage: &str, url: &str, error: &reqwest::Error) -> ManagedAcpToolError {
    let detail = format_error_with_causes(error);
    if error.is_timeout() {
        return timeout_error(stage, url, MANAGED_ACP_DOWNLOAD_TIMEOUT, Some(&detail));
    }
    if let Some(status) = error.status() {
        return http_status_error(stage, url, status, None, None);
    }
    if error.is_connect() {
        return ManagedAcpToolError::invalid(format!("{stage} connect failed for {url}: {detail}"));
    }
    ManagedAcpToolError::invalid(format!("{stage} failed for {url}: {detail}"))
}

async fn error_for_status_with_context(
    response: reqwest::Response,
    stage: &str,
    url: &str,
) -> Result<reqwest::Response, ManagedAcpToolError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let headers = summarize_response_headers(response.headers());
    let body_excerpt = extract_response_body_excerpt(response).await;
    Err(http_status_error(
        stage,
        url,
        status,
        headers.as_deref(),
        body_excerpt.as_deref(),
    ))
}

fn timeout_error(stage: &str, url: &str, timeout: Duration, detail: Option<&str>) -> ManagedAcpToolError {
    let message = format!("{stage} timed out after {}s for {url}", timeout.as_secs());
    match detail {
        Some(detail) if !detail.is_empty() => ManagedAcpToolError::invalid(format!("{message}: {detail}")),
        _ => ManagedAcpToolError::invalid(message),
    }
}

fn http_status_error(
    stage: &str,
    url: &str,
    status: reqwest::StatusCode,
    headers: Option<&str>,
    body_excerpt: Option<&str>,
) -> ManagedAcpToolError {
    let mut message = format!("{stage} returned HTTP {} for {url}", status.as_u16());
    if let Some(headers) = headers
        && !headers.is_empty()
    {
        message.push_str(": headers=");
        message.push_str(headers);
    }
    if let Some(body_excerpt) = body_excerpt
        && !body_excerpt.is_empty()
    {
        message.push_str("; body=");
        message.push_str(body_excerpt);
    }
    ManagedAcpToolError::invalid(message)
}

fn format_error_with_causes(error: &(dyn StdError + 'static)) -> String {
    let mut segments = vec![error.to_string()];
    let mut current = error.source();
    while let Some(source) = current {
        let message = source.to_string();
        if !message.is_empty() && segments.last() != Some(&message) {
            segments.push(message);
        }
        current = source.source();
    }
    segments.join(" | caused by: ")
}

fn summarize_response_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    const KEYS: &[&str] = &["server", "cf-ray", "via", "x-cache", "content-type"];
    let mut parts = Vec::new();
    for key in KEYS {
        if let Some(value) = headers.get(*key).and_then(|value| value.to_str().ok()) {
            parts.push(format!("{key}={value}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

async fn extract_response_body_excerpt(response: reqwest::Response) -> Option<String> {
    let bytes = response.bytes().await.ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let excerpt = normalized.chars().take(240).collect::<String>();
    (!excerpt.is_empty()).then_some(excerpt)
}

fn download_progress_message(url: &str, downloaded_bytes: u64, total_bytes: Option<u64>) -> String {
    let downloaded_mb = downloaded_bytes / (1024 * 1024);
    match total_bytes {
        Some(total) if total > 0 => {
            let total_mb = total / (1024 * 1024);
            format!("downloading managed ACP tool from {url} ({downloaded_mb}MB / {total_mb}MB)")
        }
        _ => format!("downloading managed ACP tool from {url} ({downloaded_mb}MB)"),
    }
}

fn classify_error(error: &ManagedAcpToolError) -> (ManagedAcpToolFailureKind, Option<u16>) {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("timed out") {
        return (ManagedAcpToolFailureKind::Timeout, None);
    }
    if let Some(status) = parse_http_status(&message) {
        return (ManagedAcpToolFailureKind::HttpStatus, Some(status));
    }
    if message.contains("unsupported") {
        return (ManagedAcpToolFailureKind::UnsupportedPlatform, None);
    }
    if message.contains("checksum mismatch") {
        return (ManagedAcpToolFailureKind::ChecksumMismatch, None);
    }
    if message.contains("validate") || message.contains("entrypoint missing") {
        return (ManagedAcpToolFailureKind::ValidationFailed, None);
    }
    if message.contains("download") || message.contains("extract") || message.contains("connect failed") {
        return (ManagedAcpToolFailureKind::DownloadFailed, None);
    }
    (ManagedAcpToolFailureKind::Unknown, None)
}

fn parse_http_status(message: &str) -> Option<u16> {
    let marker = "http ";
    let start = message.find(marker)? + marker.len();
    let digits: String = message[start..].chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse::<u16>().ok()
}

fn install_error(
    error: ManagedAcpToolError,
    reporter: Option<&dyn ManagedAcpToolProgressReporter>,
) -> ManagedAcpToolError {
    let (kind, status_code) = classify_error(&error);
    emit_progress(
        reporter,
        match status_code {
            Some(status) => ManagedAcpToolProgress::failed_with_status(kind, status, error.to_string()),
            None => ManagedAcpToolProgress::failed(kind, error.to_string()),
        },
    );
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[test]
    fn manifest_url_uses_versioned_cdn_path() {
        assert_eq!(
            manifest_url(ManagedAcpToolId::CodexAcp),
            "https://static.aionui.com/managed/acp/codex-acp/0.14.0/manifest.json"
        );
    }

    #[test]
    fn managed_acp_tool_command_uses_node_runtime() {
        let runtime = crate::ResolvedNodeRuntime {
            source: crate::ResolvedNodeSource::Managed,
            root: PathBuf::from("/tmp/node"),
            version: semver::Version::new(24, 11, 0),
            node_path: PathBuf::from("/tmp/node/bin/node"),
            npm_path: PathBuf::from("/tmp/node/bin/npm"),
            npm_args_prefix: vec![],
            npx_path: PathBuf::from("/tmp/node/bin/npx"),
            npx_args_prefix: vec![],
            env: vec![(
                std::ffi::OsString::from("PATH"),
                std::ffi::OsString::from("/tmp/node/bin"),
            )],
        };
        let tool = ResolvedManagedAcpTool {
            id: ManagedAcpToolId::CodexAcp,
            version: "0.14.0".into(),
            root: PathBuf::from("/tmp/tool"),
            entrypoint: PathBuf::from("/tmp/tool/dist/index.js"),
            env_path_entries: vec![PathBuf::from("/tmp/tool/bin")],
        };
        let command = tool.command(&runtime);
        assert_eq!(command.program, PathBuf::from("/tmp/node/bin/node"));
        assert_eq!(
            command.args_prefix,
            vec![std::ffi::OsString::from("/tmp/tool/dist/index.js")]
        );
        let path = command
            .env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .unwrap();
        assert!(path.to_string_lossy().contains("/tmp/tool/bin"));
    }

    #[test]
    fn checksum_verification_detects_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool.tar.zst");
        std::fs::write(&path, b"not-tool").unwrap();
        let error = verify_archive_checksum(&path, "deadbeef").unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn checksum_mismatch_classifies_separately() {
        let error = ManagedAcpToolError::invalid("managed ACP archive checksum mismatch");
        let (kind, status_code) = classify_error(&error);
        assert_eq!(kind, ManagedAcpToolFailureKind::ChecksumMismatch);
        assert_eq!(status_code, None);
    }

    #[test]
    fn format_error_with_causes_collects_nested_sources() {
        #[derive(Debug)]
        struct TestError {
            message: &'static str,
            source: Option<Box<dyn StdError + Send + Sync>>,
        }

        impl fmt::Display for TestError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.message)
            }
        }

        impl StdError for TestError {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                self.source.as_deref().map(|error| error as &(dyn StdError + 'static))
            }
        }

        let error = TestError {
            message: "top level",
            source: Some(Box::new(TestError {
                message: "middle",
                source: Some(Box::new(TestError {
                    message: "root cause",
                    source: None,
                })),
            })),
        };

        assert_eq!(
            format_error_with_causes(&error),
            "top level | caused by: middle | caused by: root cause"
        );
    }

    #[test]
    fn remote_manifest_accepts_extra_artifact_fields() {
        let manifest = serde_json::from_str::<ManagedAcpManifest>(
            r#"{
              "tool": "codex-acp",
              "version": "0.14.0",
              "artifacts": {
                "darwin-arm64": {
                  "url": "https://static.aionui.com/managed/acp/codex-acp/0.14.0/codex-acp-0.14.0-darwin-arm64.tar.zst",
                  "sha256": "abc123",
                  "size": 1024
                }
              }
            }"#,
        )
        .unwrap();

        let artifact = manifest.artifacts.get("darwin-arm64").unwrap();
        assert_eq!(
            artifact.url,
            "https://static.aionui.com/managed/acp/codex-acp/0.14.0/codex-acp-0.14.0-darwin-arm64.tar.zst"
        );
        assert_eq!(artifact.sha256, "abc123");
    }

    #[test]
    fn doctor_snapshot_includes_builtin_acp_tools() {
        let rows = doctor_snapshot();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tool, "codex-acp");
        assert_eq!(rows[1].tool, "claude-agent-acp");
    }

    #[test]
    fn resolve_package_bin_entry_prefers_short_name() {
        let bin = serde_json::json!({
            "codex-acp": "dist/index.js",
            "other": "ignored.js"
        });
        let entrypoint = resolve_package_bin_entry("@zed-industries/codex-acp", &bin).unwrap();
        assert_eq!(entrypoint, "dist/index.js");
    }

    #[test]
    fn resolve_package_bin_entry_accepts_string_form() {
        let entrypoint = resolve_package_bin_entry(
            "@agentclientprotocol/claude-agent-acp",
            &serde_json::json!("bin/cli.js"),
        )
        .unwrap();
        assert_eq!(entrypoint, "bin/cli.js");
    }
}
