use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tracing::{info, warn};

use crate::cache;

use super::types::{NodeRuntimeError, NodeRuntimeSupport, ResolvedNodeRuntime, ResolvedNodeSource};

const MANAGED_NODE_VERSION: &str = "24.11.0";

#[derive(Debug, Clone, Copy)]
struct PlatformSpec {
    folder_suffix: &'static str,
    archive_ext: &'static str,
}

impl PlatformSpec {
    fn directory_name(self) -> String {
        format!("node-v{MANAGED_NODE_VERSION}-{}", self.folder_suffix)
    }

    fn download_url(self) -> String {
        format!(
            "https://nodejs.org/dist/v{version}/{name}.{ext}",
            version = MANAGED_NODE_VERSION,
            name = self.directory_name(),
            ext = self.archive_ext
        )
    }
}

pub fn probe_support() -> NodeRuntimeSupport {
    match platform_spec() {
        Ok(spec) => NodeRuntimeSupport {
            supported: true,
            detail: format!("managed node runtime supported ({})", spec.folder_suffix),
        },
        Err(error) => NodeRuntimeSupport {
            supported: false,
            detail: error.to_string(),
        },
    }
}

pub async fn install_and_validate() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    let spec = platform_spec()?;
    let runtime_root = cache::node_runtime_root()
        .ok_or_else(|| NodeRuntimeError::managed_invalid("managed node runtime root unavailable"))?;
    fs::create_dir_all(&runtime_root).map_err(NodeRuntimeError::io_system)?;

    let version_dir = runtime_root.join(spec.directory_name());
    match validate_managed_runtime(&version_dir).await {
        Ok(runtime) => return Ok(runtime),
        Err(error) => {
            warn!(
                error = %error,
                root = %version_dir.display(),
                "managed node runtime validation failed before install"
            );
        }
    }

    info!(
        version = MANAGED_NODE_VERSION,
        root = %runtime_root.display(),
        url = %spec.download_url(),
        "managed node runtime install started"
    );
    install_archive(&runtime_root, spec).await?;
    match validate_managed_runtime(&version_dir).await {
        Ok(runtime) => {
            info!(
                version = %runtime.version,
                root = %runtime.root.display(),
                "managed node runtime install completed"
            );
            Ok(runtime)
        }
        Err(first_error) => {
            warn!(
                error = %first_error,
                root = %version_dir.display(),
                "managed node runtime validation failed after install; retrying"
            );
            let _ = fs::remove_dir_all(&version_dir);
            install_archive(&runtime_root, spec).await?;
            validate_managed_runtime(&version_dir)
                .await
                .inspect(|runtime| {
                    info!(
                        version = %runtime.version,
                        root = %runtime.root.display(),
                        "managed node runtime install completed"
                    );
                })
                .map_err(|retry_error| {
                    NodeRuntimeError::managed_invalid(format!("{first_error}; retry failed: {retry_error}"))
                })
        }
    }
}

pub(crate) async fn validate_managed_runtime(root: &Path) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    let runtime = runtime_from_managed_root(root)?;
    super::validate_runtime(runtime, None).await
}

fn platform_spec() -> Result<PlatformSpec, NodeRuntimeError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(PlatformSpec {
            folder_suffix: "darwin-arm64",
            archive_ext: "tar.gz",
        }),
        ("macos", "x86_64") => Ok(PlatformSpec {
            folder_suffix: "darwin-x64",
            archive_ext: "tar.gz",
        }),
        ("linux", "aarch64") => Ok(PlatformSpec {
            folder_suffix: "linux-arm64",
            archive_ext: "tar.gz",
        }),
        ("linux", "x86_64") => Ok(PlatformSpec {
            folder_suffix: "linux-x64",
            archive_ext: "tar.gz",
        }),
        ("windows", "x86_64") => Ok(PlatformSpec {
            folder_suffix: "win-x64",
            archive_ext: "zip",
        }),
        ("windows", "aarch64") => Ok(PlatformSpec {
            folder_suffix: "win-arm64",
            archive_ext: "zip",
        }),
        (os, arch) => Err(NodeRuntimeError::unsupported_platform(format!(
            "managed node runtime unsupported on {os}/{arch}"
        ))),
    }
}

fn runtime_from_managed_root(root: &Path) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    if !root.is_dir() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node runtime directory missing: {}",
            root.display()
        )));
    }

    prepare_runtime_files(root)?;

    let node_path = if cfg!(windows) {
        root.join("node.exe")
    } else {
        root.join("bin").join("node")
    };
    if !node_path.is_file() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node executable missing: {}",
            node_path.display()
        )));
    }

    let npm_wrapper = if cfg!(windows) {
        root.join("npm.cmd")
    } else {
        root.join("bin").join("npm")
    };
    let npx_wrapper = if cfg!(windows) {
        root.join("npx.cmd")
    } else {
        root.join("bin").join("npx")
    };
    let npm_cli = managed_npm_cli_path(root);
    let npx_cli = managed_npx_cli_path(root);

    let (npm_path, npm_args_prefix) = if npm_wrapper.is_file() {
        (npm_wrapper, vec![])
    } else if npm_cli.is_file() {
        (node_path.clone(), vec![npm_cli.into_os_string()])
    } else {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed npm entrypoint missing under {}",
            root.display()
        )));
    };

    let (npx_path, npx_args_prefix) = if npx_wrapper.is_file() {
        (npx_wrapper, vec![])
    } else if npx_cli.is_file() {
        (node_path.clone(), vec![npx_cli.into_os_string()])
    } else {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed npx entrypoint missing under {}",
            root.display()
        )));
    };

    Ok(ResolvedNodeRuntime {
        source: ResolvedNodeSource::Managed,
        root: root.to_path_buf(),
        version: semver::Version::new(0, 0, 0),
        node_path,
        npm_path,
        npm_args_prefix,
        npx_path,
        npx_args_prefix,
        env: managed_env(root)?,
    })
}

async fn install_archive(runtime_root: &Path, spec: PlatformSpec) -> Result<(), NodeRuntimeError> {
    let url = spec.download_url();
    let version_dir = runtime_root.join(spec.directory_name());
    if version_dir.exists() {
        let _ = fs::remove_dir_all(&version_dir);
    }

    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("build http client: {error}")))?
        .get(url.clone())
        .send()
        .await
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("download {url} failed: {error}")))?;
    let response = response
        .error_for_status()
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("download {url} failed: {error}")))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("read archive body failed: {error}")))?;

    match spec.archive_ext {
        "tar.gz" => extract_tar_gz(bytes.as_ref(), runtime_root)?,
        "zip" => extract_zip(bytes.as_ref(), runtime_root)?,
        ext => {
            return Err(NodeRuntimeError::managed_invalid(format!(
                "unsupported archive extension: {ext}"
            )));
        }
    }

    Ok(())
}

fn extract_tar_gz(bytes: &[u8], runtime_root: &Path) -> Result<(), NodeRuntimeError> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(runtime_root)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("extract tar.gz failed: {error}")))
}

fn extract_zip(bytes: &[u8], runtime_root: &Path) -> Result<(), NodeRuntimeError> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("open zip failed: {error}")))?;

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| NodeRuntimeError::managed_invalid(format!("read zip entry failed: {error}")))?;
        let Some(relative_path) = file.enclosed_name().map(|path| path.to_path_buf()) else {
            continue;
        };
        let output_path = runtime_root.join(relative_path);
        if file.is_dir() {
            fs::create_dir_all(&output_path).map_err(NodeRuntimeError::io_system)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(NodeRuntimeError::io_system)?;
        }

        let mut writer = fs::File::create(&output_path).map_err(NodeRuntimeError::io_system)?;
        std::io::copy(&mut file, &mut writer).map_err(NodeRuntimeError::io_system)?;
        writer.flush().map_err(NodeRuntimeError::io_system)?;

        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = writer.metadata().map_err(NodeRuntimeError::io_system)?.permissions();
            perms.set_mode(mode);
            fs::set_permissions(&output_path, perms).map_err(NodeRuntimeError::io_system)?;
        }
    }

    Ok(())
}

fn prepare_runtime_files(root: &Path) -> Result<(), NodeRuntimeError> {
    fs::create_dir_all(root.join("cache")).map_err(NodeRuntimeError::io_system)?;
    fs::create_dir_all(default_npm_prefix(root)).map_err(NodeRuntimeError::io_system)?;
    if !cfg!(windows) {
        fs::create_dir_all(default_npm_prefix(root).join("bin")).map_err(NodeRuntimeError::io_system)?;
    }
    fs::write(root.join("blank_user_npmrc"), []).map_err(NodeRuntimeError::io_system)?;
    fs::write(root.join("blank_global_npmrc"), []).map_err(NodeRuntimeError::io_system)?;
    Ok(())
}

fn managed_env(root: &Path) -> Result<Vec<(OsString, OsString)>, NodeRuntimeError> {
    let node_bin = managed_bin_dir(root);
    let global_bin = managed_prefix_bin_dir(root);
    let mut paths = vec![node_bin, global_bin];
    if let Some(current_path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&current_path));
    }
    let path = std::env::join_paths(paths)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("failed to build PATH: {error}")))?;

    Ok(vec![
        ("PATH".into(), path),
        ("npm_config_cache".into(), root.join("cache").into_os_string()),
        (
            "npm_config_userconfig".into(),
            root.join("blank_user_npmrc").into_os_string(),
        ),
        (
            "npm_config_globalconfig".into(),
            root.join("blank_global_npmrc").into_os_string(),
        ),
        ("npm_config_prefix".into(), default_npm_prefix(root).into_os_string()),
    ])
}

fn managed_bin_dir(root: &Path) -> PathBuf {
    if cfg!(windows) {
        root.to_path_buf()
    } else {
        root.join("bin")
    }
}

fn managed_npm_cli_path(root: &Path) -> PathBuf {
    root.join("lib")
        .join("node_modules")
        .join("npm")
        .join("bin")
        .join("npm-cli.js")
}

fn managed_npx_cli_path(root: &Path) -> PathBuf {
    root.join("lib")
        .join("node_modules")
        .join("npm")
        .join("bin")
        .join("npx-cli.js")
}

fn default_npm_prefix(root: &Path) -> PathBuf {
    root.join("tools").join("global")
}

fn managed_prefix_bin_dir(root: &Path) -> PathBuf {
    if cfg!(windows) {
        default_npm_prefix(root)
    } else {
        default_npm_prefix(root).join("bin")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn managed_runtime_validation_uses_real_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("node-v24.11.0-test");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        let node = bin.join("node");
        std::fs::write(&node, "#!/bin/sh\necho v24.11.0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&node).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&node, perms).unwrap();
        }

        let err = validate_managed_runtime(&root).await.unwrap_err();
        assert!(err.to_string().to_ascii_lowercase().contains("npm"));
    }

    #[test]
    fn managed_runtime_support_reports_current_platform() {
        let support = probe_support();
        let expected = cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(windows);
        assert_eq!(support.supported, expected);
    }

    #[test]
    fn managed_runtime_injects_npm_state_under_runtime_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("node-v24.11.0-test");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("node"), b"").unwrap();
        std::fs::write(bin.join("npm"), b"").unwrap();
        std::fs::write(bin.join("npx"), b"").unwrap();

        let runtime = runtime_from_managed_root(&root).expect("runtime");
        let env: std::collections::HashMap<_, _> = runtime
            .npm_command()
            .env
            .into_iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();

        assert_eq!(
            env.get("npm_config_cache"),
            Some(&root.join("cache").display().to_string())
        );
        assert_eq!(
            env.get("npm_config_userconfig"),
            Some(&root.join("blank_user_npmrc").display().to_string())
        );
        assert_eq!(
            env.get("npm_config_globalconfig"),
            Some(&root.join("blank_global_npmrc").display().to_string())
        );
        assert_eq!(
            env.get("npm_config_prefix"),
            Some(&root.join("tools").join("global").display().to_string())
        );
    }
}
