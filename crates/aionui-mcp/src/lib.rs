pub mod adapter;
pub mod error;
pub mod types;

pub use adapter::{DetectedServer, McpAgentAdapter};
pub use error::McpError;
pub use types::{McpServer, McpServerTransport, McpTool};
