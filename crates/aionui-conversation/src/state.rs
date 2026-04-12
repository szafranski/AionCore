use crate::service::ConversationService;

/// Shared state for conversation route handlers.
#[derive(Clone)]
pub struct ConversationRouterState {
    pub conversation_service: ConversationService,
}
