use std::ffi::OsString;
use std::path::PathBuf;

use semver::Version;

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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub version: Version,
    pub node_path: PathBuf,
    pub npm_path: PathBuf,
    pub npm_args_prefix: Vec<OsString>,
    pub npx_path: PathBuf,
    pub npx_args_prefix: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl ResolvedNodeRuntime {
    pub fn npm_command(&self) -> ResolvedCommand {
        ResolvedCommand {
            program: self.npm_path.clone(),
            args_prefix: self.npm_args_prefix.clone(),
            env: self.env.clone(),
        }
    }

    pub fn npx_command(&self) -> ResolvedCommand {
        ResolvedCommand {
            program: self.npx_path.clone(),
            args_prefix: self.npx_args_prefix.clone(),
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

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct NodeRuntimeError {
    message: String,
}

impl NodeRuntimeError {
    pub fn system_invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn managed_invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn unsupported_platform(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn io_system(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}
