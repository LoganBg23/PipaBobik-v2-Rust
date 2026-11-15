//! Фонові плейлисти, між якими стрімерка перемикається в застосунку.
//!
//! Зберігаються в data/playlists.json. Який із них грає зараз — google.fallback_playlist у config.toml
//! (порожньо — тиша, коли черга порожня).

use std::path::PathBuf;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

pub const LIKED: &str = "LM"; // «Вподобані» у вашому YouTube Music

static ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]{2,80}$").unwrap());
static LIST: Lazy<Regex> = Lazy::new(|| Regex::new(r"[?&]list=([A-Za-z0-9_-]+)").unwrap());
static BROWSE: Lazy<Regex> = Lazy::new(|| Regex::new(r"/browse/VL([A-Za-z0-9_-]+)").unwrap());

/// Ідентифікатор плейлиста з посилання YouTube / YouTube Music або сам ідентифікатор.
pub fn playlist_id(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case(LIKED) {
        return Some(LIKED.into());
    }
    if let Some(c) = LIST.captures(t).or_else(|| BROWSE.captures(t)) {
        return Some(c[1].to_string());
    }
    let t = match t.strip_prefix("VL") {
        Some(rest) if ID.is_match(rest) => rest,
        _ => t,
    };
    (ID.is_match(t) && !t.contains('/')).then(|| t.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Playlist {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

pub struct Playlists {
    pub path: PathBuf,
    pub items: Vec<Playlist>,
}

impl Playlists {
    pub fn load(path: PathBuf, active: &str) -> Self {
        let mut items: Vec<Playlist> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Playlist>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !p.id.is_empty())
            .map(|mut p| {
                if p.name.is_empty() {
                    p.name = p.id.clone();
                }
                p
            })
            .collect();
        let first = items.is_empty();
        if first {
            // перший запуск: «Вподобані» і плейлист, який уже був у config.toml
            items.push(Playlist { id: LIKED.into(), name: "Вподобані".into() });
            if let Some(pid) = playlist_id(active).filter(|p| p != LIKED) {
                items.push(Playlist { id: pid, name: "Мій плейлист".into() });
            }
        }
        let me = Self { path, items };
        if first {
            me.save();
        }
        me
    }

    pub fn save(&self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&self.path, serde_json::to_string_pretty(&self.items).unwrap_or_default()) {
            tracing::warn!("Не вдалося зберегти плейлисти: {e}");
        }
    }

    pub fn get(&self, id: &str) -> Option<&Playlist> {
        self.items.iter().find(|p| p.id == id)
    }

    pub fn add(&mut self, id: &str, name: &str) {
        if let Some(p) = self.items.iter_mut().find(|p| p.id == id) {
            if !name.is_empty() {
                p.name = name.into();
            }
        } else {
            self.items.push(Playlist { id: id.into(), name: if name.is_empty() { "Плейлист".into() } else { name.into() } });
        }
        self.save();
    }

    pub fn rename(&mut self, id: &str, name: &str) -> bool {
        match self.items.iter_mut().find(|p| p.id == id) {
            Some(p) if !name.is_empty() => {
                p.name = name.into();
                self.save();
                true
            }
            _ => false,
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.items.retain(|p| p.id != id);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(playlist_id("https://music.youtube.com/playlist?list=PLabc_123").as_deref(), Some("PLabc_123"));
        assert_eq!(playlist_id("https://music.youtube.com/browse/VLPLxyz").as_deref(), Some("PLxyz"));
        assert_eq!(playlist_id("lm").as_deref(), Some("LM"));
        assert_eq!(playlist_id("VLPLqq").as_deref(), Some("PLqq"));
        assert_eq!(playlist_id("https://example.com/a"), None);
        assert_eq!(playlist_id(""), None);
    }

    #[test]
    fn first_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Playlists::load(dir.path().join("playlists.json"), "PLmine");
        assert_eq!(p.items.len(), 2);
        p.add("PLnew", "");
        p.rename("PLnew", "Нове");
        let p2 = Playlists::load(dir.path().join("playlists.json"), "");
        assert_eq!(p2.get("PLnew").unwrap().name, "Нове");
    }
}
