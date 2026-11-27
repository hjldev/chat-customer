use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use chat_rs::cache::CachedChatStore;
use chat_rs::db::Database;
use chat_rs::hub::ChatHub;
use chat_rs::notify::BarkNotifier;
use chat_rs::{build_router, AppConfig, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt;

fn test_app() -> axum::Router {
    let db = Database::open_in_memory().expect("open in-memory sqlite");
    let store = CachedChatStore::new(db);
    let hub = ChatHub::new();
    let notifier = BarkNotifier::new(None, None);
    let config = Arc::new(AppConfig {
        owner_name: "测试主人".to_string(),
        welcome_message: "欢迎咨询".to_string(),
        admin_password: "secret-password".to_string(),
    });
    build_router(AppState {
        store,
        hub,
        notifier,
        config,
    })
}

async fn response_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn full_visitor_and_admin_chat_flow_with_cache() {
    let app = test_app();

    // 1. Visitor creates session
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "visitor_name": "小明", "visitor_contact": "xm@example.com" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let init = response_json(resp).await;
    let session_id = init["session"]["id"].as_str().unwrap().to_string();
    let visitor_token = init["visitor_token"].as_str().unwrap().to_string();
    assert_eq!(init["session"]["visitor_name"], "小明");

    // 2. Wrong visitor token is rejected
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/visitor/sessions/{}", session_id))
                .header("x-visitor-token", "wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 3. Visitor sends message
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", session_id))
                .header("content-type", "application/json")
                .header("x-visitor-token", &visitor_token)
                .body(Body::from(json!({ "content": "你好，请问在吗？" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = response_json(resp).await;
    assert_eq!(sent["message"]["sender"], "visitor");
    assert_eq!(sent["session"]["unread_count"], 1);

    // 4. Admin login: wrong password fails, right password succeeds
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "password": "wrong" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "password": "secret-password" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let admin_token = response_json(resp).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    // 5. Admin lists sessions twice to exercise session_list_cache
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/admin/sessions")
                    .header("authorization", format!("Bearer {}", admin_token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = response_json(resp).await;
        assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(list["sessions"][0]["unread_count"], 1);
    }

    // 6. Admin replies to visitor
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/admin/sessions/{}/messages", session_id))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::from(
                    json!({ "content": "我在的，请问有什么可以帮你？" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let reply = response_json(resp).await;
    assert_eq!(reply["message"]["sender"], "owner");
    assert_eq!(reply["session"]["unread_count"], 0);

    // 7. Visitor reads session and sees both messages (served from application cache)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/visitor/sessions/{}", session_id))
                .header("x-visitor-token", &visitor_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = response_json(resp).await;
    let msgs = detail["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], "你好，请问在吗？");
    assert_eq!(msgs[1]["content"], "我在的，请问有什么可以帮你？");

    // 8. Admin checks cache metrics
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/admin/stats")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let stats = response_json(resp).await;
    assert!(stats["stats"]["hits"].as_u64().unwrap() >= 3);
    assert!(stats["stats"]["cached_sessions"].as_u64().unwrap() >= 1);

    // 9. Admin deletes session and verifies cache eviction + 404
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/admin/sessions/{}", session_id))
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/admin/sessions/{}/messages", session_id))
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_profile_archive_and_realtime_hub_delivery() {
    let db = Database::open_in_memory().expect("open in-memory sqlite");
    let store = CachedChatStore::new(db);
    let hub = ChatHub::new();
    let config = Arc::new(AppConfig {
        owner_name: "主人".to_string(),
        welcome_message: "你好".to_string(),
        admin_password: "pass".to_string(),
    });
    let app = build_router(AppState {
        store,
        hub: hub.clone(),
        notifier: BarkNotifier::new(None, None),
        config,
    });

    // Verify HTML & CSS pages are served properly
    for (path, expected_snippet) in [
        ("/", "在线联系我"),
        ("/admin", "消息收件箱"),
        ("/static/styles.css", "--primary"),
        ("/health", "\"ok\""),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "failed route {}", path);
        let body = String::from_utf8(
            resp.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
        .unwrap();
        assert!(body.contains(expected_snippet), "missing snippet on {}", path);
    }

    // Create visitor session & connect virtual WS channels to Hub
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let init = response_json(resp).await;
    let sid = init["session"]["id"].as_str().unwrap().to_string();
    let vtoken = init["visitor_token"].as_str().unwrap().to_string();

    let (v_conn_id, mut v_rx, v_became_online) = hub.register_visitor(&sid).await;
    let (a_conn_id, mut a_rx, a_became_online) = hub.register_admin().await;
    assert!(v_became_online);
    assert!(a_became_online);
    assert!(hub.is_owner_online().await);
    assert!(hub.is_visitor_online(&sid).await);

    // Visitor updates profile -> admin receives real-time session_updated event
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/visitor/sessions/{}/profile", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(
                    json!({ "visitor_name": "小红", "visitor_contact": "wx:xiaohong" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let admin_event: Value = serde_json::from_str(&a_rx.recv().await.unwrap()).unwrap();
    assert_eq!(admin_event["type"], "session_updated");
    assert_eq!(admin_event["session"]["visitor_name"], "小红");

    // Admin logs in & replies -> both visitor and admin sockets receive new_message
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "password": "pass" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let admin_token = response_json(resp).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/admin/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::from(json!({ "content": "实时推送测试" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let visitor_event: Value = serde_json::from_str(&v_rx.recv().await.unwrap()).unwrap();
    assert_eq!(visitor_event["type"], "new_message");
    assert_eq!(visitor_event["message"]["content"], "实时推送测试");

    let admin_msg_event: Value = serde_json::from_str(&a_rx.recv().await.unwrap()).unwrap();
    assert_eq!(admin_msg_event["type"], "new_message");
    assert_eq!(admin_msg_event["message"]["content"], "实时推送测试");

    // Archive and logout revoke token
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/admin/sessions/{}/status", sid))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::from(json!({ "status": "archived" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(response_json(resp).await["session"]["status"], "archived");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/logout")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Token is now revoked from cache
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/admin/me")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    assert!(hub.unregister_visitor(&sid, v_conn_id).await);
    assert!(hub.unregister_admin(a_conn_id).await);
    assert!(!hub.is_owner_online().await);
}

#[tokio::test]
async fn bark_notification_sent_only_when_admin_offline() {
    let (bark_tx, mut bark_rx) = mpsc::unbounded_channel::<Value>();
    let mock_bark = Router::new().route(
        "/test-bark-key",
        post(move |Json(payload): Json<Value>| {
            let tx = bark_tx.clone();
            async move {
                let _ = tx.send(payload);
                (StatusCode::OK, Json(json!({ "code": 200, "message": "success" })))
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock bark server");
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_bark).await;
    });

    let db = Database::open_in_memory().expect("open in-memory sqlite");
    let store = CachedChatStore::new(db);
    let hub = ChatHub::new();
    let notifier = BarkNotifier::new(
        Some(format!("http://{}/test-bark-key", addr)),
        Some("https://chat.example.com".to_string()),
    );
    let config = Arc::new(AppConfig {
        owner_name: "主人".to_string(),
        welcome_message: "你好".to_string(),
        admin_password: "pass".to_string(),
    });
    let app = build_router(AppState {
        store,
        hub: hub.clone(),
        notifier,
        config,
    });

    // Visitor creates session
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "visitor_name": "访客A", "visitor_contact": "a@test.com" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let init = response_json(resp).await;
    let sid = init["session"]["id"].as_str().unwrap().to_string();
    let vtoken = init["visitor_token"].as_str().unwrap().to_string();

    // Case 1: Admin is OFFLINE -> visitor sends message -> Bark notification MUST fire
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "离线测试消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let pushed = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("expected bark notification when admin offline")
        .expect("bark channel closed");
    assert_eq!(pushed["title"], "新消息 · 访客A (a@test.com)");
    assert_eq!(pushed["body"], "离线测试消息");
    assert_eq!(pushed["group"], "chat");
    assert_eq!(pushed["ttl"], 600);
    assert_eq!(pushed["badge"], 1);
    assert_eq!(pushed["url"], "https://chat.example.com/admin");

    // Case 1b: Same visitor sends a second message within 10 minutes -> throttled (NO Bark)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "10分钟内第二条消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let throttled = tokio::time::timeout(Duration::from_millis(250), bark_rx.recv()).await;
    assert!(
        throttled.is_err(),
        "same visitor should only trigger Bark once within 10 minutes"
    );

    // Case 1c: A different visitor sends a message while admin is offline -> Bark MUST fire
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "visitor_name": "访客B" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let init_b = response_json(resp).await;
    let sid_b = init_b["session"]["id"].as_str().unwrap().to_string();
    let vtoken_b = init_b["visitor_token"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid_b))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken_b)
                .body(Body::from(json!({ "content": "访客B的消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let pushed_b = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("different visitor should trigger independent Bark notification")
        .expect("bark channel closed");
    assert_eq!(pushed_b["title"], "新消息 · 访客B");
    assert_eq!(pushed_b["body"], "访客B的消息");

    // Case 2: Admin comes ONLINE -> new visitor sends message -> Bark notification MUST NOT fire
    let (admin_conn_id, _admin_rx, _) = hub.register_admin().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "visitor_name": "访客C" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let init_c = response_json(resp).await;
    let sid_c = init_c["session"]["id"].as_str().unwrap().to_string();
    let vtoken_c = init_c["visitor_token"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid_c))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken_c)
                .body(Body::from(json!({ "content": "在线时发的消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let should_timeout = tokio::time::timeout(Duration::from_millis(250), bark_rx.recv()).await;
    assert!(
        should_timeout.is_err(),
        "bark notification should NOT fire while admin is online"
    );

    hub.unregister_admin(admin_conn_id).await;
    server_handle.abort();
}

#[tokio::test]
async fn bark_cooldown_allows_notification_again_after_expiry() {
    let (bark_tx, mut bark_rx) = mpsc::unbounded_channel::<Value>();
    let mock_bark = Router::new().route(
        "/test-bark-expiry",
        post(move |Json(payload): Json<Value>| {
            let tx = bark_tx.clone();
            async move {
                let _ = tx.send(payload);
                (StatusCode::OK, Json(json!({ "code": 200 })))
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock bark server");
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_bark).await;
    });

    let notifier = BarkNotifier::with_cooldown(
        Some(format!("http://{}/test-bark-expiry", addr)),
        None,
        Duration::from_millis(120),
    );
    let session = chat_rs::models::ChatSession {
        id: "sess-1".to_string(),
        visitor_token: "tok".to_string(),
        visitor_name: "访客".to_string(),
        visitor_contact: String::new(),
        visitor_meta: String::new(),
        status: "open".to_string(),
        unread_count: 1,
        last_message: None,
        last_message_sender: None,
        last_message_at: None,
        created_at: String::new(),
        updated_at: String::new(),
        online: false,
    };
    let msg1 = chat_rs::models::ChatMessage {
        id: 1,
        session_id: "sess-1".to_string(),
        sender: "visitor".to_string(),
        content: "第一条".to_string(),
        created_at: String::new(),
    };
    let msg2 = chat_rs::models::ChatMessage {
        id: 2,
        session_id: "sess-1".to_string(),
        sender: "visitor".to_string(),
        content: "冷却期内第二条".to_string(),
        created_at: String::new(),
    };
    let msg3 = chat_rs::models::ChatMessage {
        id: 3,
        session_id: "sess-1".to_string(),
        sender: "visitor".to_string(),
        content: "冷却期后第三条".to_string(),
        created_at: String::new(),
    };

    notifier.notify_offline_message(&session, &msg1).await;
    notifier.notify_offline_message(&session, &msg2).await;

    let first = tokio::time::timeout(Duration::from_secs(2), bark_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["body"], "第一条");
    assert!(
        tokio::time::timeout(Duration::from_millis(60), bark_rx.recv())
            .await
            .is_err()
    );

    tokio::time::sleep(Duration::from_millis(120)).await;
    notifier.notify_offline_message(&session, &msg3).await;

    let third = tokio::time::timeout(Duration::from_secs(2), bark_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third["body"], "冷却期后第三条");

    server_handle.abort();
}

#[tokio::test]
async fn bark_cooldown_resets_when_admin_comes_online_and_goes_offline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (bark_tx, mut bark_rx) = mpsc::unbounded_channel::<Value>();
    let mock_bark = Router::new().route(
        "/test-bark-reset",
        post(move |Json(payload): Json<Value>| {
            let tx = bark_tx.clone();
            async move {
                let _ = tx.send(payload);
                (StatusCode::OK, Json(json!({ "code": 200 })))
            }
        }),
    );

    let bark_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock bark server");
    let bark_addr = bark_listener.local_addr().unwrap();
    let bark_handle = tokio::spawn(async move {
        let _ = axum::serve(bark_listener, mock_bark).await;
    });

    let db = Database::open_in_memory().expect("open in-memory sqlite");
    let store = CachedChatStore::new(db);
    let hub = ChatHub::new();
    let notifier = BarkNotifier::new(
        Some(format!("http://{}/test-bark-reset", bark_addr)),
        None,
    );
    let config = Arc::new(AppConfig {
        owner_name: "主人".to_string(),
        welcome_message: "你好".to_string(),
        admin_password: "pass".to_string(),
    });
    let app = build_router(AppState {
        store,
        hub: hub.clone(),
        notifier,
        config,
    });

    let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind app server");
    let app_addr = app_listener.local_addr().unwrap();
    let app_for_server = app.clone();
    let app_handle = tokio::spawn(async move {
        let _ = axum::serve(app_listener, app_for_server).await;
    });

    // Visitor creates session A and session B
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "visitor_name": "访客A" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let init = response_json(resp).await;
    let sid = init["session"]["id"].as_str().unwrap().to_string();
    let vtoken = init["visitor_token"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/visitor/sessions")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "visitor_name": "访客B" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let init_b = response_json(resp).await;
    let sid_b = init_b["session"]["id"].as_str().unwrap().to_string();
    let vtoken_b = init_b["visitor_token"].as_str().unwrap().to_string();

    // 1. Admin offline: visitors A and B send first messages -> Bark fires for both
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "第一条离线消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let first = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("expected first bark")
        .unwrap();
    assert_eq!(first["body"], "第一条离线消息");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid_b))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken_b)
                .body(Body::from(json!({ "content": "访客B离线消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let first_b = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("expected first bark for visitor B")
        .unwrap();
    assert_eq!(first_b["body"], "访客B离线消息");

    // 2. Same visitor sends second message within 10 min -> throttled
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "冷却期内消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), bark_rx.recv())
            .await
            .is_err(),
        "should be throttled before admin comes online"
    );

    // 3. Admin logs in and connects via WebSocket (/ws/admin)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "password": "pass" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let admin_token = response_json(resp).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let mut ws_stream = tokio::net::TcpStream::connect(app_addr)
        .await
        .expect("connect to app ws");
    let upgrade_req = format!(
        "GET /ws/admin?token={} HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n",
        admin_token, app_addr
    );
    ws_stream.write_all(upgrade_req.as_bytes()).await.unwrap();
    let mut buf = [0u8; 512];
    let n = ws_stream.read(&mut buf).await.unwrap();
    let resp_head = String::from_utf8_lossy(&buf[..n]);
    assert!(
        resp_head.starts_with("HTTP/1.1 101"),
        "expected 101 Switching Protocols, got: {}",
        resp_head
    );

    // Wait until hub registers admin online
    for _ in 0..50 {
        if hub.is_owner_online().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(hub.is_owner_online().await);

    // Admin replies while online
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/admin/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::from(json!({ "content": "收到，我来回复你" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 4. Admin disconnects (goes offline again)
    drop(ws_stream);
    for _ in 0..50 {
        if !hub.is_owner_online().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!hub.is_owner_online().await);

    // 5. Visitors A (replied) and B (not replied, only WS online/offline) send new messages
    //    within original 10 min -> Bark MUST fire again for both!
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "管理员下线后的新消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after_reset = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("expected bark notification after admin went online and offline")
        .unwrap();
    assert_eq!(after_reset["body"], "管理员下线后的新消息");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid_b))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken_b)
                .body(Body::from(json!({ "content": "访客B下线后的新消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after_reset_b = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("expected bark notification for visitor B after admin came online")
        .unwrap();
    assert_eq!(after_reset_b["body"], "访客B下线后的新消息");

    // 6. A subsequent message immediately after enters a fresh 10-minute cooldown -> throttled
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "新一轮冷却期内消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), bark_rx.recv())
            .await
            .is_err(),
        "new cooldown should apply after the post-reset Bark notification"
    );

    // 7. Even without WS connect, admin REST reply resets cooldown for that session
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/admin/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", admin_token))
                .body(Body::from(json!({ "content": "通过接口直接回复" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/visitor/sessions/{}/messages", sid))
                .header("content-type", "application/json")
                .header("x-visitor-token", &vtoken)
                .body(Body::from(json!({ "content": "接口回复后的新消息" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after_rest_reply = tokio::time::timeout(Duration::from_secs(3), bark_rx.recv())
        .await
        .expect("expected bark notification after admin REST reply")
        .unwrap();
    assert_eq!(after_rest_reply["body"], "接口回复后的新消息");

    app_handle.abort();
    bark_handle.abort();
}
