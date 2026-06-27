//! Regression test for the channel `ConversationService` wiring.
//!
//! Guards the fix for "Either `type` or `assistant.id` is required when
//! creating a conversation": the channel `ConversationService` built in
//! `build_channel_state` must carry the assistant repositories, otherwise an IM
//! platform (e.g. Telegram) bound to an assistant cannot resolve a snapshot and
//! conversation creation fails.
//!
//! Unlike the behavioral twin in `aionui-channel`, this test drives the real
//! production builder `build_channel_conversation_service`, so it fails if the
//! assistant repos are ever dropped from that wiring again.

mod common;

use std::sync::Arc;

use aionui_app::build_channel_conversation_service;
use aionui_channel::channel_settings::ChannelSettingsService;
use aionui_channel::message_service::ChannelMessageService;
use aionui_channel::types::PluginType;
use aionui_common::AgentType;
use aionui_db::models::{AssistantSessionRow, UpsertAssistantDefinitionParams};
use aionui_db::{
    IAssistantDefinitionRepository, IClientPreferenceRepository, IConversationRepository,
    SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository, SqliteClientPreferenceRepository,
    SqliteConversationRepository,
};

use common::build_app_with_mock_agents;

fn bare_assistant_definition_params<'a>(
    definition_id: &'a str,
    assistant_id: &'a str,
    agent_id: &'a str,
) -> UpsertAssistantDefinitionParams<'a> {
    UpsertAssistantDefinitionParams {
        id: definition_id,
        assistant_id,
        source: "generated",
        owner_type: "system",
        source_ref: Some(assistant_id),
        source_version: None,
        source_hash: None,
        name: assistant_id,
        name_i18n: "{}",
        description: Some("Channel bare assistant"),
        description_i18n: "{}",
        avatar_type: "emoji",
        avatar_value: Some("🤖"),
        agent_id,
        rule_resource_type: "inline",
        rule_resource_ref: None,
        rule_inline_content: Some(""),
        recommended_prompts: "[]",
        recommended_prompts_i18n: "{}",
        default_model_mode: "auto",
        default_model_value: None,
        default_permission_mode: "auto",
        default_permission_value: None,
        default_skills_mode: "auto",
        default_skill_ids: "[]",
        custom_skill_names: "[]",
        default_disabled_builtin_skill_ids: "[]",
        default_mcps_mode: "auto",
        default_mcp_ids: "[]",
    }
}

/// A Telegram message to a bot bound to an assistant must create a conversation
/// with a persisted assistant snapshot. This exercises the channel
/// `ConversationService` exactly as it is wired in `build_channel_state`.
#[tokio::test]
async fn channel_conversation_service_resolves_assistant_snapshot_for_bound_telegram_bot() {
    let (_app, services) = build_app_with_mock_agents().await;
    let pool = services.database.pool().clone();

    // Build the conversation service through the production channel builder.
    // The assistant repos must be wired by this function (the regressed line).
    let conversation_svc = build_channel_conversation_service(&services);

    // Seed a bare assistant definition and bind the Telegram platform to it.
    let definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let overlay_repo = Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()));
    let pref_repo = Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    definition_repo
        .upsert(&bare_assistant_definition_params(
            "asstdef-channel-claude",
            "bare-claude",
            "claude",
        ))
        .await
        .unwrap();
    pref_repo
        .upsert_batch(&[(
            "assistant.telegram.agent",
            r#"{"assistant_id":"bare-claude","name":"Claude"}"#,
        )])
        .await
        .unwrap();

    let settings = Arc::new(ChannelSettingsService::new(pref_repo).with_assistant_repos(definition_repo, overlay_repo));
    let message_svc = ChannelMessageService::new(
        conversation_svc,
        services.worker_task_manager.clone(),
        settings,
        "system_default_user".to_owned(),
    );

    let session = AssistantSessionRow {
        id: "session-assisted".to_owned(),
        user_id: "channel-user-1".to_owned(),
        agent_type: "aionrs".to_owned(),
        conversation_id: None,
        workspace: None,
        chat_id: Some("123456789".to_owned()),
        created_at: 1,
        last_activity: 1,
    };

    let result = message_svc
        .send_to_agent(&session, "hello", PluginType::Telegram)
        .await
        .expect("channel-bound-assistant conversation must be created (no \"Either `type` or `assistant.id`\" error)");

    let conversation_repo = SqliteConversationRepository::new(pool);
    let snapshot = conversation_repo
        .get_assistant_snapshot(&result.conversation_id)
        .await
        .unwrap();
    assert!(
        snapshot.is_some(),
        "channel-created conversation should persist an assistant snapshot when the platform is bound to an assistant"
    );
    assert_eq!(snapshot.unwrap().assistant_id, "bare-claude");

    let conversation = conversation_repo.get(&result.conversation_id).await.unwrap().unwrap();
    assert_eq!(conversation.r#type, AgentType::Acp.serde_name());
}
