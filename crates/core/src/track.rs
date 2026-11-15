//! Трек у черзі й допоміжні функції (час, обкладинка, ідентифікатор відео).

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: u64,
    pub cover: String,
    pub explicit: bool,
    pub requested_by: String,
    pub requested_by_id: String,
    pub source: String, // request | fallback
    pub added_at: f64,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            video_id: String::new(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: 0,
            cover: String::new(),
            explicit: false,
            requested_by: String::new(),
            requested_by_id: String::new(),
            source: "request".into(),
            added_at: now_secs(),
        }
    }
}

impl Track {
    pub fn new(video_id: &str, title: &str) -> Self {
        Self { video_id: video_id.into(), title: title.into(), ..Default::default() }
    }
    pub fn url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.video_id)
    }
    pub fn label(&self) -> String {
        if self.artist.is_empty() {
            format!("«{}»", self.title)
        } else {
            format!("«{}» — {}", self.title, self.artist)
        }
    }
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
    /// Трек із JSON застосунку: невідомі поля ігноруються, бракує обов'язкових — помилка.
    pub fn from_json(v: &Value) -> Option<Self> {
        let t: Track = serde_json::from_value(v.clone()).ok()?;
        if t.video_id.is_empty() {
            return None;
        }
        Some(t)
    }
}

pub fn fmt_time(sec: f64) -> String {
    let s = sec.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

static VIDEO_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/|embed/|live/)|youtu\.be/|music\.youtube\.com/watch\?(?:.*&)?v=)([A-Za-z0-9_-]{11})").unwrap()
});
static SIZE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"=w\d+-h\d+").unwrap());

pub fn extract_video_id(text: &str) -> Option<String> {
    VIDEO_ID_RE.captures(text).map(|c| c[1].to_string())
}

/// Обкладинка, яка не зникне з часом.
///
/// Обкладинки YouTube Music (googleusercontent) — найбільші, до 544px. Кадри з відео (i.ytimg.com)
/// приходять із тимчасовим підписом (?sqp=…) і часто як maxresdefault, якого в багатьох відео
/// немає, — тому беремо постійне посилання hqdefault, воно є в кожного відео.
pub fn best_cover(thumbnails: Option<&Value>, video_id: &str) -> String {
    let mut items: Vec<&Value> = thumbnails.and_then(|t| t.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    let area = |t: &Value| t["width"].as_u64().unwrap_or(0) * t["height"].as_u64().unwrap_or(0);
    items.sort_by_key(|t| std::cmp::Reverse(area(t)));
    for t in &items {
        let url = t["url"].as_str().unwrap_or("");
        if url.contains("googleusercontent.com") || url.contains("ggpht.com") {
            return SIZE_RE.replace(url, "=w544-h544").into_owned();
        }
    }
    if !video_id.is_empty() {
        return format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg");
    }
    for t in &items {
        let url = t["url"].as_str().unwrap_or("").split('?').next().unwrap_or("");
        if !url.is_empty() {
            return url.into();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_and_covers() {
        assert_eq!(extract_video_id("https://youtu.be/dQw4w9WgXcQ?t=3").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(extract_video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=x").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(extract_video_id("обійми"), None);
        let th = json!([{"url": "https://lh3.googleusercontent.com/abc=w60-h60-l90", "width": 60, "height": 60},
                        {"url": "https://lh3.googleusercontent.com/abc=w120-h120-l90", "width": 120, "height": 120}]);
        assert_eq!(best_cover(Some(&th), "x"), "https://lh3.googleusercontent.com/abc=w544-h544-l90");
        let th = json!([{"url": "https://i.ytimg.com/vi/id/maxresdefault.jpg?sqp=xx", "width": 1280, "height": 720}]);
        assert_eq!(best_cover(Some(&th), "id"), "https://i.ytimg.com/vi/id/hqdefault.jpg");
        assert_eq!(fmt_time(125.7), "2:05");
        let t = Track::from_json(&json!({"video_id": "a", "title": "T", "artist": "A", "extra": 1})).unwrap();
        assert_eq!(t.label(), "«T» — A");
        assert!(Track::from_json(&json!({"title": "x"})).is_none());
    }
}
