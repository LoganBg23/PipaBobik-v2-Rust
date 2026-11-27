//! YouTube Music: пошук (InnerTube — те саме API, що й сайт music.youtube.com),
//! отримання потоку й плейлистів через yt-dlp, лайки у вашому акаунті.
//!
//! yt-dlp — окрема програма (yt-dlp.exe). Якщо її немає в системі, бот сам завантажує її в bin/
//! і раз на тиждень оновлює: YouTube часто щось змінює, і свіжий yt-dlp — головна умова стабільності.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;

use crate::paths;
use crate::proc::hidden;
use crate::track::{best_cover, now_secs, Track};

const ORIGIN: &str = "https://music.youtube.com";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CLIENT_VERSION: &str = "1.20251110.03.00";
/// Кеш на диску (data/cache): пошук і фонові плейлисти живуть до 7 днів.
pub const CACHE_TTL: f64 = 7.0 * 86400.0;
/// Скільки треків фонового плейлиста тримати в кеші (і завантажувати за раз).
pub const PLAYLIST_LIMIT: usize = 100;
/// Кеш плейлиста старший за це — оновлюється у фоні, поки грає збережений.
const PLAYLIST_REFRESH: f64 = 6.0 * 3600.0;
const SEARCH_LIMIT: usize = 300;
const SEARCH_TTL: f64 = CACHE_TTL;
const COOKIES_TTL: Duration = Duration::from_secs(6 * 3600);
/// Фільтри пошуку YouTube Music («Пісні» / «Відео»), як у ytmusicapi.
const PARAMS_SONGS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";
const PARAMS_VIDEOS: &str = "EgWKAQIQAWoMEA4QChADEAQQCRAF";
/// Плейлисти YouTube Music: добірки самого YouTube Music, плейлисти людей, усі разом.
const PARAMS_PLAYLISTS: [&str; 3] = ["EgeKAQQoADgBagwQDhAKEAMQBRAJEAQ=", "EgeKAQQoAEABagwQDhAKEAMQBRAJEAQ=", "EgWKAQIoAWoMEA4QChADEAQQCRAF"];

static DURATION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{1,2}(:\d{2}){1,2}$").unwrap());
static PLAYLIST_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[?&]list=([A-Za-z0-9_-]+)").unwrap());

#[derive(Debug)]
pub enum MusicError {
    LoginRequired,
    Other(String),
}

impl std::fmt::Display for MusicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MusicError::LoginRequired => f.write_str("немає входу в Google"),
            MusicError::Other(s) => f.write_str(s),
        }
    }
}

pub fn parse_duration(s: &str) -> u64 {
    s.split(':').fold(0, |acc, p| acc * 60 + p.trim().parse::<u64>().unwrap_or(0))
}

fn runs_text(v: &Value) -> String {
    if let Some(s) = v["simpleText"].as_str() {
        return s.into();
    }
    v["runs"].as_array().map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect()).unwrap_or_default()
}

fn page_type(run: &Value) -> &str {
    run["navigationEndpoint"]["browseEndpoint"]["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]["pageType"]
        .as_str()
        .unwrap_or("")
}

/// Знайдений плейлист (для теми фонової музики).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FoundPlaylist {
    pub id: String,
    pub title: String,
    /// «YouTube Music • 50 songs» тощо — як показує YouTube Music.
    pub info: String,
}

fn find_playlist_id(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            if let Some(id) = m.get("playlistId").and_then(Value::as_str) {
                return Some(id.to_string());
            }
            if let Some(b) = m.get("browseId").and_then(Value::as_str) {
                if let Some(id) = b.strip_prefix("VL") {
                    return Some(id.to_string());
                }
            }
            m.values().find_map(find_playlist_id)
        }
        Value::Array(a) => a.iter().find_map(find_playlist_id),
        _ => None,
    }
}

/// Результати пошуку плейлистів YouTube Music → список плейлистів.
pub fn parse_playlists(data: &Value, limit: usize) -> Vec<FoundPlaylist> {
    let mut items = Vec::new();
    collect_items(data, &mut items);
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter_map(|it| {
            // пісні мають videoId — це не плейлист
            if it["playlistItemData"]["videoId"].is_string() {
                return None;
            }
            let id = it["navigationEndpoint"]["browseEndpoint"]["browseId"]
                .as_str()
                .and_then(|b| b.strip_prefix("VL"))
                .map(String::from)
                .or_else(|| find_playlist_id(&it["overlay"]))
                .or_else(|| find_playlist_id(it))?;
            let cols = it["flexColumns"].as_array()?;
            let col = |i: usize| cols.get(i).map(|c| runs_text(&c["musicResponsiveListItemFlexColumnRenderer"]["text"])).unwrap_or_default();
            let title = col(0);
            let info = col(1).trim_start_matches("Playlist • ").trim_start_matches("Плейлист • ").to_string();
            (!title.is_empty() && seen.insert(id.clone())).then_some(FoundPlaylist { id, title, info })
        })
        .take(limit)
        .collect()
}

/// Рядок результату пошуку YouTube Music → трек.
pub fn parse_item(it: &Value) -> Option<Track> {
    let vid = it["playlistItemData"]["videoId"]
        .as_str()
        .or_else(|| it["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchEndpoint"]["videoId"].as_str())
        .or_else(|| it["flexColumns"][0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0]["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str())?;
    let cols = it["flexColumns"].as_array()?;
    let col = |i: usize| &cols.get(i).unwrap_or(&Value::Null)["musicResponsiveListItemFlexColumnRenderer"]["text"];
    let title = runs_text(col(0));
    let runs: Vec<Value> = col(1)["runs"].as_array().cloned().unwrap_or_default();
    // групи між « • »: [тип?] [виконавці] [альбом / перегляди] [тривалість]
    let mut groups: Vec<Vec<&Value>> = vec![vec![]];
    for r in &runs {
        if r["text"].as_str().map(|t| t.trim() == "•").unwrap_or(false) {
            groups.push(vec![]);
        } else {
            groups.last_mut().unwrap().push(r);
        }
    }
    let gtext = |g: &Vec<&Value>| g.iter().filter_map(|r| r["text"].as_str()).collect::<String>().trim().to_string();
    let mut artists: Vec<String> = runs
        .iter()
        .filter(|r| matches!(page_type(r), "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL"))
        .filter_map(|r| r["text"].as_str().map(String::from))
        .collect();
    let album = runs.iter().find(|r| page_type(r) == "MUSIC_PAGE_TYPE_ALBUM").and_then(|r| r["text"].as_str()).unwrap_or("").to_string();
    let mut duration = groups.iter().rev().map(gtext).find(|t| DURATION_RE.is_match(t)).map(|t| parse_duration(&t)).unwrap_or(0);
    if duration == 0 {
        if let Some(fixed) = it["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"].as_object() {
            let t = runs_text(&Value::Object(fixed.clone()));
            if DURATION_RE.is_match(t.trim()) {
                duration = parse_duration(t.trim());
            }
        }
    }
    if artists.is_empty() {
        // без посилань: «Song • Виконавець • …» або «Виконавець • …»
        let kinds = ["Song", "Video", "Пісня", "Відео", "Episode"];
        let g = groups.iter().map(gtext).find(|t| !t.is_empty() && !kinds.contains(&t.as_str()) && !DURATION_RE.is_match(t));
        if let Some(g) = g {
            artists = vec![g];
        }
    }
    let explicit = it["badges"].as_array().map(|b| b.iter().any(|x| x["musicInlineBadgeRenderer"]["icon"]["iconType"] == "MUSIC_EXPLICIT_BADGE")).unwrap_or(false);
    let thumbs = &it["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"];
    Some(Track {
        video_id: vid.into(),
        title: if title.is_empty() { "Без назви".into() } else { title },
        artist: artists.join(", "),
        album,
        duration,
        cover: best_cover(Some(thumbs), vid),
        explicit,
        ..Default::default()
    })
}

fn collect_items<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            if let Some(it) = m.get("musicResponsiveListItemRenderer") {
                out.push(it);
                return;
            }
            // «Найкращий результат» (картка зверху) пропускаємо: там часто виконавець, а не пісня
            for (k, x) in m {
                if k != "musicCardShelfRenderer" {
                    collect_items(x, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_items(x, out)),
        _ => {}
    }
}

pub fn parse_search(data: &Value, limit: usize) -> Vec<Track> {
    let mut items = Vec::new();
    collect_items(data, &mut items);
    let mut seen = std::collections::HashSet::new();
    items.into_iter().filter_map(parse_item).filter(|t| seen.insert(t.video_id.clone())).take(limit).collect()
}

/// Підпис для запитів від вашого імені (як це робить сайт music.youtube.com).
pub fn sapisid_hash(sapisid: &str, ts: u64) -> String {
    let mut h = Sha1::new();
    h.update(format!("{ts} {sapisid} {ORIGIN}"));
    format!("SAPISIDHASH {ts}_{}", hex::encode(h.finalize()))
}

/// Cookies YouTube з файлу Netscape (cookies.txt).
pub fn read_cookies(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 7 && f[0].ends_with("youtube.com") && !f[6].is_empty() {
            out.insert(f[5].to_string(), f[6].trim_end().to_string());
        }
    }
    out
}

/// Де взяти yt-dlp: поруч із ботом (bin/), у PATH або там, куди ставить winget.
pub fn find_ytdlp() -> Option<PathBuf> {
    let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    let local = paths::bin_dir().join(name);
    if local.is_file() {
        return Some(local);
    }
    if let Some(p) = paths::which("yt-dlp") {
        return Some(p);
    }
    #[cfg(windows)]
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        let p = Path::new(&la).join("Microsoft").join("WinGet").join("Links").join("yt-dlp.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn find_deno() -> Option<PathBuf> {
    let own = paths::bin_dir().join(if cfg!(windows) { "deno.exe" } else { "deno" });
    if own.is_file() {
        return Some(own);
    }
    if let Some(p) = paths::which("deno") {
        return Some(p);
    }
    #[cfg(windows)]
    for env in ["LOCALAPPDATA", "USERPROFILE"] {
        if let Ok(base) = std::env::var(env) {
            for rel in [["Microsoft", "WinGet", "Links", "deno.exe"].as_slice(), [".deno", "bin", "deno.exe"].as_slice()] {
                let p = rel.iter().fold(PathBuf::from(&base), |p, x| p.join(x));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

pub struct MusicSource {
    pub cookies_from_browser: Mutex<String>,
    pub cookies_file: Mutex<String>,
    http: reqwest::Client,
    ytdlp: Mutex<Option<PathBuf>>,
    cookies_broken: AtomicBool,
    cookies_at: Mutex<Option<Instant>>,
    export_lock: tokio::sync::Mutex<()>,
    slots: Semaphore,
    tmp_seq: AtomicU64,
    search_cache: Mutex<HashMap<(String, bool), (f64, Vec<Track>)>>,
    pub cache_dir: PathBuf,
    me: std::sync::OnceLock<std::sync::Weak<MusicSource>>,
    refreshing: Mutex<std::collections::HashSet<String>>,
    /// Deno: yt-dlp без нього не розв'язує захист YouTube, і посилання на звук відповідають 403.
    deno: Mutex<Option<PathBuf>>,
    /// До цього часу звук береться запасним способом (без входу Google, клієнт без JS):
    /// звичайні посилання щойно не відтворились.
    alt_until: Mutex<f64>,
}

/// Запасний спосіб отримати звук: без cookies, клієнт android_vr не потребує ні JS, ні PO-токена.
const ALT_EXTRACTOR_ARGS: &str = "youtube:player_client=android_vr,default;skip=hls,dash,translated_subs";
const ALT_FOR: f64 = 3600.0;

/// Збережений плейлист: коли збережено і треки (не більше PLAYLIST_LIMIT).
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedPlaylist {
    saved_at: f64,
    tracks: Vec<Track>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedSearch {
    q: String,
    songs: bool,
    at: f64,
    tracks: Vec<Track>,
}

fn playlist_file(dir: &Path, pid: &str) -> PathBuf {
    let safe: String = pid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    dir.join("playlists").join(format!("{safe}.json"))
}

/// Плейлист із кешу, якщо йому менше 7 днів: (коли збережено, треки).
pub fn load_cached_playlist(dir: &Path, pid: &str) -> Option<(f64, Vec<Track>)> {
    let c: CachedPlaylist = serde_json::from_str(&std::fs::read_to_string(playlist_file(dir, pid)).ok()?).ok()?;
    (now_secs() - c.saved_at < CACHE_TTL && !c.tracks.is_empty()).then_some((c.saved_at, c.tracks))
}

pub fn save_cached_playlist(dir: &Path, pid: &str, tracks: &[Track]) {
    let path = playlist_file(dir, pid);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let c = CachedPlaylist { saved_at: now_secs(), tracks: tracks.iter().take(PLAYLIST_LIMIT).cloned().collect() };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string(&c).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Прибирає з кешу все, що старше 7 днів.
pub fn prune_cache(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir.join("playlists")) else { return };
    for e in rd.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a.as_secs_f64() > CACHE_TTL).unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl MusicSource {
    pub fn new(cookies_from_browser: &str, cookies_file: &str, http: reqwest::Client) -> Self {
        Self {
            cookies_from_browser: Mutex::new(cookies_from_browser.trim().to_lowercase()),
            cookies_file: Mutex::new(cookies_file.trim().to_string()),
            http,
            ytdlp: Mutex::new(find_ytdlp()),
            cookies_broken: AtomicBool::new(false),
            cookies_at: Mutex::new(None),
            export_lock: tokio::sync::Mutex::new(()),
            slots: Semaphore::new(2),
            tmp_seq: AtomicU64::new(0),
            search_cache: Mutex::new(HashMap::new()),
            cache_dir: paths::data_dir().join("cache"),
            me: std::sync::OnceLock::new(),
            refreshing: Mutex::new(Default::default()),
            deno: Mutex::new(find_deno()),
            alt_until: Mutex::new(0.0),
        }
    }

    /// Посилання на себе: щоб оновлювати кеш плейлиста у фоні.
    pub fn set_self(self: &std::sync::Arc<Self>) {
        let _ = self.me.set(std::sync::Arc::downgrade(self));
    }

    /// Завантажує збережені пошуки (до 7 днів), щоб повторні замовлення знаходились миттєво.
    pub fn load_search_cache(&self) {
        prune_cache(&self.cache_dir);
        let Ok(text) = std::fs::read_to_string(self.cache_dir.join("search.json")) else { return };
        let items: Vec<CachedSearch> = serde_json::from_str(&text).unwrap_or_default();
        let mut c = self.search_cache.lock().unwrap();
        for it in items.into_iter().filter(|i| now_secs() - i.at < SEARCH_TTL) {
            c.insert((it.q, it.songs), (it.at, it.tracks));
        }
    }

    fn save_search_cache(&self) {
        let items: Vec<CachedSearch> = {
            let c = self.search_cache.lock().unwrap();
            let mut v: Vec<CachedSearch> = c.iter().map(|((q, songs), (at, tracks))| CachedSearch { q: q.clone(), songs: *songs, at: *at, tracks: tracks.clone() }).collect();
            v.sort_by(|a, b| b.at.total_cmp(&a.at));
            v.truncate(SEARCH_LIMIT);
            v
        };
        let _ = std::fs::create_dir_all(&self.cache_dir);
        let path = self.cache_dir.join("search.json");
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_string(&items).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn ytdlp_path(&self) -> Option<PathBuf> {
        self.ytdlp.lock().unwrap().clone()
    }

    fn exported_cookies() -> PathBuf {
        paths::data_dir().join("yt-cookies.txt")
    }

    fn uses_login(&self) -> bool {
        !self.cookies_broken.load(Ordering::Relaxed)
            && (!self.cookies_from_browser.lock().unwrap().is_empty() || !self.cookies_file.lock().unwrap().is_empty())
    }

    /// Файл cookies, з яким працюють yt-dlp і лайки (None — без логіну).
    fn cookie_source(&self) -> Option<PathBuf> {
        if self.cookies_broken.load(Ordering::Relaxed) {
            return None;
        }
        let file = self.cookies_file.lock().unwrap().clone();
        if !file.is_empty() && Path::new(&file).is_file() {
            return Some(PathBuf::from(file));
        }
        if self.cookies_from_browser.lock().unwrap().is_empty() {
            return None;
        }
        let p = Self::exported_cookies();
        p.is_file().then_some(p)
    }

    /// Раз на 6 годин переносить вхід Google з браузера у data/yt-cookies.txt.
    /// Так yt-dlp не розшифровує cookies Firefox на кожен трек — це найдовша частина запуску.
    pub async fn refresh_cookies(&self, force: bool) {
        let browser = self.cookies_from_browser.lock().unwrap().clone();
        if browser.is_empty() || self.cookies_broken.load(Ordering::Relaxed) || !self.cookies_file.lock().unwrap().is_empty() {
            return;
        }
        let _g = self.export_lock.lock().await;
        let fresh = self.cookies_at.lock().unwrap().map(|t| t.elapsed() < COOKIES_TTL).unwrap_or(false);
        if fresh && !force && Self::exported_cookies().is_file() {
            return;
        }
        let Some(exe) = self.ensure_ytdlp().await else { return };
        let out = Self::exported_cookies();
        let _ = std::fs::create_dir_all(paths::data_dir());
        let tmp = out.with_extension("new");
        let _ = std::fs::remove_file(&tmp);
        let mut cmd = hidden(&exe);
        cmd.args(["--ignore-config", "--no-warnings", "--skip-download", "--flat-playlist", "--playlist-end", "1", "-q"])
            .arg("--cookies-from-browser")
            .arg(&browser)
            .arg("--cookies")
            .arg(&tmp)
            .arg("https://music.youtube.com/playlist?list=LM");
        let res = tokio::time::timeout(Duration::from_secs(90), cmd.output()).await;
        let err = match res {
            Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "yt-dlp не відповів за 90 с".into(),
        };
        if tmp.is_file() && std::fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false) {
            let _ = std::fs::rename(&tmp, &out);
            *self.cookies_at.lock().unwrap() = Some(Instant::now());
            return;
        }
        let low = err.to_lowercase();
        if low.contains("cookie") || low.contains(&browser) || low.contains("profile") {
            // логін недоступний (браузер не встановлено / профіль заблоковано) — граємо без нього
            tracing::warn!("Логін Google недоступний ({}) — працюю без нього", first_line(&err));
            self.cookies_broken.store(true, Ordering::Relaxed);
        } else {
            tracing::info!("Не вдалося перенести вхід Google: {}", first_line(&err));
        }
    }

    /// Чи є вхід Google (None — логін не налаштовано).
    pub async fn google_login(&self) -> Option<bool> {
        if self.cookies_from_browser.lock().unwrap().is_empty() && self.cookies_file.lock().unwrap().is_empty() {
            return None;
        }
        self.refresh_cookies(false).await;
        let c = self.cookie_source().map(|p| read_cookies(&p)).unwrap_or_default();
        Some(c.contains_key("SAPISID") || c.contains_key("__Secure-3PAPISID"))
    }

    /// yt-dlp є? Якщо ні — на Windows завантажує його з GitHub у bin/.
    pub async fn ensure_ytdlp(&self) -> Option<PathBuf> {
        if let Some(p) = self.ytdlp_path().filter(|p| p.is_file()) {
            return Some(p);
        }
        if let Some(p) = find_ytdlp() {
            *self.ytdlp.lock().unwrap() = Some(p.clone());
            return Some(p);
        }
        if !cfg!(windows) {
            tracing::error!("yt-dlp не знайдено — встановіть його (див. README)");
            return None;
        }
        let _g = self.export_lock.try_lock();
        tracing::info!("yt-dlp не знайдено — завантажую з GitHub");
        let dest = paths::bin_dir().join("yt-dlp.exe");
        match download(&self.http, "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe", &dest).await {
            Ok(()) => {
                *self.ytdlp.lock().unwrap() = Some(dest.clone());
                Some(dest)
            }
            Err(e) => {
                tracing::error!("Не вдалося завантажити yt-dlp: {e}. Встановіть: winget install -e --id yt-dlp.yt-dlp");
                None
            }
        }
    }

    /// Deno потрібен yt-dlp, щоб YouTube віддавав робочі посилання на звук. Немає — завантажуємо в bin/.
    pub async fn ensure_deno(&self) {
        if self.deno.lock().unwrap().is_some() || !cfg!(windows) {
            return;
        }
        tracing::info!("Deno не знайдено — завантажую з GitHub (потрібен yt-dlp для YouTube)");
        let zip = paths::bin_dir().join("deno-download.zip");
        let _ = std::fs::create_dir_all(paths::bin_dir());
        let url = "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip";
        if let Err(e) = download(&self.http, url, &zip).await {
            tracing::warn!("Не вдалося завантажити Deno: {e}. Встановіть: winget install -e --id DenoLand.Deno");
            let _ = std::fs::remove_file(&zip);
            return;
        }
        let dest = paths::bin_dir().join("deno.exe");
        let res = tokio::task::spawn_blocking({
            let (zip, dest) = (zip.clone(), dest.clone());
            move || -> Result<(), String> {
                let f = std::fs::File::open(&zip).map_err(|e| e.to_string())?;
                let mut z = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
                let mut entry = z.by_name("deno.exe").map_err(|e| e.to_string())?;
                let tmp = dest.with_extension("tmp");
                let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
                drop(out);
                std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())
            }
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let _ = std::fs::remove_file(&zip);
        match res {
            Ok(()) => {
                tracing::info!("Deno встановлено: {}", dest.display());
                *self.deno.lock().unwrap() = Some(dest);
            }
            Err(e) => tracing::warn!("Не вдалося розпакувати Deno: {e}"),
        }
    }

    /// Оновлює yt-dlp, якщо він наш (bin/) і не оновлювався тиждень.
    pub async fn self_update_ytdlp(&self) {
        let Some(exe) = self.ytdlp_path() else { return };
        if !exe.starts_with(paths::bin_dir()) {
            return;
        }
        let stamp = paths::data_dir().join("yt-dlp-updated");
        let old = std::fs::metadata(&stamp).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|e| e.as_secs() > 7 * 86400).unwrap_or(true);
        if !old {
            return;
        }
        let _slot = self.slots.acquire_many(2).await;
        let mut cmd = hidden(&exe);
        cmd.args(["-U", "--no-warnings"]);
        match tokio::time::timeout(Duration::from_secs(120), cmd.output()).await {
            Ok(Ok(o)) if o.status.success() => {
                let _ = std::fs::write(&stamp, "");
                tracing::info!("yt-dlp: {}", first_line(&String::from_utf8_lossy(&o.stdout)));
            }
            _ => tracing::info!("Не вдалося оновити yt-dlp — спробую пізніше"),
        }
    }

    /// Запуск yt-dlp. Повертає stdout або текст помилки.
    async fn ytdlp(&self, args: &[&str], timeout: u64) -> Result<String, String> {
        self.ytdlp_with(args, timeout, false).await
    }

    /// alt — запасний спосіб: без входу Google і з клієнтом YouTube, якому не потрібен JS.
    async fn ytdlp_with(&self, args: &[&str], timeout: u64, alt: bool) -> Result<String, String> {
        let exe = self.ensure_ytdlp().await.ok_or_else(|| "yt-dlp не знайдено".to_string())?;
        if !alt {
            self.refresh_cookies(false).await;
        }
        let _slot = self.slots.acquire().await.map_err(|e| e.to_string())?;
        // кожен запуск — зі своєю копією cookies: yt-dlp переписує файл наприкінці, і два одночасні запуски могли б його зіпсувати
        let mut tmp_cookie = None;
        if let Some(src) = self.cookie_source().filter(|_| !alt) {
            let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
            let tmp = paths::data_dir().join("tmp").join(format!("ck-{}-{n}.txt", std::process::id()));
            let _ = std::fs::create_dir_all(tmp.parent().unwrap());
            if std::fs::copy(&src, &tmp).is_ok() {
                tmp_cookie = Some(tmp);
            }
        }
        let mut cmd = hidden(&exe);
        let xargs = if alt { ALT_EXTRACTOR_ARGS } else { "youtube:skip=hls,dash,translated_subs" };
        cmd.args(["--ignore-config", "--no-warnings", "--socket-timeout", "15", "--extractor-args", xargs]);
        // Deno у PATH: yt-dlp сам його знаходить і розв'язує захист YouTube (будь-яка версія yt-dlp)
        if let Some(dir) = self.deno.lock().unwrap().as_ref().and_then(|d| d.parent().map(Path::to_path_buf)) {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut parts = vec![dir];
            parts.extend(std::env::split_paths(&path));
            if let Ok(joined) = std::env::join_paths(parts) {
                cmd.env("PATH", joined);
            }
        }
        if let Some(c) = &tmp_cookie {
            cmd.arg("--cookies").arg(c);
        }
        cmd.args(args);
        let res = tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await;
        if let Some(c) = tmp_cookie {
            let _ = std::fs::remove_file(c);
        }
        match res {
            Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("yt-dlp не відповів за {timeout} с")),
        }
    }

    async fn ytdlp_json(&self, args: &[&str], timeout: u64) -> Option<Value> {
        match self.ytdlp(args, timeout).await {
            Ok(out) => out.lines().rev().find(|l| l.trim_start().starts_with('{')).and_then(|l| serde_json::from_str(l).ok()),
            Err(e) => {
                let low = e.to_lowercase();
                if self.uses_login() && (low.contains("cookie") || low.contains("sign in to confirm")) && !low.contains("not a bot") {
                    tracing::warn!("Логін Google не спрацював ({}) — працюю без нього", first_line(&e));
                    self.cookies_broken.store(true, Ordering::Relaxed);
                    return Box::pin(self.ytdlp_json(args, timeout)).await;
                }
                tracing::warn!("yt-dlp: {}", first_line(&e));
                None
            }
        }
    }

    // ── InnerTube ──
    async fn innertube(&self, endpoint: &str, mut body: Value, auth: bool) -> Result<Value, MusicError> {
        body["context"] = json!({"client": {"clientName": "WEB_REMIX", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "UA"}});
        let mut rb = self
            .http
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("User-Agent", UA)
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("Referer", format!("{ORIGIN}/"))
            .header("X-YouTube-Client-Name", "67")
            .header("X-YouTube-Client-Version", CLIENT_VERSION)
            .timeout(Duration::from_secs(15))
            .json(&body);
        if auth {
            let cookies = self.cookie_source().map(|p| read_cookies(&p)).unwrap_or_default();
            let sapisid = cookies.get("SAPISID").or_else(|| cookies.get("__Secure-3PAPISID")).cloned().ok_or(MusicError::LoginRequired)?;
            let header: Vec<String> = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            rb = rb
                .header("Cookie", header.join("; "))
                .header("Authorization", sapisid_hash(&sapisid, now_secs() as u64))
                .header("X-Goog-AuthUser", "0");
        }
        let r = rb.send().await.map_err(|e| MusicError::Other(e.to_string()))?;
        let status = r.status();
        let v: Value = r.json().await.map_err(|e| MusicError::Other(e.to_string()))?;
        if !status.is_success() {
            if auth && (status.as_u16() == 401 || status.as_u16() == 403) {
                return Err(MusicError::LoginRequired);
            }
            return Err(MusicError::Other(format!("YouTube Music відповів {status}: {}", v["error"]["message"].as_str().unwrap_or(""))));
        }
        Ok(v)
    }

    async fn search_uncached(&self, query: &str, only_songs: bool, limit: usize) -> Vec<Track> {
        let filters: &[&str] = if only_songs { &[PARAMS_SONGS] } else { &[PARAMS_SONGS, PARAMS_VIDEOS] };
        for params in filters {
            match self.innertube("search", json!({"query": query, "params": params}), false).await {
                Ok(v) => {
                    let found = parse_search(&v, limit);
                    if !found.is_empty() {
                        return found;
                    }
                }
                Err(e) => tracing::warn!("Пошук YT Music не вдався: {e}"),
            }
        }
        // запасний шлях — пошук YouTube через yt-dlp
        let q = format!("ytsearch{limit}:{query}");
        let Some(v) = self.ytdlp_json(&["--flat-playlist", "-J", &q], 40).await else { return vec![] };
        flat_entries(&v, "request")
    }

    pub async fn search(&self, query: &str, only_songs: bool) -> Vec<Track> {
        let key = (query.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" "), only_songs);
        if let Some((at, hit)) = self.search_cache.lock().unwrap().get(&key) {
            if now_secs() - at < SEARCH_TTL {
                return hit.clone();
            }
        }
        let found = self.search_uncached(query, only_songs, 8).await;
        if !found.is_empty() {
            {
                let mut c = self.search_cache.lock().unwrap();
                if c.len() >= SEARCH_LIMIT {
                    // найстаріший пошук іде з кешу
                    if let Some(oldest) = c.iter().min_by(|a, b| a.1 .0.total_cmp(&b.1 .0)).map(|(k, _)| k.clone()) {
                        c.remove(&oldest);
                    }
                }
                c.insert(key, (now_secs(), found.clone()));
            }
            self.save_search_cache();
        }
        found
    }

    /// Плейлисти на тему («Різдвяні», «лоуфай»): спершу добірки YouTube Music, потім плейлисти людей,
    /// а якщо YouTube Music нічого не дав — пошук плейлистів YouTube через yt-dlp.
    pub async fn search_playlists(&self, query: &str) -> Vec<FoundPlaylist> {
        let mut out: Vec<FoundPlaylist> = Vec::new();
        for params in PARAMS_PLAYLISTS {
            match self.innertube("search", json!({"query": query, "params": params}), false).await {
                Ok(v) => {
                    for p in parse_playlists(&v, 8) {
                        if !out.iter().any(|x| x.id == p.id) {
                            out.push(p);
                        }
                    }
                }
                Err(e) => tracing::warn!("Пошук плейлистів YT Music не вдався: {e}"),
            }
            if out.len() >= 6 {
                break;
            }
        }
        if out.is_empty() {
            let url = format!("https://www.youtube.com/results?search_query={}&sp=EgIQAw%3D%3D", urlencoding::encode(query));
            if let Some(v) = self.ytdlp_json(&["--flat-playlist", "-J", "--playlist-end", "8", &url], 60).await {
                for e in v["entries"].as_array().into_iter().flatten() {
                    let id = e["id"].as_str().or_else(|| e["url"].as_str().and_then(|u| PLAYLIST_RE.captures(u).and_then(|c| c.get(1)).map(|m| m.as_str()))).unwrap_or("");
                    if id.len() > 10 && !out.iter().any(|x| x.id == id) {
                        out.push(FoundPlaylist { id: id.into(), title: e["title"].as_str().unwrap_or("Плейлист").into(), info: e["channel"].as_str().or(e["uploader"].as_str()).unwrap_or("YouTube").into() });
                    }
                }
            }
        }
        out.truncate(6);
        out
    }

    /// Готує все наперед, щоб перший «!трек» і перший трек не чекали на ініціалізацію.
    pub async fn warmup(&self) {
        self.ensure_ytdlp().await;
        self.ensure_deno().await;
        self.refresh_cookies(false).await;
        self.self_update_ytdlp().await;
    }

    pub async fn by_id(&self, video_id: &str) -> Option<Track> {
        match self.innertube("player", json!({"videoId": video_id}), false).await {
            Ok(v) if v["videoDetails"]["videoId"].is_string() => {
                let vd = &v["videoDetails"];
                return Some(Track {
                    video_id: video_id.into(),
                    title: vd["title"].as_str().filter(|s| !s.is_empty()).unwrap_or("Без назви").into(),
                    artist: vd["author"].as_str().unwrap_or("").trim_end_matches(" - Topic").into(),
                    duration: vd["lengthSeconds"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                    cover: best_cover(Some(&vd["thumbnail"]["thumbnails"]), video_id),
                    ..Default::default()
                });
            }
            Ok(_) => {}
            Err(e) => tracing::info!("Дані треку {video_id} не отримано ({e}) — пробую yt-dlp"),
        }
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let v = self.ytdlp_json(&["-J", "--skip-download", "--no-playlist", &url], 60).await?;
        Some(Track {
            video_id: video_id.into(),
            title: v["track"].as_str().or(v["title"].as_str()).unwrap_or("Без назви").into(),
            artist: v["artist"].as_str().or(v["uploader"].as_str()).unwrap_or("").into(),
            duration: v["duration"].as_f64().unwrap_or(0.0) as u64,
            cover: best_cover(Some(&v["thumbnails"]), video_id),
            ..Default::default()
        })
    }

    /// Пряме посилання на аудіо і http-заголовки для mpv.
    pub async fn resolve_stream(&self, track: &Track) -> Option<(String, HashMap<String, String>, u64)> {
        let alt = now_secs() < *self.alt_until.lock().unwrap();
        self.resolve_stream_with(track, alt).await
    }

    /// Запасне посилання: коли звичайне не відтворилось у mpv.
    pub async fn resolve_stream_alt(&self, track: &Track) -> Option<(String, HashMap<String, String>, u64)> {
        self.resolve_stream_with(track, true).await
    }

    /// Звичайні посилання не грають — годину беремо звук запасним способом.
    pub fn stream_failed(&self) {
        let mut u = self.alt_until.lock().unwrap();
        if now_secs() > *u {
            tracing::warn!(
                "Звук зі звичайного посилання не відтворюється — годину беру його без входу Google{}",
                if self.deno.lock().unwrap().is_none() { " (Deno ще не встановлено)" } else { "" }
            );
        }
        *u = now_secs() + ALT_FOR;
    }

    async fn resolve_stream_with(&self, track: &Track, alt: bool) -> Option<(String, HashMap<String, String>, u64)> {
        let url = track.url();
        let args = ["--no-playlist", "-f", "bestaudio[acodec=opus]/bestaudio/best", "-O", "%(.{url,duration,http_headers,requested_formats})j", url.as_str()];
        let v = if alt {
            match self.ytdlp_with(&args, 60, true).await {
                Ok(out) => out.lines().rev().find(|l| l.trim_start().starts_with('{')).and_then(|l| serde_json::from_str::<Value>(l).ok()),
                Err(e) => {
                    tracing::warn!("yt-dlp (запасний спосіб): {}", first_line(&e));
                    None
                }
            }
        } else {
            self.ytdlp_json(&args, 60).await
        }?;
        let link = v["url"].as_str().or_else(|| v["requested_formats"][0]["url"].as_str())?.to_string();
        let headers = v["http_headers"].as_object().map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()).unwrap_or_default();
        Some((link, headers, v["duration"].as_f64().unwrap_or(0.0) as u64))
    }

    pub async fn playlist(&self, playlist: &str, shuffle: bool) -> Vec<Track> {
        if playlist.trim().is_empty() {
            return vec![];
        }
        let pid = PLAYLIST_RE.captures(playlist).map(|c| c[1].to_string()).unwrap_or_else(|| playlist.trim().to_string());
        // спершу кеш: музика починає грати одразу, без очікування на YouTube
        let mut out = match load_cached_playlist(&self.cache_dir, &pid) {
            Some((saved_at, tracks)) => {
                if now_secs() - saved_at > PLAYLIST_REFRESH {
                    self.refresh_in_background(&pid);
                }
                tracing::info!("Фоновий плейлист із кешу: {} треків", tracks.len());
                tracks
            }
            None => self.fetch_playlist(&pid).await,
        };
        if shuffle {
            use rand::seq::SliceRandom;
            out.shuffle(&mut rand::rng());
        }
        out
    }

    /// Перші PLAYLIST_LIMIT треків плейлиста з YouTube Music; результат іде в кеш.
    async fn fetch_playlist(&self, pid: &str) -> Vec<Track> {
        let url = format!("https://music.youtube.com/playlist?list={pid}");
        let end = PLAYLIST_LIMIT.to_string();
        let Some(v) = self.ytdlp_json(&["--flat-playlist", "-J", "--playlist-end", &end, &url], 120).await else { return vec![] };
        let out: Vec<Track> = flat_entries(&v, "fallback").into_iter().take(PLAYLIST_LIMIT).collect();
        if !out.is_empty() {
            save_cached_playlist(&self.cache_dir, pid, &out);
        }
        tracing::info!("Фоновий плейлист: {} треків (збережено в кеш на 7 днів)", out.len());
        out
    }

    fn refresh_in_background(&self, pid: &str) {
        let Some(me) = self.me.get().and_then(|w| w.upgrade()) else { return };
        if !self.refreshing.lock().unwrap().insert(pid.to_string()) {
            return; // уже оновлюється
        }
        let pid = pid.to_string();
        tokio::spawn(async move {
            me.fetch_playlist(&pid).await;
            me.refreshing.lock().unwrap().remove(&pid);
        });
    }

    pub async fn playlist_title(&self, playlist_id: &str) -> Option<String> {
        let url = format!("https://music.youtube.com/playlist?list={playlist_id}");
        let v = self.ytdlp_json(&["--flat-playlist", "-J", "--playlist-end", "1", &url], 30).await?;
        v["title"].as_str().filter(|s| !s.is_empty()).map(String::from)
    }

    /// Лайк (або зняти лайк) у вашому акаунті YouTube Music.
    pub async fn rate(&self, video_id: &str, like: bool) -> Result<(), MusicError> {
        self.refresh_cookies(false).await;
        if self.cookie_source().is_none() {
            return Err(MusicError::LoginRequired);
        }
        let endpoint = if like { "like/like" } else { "like/removelike" };
        match self.innertube(endpoint, json!({"target": {"videoId": video_id}}), true).await {
            Err(MusicError::LoginRequired) => {
                // можливо, cookies застаріли — переносимо вхід з браузера ще раз
                self.refresh_cookies(true).await;
                self.innertube(endpoint, json!({"target": {"videoId": video_id}}), true).await.map(|_| ())
            }
            r => r.map(|_| ()),
        }
    }
}

fn flat_entries(v: &Value, source: &str) -> Vec<Track> {
    v["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let vid = e["id"].as_str().filter(|id| id.len() == 11)?;
                    Some(Track {
                        video_id: vid.into(),
                        title: e["title"].as_str().unwrap_or("Без назви").into(),
                        artist: e["uploader"].as_str().or(e["channel"].as_str()).unwrap_or("").trim_end_matches(" - Topic").into(),
                        duration: e["duration"].as_f64().unwrap_or(0.0) as u64,
                        cover: best_cover(Some(&e["thumbnails"]), vid),
                        source: source.into(),
                        ..Default::default()
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(300).collect()
}

/// Завантажує файл (з переходами за посиланнями) і кладе його на місце лише цілим.
pub async fn download(http: &reqwest::Client, url: &str, dest: &Path) -> Result<(), String> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    let r = http.get(url).header("User-Agent", "pipabobik").timeout(Duration::from_secs(600)).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status()));
    }
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = dest.with_extension("part");
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    let mut s = r.bytes_stream();
    while let Some(chunk) = s.next().await {
        f.write_all(&chunk.map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

#[cfg(test)]
mod playlist_search_tests {
    use super::*;

    #[test]
    fn parses_playlist_results_and_skips_songs() {
        let pl = |browse: &str, title: &str, info: &str| json!({"musicResponsiveListItemRenderer": {
            "navigationEndpoint": {"browseEndpoint": {"browseId": browse}},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Playlist"}, {"text": " • "}, {"text": info}]}}}
            ]}});
        let overlay_only = json!({"musicResponsiveListItemRenderer": {
            "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {"playNavigationEndpoint": {"watchPlaylistEndpoint": {"playlistId": "RDCLAK5uy_xmas123456"}}}}}},
            "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Christmas Hits"}]}}}]}});
        let song = json!({"musicResponsiveListItemRenderer": {"playlistItemData": {"videoId": "abcdefghijk"},
            "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Щедрик"}]}}}]}});
        let data = json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [
            {"musicShelfRenderer": {"contents": [pl("VLPLxmas0000000001", "Різдвяні пісні", "YouTube Music • 60 songs"), song, overlay_only, pl("VLPLxmas0000000001", "дубль", "")]}}
        ]}}}}]}}});
        let got = parse_playlists(&data, 8);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], FoundPlaylist { id: "PLxmas0000000001".into(), title: "Різдвяні пісні".into(), info: "YouTube Music • 60 songs".into() });
        assert_eq!(got[1].id, "RDCLAK5uy_xmas123456");
        assert_eq!(got[1].title, "Christmas Hits");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Value {
        json!({"text": text})
    }
    fn link(text: &str, page: &str) -> Value {
        json!({"text": text, "navigationEndpoint": {"browseEndpoint": {"browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": page}}}}})
    }

    #[test]
    fn parses_search_results() {
        let item = json!({"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": "abcdefghijk"},
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [{"url": "https://lh3.googleusercontent.com/x=w60-h60", "width": 60, "height": 60}]}}},
            "badges": [{"musicInlineBadgeRenderer": {"icon": {"iconType": "MUSIC_EXPLICIT_BADGE"}}}],
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [run("Обійми")]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    link("Okean Elzy", "MUSIC_PAGE_TYPE_ARTIST"), run(" & "), link("Хтось", "MUSIC_PAGE_TYPE_ARTIST"),
                    run(" • "), link("Міра", "MUSIC_PAGE_TYPE_ALBUM"), run(" • "), run("4:21")]}}}
            ]}});
        let plain = json!({"musicResponsiveListItemRenderer": {
            "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {"playNavigationEndpoint": {"watchEndpoint": {"videoId": "zzzzzzzzzzz"}}}}}},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [run("Live")]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [run("Song"), run(" • "), run("Artist X"), run(" • "), run("1:02:03")]}}}
            ]}});
        let data = json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [
            {"musicCardShelfRenderer": {"contents": [{"musicResponsiveListItemRenderer": {"playlistItemData": {"videoId": "topcardxxxx"}, "flexColumns": []}}]}},
            {"musicShelfRenderer": {"contents": [item, plain]}}]}}}}]}}});
        let t = parse_search(&data, 8);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].video_id, "abcdefghijk");
        assert_eq!(t[0].artist, "Okean Elzy, Хтось");
        assert_eq!(t[0].album, "Міра");
        assert_eq!(t[0].duration, 261);
        assert!(t[0].explicit);
        assert!(t[0].cover.ends_with("=w544-h544"));
        assert_eq!(t[1].artist, "Artist X");
        assert_eq!(t[1].duration, 3723);
        assert_eq!(t[1].cover, "https://i.ytimg.com/vi/zzzzzzzzzzz/hqdefault.jpg");
    }

    #[test]
    fn cookies_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.txt");
        std::fs::write(&p, "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tabc\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tq\n.google.com\tTRUE\t/\tTRUE\t0\tNID\tz\n").unwrap();
        let c = read_cookies(&p);
        assert_eq!(c.get("SAPISID").map(String::as_str), Some("abc"));
        assert_eq!(c.get("SID").map(String::as_str), Some("q"));
        assert!(!c.contains_key("NID"));
        assert_eq!(sapisid_hash("abc", 1), "SAPISIDHASH 1_".to_string() + &hex::encode(Sha1::digest("1 abc https://music.youtube.com")));
    }

    #[test]
    fn flat() {
        let v = json!({"entries": [{"id": "abcdefghijk", "title": "T", "channel": "A - Topic", "duration": 200.0}, {"id": "short"}]});
        let t = flat_entries(&v, "fallback");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].artist, "A");
        assert_eq!(t[0].source, "fallback");
    }

    #[test]
    fn playlist_cache_lives_seven_days_and_keeps_100() {
        let dir = tempfile::tempdir().unwrap();
        let tracks: Vec<Track> = (0..150).map(|i| Track::new(&format!("id{i:09}"), &format!("T{i}"))).collect();
        save_cached_playlist(dir.path(), "LM", &tracks);
        let (_, got) = load_cached_playlist(dir.path(), "LM").unwrap();
        assert_eq!(got.len(), PLAYLIST_LIMIT);
        // кеш старший за 7 днів не використовується
        let path = playlist_file(dir.path(), "LM");
        let mut c: CachedPlaylist = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        c.saved_at -= CACHE_TTL + 10.0;
        std::fs::write(&path, serde_json::to_string(&c).unwrap()).unwrap();
        assert!(load_cached_playlist(dir.path(), "LM").is_none());
        assert!(playlist_file(dir.path(), "../../x").ends_with("x.json"));
    }

    #[test]
    fn search_cache_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let m = MusicSource { cache_dir: dir.path().to_path_buf(), ..MusicSource::new("", "", reqwest::Client::new()) };
        m.search_cache.lock().unwrap().insert(("обійми".into(), true), (now_secs(), vec![Track::new("abcdefghijk", "Обійми")]));
        m.save_search_cache();
        let m2 = MusicSource { cache_dir: dir.path().to_path_buf(), ..MusicSource::new("", "", reqwest::Client::new()) };
        m2.load_search_cache();
        assert_eq!(m2.search_cache.lock().unwrap().len(), 1);
    }
}
