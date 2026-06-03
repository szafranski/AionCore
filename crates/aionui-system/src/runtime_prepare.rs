use std::sync::Arc;

use aionui_api_types::{
    EnsureNodeRuntimeResponse, RuntimeFailureKind, RuntimeResourceKind, RuntimeStatusPayload, RuntimeStatusPhase,
    RuntimeStatusScope, WebSocketMessage,
};
use aionui_common::AppError;
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{
    NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase, SharedNodeRuntimeProgressReporter,
    ensure_node_runtime_with_reporter,
};

#[derive(Clone)]
pub struct RuntimePrepareService {
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl RuntimePrepareService {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { broadcaster }
    }

    pub async fn ensure_node_runtime(&self, scope: RuntimeStatusScope) -> Result<EnsureNodeRuntimeResponse, AppError> {
        let reporter = self.runtime_reporter(scope);
        ensure_node_runtime_with_reporter(Some(reporter.as_ref()))
            .await
            .map_err(|error| AppError::BadRequest(error.to_string()))?;
        Ok(EnsureNodeRuntimeResponse { ready: true })
    }

    fn runtime_reporter(&self, scope: RuntimeStatusScope) -> SharedNodeRuntimeProgressReporter {
        let broadcaster = self.broadcaster.clone();
        Arc::new(move |update: NodeRuntimeProgress| {
            let payload = RuntimeStatusPayload {
                resource: RuntimeResourceKind::Node,
                scope: scope.clone(),
                phase: map_phase(update.phase),
                failure_kind: update.failure_kind.map(map_failure_kind),
                message: update.message,
                status_code: update.status_code,
            };
            let payload = serde_json::to_value(payload).expect("runtime status payload should serialize");
            broadcaster.broadcast(WebSocketMessage::new("runtime.statusChanged", payload));
        })
    }
}

fn map_phase(phase: NodeRuntimeProgressPhase) -> RuntimeStatusPhase {
    match phase {
        NodeRuntimeProgressPhase::WaitingForLock => RuntimeStatusPhase::WaitingForLock,
        NodeRuntimeProgressPhase::Downloading => RuntimeStatusPhase::Downloading,
        NodeRuntimeProgressPhase::Extracting => RuntimeStatusPhase::Extracting,
        NodeRuntimeProgressPhase::Validating => RuntimeStatusPhase::Validating,
        NodeRuntimeProgressPhase::Ready => RuntimeStatusPhase::Ready,
        NodeRuntimeProgressPhase::Failed => RuntimeStatusPhase::Failed,
    }
}

fn map_failure_kind(kind: NodeRuntimeFailureKind) -> RuntimeFailureKind {
    match kind {
        NodeRuntimeFailureKind::Timeout => RuntimeFailureKind::Timeout,
        NodeRuntimeFailureKind::DownloadFailed => RuntimeFailureKind::DownloadFailed,
        NodeRuntimeFailureKind::HttpStatus => RuntimeFailureKind::HttpStatus,
        NodeRuntimeFailureKind::ValidationFailed => RuntimeFailureKind::ValidationFailed,
        NodeRuntimeFailureKind::UnsupportedPlatform => RuntimeFailureKind::UnsupportedPlatform,
        NodeRuntimeFailureKind::Unknown => RuntimeFailureKind::Unknown,
    }
}
