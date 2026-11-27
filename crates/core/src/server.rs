//! Локальний веб-сервер: віджет для OBS, застосунок-панель, авторизація Twitch.
//!
//! API однакове з pipabobik v1, тож сторінки web/*.html працюють без змін.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::access::{default_limits, normalize_name, normalize_roles, valid_name, Access, COMMANDS, LIMIT_KEYS, ROLES};
use crate::app::Bot;
use crate::config::nick;
use crate::errors::ERRORS;
use crate::playlists::{playlist_id, LIKED};
use crate::track::{extract_video_id, Track};

static NICK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z0-9_]{1,25}$").unwrap()); // логін Twitch

const APP_HTML: &str = include_str!("../../../web/app.html");
const WIDGET_HTML: &str = include_str!("../../../web/widget.html");
/// Попередній віджет — для відкату: /widget/classic
const WIDGET_CLASSIC_HTML: &str = include_str!("../../../web/legacy/widget-classic.html");
/// Fixel (MacPaw, SIL OFL) — шрифт віджета, вбудований у бота
const FONTS: &[(&str, &[u8])] = &[
    ("FixelDisplay-Bold.woff2", include_bytes!("../../../web/static/fonts/FixelDisplay-Bold.woff2")),
    ("FixelText-Medium.woff2", include_bytes!("../../../web/static/fonts/FixelText-Medium.woff2")),
    ("FixelText-SemiBold.woff2", include_bytes!("../../../web/static/fonts/FixelText-SemiBold.woff2")),
];
const PANEL_HTML: &str = include_str!("../../../web/panel.html");
const SETUP_HTML: &str = include_str!("../../../web/setup.html");
const MINI_HTML: &str = include_str!("../../../web/mini.html");
const ICO: &[u8] = include_bytes!("../../../web/static/pipabobik.ico");
const DEMO_COVER: &[u8] = include_bytes!("../../../web/static/demo-cover.svg");

type S = State<Arc<Bot>>;

/// Невід'ємне ціле з поля застосунку; сміття вважаємо нулем.
fn num(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f.max(0.0) as u64).unwrap_or(0),
        Value::String(s) => s.trim().parse::<f64>().map(|f| f.max(0.0) as u64).unwrap_or(0),
        Value::Bool(b) => *b as u64,
        _ => 0,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Null => false,
    }
}

fn as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        x => x.to_string(),
    }
}

fn err(status: StatusCode, text: impl Into<String>) -> Response {
    (status, Json(json!({"error": text.into()}))).into_response()
}

fn body(b: &Bytes) -> Value {
    serde_json::from_slice(b).unwrap_or_else(|_| json!({}))
}

fn authorized(bot: &Bot, h: &HeaderMap) -> bool {
    h.get("X-Pipabobik-Key").and_then(|v| v.to_str().ok()) == Some(bot.key.as_str())
}

macro_rules! guard {
    ($bot:expr, $h:expr) => {
        if !authorized(&$bot, &$h) {
            return err(StatusCode::FORBIDDEN, "bad key");
        }
    };
}

fn page(html: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], html).into_response()
}

pub fn router(bot: Arc<Bot>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/widget", get(|| async { page(WIDGET_HTML) }))
        .route("/widget/classic", get(|| async { page(WIDGET_CLASSIC_HTML) }))
        .route("/static/fonts/{name}", get(font))
        .route("/cover", get(cover))
        .route("/panel", get(|| async { page(PANEL_HTML) }))
        .route("/app", get(app_page))
        .route("/setup", get(|| async { page(SETUP_HTML) }))
        .route("/mini", get(|| async { page(MINI_HTML) }))
        .route("/favicon.ico", get(|| async { ([(header::CONTENT_TYPE, "image/x-icon")], ICO) }))
        .route("/static/pipabobik.ico", get(|| async { ([(header::CONTENT_TYPE, "image/x-icon")], ICO) }))
        .route("/static/demo-cover.svg", get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], DEMO_COVER) }))
        .route("/api/search", axum::routing::post(api_search))
        .route("/api/add", axum::routing::post(api_add))
        .route("/api/settings", get(api_settings_get).post(api_settings_set))
        .route("/api/update", get(api_update_get).post(api_update_set))
        .route("/api/errors", get(api_errors_get).post(api_errors_set))
        .route("/api/playlists", get(api_playlists_get).post(api_playlists_set))
        .route("/api/access", get(api_access_get).post(api_access_set))
        .route("/api/messages", get(api_messages_get).post(api_messages_set))
        .route("/api/setup", get(api_setup_get).post(api_setup_set))
        .route("/api/check", axum::routing::post(api_check))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/control", axum::routing::post(api_control))
        .route("/auth", get(auth_start))
        .route("/auth/callback", get(auth_callback))
        .with_state(bot)
}

// ── шрифти й обкладинки для віджета ──
async fn font(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match FONTS.iter().find(|(n, _)| *n == name) {
        Some((_, data)) => ([(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=31536000")], *data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Хости, з яких беремо обкладинки (YouTube Music / YouTube).
pub fn cover_allowed(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or("");
    u.scheme() == "https" && ["ytimg.com", "googleusercontent.com", "ggpht.com"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

type CoverCache = std::sync::Mutex<std::collections::VecDeque<(String, String, Bytes)>>;
static COVERS: Lazy<CoverCache> = Lazy::new(Default::default);

#[derive(serde::Deserialize)]
struct CoverQ {
    u: String,
}

/// Обкладинка через бота: віджет може прочитати з неї колір (браузер не дає читати картинки з чужих сайтів).
/// Лише в пам'яті, останні 40 штук; на диск нічого не пишеться.
async fn cover(State(bot): S, Query(q): Query<CoverQ>) -> Response {
    if !cover_allowed(&q.u) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let hit = COVERS.lock().unwrap().iter().find(|(u, ..)| *u == q.u).map(|(_, ct, b)| (ct.clone(), b.clone()));
    let (ct, data) = match hit {
        Some(h) => h,
        None => {
            let r = match bot.http.get(&q.u).timeout(Duration::from_secs(10)).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => return StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY).into_response(),
                Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
            };
            let ct = r.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("image/jpeg").to_string();
            if !ct.starts_with("image/") || r.content_length().unwrap_or(0) > 3_000_000 {
                return StatusCode::BAD_GATEWAY.into_response();
            }
            let Ok(data) = r.bytes().await else { return StatusCode::BAD_GATEWAY.into_response() };
            let mut c = COVERS.lock().unwrap();
            c.push_back((q.u.clone(), ct.clone(), data.clone()));
            while c.len() > 40 {
                c.pop_front();
            }
            (ct, data)
        }
    };
    ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "public, max-age=86400".to_string())], data).into_response()
}

// ── стан ──
async fn api_state(State(bot): S) -> Response {
    ([(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], Json(bot.payload())).into_response()
}

async fn api_events(State(bot): S) -> Response {
    let mut rx = bot.jukebox.subscribe();
    let b = bot.clone();
    let stream = async_stream::stream! {
        rx.borrow_and_update();
        yield Ok::<Event, Infallible>(Event::default().data(b.payload().to_string()));
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            // кілька змін підряд (наприклад, кінець треку й початок наступного) — одне повідомлення
            tokio::time::sleep(Duration::from_millis(40)).await;
            rx.borrow_and_update();
            yield Ok(Event::default().data(b.payload().to_string()));
        }
    };
    let sse = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("ping"));
    ([(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"), (header::CACHE_CONTROL, "no-cache")], sse).into_response()
}

// ── сторінки ──
#[derive(serde::Deserialize)]
struct KeyQ {
    key: Option<String>,
}

/// Поки бот не налаштовано (немає каналу чи застосунку Twitch), застосунок відкриває майстер.
async fn app_page(State(bot): S, Query(q): Query<KeyQ>) -> Response {
    if !bot.configured() {
        let key = q.key.unwrap_or_default();
        return Redirect::temporary(&format!("/setup?key={}", urlencoding::encode(&key))).into_response();
    }
    bot.updates.check_soon();
    page(APP_HTML)
}

async fn index(State(bot): S) -> Response {
    let authed = bot.auth.is_authorized();
    let port = bot.port();
    let base = format!("http://localhost:{port}");
    let channel = bot.cfg.read().unwrap().channel();
    let login = bot.auth.login().unwrap_or_default();
    let rows = [
        ("Twitch", if authed { format!("✅ увійшов як <b>{login}</b>") } else { "❌ <a href=\"/auth\">увійти в Twitch</a>".into() }),
        ("Канал", if channel.is_empty() { "❌ не вказано в config.toml".into() } else { format!("#{channel}") }),
        ("Чат", if bot.chat.is_connected() { "✅ підключено".into() } else { "⏳ підключення…".into() }),
        ("mpv", if bot.player.available() { "✅ знайдено".into() } else { "❌ не знайдено — встановіть mpv".into() }),
        ("Віджет", format!("<code>{base}/widget</code>")),
        ("Панель (док OBS)", format!("<code>{base}/panel?key={}</code>", bot.key)),
    ];
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>pipabobik</title>\
         <style>body{{font:15px system-ui;background:#1a0f17;color:#fbe7f2;padding:32px}}\
         h1{{color:#ff5fa8}}td{{padding:8px 14px;border-bottom:1px solid #3a2232}}\
         a{{color:#ff8cc6}}code{{background:#2c1726;padding:3px 6px;border-radius:6px}}</style>\
         <h1>🎀 pipabobik</h1><table>{}</table>",
        rows.iter().map(|(k, v)| format!("<tr><td>{k}</td><td>{v}</td></tr>")).collect::<String>()
    );
    Html(html).into_response()
}

// ── керування з застосунку ──
async fn api_control(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d = body(&b);
    let jb = &bot.jukebox;
    let action = d["action"].as_str().unwrap_or("");
    let idx = |k: &str| d[k].as_i64().or_else(|| d[k].as_f64().map(|f| f as i64)).unwrap_or(-1);
    match action {
        "skip" | "ban" => {
            // кнопка надсилає, ЯКИЙ трек пропустити: подвійне натискання чи затримка не зачеплять наступний трек
            let vid = as_str(&d["video_id"]);
            let cur = jb.now();
            let ignore = match &cur {
                None => true,
                Some((_, t)) => (!vid.is_empty() && t.video_id != vid) || (vid.is_empty() && jb.recently_skipped()),
            };
            if ignore {
                let mut p = bot.payload();
                p["ignored"] = json!(true);
                return Json(p).into_response();
            }
            let (id, _) = cur.unwrap();
            if action == "skip" {
                jb.skip("кнопка в застосунку", Some(id)).await;
            } else {
                jb.ban_current().await;
            }
        }
        "ban_artist" => {
            // ім'я напряму або виконавець треку (з того, що грає, черги чи історії)
            let mut name = as_str(&d["artist"]).trim().to_string();
            if name.is_empty() {
                let vid = as_str(&d["video_id"]);
                let found = jb.now().map(|(_, t)| t).filter(|t| vid.is_empty() || t.video_id == vid).or_else(|| jb.find_by_id(&vid));
                name = found.and_then(|t| crate::jukebox::main_artist(&t)).unwrap_or_default();
            }
            match jb.ban_artist_now(&name).await {
                None => return err(StatusCode::BAD_REQUEST, "Вкажіть ім'я виконавця"),
                Some((removed, _)) => {
                    let mut p = bot.payload();
                    p["banned_artist"] = json!(name);
                    p["removed"] = json!(removed);
                    return Json(p).into_response();
                }
            }
        }
        "pause" => jb.set_pause(!jb.player.paused()).await,
        "volume" => {
            let v = d["value"].as_f64().map(|f| f as i64).or_else(|| d["value"].as_str().and_then(|s| s.parse().ok())).unwrap_or_else(|| jb.player.volume());
            jb.set_volume(v).await;
        }
        "seek" => {
            if jb.now().is_none() {
                return err(StatusCode::CONFLICT, "Зараз нічого не грає");
            }
            let pos = if !d["position"].is_null() {
                d["position"].as_f64().or_else(|| d["position"].as_str().and_then(|s| s.parse().ok()))
            } else {
                let delta = if d["delta"].is_null() { Some(0.0) } else { d["delta"].as_f64().or_else(|| d["delta"].as_str().and_then(|s| s.parse().ok())) };
                delta.map(|x| jb.player.position() + x)
            };
            let Some(pos) = pos else { return err(StatusCode::BAD_REQUEST, "Невірна позиція") };
            if jb.seek(pos).await.is_none() {
                return err(StatusCode::CONFLICT, "Не вдалося перемотати");
            }
        }
        "remove" => {
            jb.remove_at(idx("index"));
        }
        "up" => jb.move_up(idx("index")),
        "move" => jb.move_to(idx("index"), idx("to")),
        "next" => jb.move_to(idx("index"), 0), // поставити трек із черги першим
        "clear" => jb.clear(),
        "unban" => jb.unban(&as_str(&d["video_id"])),
        "like" => {
            let mut vid = as_str(&d["video_id"]);
            if vid.is_empty() {
                vid = jb.now().map(|(_, t)| t.video_id).unwrap_or_default();
            }
            if vid.is_empty() {
                return err(StatusCode::CONFLICT, "Зараз нічого не грає");
            }
            let want = if d.get("like").is_some() { truthy(&d["like"]) } else { !jb.is_liked(&vid) };
            match jb.like(&vid, want).await {
                Ok(()) => {}
                Err(crate::music::MusicError::LoginRequired) => {
                    return err(StatusCode::CONFLICT, "Щоб лайкати, увійдіть у music.youtube.com у Firefox і перезапустіть бота");
                }
                Err(e) => {
                    tracing::warn!("Лайк не вдався: {e}");
                    return err(StatusCode::BAD_GATEWAY, "YouTube Music не прийняв лайк — спробуйте ще раз");
                }
            }
        }
        "on" => jb.activate(),
        "off" => jb.deactivate().await,
        "restart" => {
            let b2 = bot.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                b2.restart();
            });
            return Json(json!({"ok": true})).into_response();
        }
        "shutdown" => {
            let b2 = bot.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                b2.shutdown();
            });
            return Json(json!({"ok": true})).into_response();
        }
        _ => return err(StatusCode::BAD_REQUEST, "unknown action"),
    }
    Json(bot.payload()).into_response()
}

// ── пошук і додавання з застосунку ──
async fn api_search(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let q: String = as_str(&body(&b)["q"]).trim().chars().take(200).collect();
    if q.is_empty() {
        return Json(json!({"results": []})).into_response();
    }
    let results: Vec<Track> = if let Some(vid) = extract_video_id(&q) {
        bot.music.by_id(&vid).await.into_iter().collect()
    } else {
        let only = bot.cfg.read().unwrap().raw["rules"]["only_songs"].as_bool().unwrap_or(true);
        bot.music.search(&q, only).await
    };
    Json(json!({"results": results.iter().take(8).map(Track::to_json).collect::<Vec<_>>()})).into_response()
}

async fn api_add(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d = body(&b);
    let Some(track) = Track::from_json(&d["track"]) else { return err(StatusCode::BAD_REQUEST, "bad track") };
    let jb = &bot.jukebox;
    if let Some(r) = jb.banned_reason(&track) {
        let r = r.trim_end_matches(" 🚫").trim_end_matches(" каналу").to_string();
        return err(StatusCode::CONFLICT, format!("{r}. Розбаньте у вкладці «Історія», якщо хочете зіграти."));
    }
    if let Some(reason) = jb.filter_reason(&track).await {
        jb.ban_track(&track, &reason);
        return err(StatusCode::CONFLICT, format!("{} — {reason}. Трек додано в бан-лист.", track.label()));
    }
    let mode = d["mode"].as_str().unwrap_or("end"); // end | next | now
    let mut who = bot.auth.login().unwrap_or_default();
    if who.is_empty() {
        who = bot.cfg.read().unwrap().channel();
    }
    if who.is_empty() {
        who = "стрімер".into();
    }
    let playing = jb.now();
    jb.add(track, "panel", &who, mode == "next" || mode == "now");
    if mode == "now" {
        if !jb.is_active() {
            jb.activate();
        } else if let Some((id, _)) = playing {
            jb.skip("«Зараз» у застосунку", Some(id)).await;
        }
    }
    Json(bot.payload()).into_response()
}

// ── налаштування ──
#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Int,
    Bool,
    List,
    Str,
}

const SETTINGS: &[(&str, &[(&str, Ty)])] = &[
    (
        "rules",
        &[
            ("max_duration", Ty::Int),
            ("max_per_user", Ty::Int),
            ("cooldown", Ty::Int),
            ("max_queue", Ty::Int),
            ("only_songs", Ty::Bool),
            ("block_explicit", Ty::Bool),
            ("interrupt_fallback", Ty::Bool),
            ("banned_words", Ty::List),
            ("block_russian", Ty::Bool),
            ("russian_artists", Ty::List),
        ],
    ),
    ("chat", &[("announce_tracks", Ty::Bool), ("greet_on_stream_start", Ty::Bool)]),
    ("player", &[("normalize", Ty::Bool), ("volume", Ty::Int)]),
    ("google", &[("fallback_playlist", Ty::Str), ("fallback_shuffle", Ty::Bool)]),
    ("twitch", &[("helpers", Ty::List)]),
    ("widget", &[("pink", Ty::Bool), ("transparency", Ty::Int)]),
];

async fn api_settings_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    let cfg = bot.cfg.read().unwrap();
    let mut out = Map::new();
    for (sec, keys) in SETTINGS {
        let m: Map<String, Value> = keys.iter().map(|(k, _)| (k.to_string(), cfg.raw[*sec][*k].clone())).collect();
        out.insert(sec.to_string(), Value::Object(m));
    }
    // лише для підказок
    let cmds: Map<String, Value> = cfg.raw["commands"].as_object().map(|o| o.iter().filter_map(|(k, v)| v[0].as_str().map(|f| (k.clone(), json!(f)))).collect()).unwrap_or_default();
    out.insert("commands".into(), Value::Object(cmds));
    Json(Value::Object(out)).into_response()
}

async fn api_settings_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d = body(&b);
    let mut updates: Vec<(String, Map<String, Value>)> = Vec::new();
    for (sec, keys) in SETTINGS {
        let mut m = Map::new();
        for (k, ty) in keys.iter() {
            let Some(v) = d.get(*sec).and_then(|s| s.get(*k)) else { continue };
            let v = match ty {
                Ty::Bool => json!(truthy(v)),
                Ty::Int => {
                    let n = match v {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.trim().parse::<f64>().ok(),
                        Value::Bool(b) => Some(*b as u8 as f64),
                        _ => None,
                    };
                    match n {
                        // відсотки: гучність запуску, прозорість віджета
                        Some(n) if matches!(*k, "volume" | "transparency") => json!(n.clamp(0.0, 100.0) as i64),
                        Some(n) => json!(n.max(0.0) as i64),
                        None => return err(StatusCode::BAD_REQUEST, format!("{sec}.{k}: невірне значення")),
                    }
                }
                Ty::List => {
                    let items: Vec<String> = match v {
                        Value::Array(a) => a.iter().map(as_str).collect(),
                        x => as_str(x).split(',').map(String::from).collect(),
                    };
                    let mut items: Vec<String> = items.into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                    if *k == "banned_words" {
                        items = items.into_iter().map(|x| x.to_lowercase()).collect();
                    } else if *k == "helpers" {
                        let mut uniq: Vec<String> = Vec::new();
                        for n in items.iter().map(|x| nick(x)) {
                            if NICK_RE.is_match(&n) && !uniq.contains(&n) {
                                uniq.push(n);
                            }
                        }
                        items = uniq;
                    }
                    json!(items)
                }
                Ty::Str => json!(as_str(v).trim()),
            };
            m.insert(k.to_string(), v);
        }
        if !m.is_empty() {
            updates.push((sec.to_string(), m));
        }
    }
    if !updates.is_empty() {
        // застосовується одразу — ту саму конфігурацію читають черга й команди
        {
            let mut cfg = bot.cfg.write().unwrap();
            for (sec, m) in &updates {
                for (k, v) in m {
                    cfg.set(sec, k, v.clone());
                }
            }
        }
        bot.save_config(&updates);
        bot.apply_settings(&updates);
    }
    Json(json!({"ok": true})).into_response()
}

// ── оновлення ──
async fn api_update_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    Json(bot.updates.summary()).into_response()
}

async fn api_update_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let up = &bot.updates;
    match body(&b)["action"].as_str().unwrap_or("") {
        "check" => {
            up.check().await;
        }
        "install" => {
            let st = up.state();
            if st == "downloading" || st == "installing" {
                return Json(up.summary()).into_response();
            }
            if up.latest()["tag"].is_null() {
                up.check().await;
            }
            if !up.available() {
                let e = up.error();
                return err(StatusCode::CONFLICT, if e.is_empty() { "У вас уже остання версія".to_string() } else { e });
            }
            let b2 = bot.clone();
            tokio::spawn(async move { b2.install_update().await }); // далі стан приходить через події
        }
        "dismiss" => up.dismiss(),
        _ => return err(StatusCode::BAD_REQUEST, "unknown action"),
    }
    Json(up.summary()).into_response()
}

// ── помилки ──
fn errors_payload() -> Value {
    let mut v = ERRORS.summary();
    v["items"] = json!(ERRORS.list());
    v
}

async fn api_errors_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    Json(errors_payload()).into_response()
}

async fn api_errors_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    match body(&b)["action"].as_str().unwrap_or("") {
        "clear" => ERRORS.clear(),
        "seen" => ERRORS.seen(),
        _ => return err(StatusCode::BAD_REQUEST, "unknown action"),
    }
    Json(errors_payload()).into_response()
}

// ── фонові плейлисти ──
fn playlists_payload(bot: &Bot) -> Value {
    let cfg = bot.cfg.read().unwrap();
    json!({
        "items": bot.playlists.lock().unwrap().items,
        "active": playlist_id(&cfg.str("google", "fallback_playlist")).unwrap_or_default(),
        "shuffle": cfg.raw["google"]["fallback_shuffle"].as_bool().unwrap_or(true),
        "theme": cfg.str("google", "fallback_theme"),
    })
}

/// «різдвяні» + «Christmas Hits» → «Різдвяні · Christmas Hits» (до 60 знаків).
fn theme_name(theme: &str, title: &str) -> String {
    let mut c = theme.trim().chars();
    let t: String = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    let full = if title.to_lowercase().contains(&theme.trim().to_lowercase()) { title.to_string() } else { format!("{t} · {title}") };
    full.chars().take(60).collect()
}

fn set_shuffle(bot: &Bot, v: bool) {
    bot.cfg.write().unwrap().set("google", "fallback_shuffle", json!(v));
    let mut m = Map::new();
    m.insert("fallback_shuffle".into(), json!(v));
    bot.save_config(&[("google".into(), m)]);
    bot.jukebox.reset_fallback(); // перемішаємо заново з наступного фонового треку
}

async fn api_playlists_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    Json(playlists_payload(&bot)).into_response()
}

async fn select_playlist(bot: &Arc<Bot>, pid: &str) {
    bot.cfg.write().unwrap().set("google", "fallback_playlist", json!(pid));
    let mut m = Map::new();
    m.insert("fallback_playlist".into(), json!(pid));
    bot.save_config(&[("google".into(), m)]);
    let jb = &bot.jukebox;
    jb.reset_fallback();
    if let Some((id, t)) = jb.now() {
        if t.source == "fallback" {
            jb.skip("перемкнули фоновий плейлист", Some(id)).await; // новий плейлист — одразу
        }
    }
    jb.wake();
    jb.changed();
}

async fn api_playlists_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d = body(&b);
    match d["action"].as_str().unwrap_or("") {
        "add" => {
            let Some(pid) = playlist_id(&as_str(&d["link"])) else {
                return err(StatusCode::BAD_REQUEST, "Не бачу плейлиста в цьому посиланні. Скопіюйте адресу сторінки плейлиста з YouTube Music");
            };
            let mut name: String = as_str(&d["name"]).trim().chars().take(60).collect();
            if name.is_empty() {
                name = match bot.music.playlist_title(&pid).await {
                    Some(t) => t,
                    None if pid == LIKED => "Вподобані".into(),
                    None => "Плейлист".into(),
                };
            }
            bot.playlists.lock().unwrap().add(&pid, &name);
            if truthy(&d["select"]) {
                select_playlist(&bot, &pid).await;
            }
        }
        "select" => {
            let pid = as_str(&d["id"]);
            if !pid.is_empty() && bot.playlists.lock().unwrap().get(&pid).is_none() {
                return err(StatusCode::BAD_REQUEST, "Такого плейлиста немає в списку");
            }
            select_playlist(&bot, &pid).await;
        }
        "remove" => {
            let pid = as_str(&d["id"]);
            let next = {
                let mut pls = bot.playlists.lock().unwrap();
                pls.remove(&pid);
                pls.items.first().map(|p| p.id.clone()).unwrap_or_default()
            };
            let active = playlist_id(&bot.cfg.read().unwrap().str("google", "fallback_playlist"));
            if active.as_deref() == Some(pid.as_str()) {
                select_playlist(&bot, &next).await;
            }
        }
        "rename" => {
            let name: String = as_str(&d["name"]).trim().chars().take(60).collect();
            bot.playlists.lock().unwrap().rename(&as_str(&d["id"]), &name);
        }
        // тема: «Різдвяні» → знаходимо плейлист у YouTube Music і одразу вмикаємо його фоном у довільному порядку
        "theme" => {
            let q: String = as_str(&d["query"]).trim().chars().take(60).collect();
            if q.is_empty() {
                return err(StatusCode::BAD_REQUEST, "Напишіть тему, наприклад «Різдвяні»");
            }
            let found = bot.music.search_playlists(&q).await;
            let Some(first) = found.first().cloned() else {
                return err(StatusCode::BAD_REQUEST, &format!("Не знайшов плейлистів на тему «{q}». Спробуйте інакше: «різдвяні пісні» чи «christmas»"));
            };
            let name = theme_name(&q, &first.title);
            bot.playlists.lock().unwrap().add(&first.id, &name);
            set_shuffle(&bot, true);
            {
                bot.cfg.write().unwrap().set("google", "fallback_theme", json!(q));
                let mut m = Map::new();
                m.insert("fallback_theme".into(), json!(q));
                bot.save_config(&[("google".into(), m)]);
            }
            select_playlist(&bot, &first.id).await;
            tracing::info!("Тема фонової музики «{q}»: {} ({})", first.title, first.id);
            let mut out = playlists_payload(&bot);
            out["found"] = json!({"query": q, "items": found});
            return Json(out).into_response();
        }
        "shuffle" => {
            set_shuffle(&bot, truthy(&d["value"]));
        }
        _ => return err(StatusCode::BAD_REQUEST, "unknown action"),
    }
    Json(playlists_payload(&bot)).into_response()
}

// ── команди, ролі, ліміти ──
fn access_payload(bot: &Bot) -> Value {
    let cfg = bot.cfg.read().unwrap();
    let acc = Access::new(&cfg);
    let mut limits = Map::new();
    let all = acc.role_limits("all");
    limits.insert("all".into(), json!({"max_per_user": all.max_per_user, "cooldown": all.cooldown, "max_duration": all.max_duration, "bypass_queue": false}));
    if let Some(o) = cfg.raw["limits"].as_object() {
        for (k, v) in o {
            limits.insert(k.clone(), v.clone());
        }
    }
    json!({
        "roles": ROLES.iter().map(|(k, v)| json!({"key": k, "label": v})).collect::<Vec<_>>(),
        "commands": COMMANDS.iter().map(|(k, label, desc, _)| json!({
            "key": k, "label": label, "desc": desc, "names": cfg.commands(k), "roles": acc.roles_for(k),
        })).collect::<Vec<_>>(),
        "limits": limits,
        "helpers": cfg.str_list("twitch", "helpers"),
        "max_queue": cfg.raw["rules"]["max_queue"].as_u64().unwrap_or(30),
        "help": {
            "text": cfg.str("help", "text"),
            "hidden": cfg.str_list("help", "hidden"),
            "placeholder": crate::config::HELP_PLACEHOLDER,
            "default": crate::config::DEFAULT_HELP,
        },
    })
}

fn access_payload_full(bot: &Bot) -> Value {
    let mut v = access_payload(bot);
    v["help"]["preview"] = json!(bot.commands.help_preview());
    v
}

// ── тексти повідомлень у чат ──
fn messages_payload(bot: &Bot) -> Value {
    let cfg = bot.cfg.read().unwrap();
    let req = cfg.commands("request").into_iter().next().unwrap_or_else(|| "!трек".into());
    let now = cfg.commands("now").into_iter().next().unwrap_or_else(|| "!шоце".into());
    // приклад того, що побачить чат
    let sample: Vec<(&str, String)> = vec![
        ("трек", "Go_A — Шум".into()),
        ("тривалість", "3:01".into()),
        ("місце", "2".into()),
        ("коли", "заграє через ~4 хв".into()),
        ("позиція", "1:12".into()),
        ("хто", "замовив(ла) @pipa_fan".into()),
        ("посилання", "https://music.youtube.com/watch?v=…".into()),
        ("список", "1) Go_A — Шум (@pipa_fan) · 2) KAZKA — Плакала (@kotyk_ua)".into()),
        ("кількість", "2".into()),
        ("гучність", "60".into()),
        ("виконавець", "KAZKA".into()),
        ("прибрано", ", з черги прибрано треків: 1".into()),
        ("запит", "шум гоа".into()),
        ("нік", "pipa_fan".into()),
        ("замовити", req),
        ("щограє", now),
    ];
    let items: Vec<Value> = crate::messages::MESSAGES
        .iter()
        .map(|m| {
            json!({
                "key": m.key,
                "label": m.label,
                "default": m.default,
                "vars": m.vars,
                "text": cfg.str("messages", m.key),
                "preview": crate::messages::render(&cfg, m.key, &sample),
            })
        })
        .collect();
    json!({ "items": items, "max": crate::messages::MAX_LEN })
}

async fn api_messages_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    Json(messages_payload(&bot)).into_response()
}

/// {"messages": {"skip": "текст"}} — порожній текст повертає стандартний.
async fn api_messages_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
    let mut upd = Map::new();
    for (k, v) in d["messages"].as_object().into_iter().flatten() {
        if crate::messages::find(k).is_none() {
            continue;
        }
        let t: String = as_str(v).trim().chars().take(crate::messages::MAX_LEN).collect();
        upd.insert(k.clone(), json!(t));
    }
    if !upd.is_empty() {
        {
            let mut cfg = bot.cfg.write().unwrap();
            for (k, v) in &upd {
                cfg.set("messages", k, v.clone());
            }
        }
        bot.save_config(&[("messages".into(), upd)]);
        tracing::info!("Тексти повідомлень змінено з застосунку");
    }
    Json(messages_payload(&bot)).into_response()
}

async fn api_access_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    Json(access_payload_full(&bot)).into_response()
}

async fn api_access_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d = body(&b);
    let bad = |t: String| err(StatusCode::BAD_REQUEST, t);
    let label_of = |k: &str| COMMANDS.iter().find(|c| c.0 == k).map(|c| c.1).unwrap_or(k).to_string();

    let (cur_names, cur_limits) = {
        let cfg = bot.cfg.read().unwrap();
        let names: Vec<(String, Vec<String>)> = COMMANDS.iter().map(|(k, ..)| (k.to_string(), cfg.commands(k))).collect();
        (names, cfg.raw["limits"].clone())
    };
    // назви команд і хто може
    let mut new_names = cur_names.clone();
    let mut new_perms = Map::new();
    if let Some(cmds) = d["commands"].as_object() {
        for (key, upd) in cmds {
            if !COMMANDS.iter().any(|c| c.0 == key) || !upd.is_object() {
                continue;
            }
            if let Some(list) = upd.get("names") {
                let mut names: Vec<String> = Vec::new();
                for n in list.as_array().map(|a| a.iter().map(as_str).collect::<Vec<_>>()).unwrap_or_default() {
                    let n = normalize_name(&n);
                    if !n.is_empty() && !names.contains(&n) {
                        names.push(n);
                    }
                }
                if names.is_empty() {
                    return bad(format!("«{}»: потрібна хоча б одна назва", label_of(key)));
                }
                if let Some(b) = names.iter().find(|n| !valid_name(n)) {
                    return bad(format!("«{b}» не підходить: одне слово з ! на початку, до 25 символів, без пробілів"));
                }
                if let Some(e) = new_names.iter_mut().find(|(k, _)| k == key) {
                    e.1 = names;
                }
            }
            if let Some(r) = upd.get("roles") {
                new_perms.insert(key.clone(), json!(normalize_roles(r)));
            }
        }
    }
    let mut owner: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (key, names) in &new_names {
        for n in names {
            if let Some(o) = owner.get(n) {
                if o != key {
                    return bad(format!("{n} уже зайнято командою «{}»", label_of(o)));
                }
            }
            owner.insert(n.clone(), key.clone());
        }
    }
    // ліміти ролей
    let mut rules_upd = Map::new();
    let mut role_upd: Vec<(String, Value)> = Vec::new();
    let defaults = default_limits();
    if let Some(lims) = d["limits"].as_object() {
        for (role, vals) in lims {
            let Some(vals) = vals.as_object() else { continue };
            if role == "all" {
                for k in ["max_per_user", "cooldown", "max_duration"] {
                    if let Some(v) = vals.get(k) {
                        rules_upd.insert(k.into(), json!(num(v)));
                    }
                }
            } else if defaults.get(role).is_some() {
                let mut cur = cur_limits.get(role).cloned().unwrap_or_else(|| defaults[role].clone());
                for k in std::iter::once("custom").chain(LIMIT_KEYS.iter().copied()) {
                    if let Some(v) = vals.get(k) {
                        cur[k] = if k == "custom" || k == "bypass_queue" { json!(truthy(v)) } else { json!(num(v)) };
                    }
                }
                role_upd.push((role.clone(), cur));
            }
        }
    }
    if let Some(v) = d.get("max_queue") {
        rules_upd.insert("max_queue".into(), json!(num(v).max(1)));
    }
    let helpers = d.get("helpers").map(|hs| {
        let mut out: Vec<String> = Vec::new();
        for n in hs.as_array().map(|a| a.iter().map(|x| nick(&as_str(x))).collect::<Vec<_>>()).unwrap_or_default() {
            if NICK_RE.is_match(&n) && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    });

    // підказка !музика: власний текст і які команди не показувати в списку
    let mut help_upd = Map::new();
    if let Some(h) = d.get("help").and_then(|h| h.as_object()) {
        if let Some(t) = h.get("text") {
            let t: String = as_str(t).trim().chars().take(450).collect();
            help_upd.insert("text".into(), json!(if t.is_empty() { crate::config::DEFAULT_HELP.to_string() } else { t }));
        }
        if let Some(hd) = h.get("hidden") {
            let keys: Vec<String> = hd.as_array().map(|a| a.iter().map(as_str).filter(|k| COMMANDS.iter().any(|c| c.0 == k)).collect()).unwrap_or_default();
            help_upd.insert("hidden".into(), json!(keys));
        }
    }

    // усе перевірено — застосовуємо одразу і зберігаємо в config.toml
    let mut updates: Vec<(String, Map<String, Value>)> = Vec::new();
    let changed_names: Map<String, Value> = new_names.iter().zip(cur_names.iter()).filter(|(n, c)| n.1 != c.1).map(|(n, _)| (n.0.clone(), json!(n.1))).collect();
    {
        let mut cfg = bot.cfg.write().unwrap();
        for (k, v) in &changed_names {
            cfg.set("commands", k, v.clone());
        }
        for (k, v) in &new_perms {
            cfg.set("permissions", k, v.clone());
        }
        for (k, v) in &rules_upd {
            cfg.set("rules", k, v.clone());
        }
        for (role, cur) in &role_upd {
            cfg.set("limits", role, cur.clone());
        }
        if let Some(hl) = &helpers {
            cfg.set("twitch", "helpers", json!(hl));
        }
        for (k, v) in &help_upd {
            cfg.set("help", k, v.clone());
        }
    }
    if !help_upd.is_empty() {
        updates.push(("help".into(), help_upd));
    }
    if !changed_names.is_empty() {
        updates.push(("commands".into(), changed_names.clone()));
    }
    if !new_perms.is_empty() {
        updates.push(("permissions".into(), new_perms));
    }
    if !rules_upd.is_empty() {
        updates.push(("rules".into(), rules_upd));
    }
    for (role, cur) in role_upd {
        updates.push((format!("limits.{role}"), cur.as_object().cloned().unwrap_or_default()));
    }
    if let Some(hl) = helpers {
        let mut m = Map::new();
        m.insert("helpers".into(), json!(hl));
        updates.push(("twitch".into(), m));
    }
    if !updates.is_empty() {
        bot.save_config(&updates);
        if !changed_names.is_empty() {
            bot.commands.rebuild();
        }
        tracing::info!("Доступ оновлено з застосунку: {}", updates.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", "));
    }
    Json(access_payload_full(&bot)).into_response()
}

// ── майстер першого запуску й діагностика ──
async fn api_setup_get(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    let cfg = bot.cfg.read().unwrap();
    Json(json!({
        "channel": cfg.channel(),
        "client_id": cfg.str("twitch", "client_id"),
        "has_secret": !cfg.str("twitch", "client_secret").is_empty(),
        "cookies_from_browser": cfg.str("google", "cookies_from_browser"),
        "cookies_file": cfg.str("google", "cookies_file"),
        "fallback_playlist": cfg.str("google", "fallback_playlist"),
        "redirect": bot.auth.redirect_uri,
        "configured": bot.configured(),
        "twitch": bot.auth.is_authorized(),
    }))
    .into_response()
}

async fn api_setup_set(State(bot): S, h: HeaderMap, b: Bytes) -> Response {
    guard!(bot, h);
    let d = body(&b);
    let channel = as_str(&d["channel"]).trim().trim_start_matches(['#', '@']).to_lowercase();
    let client_id = as_str(&d["client_id"]).trim().to_string();
    let secret = as_str(&d["client_secret"]).trim().to_string();
    if !NICK_RE.is_match(&channel) {
        return err(StatusCode::BAD_REQUEST, "Вкажіть нік каналу на Twitch: латинські літери, цифри й _");
    }
    if client_id.len() < 10 {
        return err(StatusCode::BAD_REQUEST, "Вставте Client ID зі сторінки застосунку на dev.twitch.tv");
    }
    let has_secret = !bot.cfg.read().unwrap().str("twitch", "client_secret").is_empty();
    if secret.is_empty() && !has_secret {
        return err(StatusCode::BAD_REQUEST, "Вставте Client Secret (кнопка «New Secret» на dev.twitch.tv)");
    }
    let browser = as_str(&d["cookies_from_browser"]).trim().to_lowercase();
    let browser = if ["firefox", "chrome", "edge", "brave", "opera", "vivaldi", "chromium", ""].contains(&browser.as_str()) { browser } else { "firefox".into() };
    let mut fallback = as_str(&d["fallback_playlist"]).trim().to_string();
    if fallback != "-" && !fallback.is_empty() {
        match playlist_id(&fallback) {
            Some(p) => fallback = p,
            None => return err(StatusCode::BAD_REQUEST, "Не бачу плейлиста в цьому посиланні. Залиште LM (ваші вподобані) або «-» для тиші"),
        }
    } else {
        fallback = String::new();
    }
    let mut tw = Map::new();
    tw.insert("channel".into(), json!(channel));
    tw.insert("client_id".into(), json!(client_id));
    if !secret.is_empty() {
        tw.insert("client_secret".into(), json!(secret));
    }
    let mut g = Map::new();
    g.insert("cookies_from_browser".into(), json!(browser));
    g.insert("fallback_playlist".into(), json!(fallback));
    let updates = vec![("twitch".to_string(), tw), ("google".to_string(), g)];
    {
        let mut cfg = bot.cfg.write().unwrap();
        for (sec, m) in &updates {
            for (k, v) in m {
                cfg.set(sec, k, v.clone());
            }
        }
    }
    bot.save_config(&updates);
    if let Some(pid) = playlist_id(&fallback) {
        if pid != LIKED {
            bot.playlists.lock().unwrap().add(&pid, "Мій плейлист");
        }
    }
    tracing::info!("Налаштування збережено майстром — перезапускаю бота");
    let b2 = bot.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        b2.restart(); // новий канал і застосунок Twitch підхоплюються лише при старті
    });
    Json(json!({"ok": true, "restart": true})).into_response()
}

async fn api_check(State(bot): S, h: HeaderMap) -> Response {
    guard!(bot, h);
    Json(bot.diagnostics().await).into_response()
}

// ── авторизація Twitch ──
async fn auth_start(State(bot): S) -> Response {
    if bot.auth.client_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "Вкажіть client_id у config.toml (або в майстрі налаштування)").into_response();
    }
    Redirect::to(&bot.auth.authorize_url()).into_response()
}

#[derive(serde::Deserialize)]
struct CallbackQ {
    code: Option<String>,
    state: Option<String>,
    error_description: Option<String>,
}

async fn auth_callback(State(bot): S, Query(q): Query<CallbackQ>) -> Response {
    let text = |t: String| ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], t).into_response();
    let Some(code) = q.code else {
        return text(format!("❌ {}", q.error_description.unwrap_or_else(|| "авторизацію скасовано".into())));
    };
    if let Err(e) = bot.auth.exchange_code(&code, q.state.as_deref().unwrap_or("")).await {
        tracing::error!("Вхід у Twitch не вдався: {e}");
        return text(format!("❌ {e}"));
    }
    bot.jukebox.changed();
    let login = bot.auth.login().unwrap_or_default();
    Html(format!(
        "<!doctype html><meta charset=utf-8><body style='font:18px system-ui;background:#1a0f17;\
         color:#fbe7f2;display:grid;place-items:center;height:90vh'>\
         <div>✅ Готово! pipabobik увійшов як <b>{login}</b>. Вкладку можна закрити.</div>"
    ))
    .into_response()
}

pub async fn serve(bot: Arc<Bot>, listener: tokio::net::TcpListener, shutdown: impl std::future::Future<Output = ()> + Send + 'static) {
    let app = router(bot);
    if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown).await {
        tracing::error!("Сервер застосунку зупинився: {e}");
    }
}
