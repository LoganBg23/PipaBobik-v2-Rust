//! Перевірка API застосунку: ті самі запити, що надсилають сторінки web/*.html.

use std::sync::Arc;

use pipabobik_core::app::{Bot, BotOptions};
use pipabobik_core::config::load_config;
use serde_json::{json, Value};

struct T {
    base: String,
    key: String,
    http: reqwest::Client,
    bot: Arc<Bot>,
    _dir: tempfile::TempDir,
}

impl T {
    async fn new(config: &str) -> T {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, config).unwrap();
        // шляхи бота відносно PIPABOBIK_ROOT; кожен тест — у своїй папці (тести йдуть в одному процесі)
        std::env::set_var("PIPABOBIK_ROOT", dir.path());
        let cfg = load_config(&cfg_path).unwrap();
        let bot = Bot::new(cfg, BotOptions::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = pipabobik_core::server::router(bot.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        T { base, key: bot.key.clone(), http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(), bot, _dir: dir }
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.http.post(format!("{}{path}", self.base)).header("X-Pipabobik-Key", &self.key).json(&body).send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.http.get(format!("{}{path}", self.base)).header("X-Pipabobik-Key", &self.key).send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }
}

const CONFIGURED: &str = "# мій конфіг\n[twitch]\nchannel = \"pipa\"\nclient_id = \"abcdefghijklmnop\"\nclient_secret = \"s\"\n\n[google]\ncookies_from_browser = \"\"\nfallback_playlist = \"\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn api_matches_v1() {
    let t = T::new(CONFIGURED).await;

    // стан: ті самі поля, що в v1
    let (s, st) = t.get("/api/state").await;
    assert_eq!(s, 200);
    for k in ["status", "now", "queue", "queue_len", "volume", "active", "history", "banned", "health", "errors", "update", "streaming", "obs_restart", "boot", "fallback"] {
        assert!(st.get(k).is_some(), "немає поля {k}");
    }
    assert_eq!(st["health"]["channel"], "pipa");

    // без ключа керувати не можна
    let r = t.http.post(format!("{}/api/control", t.base)).json(&json!({"action": "pause"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);

    let (s, st) = t.post("/api/control", json!({"action": "volume", "value": 33})).await;
    assert_eq!(s, 200);
    assert_eq!(st["volume"], 33);
    let (s, _) = t.post("/api/control", json!({"action": "nope"})).await;
    assert_eq!(s, 400);
    let (s, e) = t.post("/api/control", json!({"action": "seek", "position": 10})).await;
    assert_eq!(s, 409);
    assert_eq!(e["error"], "Зараз нічого не грає");

    // додавання з застосунку і черга
    let track = json!({"video_id": "abcdefghijk", "title": "Обійми", "artist": "Океан Ельзи", "duration": 200});
    let (s, st) = t.post("/api/add", json!({"track": track, "mode": "end"})).await;
    assert_eq!(s, 200);
    assert_eq!(st["queue_len"], 1);
    assert_eq!(st["queue"][0]["requested_by"], "pipa");
    let (s, st) = t.post("/api/control", json!({"action": "remove", "index": 0})).await;
    assert_eq!(s, 200);
    assert_eq!(st["queue_len"], 0);

    // фільтр російської музики
    let (s, e) = t.post("/api/add", json!({"track": {"video_id": "zzzzzzzzzzz", "title": "Ты моя", "artist": "X"}})).await;
    assert_eq!(s, 409);
    assert!(e["error"].as_str().unwrap().contains("бан-лист"));

    // налаштування зберігаються в config.toml разом із коментарями
    let (s, _) = t.post("/api/settings", json!({"rules": {"cooldown": "45", "banned_words": "Earrape, 10 Hours"}, "twitch": {"helpers": ["@Helper_1", "bad nick!"]}})).await;
    assert_eq!(s, 200);
    let (_, set) = t.get("/api/settings").await;
    assert_eq!(set["rules"]["cooldown"], 45);
    assert_eq!(set["rules"]["banned_words"], json!(["earrape", "10 hours"]));
    assert_eq!(set["twitch"]["helpers"], json!(["helper_1"]));
    assert_eq!(set["commands"]["skip"], "!скіп");
    let text = std::fs::read_to_string(t.bot.cfg.read().unwrap().path.clone()).unwrap();
    assert!(text.contains("# мій конфіг") && text.contains("cooldown = 45"));

    // команди й ролі
    let (s, acc) = t.get("/api/access").await;
    assert_eq!(s, 200);
    assert_eq!(acc["commands"].as_array().unwrap().len(), 12);
    assert_eq!(acc["limits"]["all"]["cooldown"], 45);
    let (s, e) = t.post("/api/access", json!({"commands": {"now": {"names": ["!скіп"]}}})).await;
    assert_eq!(s, 400);
    assert!(e["error"].as_str().unwrap().contains("уже зайнято"));
    let (s, e) = t.post("/api/access", json!({"commands": {"now": {"names": ["два слова"]}}})).await;
    assert_eq!(s, 400, "{e}");
    let (s, acc) = t.post("/api/access", json!({"commands": {"now": {"names": ["Шоце", "!song"], "roles": ["subs", "vip"]}}, "limits": {"subs": {"custom": true, "max_per_user": 5}}, "max_queue": 0})).await;
    assert_eq!(s, 200);
    let now = acc["commands"].as_array().unwrap().iter().find(|c| c["key"] == "now").unwrap();
    assert_eq!(now["names"], json!(["!шоце", "!song"]));
    assert_eq!(now["roles"], json!(["subs", "vip"]));
    assert_eq!(acc["limits"]["subs"]["max_per_user"], 5);
    assert_eq!(acc["max_queue"], 1);

    // підказка !музика: власний текст і приховані команди
    let (_, acc) = t.get("/api/access").await;
    assert!(acc["help"]["preview"].as_str().unwrap().starts_with("🎵 pipabobik: !трек назва"));
    let (s, acc) = t.post("/api/access", json!({"help": {"text": "Привіт! {команди} · донат на ЗСУ", "hidden": ["wrong", "nope"]}})).await;
    assert_eq!(s, 200);
    let p = acc["help"]["preview"].as_str().unwrap();
    assert!(p.starts_with("Привіт! !трек назва") && p.ends_with("донат на ЗСУ") && !p.contains("!нетой"), "{p}");
    assert_eq!(acc["help"]["hidden"], json!(["wrong"]));
    let (_, acc) = t.post("/api/access", json!({"help": {"text": "  "}})).await;
    assert_eq!(acc["help"]["text"], "🎵 pipabobik: {команди}");

    // віджет завжди рожевий
    let (s, _) = t.post("/api/settings", json!({"widget": {"pink": true}})).await;
    assert_eq!(s, 200);
    let (_, st) = t.get("/api/state").await;
    assert_eq!(st["widget"]["pink"], true);

    // фонові плейлисти
    let (s, pl) = t.post("/api/playlists", json!({"action": "add", "link": "https://music.youtube.com/playlist?list=PLtest123", "name": "Тест", "select": true})).await;
    assert_eq!(s, 200);
    assert_eq!(pl["active"], "PLtest123");
    assert!(pl["items"].as_array().unwrap().iter().any(|p| p["name"] == "Тест"));
    let (s, _) = t.post("/api/playlists", json!({"action": "add", "link": "https://example.com"})).await;
    assert_eq!(s, 400);
    let (_, pl) = t.post("/api/playlists", json!({"action": "remove", "id": "PLtest123"})).await;
    assert_eq!(pl["active"], "LM");

    // помилки й оновлення
    let (s, er) = t.post("/api/errors", json!({"action": "seen"})).await;
    assert_eq!(s, 200);
    assert_eq!(er["unseen"], 0);
    let (s, up) = t.get("/api/update").await;
    assert_eq!(s, 200);
    assert_eq!(up["current_known"], true);

    // сторінки
    for p in ["/widget", "/widget/classic", "/static/fonts/FixelDisplay-Bold.woff2", "/panel", "/app", "/setup", "/mini", "/favicon.ico", "/"] {
        let r = t.http.get(format!("{}{p}", t.base)).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200, "{p}");
    }
}

#[test]
fn covers_only_from_youtube() {
    use pipabobik_core::server::cover_allowed;
    assert!(cover_allowed("https://lh3.googleusercontent.com/abc=w544-h544"));
    assert!(cover_allowed("https://i.ytimg.com/vi/x/hqdefault.jpg"));
    assert!(!cover_allowed("http://i.ytimg.com/vi/x/hqdefault.jpg"));
    assert!(!cover_allowed("https://evil.com/?i.ytimg.com"));
    assert!(!cover_allowed("https://ytimg.com.evil.com/a.jpg"));
    assert!(!cover_allowed("file:///etc/passwd"));
}

#[tokio::test(flavor = "multi_thread")]
async fn first_run_opens_setup() {
    let t = T::new("[twitch]\nchannel = \"\"\n").await;
    let r = t.http.get(format!("{}/app?key={}", t.base, t.key)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 307);
    assert!(r.headers()["location"].to_str().unwrap().starts_with("/setup?key="));
    let (s, e) = t.post("/api/setup", json!({"channel": "x y", "client_id": "abcdefghijk", "client_secret": "s"})).await;
    assert_eq!(s, 400, "{e}");
    let (_, st) = t.get("/api/setup").await;
    assert_eq!(st["configured"], false);
    assert!(st["redirect"].as_str().unwrap().ends_with("/auth/callback"));
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_messages_are_editable() {
    let t = T::new(CONFIGURED).await;
    let (s, v) = t.get("/api/messages").await;
    assert_eq!(s, 200);
    let skip = v["items"].as_array().unwrap().iter().find(|i| i["key"] == "skip").unwrap().clone();
    assert_eq!(skip["preview"], "⏭ скіпнуто Go_A — Шум");
    let (_, v) = t.post("/api/messages", json!({"messages": {"skip": "пропускаємо {трек} 🙃", "нема": "x"}})).await;
    let skip = v["items"].as_array().unwrap().iter().find(|i| i["key"] == "skip").unwrap().clone();
    assert_eq!(skip["preview"], "пропускаємо Go_A — Шум 🙃");
    assert!(std::fs::read_to_string(t.bot.cfg.read().unwrap().path.clone()).unwrap().contains("пропускаємо {трек}"));
    // порожній — знову стандартний
    let (_, v) = t.post("/api/messages", json!({"messages": {"skip": ""}})).await;
    let skip = v["items"].as_array().unwrap().iter().find(|i| i["key"] == "skip").unwrap().clone();
    assert_eq!(skip["preview"], "⏭ скіпнуто Go_A — Шум");
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_volume_and_widget_transparency_are_settings() {
    let t = T::new(CONFIGURED).await;
    let (s, _) = t.post("/api/settings", json!({"player": {"volume": 140}, "widget": {"transparency": 35}})).await;
    assert_eq!(s, 200);
    let (_, v) = t.get("/api/settings").await;
    assert_eq!(v["player"]["volume"], 100); // не більше 100
    assert_eq!(v["widget"]["transparency"], 35);
    let (_, st) = t.get("/api/state").await;
    assert_eq!(st["widget"]["transparency"], 35);
}
