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
const SEARCH_TTL: f64 = 15.0 * 60.0;
const COOKIES_TTL: Duration = Duration::from_secs(6 * 3600);
/// Фільтри пошуку YouTube Music («Пісні» / «Відео»), як у ytmusicapi.
const PARAMS_SONGS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";
const PARAMS_VIDEOS: &str = "EgWKAQIQAWoMEA4QChADEAQQCRAF";

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
        let exe = self.ensure_ytdlp().await.ok_or_else(|| "yt-dlp не знайдено".to_string())?;
        self.refresh_cookies(false).await;
        let _slot = self.slots.acquire().await.map_err(|e| e.to_string())?;
        // кожен запуск — зі своєю копією cookies: yt-dlp переписує файл наприкінці, і два одночасні запуски могли б його зіпсувати
        let mut tmp_cookie = None;
        if let Some(src) = self.cookie_source() {
            let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
            let tmp = paths::data_dir().join("tmp").join(format!("ck-{}-{n}.txt", std::process::id()));
            let _ = std::fs::create_dir_all(tmp.parent().unwrap());
            if std::fs::copy(&src, &tmp).is_ok() {
                tmp_cookie = Some(tmp);
            }
        }
        let mut cmd = hidden(&exe);
        cmd.args(["--ignore-config", "--no-warnings", "--socket-timeout", "15", "--extractor-args", "youtube:skip=hls,dash,translated_subs"]);
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
            let mut c = self.search_cache.lock().unwrap();
            if c.len() > 300 {
                c.clear();
            }
            c.insert(key, (now_secs(), found.clone()));
        }
        found
    }

    /// Готує все наперед, щоб перший «!трек» і перший трек не чекали на ініціалізацію.
    pub async fn warmup(&self) {
        self.ensure_ytdlp().await;
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
        let url = track.url();
        let v = self
            .ytdlp_json(
                &["--no-playlist", "-f", "bestaudio[acodec=opus]/bestaudio/best", "-O", "%(.{url,duration,http_headers,requested_formats})j", &url],
                60,
            )
            .await?;
        let link = v["url"].as_str().or_else(|| v["requested_formats"][0]["url"].as_str())?.to_string();
        let headers = v["http_headers"].as_object().map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()).unwrap_or_default();
        Some((link, headers, v["duration"].as_f64().unwrap_or(0.0) as u64))
    }

    pub async fn playlist(&self, playlist: &str, shuffle: bool) -> Vec<Track> {
        if playlist.trim().is_empty() {
            return vec![];
        }
        let pid = PLAYLIST_RE.captures(playlist).map(|c| c[1].to_string()).unwrap_or_else(|| playlist.trim().to_string());
        let url = format!("https://music.youtube.com/playlist?list={pid}");
        let Some(v) = self.ytdlp_json(&["--flat-playlist", "-J", &url], 120).await else { return vec![] };
        let mut out = flat_entries(&v, "fallback");
        if shuffle {
            use rand::seq::SliceRandom;
            out.shuffle(&mut rand::rng());
        }
        tracing::info!("Фоновий плейлист: {} треків", out.len());
        out
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
}
