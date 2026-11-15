//! Twitch: OAuth (authorization code + refresh), Helix API, чат через EventSub WebSocket.
//!
//! Офіційний сучасний шлях Twitch:
//!   * читання чату — EventSub `channel.chat.message` по WebSocket;
//!   * надсилання — Helix `POST /chat/messages`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{watch, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;

pub const AUTH_URL: &str = "https://id.twitch.tv/oauth2/authorize";
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
pub const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
pub const HELIX: &str = "https://api.twitch.tv/helix";
pub const EVENTSUB_WS: &str = "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30";
pub const SCOPES: &[&str] = &["user:read:chat", "user:write:chat", "user:bot"];

#[derive(Debug, Clone, Default)]
pub struct ChatMessage {
    pub message_id: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub text: String,
    pub badges: HashSet<String>,
}

impl ChatMessage {
    pub fn is_broadcaster(&self) -> bool {
        self.badges.contains("broadcaster")
    }
    pub fn is_mod(&self) -> bool {
        ["broadcaster", "moderator", "lead_moderator"].iter().any(|b| self.badges.contains(*b))
    }
    pub fn is_vip(&self) -> bool {
        self.badges.contains("vip")
    }
    pub fn is_sub(&self) -> bool {
        self.badges.contains("subscriber") || self.badges.contains("founder")
    }

    pub fn from_event(ev: &Value) -> Self {
        let mut text = ev["message"]["text"].as_str().unwrap_or("").to_string();
        // якщо це відповідь на повідомлення, Twitch додає на початок "@нік "
        if let Some(parent) = ev["reply"]["parent_user_login"].as_str() {
            let prefix = format!("@{parent}");
            if text.to_lowercase().starts_with(&prefix.to_lowercase()) {
                text = text[prefix.len()..].to_string();
            }
        }
        let login = ev["chatter_user_login"].as_str().unwrap_or("").to_string();
        Self {
            message_id: ev["message_id"].as_str().unwrap_or("").into(),
            user_id: ev["chatter_user_id"].as_str().unwrap_or("").into(),
            display_name: ev["chatter_user_name"].as_str().filter(|s| !s.is_empty()).unwrap_or(&login).into(),
            login,
            text: text.trim().into(),
            badges: ev["badges"].as_array().map(|a| a.iter().filter_map(|b| b["set_id"].as_str().map(String::from)).collect()).unwrap_or_default(),
        }
    }
}

#[derive(Debug)]
pub enum TwitchError {
    Auth(String),
    Net(String),
}

impl std::fmt::Display for TwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TwitchError::Auth(s) | TwitchError::Net(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for TwitchError {}

impl From<reqwest::Error> for TwitchError {
    fn from(e: reqwest::Error) -> Self {
        TwitchError::Net(e.to_string())
    }
}

#[derive(Default, Clone)]
struct Tokens {
    access: Option<String>,
    refresh: Option<String>,
    user_id: Option<String>,
    login: Option<String>,
}

/// Зберігає та оновлює токен користувача (акаунта бота).
pub struct TwitchAuth {
    pub client_id: String,
    client_secret: String,
    token_path: PathBuf,
    pub redirect_uri: String,
    tokens: std::sync::RwLock<Tokens>,
    state: String,
    refresh_lock: Mutex<()>,
    authorized: watch::Sender<bool>,
    http: reqwest::Client,
}

impl TwitchAuth {
    pub fn new(client_id: &str, client_secret: &str, token_path: PathBuf, redirect_uri: String, http: reqwest::Client) -> Self {
        let mut t = Tokens::default();
        if let Ok(s) = std::fs::read_to_string(&token_path) {
            match serde_json::from_str::<Value>(&s) {
                Ok(d) => {
                    t.access = d["access_token"].as_str().map(String::from);
                    t.refresh = d["refresh_token"].as_str().map(String::from);
                }
                Err(_) => tracing::warn!("Не вдалося прочитати {}", token_path.display()),
            }
        }
        let state: String = (0..22).map(|_| {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            A[rand::random_range(0..A.len())] as char
        }).collect();
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token_path,
            redirect_uri,
            tokens: std::sync::RwLock::new(t),
            state,
            refresh_lock: Mutex::new(()),
            authorized: watch::channel(false).0,
            http,
        }
    }

    pub fn is_authorized(&self) -> bool {
        *self.authorized.borrow()
    }
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.authorized.subscribe()
    }
    fn set_authorized(&self, v: bool) {
        self.authorized.send_replace(v);
    }
    pub fn login(&self) -> Option<String> {
        self.tokens.read().unwrap().login.clone()
    }
    pub fn user_id(&self) -> Option<String> {
        self.tokens.read().unwrap().user_id.clone()
    }
    pub fn access_token(&self) -> Option<String> {
        self.tokens.read().unwrap().access.clone()
    }
    pub fn has_refresh(&self) -> bool {
        self.tokens.read().unwrap().refresh.is_some()
    }

    fn save(&self) {
        let t = self.tokens.read().unwrap().clone();
        if let Some(d) = self.token_path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&self.token_path, json!({"access_token": t.access, "refresh_token": t.refresh}).to_string());
    }

    pub fn authorize_url(&self) -> String {
        let q = [
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", &SCOPES.join(" ")),
            ("state", self.state.as_str()),
            ("force_verify", "true"), // дозволяє обрати інший акаунт (бота)
        ];
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(v))).collect();
        format!("{AUTH_URL}?{}", qs.join("&"))
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<Value, TwitchError> {
        let r = self.http.post(TOKEN_URL).form(form).timeout(Duration::from_secs(20)).send().await?;
        let ok = r.status().is_success();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        if !ok {
            let msg = body["message"].as_str().map(String::from).unwrap_or_else(|| body.to_string());
            return Err(TwitchError::Auth(msg));
        }
        Ok(body)
    }

    pub async fn exchange_code(&self, code: &str, state: &str) -> Result<(), TwitchError> {
        if state != self.state {
            return Err(TwitchError::Auth("Невірний state — спробуйте авторизуватися ще раз".into()));
        }
        let body = self
            .token_request(&[
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("code", code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", &self.redirect_uri),
            ])
            .await
            .map_err(|e| TwitchError::Auth(format!("Twitch відмовив: {e}")))?;
        {
            let mut t = self.tokens.write().unwrap();
            t.access = body["access_token"].as_str().map(String::from);
            t.refresh = body["refresh_token"].as_str().map(String::from);
        }
        self.save();
        self.validate().await;
        Ok(())
    }

    pub async fn refresh(&self) -> Result<(), TwitchError> {
        let _g = self.refresh_lock.lock().await;
        let refresh = self.tokens.read().unwrap().refresh.clone();
        let Some(refresh) = refresh else {
            return Err(TwitchError::Auth("Немає refresh-токена — потрібна повторна авторизація".into()));
        };
        let body = match self
            .token_request(&[
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
            ])
            .await
        {
            Ok(b) => b,
            Err(e) => {
                self.set_authorized(false);
                return Err(TwitchError::Auth(format!("Не вдалося оновити токен: {e}")));
            }
        };
        {
            let mut t = self.tokens.write().unwrap();
            t.access = body["access_token"].as_str().map(String::from);
            if let Some(r) = body["refresh_token"].as_str() {
                t.refresh = Some(r.into());
            }
        }
        self.save();
        tracing::info!("Токен Twitch оновлено");
        Ok(())
    }

    /// Перевіряє токен (Twitch вимагає робити це щогодини). Оновлює за потреби.
    pub async fn validate(&self) -> bool {
        for attempt in 0..2 {
            let Some(tok) = self.access_token() else { break };
            let r = self.http.get(VALIDATE_URL).header("Authorization", format!("OAuth {tok}")).timeout(Duration::from_secs(15)).send().await;
            match r {
                Ok(r) if r.status().is_success() => {
                    let d: Value = r.json().await.unwrap_or(Value::Null);
                    let scopes: HashSet<&str> = d["scopes"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect()).unwrap_or_default();
                    let missing: Vec<&str> = SCOPES[..2].iter().copied().filter(|s| !scopes.contains(s)).collect();
                    if !missing.is_empty() {
                        tracing::error!("У токена бракує прав {missing:?} — авторизуйтесь ще раз");
                        return false;
                    }
                    {
                        let mut t = self.tokens.write().unwrap();
                        t.user_id = d["user_id"].as_str().map(String::from);
                        t.login = d["login"].as_str().map(String::from);
                    }
                    self.set_authorized(true);
                    return true;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("validate: {e}");
                    return self.is_authorized();
                }
            }
            if attempt == 0 && self.has_refresh() {
                match self.refresh().await {
                    Ok(()) => continue,
                    Err(e) => tracing::error!("{e}"),
                }
            }
            break;
        }
        self.set_authorized(false);
        false
    }
}

pub struct Helix {
    pub auth: Arc<TwitchAuth>,
    http: reqwest::Client,
}

impl Helix {
    pub fn new(auth: Arc<TwitchAuth>, http: reqwest::Client) -> Self {
        Self { auth, http }
    }

    pub async fn request(&self, method: reqwest::Method, path: &str, query: &[(&str, &str)], body: Option<&Value>) -> Result<(u16, Value), TwitchError> {
        for attempt in 0..3 {
            let tok = self.auth.access_token().unwrap_or_default();
            let mut rb = self
                .http
                .request(method.clone(), format!("{HELIX}{path}"))
                .query(query)
                .header("Authorization", format!("Bearer {tok}"))
                .header("Client-Id", &self.auth.client_id)
                .timeout(Duration::from_secs(20));
            if let Some(b) = body {
                rb = rb.json(b);
            }
            let r = rb.send().await?;
            let status = r.status().as_u16();
            if status == 401 && attempt == 0 {
                self.auth.refresh().await?;
                continue;
            }
            if status == 429 {
                let reset: f64 = r.headers().get("Ratelimit-Reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let wait = (reset - crate::track::now_secs()).max(0.5);
                tokio::time::sleep(Duration::from_secs_f64(wait.min(30.0))).await;
                continue;
            }
            let data: Value = r.json().await.unwrap_or(json!({}));
            return Ok((status, data));
        }
        Ok((0, json!({})))
    }

    pub async fn get_user_id(&self, login: &str) -> Result<Option<String>, TwitchError> {
        let (status, d) = self.request(reqwest::Method::GET, "/users", &[("login", login)], None).await?;
        Ok((status == 200).then(|| d["data"][0]["id"].as_str().map(String::from)).flatten())
    }

    pub async fn send_chat(&self, broadcaster_id: &str, text: &str, reply_to: Option<&str>) -> Result<bool, TwitchError> {
        let text: String = text.chars().take(500).collect();
        let mut body = json!({"broadcaster_id": broadcaster_id, "sender_id": self.auth.user_id(), "message": text});
        if let Some(r) = reply_to.filter(|r| !r.is_empty()) {
            body["reply_parent_message_id"] = json!(r);
        }
        let (status, d) = self.request(reqwest::Method::POST, "/chat/messages", &[], Some(&body)).await?;
        let ok = status == 200 && d["data"][0]["is_sent"].as_bool().unwrap_or(false);
        if !ok {
            tracing::warn!("Повідомлення не надіслано ({status}): {d}");
        }
        Ok(ok)
    }

    pub async fn subscribe_chat(&self, session_id: &str, broadcaster_id: &str) -> Result<(), TwitchError> {
        let body = json!({
            "type": "channel.chat.message",
            "version": "1",
            "condition": {"broadcaster_user_id": broadcaster_id, "user_id": self.auth.user_id()},
            "transport": {"method": "websocket", "session_id": session_id},
        });
        let (status, d) = self.request(reqwest::Method::POST, "/eventsub/subscriptions", &[], Some(&body)).await?;
        if status != 202 && status != 409 {
            let msg = d["message"].as_str().map(String::from).unwrap_or_else(|| d.to_string());
            return Err(TwitchError::Auth(format!("Не вдалося підписатися на чат ({status}): {msg}")));
        }
        Ok(())
    }
}

pub type Handler = Arc<dyn Fn(ChatMessage) + Send + Sync>;

/// Підключення до чату через EventSub WebSocket з автоматичним перепідключенням.
pub struct TwitchChat {
    pub auth: Arc<TwitchAuth>,
    pub helix: Helix,
    pub channel: String,
    broadcaster_id: RwLock<Option<String>>,
    connected: watch::Sender<bool>,
}

impl TwitchChat {
    pub fn new(auth: Arc<TwitchAuth>, http: reqwest::Client, channel: &str) -> Self {
        Self {
            helix: Helix::new(auth.clone(), http),
            auth,
            channel: channel.into(),
            broadcaster_id: RwLock::new(None),
            connected: watch::channel(false).0,
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }
    pub fn connected_rx(&self) -> watch::Receiver<bool> {
        self.connected.subscribe()
    }

    pub async fn say(&self, text: &str, reply_to: Option<&str>) {
        let Some(bid) = self.broadcaster_id.read().await.clone() else {
            tracing::info!("[чат офлайн] {text}");
            return;
        };
        if let Err(e) = self.helix.send_chat(&bid, text, reply_to).await {
            tracing::warn!("Помилка надсилання в чат: {e}");
        }
    }

    pub async fn run(self: Arc<Self>, handler: Handler) {
        let mut backoff = 1u64;
        let mut auth_rx = self.auth.subscribe();
        loop {
            while !*auth_rx.borrow_and_update() {
                if auth_rx.changed().await.is_err() {
                    return;
                }
            }
            let result: Result<(), TwitchError> = async {
                if self.broadcaster_id.read().await.is_none() {
                    match self.helix.get_user_id(&self.channel).await? {
                        Some(id) => *self.broadcaster_id.write().await = Some(id),
                        None => {
                            tracing::error!("Канал «{}» не знайдено на Twitch — перевірте config.toml", self.channel);
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            return Ok(());
                        }
                    }
                }
                let mut url = EVENTSUB_WS.to_string();
                let mut subscribe = true;
                while let Some(next) = self.session(&url, subscribe, &handler).await? {
                    // Twitch просить перепідключитися: підписки переносяться автоматично
                    url = next;
                    subscribe = false;
                }
                Ok(())
            }
            .await;
            match result {
                Ok(()) => backoff = 1,
                Err(TwitchError::Auth(e)) => {
                    tracing::error!("{e}");
                    self.auth.set_authorized(false);
                }
                Err(TwitchError::Net(e)) => {
                    tracing::warn!("Зв'язок з Twitch втрачено ({e}), перепідключаюсь через {backoff}с");
                }
            }
            self.connected.send_replace(false);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    }

    /// Одна сесія WebSocket. Some(url) — Twitch просить перейти на нову адресу.
    async fn session(&self, url: &str, subscribe: bool, handler: &Handler) -> Result<Option<String>, TwitchError> {
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.map_err(|e| TwitchError::Net(e.to_string()))?;
        let mut keepalive = 40.0f64;
        loop {
            let msg = tokio::time::timeout(Duration::from_secs_f64(keepalive + 10.0), ws.next())
                .await
                .map_err(|_| TwitchError::Net("немає відповіді від Twitch".into()))?;
            let msg = match msg {
                None => return Err(TwitchError::Net("WebSocket закрито".into())),
                Some(Err(e)) => return Err(TwitchError::Net(e.to_string())),
                Some(Ok(m)) => m,
            };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Ping(p) => {
                    let _ = ws.send(Message::Pong(p)).await;
                    continue;
                }
                Message::Close(c) => return Err(TwitchError::Net(format!("WebSocket закрито ({c:?})"))),
                _ => continue,
            };
            let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let payload = &data["payload"];
            match data["metadata"]["message_type"].as_str().unwrap_or("") {
                "session_welcome" => {
                    let sess = &payload["session"];
                    keepalive = sess["keepalive_timeout_seconds"].as_f64().unwrap_or(30.0);
                    if subscribe {
                        let bid = self.broadcaster_id.read().await.clone().unwrap_or_default();
                        self.helix.subscribe_chat(sess["id"].as_str().unwrap_or(""), &bid).await?;
                    }
                    self.connected.send_replace(true);
                    tracing::info!("Чат підключено: #{} (бот: {})", self.channel, self.auth.login().unwrap_or_default());
                }
                "notification" => {
                    if payload["subscription"]["type"] == "channel.chat.message" {
                        let cm = ChatMessage::from_event(&payload["event"]);
                        if Some(&cm.user_id) != self.auth.user_id().as_ref() {
                            handler(cm);
                        }
                    }
                }
                "session_reconnect" => {
                    tracing::info!("Twitch просить перепідключитися");
                    return Ok(payload["session"]["reconnect_url"].as_str().map(String::from));
                }
                "revocation" => {
                    return Err(TwitchError::Auth(format!(
                        "Twitch відкликав підписку: {}",
                        payload["subscription"]["status"].as_str().unwrap_or("")
                    )))
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event() {
        let ev = json!({
            "message_id": "m1", "chatter_user_id": "42", "chatter_user_login": "pipa", "chatter_user_name": "Pipa",
            "message": {"text": "@Bot !трек обійми"}, "reply": {"parent_user_login": "bot"},
            "badges": [{"set_id": "subscriber"}, {"set_id": "vip"}]
        });
        let m = ChatMessage::from_event(&ev);
        assert_eq!(m.text, "!трек обійми");
        assert!(m.is_sub() && m.is_vip() && !m.is_mod());
        assert_eq!(m.display_name, "Pipa");
    }

    #[test]
    fn auth_url() {
        let a = TwitchAuth::new("cid", "sec", std::env::temp_dir().join("nope.json"), "http://localhost:17777/auth/callback".into(), reqwest::Client::new());
        let u = a.authorize_url();
        assert!(u.starts_with(AUTH_URL) && u.contains("client_id=cid") && u.contains("user%3Aread%3Achat"));
    }
}
