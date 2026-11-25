use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use crate::models::WsEvent;

type Tx = mpsc::UnboundedSender<String>;

#[derive(Clone, Default)]
pub struct ChatHub {
    inner: Arc<HubInner>,
}

#[derive(Default)]
struct HubInner {
    next_id: AtomicU64,
    visitors: RwLock<HashMap<String, HashMap<u64, Tx>>>,
    admins: RwLock<HashMap<u64, Tx>>,
}

impl ChatHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_conn_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn register_visitor(
        &self,
        session_id: &str,
    ) -> (u64, mpsc::UnboundedReceiver<String>, bool) {
        let conn_id = self.next_conn_id();
        let (tx, rx) = mpsc::unbounded_channel();

        let mut visitors = self.inner.visitors.write().await;
        let entry = visitors.entry(session_id.to_string()).or_default();
        let became_online = entry.is_empty();
        entry.insert(conn_id, tx);

        (conn_id, rx, became_online)
    }

    pub async fn unregister_visitor(&self, session_id: &str, conn_id: u64) -> bool {
        let mut visitors = self.inner.visitors.write().await;
        if let Some(conns) = visitors.get_mut(session_id) {
            conns.remove(&conn_id);
            if conns.is_empty() {
                visitors.remove(session_id);
                return true;
            }
        }
        false
    }

    pub async fn register_admin(&self) -> (u64, mpsc::UnboundedReceiver<String>, bool) {
        let conn_id = self.next_conn_id();
        let (tx, rx) = mpsc::unbounded_channel();

        let mut admins = self.inner.admins.write().await;
        let became_online = admins.is_empty();
        admins.insert(conn_id, tx);

        (conn_id, rx, became_online)
    }

    pub async fn unregister_admin(&self, conn_id: u64) -> bool {
        let mut admins = self.inner.admins.write().await;
        admins.remove(&conn_id);
        admins.is_empty()
    }

    pub async fn is_owner_online(&self) -> bool {
        !self.inner.admins.read().await.is_empty()
    }

    pub async fn is_visitor_online(&self, session_id: &str) -> bool {
        self.inner
            .visitors
            .read()
            .await
            .get(session_id)
            .map(|m| !m.is_empty())
            .unwrap_or(false)
    }

    pub async fn online_visitor_ids(&self) -> HashSet<String> {
        self.inner.visitors.read().await.keys().cloned().collect()
    }

    pub async fn connection_counts(&self) -> (usize, usize) {
        let visitor_conns: usize = self
            .inner
            .visitors
            .read()
            .await
            .values()
            .map(|m| m.len())
            .sum();
        let admin_conns = self.inner.admins.read().await.len();
        (visitor_conns, admin_conns)
    }

    pub async fn send_to_visitor(&self, session_id: &str, event: &WsEvent) {
        let Ok(payload) = serde_json::to_string(event) else {
            return;
        };
        let visitors = self.inner.visitors.read().await;
        if let Some(conns) = visitors.get(session_id) {
            for tx in conns.values() {
                let _ = tx.send(payload.clone());
            }
        }
    }

    pub async fn send_to_all_visitors(&self, event: &WsEvent) {
        let Ok(payload) = serde_json::to_string(event) else {
            return;
        };
        let visitors = self.inner.visitors.read().await;
        for conns in visitors.values() {
            for tx in conns.values() {
                let _ = tx.send(payload.clone());
            }
        }
    }

    pub async fn send_to_admins(&self, event: &WsEvent) {
        let Ok(payload) = serde_json::to_string(event) else {
            return;
        };
        let admins = self.inner.admins.read().await;
        for tx in admins.values() {
            let _ = tx.send(payload.clone());
        }
    }

    pub async fn broadcast_to_session_and_admins(&self, session_id: &str, event: &WsEvent) {
        self.send_to_visitor(session_id, event).await;
        self.send_to_admins(event).await;
    }
}
