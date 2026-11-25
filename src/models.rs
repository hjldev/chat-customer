use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    #[serde(skip_serializing)]
    pub visitor_token: String,
    pub visitor_name: String,
    pub visitor_contact: String,
    pub visitor_meta: String,
    pub status: String,
    pub unread_count: i64,
    pub last_message: Option<String>,
    pub last_message_sender: Option<String>,
    pub last_message_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub session_id: String,
    pub sender: String, // "visitor" | "owner"
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VisitorSessionInitResponse {
    pub session: ChatSession,
    pub visitor_token: String,
    pub messages: Vec<ChatMessage>,
    pub owner_online: bool,
    pub owner_name: String,
    pub welcome_message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VisitorSessionDetailResponse {
    pub session: ChatSession,
    pub messages: Vec<ChatMessage>,
    pub owner_online: bool,
    pub owner_name: String,
    pub welcome_message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub cached_sessions: u64,
    pub cached_message_lists: u64,
    pub cached_session_lists: u64,
    pub active_visitor_connections: usize,
    pub active_admin_connections: usize,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub visitor_name: Option<String>,
    pub visitor_contact: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateVisitorProfileRequest {
    pub visitor_name: String,
    pub visitor_contact: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionStatusRequest {
    pub status: String, // "open" | "archived"
}

#[derive(Debug, Deserialize)]
pub struct AdminLoginRequest {
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AdminLoginResponse {
    pub token: String,
    pub owner_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    NewMessage {
        message: ChatMessage,
        session: ChatSession,
    },
    SessionUpdated {
        session: ChatSession,
    },
    SessionDeleted {
        session_id: String,
    },
    Presence {
        owner_online: bool,
        session_id: Option<String>,
        visitor_online: Option<bool>,
    },
    Typing {
        session_id: String,
        sender: String,
        is_typing: bool,
    },
    Pong,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientWsMessage {
    SendMessage {
        session_id: Option<String>,
        content: String,
    },
    Typing {
        session_id: Option<String>,
        is_typing: bool,
    },
    MarkRead {
        session_id: String,
    },
    Ping,
}
