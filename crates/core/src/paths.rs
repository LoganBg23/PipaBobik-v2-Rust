//! Де лежать файли бота: папка з PipaBobik.exe, data/, config.toml.

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;

pub static ROOT: Lazy<PathBuf> = Lazy::new(find_root);

fn find_root() -> PathBuf {
    if let Ok(p) = std::env::var("PIPABOBIK_ROOT") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    if let Some(d) = &exe_dir {
        let markers = ["config.toml", "config.example.toml", "obs"];
        if markers.iter().any(|m| d.join(m).exists()) {
            return d.clone();
        }
    }
    // запуск із cargo: target/debug → корінь проєкту (там, де config.toml)
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("config.toml").exists() || cwd.join("config.example.toml").exists() {
        return cwd;
    }
    exe_dir.unwrap_or(cwd)
}

pub fn root() -> &'static Path {
    &ROOT
}
pub fn data_dir() -> PathBuf {
    ROOT.join("data")
}
pub fn config_path() -> PathBuf {
    ROOT.join("config.toml")
}
/// Сюди бот кладе yt-dlp.exe, якщо його немає в системі.
pub fn bin_dir() -> PathBuf {
    ROOT.join("bin")
}
/// Позначка «бот перезапускається»: поки вона свіжа, вікно застосунку не закривається.
pub fn restart_mark() -> PathBuf {
    data_dir().join("restarting")
}

pub fn touch_restart_mark() {
    let _ = std::fs::create_dir_all(data_dir());
    let _ = std::fs::write(restart_mark(), format!("{}", crate::track::now_secs()));
}

pub fn restarting() -> bool {
    std::fs::metadata(restart_mark())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e.as_secs() < 60)
        .unwrap_or(false)
}

/// Шукає програму в PATH (на Windows — з .exe).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) { vec![".exe".into(), ".cmd".into(), String::new()] } else { vec![String::new()] };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let p = dir.join(format!("{name}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}
