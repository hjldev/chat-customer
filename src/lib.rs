pub mod cache;
pub mod db;
pub mod handlers;
pub mod hub;
pub mod models;
pub mod notify;

use std::sync::Arc;

use axum::routing::{delete, get, patch, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::cache::CachedChatStore;
use crate::hub::ChatHub;
use crate::notify::BarkNotifier;

#[derive(Clone)]
pub struct AppConfig {
    pub owner_name: String,
    pub welcome_message: String,
    pub admin_password: String,
}

#[derive(Clone)]
pub struct AppState {
    pub store: CachedChatStore,
    pub hub: ChatHub,
    pub notifier: BarkNotifier,
    pub config: Arc<AppConfig>,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Pages & static assets
        .route("/", get(handlers::serve_index))
        .route("/admin", get(handlers::serve_admin))
        .route("/static/styles.css", get(handlers::serve_styles))
        .route("/health", get(handlers::health_check))
        // Visitor REST API
        .route("/api/visitor/sessions", post(handlers::create_visitor_session))
        .route("/api/visitor/sessions/{id}", get(handlers::get_visitor_session))
        .route(
            "/api/visitor/sessions/{id}/profile",
            patch(handlers::update_visitor_profile),
        )
        .route(
            "/api/visitor/sessions/{id}/messages",
            post(handlers::send_visitor_message),
        )
        // Admin REST API
        .route("/api/admin/login", post(handlers::admin_login))
        .route("/api/admin/logout", post(handlers::admin_logout))
        .route("/api/admin/me", get(handlers::admin_me))
        .route("/api/admin/sessions", get(handlers::admin_list_sessions))
        .route(
            "/api/admin/sessions/{id}/messages",
            get(handlers::admin_get_messages).post(handlers::admin_send_message),
        )
        .route(
            "/api/admin/sessions/{id}/read",
            post(handlers::admin_mark_read),
        )
        .route(
            "/api/admin/sessions/{id}/status",
            patch(handlers::admin_update_status),
        )
        .route(
            "/api/admin/sessions/{id}",
            delete(handlers::admin_delete_session),
        )
        .route("/api/admin/stats", get(handlers::admin_stats))
        // WebSockets
        .route("/ws/visitor/{id}", get(handlers::ws_visitor))
        .route("/ws/admin", get(handlers::ws_admin))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
