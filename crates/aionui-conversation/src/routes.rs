use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;

use aionui_api_types::{
    ApiResponse, ConversationListResponse, ConversationResponse, CreateConversationRequest,
    ListConversationsQuery, UpdateConversationRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::AppError;

use crate::state::ConversationRouterState;

/// Build the conversation CRUD router.
///
/// All routes require authentication (applied by the caller).
///
/// Endpoints:
/// - `POST   /api/conversations`     — create a conversation (201)
/// - `GET    /api/conversations`      — list conversations (cursor pagination)
/// - `GET    /api/conversations/:id`  — get a single conversation
/// - `PATCH  /api/conversations/:id`  — partial update (extra merge)
/// - `DELETE /api/conversations/:id`  — delete a conversation
pub fn conversation_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations", post(create).get(list))
        .route(
            "/api/conversations/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
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

async fn get_one(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let conversation = state.conversation_service.get(&id).await?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn update(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateConversationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let conversation = state.conversation_service.update(&id, req).await?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn delete_one(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.conversation_service.delete(&id).await?;
    Ok(Json(ApiResponse::success()))
}
