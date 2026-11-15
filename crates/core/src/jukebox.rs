//! Черга, правила замовлень і цикл відтворення.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use futures::future::{FutureExt, Shared};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::access::Limits;
use crate::config::Config;
use crate::music::{MusicError, MusicSource};
use crate::player::{Outcome, Player};
use crate::track::{fmt_time, now_secs, Track};

pub const RESUME_ATTEMPTS: usize = 2; // скільки разів продовжувати трек, який YouTube обірвав посередині
pub const SKIP_GUARD: Duration = Duration::from_secs(3); // повторний «скіп» без адресата одразу після попереднього не чіпає наступний трек
pub const STREAM_TTL: f64 = 20.0 * 60.0; // скільки тримати наперед отримане посилання на аудіо
pub const PREFETCH_AHEAD: usize = 2; // скільки наступних треків готувати заздалегідь

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// Пряме посилання на аудіо, http-заголовки, тривалість.
pub type Stream = (String, HashMap<String, String>, u64);

/// Звідки брати аудіо (у тестах — підробка).
pub trait Source: Send + Sync + 'static {
    fn resolve_stream<'a>(&'a self, t: &'a Track) -> BoxFut<'a, Option<Stream>>;
    /// Запасне посилання, коли звичайне не відтворилось.
    fn resolve_stream_alt<'a>(&'a self, _t: &'a Track) -> BoxFut<'a, Option<Stream>> {
        Box::pin(async { None })
    }
    /// Звичайне посилання не відтворилось — далі якийсь час брати запасні.
    fn stream_failed(&self) {}
    fn playlist<'a>(&'a self, pl: &'a str, shuffle: bool) -> BoxFut<'a, Vec<Track>>;
    fn rate<'a>(&'a self, video_id: &'a str, like: bool) -> BoxFut<'a, Result<(), MusicError>>;
}

/// Чим грати (у тестах — підробка).
pub trait Playback: Send + Sync + 'static {
    fn play<'a>(&'a self, url: &'a str, headers: &'a HashMap<String, String>, start: f64) -> BoxFut<'a, Outcome>;
    fn stop(&self) -> BoxFut<'_, ()>;
    fn position(&self) -> f64;
    fn paused(&self) -> bool;
    fn volume(&self) -> i64;
    fn set_volume(&self, v: i64) -> BoxFut<'_, ()>;
    fn seek(&self, pos: f64) -> BoxFut<'_, bool>;
    fn set_pause(&self, p: bool) -> BoxFut<'_, ()>;
    fn last_played(&self) -> f64;
    fn last_error(&self) -> String;
}

impl Source for MusicSource {
    fn resolve_stream<'a>(&'a self, t: &'a Track) -> BoxFut<'a, Option<Stream>> {
        Box::pin(MusicSource::resolve_stream(self, t))
    }
    fn resolve_stream_alt<'a>(&'a self, t: &'a Track) -> BoxFut<'a, Option<Stream>> {
        Box::pin(MusicSource::resolve_stream_alt(self, t))
    }
    fn stream_failed(&self) {
        MusicSource::stream_failed(self)
    }
    fn playlist<'a>(&'a self, pl: &'a str, shuffle: bool) -> BoxFut<'a, Vec<Track>> {
        Box::pin(MusicSource::playlist(self, pl, shuffle))
    }
    fn rate<'a>(&'a self, video_id: &'a str, like: bool) -> BoxFut<'a, Result<(), MusicError>> {
        Box::pin(MusicSource::rate(self, video_id, like))
    }
}

impl Playback for Player {
    fn play<'a>(&'a self, url: &'a str, headers: &'a HashMap<String, String>, start: f64) -> BoxFut<'a, Outcome> {
        Box::pin(Player::play(self, url, headers, start))
    }
    fn stop(&self) -> BoxFut<'_, ()> {
        Box::pin(Player::stop(self))
    }
    fn position(&self) -> f64 {
        Player::position(self)
    }
    fn paused(&self) -> bool {
        Player::paused(self)
    }
    fn volume(&self) -> i64 {
        Player::volume(self)
    }
    fn set_volume(&self, v: i64) -> BoxFut<'_, ()> {
        Box::pin(Player::set_volume(self, v))
    }
    fn seek(&self, pos: f64) -> BoxFut<'_, bool> {
        Box::pin(Player::seek(self, pos))
    }
    fn set_pause(&self, p: bool) -> BoxFut<'_, ()> {
        Box::pin(Player::set_pause(self, p))
    }
    fn last_played(&self) -> f64 {
        Player::last_played(self)
    }
    fn last_error(&self) -> String {
        Player::last_error(self)
    }
}

static BRACKETS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[(\[{].*?[)\]}]").unwrap());
static FEAT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s+(feat\.?|ft\.?|x|&|and|і|та)\s+.*$|,.*$").unwrap());
static NON_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w']+").unwrap());

/// Нижній регістр, без розділових знаків; ё/е та апострофи не заважають збігу.
pub fn norm(text: &str) -> String {
    let t = text.to_lowercase().replace('ё', "е").replace(['’', 'ʼ'], "'");
    NON_WORD.replace_all(&t, " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Ключ виконавця в бан-листі: «Океан Ельзи» і «ОКЕАН ЕЛЬЗИ» — один і той самий.
pub fn artist_key(name: &str) -> String {
    format!("artist:{}", crate::rufilter::norm(name).trim_end_matches(" - topic"))
}

/// Головний виконавець треку: «A feat. B» → A.
pub fn main_artist(t: &Track) -> Option<String> {
    crate::rufilter::split_artists(&t.artist).into_iter().next().filter(|a| !a.trim().is_empty())
}

/// Ключ пісні без версій: «Обійми (Official Video)» і «Обійми (Live)» — одна пісня.
pub fn song_key(t: &Track) -> String {
    let clean = BRACKETS.replace_all(&t.title, " ").into_owned();
    let title = if t.title.contains(" - ") && t.artist.is_empty() { clean.rsplit(" - ").next().unwrap_or(&clean).to_string() } else { clean };
    let artist = norm(&FEAT.replace(&t.artist, ""));
    format!("song:{artist}|{}", norm(&title))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    Now(u64),
    Queue(usize),
    History,
}

pub type SayFn = Arc<dyn Fn(String) -> BoxFut<'static, ()> + Send + Sync>;
pub type FilterFn = Arc<dyn Fn(Track) -> BoxFut<'static, Option<String>> + Send + Sync>;
type StreamFut = Shared<BoxFut<'static, Option<Stream>>>;

#[derive(Default)]
struct St {
    queue: Vec<Track>,
    now: Option<(u64, Track)>,
    play_seq: u64,
    loading: bool,
    history: VecDeque<Track>,
    banned_ids: HashSet<String>,
    banned_meta: BTreeMap<String, String>,
    allowed_ids: HashSet<String>, // розбанені вручну — фільтр їх більше не чіпає
    liked_ids: HashSet<String>,   // лайкнуті з застосунку в YouTube Music
    last_request: HashMap<String, f64>,
    fallback: Vec<Track>,
    fallback_loaded_at: f64,
    fallback_gen: u64,
    // який саме трек треба пропустити: так запізнілий «скіп» не зачепить наступний трек
    skip_target: Option<u64>,
    last_skip_at: Option<Instant>,
    streams: HashMap<String, (Stream, f64)>,
    stream_tasks: HashMap<String, StreamFut>,
    prefetch: Option<tokio::task::JoinHandle<()>>,
}

pub struct Jukebox {
    pub cfg: Arc<RwLock<Config>>,
    pub music: Arc<dyn Source>,
    pub player: Arc<dyn Playback>,
    data_dir: PathBuf,
    st: Mutex<St>,
    active: watch::Sender<bool>,
    wake: watch::Sender<u64>,
    changes: watch::Sender<u64>,
    pub say: Mutex<Option<SayFn>>,
    pub content_filter: Mutex<Option<FilterFn>>,
    pub announce: std::sync::atomic::AtomicBool,
}

impl Jukebox {
    pub fn new(cfg: Arc<RwLock<Config>>, music: Arc<dyn Source>, player: Arc<dyn Playback>, data_dir: PathBuf) -> Arc<Self> {
        let jb = Arc::new(Self {
            cfg,
            music,
            player,
            data_dir,
            st: Mutex::new(St::default()),
            active: watch::channel(false).0,
            wake: watch::channel(0).0,
            changes: watch::channel(0).0,
            say: Mutex::new(None),
            content_filter: Mutex::new(None),
            announce: std::sync::atomic::AtomicBool::new(false),
        });
        jb.load();
        jb
    }

    fn rule_i(&self, key: &str) -> u64 {
        self.cfg.read().unwrap().int("rules", key).max(0) as u64
    }
    fn rule_b(&self, key: &str, default: bool) -> bool {
        self.cfg.read().unwrap().raw["rules"][key].as_bool().unwrap_or(default)
    }

    // ── збереження ──
    fn file(&self, name: &str) -> PathBuf {
        self.data_dir.join(name)
    }

    fn load(&self) {
        let mut st = self.st.lock().unwrap();
        let read = |n: &str| std::fs::read_to_string(self.file(n)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(Value::Array(a)) = read("queue.json") {
            st.queue = a.iter().filter_map(Track::from_json).collect();
            for t in st.queue.iter_mut() {
                // старі тимчасові посилання на кадри з відео з часом перестають працювати
                if t.cover.contains("i.ytimg.com") {
                    t.cover = format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", t.video_id);
                }
            }
            if !st.queue.is_empty() {
                tracing::info!("Відновлено чергу: {} треків", st.queue.len());
            }
        }
        match read("banned.json") {
            Some(Value::Object(m)) => {
                st.banned_meta = m.into_iter().map(|(k, v)| (k, v.as_str().unwrap_or("").to_string())).collect();
            }
            Some(Value::Array(a)) => {
                // старий формат — просто список id
                st.banned_meta = a.iter().filter_map(|v| v.as_str()).map(|v| (v.to_string(), v.to_string())).collect();
            }
            _ => {}
        }
        st.banned_ids = st.banned_meta.keys().cloned().collect();
        let set = |v: Option<Value>| -> HashSet<String> {
            v.and_then(|v| v.as_array().cloned()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        st.allowed_ids = set(read("allowed.json"));
        st.liked_ids = set(read("liked.json"));
    }

    pub fn save(&self) {
        let st = self.st.lock().unwrap();
        self.save_locked(&st);
    }

    fn save_locked(&self, st: &St) {
        let _ = std::fs::create_dir_all(&self.data_dir);
        let mut items = st.queue.clone();
        if let Some((_, now)) = &st.now {
            // якщо бота закривають посеред треку — не губимо його
            if now.source == "request" && !items.iter().any(|t| t == now) {
                items.insert(0, now.clone());
            }
        }
        let sorted = |s: &HashSet<String>| {
            let mut v: Vec<&String> = s.iter().collect();
            v.sort();
            json!(v)
        };
        let mut meta = serde_json::Map::new();
        let mut ids: Vec<&String> = st.banned_ids.iter().collect();
        ids.sort();
        for id in ids {
            meta.insert(id.clone(), json!(st.banned_meta.get(id).cloned().unwrap_or_else(|| id.clone())));
        }
        let write = |n: &str, v: Value| {
            let path = self.file(n);
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, v.to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        };
        write("queue.json", Value::Array(items.iter().map(Track::to_json).collect()));
        write("banned.json", Value::Object(meta));
        write("allowed.json", sorted(&st.allowed_ids));
        write("liked.json", sorted(&st.liked_ids));
    }

    /// Застосунок і віджет отримують новий стан.
    pub fn changed(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    // ── доступ до стану ──
    pub fn is_active(&self) -> bool {
        *self.active.borrow()
    }
    pub fn now(&self) -> Option<(u64, Track)> {
        self.st.lock().unwrap().now.clone()
    }
    pub fn queue(&self) -> Vec<Track> {
        self.st.lock().unwrap().queue.clone()
    }
    pub fn queue_len(&self) -> usize {
        self.st.lock().unwrap().queue.len()
    }
    pub fn is_liked(&self, vid: &str) -> bool {
        self.st.lock().unwrap().liked_ids.contains(vid)
    }

    // ── правила ──
    /// Причина відмови або None, якщо трек можна додати.
    /// limits — ліміти ролі глядача (див. access.rs); без них діють загальні правила.
    pub fn check(&self, track: &Track, privileged: bool, limits: Option<&Limits>) -> Option<String> {
        if let Some(r) = self.banned_reason(track) {
            return Some(r);
        }
        let text = format!("{} {}", track.title, track.artist).to_lowercase();
        let (words, block_explicit) = {
            let c = self.cfg.read().unwrap();
            (c.str_list("rules", "banned_words"), c.bool("rules", "block_explicit"))
        };
        if words.iter().any(|w| !w.is_empty() && text.contains(w.as_str())) {
            return Some("цей трек заборонений на каналі 🚫".into());
        }
        if block_explicit && track.explicit {
            return Some(format!("{} позначений як Explicit — на цьому каналі такі не граємо", track.label()));
        }
        let max_d = limits.map(|l| l.max_duration).unwrap_or_else(|| self.rule_i("max_duration"));
        if max_d > 0 && track.duration > max_d && !privileged {
            return Some(format!("{} задовгий ({}), максимум {}", track.label(), fmt_time(track.duration as f64), fmt_time(max_d as f64)));
        }
        let st = self.st.lock().unwrap();
        if st.now.as_ref().map(|(_, t)| t.video_id == track.video_id).unwrap_or(false) || st.queue.iter().any(|t| t.video_id == track.video_id) {
            return Some(format!("{} вже є в черзі 😉", track.label()));
        }
        None
    }

    pub fn check_user(&self, user_id: &str, privileged: bool, limits: Option<&Limits>) -> Option<String> {
        if privileged {
            return None;
        }
        let lim = limits.copied().unwrap_or_else(|| crate::access::limits_for_all(&self.cfg.read().unwrap().raw["rules"]));
        let max_queue = self.cfg.read().unwrap().raw["rules"]["max_queue"].as_u64().unwrap_or(30) as usize;
        let st = self.st.lock().unwrap();
        if st.queue.len() >= max_queue && !lim.bypass_queue {
            return Some("черга переповнена, спробуй трохи згодом 🙏".into());
        }
        if lim.max_per_user > 0 && st.queue.iter().filter(|t| t.requested_by_id == user_id).count() as u64 >= lim.max_per_user {
            return Some(format!("у тебе вже {} трек(и) в черзі — дочекайся їх 🎧", lim.max_per_user));
        }
        let left = lim.cooldown as f64 - (now_secs() - st.last_request.get(user_id).copied().unwrap_or(0.0));
        if lim.cooldown > 0 && left > 0.0 {
            return Some(format!("зачекай ще {} с перед наступним замовленням ⏳", left as u64 + 1));
        }
        None
    }

    pub fn add(self: &Arc<Self>, mut track: Track, user_id: &str, login: &str, front: bool) -> usize {
        track.requested_by = login.into();
        track.requested_by_id = user_id.into();
        track.source = "request".into();
        track.added_at = now_secs();
        let interrupt = self.rule_b("interrupt_fallback", true);
        let (pos, fallback_now, playing) = {
            let mut st = self.st.lock().unwrap();
            let pos = if front {
                st.queue.insert(0, track);
                1
            } else {
                st.queue.push(track);
                st.queue.len()
            };
            st.last_request.insert(user_id.into(), now_secs());
            let fb = st.now.as_ref().filter(|(_, t)| t.source == "fallback").map(|(id, _)| *id);
            self.save_locked(&st);
            (pos, fb, st.now.is_some())
        };
        // замовлення перебиває фонову музику
        if let (Some(id), true) = (fallback_now, interrupt) {
            let me = self.clone();
            tokio::spawn(async move {
                me.skip("замовлення перебиває фонову музику", Some(id)).await;
            });
        }
        self.wake();
        if pos <= PREFETCH_AHEAD && playing {
            self.prefetch(); // цей трек скоро заграє — готуємо його заздалегідь
        }
        self.changed();
        pos
    }

    /// Скільки секунд до початку треку на позиції position (з 1).
    pub fn eta(&self, position: usize) -> u64 {
        let interrupt = self.rule_b("interrupt_fallback", true);
        let st = self.st.lock().unwrap();
        let mut sec = 0u64;
        if let Some((_, t)) = &st.now {
            if !(t.source == "fallback" && interrupt) {
                let d = if t.duration > 0 { t.duration } else { 180 };
                sec += d.saturating_sub(self.player.position() as u64);
            }
        }
        for t in st.queue.iter().take(position.saturating_sub(1)) {
            sec += if t.duration > 0 { t.duration } else { 180 };
        }
        sec
    }

    fn mutate<R>(&self, f: impl FnOnce(&mut St) -> R) -> R {
        let r = {
            let mut st = self.st.lock().unwrap();
            let r = f(&mut st);
            self.save_locked(&st);
            r
        };
        self.changed();
        r
    }

    pub fn remove_last_by(&self, user_id: &str) -> Option<Track> {
        let idx = self.st.lock().unwrap().queue.iter().rposition(|t| t.requested_by_id == user_id)?;
        Some(self.mutate(|st| {
            st.last_request.remove(user_id);
            st.queue.remove(idx)
        }))
    }

    pub fn remove_at(&self, index: i64) -> Option<Track> {
        if index < 0 || index as usize >= self.queue_len() {
            return None;
        }
        Some(self.mutate(|st| st.queue.remove(index as usize)))
    }

    pub fn move_up(&self, index: i64) {
        self.move_to(index, index - 1);
    }

    pub fn move_to(&self, index: i64, to: i64) {
        let n = self.queue_len() as i64;
        if (0..n).contains(&index) && (0..n).contains(&to) && index != to {
            self.mutate(|st| {
                let t = st.queue.remove(index as usize);
                st.queue.insert(to as usize, t);
            });
        }
    }

    pub fn clear(&self) {
        self.mutate(|st| st.queue.clear());
    }

    pub fn unban(&self, video_id: &str) {
        self.mutate(|st| {
            st.banned_ids.remove(video_id);
            st.banned_meta.remove(video_id);
            if !video_id.contains(':') {
                st.allowed_ids.insert(video_id.into());
            }
        });
    }

    pub fn ban_track(&self, track: &Track, reason: &str) {
        self.mutate(|st| {
            st.banned_ids.insert(track.video_id.clone());
            let label = if reason.is_empty() { track.label() } else { format!("{} · {reason}", track.label()) };
            st.banned_meta.insert(track.video_id.clone(), label);
            st.queue.retain(|t| t.video_id != track.video_id);
        });
    }

    fn banned_in(st: &St, t: &Track) -> bool {
        st.banned_ids.contains(&t.video_id) || st.banned_ids.contains(&song_key(t)) || Self::banned_artist(st, t).is_some()
    }

    /// Заборонений виконавець цього треку (з усіх, хто вказаний: «A feat. B» — і A, і B).
    fn banned_artist(st: &St, t: &Track) -> Option<String> {
        if t.artist.trim().is_empty() {
            return None;
        }
        std::iter::once(t.artist.clone())
            .chain(crate::rufilter::split_artists(&t.artist))
            .find(|a| st.banned_ids.contains(&artist_key(a)))
    }

    /// Чому трек не можна грати через бан-лист (None — можна).
    pub fn banned_reason(&self, t: &Track) -> Option<String> {
        let st = self.st.lock().unwrap();
        if let Some(a) = Self::banned_artist(&st, t) {
            return Some(format!("виконавець {a} заборонений на каналі 🚫"));
        }
        Self::banned_in(&st, t).then(|| format!("{} у бан-листі каналу 🚫", t.label()))
    }

    /// Забороняє виконавця: усі його треки (і спільні — «feat.») більше не гратимуть.
    /// Повертає, скільки треків прибрано з черги.
    pub fn ban_artist(&self, name: &str) -> Option<usize> {
        let name = name.trim().trim_end_matches(" - Topic").trim().to_string();
        let key = artist_key(&name);
        if key == "artist:" {
            return None;
        }
        Some(self.mutate(|st| {
            st.banned_ids.insert(key.clone());
            st.banned_meta.insert(key.clone(), format!("Виконавець: {name}"));
            let before = st.queue.len();
            let q = std::mem::take(&mut st.queue);
            st.queue = q.into_iter().filter(|t| !Self::banned_in(st, t)).collect();
            before - st.queue.len()
        }))
    }

    /// Забороняє виконавця і пропускає поточний трек, якщо він його.
    pub async fn ban_artist_now(&self, name: &str) -> Option<(usize, bool)> {
        let removed = self.ban_artist(name)?;
        let mut skipped = false;
        if let Some((id, t)) = self.now() {
            if self.is_banned(&t) {
                skipped = self.skip(&format!("виконавця {name} заборонено"), Some(id)).await.is_some();
            }
        }
        Some((removed, skipped))
    }

    pub fn is_banned(&self, track: &Track) -> bool {
        Self::banned_in(&self.st.lock().unwrap(), track)
    }

    /// Забороняє пісню за назвою й виконавцем — разом з іншими версіями (кліп, lyric video, live).
    pub fn ban_song(&self, track: &Track) -> String {
        let key = song_key(track);
        self.mutate(|st| {
            st.banned_ids.insert(key.clone());
            st.banned_meta.insert(key.clone(), track.label());
            let q = std::mem::take(&mut st.queue);
            st.queue = q.into_iter().filter(|t| !Self::banned_in(st, t)).collect();
        });
        key
    }

    /// Трек за id: черга, потім історія.
    pub fn find_by_id(&self, video_id: &str) -> Option<Track> {
        let st = self.st.lock().unwrap();
        st.queue.iter().chain(st.history.iter().rev()).find(|t| t.video_id == video_id).cloned()
    }

    /// Трек за назвою: спершу той, що грає, потім черга, потім нещодавно зіграні. «3» — третій у черзі.
    pub fn find(&self, query: &str) -> Option<(Place, Track)> {
        let st = self.st.lock().unwrap();
        let q = query.trim();
        if let Ok(n) = q.parse::<usize>() {
            if n >= 1 && n <= st.queue.len() {
                return Some((Place::Queue(n - 1), st.queue[n - 1].clone()));
            }
        }
        let words: Vec<String> = norm(q).split_whitespace().map(String::from).collect();
        if words.is_empty() {
            return None;
        }
        let joined = words.join(" ");
        let score = |t: &Track| -> u8 {
            let title = norm(&t.title);
            let hay = norm(&format!("{} {}", t.title, t.artist));
            if !words.iter().all(|w| hay.contains(w.as_str())) {
                return 0;
            }
            if title == joined {
                3
            } else if words.iter().all(|w| title.contains(w.as_str())) {
                2
            } else {
                1
            }
        };
        let mut places: Vec<(Place, &Track)> = Vec::new();
        if let Some((id, t)) = &st.now {
            places.push((Place::Now(*id), t));
        }
        places.extend(st.queue.iter().enumerate().map(|(i, t)| (Place::Queue(i), t)));
        places.extend(st.history.iter().rev().map(|t| (Place::History, t)));
        let mut best: Option<(u8, Place, Track)> = None;
        for (place, t) in places {
            let sc = score(t);
            // краща відповідність перемагає; за рівної — те, що раніше в списку (зараз → черга → історія)
            if sc > 0 && best.as_ref().map(|b| sc > b.0).unwrap_or(true) {
                best = Some((sc, place, t.clone()));
            }
        }
        best.map(|(_, p, t)| (p, t))
    }

    /// Автофільтр (російська музика). Розбанені вручну треки пропускає.
    pub async fn filter_reason(&self, track: &Track) -> Option<String> {
        if self.st.lock().unwrap().allowed_ids.contains(&track.video_id) {
            return None;
        }
        let f = self.content_filter.lock().unwrap().clone()?;
        f(track.clone()).await
    }

    /// Розбудити головний цикл (напр. з'явився фоновий плейлист, а до того була тиша).
    pub fn wake(&self) {
        self.wake.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn reset_fallback(&self) {
        let mut st = self.st.lock().unwrap();
        st.fallback.clear();
        st.fallback_loaded_at = 0.0;
        st.fallback_gen += 1;
        if let Some(h) = st.prefetch.take() {
            h.abort();
        }
    }

    // ── керування ──
    /// Пропускає поточний трек. only — пропустити лише якщо зараз грає саме він.
    ///
    /// Без only (команда в чаті) повторний скіп протягом SKIP_GUARD ігнорується:
    /// інакше два «!скіп» поспіль чи подвійне натискання пропускають і наступний трек.
    pub async fn skip(&self, reason: &str, only: Option<u64>) -> Option<Track> {
        let cur = {
            let mut st = self.st.lock().unwrap();
            let (id, t) = st.now.clone()?;
            if only.is_some_and(|o| o != id) {
                return None; // поки чекали, заграв інший трек — його не чіпаємо
            }
            if only.is_none() && st.last_skip_at.is_some_and(|t| t.elapsed() < SKIP_GUARD) {
                tracing::info!("Повторний пропуск одразу після попереднього — ігнорую ({})", if reason.is_empty() { "без причини" } else { reason });
                return None;
            }
            st.last_skip_at = Some(Instant::now());
            st.skip_target = Some(id);
            t
        };
        tracing::info!("⏭ Пропущено {} — {}", cur.label(), if reason.is_empty() { "без причини" } else { reason });
        self.player.stop().await;
        Some(cur)
    }

    pub fn recently_skipped(&self) -> bool {
        self.st.lock().unwrap().last_skip_at.is_some_and(|t| t.elapsed() < SKIP_GUARD)
    }

    // ── посилання на аудіо (з попереднім отриманням) ──
    fn stream_task(self: &Arc<Self>, track: &Track) -> StreamFut {
        let mut st = self.st.lock().unwrap();
        if let Some(f) = st.stream_tasks.get(&track.video_id) {
            return f.clone();
        }
        let me = self.clone();
        let t = track.clone();
        // окрема задача: доробить, навіть якщо того, хто чекав, уже скасували
        let handle = tokio::spawn(async move {
            let res = me.music.resolve_stream(&t).await;
            let mut st = me.st.lock().unwrap();
            st.stream_tasks.remove(&t.video_id);
            if let Some(r) = &res {
                st.streams.insert(t.video_id.clone(), (r.clone(), now_secs()));
            }
            res
        });
        let fut: BoxFut<'static, Option<Stream>> = Box::pin(async move { handle.await.ok().flatten() });
        let shared = fut.shared();
        st.stream_tasks.insert(track.video_id.clone(), shared.clone());
        shared
    }

    /// Посилання на аудіо: готове з кешу, те, що вже отримується, або нове.
    async fn resolve(self: &Arc<Self>, track: &Track) -> Option<Stream> {
        let hit = self.st.lock().unwrap().streams.remove(&track.video_id);
        if let Some((s, at)) = hit {
            if now_secs() - at < STREAM_TTL {
                return Some(s);
            }
        }
        let res = self.stream_task(track).await;
        self.st.lock().unwrap().streams.remove(&track.video_id); // використали — наступного разу свіже
        res
    }

    /// У фоні готує наступні треки, щоб після скіпу чи кінця треку не чекати.
    pub fn prefetch(self: &Arc<Self>) {
        let me = self.clone();
        let h = tokio::spawn(async move { me.do_prefetch().await });
        if let Some(old) = self.st.lock().unwrap().prefetch.replace(h) {
            old.abort();
        }
    }

    async fn do_prefetch(self: &Arc<Self>) {
        tokio::time::sleep(Duration::from_millis(200)).await; // даємо поточному треку спокійно стартувати
        let (mut targets, gen) = {
            let st = self.st.lock().unwrap();
            (st.queue.iter().take(PREFETCH_AHEAD).cloned().collect::<Vec<_>>(), st.fallback_gen)
        };
        if targets.len() < PREFETCH_AHEAD {
            if let Some(nxt) = self.next_fallback().await {
                let mut st = self.st.lock().unwrap();
                if gen == st.fallback_gen {
                    st.fallback.insert(0, nxt.clone()); // повертаємо: заграє наступним
                    targets.push(nxt);
                }
            }
        }
        {
            let mut st = self.st.lock().unwrap();
            let now = now_secs();
            st.streams.retain(|_, (_, at)| now - *at <= STREAM_TTL);
        }
        for t in targets {
            let have = self.st.lock().unwrap().streams.contains_key(&t.video_id);
            if !have {
                self.stream_task(&t).await;
            }
        }
    }

    pub async fn ban_current(&self) -> Option<Track> {
        let (id, cur) = self.now()?;
        self.ban_song(&cur);
        self.skip("трек заборонено", Some(id)).await;
        Some(cur)
    }

    /// Лайкає трек у YouTube Music (через ваш вхід Google) і запам'ятовує це.
    pub async fn like(&self, video_id: &str, like: bool) -> Result<(), MusicError> {
        self.music.rate(video_id, like).await?;
        self.mutate(|st| {
            if like {
                st.liked_ids.insert(video_id.into());
            } else {
                st.liked_ids.remove(video_id);
            }
        });
        Ok(())
    }

    pub async fn set_pause(&self, paused: bool) {
        self.player.set_pause(paused).await;
        self.changed();
    }

    pub async fn set_volume(&self, vol: i64) {
        self.player.set_volume(vol).await;
        self.changed();
    }

    /// Перемотує поточний трек. Повертає нову позицію або None.
    pub async fn seek(&self, position: f64) -> Option<f64> {
        let (_, now) = self.now()?;
        let mut pos = position.max(0.0);
        if now.duration > 0 {
            pos = pos.min((now.duration as f64 - 2.0).max(0.0)); // не в самий кінець, щоб трек не обірвався
        }
        if !self.player.seek(pos).await {
            return None;
        }
        self.changed();
        Some(pos)
    }

    pub fn activate(&self) {
        if !self.is_active() {
            tracing::info!("Музику ввімкнено");
        }
        self.active.send_replace(true);
        self.wake();
        self.changed();
    }

    pub async fn deactivate(&self) {
        if self.is_active() {
            tracing::info!("Музику вимкнено (стрім завершено)");
        }
        self.active.send_replace(false);
        {
            let mut st = self.st.lock().unwrap();
            if let Some((_, cur)) = st.now.take() {
                if cur.source == "request" && !st.queue.contains(&cur) {
                    st.queue.insert(0, cur); // дограємо наступного разу
                }
            }
        }
        self.player.stop().await;
        self.save();
        self.changed();
    }

    // ── стан для віджета ──
    pub fn state(&self) -> Value {
        let st = self.st.lock().unwrap();
        let now = st.now.as_ref().map(|(_, t)| {
            let mut v = t.to_json();
            v["elapsed"] = json!((self.player.position() * 100.0).round() / 100.0);
            v["url"] = json!(t.url());
            v["liked"] = json!(st.liked_ids.contains(&t.video_id));
            v
        });
        let status = if !self.is_active() {
            "offline"
        } else if st.loading {
            "loading"
        } else if st.now.is_some() && self.player.paused() {
            "paused"
        } else if st.now.is_some() {
            "playing"
        } else {
            "idle"
        };
        let history: Vec<Value> = st
            .history
            .iter()
            .rev()
            .take(8)
            .map(|t| {
                let mut v = t.to_json();
                v["liked"] = json!(st.liked_ids.contains(&t.video_id));
                v
            })
            .collect();
        let banned: Vec<Value> = st.banned_meta.iter().filter(|(k, _)| st.banned_ids.contains(*k)).map(|(k, v)| json!({"video_id": k, "label": v})).collect();
        json!({
            "status": status,
            "now": now,
            "queue": st.queue.iter().take(15).map(Track::to_json).collect::<Vec<_>>(),
            "queue_len": st.queue.len(),
            "volume": self.player.volume(),
            "active": self.is_active(),
            "history": history,
            "banned": banned,
        })
    }

    // ── фоновий плейлист ──
    async fn next_fallback(&self) -> Option<Track> {
        let (pl, shuffle) = {
            let c = self.cfg.read().unwrap();
            (c.str("google", "fallback_playlist").trim().to_string(), c.raw["google"]["fallback_shuffle"].as_bool().unwrap_or(true))
        };
        if pl.is_empty() {
            return None;
        }
        let need_load = {
            let mut st = self.st.lock().unwrap();
            let need = st.fallback.is_empty() && now_secs() - st.fallback_loaded_at > 300.0;
            if need {
                st.fallback_loaded_at = now_secs();
            }
            need.then_some(st.fallback_gen)
        };
        if let Some(gen) = need_load {
            let list = self.music.playlist(&pl, shuffle).await;
            let mut st = self.st.lock().unwrap();
            if st.fallback_gen == gen {
                st.fallback = list;
            }
        }
        loop {
            let t = {
                let mut st = self.st.lock().unwrap();
                if st.fallback.is_empty() {
                    return None;
                }
                st.fallback.remove(0)
            };
            if self.is_banned(&t) {
                continue;
            }
            if let Some(reason) = self.filter_reason(&t).await {
                tracing::info!("Фоновий плейлист: пропущено {} ({reason})", t.label());
                self.ban_track(&t, &reason);
                continue;
            }
            return Some(t);
        }
    }

    async fn say(&self, text: String) {
        let f = self.say.lock().unwrap().clone();
        if let Some(f) = f {
            f(text).await;
        }
    }

    // ── головний цикл ──
    pub async fn run(self: Arc<Self>) {
        let mut active_rx = self.active.subscribe();
        loop {
            if !self.is_active() {
                let _ = active_rx.wait_for(|a| *a).await;
                continue;
            }
            self.st.lock().unwrap().skip_target = None;
            let next = {
                let mut st = self.st.lock().unwrap();
                if st.queue.is_empty() {
                    None
                } else {
                    let t = st.queue.remove(0);
                    self.save_locked(&st);
                    Some(t)
                }
            };
            let track = match next {
                Some(t) => t,
                None => {
                    let mut wake_rx = self.wake.subscribe();
                    wake_rx.borrow_and_update();
                    match self.next_fallback().await {
                        None => {
                            self.st.lock().unwrap().now = None;
                            self.changed();
                            tokio::select! {
                                _ = wake_rx.changed() => {}
                                _ = active_rx.wait_for(|a| !*a) => {}
                            }
                            continue;
                        }
                        Some(t) => {
                            let mut st = self.st.lock().unwrap();
                            if !st.queue.is_empty() {
                                st.fallback.insert(0, t); // поки вантажився плейлист, хтось замовив трек
                                continue;
                            }
                            t
                        }
                    }
                }
            };
            self.play_one(track).await;
        }
    }

    fn is_skipped(&self, id: u64) -> bool {
        self.st.lock().unwrap().skip_target == Some(id)
    }

    async fn play_one(self: &Arc<Self>, track: Track) {
        let id = {
            let mut st = self.st.lock().unwrap();
            st.play_seq += 1;
            let id = st.play_seq;
            st.now = Some((id, track.clone()));
            st.loading = true;
            id
        };
        self.changed();
        let res = self.resolve(&track).await;
        self.st.lock().unwrap().loading = false;
        let clear_now = |me: &Self| {
            let mut st = me.st.lock().unwrap();
            if st.now.as_ref().map(|(i, _)| *i) == Some(id) {
                st.now = None;
            }
        };
        if !self.is_active() || self.is_skipped(id) {
            clear_now(self);
            self.changed();
            return;
        }
        let Some((mut url, mut headers, dur)) = res else {
            tracing::warn!("Не вдалося отримати аудіо для {}", track.label());
            if track.source == "request" {
                self.say(format!("@{} не вдалося відтворити {} (недоступний у регіоні чи видалений) — пропускаю 😔", track.requested_by, track.label())).await;
            }
            clear_now(self);
            self.changed();
            tokio::time::sleep(Duration::from_secs(1)).await;
            return;
        };
        let mut track = track;
        if track.duration == 0 && dur > 0 {
            track.duration = dur;
            if let Some((i, t)) = self.st.lock().unwrap().now.as_mut() {
                if *i == id {
                    t.duration = dur;
                }
            }
        }
        self.changed();
        if self.announce.load(std::sync::atomic::Ordering::Relaxed) {
            let who = if track.source == "request" { format!("замовив(ла) @{}", track.requested_by) } else { "з плейлиста стримерки".to_string() };
            let dur = if track.duration > 0 { crate::track::fmt_time(track.duration as f64) } else { String::new() };
            let text = crate::messages::render(&self.cfg.read().unwrap(), "announce", &[("трек", track.label()), ("хто", who), ("тривалість", dur), ("посилання", track.url())]);
            self.say(text).await;
        }
        tracing::info!("▶ {} [{}]", track.label(), track.source);
        self.prefetch();
        let mut start = 0.0;
        let mut outcome = Outcome::Ended;
        for attempt in 0..=RESUME_ATTEMPTS {
            outcome = self.player.play(&url, &headers, start).await;
            let played = self.player.last_played();
            if outcome == Outcome::Stopped || self.is_skipped(id) || !self.is_active() {
                break;
            }
            // YouTube інколи обриває потік посеред треку: mpv завершується, ніби трек скінчився.
            // Тоді беремо свіже посилання і продовжуємо з того самого місця.
            let cut = track.duration > 0 && played >= 3.0 && played < track.duration as f64 - 15.0;
            if !cut || attempt == RESUME_ATTEMPTS {
                if cut {
                    tracing::warn!("{} обірвався на {} з {} і не відновився", track.label(), fmt_time(played), fmt_time(track.duration as f64));
                }
                break;
            }
            let err = self.player.last_error();
            tracing::warn!(
                "{} обірвався на {} з {} (mpv: {}) — продовжую з того ж місця",
                track.label(),
                fmt_time(played),
                fmt_time(track.duration as f64),
                if err.is_empty() { "—" } else { &err }
            );
            let Some((u, h, _)) = self.music.resolve_stream(&track).await else { break };
            if self.is_skipped(id) || !self.is_active() {
                break;
            }
            url = u;
            headers = h;
            start = (played - 2.0).max(0.0);
        }
        // посилання не відтворилось зовсім — пробуємо запасне (без входу Google, інший клієнт YouTube)
        if outcome == Outcome::Error && !self.is_skipped(id) && self.is_active() {
            let err = self.player.last_error();
            tracing::warn!("{}: mpv не відтворив ({}) — пробую запасне посилання", track.label(), if err.is_empty() { "без пояснень" } else { &err });
            self.music.stream_failed();
            // уже підготовлені звичайні посилання теж, найімовірніше, не грають
            self.st.lock().unwrap().streams.clear();
            if let Some((u, h, _)) = self.music.resolve_stream_alt(&track).await {
                if !self.is_skipped(id) && self.is_active() {
                    outcome = self.player.play(&u, &h, 0.0).await;
                    if outcome == Outcome::Error {
                        let err = self.player.last_error();
                        tracing::error!("{} не відтворився і з запасного посилання: {}", track.label(), if err.is_empty() { "mpv без пояснень" } else { &err });
                    }
                }
            }
        }
        if outcome == Outcome::Error && track.source == "request" {
            let text = crate::messages::render(&self.cfg.read().unwrap(), "failed", &[("нік", track.requested_by.clone()), ("трек", track.label())]);
            self.say(text).await;
        }
        {
            let mut st = self.st.lock().unwrap();
            if outcome != Outcome::Stopped || st.skip_target == Some(id) {
                st.history.push_back(track.clone());
                while st.history.len() > 20 {
                    st.history.pop_front();
                }
            }
        }
        clear_now(self);
        self.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeSource {
        resolved: Mutex<Vec<String>>,
        delay: Duration,
        fallback: Vec<Track>,
    }
    impl Source for FakeSource {
        fn resolve_stream<'a>(&'a self, t: &'a Track) -> BoxFut<'a, Option<Stream>> {
            Box::pin(async move {
                self.resolved.lock().unwrap().push(t.video_id.clone());
                tokio::time::sleep(self.delay).await;
                Some((format!("http://{}", t.video_id), HashMap::new(), 0))
            })
        }
        fn playlist<'a>(&'a self, _pl: &'a str, _s: bool) -> BoxFut<'a, Vec<Track>> {
            Box::pin(async move { self.fallback.clone() })
        }
        fn rate<'a>(&'a self, _v: &'a str, _l: bool) -> BoxFut<'a, Result<(), MusicError>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// Програвач, що «грає» доти, доки його не зупинять (або length секунд).
    struct FakePlayer {
        played: Mutex<Vec<(String, f64)>>,
        stop: tokio::sync::Notify,
        length: f64,
        cut_at: Mutex<Option<f64>>,
        last: Mutex<f64>,
        plays: AtomicUsize,
    }
    impl FakePlayer {
        fn new(length: f64) -> Self {
            Self { played: Mutex::new(vec![]), stop: tokio::sync::Notify::new(), length, cut_at: Mutex::new(None), last: Mutex::new(0.0), plays: AtomicUsize::new(0) }
        }
    }
    impl Playback for FakePlayer {
        fn play<'a>(&'a self, url: &'a str, _h: &'a HashMap<String, String>, start: f64) -> BoxFut<'a, Outcome> {
            Box::pin(async move {
                self.played.lock().unwrap().push((url.to_string(), start));
                self.plays.fetch_add(1, Ordering::SeqCst);
                let cut = self.cut_at.lock().unwrap().take();
                if let Some(c) = cut {
                    *self.last.lock().unwrap() = c;
                    return Outcome::Ended;
                }
                tokio::select! {
                    _ = self.stop.notified() => { *self.last.lock().unwrap() = 1.0; Outcome::Stopped }
                    _ = tokio::time::sleep(Duration::from_secs_f64(self.length)) => { *self.last.lock().unwrap() = self.length; Outcome::Ended }
                }
            })
        }
        fn stop(&self) -> BoxFut<'_, ()> {
            Box::pin(async move { self.stop.notify_waiters() })
        }
        fn position(&self) -> f64 {
            0.0
        }
        fn paused(&self) -> bool {
            false
        }
        fn volume(&self) -> i64 {
            50
        }
        fn set_volume(&self, _v: i64) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
        fn seek(&self, _p: f64) -> BoxFut<'_, bool> {
            Box::pin(async { true })
        }
        fn set_pause(&self, _p: bool) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
        fn last_played(&self) -> f64 {
            *self.last.lock().unwrap()
        }
        fn last_error(&self) -> String {
            String::new()
        }
    }

    fn t(id: &str, title: &str, artist: &str) -> Track {
        Track { video_id: id.into(), title: title.into(), artist: artist.into(), duration: 200, ..Default::default() }
    }

    fn setup(player: Arc<FakePlayer>, src: Arc<FakeSource>) -> (Arc<Jukebox>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.set("google", "fallback_playlist", json!(""));
        let jb = Jukebox::new(Arc::new(RwLock::new(cfg)), src, player, dir.path().to_path_buf());
        (jb, dir)
    }

    fn src(delay_ms: u64) -> Arc<FakeSource> {
        Arc::new(FakeSource { resolved: Mutex::new(vec![]), delay: Duration::from_millis(delay_ms), fallback: vec![] })
    }

    async fn wait_now(jb: &Jukebox, id: &str) {
        for _ in 0..200 {
            if jb.now().map(|(_, t)| t.video_id == id).unwrap_or(false) && jb.player.last_played() >= 0.0 {
                if jb.state()["status"] == "playing" {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("не заграв {id}");
    }

    #[tokio::test]
    async fn song_keys_and_find() {
        let a = t("a", "Обійми (Official Video)", "Океан Ельзи feat. Хтось");
        let b = t("b", "Обійми [Live]", "Океан Ельзи");
        assert_eq!(song_key(&a), song_key(&b));
        let (jb, _d) = setup(Arc::new(FakePlayer::new(100.0)), src(0));
        jb.add(t("x", "Без бою", "Океан Ельзи"), "u", "u", false);
        jb.add(t("y", "Обійми", "Океан Ельзи"), "u", "u", false);
        assert_eq!(jb.find("обійми").unwrap().0, Place::Queue(1));
        assert_eq!(jb.find("2").unwrap().1.video_id, "y");
        jb.ban_song(&b);
        assert_eq!(jb.queue_len(), 1);
        assert!(jb.is_banned(&a));
        assert!(jb.check(&a, false, None).unwrap().contains("бан-листі"));
    }

    #[tokio::test]
    async fn bans_artists() {
        let (jb, _d) = setup(Arc::new(FakePlayer::new(100.0)), src(0));
        jb.add(t("a", "Обійми", "Океан Ельзи"), "u", "u", false);
        jb.add(t("b", "Пісня", "Хтось feat. ОКЕАН ЕЛЬЗИ"), "u", "u", false);
        jb.add(t("c", "Інша", "Хтось"), "u", "u", false);
        assert_eq!(jb.ban_artist("океан ельзи"), Some(2));
        assert_eq!(jb.queue_len(), 1);
        assert!(jb.check(&t("d", "Без бою", "Океан Ельзи"), true, None).unwrap().contains("виконавець"));
        assert!(jb.state()["banned"].as_array().unwrap().iter().any(|b| b["label"] == "Виконавець: океан ельзи"));
        jb.unban(&artist_key("Океан Ельзи"));
        assert!(jb.check(&t("d", "Без бою", "Океан Ельзи"), true, None).is_none());
        assert_eq!(jb.ban_artist("  "), None);
        assert_eq!(main_artist(&t("x", "y", "A feat. B")).as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn limits_and_persistence() {
        let player = Arc::new(FakePlayer::new(100.0));
        let (jb, dir) = setup(player.clone(), src(0));
        let lim = Limits { max_per_user: 1, cooldown: 0, max_duration: 100, bypass_queue: false };
        assert!(jb.check(&t("a", "A", ""), false, Some(&lim)).unwrap().contains("задовгий"));
        assert!(jb.check(&t("a", "A", ""), true, Some(&lim)).is_none());
        jb.add(t("a", "A", ""), "u1", "u1", false);
        assert!(jb.check_user("u1", false, Some(&lim)).unwrap().contains("вже 1"));
        assert!(jb.check(&t("a", "A", ""), false, None).unwrap().contains("вже є"));
        let jb2 = Jukebox::new(jb.cfg.clone(), src(0), player, dir.path().to_path_buf());
        assert_eq!(jb2.queue_len(), 1);
    }

    #[tokio::test]
    async fn skip_targets_only_current_and_guard() {
        let player = Arc::new(FakePlayer::new(100.0));
        let source = src(0);
        let (jb, _d) = setup(player.clone(), source.clone());
        for id in ["a", "b", "c", "d"] {
            jb.add(t(id, id, ""), "u", "u", false);
        }
        jb.activate();
        tokio::spawn(jb.clone().run());
        wait_now(&jb, "a").await;
        let (id_a, _) = jb.now().unwrap();
        assert!(jb.skip("тест", None).await.is_some());
        // одразу другий скіп без адресата — ігнорується
        assert!(jb.skip("тест", None).await.is_none());
        wait_now(&jb, "b").await;
        // запізнілий скіп для «a» не зачіпає «b»
        assert!(jb.skip("кнопка", Some(id_a)).await.is_none());
        assert_eq!(jb.now().unwrap().1.video_id, "b");
        // наступні треки готуються заздалегідь
        for _ in 0..100 {
            if source.resolved.lock().unwrap().contains(&"c".to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let resolved = source.resolved.lock().unwrap().clone();
        assert!(resolved.contains(&"c".to_string()));
        let h = jb.state()["history"].clone();
        assert_eq!(h[0]["video_id"], "a");
    }

    #[tokio::test]
    async fn resumes_cut_stream() {
        let player = Arc::new(FakePlayer::new(100.0));
        *player.cut_at.lock().unwrap() = Some(50.0);
        let (jb, _d) = setup(player.clone(), src(0));
        jb.add(t("a", "A", ""), "u", "u", false);
        jb.activate();
        tokio::spawn(jb.clone().run());
        for _ in 0..200 {
            if player.plays.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let played = player.played.lock().unwrap().clone();
        assert_eq!(played.len(), 2);
        assert_eq!(played[1].1, 48.0);
    }

    #[tokio::test]
    async fn request_interrupts_fallback() {
        let player = Arc::new(FakePlayer::new(100.0));
        let source = Arc::new(FakeSource { resolved: Mutex::new(vec![]), delay: Duration::ZERO, fallback: vec![{ let mut x = t("f", "F", ""); x.source = "fallback".into(); x }] });
        let (jb, _d) = setup(player, source);
        jb.cfg.write().unwrap().set("google", "fallback_playlist", json!("LM"));
        jb.activate();
        tokio::spawn(jb.clone().run());
        wait_now(&jb, "f").await;
        jb.add(t("r", "R", ""), "u", "u", false);
        wait_now(&jb, "r").await;
        jb.deactivate().await;
        assert_eq!(jb.queue()[0].video_id, "r"); // недограний трек повернувся в чергу
    }
}
