# chat-rs

基于 **Rust (Axum + WebSocket) + SQLite (WAL 模式) + 应用缓存 (Moka)** 构建的轻量级实时 Web 聊天应用。

- **访客聊天页 (`/`)**：任何人打开网页即可直接向你发消息，自动保留本地会话凭证与历史记录，支持填写称呼与联系方式、实时查看你是否在线、正在输入提示。
- **专属收件箱/回复页 (`/admin`)**：受管理密码保护的双栏工作台，实时接收所有访客消息（支持提示音与未读角标）、直接回复、搜索/筛选/归档/删除会话，并内置应用缓存命中率与在线连接监控面板。

## 架构概览

- **持久化层 (`src/db.rs`)**：使用 `rusqlite`（内置 `bundled` SQLite，无需额外安装系统库），开启 `WAL` 日志模式和外键级联删除，通过 `spawn_blocking` 避免阻塞 Tokio 异步线程。
- **应用缓存层 (`src/cache.rs`)**：使用 `moka::future::Cache` 实现多层内存缓存（会话元数据缓存、每会话最近 200 条消息缓存、会话列表缓存、访客与管理员 Token 缓存），采用 **Cache-Aside 读 + Write-Through 写 + 变更失效** 策略，并实时统计命中率。
- **实时通信层 (`src/hub.rs`)**：基于 WebSocket 维护访客与管理员多标签页连接，支持消息实时双向推送、在线状态同步和输入状态提示。
- **单二进制部署**：前端页面（`static/index.html`、`static/admin.html`、`static/styles.css`）在编译期通过 `include_str!` 内嵌进二进制文件。

## 快速运行

```bash
# 默认启动（端口 3000，默认管理密码 admin123，数据库 chat.db）
cargo run

# 自定义环境变量启动
OWNER_NAME="小明" \
ADMIN_PASSWORD="your-strong-password" \
WELCOME_MESSAGE="你好！有事直接给我留言，我会尽快回复。" \
PORT=3000 \
DATABASE_PATH="./chat.db" \
cargo run
```

启动后访问：
- 访客联系页面：`http://localhost:3000/`
- 专属接收与回复页面：`http://localhost:3000/admin`

## 环境变量

| 变量名 | 默认值 | 说明 |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | 监听地址 |
| `PORT` | `3000` | 监听端口 |
| `DATABASE_PATH` | `chat.db` | SQLite 数据库文件路径 |
| `ADMIN_PASSWORD` | `admin123` | `/admin` 登录密码（生产环境请务必修改） |
| `OWNER_NAME` | `我` | 访客页面展示的你的称呼 |
| `WELCOME_MESSAGE` | `你好！直接在这里给我发消息...` | 访客进入页面时的欢迎语 |
| `BARK_URL` | 未配置（关闭） | 后台管理不在线时推送通知的 Bark URL（如 `https://api.day.app/your_key`，同用户 10 分钟内仅通知一次） |
| `PUBLIC_URL` | （可选） | 公网访问根地址（如 `https://chat.example.com`），配置后点击 Bark 通知可直接跳转 `/admin` |

## 运行测试

```bash
cargo test
```

## GitHub Actions 手动打包（Linux x86_64）

仓库已配置 [.github/workflows/release-linux.yml](.github/workflows/release-linux.yml)，仅在 GitHub Actions 页面点击 **Run workflow** 时手动触发，编译零外部依赖的 `linux-x86_64` 静态单二进制包（`x86_64-unknown-linux-musl`，前端与 SQLite 均已内嵌）：

- 默认直接生成并上传 `chat-rs-linux-x86_64.tar.gz`（可在 Actions 运行详情页下载 Artifacts）。
- 若手动运行时填写了可选的 `release_tag`（如 `v0.1.0`），还会自动创建对应的 GitHub Release。
