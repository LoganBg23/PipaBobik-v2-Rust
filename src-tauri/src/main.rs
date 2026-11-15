//! PipaBobik.exe — один файл на все:
//!
//!   PipaBobik.exe                — вікно застосунку (запускає бота, якщо він ще не працює);
//!   PipaBobik.exe --bot [--obs] [--live] [--paused] [-v]
//!                                — сам бот без вікна (так його запускає скрипт OBS);
//!   PipaBobik.exe --update …     — вікно «Оновлення pipabobik» (запускає бот кнопкою «Оновити»);
//!   PipaBobik.exe --apply-update … — те саме для оновлення зі старих версій (2.0.1, 2.0.2);
//!   PipaBobik.exe --uninstall    — видалити pipabobik (ярлик «Видалити pipabobik»).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pipabobik_core::app::{open_url, setup_logging, Bot, BotOptions};
use pipabobik_core::config::{load_config, Config};
use pipabobik_core::system::ObsWatch;
use pipabobik_core::uninstall::{self, ask, Answer, Buttons};
use pipabobik_core::{paths, proc, updater};
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::window::Color;
use tauri::{Manager, PhysicalPosition, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    if has("--version") {
        println!("PipaBobik {}", updater::version());
        return;
    }
    if has("--update") || has("--apply-update") {
        std::process::exit(run_update_window(&args));
    }
    if has("--uninstall-run") {
        std::process::exit(uninstall_run(&args));
    }
    if has("--uninstall") {
        std::process::exit(uninstall());
    }
    let bot_flags = ["--bot", "--obs", "--live", "--paused", "--restart"];
    if args.iter().any(|a| bot_flags.contains(&a.as_str())) {
        std::process::exit(run_bot(&args));
    }
    run_panel();
}

fn arg_value(args: &[String], name: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default()
}

// ── вікно «Оновлення pipabobik» ──

fn update_job(args: &[String]) -> updater::UpdateJob {
    let staged = arg_value(args, "--staged");
    updater::UpdateJob {
        target: PathBuf::from(arg_value(args, "--target")),
        tag: arg_value(args, "--tag"),
        message: arg_value(args, "--message"),
        url: arg_value(args, "--url"),
        size: arg_value(args, "--size").parse().unwrap_or(0),
        bot_pid: arg_value(args, "--bot-pid").parse().unwrap_or(0),
        obs_pid: arg_value(args, "--obs-pid").parse().unwrap_or(0),
        obs_exe: arg_value(args, "--obs-exe"),
        bot_args: serde_json::from_str(&arg_value(args, "--bot-args")).unwrap_or_default(),
        staged: (!staged.is_empty()).then(|| PathBuf::from(staged)),
    }
}

/// Оновлення з вікном, де видно кожен крок. Якщо вікно не відкрилося — оновлюємо без нього.
fn run_update_window(args: &[String]) -> i32 {
    let job = update_job(args);
    if job.target.as_os_str().is_empty() {
        return 2;
    }
    let from_temp = job.staged.is_none(); // копія з TEMP — після роботи видаляє себе
    let events: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let ok = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let title = format!("pipabobik → {}", job.tag.trim_start_matches('v'));
    let meta = if job.message.is_empty() { format!("Нова версія {}", job.tag) } else { job.message.clone() };
    events.lock().unwrap().push(serde_json::json!({"type": "meta", "text": meta}));

    let (ev, ok2, fin2) = (events.clone(), ok.clone(), finished.clone());
    let ctx = tauri::generate_context!();
    let built = tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            let ev_nav = ev.clone();
            let h_nav = handle.clone();
            let fin_nav = fin2.clone();
            let win = WebviewWindowBuilder::new(app, "update", WebviewUrl::App("update.html".into()))
                .title(&title)
                .inner_size(440.0, 420.0)
                .resizable(false)
                .center()
                .always_on_top(true)
                .focused(true)
                .background_color(Color(0x1c, 0x07, 0x16, 0xff))
                // окрема папка WebView2: старе вікно застосунку ще може закриватися
                .data_directory(std::env::temp_dir().join("pipabobik-update-webview"))
                .initialization_script(BRIDGE)
                .on_navigation(move |url| {
                    if let Some(cmd) = bridge_cmd(url) {
                        match cmd.as_str() {
                            "ready" => {
                                if let Some(w) = h_nav.get_webview_window("update") {
                                    for e in ev_nav.lock().unwrap().iter() {
                                        let _ = w.eval(format!("window.upd && window.upd({e})"));
                                    }
                                }
                            }
                            "close" if fin_nav.load(Ordering::SeqCst) => h_nav.exit(0),
                            _ => {}
                        }
                        return false;
                    }
                    is_local(url)
                })
                .build()?;
            let ev_job = ev.clone();
            let w2 = win.clone();
            let (ok3, fin3) = (ok2.clone(), fin2.clone());
            std::thread::spawn(move || {
                let emit = move |e: updater::Ev| {
                    let j = e.json();
                    ev_job.lock().unwrap().push(j.clone());
                    let _ = w2.eval(format!("window.upd && window.upd({j})"));
                };
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
                let http = reqwest_client();
                let good = rt.block_on(updater::run_job(job, http, emit));
                ok3.store(good, Ordering::SeqCst);
                fin3.store(true, Ordering::SeqCst);
                // успіх — вікно закривається саме за кілька секунд; помилку людина закриває кнопкою
                if good {
                    std::thread::sleep(Duration::from_secs(4));
                    handle.exit(0);
                }
            });
            Ok(())
        })
        .on_window_event(move |window, event| {
            // поки йде оновлення, вікно не закриваємо — інакше можна лишитися без бота
            if let WindowEvent::CloseRequested { api, .. } = event {
                if !finished.load(Ordering::SeqCst) {
                    api.prevent_close();
                    let _ = window.set_focus();
                }
            }
        })
        .build(ctx);
    match built {
        Ok(app) => app.run(|_, _| {}),
        Err(e) => {
            // без вікна: оновлюємо мовчки, кроки — у data\update.log
            let _ = e;
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
            let job = update_job(args);
            let good = rt.block_on(updater::run_job(job, reqwest_client(), |_| {}));
            ok.store(good, Ordering::SeqCst);
        }
    }
    if from_temp {
        uninstall::delete_self_later();
    }
    if ok.load(Ordering::SeqCst) { 0 } else { 1 }
}

fn reqwest_client() -> pipabobik_core::Http {
    pipabobik_core::http_client()
}

/// config.toml із коментарями: при першому запуску копіюємо зразок.
fn ensure_config() -> PathBuf {
    let path = paths::config_path();
    let example = paths::root().join("config.example.toml");
    if !path.exists() && example.exists() {
        let _ = std::fs::copy(&example, &path);
    }
    path
}

fn run_bot(args: &[String]) -> i32 {
    let has = |f: &str| args.iter().any(|a| a == f);
    let _guard = setup_logging(has("-v") || has("--verbose"), cfg!(debug_assertions));
    let path = ensure_config();
    let cfg = match load_config(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Помилка в config.toml: {e}");
            return 1;
        }
    };
    let known = ["--bot", "--obs", "--live", "--paused", "--restart"];
    let opts = BotOptions {
        obs_mode: has("--obs"),
        live: has("--live"),
        paused: has("--paused"),
        restarted: has("--restart"),
        extra: args.iter().filter(|a| !known.contains(&a.as_str())).cloned().collect(),
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio");
    rt.block_on(async move { Bot::new(cfg, opts).run().await })
}

// ── вікно застосунку ──

fn port() -> u16 {
    load_config(&paths::config_path()).map(|c| c.port()).unwrap_or_else(|_| Config::default().port())
}

fn bot_running(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(400)).is_ok()
}

fn read_key() -> String {
    std::fs::read_to_string(paths::data_dir().join("panel.key")).map(|k| k.trim().to_string()).unwrap_or_default()
}

fn start_bot(obs: &ObsWatch) {
    let exe = std::env::current_exe().unwrap_or_else(|_| paths::root().join(updater::EXE));
    // OBS уже відкритий — бот слухає його трансляцію; інакше чекає «Увімкнути»
    let mode = if obs.running() { "--obs" } else { "--paused" };
    let _ = proc::spawn_detached(&exe, &["--bot".into(), mode.into()], paths::root());
}

/// Просить бота завершитися (музика зупиняється, черга зберігається).
fn shutdown_bot(port: u16, key: &str) {
    let body = r#"{"action":"shutdown"}"#;
    if let Ok(mut s) = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2)) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
        let req = format!(
            "POST /api/control HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nX-Pipabobik-Key: {key}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = s.write_all(req.as_bytes());
        let mut buf = [0u8; 256];
        let _ = s.read(&mut buf);
    }
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && bot_running(port) {
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn is_local(url: &Url) -> bool {
    matches!(url.scheme(), "tauri" | "about" | "data")
        || matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "tauri.localhost"))
}

// ── вікно застосунку, режим «поверх усіх вікон» і міні-плеєр ──

/// Міст між сторінкою і вікном: сторінка «переходить» на службову адресу, а ми перехоплюємо
/// цей перехід і виконуємо команду (pin, mini, full, drag). Так не потрібен доступ сторінки до IPC.
const BRIDGE: &str = r#"(() => {
  const h = location.hostname;
  if (!["localhost", "127.0.0.1", "tauri.localhost"].includes(h) && location.protocol !== "tauri:") return;
  window.pipabobikWindow = (cmd) => { location.href = "https://pipabobik-window.invalid/" + cmd; };
})();"#;

fn bridge_cmd(url: &Url) -> Option<String> {
    (url.host_str() == Some("pipabobik-window.invalid")).then(|| url.path().trim_start_matches('/').to_string())
}

const MINI_W: f64 = 360.0;
const MINI_H: f64 = 104.0;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct WinPrefs {
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    mini_x: Option<i32>,
    #[serde(default)]
    mini_y: Option<i32>,
}

fn prefs_path() -> PathBuf {
    paths::data_dir().join("window.json")
}
fn load_prefs() -> WinPrefs {
    std::fs::read_to_string(prefs_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_prefs(p: &WinPrefs) {
    let _ = std::fs::create_dir_all(paths::data_dir());
    let _ = std::fs::write(prefs_path(), serde_json::to_string(p).unwrap_or_default());
}

/// Повідомляє сторінці застосунку, чи вікно закріплене поверх інших.
fn tell_pinned(w: &WebviewWindow) {
    let pinned = load_prefs().pinned;
    let _ = w.eval(format!("window.onPipabobikWindow && window.onPipabobikWindow({{pinned: {pinned}}})"));
}

/// Обробник посилань для обох вікон: службові команди, свої сторінки, решта — у браузер.
fn navigation(app: tauri::AppHandle) -> impl Fn(&Url) -> bool + Send + 'static {
    move |url: &Url| {
        if let Some(cmd) = bridge_cmd(url) {
            let a = app.clone();
            // окремий потік: команда виконується вже після того, як перехід скасовано
            std::thread::spawn(move || window_cmd(&a, &cmd));
            return false;
        }
        if is_local(url) && !url.path().starts_with("/auth") {
            return true;
        }
        open_url(url.as_str());
        false
    }
}

fn window_cmd(app: &tauri::AppHandle, cmd: &str) {
    match cmd {
        "pin" => {
            if let Some(w) = app.get_webview_window("main") {
                let on = !load_prefs().pinned;
                let _ = w.set_always_on_top(on);
                let mut p = load_prefs();
                p.pinned = on;
                save_prefs(&p);
                tell_pinned(&w);
            }
        }
        "state" => {
            if let Some(w) = app.get_webview_window("main") {
                tell_pinned(&w);
            }
        }
        "mini" => show_mini(app),
        "full" => show_full(app),
        "drag" => {
            if let Some(m) = app.get_webview_window("mini") {
                let _ = m.start_dragging();
            }
        }
        _ => {}
    }
}

/// Міні-плеєр: маленьке вікно поверх усіх у правому нижньому куті (або там, де його залишили).
fn show_mini(app: &tauri::AppHandle) {
    let mini = match app.get_webview_window("mini") {
        Some(m) => m,
        None => {
            let url = match Url::parse(&format!("http://localhost:{}/mini?key={}", port(), read_key())) {
                Ok(u) => u,
                Err(_) => return,
            };
            match WebviewWindowBuilder::new(app, "mini", WebviewUrl::External(url))
                .title("pipabobik")
                .inner_size(MINI_W, MINI_H)
                .resizable(false)
                .decorations(false)
                .always_on_top(true)
                .visible(false)
                .background_color(Color(0x2a, 0x0d, 0x22, 0xff))
                .initialization_script(BRIDGE)
                .on_navigation(navigation(app.clone()))
                .on_new_window(|url, _| {
                    open_url(url.as_str());
                    NewWindowResponse::Deny
                })
                .build()
            {
                Ok(m) => m,
                Err(_) => return,
            }
        }
    };
    let main = app.get_webview_window("main");
    let prefs = load_prefs();
    let pos = match (prefs.mini_x, prefs.mini_y) {
        (Some(x), Some(y)) => Some(PhysicalPosition::new(x, y)),
        _ => {
            // правий нижній кут робочої області (над панеллю задач) того екрана, де застосунок
            let mon = main.as_ref().and_then(|w| w.current_monitor().ok().flatten()).or_else(|| mini.primary_monitor().ok().flatten());
            mon.map(|m| {
                let wa = m.work_area();
                let sc = m.scale_factor();
                let (w, h) = ((MINI_W * sc) as i32, (MINI_H * sc) as i32);
                let gap = (16.0 * sc) as i32;
                PhysicalPosition::new(wa.position.x + wa.size.width as i32 - w - gap, wa.position.y + wa.size.height as i32 - h - gap)
            })
        }
    };
    if let Some(p) = pos {
        let _ = mini.set_position(p);
    }
    let _ = mini.show();
    let _ = mini.set_always_on_top(true);
    if let Some(w) = main {
        let _ = w.hide();
    }
}

/// Назад до повного вікна. Місце міні-плеєра запам'ятовується.
fn show_full(app: &tauri::AppHandle) {
    if let Some(m) = app.get_webview_window("mini") {
        if m.is_visible().unwrap_or(false) {
            if let Ok(p) = m.outer_position() {
                let mut pr = load_prefs();
                pr.mini_x = Some(p.x);
                pr.mini_y = Some(p.y);
                save_prefs(&pr);
            }
        }
        let _ = m.hide();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn run_panel() {
    let port = port();
    // ярлики в меню «Пуск» (і «Видалити pipabobik») та запис у «Програмах»
    std::thread::spawn(uninstall::register_if_needed);
    let closing = Arc::new(AtomicBool::new(false));
    let closing_ev = closing.clone();
    let ctx = tauri::generate_context!();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // вдруге відкрили ярлик — показуємо вже відкрите вікно
            show_full(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            let pinned = load_prefs().pinned;
            let win = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("pipabobik")
                .inner_size(480.0, 900.0)
                .min_inner_size(380.0, 560.0)
                .always_on_top(pinned)
                .background_color(Color(0x1c, 0x07, 0x16, 0xff))
                .initialization_script(BRIDGE)
                // посилання назовні (YouTube, вхід у Twitch) — у звичайному браузері
                .on_navigation(navigation(app.handle().clone()))
                .on_new_window(|url, _features| {
                    open_url(url.as_str());
                    NewWindowResponse::Deny
                })
                // сторінка завантажилась — кажемо їй, чи вікно закріплене
                .on_page_load(|w, p| {
                    if p.event() == PageLoadEvent::Finished {
                        tell_pinned(&w);
                    }
                })
                .build()?;
            let handle = app.handle().clone();
            std::thread::spawn(move || panel_worker(handle, win, port));
            Ok(())
        })
        .on_window_event(move |window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // міні-плеєр не закривається, а повертає повне вікно
                if window.label() == "mini" {
                    api.prevent_close();
                    show_full(window.app_handle());
                    return;
                }
                if closing_ev.load(Ordering::SeqCst) {
                    return;
                }
                api.prevent_close();
                let app = window.app_handle().clone();
                let closing = closing_ev.clone();
                std::thread::spawn(move || {
                    let yes = app
                        .dialog()
                        .message("Музика зупиниться, черга збережеться.")
                        .title("Закрити pipabobik?")
                        .kind(MessageDialogKind::Info)
                        .buttons(MessageDialogButtons::OkCancelCustom("Закрити".into(), "Скасувати".into()))
                        .blocking_show();
                    if yes {
                        closing.store(true, Ordering::SeqCst);
                        shutdown_bot(port, &read_key());
                        app.exit(0);
                    }
                });
            }
        })
        .build(ctx);
    match app {
        Ok(app) => app.run(|_, _| {}),
        Err(e) => edge_fallback(port, &e.to_string()),
    }
    let _ = closing;
}

/// Запускає бота (якщо треба), відкриває застосунок і стежить, чи живі бот і OBS.
fn panel_worker(app: tauri::AppHandle, win: WebviewWindow, port: u16) {
    let obs = ObsWatch::default();
    if !bot_running(port) {
        // OBS міг якраз запускати бота — даємо йому кілька секунд
        let end = Instant::now() + Duration::from_secs(6);
        while Instant::now() < end && !bot_running(port) {
            std::thread::sleep(Duration::from_millis(300));
        }
        if !bot_running(port) {
            start_bot(&obs);
        }
    }
    let end = Instant::now() + Duration::from_secs(25);
    while Instant::now() < end && !bot_running(port) {
        std::thread::sleep(Duration::from_millis(300));
    }
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && read_key().is_empty() {
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Ok(url) = Url::parse(&format!("http://localhost:{port}/app?key={}", read_key())) {
        let _ = win.navigate(url);
    }

    // OBS закрили — закриваємо й застосунок. Бот перезапускається чи оновлюється — чекаємо.
    let mut obs_seen = false;
    let mut bot_missing = 0;
    loop {
        std::thread::sleep(Duration::from_secs(3));
        if paths::restarting() {
            bot_missing = 0;
            continue;
        }
        if obs.running() {
            obs_seen = true;
        } else if obs_seen {
            app.exit(0);
            return;
        }
        if bot_running(port) {
            bot_missing = 0;
        } else {
            bot_missing += 1;
            if bot_missing >= 2 {
                app.exit(0);
                return;
            }
        }
    }
}

/// Якщо WebView2 недоступний — відкриваємо застосунок у Microsoft Edge як окреме вікно.
fn edge_fallback(port: u16, why: &str) {
    let _guard = setup_logging(false, false);
    tracing::warn!("Вікно застосунку не відкрилося ({why}) — відкриваю в Edge");
    let obs = ObsWatch::default();
    if !bot_running(port) {
        start_bot(&obs);
        let end = Instant::now() + Duration::from_secs(25);
        while Instant::now() < end && !bot_running(port) {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    let url = format!("http://localhost:{port}/app?key={}", read_key());
    #[cfg(windows)]
    {
        let mut candidates = vec![];
        for env in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
            if let Ok(b) = std::env::var(env) {
                candidates.push(PathBuf::from(b).join("Microsoft").join("Edge").join("Application").join("msedge.exe"));
            }
        }
        if let Some(edge) = candidates.into_iter().find(|p| p.is_file()) {
            let _ = std::process::Command::new(edge).arg(format!("--app={url}")).arg("--window-size=480,900").spawn();
            return;
        }
    }
    open_url(&url);
}

// ── видалення ──

const TITLE: &str = "Видалення pipabobik";

/// Крок 1: питання, зупинка бота, передача роботи копії з TEMP.
fn uninstall() -> i32 {
    let root = paths::root().to_path_buf();
    if !uninstall::looks_installed(&root) {
        ask(TITLE, &format!("Не бачу pipabobik у папці\n{}", root.display()), Buttons::Ok);
        return 1;
    }
    let sure = ask(
        TITLE,
        &format!("Видалити pipabobik з комп'ютера?\n\nПапка: {}\nБот, застосунок, ярлики й запис у «Програмах» буде видалено.", root.display()),
        Buttons::YesNo,
    );
    if sure != Answer::Yes {
        return 0;
    }
    let keep = match ask(
        TITLE,
        "Залишити налаштування й дані?\n\n«Так» — config.toml і папка data (черга, бан-лист, лайки, вхід у Twitch) залишаться, і наступне встановлення в цю папку їх підхопить.\n\n«Ні» — видалити все.",
        Buttons::YesNoCancel,
    ) {
        Answer::Yes => true,
        Answer::No => false,
        Answer::Cancel => return 0,
    };
    // зупиняємо бота (черга зберігається), вікно застосунку й програвач
    let port = port();
    if bot_running(port) {
        shutdown_bot(port, &read_key());
    }
    uninstall::kill_other_instances(&root);
    pipabobik_core::system::kill_orphan_players();
    let copy = match uninstall::temp_copy() {
        Ok(c) => c,
        Err(e) => {
            ask(TITLE, &format!("Не вдалося почати видалення: {e}"), Buttons::Ok);
            return 1;
        }
    };
    let mut a = vec!["--uninstall-run".to_string(), "--target".into(), root.to_string_lossy().into_owned(), "--pid".into(), std::process::id().to_string()];
    if keep {
        a.push("--keep-data".into());
    }
    let tmp = std::env::temp_dir();
    if let Err(e) = proc::spawn_detached(&copy, &a, &tmp) {
        ask(TITLE, &format!("Не вдалося почати видалення: {e}"), Buttons::Ok);
        return 1;
    }
    0
}

/// Крок 2 (копія з TEMP): ярлики, реєстр, файли, повідомлення, самовидалення.
fn uninstall_run(args: &[String]) -> i32 {
    let root = PathBuf::from(arg_value(args, "--target"));
    let keep = args.iter().any(|a| a == "--keep-data");
    if !uninstall::looks_installed(&root) {
        return 1;
    }
    let pid: u32 = arg_value(args, "--pid").parse().unwrap_or(0);
    let _ = updater::wait_pid(pid, Duration::from_secs(15), &std::env::temp_dir());
    uninstall::kill_other_instances(&root);
    std::thread::sleep(Duration::from_millis(800)); // Windows відпускає файли не миттєво
    uninstall::run_powershell(&uninstall::unregister_script(&root.join(updater::EXE)));
    let mut res = uninstall::remove_files(&root, keep);
    if !res.failed.is_empty() {
        // файл міг бути ще зайнятий — друга спроба
        std::thread::sleep(Duration::from_secs(2));
        res = uninstall::remove_files(&root, keep);
    }
    let mut text = String::from(if res.failed.is_empty() { "pipabobik видалено." } else { "pipabobik видалено не повністю." });
    if keep && !res.folder_removed {
        text += &format!("\n\nНалаштування й дані залишились у папці:\n{}", root.display());
    }
    if !res.failed.is_empty() {
        text += &format!("\n\nНе вдалося видалити (видаліть вручну):\n{}", res.failed.iter().take(5).cloned().collect::<Vec<_>>().join("\n"));
    }
    text += "\n\nВ OBS приберіть скрипт pipabobik.lua: Інструменти → Скрипти → pipabobik.lua → «−».\n\nmpv, Deno і yt-dlp залишились: якщо вони вам не потрібні, видаліть їх у «Параметри → Програми».";
    ask(TITLE, &text, Buttons::Ok);
    uninstall::delete_self_later();
    0
}
