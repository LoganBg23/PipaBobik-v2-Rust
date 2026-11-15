//! Фільтр російської музики.
//!
//! Три перевірки, від найдешевшої до найточнішої:
//!   1. мова назви: літери ы/э/ъ/ё або типово російські слова (без українських і/ї/є/ґ);
//!   2. вбудований список російських виконавців + ваш список із config.toml;
//!   3. країна виконавця з відкритої бази MusicBrainz (кешується в data/artist_country.json).
//!
//! Фільтр евристичний: щось може проскочити або рідко спрацювати зайве — тоді трек можна
//! «Розбанити» в застосунку, і фільтр більше його не чіпатиме.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::rulists::{RU_AREAS, RU_ARTISTS, RU_WORDS};

const RU_ONLY: &[char] = &['ы', 'э', 'ъ', 'ё', 'Ы', 'Э', 'Ъ', 'Ё'];
const UA: &[char] = &['і', 'ї', 'є', 'ґ', 'І', 'Ї', 'Є', 'Ґ'];
const BY: &[char] = &['ў', 'Ў']; // білоруська теж має ы/э/ё — її не чіпаємо за мовою

static SPLIT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\s*(?:,|&|\+|\bfeat\.?|\bft\.?|\bx\b|(?:^|\s)х(?:\s|$)|(?:^|\s)и(?:\s|$)|\band\b)\s*").unwrap());
static WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)[а-яёіїєґ']+").unwrap());
static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static ARTISTS: Lazy<HashSet<&'static str>> = Lazy::new(|| RU_ARTISTS.iter().copied().collect());

pub fn norm(s: &str) -> String {
    let s = s.to_lowercase().replace('ё', "е");
    let s = SPACES.replace_all(s.trim(), " ").into_owned();
    let s = s.strip_suffix(" - topic").map(str::to_string).unwrap_or(s);
    s.trim_matches(|c| c == ' ' || c == '.').to_string()
}

pub fn split_artists(artist: &str) -> Vec<String> {
    let parts: Vec<String> = SPLIT.split(artist).map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() && !artist.is_empty() {
        vec![artist.to_string()]
    } else {
        parts
    }
}

pub fn russian_language(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if text.chars().any(|c| UA.contains(&c) || BY.contains(&c)) {
        return false;
    }
    if text.chars().any(|c| RU_ONLY.contains(&c)) {
        return true;
    }
    let low = text.to_lowercase();
    if WORD.find_iter(&low).any(|m| RU_WORDS.contains(&m.as_str())) {
        return true;
    }
    low.contains("люблю тебя")
}

pub struct RuFilter {
    cache_path: PathBuf,
    pub extra: std::sync::Mutex<HashSet<String>>,
    cache: std::sync::Mutex<HashMap<String, String>>,
    http: reqwest::Client,
    lock: Mutex<Instant>,
    offline_until: std::sync::Mutex<Option<Instant>>,
}

impl RuFilter {
    pub fn new(cache_path: PathBuf, extra: &[String], http: reqwest::Client) -> Self {
        let cache = std::fs::read_to_string(&cache_path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self {
            cache_path,
            extra: std::sync::Mutex::new(extra.iter().map(|a| norm(a)).filter(|a| !a.is_empty()).collect()),
            cache: std::sync::Mutex::new(cache),
            http,
            lock: Mutex::new(Instant::now() - Duration::from_secs(5)),
            offline_until: std::sync::Mutex::new(None),
        }
    }

    pub fn set_extra(&self, extra: &[String]) {
        *self.extra.lock().unwrap() = extra.iter().map(|a| norm(a)).filter(|a| !a.is_empty()).collect();
    }

    fn save(&self) {
        let data = serde_json::to_string(&*self.cache.lock().unwrap()).unwrap_or_default();
        if let Some(d) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&self.cache_path, data);
    }

    /// Перевірки без інтернету: мова і список виконавців.
    pub fn quick_check(&self, title: &str, artist: &str, album: &str) -> Option<String> {
        if russian_language(&format!("{title} {album}")) || russian_language(artist) {
            return Some("російською мовою".into());
        }
        let extra = self.extra.lock().unwrap();
        // спершу повна назва («Король и Шут»), потім частини
        for name in std::iter::once(artist.to_string()).chain(split_artists(artist)) {
            let n = norm(&name);
            if ARTISTS.contains(n.as_str()) || extra.contains(&n) {
                return Some(format!("російський виконавець ({name})"));
            }
        }
        None
    }

    pub async fn check(&self, title: &str, artist: &str, album: &str) -> Option<String> {
        if let Some(r) = self.quick_check(title, artist, album) {
            return Some(r);
        }
        let ua_text = format!("{title}{artist}").chars().any(|c| UA.contains(&c));
        for name in split_artists(artist).into_iter().take(3) {
            let country = self.country(&name).await;
            if country.as_deref() == Some("RU") || (country.as_deref() == Some("SU") && !ua_text) {
                return Some(format!("російський виконавець ({name})"));
            }
        }
        None
    }

    fn cached(&self, key: &str) -> Option<Option<String>> {
        self.cache.lock().unwrap().get(key).map(|c| (!c.is_empty()).then(|| c.clone()))
    }

    pub async fn country(&self, name: &str) -> Option<String> {
        let key = norm(name);
        if key.is_empty() {
            return None;
        }
        if let Some(c) = self.cached(&key) {
            return c;
        }
        if matches!(*self.offline_until.lock().unwrap(), Some(t) if Instant::now() < t) {
            return None;
        }
        let data = {
            let mut last = self.lock.lock().await;
            if let Some(c) = self.cached(&key) {
                return c;
            }
            // MusicBrainz дозволяє 1 запит на секунду
            let wait = Duration::from_millis(1100).saturating_sub(last.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            *last = Instant::now();
            let res = self
                .http
                .get("https://musicbrainz.org/ws/2/artist/")
                .query(&[("query", format!("artist:\"{name}\"")), ("fmt", "json".into()), ("limit", "5".into())])
                .header("User-Agent", "pipabobik/2.0 ( https://github.com/LoganBg23/PipaBobik-v2-Rust )")
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            let parsed = match res {
                Ok(r) if r.status().is_success() => r.json::<Value>().await.map_err(|e| e.to_string()),
                Ok(r) => Err(format!("HTTP {}", r.status())),
                Err(e) => Err(e.to_string()),
            };
            match parsed {
                Ok(v) => v,
                Err(e) => {
                    tracing::info!("MusicBrainz недоступний ({e}) — перевіряю лише за мовою і списком");
                    *self.offline_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(300));
                    return None;
                }
            }
        };
        let country = pick_country(&key, data["artists"].as_array().map(|a| a.as_slice()).unwrap_or(&[]));
        self.cache.lock().unwrap().insert(key, country.clone().unwrap_or_default());
        self.save();
        country
    }
}

pub fn pick_country(key: &str, artists: &[Value]) -> Option<String> {
    for a in artists {
        let score = a["score"].as_i64().or_else(|| a["score"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
        if score < 90 {
            continue;
        }
        let mut names = vec![norm(a["name"].as_str().unwrap_or(""))];
        if let Some(al) = a["aliases"].as_array() {
            names.extend(al.iter().map(|x| norm(x["name"].as_str().unwrap_or(""))));
        }
        if !names.iter().any(|n| n == key) {
            continue;
        }
        let c = a["country"].as_str().unwrap_or("").to_uppercase();
        if !c.is_empty() {
            return Some(c);
        }
        for area_key in ["area", "begin-area"] {
            let area = norm(a[area_key]["name"].as_str().unwrap_or(""));
            if RU_AREAS.contains(&area.as_str()) {
                return Some("RU".into());
            }
            if area == "soviet union" {
                return Some("SU".into());
            }
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f() -> RuFilter {
        let dir = std::env::temp_dir().join(format!("ruf-{}", rand::random::<u32>()));
        RuFilter::new(dir.join("c.json"), &["Мій Рос".into()], reqwest::Client::new())
    }

    #[test]
    fn language_and_artists() {
        assert!(russian_language("Ты моя любовь"));
        assert!(russian_language("я тебя люблю"));
        assert!(!russian_language("Обійми мене"));
        assert!(!russian_language("Shape of You"));
        let r = f();
        assert!(r.quick_check("Song", "Король и Шут", "").is_some());
        assert!(r.quick_check("Song", "Morgenshtern & Someone", "").is_some());
        assert!(r.quick_check("Song", "мій рос", "").is_some());
        assert!(r.quick_check("Обійми", "Океан Ельзи", "").is_none());
        assert_eq!(split_artists("A feat. B, C & D"), vec!["A", "B", "C", "D"]);
        assert_eq!(norm("Artist - Topic"), "artist");
    }

    #[test]
    fn musicbrainz_pick() {
        let a = vec![json!({"score": 100, "name": "X", "area": {"name": "Moscow"}})];
        assert_eq!(pick_country("x", &a).as_deref(), Some("RU"));
        let a = vec![json!({"score": 80, "name": "X", "country": "RU"})];
        assert_eq!(pick_country("x", &a), None);
        let a = vec![json!({"score": 100, "name": "Y", "aliases": [{"name": "X"}], "country": "ua"})];
        assert_eq!(pick_country("x", &a).as_deref(), Some("UA"));
    }
}
