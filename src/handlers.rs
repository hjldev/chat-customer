use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cache::constant_time_eq;
use crate::db::DbError;
use crate::models::{
    AdminLoginRequest, AdminLoginResponse, ClientWsMessage, CreateSessionRequest,
    SendMessageRequest, UpdateSessionStatusRequest, UpdateVisitorProfileRequest,
    VisitorSessionDetailResponse, VisitorSessionInitResponse, WsEvent,
};
use crate::AppState;

const INDEX_HTML: &str = include_str!("../static/index.html");
const ADMIN_HTML: &str = include_str!("../static/admin.html");
const STYLES_CSS: &str = include_str!("../static/styles.css");

const MAX_MESSAGE_CHARS: usize = 4000;
const MAX_NAME_CHARS: usize = 48;
const MAX_CONTACT_CHARS: usize = 120;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }

    fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, msg)
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => ApiError::not_found("会话不存在"),
            other => {
                tracing::error!("Database error: {}", other);
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "服务器内部错误")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub async fn serve_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn serve_admin() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

pub async fn serve_styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLES_CSS,
    )
}

pub async fn health_check() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

// ============================================================
// Visitor APIs
// ============================================================

pub async fn create_visitor_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateSessionRequest>,
) -> Result<Json<VisitorSessionInitResponse>, ApiError> {
    let raw_name = payload.visitor_name.unwrap_or_default().trim().to_string();
    let visitor_name = if raw_name.is_empty() {
        format!("访客 {}", &uuid::Uuid::new_v4().simple().to_string()[..4].to_uppercase())
    } else {
        raw_name.chars().take(MAX_NAME_CHARS).collect()
    };

    let visitor_contact: String = payload
        .visitor_contact
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_CONTACT_CHARS)
        .collect();

    let visitor_meta = extract_visitor_meta(&headers);

    let (mut session, visitor_token) = state
        .store
        .create_session(visitor_name, visitor_contact, visitor_meta)
        .await?;

    session.online = false;

    state
        .hub
        .send_to_admins(&WsEvent::SessionUpdated {
            session: session.clone(),
        })
        .await;

    Ok(Json(VisitorSessionInitResponse {
        session,
        visitor_token,
        messages: Vec::new(),
        owner_online: state.hub.is_owner_online().await,
        owner_name: state.config.owner_name.clone(),
        welcome_message: state.config.welcome_message.clone(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: Option<String>,
}

pub async fn get_visitor_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Result<Json<VisitorSessionDetailResponse>, ApiError> {
    let token = extract_visitor_token(&headers, query.token.as_deref())
        .ok_or_else(|| ApiError::unauthorized("缺少访客凭证"))?;

    if !state.store.verify_visitor_token(&session_id, &token).await? {
        return Err(ApiError::unauthorized("访客凭证无效或会话已失效"));
    }

    let mut session = state
        .store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    let messages = state.store.get_messages(&session_id).await?;

    Ok(Json(VisitorSessionDetailResponse {
        session,
        messages,
        owner_online: state.hub.is_owner_online().await,
        owner_name: state.config.owner_name.clone(),
        welcome_message: state.config.welcome_message.clone(),
    }))
}

pub async fn update_visitor_profile(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<UpdateVisitorProfileRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = extract_visitor_token(&headers, None)
        .ok_or_else(|| ApiError::unauthorized("缺少访客凭证"))?;

    if !state.store.verify_visitor_token(&session_id, &token).await? {
        return Err(ApiError::unauthorized("访客凭证无效"));
    }

    let name: String = payload
        .visitor_name
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    if name.is_empty() {
        return Err(ApiError::bad_request("昵称不能为空"));
    }

    let contact: String = payload
        .visitor_contact
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_CONTACT_CHARS)
        .collect();

    let mut session = state
        .store
        .update_visitor_profile(&session_id, name, contact)
        .await?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    state
        .hub
        .send_to_admins(&WsEvent::SessionUpdated {
            session: session.clone(),
        })
        .await;

    Ok(Json(json!({ "session": session })))
}

pub async fn send_visitor_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = extract_visitor_token(&headers, None)
        .ok_or_else(|| ApiError::unauthorized("缺少访客凭证"))?;

    if !state.store.verify_visitor_token(&session_id, &token).await? {
        return Err(ApiError::unauthorized("访客凭证无效"));
    }

    let content = sanitize_message(&payload.content)?;
    let (message, mut session) = state
        .store
        .add_message(&session_id, "visitor", content)
        .await?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    state
        .hub
        .broadcast_to_session_and_admins(
            &session_id,
            &WsEvent::NewMessage {
                message: message.clone(),
                session: session.clone(),
            },
        )
        .await;

    if !state.hub.is_owner_online().await {
        state.notifier.notify_offline_message(&session, &message).await;
    }

    Ok(Json(json!({ "message": message, "session": session })))
}

// ============================================================
// Admin APIs
// ============================================================

pub async fn admin_login(
    State(state): State<AppState>,
    Json(payload): Json<AdminLoginRequest>,
) -> Result<Json<AdminLoginResponse>, ApiError> {
    if !constant_time_eq(
        payload.password.as_bytes(),
        state.config.admin_password.as_bytes(),
    ) {
        return Err(ApiError::unauthorized("管理密码错误"));
    }

    let token = state.store.issue_admin_token().await;
    Ok(Json(AdminLoginResponse {
        token,
        owner_name: state.config.owner_name.clone(),
    }))
}

pub async fn admin_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(token) = extract_admin_token(&headers, None) {
        state.store.revoke_admin_token(&token).await;
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn admin_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;
    Ok(Json(json!({
        "ok": true,
        "owner_name": state.config.owner_name,
    })))
}

#[derive(Debug, Deserialize)]
pub struct SessionListQuery {
    pub status: Option<String>,
}

pub async fn admin_list_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionListQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;

    let status_filter = match query.status.as_deref() {
        Some("open") => Some("open"),
        Some("archived") => Some("archived"),
        _ => None,
    };

    let mut sessions = state.store.list_sessions(status_filter).await?;
    let online_ids = state.hub.online_visitor_ids().await;
    for s in &mut sessions {
        s.online = online_ids.contains(&s.id);
    }

    Ok(Json(json!({ "sessions": sessions })))
}

pub async fn admin_get_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;

    let mut session = state
        .store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    let messages = state.store.get_messages(&session_id).await?;
    Ok(Json(json!({
        "session": session,
        "messages": messages
    })))
}

pub async fn admin_send_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;

    let content = sanitize_message(&payload.content)?;
    let (message, mut session) = state
        .store
        .add_message(&session_id, "owner", content)
        .await?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    state
        .notifier
        .reset_session_cooldown(&session_id)
        .await;

    state
        .hub
        .broadcast_to_session_and_admins(
            &session_id,
            &WsEvent::NewMessage {
                message: message.clone(),
                session: session.clone(),
            },
        )
        .await;

    Ok(Json(json!({ "message": message, "session": session })))
}

pub async fn admin_mark_read(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;

    let mut session = state
        .store
        .mark_session_read(&session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    state
        .notifier
        .reset_session_cooldown(&session_id)
        .await;

    state
        .hub
        .send_to_admins(&WsEvent::SessionUpdated {
            session: session.clone(),
        })
        .await;

    Ok(Json(json!({ "session": session })))
}

pub async fn admin_update_status(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<UpdateSessionStatusRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;

    if payload.status != "open" && payload.status != "archived" {
        return Err(ApiError::bad_request("无效的会话状态"));
    }

    let mut session = state
        .store
        .update_session_status(&session_id, payload.status)
        .await?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    session.online = state.hub.is_visitor_online(&session_id).await;

    state
        .hub
        .send_to_admins(&WsEvent::SessionUpdated {
            session: session.clone(),
        })
        .await;

    Ok(Json(json!({ "session": session })))
}

pub async fn admin_delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;

    let deleted = state.store.delete_session(&session_id).await?;
    if !deleted {
        return Err(ApiError::not_found("会话不存在"));
    }

    state
        .hub
        .send_to_admins(&WsEvent::SessionDeleted {
            session_id: session_id.clone(),
        })
        .await;

    Ok(Json(json!({ "deleted": true, "session_id": session_id })))
}

pub async fn admin_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &headers, None).await?;
    let (active_visitors, active_admins) = state.hub.connection_counts().await;
    let stats = state.store.stats(active_visitors, active_admins).await;
    Ok(Json(json!({ "stats": stats })))
}

// ============================================================
// WebSockets
// ============================================================

pub async fn ws_visitor(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let token = extract_visitor_token(&headers, query.token.as_deref())
        .ok_or_else(|| ApiError::unauthorized("缺少访客凭证"))?;

    if !state.store.verify_visitor_token(&session_id, &token).await? {
        return Err(ApiError::unauthorized("访客凭证无效"));
    }

    Ok(ws.on_upgrade(move |socket| handle_visitor_socket(socket, state, session_id)))
}

async fn handle_visitor_socket(socket: WebSocket, state: AppState, session_id: String) {
    let (conn_id, mut rx, became_online) = state.hub.register_visitor(&session_id).await;

    if became_online {
        state
            .hub
            .send_to_admins(&WsEvent::Presence {
                owner_online: state.hub.is_owner_online().await,
                session_id: Some(session_id.clone()),
                visitor_online: Some(true),
            })
            .await;
    }

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send initial owner presence status
    let init_event = WsEvent::Presence {
        owner_online: state.hub.is_owner_online().await,
        session_id: Some(session_id.clone()),
        visitor_online: Some(true),
    };
    if let Ok(payload) = serde_json::to_string(&init_event) {
        let _ = ws_tx.send(Message::Text(payload.into())).await;
    }

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(frame)) = ws_rx.next().await {
        match frame {
            Message::Text(text) => {
                if let Ok(client_msg) = serde_json::from_str::<ClientWsMessage>(&text) {
                    match client_msg {
                        ClientWsMessage::SendMessage { content, .. } => {
                            if let Ok(clean) = sanitize_message(&content) {
                                if let Ok((message, mut session)) =
                                    state.store.add_message(&session_id, "visitor", clean).await
                                {
                                    session.online = true;
                                    let owner_online = state.hub.is_owner_online().await;
                                    if !owner_online {
                                        state
                                            .notifier
                                            .notify_offline_message(&session, &message)
                                            .await;
                                    }
                                    state
                                        .hub
                                        .broadcast_to_session_and_admins(
                                            &session_id,
                                            &WsEvent::NewMessage { message, session },
                                        )
                                        .await;
                                }
                            }
                        }
                        ClientWsMessage::Typing { is_typing, .. } => {
                            state
                                .hub
                                .send_to_admins(&WsEvent::Typing {
                                    session_id: session_id.clone(),
                                    sender: "visitor".to_string(),
                                    is_typing,
                                })
                                .await;
                        }
                        ClientWsMessage::Ping => {
                            state
                                .hub
                                .send_to_visitor(&session_id, &WsEvent::Pong)
                                .await;
                        }
                        ClientWsMessage::MarkRead { .. } => {}
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
    let became_offline = state.hub.unregister_visitor(&session_id, conn_id).await;
    if became_offline {
        state
            .hub
            .send_to_admins(&WsEvent::Presence {
                owner_online: state.hub.is_owner_online().await,
                session_id: Some(session_id),
                visitor_online: Some(false),
            })
            .await;
    }
}

pub async fn ws_admin(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_admin(&state, &headers, query.token.as_deref()).await?;
    Ok(ws.on_upgrade(move |socket| handle_admin_socket(socket, state)))
}

async fn handle_admin_socket(socket: WebSocket, state: AppState) {
    let (conn_id, mut rx, owner_became_online) = state.hub.register_admin().await;

    if owner_became_online {
        state.notifier.reset_all_cooldowns();
        state
            .hub
            .send_to_all_visitors(&WsEvent::Presence {
                owner_online: true,
                session_id: None,
                visitor_online: None,
            })
            .await;
    }

    let (mut ws_tx, mut ws_rx) = socket.split();

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(frame)) = ws_rx.next().await {
        match frame {
            Message::Text(text) => {
                if let Ok(client_msg) = serde_json::from_str::<ClientWsMessage>(&text) {
                    match client_msg {
                        ClientWsMessage::SendMessage {
                            session_id: Some(sid),
                            content,
                        } => {
                            if let Ok(clean) = sanitize_message(&content) {
                                if let Ok((message, mut session)) =
                                    state.store.add_message(&sid, "owner", clean).await
                                {
                                    session.online = state.hub.is_visitor_online(&sid).await;
                                    state
                                        .hub
                                        .broadcast_to_session_and_admins(
                                            &sid,
                                            &WsEvent::NewMessage { message, session },
                                        )
                                        .await;
                                }
                            }
                        }
                        ClientWsMessage::Typing {
                            session_id: Some(sid),
                            is_typing,
                        } => {
                            state
                                .hub
                                .send_to_visitor(
                                    &sid,
                                    &WsEvent::Typing {
                                        session_id: sid.clone(),
                                        sender: "owner".to_string(),
                                        is_typing,
                                    },
                                )
                                .await;
                        }
                        ClientWsMessage::MarkRead { session_id } => {
                            if let Ok(Some(mut session)) =
                                state.store.mark_session_read(&session_id).await
                            {
                                session.online = state.hub.is_visitor_online(&session_id).await;
                                state
                                    .hub
                                    .send_to_admins(&WsEvent::SessionUpdated { session })
                                    .await;
                            }
                        }
                        ClientWsMessage::Ping => {
                            state.hub.send_to_admins(&WsEvent::Pong).await;
                        }
                        _ => {}
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
    let owner_became_offline = state.hub.unregister_admin(conn_id).await;
    if owner_became_offline {
        state.notifier.reset_all_cooldowns();
        state
            .hub
            .send_to_all_visitors(&WsEvent::Presence {
                owner_online: false,
                session_id: None,
                visitor_online: None,
            })
            .await;
    }
}

// ============================================================
// Helpers
// ============================================================

fn sanitize_message(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("消息内容不能为空"));
    }
    if trimmed.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ApiError::bad_request(format!(
            "消息长度不能超过 {} 字",
            MAX_MESSAGE_CHARS
        )));
    }
    Ok(trimmed.to_string())
}

fn extract_visitor_token(headers: &HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(val) = headers
        .get("x-visitor-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(val.to_string());
    }
    query_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn extract_admin_token(headers: &HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(bearer) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            let trimmed = bearer.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    query_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), ApiError> {
    let token = extract_admin_token(headers, query_token)
        .ok_or_else(|| ApiError::unauthorized("请先登录管理后台"))?;
    if !state.store.verify_admin_token(&token).await {
        return Err(ApiError::unauthorized("登录已过期，请重新登录"));
    }
    Ok(())
}

fn extract_visitor_meta(headers: &HeaderMap) -> String {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Unknown Browser");

    let os = if ua.contains("iPhone") || ua.contains("iPad") {
        "iOS"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Mac OS X") {
        "macOS"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "Unknown OS"
    };

    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Chrome/") && !ua.contains("Chromium/") {
        "Chrome"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "Browser"
    };

    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match ip {
        Some(ip_addr) => format!("{} · {} · {}", os, browser, ip_addr),
        None => format!("{} · {}", os, browser),
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
}
