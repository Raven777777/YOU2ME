use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::body::{Body, Bytes};
use axum::{
    extract::DefaultBodyLimit,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Query, Request, State,
    },
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use rand::{distr::Alphanumeric, Rng};
use rusqlite::{
    backup::Backup, params, params_from_iter, types::Value as SqlValue, Connection,
    OptionalExtension,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    env,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_rusqlite::Connection as DbConnection;

const INDEX_HTML: &str = include_str!("index.html");
const APP_JS: &str = include_str!("app.js");
const STYLE_CSS: &str = include_str!("style.css");
const SYSTEM_USERNAME: &str = "__y2m_system__";
const DEFAULT_ADMIN_USERNAME: &str = "井水玉藻";
const REGISTRATION_MODE_KEY: &str = "registration_mode";
const REGISTRATION_INVITE_KEY: &str = "registration_invite";
const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const MAX_SESSIONS: usize = 10_000;
const MAX_SESSIONS_PER_USER: usize = 20;
const LOGIN_WINDOW_SECS: i64 = 60;
const MAX_LOGIN_ATTEMPTS: usize = 5;
const MAX_LOGIN_ATTEMPTS_PER_IP: usize = 30;
const MAX_LOGIN_IPS: usize = 100_000;
const MAX_LOGIN_USERS: usize = 10_000;
const REGISTRATION_WINDOW_SECS: i64 = 60 * 60;
const MAX_REGISTRATION_ATTEMPTS: usize = 10;
const MAX_REGISTRATION_SOURCES: usize = 10_000;
const MAX_ROOMS_PER_USER: i64 = 100;
const MAX_MESSAGE_SEARCH_CHARS: usize = 128;
const MESSAGE_RETENTION_SECS: i64 = 0;

#[derive(Clone)]
struct AppState {
    db: DbConnection,
    restore_lock: Arc<tokio::sync::Mutex<()>>,
    login_attempts_by_ip: Arc<Mutex<HashMap<String, VecDeque<i64>>>>,
    database: String,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    login_attempts: Arc<Mutex<HashMap<String, VecDeque<i64>>>>,
    registration_attempts: Arc<Mutex<HashMap<String, VecDeque<i64>>>>,
    password_work: Arc<tokio::sync::Semaphore>,
    secure_cookie: bool,
    message_retention_secs: i64,
    max_messages_per_room: i64,
    debug: bool,
    dummy_hash: String,
    message_events: broadcast::Sender<MessageEvent>,
}

#[derive(Debug)]
struct StartupError(String);
impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StartupError {}

struct Session {
    user_id: i64,
    expires_at: i64,
}

fn insert_session(
    sessions: &mut HashMap<String, Session>,
    token: String,
    user_id: i64,
    current_time: i64,
) {
    sessions.retain(|_, session| session.expires_at > current_time);

    // 防止单个账号通过反复登录挤掉其他用户的会话。
    while sessions
        .values()
        .filter(|session| session.user_id == user_id)
        .count()
        >= MAX_SESSIONS_PER_USER
    {
        let Some(oldest_token) = sessions
            .iter()
            .filter(|(_, session)| session.user_id == user_id)
            .min_by_key(|(_, session)| session.expires_at)
            .map(|(token, _)| token.clone())
        else {
            break;
        };
        sessions.remove(&oldest_token);
    }

    if sessions.len() >= MAX_SESSIONS {
        if let Some(oldest_token) = sessions
            .iter()
            .min_by_key(|(_, session)| session.expires_at)
            .map(|(token, _)| token.clone())
        {
            sessions.remove(&oldest_token);
        }
    }

    sessions.insert(
        token,
        Session {
            user_id,
            expires_at: current_time + SESSION_TTL_SECS,
        },
    );
}

#[derive(Clone, Debug, Serialize)]
struct MessageEvent {
    id: i64,
    room_id: i64,
    username: String,
    text: String,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct WebSocketCommand {
    room: Option<i64>,
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
    password_confirm: Option<String>,
    invite_code: Option<String>,
}
#[derive(Deserialize)]
struct AdminSettingsRequest {
    registration_mode: String,
    invite_code: Option<String>,
}
#[derive(Deserialize)]
struct PasswordRequest {
    current_password: String,
    new_password: String,
    confirm_password: String,
}
#[derive(Deserialize)]
struct RoomRequest {
    name: Option<String>,
    code: Option<String>,
}
#[derive(Deserialize)]
struct MessageRequest {
    room: i64,
    text: String,
}

fn response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn is_constraint_violation(error: &tokio_rusqlite::Error) -> bool {
    matches!(
        error,
        tokio_rusqlite::Error::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn db_error<E: std::fmt::Display>(error: E) -> Response {
    eprintln!("database error: {error}");
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"数据库错误"}),
    )
}
fn server_error(message: &str) -> Response {
    eprintln!("server error: {message}");
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"服务器内部错误"}),
    )
}
fn debug(state: &AppState, message: impl std::fmt::Display) {
    if state.debug {
        println!("[DEBUG] {message}");
    }
}

fn user_id(state: &AppState, headers: &HeaderMap) -> Option<i64> {
    let token = session_token(headers)?;
    let mut sessions = state.sessions.lock().ok()?;
    let current_time = now();
    let session = sessions.get_mut(token)?;
    if session.expires_at <= current_time {
        sessions.remove(token);
        return None;
    }
    session.expires_at = current_time + SESSION_TTL_SECS;
    Some(session.user_id)
}
fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("y2m_session="))
}

fn websocket_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    ["http://", "https://"].iter().any(|scheme| {
        origin
            .strip_prefix(scheme)
            .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(host))
    })
}

fn same_origin_request(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // Non-browser clients do not send Origin. SameSite cookies still protect
        // browser requests that omit it, while any supplied Origin is validated.
        return true;
    };
    websocket_origin_allowed(headers) && !origin.is_empty()
}

fn browser_cross_site_request(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
}

async fn security_headers(request: Request, next: Next) -> Response {
    let is_state_change = matches!(
        request.method(),
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    );
    if is_state_change {
        // 现代浏览器会为跨站表单/请求发送 Sec-Fetch-Site: cross-site；
        // Origin 缺失时仅靠 SameSite Cookie 无法覆盖“登录 CSRF”等场景。
        if browser_cross_site_request(request.headers()) || !same_origin_request(request.headers())
        {
            return response(StatusCode::FORBIDDEN, json!({"error":"无效的请求来源"}));
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "same-origin".parse().unwrap());
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
            .parse()
            .unwrap(),
    );
    response
}
fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "y2m_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}{}",
        if secure { "; Secure" } else { "" }
    )
}
fn clear_session_cookie(secure: bool) -> String {
    format!(
        "y2m_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}
fn env_nonnegative_i64(name: &str, default: i64) -> Result<i64, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<i64>()
            .map_err(|_| format!("环境变量 {name} 必须是非负整数"))
            .and_then(|value| {
                if value >= 0 {
                    Ok(value)
                } else {
                    Err(format!("环境变量 {name} 必须是非负整数"))
                }
            }),
        Err(_) => Ok(default),
    }
}

fn env_positive_u16(name: &str, default: u16) -> Result<u16, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u16>()
            .map_err(|_| format!("环境变量 {name} 必须是 1-65535 的端口号"))
            .and_then(|value| {
                if value > 0 {
                    Ok(value)
                } else {
                    Err(format!("环境变量 {name} 必须是 1-65535 的端口号"))
                }
            }),
        Err(_) => Ok(default),
    }
}

fn env_bool(name: &str, default: bool) -> Result<bool, String> {
    match env::var(name) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "yes" | "true" | "1" => Ok(true),
            "no" | "false" | "0" => Ok(false),
            _ => Err(format!("环境变量 {name} 只能使用 yes/no")),
        },
        Err(_) => Ok(default),
    }
}

struct RuntimeConfig {
    port: u16,
    database: String,
    message_retention_secs: i64,
    max_messages_per_room: i64,
    secure_cookie: bool,
    debug: bool,
}

fn next_argument(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| format!("参数 {flag} 缺少值"))
}

fn runtime_config() -> Result<RuntimeConfig, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("用法: y2m [-p 8000] [-db ./y2m.sqlite3] [-ms 0] [-rm 0] [-cook yes|no] [-debug]");
        std::process::exit(0);
    }

    // 先解析命令行，确保命令行参数真正优先于环境变量：
    // 只有命令行未提供的字段才读取并校验环境变量。
    let mut port_override = None;
    let mut database_override = None;
    let mut message_retention_override = None;
    let mut max_messages_override = None;
    let mut secure_cookie_override = None;
    let mut debug = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-debug" | "--debug" => debug = true,
            "-p" => {
                let value = next_argument(&args, &mut index, "-p")?;
                port_override = Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port > 0)
                        .ok_or_else(|| "-p 必须是 1-65535 的端口号".to_string())?,
                );
            }
            "-db" => database_override = Some(next_argument(&args, &mut index, "-db")?),
            "-ms" => {
                let value = next_argument(&args, &mut index, "-ms")?;
                message_retention_override = Some(
                    value
                        .parse::<i64>()
                        .ok()
                        .filter(|seconds| *seconds >= 0)
                        .ok_or_else(|| "-ms 必须是非负整数秒数，0 表示永不删除".to_string())?,
                );
            }
            "-rm" => {
                let value = next_argument(&args, &mut index, "-rm")?;
                max_messages_override = Some(
                    value
                        .parse::<i64>()
                        .ok()
                        .filter(|count| *count >= 0)
                        .ok_or_else(|| "-rm 必须是非负整数条数，0 表示不限制".to_string())?,
                );
            }
            "-cook" => {
                let value = next_argument(&args, &mut index, "-cook")?;
                secure_cookie_override = Some(match value.to_ascii_lowercase().as_str() {
                    "yes" | "true" | "1" => true,
                    "no" | "false" | "0" => false,
                    _ => return Err("-cook 只能使用 yes/no".to_string()),
                });
            }
            unknown => return Err(format!("未知参数: {unknown}")),
        }
        index += 1;
    }

    let port = match port_override {
        Some(port) => port,
        None => env_positive_u16("PORT", 8000)?,
    };
    let database = match database_override {
        Some(database) => database,
        None => env::var("Y2M_DB").unwrap_or_else(|_| "y2m.sqlite3".into()),
    };
    let message_retention_secs = match message_retention_override {
        Some(seconds) => seconds,
        None => env_nonnegative_i64("Y2M_MESSAGE_RETENTION_SECS", MESSAGE_RETENTION_SECS)?,
    };
    let max_messages_per_room = match max_messages_override {
        Some(count) => count,
        None => env_nonnegative_i64("Y2M_MAX_MESSAGES_PER_ROOM", 0)?,
    };
    let secure_cookie = match secure_cookie_override {
        Some(secure_cookie) => secure_cookie,
        None => env_bool("Y2M_SECURE_COOKIE", false)?,
    };
    if database.trim().is_empty() {
        return Err("数据库路径不能为空".to_string());
    }
    Ok(RuntimeConfig {
        port,
        database,
        message_retention_secs,
        max_messages_per_room,
        secure_cookie,
        debug,
    })
}

fn login_allowed(state: &AppState, username: &str, source: &str) -> bool {
    let current_time = now();
    let mut attempts = match state.login_attempts.lock() {
        Ok(attempts) => attempts,
        Err(_) => return false,
    };
    if !attempt_allowed(
        &mut attempts,
        username,
        current_time,
        LOGIN_WINDOW_SECS,
        MAX_LOGIN_ATTEMPTS,
        MAX_LOGIN_USERS,
    ) {
        return false;
    }
    let mut attempts = match state.login_attempts_by_ip.lock() {
        Ok(attempts) => attempts,
        Err(_) => return false,
    };
    attempt_allowed(
        &mut attempts,
        source,
        current_time,
        LOGIN_WINDOW_SECS,
        MAX_LOGIN_ATTEMPTS_PER_IP,
        MAX_LOGIN_IPS,
    )
}
fn clear_login_attempts(state: &AppState, username: &str, source: &str) {
    if let Ok(mut attempts) = state.login_attempts.lock() {
        attempts.remove(username);
    }
    if let Ok(mut attempts) = state.login_attempts_by_ip.lock() {
        attempts.remove(source);
    }
}
fn attempt_allowed(
    attempts: &mut HashMap<String, VecDeque<i64>>,
    key: &str,
    current_time: i64,
    window_secs: i64,
    max_attempts: usize,
    max_keys: usize,
) -> bool {
    attempts.retain(|_, entries| {
        while entries
            .front()
            .is_some_and(|time| *time <= current_time - window_secs)
        {
            entries.pop_front();
        }
        !entries.is_empty()
    });
    if !attempts.contains_key(key) && attempts.len() >= max_keys {
        return false;
    }
    let entries = attempts.entry(key.to_string()).or_default();
    if entries.len() >= max_attempts {
        return false;
    }
    entries.push_back(current_time);
    true
}
fn registration_allowed(state: &AppState, source: &str) -> bool {
    let mut attempts = match state.registration_attempts.lock() {
        Ok(attempts) => attempts,
        Err(_) => return false,
    };
    attempt_allowed(
        &mut attempts,
        source,
        now(),
        REGISTRATION_WINDOW_SECS,
        MAX_REGISTRATION_ATTEMPTS,
        MAX_REGISTRATION_SOURCES,
    )
}
fn escape_like(pattern: &str) -> String {
    pattern
        .chars()
        .flat_map(|character| match character {
            '%' | '_' | '\\' => vec!['\\', character],
            _ => vec![character],
        })
        .collect()
}
async fn hash_password(state: &AppState, password: String) -> Result<String, ()> {
    let permit = state
        .password_work
        .clone()
        .try_acquire_owned()
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| ())
    })
    .await
    .map_err(|_| ())?
}
async fn verify_password(state: &AppState, password: String, hash: String) -> Result<bool, ()> {
    let permit = state
        .password_work
        .clone()
        .try_acquire_owned()
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Ok(parsed) = PasswordHash::new(&hash) else {
            return Ok(false);
        };
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    })
    .await
    .map_err(|_| ())?
}
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}
fn clean(value: &str, max: usize) -> String {
    value.trim().chars().take(max).collect()
}

fn random_alphanumeric(length: usize) -> String {
    (&mut rand::rng())
        .sample_iter(Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn valid_username(username: &str) -> bool {
    !username.is_empty() && !username.chars().any(char::is_control)
}

fn add_column_if_missing(connection: &Connection, statement: &str) -> rusqlite::Result<()> {
    match connection.execute(statement, []) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("duplicate column name") => Ok(()),
        Err(error) => Err(error),
    }
}

fn init_db(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA journal_mode = WAL;
        CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, created_at INTEGER NOT NULL, is_admin INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS server_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS rooms (id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE, name TEXT NOT NULL, owner_id INTEGER NOT NULL REFERENCES users(id), is_system INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS room_members (room_id INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, UNIQUE(room_id,user_id));
        CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, room_id INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE, user_id INTEGER NOT NULL REFERENCES users(id), text TEXT NOT NULL, created_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS messages_room_id ON messages(room_id, id);
         CREATE INDEX IF NOT EXISTS messages_created_at ON messages(created_at);")?;
    add_column_if_missing(
        connection,
        "ALTER TABLE rooms ADD COLUMN is_system INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0",
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO server_settings(key,value) VALUES(?1,'open')",
        params![REGISTRATION_MODE_KEY],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO server_settings(key,value) VALUES(?1,'')",
        params![REGISTRATION_INVITE_KEY],
    )?;
    ensure_lobby(connection)?;
    Ok(())
}

async fn ensure_admin(state: &AppState) -> Result<Option<String>, String> {
    let has_admin = state
        .db
        .call(|db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE is_admin=1)",
                [],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await
        .map_err(|error| error.to_string())?;
    if has_admin {
        return Ok(None);
    }
    let password = random_alphanumeric(24);
    let hash = hash_password(state, password.clone())
        .await
        .map_err(|_| "无法生成默认管理员密码哈希".to_string())?;
    state
        .db
        .call(move |db| {
            let username_exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE username=?1)",
                params![DEFAULT_ADMIN_USERNAME],
                |row| row.get(0),
            )?;
            if username_exists {
                return Err(tokio_rusqlite::Error::Other(Box::new(StartupError(
                    "默认管理员用户名已被占用，请先处理该账号".to_string(),
                ))));
            }
            db.execute(
                "INSERT INTO users(username,password_hash,created_at,is_admin) VALUES(?1,?2,?3,1)
                 ",
                params![DEFAULT_ADMIN_USERNAME, hash, now()],
            )?;
            Ok(true)
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some(password))
}

fn ensure_lobby(db: &Connection) -> rusqlite::Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO users(username,password_hash,created_at) VALUES(?1,'!system-account-disabled!',?2)",
        params![SYSTEM_USERNAME, now()],
    )?;
    let owner_id: i64 = db.query_row(
        "SELECT id FROM users WHERE username=?1",
        params![SYSTEM_USERNAME],
        |row| row.get(0),
    )?;
    db.execute(
        "INSERT OR IGNORE INTO rooms(code,name,owner_id,is_system,created_at) VALUES('LOBBY','公共大厅',?1,1,?2)",
        params![owner_id, now()],
    )?;
    db.execute(
        "UPDATE rooms SET owner_id=?1,is_system=1,name='公共大厅' WHERE code='LOBBY'",
        params![owner_id],
    )?;
    Ok(())
}

async fn register(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<Credentials>,
) -> Response {
    if !registration_allowed(&state, &peer.ip().to_string()) {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":"注册尝试过于频繁，请稍后重试"}),
        );
    }
    let username = input.username.trim().to_string();
    let length = username.chars().count();
    if !(2..=24).contains(&length) || !valid_username(&username) {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"用户名长度必须为 2-24 个字符"}),
        );
    }
    debug(&state, format!("register username={username}"));
    if username == SYSTEM_USERNAME {
        return response(StatusCode::CONFLICT, json!({"error":"该用户名不可用"}));
    }
    let password_confirm = input.password_confirm.as_deref().unwrap_or("");
    if input.password.chars().count() < 6 || input.password.chars().count() > 128 {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"密码长度必须为 6-128 个字符"}),
        );
    }
    if input.password != password_confirm {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"两次输入的密码不一致"}),
        );
    }
    let invite_code = input
        .invite_code
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let registration = state
        .db
        .call(|db| {
            let mode: String = db.query_row(
                "SELECT value FROM server_settings WHERE key=?1",
                params![REGISTRATION_MODE_KEY],
                |row| row.get(0),
            )?;
            let configured_invite: String = db.query_row(
                "SELECT value FROM server_settings WHERE key=?1",
                params![REGISTRATION_INVITE_KEY],
                |row| row.get(0),
            )?;
            Ok((mode, configured_invite))
        })
        .await;
    let (registration_mode, configured_invite) = match registration {
        Ok(value) => value,
        Err(error) => return db_error(error),
    };
    match registration_mode.as_str() {
        "open" => {}
        "invite"
            if !configured_invite.is_empty()
                && constant_time_eq(&invite_code, &configured_invite) => {}
        "invite" => {
            return response(StatusCode::FORBIDDEN, json!({"error":"请输入有效的邀请码"}));
        }
        _ => return server_error("invalid registration mode"),
    }
    let hash = match hash_password(&state, input.password).await {
        Ok(hash) => hash,
        Err(_) => {
            return response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"服务器繁忙，请稍后重试"}),
            )
        }
    };
    match state
        .db
        .call(move |db| {
            Ok(db.execute(
                "INSERT INTO users(username,password_hash,created_at) VALUES(?1,?2,?3)",
                params![username, hash, now()],
            )?)
        })
        .await
    {
        Ok(_) => response(StatusCode::OK, json!({"ok":true})),
        Err(error) if is_constraint_violation(&error) => {
            response(StatusCode::CONFLICT, json!({"error":"用户名已存在"}))
        }
        Err(error) => db_error(error),
    }
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<Credentials>,
) -> Response {
    let login_name = clean(&input.username, 24);
    let source = peer.ip().to_string();
    if !login_allowed(&state, &login_name, &source) {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":"登录尝试过于频繁，请稍后重试"}),
        );
    }
    debug(&state, format!("login username={login_name}"));
    let row = state
        .db
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT id,username,password_hash,is_admin FROM users WHERE username=?1",
                    params![login_name],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)? == 1,
                        ))
                    },
                )
                .optional()?)
        })
        .await;
    let (id, username, hash, is_admin) = match row {
        Ok(Some(row)) => row,
        Ok(None) => {
            let _ = verify_password(&state, input.password, state.dummy_hash.clone()).await;
            return response(
                StatusCode::UNAUTHORIZED,
                json!({"error":"用户名或密码错误"}),
            );
        }
        Err(error) => return db_error(error),
    };
    let valid = match verify_password(&state, input.password, hash).await {
        Ok(valid) => valid,
        Err(_) => {
            return response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"服务器繁忙，请稍后重试"}),
            )
        }
    };
    if !valid {
        return response(
            StatusCode::UNAUTHORIZED,
            json!({"error":"用户名或密码错误"}),
        );
    }
    clear_login_attempts(&state, &username, &source);
    let token = random_alphanumeric(48);
    let mut sessions = match state.sessions.lock() {
        Ok(sessions) => sessions,
        Err(_) => return server_error("session lock poisoned"),
    };
    insert_session(&mut sessions, token.clone(), id, now());
    debug(
        &state,
        format!("session created user_id={id} active={}", sessions.len()),
    );
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            session_cookie(&token, state.secure_cookie),
        )],
        Json(json!({"user":{"id":id,"username":username,"is_admin":is_admin}})),
    )
        .into_response()
}

async fn current_user(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    let row = state
        .db
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT username,is_admin FROM users WHERE id=?1",
                    params![uid],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? == 1)),
                )
                .optional()?)
        })
        .await;
    match row {
        Ok(Some((username, is_admin))) => response(
            StatusCode::OK,
            json!({"user":{"id":uid,"username":username,"is_admin":is_admin}}),
        ),
        Ok(None) => response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"})),
        Err(error) => db_error(error),
    }
}

async fn admin_user_id(state: &AppState, headers: &HeaderMap) -> Result<i64, Response> {
    let Some(uid) = user_id(state, headers) else {
        return Err(response(
            StatusCode::UNAUTHORIZED,
            json!({"error":"请先登录"}),
        ));
    };
    let is_admin = state
        .db
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT is_admin FROM users WHERE id=?1",
                    params![uid],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?)
        })
        .await;
    match is_admin {
        Ok(Some(1)) => Ok(uid),
        Ok(Some(_)) => Err(response(
            StatusCode::FORBIDDEN,
            json!({"error":"需要管理员权限"}),
        )),
        Ok(None) => Err(response(
            StatusCode::UNAUTHORIZED,
            json!({"error":"请先登录"}),
        )),
        Err(error) => Err(db_error(error)),
    }
}

async fn admin_settings(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = admin_user_id(&state, &headers).await {
        return error;
    }
    let settings = state
        .db
        .call(|db| {
            let mode: String = db.query_row(
                "SELECT value FROM server_settings WHERE key=?1",
                params![REGISTRATION_MODE_KEY],
                |row| row.get(0),
            )?;
            let invite_code: String = db.query_row(
                "SELECT value FROM server_settings WHERE key=?1",
                params![REGISTRATION_INVITE_KEY],
                |row| row.get(0),
            )?;
            Ok((mode, invite_code))
        })
        .await;
    match settings {
        Ok((registration_mode, invite_code)) => response(
            StatusCode::OK,
            json!({"registration_mode":registration_mode,"invite_code":invite_code}),
        ),
        Err(error) => db_error(error),
    }
}

async fn update_admin_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AdminSettingsRequest>,
) -> Response {
    if let Err(error) = admin_user_id(&state, &headers).await {
        return error;
    }
    if input.registration_mode != "open" && input.registration_mode != "invite" {
        return response(StatusCode::BAD_REQUEST, json!({"error":"注册模式无效"}));
    }
    let invite_code = clean(input.invite_code.as_deref().unwrap_or(""), 128);
    if input.registration_mode == "invite" && invite_code.is_empty() {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"邀请码模式必须设置邀请码"}),
        );
    }
    let mode = input.registration_mode;
    let result = state
        .db
        .call(move |db| {
            let transaction = db.transaction()?;
            transaction.execute(
                "INSERT INTO server_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![REGISTRATION_MODE_KEY, mode],
            )?;
            transaction.execute(
                "INSERT INTO server_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![REGISTRATION_INVITE_KEY, invite_code],
            )?;
            Ok(transaction.commit()?)
        })
        .await;
    match result {
        Ok(_) => response(StatusCode::OK, json!({"ok":true})),
        Err(error) => db_error(error),
    }
}

fn apply_pending_restore(database: &str) {
    use std::path::Path;
    let staging = format!("{database}.restore");
    if !Path::new(&staging).exists() {
        return;
    }
    let old = format!("{database}.old");
    let _ = std::fs::remove_file(&old);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{old}{suffix}"));
    }

    // 先把原库移开（重试等待上一个进程释放文件句柄）
    let moved_aside = if Path::new(database).exists() {
        let mut moved = false;
        for _ in 0..50 {
            if std::fs::rename(database, &old).is_ok() {
                moved = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        moved
    } else {
        true
    };
    if !moved_aside {
        let _ = std::fs::remove_file(&staging);
        eprintln!("[restore] 原数据库文件被占用，取消恢复");
        return;
    }

    // 同步移走 WAL/SHM：成功恢复时清理；失败回滚时尽量恢复原库的未 checkpoint 数据。
    for suffix in ["-wal", "-shm"] {
        let source = format!("{database}{suffix}");
        let target = format!("{old}{suffix}");
        if !Path::new(&source).exists() {
            continue;
        }
        if let Err(error) = std::fs::rename(&source, &target) {
            eprintln!("[restore] 移动 {source} 失败: {error}");
            // 宁可丢弃原库 WAL，也不能让它污染即将恢复的新库。
            let _ = std::fs::remove_file(&source);
        }
    }

    match std::fs::rename(&staging, database) {
        Ok(_) => {
            let _ = std::fs::remove_file(&old);
            for suffix in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{old}{suffix}"));
            }
            println!("[restore] 已应用数据库备份");
        }
        Err(error) => {
            eprintln!("[restore] 应用备份失败: {error}，正在回退原数据库");
            if Path::new(&old).exists() {
                if let Err(error) = std::fs::rename(&old, database) {
                    eprintln!("[restore] 回退原数据库失败: {error}");
                }
            }
            for suffix in ["-wal", "-shm"] {
                let source = format!("{old}{suffix}");
                let target = format!("{database}{suffix}");
                if Path::new(&source).exists() {
                    if let Err(error) = std::fs::rename(&source, &target) {
                        eprintln!("[restore] 回退 {source} 失败: {error}");
                    }
                }
            }
            let _ = std::fs::remove_file(&staging);
        }
    }
}

async fn backup_database(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = admin_user_id(&state, &headers).await {
        return error;
    }
    let stamp = now();
    let nonce = random_alphanumeric(32);
    let temp_dir = std::env::temp_dir();
    if let Err(error) = std::fs::create_dir_all(&temp_dir) {
        eprintln!("backup temp directory error: {error}");
        return server_error("创建备份临时目录失败");
    }
    let temp = temp_dir.join(format!("y2m-backup-{stamp}-{nonce}.sqlite3"));
    let temp_for_closure = temp.clone();
    let result = state
        .db
        .call(move |db| {
            let mut dest = Connection::open(&temp_for_closure)?;
            let backup = Backup::new(db, &mut dest)?;
            backup.run_to_completion(64, std::time::Duration::from_millis(5), None)?;
            Ok(())
        })
        .await;
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return db_error(error);
    }
    let bytes = match tokio::fs::read(&temp).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            eprintln!("backup read error: {error}");
            return server_error("读取备份失败");
        }
    };
    let _ = std::fs::remove_file(&temp);
    let filename = format!("y2m-backup-{stamp}.sqlite3");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(bytes))
        .unwrap_or_else(|_| server_error("构建备份响应失败"))
}

fn restore_database_is_compatible(connection: &Connection) -> rusqlite::Result<bool> {
    let required_tables = [
        (
            "users",
            &["id", "username", "password_hash", "created_at"][..],
        ),
        ("server_settings", &["key", "value"][..]),
        (
            "rooms",
            &["id", "code", "name", "owner_id", "created_at"][..],
        ),
        ("room_members", &["room_id", "user_id"][..]),
        (
            "messages",
            &["id", "room_id", "user_id", "text", "created_at"][..],
        ),
    ];

    for (table, required_columns) in required_tables {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            params![table],
            |row| row.get(0),
        )?;
        if !exists {
            // init_db 会在启动时为缺失的表创建兼容结构。
            continue;
        }
        let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
        let columns = statement
            .query_map(params![table], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if required_columns
            .iter()
            .any(|name| !columns.iter().any(|column| column == name))
        {
            return Ok(false);
        }
        if table == "users" {
            let has_admin_column = columns.iter().any(|column| column == "is_admin");
            let has_admin: bool = connection.query_row(
                if has_admin_column {
                    "SELECT EXISTS(SELECT 1 FROM users WHERE is_admin=1)"
                } else {
                    "SELECT 0"
                },
                [],
                |row| row.get(0),
            )?;
            // 若备份里没有管理员且默认管理员用户名已被普通账号占用，
            // 启动时的 ensure_admin 会失败并让服务无法启动。
            if !has_admin {
                let default_username_exists: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM users WHERE username=?1)",
                    params![DEFAULT_ADMIN_USERNAME],
                    |row| row.get(0),
                )?;
                if default_username_exists {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

async fn restore_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    if let Err(error) = admin_user_id(&state, &headers).await {
        return error;
    }
    if bytes.len() < 16 || &bytes[..16] != b"SQLite format 3\x00" {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"无效的 SQLite 备份文件"}),
        );
    }
    // 串行化恢复流程，避免两个并发请求写坏同一个 staging 文件。
    let restore_guard = state.restore_lock.clone().lock_owned().await;
    let staging = format!("{}.restore", state.database);
    if let Err(error) = tokio::fs::write(&staging, &bytes).await {
        eprintln!("restore write error: {error}");
        return server_error("写入备份文件失败");
    }
    let staging_for_validation = staging.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let connection = Connection::open_with_flags(
            &staging_for_validation,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Ok(false);
        }
        restore_database_is_compatible(&connection)
    })
    .await;
    match valid {
        Ok(Ok(true)) => {}
        _ => {
            let _ = tokio::fs::remove_file(&staging).await;
            return response(
                StatusCode::BAD_REQUEST,
                json!({"error":"备份文件无效或结构不兼容"}),
            );
        }
    }
    let exe = std::env::current_exe().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // 持有锁直到进程退出，防止后续请求再次覆盖待恢复文件。
        drop(restore_guard);
        spawn_restart(exe, args);
        std::process::exit(0);
    });
    response(StatusCode::OK, json!({"ok":true,"restarting":true}))
}

fn spawn_restart(exe: Option<std::path::PathBuf>, args: Vec<String>) {
    let Some(exe) = exe else {
        eprintln!("[restart] 无法获取可执行文件路径，请手动重启");
        return;
    };
    #[cfg(unix)]
    let result = {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(&exe)
            .args(&args)
            .process_group(0)
            .spawn()
    };
    #[cfg(windows)]
    let result = std::process::Command::new(&exe).args(&args).spawn();
    match result {
        Ok(_) => println!("[restart] 服务已重启"),
        Err(error) => eprintln!("[restart] 重启失败: {error}，请手动启动"),
    }
}

async fn list_rooms(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    debug(&state, format!("roomlist user_id={uid}"));
    let rows = state.db.call(move |db| {
        let mut statement = db.prepare("SELECT r.id,r.code,r.name,owner.username,r.is_system,CASE WHEN member.user_id IS NULL THEN 0 ELSE 1 END FROM rooms r JOIN users owner ON owner.id=r.owner_id LEFT JOIN room_members member ON member.room_id=r.id AND member.user_id=?1 ORDER BY r.is_system DESC,r.id DESC")?;
        let rooms = statement
            .query_map(params![uid], |row| {
                Ok(json!({"id":row.get::<_,i64>(0)?,"code":row.get::<_,String>(1)?,"name":row.get::<_,String>(2)?,"owner":row.get::<_,String>(3)?,"system":row.get::<_,i64>(4)? == 1,"joined":row.get::<_,i64>(5)? == 1}))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rooms)
    }).await;
    match rows {
        Ok(rooms) => response(StatusCode::OK, json!({"rooms":rooms})),
        Err(error) => db_error(error),
    }
}

fn room_code(db: &Connection) -> rusqlite::Result<String> {
    loop {
        let code = random_alphanumeric(6).to_uppercase();
        if db
            .query_row("SELECT id FROM rooms WHERE code=?1", params![code], |_| {
                Ok(())
            })
            .optional()?
            .is_none()
        {
            return Ok(code);
        }
    }
}

fn is_member(db: &Connection, room_id: i64, user_id: i64) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM room_members WHERE room_id=?1 AND user_id=?2)",
        params![room_id, user_id],
        |row| row.get(0),
    )
}

async fn create_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RoomRequest>,
) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    debug(&state, format!("create room user_id={uid}"));
    let raw_name = input.name.as_deref().unwrap_or("新聊天室");
    if raw_name.trim().chars().count() > 40 {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"聊天室名称最多 40 个字符"}),
        );
    }
    let name = clean(raw_name, 40);
    let name = if name.is_empty() {
        "新聊天室".into()
    } else {
        name
    };
    let result = state
        .db
        .call(move |db| {
            let room_count: i64 = db.query_row(
                "SELECT COUNT(*) FROM rooms WHERE owner_id=?1 AND is_system=0",
                params![uid],
                |row| row.get(0),
            )?;
            if room_count >= MAX_ROOMS_PER_USER {
                return Ok(None);
            }
            let code = room_code(db)?;
            let transaction = db.transaction()?;
            transaction.execute(
                "INSERT INTO rooms(code,name,owner_id,created_at) VALUES(?1,?2,?3,?4)",
                params![code, name, uid, now()],
            )?;
            let id = transaction.last_insert_rowid();
            transaction.execute(
                "INSERT INTO room_members(room_id,user_id) VALUES(?1,?2)",
                params![id, uid],
            )?;
            transaction.commit()?;
            Ok(Some((id, code, name)))
        })
        .await;
    let Some((id, code, name)) = (match result {
        Ok(result) => result,
        Err(error) => return db_error(error),
    }) else {
        return response(
            StatusCode::CONFLICT,
            json!({"error":"创建的聊天室数量已达上限"}),
        );
    };
    response(StatusCode::OK, json!({"id":id,"code":code,"name":name}))
}

async fn join_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RoomRequest>,
) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    let raw_code = input.code.as_deref().unwrap_or("").trim();
    if raw_code.chars().count() != 6 && !raw_code.eq_ignore_ascii_case("LOBBY") {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"房间码必须为 6 个字符"}),
        );
    }
    let code = raw_code.to_uppercase();
    debug(&state, format!("join room code={code} user_id={uid}"));
    let room = state
        .db
        .call(move |db| {
            let room = db
                .query_row(
                    "SELECT id,code,name FROM rooms WHERE code=?1",
                    params![code],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, code, name)) = room {
                db.execute(
                    "INSERT OR IGNORE INTO room_members(room_id,user_id) VALUES(?1,?2)",
                    params![id, uid],
                )?;
                Ok(Some((id, code, name)))
            } else {
                Ok(None)
            }
        })
        .await;
    let Some((id, code, name)) = (match room {
        Ok(room) => room,
        Err(error) => return db_error(error),
    }) else {
        return response(StatusCode::NOT_FOUND, json!({"error":"聊天室不存在"}));
    };
    response(StatusCode::OK, json!({"id":id,"code":code,"name":name}))
}

async fn delete_room(
    State(state): State<AppState>,
    Path(room_id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    debug(
        &state,
        format!("delete room room_id={room_id} user_id={uid}"),
    );
    let result = state
        .db
        .call(move |db| {
            let transaction = db.transaction()?;
            let room_info: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT owner_id,is_system FROM rooms WHERE id=?1",
                    params![room_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let outcome = match room_info {
                None => DeleteRoomOutcome::NotFound,
                Some((_, 1)) => DeleteRoomOutcome::SystemRoom,
                Some((owner, _)) if owner != uid => DeleteRoomOutcome::NotOwner,
                Some(_) => {
                    transaction.execute("DELETE FROM rooms WHERE id=?1", params![room_id])?;
                    transaction.commit()?;
                    DeleteRoomOutcome::Deleted
                }
            };
            Ok(outcome)
        })
        .await;
    match result {
        Ok(DeleteRoomOutcome::Deleted) => response(StatusCode::OK, json!({"ok":true})),
        Ok(DeleteRoomOutcome::NotFound) => {
            response(StatusCode::NOT_FOUND, json!({"error":"聊天室不存在"}))
        }
        Ok(DeleteRoomOutcome::SystemRoom) => {
            response(StatusCode::FORBIDDEN, json!({"error":"公共大厅不可删除"}))
        }
        Ok(DeleteRoomOutcome::NotOwner) => response(
            StatusCode::FORBIDDEN,
            json!({"error":"只有房主可以删除聊天室"}),
        ),
        Err(error) => db_error(error),
    }
}

enum DeleteRoomOutcome {
    Deleted,
    NotFound,
    SystemRoom,
    NotOwner,
}

async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PasswordRequest>,
) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    if input.new_password.chars().count() < 6 || input.new_password.chars().count() > 128 {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"新密码长度必须为 6-128 个字符"}),
        );
    }
    if input.new_password != input.confirm_password {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"两次输入的新密码不一致"}),
        );
    }
    let hash_result = state
        .db
        .call(move |db| {
            Ok(db.query_row(
                "SELECT password_hash FROM users WHERE id=?1",
                params![uid],
                |row| row.get(0),
            )?)
        })
        .await;
    let hash: String = match hash_result {
        Ok(hash) => hash,
        Err(error) => return db_error(error),
    };
    let valid = match verify_password(&state, input.current_password, hash).await {
        Ok(valid) => valid,
        Err(_) => {
            return response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"服务器繁忙，请稍后重试"}),
            )
        }
    };
    if !valid {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"当前密码错误"}));
    }
    let new_hash = match hash_password(&state, input.new_password).await {
        Ok(hash) => hash,
        Err(_) => {
            return response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"服务器繁忙，请稍后重试"}),
            )
        }
    };
    match state
        .db
        .call(move |db| {
            Ok(db.execute(
                "UPDATE users SET password_hash=?1 WHERE id=?2",
                params![new_hash, uid],
            )?)
        })
        .await
    {
        Ok(_) => {
            let mut sessions = match state.sessions.lock() {
                Ok(sessions) => sessions,
                Err(_) => return server_error("session lock poisoned"),
            };
            sessions.retain(|_, session| session.user_id != uid);
            response(StatusCode::OK, json!({"ok":true}))
        }
        Err(error) => db_error(error),
    }
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MessageQuery>,
) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 100) as usize;
    if query
        .q
        .as_deref()
        .is_some_and(|search| search.trim().chars().count() > MAX_MESSAGE_SEARCH_CHARS)
    {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"搜索关键词最多 128 个字符"}),
        );
    }
    let mut sql="SELECT m.id,u.username,m.text,m.created_at FROM messages m JOIN users u ON u.id=m.user_id WHERE m.room_id = ?".to_string();
    let mut values = vec![SqlValue::Integer(query.room)];
    if state.message_retention_secs > 0 {
        sql.push_str(" AND m.created_at >= ?");
        values.push(SqlValue::Integer(
            now().saturating_sub(state.message_retention_secs),
        ));
    }
    if let Some(after) = query.after {
        sql.push_str(" AND m.id > ?");
        values.push(SqlValue::Integer(after));
    } else if let Some(before) = query.before {
        sql.push_str(" AND m.id < ?");
        values.push(SqlValue::Integer(before))
    }
    if let Some(search) = query.q.as_deref().filter(|q| !q.trim().is_empty()) {
        sql.push_str(" AND m.text LIKE ? ESCAPE '\\'");
        values.push(SqlValue::Text(format!("%{}%", escape_like(search.trim()))))
    }
    if let (Some(from), Some(to)) = (query.from, query.to) {
        if from < to {
            sql.push_str(" AND m.created_at >= ? AND m.created_at < ?");
            values.push(SqlValue::Integer(from));
            values.push(SqlValue::Integer(to));
        }
    }
    sql.push_str(if query.after.is_some() {
        " ORDER BY m.id ASC LIMIT ?"
    } else {
        " ORDER BY m.id DESC LIMIT ?"
    });
    values.push(SqlValue::Integer((limit + 1) as i64));
    let rows = state.db.call(move |db| {
        if !is_member(db, query.room, uid)? {
            return Ok(None);
        }
        let mut statement = db.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| Ok(json!({"id":row.get::<_,i64>(0)?,"username":row.get::<_,String>(1)?,"text":row.get::<_,String>(2)?,"created_at":row.get::<_,i64>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
        Ok(Some(rows))
    }).await;
    match rows {
        Ok(Some(mut items)) => {
            let has_more = items.len() > limit;
            if has_more {
                items.truncate(limit)
            }
            if query.after.is_none() {
                items.reverse();
            }
            response(
                StatusCode::OK,
                json!({"messages":items,"has_more":has_more}),
            )
        }
        Ok(None) => response(StatusCode::FORBIDDEN, json!({"error":"你不是该聊天室成员"})),
        Err(error) => db_error(error),
    }
}

#[derive(Deserialize)]
struct MessageQuery {
    room: i64,
    before: Option<i64>,
    after: Option<i64>,
    limit: Option<i64>,
    q: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
}
async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<MessageRequest>,
) -> Response {
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    debug(
        &state,
        format!(
            "message room_id={} user_id={} length={}",
            input.room,
            uid,
            input.text.chars().count()
        ),
    );
    if input.text.trim().chars().count() > 2000 {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"消息最多 2000 个字符"}),
        );
    }
    let text = clean(&input.text, 2000);
    if text.is_empty() {
        return response(StatusCode::BAD_REQUEST, json!({"error":"消息不能为空"}));
    };
    let retention_secs = state.message_retention_secs;
    let max_messages_per_room = state.max_messages_per_room;
    let expiration = now().saturating_sub(retention_secs);
    let result = state
        .db
        .call(move |db| {
            let transaction = db.transaction()?;
            if !is_member(&transaction, input.room, uid)? {
                return Ok(SendMessageOutcome::NotMember);
            }
            if retention_secs > 0 {
                transaction.execute(
                    "DELETE FROM messages WHERE room_id=?1 AND created_at < ?2",
                    params![input.room, expiration],
                )?;
            }
            if max_messages_per_room > 0 {
                let room_full: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE room_id=?1 LIMIT 1 OFFSET ?2)",
                    params![input.room, max_messages_per_room - 1],
                    |row| row.get(0),
                )?;
                if room_full {
                    return Ok(SendMessageOutcome::RoomFull);
                }
            }
            transaction.execute(
                "INSERT INTO messages(room_id,user_id,text,created_at) VALUES(?1,?2,?3,?4)",
                params![input.room, uid, text, now()],
            )?;
            let id = transaction.last_insert_rowid();
            let event = transaction.query_row(
                "SELECT m.id,m.room_id,u.username,m.text,m.created_at FROM messages m JOIN users u ON u.id=m.user_id WHERE m.id=?1",
                params![id],
                |row| Ok(MessageEvent {
                    id: row.get(0)?,
                    room_id: row.get(1)?,
                    username: row.get(2)?,
                    text: row.get(3)?,
                    created_at: row.get(4)?,
                }),
            )?;
            transaction.commit()?;
            Ok(SendMessageOutcome::Sent(event))
        })
        .await;
    match result {
        Ok(SendMessageOutcome::Sent(event)) => {
            let id = event.id;
            let _ = state.message_events.send(event);
            response(StatusCode::OK, json!({"ok":true,"id":id}))
        }
        Ok(SendMessageOutcome::RoomFull) => response(
            StatusCode::CONFLICT,
            json!({"error":"该聊天室消息数量已达上限"}),
        ),
        Ok(SendMessageOutcome::NotMember) => {
            response(StatusCode::FORBIDDEN, json!({"error":"你不是该聊天室成员"}))
        }
        Err(error) => db_error(error),
    }
}

async fn websocket(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !websocket_origin_allowed(&headers) {
        return response(StatusCode::FORBIDDEN, json!({"error":"无效的请求来源"}));
    }
    let Some(uid) = user_id(&state, &headers) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    let Some(token) = session_token(&headers).map(str::to_owned) else {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"请先登录"}));
    };
    upgrade
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| websocket_session(socket, state, uid, token))
        .into_response()
}

fn session_active(state: &AppState, token: &str, expected_uid: i64) -> bool {
    let Ok(mut sessions) = state.sessions.lock() else {
        return false;
    };
    let current_time = now();
    let Some(session) = sessions.get_mut(token) else {
        return false;
    };
    if session.expires_at <= current_time || session.user_id != expected_uid {
        sessions.remove(token);
        return false;
    }
    session.expires_at = current_time + SESSION_TTL_SECS;
    true
}

async fn websocket_session(socket: WebSocket, state: AppState, uid: i64, token: String) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = state.message_events.subscribe();
    let mut auth_check = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut room_id = None;

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else { break };
                if !session_active(&state, &token, uid) { break; }
                match message {
                    Message::Text(text) => {
                        let Ok(command) = serde_json::from_str::<WebSocketCommand>(&text) else {
                            continue;
                        };
                        let Some(room) = command.room.filter(|room| *room > 0) else {
                            continue;
                        };
                        let member = state.db.call(move |db| Ok(is_member(db, room, uid)?)).await;
                        if !matches!(member, Ok(true)) {
                            let _ = sender.send(Message::Text(json!({"type":"error","error":"你不是该聊天室成员"}).to_string().into())).await;
                            continue;
                        }
                        room_id = Some(room);
                        if sender.send(Message::Text(json!({"type":"subscribed","room":room}).to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        if sender.send(Message::Pong(payload)).await.is_err() { break; }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) if room_id == Some(event.room_id) => {
                        if sender.send(Message::Text(json!({"type":"message","message":event}).to_string().into())).await.is_err() { break; }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if sender.send(Message::Text(json!({"type":"sync_required"}).to_string().into())).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = auth_check.tick() => {
                if !session_active(&state, &token, uid) { break; }
            }
        }
    }
}

enum SendMessageOutcome {
    Sent(MessageEvent),
    RoomFull,
    NotMember,
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        if let Ok(mut sessions) = state.sessions.lock() {
            sessions.remove(token);
        }
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            clear_session_cookie(state.secure_cookie),
        )],
        Json(json!({"ok":true})),
    )
        .into_response()
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML))
}
async fn app_js() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        APP_JS,
    )
}
async fn style_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        STYLE_CSS,
    )
}

#[tokio::main]
async fn main() {
    let config = match runtime_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("参数错误: {error}");
            std::process::exit(2);
        }
    };
    apply_pending_restore(&config.database);
    let connection = DbConnection::open(config.database.clone())
        .await
        .expect("failed to open sqlite database");
    connection
        .call(|connection| Ok(init_db(connection)?))
        .await
        .expect("failed to initialize database");
    let mut state = AppState {
        db: connection,
        database: config.database.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        login_attempts: Arc::new(Mutex::new(HashMap::new())),
        login_attempts_by_ip: Arc::new(Mutex::new(HashMap::new())),
        registration_attempts: Arc::new(Mutex::new(HashMap::new())),
        restore_lock: Arc::new(tokio::sync::Mutex::new(())),
        password_work: Arc::new(tokio::sync::Semaphore::new(2)),
        secure_cookie: config.secure_cookie,
        message_retention_secs: config.message_retention_secs,
        max_messages_per_room: config.max_messages_per_room,
        debug: config.debug,
        dummy_hash: String::new(),
        message_events: broadcast::channel(1024).0,
    };
    state.dummy_hash = hash_password(&state, "y2m-timing-dummy-password".to_string())
        .await
        .expect("failed to initialize login timing protection");
    let initial_admin_password = ensure_admin(&state)
        .await
        .expect("failed to initialize administrator account");
    if let Some(password) = initial_admin_password {
        println!("[ADMIN] 初始管理员账号: {DEFAULT_ADMIN_USERNAME}");
        println!("[ADMIN] 初始管理员密码: {password}");
        println!("[ADMIN] 请登录后立即修改管理员密码");
    }
    let retention_secs = state.message_retention_secs;
    if retention_secs > 0 {
        let cleanup_db = state.db.clone();
        tokio::spawn(async move {
            // 清理周期跟随保留时长，但限制在 1 秒到 24 小时之间，
            // 避免极小周期空转或极大 Duration 触发计时器溢出。
            let cleanup_period_secs = retention_secs.clamp(1, 24 * 60 * 60);
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(cleanup_period_secs as u64));
            loop {
                interval.tick().await;
                let expiration = now().saturating_sub(retention_secs);
                // 分批删除直到本轮过期消息清理完毕，避免长事务阻塞请求。
                loop {
                    let deleted = cleanup_db
                        .call(move |db| {
                            Ok(db.execute(
                                "DELETE FROM messages WHERE id IN (SELECT id FROM messages WHERE created_at < ?1 LIMIT 1000)",
                                params![expiration],
                            )?)
                        })
                        .await;
                    match deleted {
                        Ok(count) if count >= 1000 => continue,
                        Ok(_) => break,
                        Err(error) => {
                            eprintln!("message cleanup error: {error}");
                            break;
                        }
                    }
                }
            }
        });
    }
    let cleanup_sessions = state.sessions.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
        loop {
            interval.tick().await;
            let current_time = now();
            if let Ok(mut sessions) = cleanup_sessions.lock() {
                sessions.retain(|_, session| session.expires_at > current_time);
            }
        }
    });
    if config.debug {
        println!("[DEBUG] debug logging enabled");
        println!("[DEBUG] database initialized");
    }
    let app = Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(current_user))
        .route(
            "/api/admin/settings",
            get(admin_settings).post(update_admin_settings),
        )
        .route("/api/admin/backup", get(backup_database))
        .route(
            "/api/admin/backup/restore",
            post(restore_backup).layer(DefaultBodyLimit::max(256 * 1024 * 1024)),
        )
        .route("/api/profile/password", post(change_password))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/rooms/join", post(join_room))
        .route("/api/rooms/{id}", delete(delete_room))
        .route("/api/messages", get(messages).post(send_message))
        .route("/api/ws", get(websocket))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);
    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .expect("failed to bind port");
    println!("Y2M Chat listening at http://localhost:{}", config.port);
    if config.debug {
        println!("[DEBUG] routes: auth, rooms, messages");
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server stopped")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_limit_expires_and_resets() {
        let mut attempts = HashMap::new();
        assert!(attempt_allowed(&mut attempts, "source", 100, 60, 2, 10));
        assert!(attempt_allowed(&mut attempts, "source", 101, 60, 2, 10));
        assert!(!attempt_allowed(&mut attempts, "source", 102, 60, 2, 10));
        assert!(attempt_allowed(&mut attempts, "source", 161, 60, 2, 10));
    }

    #[test]
    fn attempt_limit_bounds_source_map() {
        let mut attempts = HashMap::new();
        assert!(attempt_allowed(&mut attempts, "first", 100, 60, 2, 1));
        assert!(!attempt_allowed(&mut attempts, "second", 101, 60, 2, 1));
        assert!(attempt_allowed(&mut attempts, "second", 161, 60, 2, 1));
    }

    #[test]
    fn websocket_origin_must_match_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "example.test".parse().unwrap());
        headers.insert(header::ORIGIN, "https://example.test".parse().unwrap());
        assert!(websocket_origin_allowed(&headers));
        headers.insert(header::ORIGIN, "https://attacker.test".parse().unwrap());
        assert!(!websocket_origin_allowed(&headers));
    }

    #[test]
    fn state_change_origin_is_checked_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "example.test".parse().unwrap());
        assert!(same_origin_request(&headers));
        headers.insert(header::ORIGIN, "https://example.test".parse().unwrap());
        assert!(same_origin_request(&headers));
        headers.insert(header::ORIGIN, "https://attacker.test".parse().unwrap());
        assert!(!same_origin_request(&headers));
    }

    #[test]
    fn cross_site_fetch_header_is_detected() {
        let mut headers = HeaderMap::new();
        assert!(!browser_cross_site_request(&headers));
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(!browser_cross_site_request(&headers));
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(browser_cross_site_request(&headers));
    }

    #[test]
    fn usernames_cannot_contain_control_characters() {
        assert!(valid_username("井水玉藻"));
        assert!(!valid_username("user\nname"));
        assert!(!valid_username("user\0name"));
    }

    #[test]
    fn session_insertion_caps_sessions_per_user() {
        let mut sessions = HashMap::new();
        for index in 0..MAX_SESSIONS_PER_USER {
            insert_session(
                &mut sessions,
                format!("user1-{index}"),
                1,
                1_000 + index as i64,
            );
        }
        insert_session(&mut sessions, "user2".to_string(), 2, 1_000);
        for index in 0..5 {
            insert_session(
                &mut sessions,
                format!("user1-extra-{index}"),
                1,
                2_000 + index as i64,
            );
        }
        assert_eq!(
            sessions
                .values()
                .filter(|session| session.user_id == 1)
                .count(),
            MAX_SESSIONS_PER_USER
        );
        assert!(sessions.contains_key("user2"));
    }

    #[test]
    fn compatible_restore_schema_is_accepted() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT, password_hash TEXT, created_at INTEGER);
                 CREATE TABLE server_settings (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE rooms (id INTEGER PRIMARY KEY, code TEXT, name TEXT, owner_id INTEGER, created_at INTEGER);
                 CREATE TABLE room_members (room_id INTEGER, user_id INTEGER);
                 CREATE TABLE messages (id INTEGER PRIMARY KEY, room_id INTEGER, user_id INTEGER, text TEXT, created_at INTEGER);",
            )
            .unwrap();
        assert!(restore_database_is_compatible(&connection).unwrap());
    }

    #[test]
    fn incompatible_restore_schema_is_rejected() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        assert!(!restore_database_is_compatible(&connection).unwrap());
    }

    #[test]
    fn restore_rejects_default_admin_name_conflict_without_admin() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT, password_hash TEXT, created_at INTEGER, is_admin INTEGER DEFAULT 0);
                 INSERT INTO users(username,password_hash,created_at,is_admin) VALUES('井水玉藻','x',0,0);",
            )
            .unwrap();
        assert!(!restore_database_is_compatible(&connection).unwrap());
    }

    #[test]
    fn like_escape_protects_wildcards() {
        assert_eq!(escape_like(r"100%_a\b"), r"100\%\_a\\b");
    }

    #[test]
    fn random_tokens_have_requested_length() {
        assert_eq!(random_alphanumeric(24).chars().count(), 24);
        assert!(random_alphanumeric(32)
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric()));
    }
}
