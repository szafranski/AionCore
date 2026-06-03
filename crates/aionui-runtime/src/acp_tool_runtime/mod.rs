mod types;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::cache;
use crate::node_runtime::DoctorRow;

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

pub async fn ensure_managed_acp_tool(
    tool: ManagedAcpToolId,
) -> Result<ResolvedManagedAcpTool, ManagedAcpToolError> {
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

    install_tool_with_retry(tool, spec, &root, reporter).await?;
    validate_tool_root(tool, &root, reporter)
}

pub fn probe_managed_acp_tool_supported(tool: ManagedAcpToolId) -> ManagedAcpToolSupport {
    match platform_spec() {
        Ok(spec) => match tool_root(tool, spec) {
            Ok(root) => ManagedAcpToolSupport {
                supported: true,
                detail: format!("managed {} artifact supported under {}", tool.display_name(), root.display()),
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

    emit_progress(
        reporter,
        ManagedAcpToolProgress::downloading(format!(
            "downloading managed {} artifact from {}",
            tool.display_name(),
            artifact.url
        )),
    );
    info!(
        tool = tool.slug(),
        version = tool.version(),
        platform = spec.manifest_key,
        url = %artifact.url,
        "managed ACP tool download source selected"
    );

    let response = client
        .get(artifact.url.clone())
        .send()
        .await
        .map_err(|error| reqwest_error("download ACP tool archive", &artifact.url, &error))?;
    let response = response
        .error_for_status()
        .map_err(|error| reqwest_error("download ACP tool archive", &artifact.url, &error))?;
    stream_archive_to_file(response, &archive_path, &artifact.url, reporter).await?;

    emit_progress(
        reporter,
        ManagedAcpToolProgress::validating(format!("verifying managed {} artifact checksum", tool.display_name())),
    );
    verify_archive_checksum(&archive_path, &artifact.sha256)?;

    emit_progress(
        reporter,
        ManagedAcpToolProgress::extracting(format!("extracting managed {} artifact", tool.display_name())),
    );
    match spec.archive_ext {
        "tar.zst" => extract_tar_zst(&archive_path, root)?,
        "zip" => extract_zip(&archive_path, root)?,
        ext => return Err(ManagedAcpToolError::invalid(format!("unsupported ACP archive extension: {ext}"))),
    }
    let _ = fs::remove_file(&archive_path);

    validate_tool_root(tool, root, reporter)?;
    Ok(())
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
        }),
        ("macos", "x86_64") => Ok(PlatformSpec {
            manifest_key: "darwin-x64",
            archive_ext: "tar.zst",
        }),
        ("linux", "aarch64") => Ok(PlatformSpec {
            manifest_key: "linux-arm64",
            archive_ext: "tar.zst",
        }),
        ("linux", "x86_64") => Ok(PlatformSpec {
            manifest_key: "linux-x64",
            archive_ext: "tar.zst",
        }),
        ("windows", "x86_64") => Ok(PlatformSpec {
            manifest_key: "win32-x64",
            archive_ext: "zip",
        }),
        ("windows", "aarch64") => Ok(PlatformSpec {
            manifest_key: "win32-arm64",
            archive_ext: "zip",
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
    serde_json::from_str(&contents)
        .map_err(|error| ManagedAcpToolError::invalid(format!("parse local ACP manifest failed for {}: {error}", path.display())))
}

async fn fetch_manifest(client: &reqwest::Client, tool: ManagedAcpToolId) -> Result<ManagedAcpManifest, ManagedAcpToolError> {
    let url = manifest_url(tool);
    let manifest = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| reqwest_error("fetch managed ACP manifest", &url, &error))?
        .error_for_status()
        .map_err(|error| reqwest_error("fetch managed ACP manifest", &url, &error))?
        .json::<ManagedAcpManifest>()
        .await
        .map_err(|error| ManagedAcpToolError::invalid(format!("parse managed ACP manifest failed for {url}: {error}")))?;
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
    Ok(manifest)
}

fn manifest_url(tool: ManagedAcpToolId) -> String {
    format!("{}/{}/{}/manifest.json", MANAGED_ACP_TOOL_CDN_BASE, tool.slug(), tool.version())
}

fn archive_download_path(root: &Path, spec: PlatformSpec) -> PathBuf {
    root.join(format!("artifact.{}", spec.archive_ext))
}

fn install_lock_path(root: &Path) -> PathBuf {
    root.join(".install.lock")
}

fn build_http_client() -> Result<reqwest::Client, ManagedAcpToolError> {
    reqwest::Client::builder()
        .connect_timeout(MANAGED_ACP_CONNECT_TIMEOUT)
        .timeout(MANAGED_ACP_DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| ManagedAcpToolError::invalid(format!("build http client: {error}")))
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
            .map_err(|_| timeout_error("read ACP archive body", url, MANAGED_ACP_DOWNLOAD_IDLE_TIMEOUT))?
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
    if error.is_timeout() {
        return timeout_error(stage, url, MANAGED_ACP_DOWNLOAD_TIMEOUT);
    }
    if let Some(status) = error.status() {
        return http_status_error(stage, url, status);
    }
    if error.is_connect() {
        return ManagedAcpToolError::invalid(format!("{stage} connect failed for {url}: {error}"));
    }
    ManagedAcpToolError::invalid(format!("{stage} failed for {url}: {error}"))
}

fn timeout_error(stage: &str, url: &str, timeout: Duration) -> ManagedAcpToolError {
    ManagedAcpToolError::invalid(format!("{stage} timed out after {}s for {url}", timeout.as_secs()))
}

fn http_status_error(stage: &str, url: &str, status: reqwest::StatusCode) -> ManagedAcpToolError {
    ManagedAcpToolError::invalid(format!("{stage} returned HTTP {} for {url}", status.as_u16()))
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
            env: vec![(std::ffi::OsString::from("PATH"), std::ffi::OsString::from("/tmp/node/bin"))],
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
        assert_eq!(command.args_prefix, vec![std::ffi::OsString::from("/tmp/tool/dist/index.js")]);
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
}
