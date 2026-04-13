use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;

use aionui_api_types::{
    ApiResponse, CloneConversationRequest, ConversationListResponse, ConversationResponse,
    CreateConversationRequest, ListConversationsQuery, ListMessagesQuery, MessageListResponse,
    MessageSearchResponse, SearchMessagesQuery, SendMessageRequest, UpdateConversationRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::AppError;

use crate::state::ConversationRouterState;

/// Build the conversation router (CRUD + message flow + extended operations).
///
/// All routes require authentication (applied by the caller).
///
/// Endpoints:
/// - `POST   /api/conversations`              — create a conversation (201)
/// - `GET    /api/conversations`              — list conversations (cursor pagination)
/// - `POST   /api/conversations/clone`        — clone a conversation (201)
/// - `GET    /api/conversations/:id`          — get a single conversation
/// - `PATCH  /api/conversations/:id`          — partial update (extra merge)
/// - `DELETE /api/conversations/:id`          — delete a conversation
/// - `POST   /api/conversations/:id/reset`    — reset a conversation
/// - `GET    /api/conversations/:id/associated` — list associated conversations
/// - `GET    /api/conversations/:id/messages`  — list messages (page pagination)
/// - `POST   /api/conversations/:id/messages`  — send a user message (202)
/// - `POST   /api/conversations/:id/stop`      — stop streaming response
/// - `POST   /api/conversations/:id/warmup`    — pre-initialize agent
/// - `GET    /api/messages/search`            — search messages across conversations
pub fn conversation_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations", post(create).get(list))
        // Static path must come before `{id}` wildcard
        .route("/api/conversations/clone", post(clone))
        .route(
            "/api/conversations/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/api/conversations/{id}/reset", post(reset))
        .route("/api/conversations/{id}/associated", get(associated))
        .route(
            "/api/conversations/{id}/messages",
            get(list_messages).post(send_message),
        )
        .route("/api/conversations/{id}/stop", post(stop_stream))
        .route("/api/conversations/{id}/warmup", post(warmup))
        .route("/api/messages/search", get(search_messages))
        .with_state(state)
}

// ── Handlers ───────────────────────────────────────────────────────

async fn create(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state.conversation_service.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn list(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListConversationsQuery>,
) -> Result<Json<ApiResponse<ConversationListResponse>>, AppError> {
    let result = state.conversation_service.list(&user.id, query).await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn clone(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CloneConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state
        .conversation_service
        .clone_create(&user.id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn get_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let conversation = state.conversation_service.get(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn update(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateConversationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state
        .conversation_service
        .update(&user.id, &id, req)
        .await?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn delete_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.conversation_service.delete(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn reset(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.conversation_service.reset(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn associated(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, AppError> {
    let items = state
        .conversation_service
        .list_associated(&user.id, &id)
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn list_messages(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<ListMessagesQuery>,
) -> Result<Json<ApiResponse<MessageListResponse>>, AppError> {
    let result = state
        .conversation_service
        .list_messages(&user.id, &id, query)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn send_message(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<()>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .conversation_service
        .send_message(&user.id, &id, req, &state.worker_task_manager)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::success())))
}

async fn stop_stream(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .conversation_service
        .stop_stream(&user.id, &id, &state.worker_task_manager)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn warmup(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .conversation_service
        .warmup(&user.id, &id, &state.worker_task_manager)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn search_messages(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<SearchMessagesQuery>,
) -> Result<Json<ApiResponse<MessageSearchResponse>>, AppError> {
    let result = state
        .conversation_service
        .search_messages(&user.id, query)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}
