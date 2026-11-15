//! Ядро PipaBobik v2: черга, YouTube Music, mpv, чат Twitch, локальний сервер для застосунку й віджета.

pub mod access;
pub mod app;
pub mod commands;
pub mod config;
pub mod errors;
pub mod jukebox;
pub mod messages;
pub mod music;
pub mod paths;
pub mod player;
pub mod playlists;
pub mod proc;
pub mod rufilter;
pub mod rulists;
pub mod server;
pub mod system;
pub mod track;
pub mod twitch;
pub mod uninstall;
pub mod updater;

pub type Http = reqwest::Client;

/// HTTP-клієнт для GitHub і YouTube (з переходами за посиланнями й таймаутом з'єднання).
pub fn http_client() -> Http {
    reqwest::Client::builder().gzip(true).connect_timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default()
}
