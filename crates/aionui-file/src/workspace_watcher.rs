//! Workspace-level file watcher: shared OS watcher via notify-debouncer-full,
//! gitignore filtering, event fan-out to workspace subscribers + office watch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use aionui_api_types::WebSocketMessage;
use aionui_common::AppError;
use aionui_realtime::WebSocketManager;

use crate::workspace_watcher_registry::SubscriptionRegistry;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Kind of file-system change detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchChangeKind {
    Create,
    Modify,
    Delete,
}

/// A single file-system change within a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchChange {
    pub path: String,
    pub kind: WatchChangeKind,
}

/// Batch event pushed to subscribed connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchBatchEvent {
    pub workspace: String,
    pub changes: Vec<WatchChange>,
}

/// Overflow event when too many changes occur in a single batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchOverflowEvent {
    pub workspace: String,
}

// ---------------------------------------------------------------------------
// Debounced event from notify-debouncer-full
// ---------------------------------------------------------------------------

/// Processed batch of debounced events for a workspace.
#[derive(Debug)]
pub struct DebouncedBatch {
    pub workspace: String,
    pub events: Vec<DebouncedEvent>,
}

// ---------------------------------------------------------------------------
// GitignoreFilter
// ---------------------------------------------------------------------------

/// Caches per-workspace gitignore matchers.
pub struct GitignoreFilter {
    cache: DashMap<String, Gitignore>,
}

impl Default for GitignoreFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl GitignoreFilter {
    pub fn new() -> Self {
        Self { cache: DashMap::new() }
    }

    /// Returns true if the path should be ignored (filtered out).
    pub fn is_ignored(&self, workspace: &str, relative_path: &str, is_dir: bool) -> bool {
        if self.cache.get(workspace).is_none() {
            self.rebuild(workspace);
        }
        if let Some(matcher) = self.cache.get(workspace) {
            // Check the path itself
            if matcher.matched(relative_path, is_dir).is_ignore() {
                return true;
            }
            // Check ancestors (e.g. ".git/config" → check ".git" as dir)
            let p = Path::new(relative_path);
            for ancestor in p.ancestors().skip(1) {
                if ancestor == Path::new("") {
                    break;
                }
                if matcher.matched(ancestor.to_string_lossy().as_ref(), true).is_ignore() {
                    return true;
                }
            }
            false
        } else {
            false
        }
    }

    /// Rebuild the gitignore matcher for a workspace.
    pub fn rebuild(&self, workspace: &str) {
        let gitignore_path = Path::new(workspace).join(".gitignore");
        let mut builder = GitignoreBuilder::new(workspace);
        if gitignore_path.exists() {
            let _ = builder.add(&gitignore_path);
        }
        // Always ignore .git directory and its contents
        let _ = builder.add_line(None, ".git");
        match builder.build() {
            Ok(matcher) => {
                self.cache.insert(workspace.to_owned(), matcher);
            }
            Err(e) => {
                warn!(workspace, error = %e, "failed to build gitignore matcher");
            }
        }
    }

    /// Invalidate cache for a workspace (e.g. when .gitignore changes).
    pub fn invalidate(&self, workspace: &str) {
        self.cache.remove(workspace);
    }
}

// ---------------------------------------------------------------------------
// SharedWorkspaceWatcher (using notify-debouncer-full)
// ---------------------------------------------------------------------------

/// A shared OS-level watcher for a single workspace directory.
///
/// Uses `notify-debouncer-full` to handle:
/// - Atomic save detection (write-to-tmp + rename → single Modify)
/// - Event deduplication and coalescing
/// - Cross-platform rename pairing via file-id tracking
pub struct SharedWorkspaceWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    pub workspace: String,
}

impl SharedWorkspaceWatcher {
    /// Create a new recursive debounced watcher for the given workspace.
    /// Debounced events are forwarded to the provided sender.
    pub fn new(workspace: &str, event_tx: mpsc::UnboundedSender<DebouncedBatch>) -> Result<Self, AppError> {
        let ws = workspace.to_owned();
        let canonical = std::fs::canonicalize(workspace)
            .map_err(|e| AppError::NotFound(format!("cannot resolve workspace {workspace}: {e}")))?;

        let ws_clone = ws.clone();
        let mut debouncer = new_debouncer(
            std::time::Duration::from_millis(500),
            None,
            move |result: DebounceEventResult| {
                let events = match result {
                    Ok(events) => events,
                    Err(errors) => {
                        for e in errors {
                            warn!(error = %e, "debouncer error");
                        }
                        return;
                    }
                };
                if events.is_empty() {
                    return;
                }
                let _ = event_tx.send(DebouncedBatch {
                    workspace: ws_clone.clone(),
                    events,
                });
            },
        )
        .map_err(|e| AppError::Internal(format!("failed to create workspace debouncer: {e}")))?;

        debouncer
            .watch(&canonical, notify::RecursiveMode::Recursive)
            .map_err(|e| AppError::Internal(format!("failed to watch workspace {workspace}: {e}")))?;

        Ok(Self {
            _debouncer: debouncer,
            workspace: ws,
        })
    }
}

// ---------------------------------------------------------------------------
// Office file extension check (for fan-out)
// ---------------------------------------------------------------------------

const OFFICE_EXTENSIONS: &[&str] = &["pptx", "docx", "xlsx"];

fn is_office_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
        let lower = ext.to_ascii_lowercase();
        OFFICE_EXTENSIONS.contains(&lower.as_str())
    })
}

// ---------------------------------------------------------------------------
// EventDispatcher (replaces EventAggregator)
// ---------------------------------------------------------------------------

/// Overflow threshold: max changes per directory per batch.
const OVERFLOW_THRESHOLD: usize = 500;

/// Receives debounced events and dispatches to workspace subscribers + office fan-out.
pub struct EventDispatcher {
    registry: Arc<SubscriptionRegistry>,
    ws_manager: Arc<WebSocketManager>,
    gitignore: Arc<GitignoreFilter>,
    office_broadcaster: Option<Arc<dyn aionui_realtime::EventBroadcaster>>,
}

impl EventDispatcher {
    pub fn new(
        registry: Arc<SubscriptionRegistry>,
        ws_manager: Arc<WebSocketManager>,
        gitignore: Arc<GitignoreFilter>,
    ) -> Self {
        Self {
            registry,
            ws_manager,
            gitignore,
            office_broadcaster: None,
        }
    }

    pub fn with_office_broadcaster(mut self, broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>) -> Self {
        self.office_broadcaster = Some(broadcaster);
        self
    }

    /// Run the dispatch loop, consuming debounced batches from the channel.
    pub async fn run(self, mut event_rx: mpsc::UnboundedReceiver<DebouncedBatch>) {
        while let Some(batch) = event_rx.recv().await {
            self.dispatch_batch(batch);
        }
    }

    fn dispatch_batch(&self, batch: DebouncedBatch) {
        let workspace_path = PathBuf::from(&batch.workspace);
        let mut changes: Vec<WatchChange> = Vec::new();

        for event in &batch.events {
            let kind = match map_debounced_kind(&event.kind) {
                Some(k) => k,
                None => continue,
            };

            for path in &event.paths {
                let relative = match path.strip_prefix(&workspace_path) {
                    Ok(r) => r.to_string_lossy().into_owned(),
                    Err(_) => continue,
                };

                if relative.is_empty() {
                    continue;
                }

                // Filter editor temp files (atomic save intermediates)
                if is_temp_file(&relative) {
                    continue;
                }

                // Check if .gitignore itself changed
                if relative == ".gitignore" {
                    self.gitignore.invalidate(&batch.workspace);
                    self.gitignore.rebuild(&batch.workspace);
                }

                let is_dir = path.is_dir();
                if self.gitignore.is_ignored(&batch.workspace, &relative, is_dir) {
                    continue;
                }

                // Office fan-out: Create events for office files
                if kind == WatchChangeKind::Create && is_office_file(path) {
                    self.emit_office_event(path, &batch.workspace);
                }

                changes.push(WatchChange { path: relative, kind });
            }
        }

        if changes.is_empty() {
            return;
        }

        // Group changes by parent directory and dispatch to subscribers
        let mut per_dir: HashMap<String, Vec<WatchChange>> = HashMap::new();
        for change in changes {
            let parent = parent_dir(&change.path);
            per_dir.entry(parent).or_default().push(change);
        }

        for (dir, dir_changes) in per_dir {
            let subscribers = self.registry.get_subscribers_for_dir(&batch.workspace, &dir);
            if subscribers.is_empty() {
                continue;
            }

            if dir_changes.len() > OVERFLOW_THRESHOLD {
                let event = WatchOverflowEvent {
                    workspace: batch.workspace.clone(),
                };
                let msg = WebSocketMessage::new("workspace.overflow", serde_json::to_value(&event).unwrap_or_default());
                for conn_id in &subscribers {
                    self.ws_manager.send_to(*conn_id, msg.clone());
                }
            } else {
                let event = WatchBatchEvent {
                    workspace: batch.workspace.clone(),
                    changes: dir_changes,
                };
                let msg = WebSocketMessage::new("workspace.changed", serde_json::to_value(&event).unwrap_or_default());
                for conn_id in &subscribers {
                    self.ws_manager.send_to(*conn_id, msg.clone());
                }
            }
        }
    }

    fn emit_office_event(&self, path: &Path, workspace: &str) {
        if let Some(ref broadcaster) = self.office_broadcaster {
            let payload = crate::types::OfficeFileAddedEvent {
                file_path: path.to_string_lossy().into_owned(),
                workspace: workspace.to_owned(),
            };
            let json = serde_json::to_value(&payload).unwrap_or_default();
            broadcaster.broadcast(WebSocketMessage::new("workspaceOfficeWatch.fileAdded", json));
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map debounced EventKind to our WatchChangeKind.
fn map_debounced_kind(kind: &notify::EventKind) -> Option<WatchChangeKind> {
    match kind {
        notify::EventKind::Create(_) => Some(WatchChangeKind::Create),
        notify::EventKind::Modify(_) => Some(WatchChangeKind::Modify),
        notify::EventKind::Remove(_) => Some(WatchChangeKind::Delete),
        notify::EventKind::Access(_) => None,
        notify::EventKind::Any | notify::EventKind::Other => Some(WatchChangeKind::Modify),
    }
}

/// Returns true if the path looks like an editor temporary file.
fn is_temp_file(relative_path: &str) -> bool {
    let name = Path::new(relative_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(relative_path);

    // Pattern: "file.tmp.XXXXX" (e.g. README.md.tmp.74488.abea7e8eacb7)
    if name.contains(".tmp.") {
        return true;
    }
    // Vim swap files
    if name.ends_with(".swp") || name.ends_with(".swo") {
        return true;
    }
    // Emacs backup/lock files
    if (name.starts_with('#') && name.ends_with('#')) || name.ends_with('~') {
        return true;
    }
    false
}

/// Get the parent directory of a relative path (as a string).
/// Returns "" for top-level files.
pub(crate) fn parent_dir(relative_path: &str) -> String {
    match Path::new(relative_path).parent() {
        Some(p) if p == Path::new("") => String::new(),
        Some(p) => p.to_string_lossy().into_owned(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// WorkspaceWatchManager
// ---------------------------------------------------------------------------

/// Top-level manager that owns shared watchers and coordinates lifecycle.
///
/// Implements `WatcherLifecycle` so the router can trigger start/stop.
pub struct WorkspaceWatchManager {
    shared_watchers: DashMap<String, Arc<SharedWorkspaceWatcher>>,
    event_tx: mpsc::UnboundedSender<DebouncedBatch>,
}

impl WorkspaceWatchManager {
    pub fn new(event_tx: mpsc::UnboundedSender<DebouncedBatch>) -> Self {
        Self {
            shared_watchers: DashMap::new(),
            event_tx,
        }
    }
}

impl crate::workspace_watcher_router::WatcherLifecycle for WorkspaceWatchManager {
    fn start_workspace_watch(&self, workspace: &str) {
        if self.shared_watchers.contains_key(workspace) {
            return;
        }
        match SharedWorkspaceWatcher::new(workspace, self.event_tx.clone()) {
            Ok(watcher) => {
                debug!(workspace, "workspace watcher started (debouncer-full)");
                self.shared_watchers.insert(workspace.to_owned(), Arc::new(watcher));
            }
            Err(e) => {
                warn!(workspace, error = %e, "failed to start workspace watcher");
            }
        }
    }

    fn stop_workspace_watch(&self, workspace: &str) {
        if self.shared_watchers.remove(workspace).is_some() {
            debug!(workspace, "workspace watcher stopped");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_debounced_kind_create() {
        assert_eq!(
            map_debounced_kind(&notify::EventKind::Create(notify::event::CreateKind::File)),
            Some(WatchChangeKind::Create)
        );
    }

    #[test]
    fn map_debounced_kind_modify() {
        assert_eq!(
            map_debounced_kind(&notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content
            ))),
            Some(WatchChangeKind::Modify)
        );
    }

    #[test]
    fn map_debounced_kind_rename_is_modify() {
        assert_eq!(
            map_debounced_kind(&notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both
            ))),
            Some(WatchChangeKind::Modify)
        );
    }

    #[test]
    fn map_debounced_kind_remove() {
        assert_eq!(
            map_debounced_kind(&notify::EventKind::Remove(notify::event::RemoveKind::File)),
            Some(WatchChangeKind::Delete)
        );
    }

    #[test]
    fn map_debounced_kind_access_is_none() {
        assert_eq!(
            map_debounced_kind(&notify::EventKind::Access(notify::event::AccessKind::Read)),
            None
        );
    }

    #[test]
    fn parent_dir_top_level() {
        assert_eq!(parent_dir("main.rs"), "");
    }

    #[test]
    fn parent_dir_nested() {
        assert_eq!(parent_dir("src/main.rs"), "src");
    }

    #[test]
    fn parent_dir_deeply_nested() {
        assert_eq!(parent_dir("src/components/Button.tsx"), "src/components");
    }

    #[test]
    fn watch_change_serialization() {
        let change = WatchChange {
            path: "src/new_file.rs".into(),
            kind: WatchChangeKind::Create,
        };
        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(json["path"], "src/new_file.rs");
        assert_eq!(json["kind"], "create");
    }

    #[test]
    fn watch_batch_event_serialization() {
        let event = WatchBatchEvent {
            workspace: "/project".into(),
            changes: vec![
                WatchChange {
                    path: "src/a.rs".into(),
                    kind: WatchChangeKind::Create,
                },
                WatchChange {
                    path: "src/b.rs".into(),
                    kind: WatchChangeKind::Delete,
                },
            ],
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["workspace"], "/project");
        assert_eq!(json["changes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn watch_overflow_event_serialization() {
        let event = WatchOverflowEvent {
            workspace: "/project".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["workspace"], "/project");
    }

    #[test]
    fn gitignore_filter_always_ignores_git_dir() {
        let tmp = std::env::temp_dir().join("test_gitignore_ws");
        let _ = std::fs::create_dir_all(&tmp);
        let ws = tmp.to_string_lossy().into_owned();

        let filter = GitignoreFilter::new();
        filter.rebuild(&ws);
        assert!(filter.is_ignored(&ws, ".git/config", false));
        assert!(filter.is_ignored(&ws, ".git", true));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gitignore_filter_non_ignored_passes() {
        let tmp = std::env::temp_dir().join("test_gitignore_ws2");
        let _ = std::fs::create_dir_all(&tmp);
        let ws = tmp.to_string_lossy().into_owned();

        let filter = GitignoreFilter::new();
        filter.rebuild(&ws);
        assert!(!filter.is_ignored(&ws, "src/main.rs", false));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_rename_kind_exists() {
        let json = serde_json::to_string(&WatchChangeKind::Create).unwrap();
        assert_eq!(json, "\"create\"");
        let json = serde_json::to_string(&WatchChangeKind::Modify).unwrap();
        assert_eq!(json, "\"modify\"");
        let json = serde_json::to_string(&WatchChangeKind::Delete).unwrap();
        assert_eq!(json, "\"delete\"");
    }
}
