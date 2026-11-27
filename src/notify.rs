use std::time::Duration;

use moka::future::Cache;
use reqwest::Client;
use serde::Serialize;

use crate::models::{ChatMessage, ChatSession};

const MAX_BODY_CHARS: usize = 200;
const BARK_GROUP: &str = "chat";
const BARK_TTL_SECONDS: u64 = 600;
const DEFAULT_COOLDOWN_SECONDS: u64 = 600;

#[derive(Clone)]
pub struct BarkNotifier {
    client: Client,
    bark_url: Option<String>,
    admin_url: Option<String>,
    cooldown_cache: Cache<String, ()>,
}

#[derive(Debug, Serialize)]
struct BarkPayload {
    title: String,
    body: String,
    group: String,
    ttl: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    badge: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

impl BarkNotifier {
    pub fn new(bark_url: Option<String>, public_url: Option<String>) -> Self {
        Self::with_cooldown(
            bark_url,
            public_url,
            Duration::from_secs(DEFAULT_COOLDOWN_SECONDS),
        )
    }

    pub fn with_cooldown(
        bark_url: Option<String>,
        public_url: Option<String>,
        cooldown: Duration,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("failed to build reqwest client");

        let bark_url = bark_url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());

        let admin_url = public_url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .map(|u| format!("{}/admin", u));

        let cooldown_cache = Cache::builder()
            .max_capacity(5_000)
            .time_to_live(cooldown)
            .build();

        Self {
            client,
            bark_url,
            admin_url,
            cooldown_cache,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.bark_url.is_some()
    }

    /// Reset per-session Bark notification cooldowns for all sessions (e.g. when admin comes online).
    pub fn reset_all_cooldowns(&self) {
        self.cooldown_cache.invalidate_all();
        tracing::debug!("Reset all Bark notification cooldowns");
    }

    /// Reset Bark notification cooldown for a specific session (e.g. when admin replies or marks read).
    pub async fn reset_session_cooldown(&self, session_id: &str) {
        self.cooldown_cache.invalidate(session_id).await;
        tracing::debug!(session_id = %session_id, "Reset Bark notification cooldown for session");
    }

    pub async fn notify_offline_message(&self, session: &ChatSession, message: &ChatMessage) {
        let Some(url) = self.bark_url.clone() else {
            return;
        };

        // Atomic per-user (per-session) 10-minute cooldown via moka TTL cache
        let entry = self
            .cooldown_cache
            .entry(session.id.clone())
            .or_insert(())
            .await;
        if !entry.is_fresh() {
            tracing::debug!(
                session_id = %session.id,
                "Skipped Bark notification due to per-user cooldown"
            );
            return;
        }

        let title = if session.visitor_contact.is_empty() {
            format!("新消息 · {}", session.visitor_name)
        } else {
            format!("新消息 · {} ({})", session.visitor_name, session.visitor_contact)
        };

        let body: String = if message.content.chars().count() > MAX_BODY_CHARS {
            let truncated: String = message.content.chars().take(MAX_BODY_CHARS).collect();
            format!("{}…", truncated)
        } else {
            message.content.clone()
        };

        let payload = BarkPayload {
            title,
            body,
            group: BARK_GROUP.to_string(),
            ttl: BARK_TTL_SECONDS,
            badge: if session.unread_count > 0 {
                Some(session.unread_count)
            } else {
                None
            },
            url: self.admin_url.clone(),
        };

        let client = self.client.clone();
        let session_id = session.id.clone();

        tokio::spawn(async move {
            match client.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(session_id = %session_id, "Bark offline notification sent");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        session_id = %session_id,
                        %status,
                        body = %text,
                        "Bark notification returned non-success status"
                    );
                }
                Err(err) => {
                    let sanitized = err.without_url();
                    tracing::warn!(
                        session_id = %session_id,
                        error = %sanitized,
                        "Failed to send Bark notification"
                    );
                }
            }
        });
    }
}
