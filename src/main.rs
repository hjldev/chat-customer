use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use chat_rs::cache::CachedChatStore;
use chat_rs::db::Database;
use chat_rs::hub::ChatHub;
use chat_rs::notify::BarkNotifier;
use chat_rs::{build_router, AppConfig, AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("chat_rs=info,tower_http=info")),
        )
        .init();

    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let db_path = env::var("DATABASE_PATH").unwrap_or_else(|_| "chat.db".to_string());
    let owner_name = env::var("OWNER_NAME").unwrap_or_else(|_| "我".to_string());
    let welcome_message = env::var("WELCOME_MESSAGE").unwrap_or_else(|_| {
        "你好！直接在这里给我发消息，我看到后会第一时间回复你。".to_string()
    });
    let admin_password = env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin123".to_string());
    let bark_url = env::var("BARK_URL").ok();
    let public_url = env::var("PUBLIC_URL").ok();

    let db = Database::open(&db_path)?;
    let store = CachedChatStore::new(db);
    let hub = ChatHub::new();
    let notifier = BarkNotifier::new(bark_url, public_url);

    let config = Arc::new(AppConfig {
        owner_name,
        welcome_message,
        admin_password: admin_password.clone(),
    });

    let state = AppState {
        store,
        hub,
        notifier: notifier.clone(),
        config,
    };
    let app = build_router(state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    println!("============================================================");
    println!("  chat-rs Web 实时聊天服务已启动");
    println!("  访客聊天页面 : http://localhost:{}/", port);
    println!("  专属接收/回复: http://localhost:{}/admin", port);
    println!("  数据库文件   : {}", db_path);
    println!(
        "  离线 Bark 通知: {}",
        if notifier.is_enabled() {
            "已启用（后台不在线时自动推送）"
        } else {
            "未启用"
        }
    );
    if env::var("ADMIN_PASSWORD").is_err() {
        println!("  管理密码     : {} (建议通过 ADMIN_PASSWORD 环境变量修改)", admin_password);
    } else {
        println!("  管理密码     : [已通过 ADMIN_PASSWORD 环境变量配置]");
    }
    println!("============================================================");

    axum::serve(listener, app).await?;
    Ok(())
}
