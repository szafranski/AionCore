use std::sync::Arc;

use aionui_api_types::{
    ConversationListResponse, ConversationResponse, CreateConversationRequest,
    ListConversationsQuery, UpdateConversationRequest, WebSocketMessage,
};
use aionui_common::{
    generate_id, now_ms, AppError, ConversationSource, ConversationStatus, PaginatedResult,
};
use aionui_db::{ConversationFilters, ConversationRowUpdate, IConversationRepository};
use aionui_realtime::EventBroadcaster;
use crate::convert::{row_to_response, string_to_enum};

/// Business logic for conversation CRUD operations.
///
/// Handles ID generation, defaults, extra-merge semantics,
/// and broadcasts `conversation.listChanged` events via WebSocket.
#[derive(Clone)]
pub struct ConversationService {
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl ConversationService {
    pub fn new(
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
    ) -> Self {
        Self { repo, broadcaster }
    }

    /// Create a new conversation.
    ///
    /// Generates a UUID v7, sets status to `pending`, defaults source
    /// to `aionui`, and broadcasts `conversation.listChanged(created)`.
    pub async fn create(
        &self,
        user_id: &str,
        req: CreateConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        let id = generate_id();
        let now = now_ms();
        let source = req.source.unwrap_or(ConversationSource::Aionui);

        let row = aionui_db::models::ConversationRow {
            id: id.clone(),
            user_id: user_id.to_owned(),
            name: req.name.unwrap_or_default(),
            r#type: enum_to_db(&req.r#type)?,
            extra: serde_json::to_string(&req.extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
            model: Some(
                serde_json::to_string(&req.model).map_err(|e| {
                    AppError::Internal(format!("Failed to serialize model: {e}"))
                })?,
            ),
            status: enum_to_db(&ConversationStatus::Pending)?,
            source: Some(enum_to_db(&source)?),
            channel_chat_id: req.channel_chat_id,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
        };

        self.repo.create(&row).await?;

        let response = row_to_response(row)?;

        self.broadcast_list_changed(&response.id, "created", response.source.as_ref());

        Ok(response)
    }

    /// Get a single conversation by ID.
    pub async fn get(&self, id: &str) -> Result<ConversationResponse, AppError> {
        let row = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;
        row_to_response(row)
    }

    /// List conversations with cursor-based pagination and optional filters.
    pub async fn list(
        &self,
        user_id: &str,
        query: ListConversationsQuery,
    ) -> Result<ConversationListResponse, AppError> {
        let filters = ConversationFilters {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(0),
            source: query.source,
            cron_job_id: query.cron_job_id,
            pinned: query.pinned,
        };

        let result = self.repo.list_paginated(user_id, &filters).await?;

        let items = result
            .items
            .into_iter()
            .map(row_to_response)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }

    /// Update a conversation (partial update with extra-merge semantics).
    ///
    /// If `extra` is provided, it is merged into the existing extra JSON
    /// (top-level keys are overwritten, unlisted keys are preserved).
    /// Broadcasts `conversation.listChanged(updated)`.
    pub async fn update(
        &self,
        id: &str,
        req: UpdateConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        let existing = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        let now = now_ms();

        // Merge extra if provided
        let merged_extra = if let Some(new_extra) = &req.extra {
            let mut existing_extra: serde_json::Value =
                serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
            merge_json(&mut existing_extra, new_extra);
            Some(
                serde_json::to_string(&existing_extra).map_err(|e| {
                    AppError::Internal(format!("Failed to serialize merged extra: {e}"))
                })?,
            )
        } else {
            None
        };

        // Handle pinned_at: set timestamp on pin, clear on unpin
        let pinned_at = req.pinned.map(|p| if p { Some(now) } else { None });

        let model_json = req
            .model
            .as_ref()
            .map(|m| {
                serde_json::to_string(m)
                    .map(Some)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize model: {e}")))
            })
            .transpose()?;

        let updates = ConversationRowUpdate {
            name: req.name,
            pinned: req.pinned,
            pinned_at,
            model: model_json,
            extra: merged_extra,
            status: None,
            updated_at: Some(now),
        };

        self.repo.update(id, &updates).await?;

        // Re-fetch to return the updated version
        let updated = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| AppError::Internal("Conversation vanished after update".into()))?;

        let response = row_to_response(updated)?;

        self.broadcast_list_changed(id, "updated", response.source.as_ref());

        Ok(response)
    }

    /// Delete a conversation (messages cascade via FK).
    ///
    /// Broadcasts `conversation.listChanged(deleted)`.
    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        // Get existing to retrieve source for broadcast
        let existing = self
            .repo
            .get(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        let source: Option<ConversationSource> = existing
            .source
            .as_deref()
            .and_then(|s| string_to_enum::<ConversationSource>(s).ok());

        self.repo.delete(id).await?;

        self.broadcast_list_changed(id, "deleted", source.as_ref());

        Ok(())
    }

    /// Broadcast a `conversation.listChanged` WebSocket event.
    fn broadcast_list_changed(
        &self,
        conversation_id: &str,
        action: &str,
        source: Option<&ConversationSource>,
    ) {
        let payload = serde_json::json!({
            "conversationId": conversation_id,
            "action": action,
            "source": source,
        });
        let event = WebSocketMessage::new("conversation.listChanged", payload);
        self.broadcaster.broadcast(event);
    }
}

// ── Helpers ────────────────────────────────────────────────────────

/// Serialize a serde-compatible enum to its JSON string form for DB storage.
///
/// e.g. `AgentType::Gemini` → `"gemini"`
fn enum_to_db<T: serde::Serialize>(val: &T) -> Result<String, AppError> {
    let json_val = serde_json::to_value(val)
        .map_err(|e| AppError::Internal(format!("Enum serialization failed: {e}")))?;
    json_val
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| AppError::Internal("Expected string enum value".into()))
}

/// Merge `patch` into `base` (top-level key overwrite).
fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    if let (Some(base_obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_obj {
            base_obj.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_to_db_agent_type() {
        use aionui_common::AgentType;
        assert_eq!(enum_to_db(&AgentType::Gemini).unwrap(), "gemini");
        assert_eq!(enum_to_db(&AgentType::Acp).unwrap(), "acp");
        assert_eq!(
            enum_to_db(&AgentType::OpenclawGateway).unwrap(),
            "openclawGateway"
        );
    }

    #[test]
    fn enum_to_db_status() {
        assert_eq!(
            enum_to_db(&ConversationStatus::Pending).unwrap(),
            "pending"
        );
        assert_eq!(
            enum_to_db(&ConversationStatus::Running).unwrap(),
            "running"
        );
        assert_eq!(
            enum_to_db(&ConversationStatus::Finished).unwrap(),
            "finished"
        );
    }

    #[test]
    fn enum_to_db_source() {
        assert_eq!(
            enum_to_db(&ConversationSource::Aionui).unwrap(),
            "aionui"
        );
        assert_eq!(
            enum_to_db(&ConversationSource::Telegram).unwrap(),
            "telegram"
        );
    }

    #[test]
    fn merge_json_top_level_overwrite() {
        let mut base = json!({"a": 1, "b": 2});
        let patch = json!({"b": 3, "c": 4});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"a": 1, "b": 3, "c": 4}));
    }

    #[test]
    fn merge_json_into_empty() {
        let mut base = json!({});
        let patch = json!({"x": "hello"});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"x": "hello"}));
    }

    #[test]
    fn merge_json_non_object_noop() {
        let mut base = json!("string");
        let patch = json!({"a": 1});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!("string"));
    }

    #[test]
    fn merge_json_empty_patch() {
        let mut base = json!({"a": 1});
        let patch = json!({});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"a": 1}));
    }
}
