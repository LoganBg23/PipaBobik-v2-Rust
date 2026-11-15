//! PipaBobik.exe — один файл на все:
//!
//!   PipaBobik.exe                — вікно застосунку (запускає бота, якщо він ще не працює);
//!   PipaBobik.exe --bot [--obs] [--live] [--paused] [-v]
//!                                — сам бот без вікна (так його запускає скрипт OBS);
//!   PipaBobik.exe --apply-update …
//!                                — встановлення оновлення (запускає бот, людині не потрібно).
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
use pipabobik_core::{paths, proc, updater};
use tauri::webview::NewWindowResponse;
use tauri::window::Color;
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    if has("--version") {
        println!("PipaBobik {}", updater::version());
        return;
    }
    if has("--apply-update") {
        std::process::exit(apply_update(&args));
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

fn apply_update(args: &[String]) -> i32 {
    let a = updater::ApplyArgs {
        staged: PathBuf::from(arg_value(args, "--staged")),
        target: PathBuf::from(arg_value(args, "--target")),
        tag: arg_value(args, "--tag"),
        message: arg_value(args, "--message"),
        bot_pid: arg_value(args, "--bot-pid").parse().unwrap_or(0),
        obs_pid: arg_value(args, "--obs-pid").parse().unwrap_or(0),
        obs_exe: arg_value(args, "--obs-exe"),
        bot_args: serde_json::from_str(&arg_value(args, "--bot-args")).unwrap_or_default(),
    };
    if a.target.as_os_str().is_empty() || a.staged.as_os_str().is_empty() {
        return 2;
    }
    updater::apply_update(a)
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

fn run_panel() {
    let port = port();
    let closing = Arc::new(AtomicBool::new(false));
    let closing_ev = closing.clone();
    let ctx = tauri::generate_context!();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // вдруге відкрили ярлик — показуємо вже відкрите вікно
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            let win = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("pipabobik")
                .inner_size(480.0, 900.0)
                .min_inner_size(380.0, 560.0)
                .background_color(Color(0x1c, 0x07, 0x16, 0xff))
                // посилання назовні (YouTube, вхід у Twitch) — у звичайному браузері
                .on_navigation(|url| {
                    if is_local(url) && !url.path().starts_with("/auth") {
                        return true;
                    }
                    open_url(url.as_str());
                    false
                })
                .on_new_window(|url, _features| {
                    open_url(url.as_str());
                    NewWindowResponse::Deny
                })
                .build()?;
            let handle = app.handle().clone();
            std::thread::spawn(move || panel_worker(handle, win, port));
            Ok(())
        })
        .on_window_event(move |window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
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
fn panel_worker(app: tauri::AppHandle, win: tauri::WebviewWindow, port: u16) {
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
