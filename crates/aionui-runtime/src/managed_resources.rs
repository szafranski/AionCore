use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResourceSourceKind {
    Bundled,
    DevLocal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedResourceSource {
    pub kind: ManagedResourceSourceKind,
    pub root: PathBuf,
}

const BUNDLED_RESOURCES_ENV: &str = "AIONUI_BUNDLED_MANAGED_RESOURCES";
const DEV_LOCAL_RESOURCES_ENV: &str = "AIONUI_DEV_MANAGED_RESOURCES";

pub fn node_sources(directory_name: &str) -> Vec<ManagedResourceSource> {
    resource_roots()
        .into_iter()
        .map(|source| ManagedResourceSource {
            root: source.root.join("node").join(directory_name),
            ..source
        })
        .filter(|source| source.root.is_dir())
        .collect()
}

pub fn acp_tool_sources(tool_slug: &str, version: &str, platform_key: &str) -> Vec<ManagedResourceSource> {
    resource_roots()
        .into_iter()
        .map(|source| ManagedResourceSource {
            root: source.root.join("acp").join(tool_slug).join(version).join(platform_key),
            ..source
        })
        .filter(|source| source.root.is_dir())
        .collect()
}

pub fn export_node_runtime_to_dev_local(source_root: &Path, directory_name: &str) -> std::io::Result<PathBuf> {
    export_node_runtime_to_root(&ensure_dev_local_root()?, source_root, directory_name)
}

pub fn export_acp_tool_to_dev_local(
    source_root: &Path,
    tool_slug: &str,
    version: &str,
    platform_key: &str,
) -> std::io::Result<PathBuf> {
    export_acp_tool_to_root(&ensure_dev_local_root()?, source_root, tool_slug, version, platform_key)
}

pub fn export_node_runtime_to_root(root: &Path, source_root: &Path, directory_name: &str) -> std::io::Result<PathBuf> {
    let target = root.join("node").join(directory_name);
    materialize_directory(source_root, &target)?;
    Ok(target)
}

pub fn export_acp_tool_to_root(
    root: &Path,
    source_root: &Path,
    tool_slug: &str,
    version: &str,
    platform_key: &str,
) -> std::io::Result<PathBuf> {
    let target = root.join("acp").join(tool_slug).join(version).join(platform_key);
    materialize_directory(source_root, &target)?;
    Ok(target)
}

pub fn materialize_directory(source_root: &Path, target_root: &Path) -> std::io::Result<()> {
    if !source_root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("managed resource source missing: {}", source_root.display()),
        ));
    }

    if source_root == target_root {
        return Ok(());
    }
    if let (Ok(source), Ok(target)) = (fs::canonicalize(source_root), fs::canonicalize(target_root))
        && source == target
    {
        return Ok(());
    }

    if target_root.exists() {
        fs::remove_dir_all(target_root)?;
    }
    fs::create_dir_all(target_root)?;

    for entry in WalkDir::new(source_root) {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(source_root)
            .expect("walkdir path should stay under source root");

        if relative.as_os_str().is_empty() {
            continue;
        }

        let target_path = target_root.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target_path)?;
            copy_permissions(entry.path(), &target_path)?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &target_path)?;
        copy_permissions(entry.path(), &target_path)?;
    }

    Ok(())
}

pub fn ensure_dev_local_root() -> std::io::Result<PathBuf> {
    let root = configured_root(DEV_LOCAL_RESOURCES_ENV).unwrap_or_else(default_dev_local_root);
    fs::create_dir_all(&root)?;
    Ok(root)
}

pub fn should_auto_prepare_dev_local() -> bool {
    if configured_root(DEV_LOCAL_RESOURCES_ENV).is_some() {
        return true;
    }

    let workspace_root = workspace_root();
    if !workspace_root.join(".git").exists() {
        return false;
    }

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Ok(exe) = fs::canonicalize(exe) else {
        return false;
    };

    exe.starts_with(workspace_root.join("target"))
}

fn resource_roots() -> Vec<ManagedResourceSource> {
    let mut roots = Vec::new();

    if let Some(root) = bundled_root()
        && root.is_dir()
    {
        roots.push(ManagedResourceSource {
            kind: ManagedResourceSourceKind::Bundled,
            root,
        });
    }

    if let Some(root) = dev_local_root()
        && root.is_dir()
    {
        roots.push(ManagedResourceSource {
            kind: ManagedResourceSourceKind::DevLocal,
            root,
        });
    }

    roots
}

fn bundled_root() -> Option<PathBuf> {
    configured_root(BUNDLED_RESOURCES_ENV).or_else(default_bundled_root)
}

fn dev_local_root() -> Option<PathBuf> {
    configured_root(DEV_LOCAL_RESOURCES_ENV).or_else(|| {
        let root = default_dev_local_root();
        root.is_dir().then_some(root)
    })
}

fn configured_root(env_key: &str) -> Option<PathBuf> {
    std::env::var_os(env_key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn default_bundled_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = fs::canonicalize(exe).ok()?.parent()?.to_path_buf();
    Some(exe_dir.join("managed-resources"))
}

fn default_dev_local_root() -> PathBuf {
    workspace_root().join(".tmp").join("managed-resources")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("aionui-runtime lives under crates/<name>")
        .to_path_buf()
}

fn copy_permissions(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = fs::metadata(source)?;
    fs::set_permissions(target, metadata.permissions())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn node_sources_prefer_existing_dev_local_root() {
        let _guard = env_lock().lock().expect("lock env");
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("managed");
        fs::create_dir_all(root.join("node").join("node-v24.11.0-darwin-arm64")).expect("create node dir");

        unsafe {
            std::env::set_var(DEV_LOCAL_RESOURCES_ENV, &root);
        }

        let sources = node_sources("node-v24.11.0-darwin-arm64");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, ManagedResourceSourceKind::DevLocal);
        assert_eq!(sources[0].root, root.join("node").join("node-v24.11.0-darwin-arm64"));

        unsafe {
            std::env::remove_var(DEV_LOCAL_RESOURCES_ENV);
        }
    }

    #[test]
    fn export_node_runtime_copies_files_into_dev_local_root() {
        let _guard = env_lock().lock().expect("lock env");
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-node");
        fs::create_dir_all(source.join("bin")).expect("create source");
        fs::write(source.join("bin").join("node"), b"node").expect("write node");

        let dev_root = temp.path().join("dev-root");
        unsafe {
            std::env::set_var(DEV_LOCAL_RESOURCES_ENV, &dev_root);
        }

        let exported = export_node_runtime_to_dev_local(&source, "node-v24.11.0-darwin-arm64").expect("export");
        assert!(exported.join("bin").join("node").is_file());

        unsafe {
            std::env::remove_var(DEV_LOCAL_RESOURCES_ENV);
        }
    }

    #[test]
    fn auto_prepare_is_enabled_when_dev_root_is_explicitly_configured() {
        let _guard = env_lock().lock().expect("lock env");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var(DEV_LOCAL_RESOURCES_ENV, temp.path());
        }

        assert!(should_auto_prepare_dev_local());

        unsafe {
            std::env::remove_var(DEV_LOCAL_RESOURCES_ENV);
        }
    }
}
