use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use uuid::Uuid;

use crate::db::{Database, DbError};
use crate::models::{CacheStats, ChatMessage, ChatSession};

const MAX_CACHED_MESSAGES_PER_SESSION: usize = 200;

/// Application cache layer sitting in front of SQLite.
/// Uses moka concurrent TTL/TTI caches with cache-aside reads and write-through/invalidation updates.
#[derive(Clone)]
pub struct CachedChatStore {
    db: Database,
    session_cache: Cache<String, ChatSession>,
    messages_cache: Cache<String, Vec<ChatMessage>>,
    session_list_cache: Cache<String, Vec<ChatSession>>,
    visitor_token_cache: Cache<String, String>,
    admin_token_cache: Cache<String, String>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl CachedChatStore {
    pub fn new(db: Database) -> Self {
        let session_cache = Cache::builder()
            .max_capacity(1_000)
            .time_to_live(Duration::from_secs(600))
            .time_to_idle(Duration::from_secs(300))
            .build();

        let messages_cache = Cache::builder()
            .max_capacity(500)
            .time_to_live(Duration::from_secs(600))
            .time_to_idle(Duration::from_secs(300))
            .build();

        let session_list_cache = Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build();

        let visitor_token_cache = Cache::builder()
            .max_capacity(2_000)
            .time_to_live(Duration::from_secs(1800))
            .build();

        let admin_token_cache = Cache::builder()
            .max_capacity(100)
            .time_to_live(Duration::from_secs(60 * 60 * 24))
            .build();

        Self {
            db,
            session_cache,
            messages_cache,
            session_list_cache,
            visitor_token_cache,
            admin_token_cache,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    async fn invalidate_session_lists(&self) {
        self.session_list_cache.invalidate_all();
    }

    pub async fn create_session(
        &self,
        visitor_name: String,
        visitor_contact: String,
        visitor_meta: String,
    ) -> Result<(ChatSession, String), DbError> {
        let (session, token) = self
            .db
            .create_session(visitor_name, visitor_contact, visitor_meta)
            .await?;

        self.session_cache
            .insert(session.id.clone(), session.clone())
            .await;
        self.messages_cache
            .insert(session.id.clone(), Vec::new())
            .await;
        self.visitor_token_cache
            .insert(session.id.clone(), token.clone())
            .await;
        self.invalidate_session_lists().await;

        Ok((session, token))
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Option<ChatSession>, DbError> {
        if let Some(cached) = self.session_cache.get(session_id).await {
            self.record_hit();
            return Ok(Some(cached));
        }

        self.record_miss();
        let session = self.db.get_session(session_id.to_string()).await?;
        if let Some(ref s) = session {
            self.session_cache.insert(s.id.clone(), s.clone()).await;
            self.visitor_token_cache
                .insert(s.id.clone(), s.visitor_token.clone())
                .await;
        }
        Ok(session)
    }

    pub async fn verify_visitor_token(&self, session_id: &str, token: &str) -> Result<bool, DbError> {
        if let Some(cached_token) = self.visitor_token_cache.get(session_id).await {
            self.record_hit();
            return Ok(constant_time_eq(cached_token.as_bytes(), token.as_bytes()));
        }

        self.record_miss();
        if let Some(session) = self.db.get_session(session_id.to_string()).await? {
            let valid = constant_time_eq(session.visitor_token.as_bytes(), token.as_bytes());
            self.visitor_token_cache
                .insert(session.id.clone(), session.visitor_token.clone())
                .await;
            self.session_cache.insert(session.id.clone(), session).await;
            Ok(valid)
        } else {
            Ok(false)
        }
    }

    pub async fn list_sessions(
        &self,
        status_filter: Option<&str>,
    ) -> Result<Vec<ChatSession>, DbError> {
        let cache_key = status_filter.unwrap_or("all").to_string();
        if let Some(cached) = self.session_list_cache.get(&cache_key).await {
            self.record_hit();
            return Ok(cached);
        }

        self.record_miss();
        let sessions = self
            .db
            .list_sessions(status_filter.map(|s| s.to_string()))
            .await?;

        self.session_list_cache
            .insert(cache_key, sessions.clone())
            .await;

        Ok(sessions)
    }

    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<ChatMessage>, DbError> {
        if let Some(cached) = self.messages_cache.get(session_id).await {
            self.record_hit();
            return Ok(cached);
        }

        self.record_miss();
        let messages = self
            .db
            .list_messages(session_id.to_string(), MAX_CACHED_MESSAGES_PER_SESSION)
            .await?;
        self.messages_cache
            .insert(session_id.to_string(), messages.clone())
            .await;
        Ok(messages)
    }

    pub async fn add_message(
        &self,
        session_id: &str,
        sender: &str,
        content: String,
    ) -> Result<(ChatMessage, ChatSession), DbError> {
        let (message, session) = self
            .db
            .insert_message(session_id.to_string(), sender.to_string(), content)
            .await?;

        // Write-through session metadata cache
        self.session_cache
            .insert(session.id.clone(), session.clone())
            .await;

        // Write-through message list cache if already warm; otherwise let next read warm it cleanly
        if let Some(mut cached_messages) = self.messages_cache.get(session_id).await {
            cached_messages.push(message.clone());
            if cached_messages.len() > MAX_CACHED_MESSAGES_PER_SESSION {
                let overflow = cached_messages.len() - MAX_CACHED_MESSAGES_PER_SESSION;
                cached_messages.drain(0..overflow);
            }
            self.messages_cache
                .insert(session_id.to_string(), cached_messages)
                .await;
        }

        self.invalidate_session_lists().await;

        Ok((message, session))
    }

    pub async fn update_visitor_profile(
        &self,
        session_id: &str,
        visitor_name: String,
        visitor_contact: String,
    ) -> Result<Option<ChatSession>, DbError> {
        let updated = self
            .db
            .update_visitor_profile(session_id.to_string(), visitor_name, visitor_contact)
            .await?;

        if let Some(ref s) = updated {
            self.session_cache.insert(s.id.clone(), s.clone()).await;
            self.invalidate_session_lists().await;
        }
        Ok(updated)
    }

    pub async fn update_session_status(
        &self,
        session_id: &str,
        status: String,
    ) -> Result<Option<ChatSession>, DbError> {
        let updated = self
            .db
            .update_session_status(session_id.to_string(), status)
            .await?;

        if let Some(ref s) = updated {
            self.session_cache.insert(s.id.clone(), s.clone()).await;
            self.invalidate_session_lists().await;
        }
        Ok(updated)
    }

    pub async fn mark_session_read(&self, session_id: &str) -> Result<Option<ChatSession>, DbError> {
        let updated = self.db.mark_session_read(session_id.to_string()).await?;
        if let Some(ref s) = updated {
            self.session_cache.insert(s.id.clone(), s.clone()).await;
            self.invalidate_session_lists().await;
        }
        Ok(updated)
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<bool, DbError> {
        let deleted = self.db.delete_session(session_id.to_string()).await?;
        if deleted {
            self.session_cache.invalidate(session_id).await;
            self.messages_cache.invalidate(session_id).await;
            self.visitor_token_cache.invalidate(session_id).await;
            self.invalidate_session_lists().await;
        }
        Ok(deleted)
    }

    pub async fn issue_admin_token(&self) -> String {
        let token = format!("adm_{}", Uuid::new_v4().simple());
        self.admin_token_cache
            .insert(token.clone(), chrono::Utc::now().to_rfc3339())
            .await;
        token
    }

    pub async fn verify_admin_token(&self, token: &str) -> bool {
        self.admin_token_cache.get(token).await.is_some()
    }

    pub async fn revoke_admin_token(&self, token: &str) {
        self.admin_token_cache.invalidate(token).await;
    }

    pub async fn stats(&self, active_visitors: usize, active_admins: usize) -> CacheStats {
        // Sync pending cache tasks so entry_count() reflects recent writes accurately
        self.session_cache.run_pending_tasks().await;
        self.messages_cache.run_pending_tasks().await;
        self.session_list_cache.run_pending_tasks().await;

        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        let hit_rate = if total == 0 {
            0.0
        } else {
            (hits as f64 / total as f64) * 100.0
        };

        CacheStats {
            hits,
            misses,
            hit_rate,
            cached_sessions: self.session_cache.entry_count(),
            cached_message_lists: self.messages_cache.entry_count(),
            cached_session_lists: self.session_list_cache.entry_count(),
            active_visitor_connections: active_visitors,
            active_admin_connections: active_admins,
        }
    }
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
