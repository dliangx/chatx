use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bcrypt::{hash, verify, DEFAULT_COST};
use chrono::{Duration, Utc};
use dashmap::DashMap;
use futures_util::{sink::SinkExt, stream::StreamExt};
use include_dir::{include_dir, Dir};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use mime_guess::from_path;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    username: String,
    message: String,
    channel: String,
    message_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    id: Uuid,
    username: String,
    email: String,
    password_hash: String,
    created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String, // user id
    username: String,
    exp: usize,
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    username: String,
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct AuthResponse {
    token: String,
    user: UserResponse,
}

#[derive(Debug, Serialize)]
struct UserResponse {
    id: Uuid,
    username: String,
    email: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

struct Channel {
    tx: broadcast::Sender<String>,
    users: DashMap<String, ()>,
}

impl Default for Channel {
    fn default() -> Self {
        let (tx, _rx) = broadcast::channel(100);
        Self {
            tx,
            users: DashMap::new(),
        }
    }
}

struct AppState {
    channels: DashMap<String, Arc<Channel>>,
    users: DashMap<String, User>,     // username -> User
    users_by_id: DashMap<Uuid, User>, // id -> User
    jwt_secret: String,
}

// 静态文件目录（前端打包产物）
static STATIC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../frontend/dist");

#[tokio::main]
async fn main() {
    let jwt_secret = std::env::var("JWT_SECRET")
        .unwrap_or_else(|_| "your-secret-key-change-this-in-production".to_string());

    let app_state = Arc::new(AppState {
        channels: DashMap::new(),
        users: DashMap::new(),
        users_by_id: DashMap::new(),
        jwt_secret,
    });

    let cors = CorsLayer::new()
        .allow_origin("http://localhost:3000".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ]);

    let app = Router::new()
        .route("/ws", get(websocket_handler))
        .route("/api/channels", get(get_channels_handler))
        .route("/api/auth/register", post(register_handler))
        .route("/api/auth/login", post(login_handler))
        .route("/api/auth/verify", post(verify_token_handler))
        // 静态文件服务，根路径单独处理
        .route("/", get(static_index_handler))
        .route("/*path", get(static_handler))
        .with_state(app_state)
        .layer(cors);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// 用户注册
async fn register_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 验证输入
    if req.username.trim().is_empty() || req.email.trim().is_empty() || req.password.len() < 6 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Username and email are required, password must be at least 6 characters"
                    .to_string(),
            }),
        ));
    }

    // 检查用户名是否已存在
    if state.users.contains_key(&req.username) {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "Username already exists".to_string(),
            }),
        ));
    }

    // 哈希密码
    let password_hash = match hash(&req.password, DEFAULT_COST) {
        Ok(hash) => hash,
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to hash password".to_string(),
                }),
            ));
        }
    };

    // 创建用户
    let user_id = Uuid::new_v4();
    let user = User {
        id: user_id,
        username: req.username.clone(),
        email: req.email.clone(),
        password_hash,
        created_at: Utc::now(),
    };

    // 保存用户
    state.users.insert(req.username.clone(), user.clone());
    state.users_by_id.insert(user_id, user.clone());

    // 生成JWT token
    let claims = Claims {
        sub: user_id.to_string(),
        username: user.username.clone(),
        exp: (Utc::now() + Duration::hours(24)).timestamp() as usize,
    };

    let token = match encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_ref()),
    ) {
        Ok(token) => token,
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to generate token".to_string(),
                }),
            ));
        }
    };

    Ok(Json(AuthResponse {
        token,
        user: UserResponse {
            id: user.id,
            username: user.username,
            email: user.email,
        },
    }))
}

// 用户登录
async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 查找用户
    let user = match state.users.get(&req.username) {
        Some(user) => user.clone(),
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid username or password".to_string(),
                }),
            ));
        }
    };

    // 验证密码
    if !verify(&req.password, &user.password_hash).unwrap_or(false) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid username or password".to_string(),
            }),
        ));
    }

    // 生成JWT token
    let claims = Claims {
        sub: user.id.to_string(),
        username: user.username.clone(),
        exp: (Utc::now() + Duration::hours(24)).timestamp() as usize,
    };

    let token = match encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_ref()),
    ) {
        Ok(token) => token,
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to generate token".to_string(),
                }),
            ));
        }
    };

    Ok(Json(AuthResponse {
        token,
        user: UserResponse {
            id: user.id,
            username: user.username,
            email: user.email,
        },
    }))
}

// 验证token
async fn verify_token_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<UserResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = match params.get("token") {
        Some(token) => token,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Token is required".to_string(),
                }),
            ));
        }
    };

    let claims = match decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.jwt_secret.as_ref()),
        &Validation::default(),
    ) {
        Ok(claims) => claims.claims,
        Err(_) => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid token".to_string(),
                }),
            ));
        }
    };

    let user_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid token".to_string(),
                }),
            ));
        }
    };

    let user = match state.users_by_id.get(&user_id) {
        Some(user) => user.clone(),
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "User not found".to_string(),
                }),
            ));
        }
    };

    Ok(Json(UserResponse {
        id: user.id,
        username: user.username,
        email: user.email,
    }))
}

// 新增：根路径 handler
async fn static_index_handler() -> Response {
    if let Some(file) = STATIC_DIR.get_file("index.html") {
        let mime = from_path("index.html").first_or_octet_stream();
        (
            axum::http::StatusCode::OK,
            [("content-type", mime.as_ref())],
            file.contents(),
        )
            .into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, "404 Not Found").into_response()
    }
}

// 获取所有频道的 handler
async fn get_channels_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let channels: Vec<String> = state
        .channels
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    axum::Json(channels)
}

// 静态文件 handler
async fn static_handler(Path(path): Path<String>) -> Response {
    let rel_path = path.trim_start_matches("/");
    if let Some(file) = STATIC_DIR.get_file(rel_path) {
        let mime = from_path(rel_path).first_or_octet_stream();
        (
            axum::http::StatusCode::OK,
            [("content-type", mime.as_ref())],
            file.contents(),
        )
            .into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, "404 Not Found").into_response()
    }
}

#[axum::debug_handler]
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

async fn websocket(stream: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = stream.split();
    let mut channel_name = String::new();
    let mut username = String::new();

    if let Some(Ok(Message::Text(text))) = receiver.next().await {
        if let Ok(msg) = serde_json::from_str::<ChatMessage>(&text) {
            channel_name = msg.channel;
            username = msg.username;
        } else {
            // Handle invalid initial message
            let _ = sender
                .send(Message::Text("Invalid join message".to_string()))
                .await;
            return;
        }
    }

    let channel = state
        .channels
        .entry(channel_name.clone())
        .or_default()
        .clone();

    let mut rx = channel.tx.subscribe();

    // Add user to channel
    channel.users.insert(username.clone(), ());

    // Send user join message
    if let Ok(join_msg) = serde_json::to_string(&ChatMessage {
        username: "System".to_string(),
        message: format!("{} joined {}", username, channel_name),
        channel: channel_name.clone(),
        message_type: Some("system".to_string()),
    }) {
        let _ = channel.tx.send(join_msg);
    }

    // Send current user list
    let users: Vec<String> = channel
        .users
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    if let Ok(user_list_msg) = serde_json::to_string(&ChatMessage {
        username: "System".to_string(),
        message: serde_json::to_string(&users).unwrap_or_default(),
        channel: channel_name.clone(),
        message_type: Some("user_list".to_string()),
    }) {
        let _ = channel.tx.send(user_list_msg);
    }

    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = {
        let channel = channel.clone();
        let username = username.clone();
        let channel_name = channel_name.clone();
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = receiver.next().await {
                // Try to parse the incoming message as JSON
                if let Ok(parsed_msg) = serde_json::from_str::<ChatMessage>(&text) {
                    // If it's a valid ChatMessage, use the parsed data
                    if let Ok(msg) = serde_json::to_string(&ChatMessage {
                        username: parsed_msg.username,
                        message: parsed_msg.message,
                        channel: parsed_msg.channel,
                        message_type: parsed_msg.message_type,
                    }) {
                        let _ = channel.tx.send(msg);
                    }
                } else {
                    // If it's not valid JSON, treat it as a raw message
                    if let Ok(msg) = serde_json::to_string(&ChatMessage {
                        username: username.clone(),
                        message: text,
                        channel: channel_name.clone(),
                        message_type: None,
                    }) {
                        let _ = channel.tx.send(msg);
                    }
                }
            }
        })
    };

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };

    // Remove user from channel
    channel.users.remove(&username);

    // Send user leave message
    if let Ok(leave_msg) = serde_json::to_string(&ChatMessage {
        username: "System".to_string(),
        message: format!("{} left {}", username, channel_name),
        channel: channel_name.clone(),
        message_type: Some("system".to_string()),
    }) {
        let _ = channel.tx.send(leave_msg);
    }

    // Send updated user list
    let users: Vec<String> = channel
        .users
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    if let Ok(user_list_msg) = serde_json::to_string(&ChatMessage {
        username: "System".to_string(),
        message: serde_json::to_string(&users).unwrap_or_default(),
        channel: channel_name.clone(),
        message_type: Some("user_list".to_string()),
    }) {
        let _ = channel.tx.send(user_list_msg);
    }
}
