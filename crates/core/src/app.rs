//! Збирає все докупи: Twitch-чат, черга, плеєр, веб-сервер, стеження за OBS.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::watch;

use crate::commands::{Commands, Say};
use crate::config::{set_values, Config};
use crate::errors::{ErrorLayer, ERRORS};
use crate::jukebox::{BoxFut, FilterFn, Jukebox, SayFn};
use crate::music::{find_deno, MusicSource};
use crate::paths;
use crate::player::Player;
use crate::playlists::{playlist_id, Playlists};
use crate::rufilter::RuFilter;
use crate::system::{self, ObsWatch};
use crate::track::{now_secs, Track};
use crate::twitch::{TwitchAuth, TwitchChat};
use crate::updater::{self, Updater};

/// Журнал у data/pipabobik.log (3 останні дні) + вкладка «Помилки».
pub fn setup_logging(verbose: bool, console: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let _ = std::fs::create_dir_all(paths::data_dir());
    let level = if verbose { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::new(format!("{level},hyper=warn,hyper_util=warn,reqwest=warn,tungstenite=warn,tokio_tungstenite=warn,axum=warn,tower_http=warn"));
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("pipabobik")
        .filename_suffix("log")
        .max_log_files(3)
        .build(paths::data_dir())
        .ok();
    let (file_layer, guard) = match appender {
        Some(a) => {
            let (nb, g) = tracing_appender::non_blocking(a);
            (Some(tracing_subscriber::fmt::layer().with_ansi(false).with_target(true).with_writer(nb)), Some(g))
        }
        None => (None, None),
    };
    let console_layer = console.then(|| tracing_subscriber::fmt::layer().with_target(false));
    let _ = tracing_subscriber::registry().with(filter).with(file_layer).with(console_layer).with(ErrorLayer).try_init();
    guard
}

/// Відкриває посилання в браузері за замовчуванням.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).creation_flags(crate::proc::CREATE_NO_WINDOW).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Ключ застосунку: без нього сторінки не можуть керувати ботом.
pub fn load_key() -> String {
    let p = paths::data_dir().join("panel.key");
    if let Ok(k) = std::fs::read_to_string(&p) {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return k;
        }
    }
    let _ = std::fs::create_dir_all(paths::data_dir());
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let k: String = (0..16).map(|_| A[rand::random_range(0..A.len())] as char).collect();
    let _ = std::fs::write(&p, &k);
    k
}

#[derive(Debug, Clone, Default)]
pub struct BotOptions {
    pub obs_mode: bool,
    pub live: bool,
    pub paused: bool,
    pub restarted: bool,
    /// Інші аргументи командного рядка (передаються новому екземпляру при перезапуску).
    pub extra: Vec<String>,
}

pub struct Bot {
    pub cfg: Arc<RwLock<Config>>,
    pub http: reqwest::Client,
    pub auth: Arc<TwitchAuth>,
    pub music: Arc<MusicSource>,
    pub player: Arc<Player>,
    pub jukebox: Arc<Jukebox>,
    pub playlists: Mutex<Playlists>,
    pub updates: Arc<Updater>,
    pub chat: Arc<TwitchChat>,
    pub commands: Arc<Commands>,
    pub rufilter: Arc<RuFilter>,
    pub key: String,
    pub boot: String,
    pub opts: BotOptions,
    pub obs: ObsWatch,
    obs_live: Mutex<Option<bool>>, // None — бот запущено не з OBS
    google_login: Mutex<Option<bool>>,
    shutdown: watch::Sender<bool>,
    greeted: Mutex<f64>,
}

impl Bot {
    pub fn new(cfg: Config, opts: BotOptions) -> Arc<Self> {
        let http = reqwest::Client::builder().gzip(true).connect_timeout(Duration::from_secs(10)).build().expect("http");
        let port = cfg.port();
        let auth = Arc::new(TwitchAuth::new(
            &cfg.str("twitch", "client_id"),
            &cfg.str("twitch", "client_secret"),
            paths::data_dir().join("twitch_token.json"),
            format!("http://localhost:{port}/auth/callback"),
            http.clone(),
        ));
        let music = Arc::new(MusicSource::new(&cfg.str("google", "cookies_from_browser"), &cfg.str("google", "cookies_file"), http.clone()));
        let player = Arc::new(Player::new(
            &cfg.str("player", "mpv_path"),
            cfg.int("player", "volume"),
            cfg.raw["player"]["normalize"].as_bool().unwrap_or(true),
            &cfg.str("player", "audio_device"),
            cfg.raw["player"]["show_window"].as_bool().unwrap_or(true),
        ));
        let playlists = Playlists::load(paths::data_dir().join("playlists.json"), &cfg.str("google", "fallback_playlist"));
        let rufilter = Arc::new(RuFilter::new(paths::data_dir().join("artist_country.json"), &cfg.str_list("rules", "russian_artists"), http.clone()));
        let chat = Arc::new(TwitchChat::new(auth.clone(), http.clone(), &cfg.channel()));
        let announce = cfg.bool("chat", "announce_tracks");
        let cfg = Arc::new(RwLock::new(cfg));
        let jukebox = Jukebox::new(cfg.clone(), music.clone(), player.clone(), paths::data_dir());
        jukebox.announce.store(announce, std::sync::atomic::Ordering::Relaxed);
        let chat2 = chat.clone();
        let say: Say = Arc::new(move |text: String, reply: Option<String>| -> BoxFut<'static, ()> {
            let c = chat2.clone();
            Box::pin(async move { c.say(&text, reply.as_deref()).await })
        });
        let commands = Commands::new(cfg.clone(), jukebox.clone(), music.clone(), say);
        let bot = Arc::new(Self {
            cfg,
            updates: Arc::new(Updater::new(http.clone())),
            http,
            auth,
            music,
            player,
            jukebox,
            playlists: Mutex::new(playlists),
            chat,
            commands,
            rufilter,
            key: load_key(),
            boot: format!("{}-{}", std::process::id(), now_secs() as u64),
            opts,
            obs: ObsWatch::default(),
            obs_live: Mutex::new(None),
            google_login: Mutex::new(None),
            shutdown: watch::channel(false).0,
            greeted: Mutex::new(0.0),
        });
        bot.wire();
        bot
    }

    /// Зв'язки між частинами: фільтр, повідомлення в чат, оновлення стану застосунку.
    fn wire(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let f: FilterFn = Arc::new(move |t: Track| -> BoxFut<'static, Option<String>> {
            let me = me.clone();
            Box::pin(async move {
                match me.upgrade() {
                    Some(b) => b.ru_check(t).await,
                    None => None,
                }
            })
        });
        *self.jukebox.content_filter.lock().unwrap() = Some(f);
        let chat = self.chat.clone();
        let say: SayFn = Arc::new(move |text: String| -> BoxFut<'static, ()> {
            let c = chat.clone();
            Box::pin(async move { c.say(&text, None).await })
        });
        *self.jukebox.say.lock().unwrap() = Some(say);
        // нова помилка — застосунок одразу показує її на вкладці «Помилки»
        let jb = Arc::downgrade(&self.jukebox);
        ERRORS.set_on_change(move || {
            if let Some(j) = jb.upgrade() {
                j.changed();
            }
        });
        let jb = Arc::downgrade(&self.jukebox);
        self.updates.set_on_change(move || {
            if let Some(j) = jb.upgrade() {
                j.changed();
            }
        });
    }

    pub fn port(&self) -> u16 {
        self.cfg.read().unwrap().port()
    }

    pub fn configured(&self) -> bool {
        let c = self.cfg.read().unwrap();
        !c.channel().is_empty() && !c.str("twitch", "client_id").is_empty() && !c.str("twitch", "client_secret").is_empty()
    }

    pub fn obs_live(&self) -> Option<bool> {
        *self.obs_live.lock().unwrap()
    }

    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn health(&self) -> Value {
        let login = self.auth.is_authorized().then(|| self.auth.login()).flatten();
        json!({
            "twitch": login,
            "channel": self.cfg.read().unwrap().channel(),
            "chat": self.chat.is_connected(),
            "mpv": self.player.available(),
            "google": *self.google_login.lock().unwrap(),
            "obs_live": self.obs_live(),
        })
    }

    pub fn payload(&self) -> Value {
        let mut st = self.jukebox.state();
        let obs = self.obs_live();
        st["health"] = self.health();
        st["errors"] = ERRORS.summary();
        st["update"] = self.updates.summary();
        st["streaming"] = json!(obs == Some(true));
        st["obs_restart"] = json!(obs == Some(false)); // оновлення може перезапустити OBS (трансляція не йде)
        st["boot"] = json!(self.boot); // змінюється після перезапуску — застосунок оновить сторінку
        st["fallback"] = json!(playlist_id(&self.cfg.read().unwrap().str("google", "fallback_playlist")).unwrap_or_default());
        st
    }

    /// Записує зміни в config.toml (коментарі й порядок зберігаються).
    pub fn save_config(&self, updates: &[(String, Map<String, Value>)]) {
        let path = self.cfg.read().unwrap().path.clone();
        let list: Vec<(&str, Value)> = updates.iter().map(|(s, m)| (s.as_str(), Value::Object(m.clone()))).collect();
        if let Err(e) = set_values(&path, &list) {
            tracing::error!("Не вдалося зберегти config.toml: {e}");
        }
    }

    /// Застосовує зміни з застосунку, які не підхоплюються самі.
    pub fn apply_settings(&self, updates: &[(String, Map<String, Value>)]) {
        for (sec, m) in updates {
            match sec.as_str() {
                "chat" => self.jukebox.announce.store(self.cfg.read().unwrap().bool("chat", "announce_tracks"), std::sync::atomic::Ordering::Relaxed),
                "player" => {
                    if let Some(v) = m.get("normalize") {
                        self.player.normalize.store(v.as_bool().unwrap_or(true), std::sync::atomic::Ordering::Relaxed); // з наступного треку
                    }
                }
                "google" => self.jukebox.reset_fallback(),
                "rules" => {
                    if m.contains_key("russian_artists") {
                        self.rufilter.set_extra(&self.cfg.read().unwrap().str_list("rules", "russian_artists"));
                    }
                }
                _ => {}
            }
        }
        let keys: Vec<String> = updates.iter().flat_map(|(s, m)| m.keys().map(move |k| format!("{s}.{k}"))).collect();
        tracing::info!("Налаштування оновлено з застосунку: {}", keys.join(", "));
        self.jukebox.changed();
    }

    /// Аргументи для нового екземпляра: той самий режим, музика грає, якщо грала.
    pub fn restart_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        if self.opts.obs_mode {
            args.push("--obs".into());
        }
        args.extend(self.opts.extra.iter().cloned());
        if self.jukebox.is_active() {
            if self.opts.obs_mode {
                args.push("--live".into());
            }
        } else if !self.opts.obs_mode {
            args.push("--paused".into());
        }
        args
    }

    /// Запускає новий екземпляр бота і закриває цей. Черга зберігається.
    pub fn restart(&self) {
        paths::touch_restart_mark();
        let exe = std::env::current_exe().unwrap_or_else(|_| paths::root().join(updater::EXE));
        let mut args = vec!["--bot".to_string()];
        args.extend(self.restart_args());
        args.push("--restart".into());
        tracing::info!("Перезапуск: {}", args.join(" "));
        if let Err(e) = crate::proc::spawn_detached(&exe, &args, paths::root()) {
            tracing::error!("Не вдалося перезапустити: {e}");
            return;
        }
        self.shutdown();
    }

    /// Завантажує нову версію і передає її новому PipaBobik.exe; той перезапустить OBS чи бота.
    pub async fn install_update(self: &Arc<Self>) {
        let up = &self.updates;
        let st = up.state();
        if st == "downloading" || st == "installing" {
            return;
        }
        let new = match up.stage().await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Не вдалося завантажити оновлення: {e}");
                up.set("error", &format!("Не вдалося завантажити оновлення: {e}"));
                return;
            }
        };
        up.set("installing", "");
        // OBS перезапускаємо лише коли точно знаємо, що трансляція не йде
        let (mut obs_pid, mut obs_exe) = (0u32, String::new());
        if self.obs_live() == Some(false) && self.obs.running() {
            if let (Some(pid), Some(exe)) = (self.obs.pid(), self.obs.exe()) {
                obs_pid = pid;
                obs_exe = exe;
            }
        }
        let latest = up.latest();
        let tag = latest["tag"].as_str().unwrap_or("").to_string();
        let message = latest["message"].as_str().unwrap_or("").to_string();
        let args = vec![
            "--apply-update".to_string(),
            "--staged".into(),
            new.to_string_lossy().into_owned(),
            "--target".into(),
            paths::root().to_string_lossy().into_owned(),
            "--tag".into(),
            tag.clone(),
            "--message".into(),
            message.clone(),
            "--bot-pid".into(),
            std::process::id().to_string(),
            "--obs-pid".into(),
            obs_pid.to_string(),
            "--obs-exe".into(),
            obs_exe,
            "--bot-args".into(),
            serde_json::to_string(&self.restart_args()).unwrap_or_else(|_| "[]".into()),
        ];
        tracing::info!("Оновлення до {tag}: {message} (OBS {})", if obs_pid != 0 { "перезапуститься" } else { "не чіпаємо" });
        tokio::time::sleep(Duration::from_millis(500)).await; // застосунок встигає показати «Оновлюю…»
        paths::touch_restart_mark();
        if let Err(e) = crate::proc::spawn_detached(&new.join(updater::EXE), &args, &new) {
            tracing::error!("Не вдалося запустити оновлення: {e}");
            up.set("error", &format!("Не вдалося запустити оновлення: {e}"));
            return;
        }
        self.shutdown();
    }

    async fn ru_check(&self, track: Track) -> Option<String> {
        if !self.cfg.read().unwrap().raw["rules"]["block_russian"].as_bool().unwrap_or(true) {
            return None;
        }
        self.rufilter.check(&track.title, &track.artist, &track.album).await
    }

    /// Діагностика для майстра налаштування (замість check.bat у v1).
    pub async fn diagnostics(&self) -> Value {
        let mut items: Vec<Value> = Vec::new();
        let mut add = |ok: bool, text: String, hint: &str| items.push(json!({"ok": ok, "text": text, "hint": hint}));
        let (channel, cid, secret) = {
            let c = self.cfg.read().unwrap();
            (c.channel(), c.str("twitch", "client_id"), c.str("twitch", "client_secret"))
        };
        add(paths::config_path().exists(), "config.toml".into(), "збережіть налаштування в майстрі");
        add(!channel.is_empty(), format!("Канал: {}", if channel.is_empty() { "—" } else { &channel }), "вкажіть нік каналу");
        add(!cid.is_empty() && !secret.is_empty(), "Застосунок Twitch (Client ID і Secret)".into(), "див. крок 2 майстра");
        let login = self.auth.login();
        add(self.auth.is_authorized(), format!("Вхід у Twitch{}", login.map(|l| format!(": {l}")).unwrap_or_default()), "натисніть «Увійти в Twitch»");
        add(self.player.available(), format!("Програвач mpv: {}", self.player.mpv.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "не знайдено".into())), "winget install -e --id shinchiro.mpv, потім перезапустіть pipabobik");
        let ytdlp = self.music.ensure_ytdlp().await;
        add(ytdlp.is_some(), format!("yt-dlp: {}", ytdlp.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "не знайдено".into())), "winget install -e --id yt-dlp.yt-dlp");
        let deno = find_deno();
        add(deno.is_some(), format!("Deno (потрібен yt-dlp для YouTube): {}", deno.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "не знайдено".into())), "winget install -e --id DenoLand.Deno");
        let google = self.music.google_login().await;
        *self.google_login.lock().unwrap() = google;
        add(google != Some(false), match google { None => "Вхід у Google не налаштовано (без треків 18+ і вподобаних)".into(), Some(true) => "Вхід у Google знайдено".into(), Some(false) => "Вхід у Google не знайдено".into() }, "увійдіть у music.youtube.com у Firefox");
        let found = self.music.search("Океан Ельзи Обійми", true).await;
        add(!found.is_empty(), format!("Пошук YouTube Music: {}", found.first().map(|t| t.label()).unwrap_or_else(|| "не працює".into())), "перевірте інтернет");
        if let Some(t) = found.first() {
            let s = self.music.resolve_stream(t).await;
            add(s.is_some(), "Отримання аудіо через yt-dlp".into(), "оновіть yt-dlp і перевірте, що встановлено Deno");
        }
        let ok = items.iter().all(|i| i["ok"].as_bool().unwrap_or(false));
        json!({"ok": ok, "items": items})
    }

    pub async fn run(self: Arc<Self>) -> i32 {
        let port = self.port();
        // після перезапуску старий екземпляр ще кілька секунд закривається — чекаємо на порт
        let deadline = std::time::Instant::now() + Duration::from_secs(if self.opts.restarted { 25 } else { 0 });
        let listener = loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => break l,
                Err(_) if std::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(500)).await,
                Err(_) => {
                    tracing::info!("pipabobik вже запущено (порт {port} зайнятий) — виходжу");
                    return 0;
                }
            }
        };
        if self.opts.restarted {
            tracing::info!("pipabobik перезапущено");
        }
        let _ = std::fs::remove_file(paths::restart_mark());
        system::kill_children_with_me(); // mpv/yt-dlp закриються разом із ботом, навіть при аварійному завершенні
        if system::kill_orphan_players() > 0 {
            tracing::info!("Закрито mpv, що залишився від попереднього запуску");
        }
        updater::cleanup_after_update();

        let base = format!("http://localhost:{port}");
        tracing::info!("🎀 pipabobik {} запущено · віджет: {base}/widget · панель: {base}/panel?key={}", updater::version(), self.key);
        if !self.player.available() {
            tracing::error!("mpv не знайдено! Встановіть: winget install -e --id shinchiro.mpv");
        }
        if !self.configured() {
            tracing::error!("Бот ще не налаштовано: відкрийте застосунок pipabobik — там майстер налаштування");
        } else if !self.auth.validate().await {
            if !self.auth.has_refresh() {
                // перший запуск — відкриваємо вхід; надалі лише підказка в застосунку
                tracing::warn!("Потрібно увійти в Twitch: відкриваю {base}/auth");
                open_url(&format!("{base}/auth"));
            } else {
                tracing::warn!("Не вдалося перевірити вхід Twitch — увійдіть заново: {base}/auth");
            }
        }

        let mut shutdown_rx = self.shutdown.subscribe();
        let server = {
            let mut rx = self.shutdown.subscribe();
            tokio::spawn(crate::server::serve(self.clone(), listener, async move {
                let _ = rx.wait_for(|s| *s).await;
            }))
        };
        let commands = self.commands.clone();
        let handler: crate::twitch::Handler = Arc::new(move |m| {
            let c = commands.clone();
            tokio::spawn(async move { c.handle(m).await });
        });
        let mut tasks = vec![
            tokio::spawn(self.chat.clone().run(handler)),
            tokio::spawn(self.jukebox.clone().run()),
            tokio::spawn(self.clone().validate_loop()),
            tokio::spawn(self.clone().watch_obs()),
            tokio::spawn(self.clone().cpu_report()),
        ];
        if self.opts.live || !(self.opts.obs_mode || self.opts.paused) {
            self.jukebox.activate();
        }
        {
            let me = self.clone();
            tasks.push(tokio::spawn(async move {
                me.music.warmup().await; // перший пошук і перший трек — без очікування
                let g = me.music.google_login().await;
                *me.google_login.lock().unwrap() = g;
                me.jukebox.changed();
            }));
        }
        if self.cfg.read().unwrap().raw["update"]["auto_check"].as_bool().unwrap_or(true) {
            tasks.push(tokio::spawn(self.updates.clone().run_loop())); // нова версія на GitHub — застосунок запропонує оновитись
        }

        let _ = shutdown_rx.wait_for(|s| *s).await;
        tracing::info!("Завершення роботи…");
        // спершу зупиняємо відтворення: поточний трек повертається на початок черги
        self.jukebox.deactivate().await;
        self.jukebox.save();
        for t in &tasks {
            t.abort();
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), server).await;
        0
    }

    async fn validate_loop(self: Arc<Self>) {
        // Twitch вимагає перевіряти токен щогодини
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            self.auth.validate().await;
        }
    }

    async fn greet(self: Arc<Self>) {
        let mut rx = self.chat.connected_rx();
        let _ = rx.wait_for(|c| *c).await;
        let text = format!("🎀 pipabobik на місці! Замовляй музику: {} назва · що грає: {}", self.commands.first("request"), self.commands.first("now"));
        self.chat.say(&text, None).await;
    }

    /// Стежить за OBS у будь-якому режимі запуску.
    ///
    /// * файл data/obs_state пише Lua-скрипт: live / idle / exit;
    /// * якщо OBS був відкритий і зник — бот зупиняє музику (черга зберігається) і закривається.
    ///   Бот, запущений без OBS (з ярлика), реагує лише на зміни після свого старту.
    async fn watch_obs(self: Arc<Self>) {
        let state_file: PathBuf = paths::data_dir().join("obs_state");
        let started = std::time::SystemTime::now() - Duration::from_secs(1);
        let mut last: Option<String> = None;
        let mut obs_seen = self.opts.obs_mode;
        let mut missing = 0;
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            tick += 1;
            let mut state = String::new();
            if let Ok(meta) = std::fs::metadata(&state_file) {
                let fresh = meta.modified().map(|m| m >= started).unwrap_or(false);
                if self.opts.obs_mode || fresh {
                    state = std::fs::read_to_string(&state_file).unwrap_or_default().split_whitespace().next().unwrap_or("").to_string();
                }
            }
            if Some(&state) != last.as_ref() {
                if !state.is_empty() {
                    *self.obs_live.lock().unwrap() = Some(state == "live");
                    self.jukebox.changed();
                }
                match state.as_str() {
                    "live" => {
                        obs_seen = true;
                        tracing::info!("OBS: трансляцію розпочато");
                        self.jukebox.activate();
                        let greet = self.cfg.read().unwrap().raw["chat"]["greet_on_stream_start"].as_bool().unwrap_or(true);
                        let mut g = self.greeted.lock().unwrap();
                        if greet && now_secs() - *g > 1800.0 {
                            *g = now_secs();
                            tokio::spawn(self.clone().greet());
                        }
                    }
                    "idle" if last.as_deref() == Some("live") => {
                        tracing::info!("OBS: трансляцію завершено — музику на паузу, черга збережена");
                        self.jukebox.deactivate().await;
                    }
                    "exit" if last.is_some() => {
                        tracing::info!("OBS закрито — зупиняю музику і закриваюсь");
                        self.shutdown();
                        return;
                    }
                    _ => {}
                }
                last = Some(state);
            }
            if tick % 5 == 0 {
                let me = self.clone();
                let running = tokio::task::spawn_blocking(move || me.obs.running()).await.unwrap_or(true);
                if running {
                    obs_seen = true;
                    missing = 0;
                } else if obs_seen {
                    missing += 1;
                    if missing >= 2 {
                        tracing::info!("OBS закрито — зупиняю музику і закриваюсь");
                        self.shutdown();
                        return;
                    }
                }
            }
        }
    }

    /// Раз на 10 хвилин пише в журнал, скільки процесора беруть бот і програвач.
    async fn cpu_report(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let mut pids = vec![std::process::id()];
            if let Some(p) = self.player.pid() {
                pids.push(p);
            }
            let vals = system::cpu_percent(&pids, 10).await;
            let player = vals.get(1).map(|v| format!("{v:.1}%")).unwrap_or_else(|| "не грає".into());
            tracing::info!("Навантаження на процесор: бот {:.1}%, програвач {player}", vals.first().copied().unwrap_or(0.0));
        }
    }
}
