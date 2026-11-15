//! Команди чату.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::access::Access;
use crate::config::Config;
use crate::jukebox::{BoxFut, Jukebox, Place};
use crate::music::MusicSource;
use crate::track::{extract_video_id, fmt_time, Track};
use crate::twitch::ChatMessage;

/// Надіслати в чат: (текст, на яке повідомлення відповісти).
pub type Say = Arc<dyn Fn(String, Option<String>) -> BoxFut<'static, ()> + Send + Sync>;

/// «1:30», «90» — абсолютна позиція; «+30», «-1:00» — зсув від поточної.
pub fn parse_seek(arg: &str, current: f64) -> Option<f64> {
    let a: String = arg.trim().replace(',', ".").chars().filter(|c| *c != ' ').collect();
    if a.is_empty() {
        return None;
    }
    let (sign, rest) = match a.chars().next() {
        Some('+') => (1.0, &a[1..]),
        Some('-') => (-1.0, &a[1..]),
        _ => (0.0, a.as_str()),
    };
    let parts: Vec<f64> = rest.split(':').map(|x| x.parse::<f64>()).collect::<Result<_, _>>().ok()?;
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|x| *x < 0.0 || !x.is_finite()) {
        return None;
    }
    let sec = parts.iter().fold(0.0, |acc, x| acc * 60.0 + x);
    Some(if sign != 0.0 { current + sign * sec } else { sec })
}

pub fn minutes(sec: u64) -> String {
    if sec < 60 {
        "менш ніж хвилину".into()
    } else {
        format!("~{} хв", ((sec as f64) / 60.0).round() as u64)
    }
}

pub struct Commands {
    pub cfg: Arc<RwLock<Config>>,
    pub jb: Arc<Jukebox>,
    pub music: Arc<MusicSource>,
    say: Say,
    table: RwLock<HashMap<String, String>>,
}

const KEYS: &[&str] = &["request", "now", "skip", "queue", "wrong", "volume", "seek", "pause", "play", "ban", "ban_artist", "help"];

impl Commands {
    pub fn new(cfg: Arc<RwLock<Config>>, jb: Arc<Jukebox>, music: Arc<MusicSource>, say: Say) -> Arc<Self> {
        let me = Arc::new(Self { cfg, jb, music, say, table: RwLock::new(HashMap::new()) });
        me.rebuild();
        me
    }

    /// Перечитує назви команд (після змін у застосунку).
    pub fn rebuild(&self) {
        let cfg = self.cfg.read().unwrap();
        let mut table = HashMap::new();
        for key in KEYS {
            for alias in cfg.commands(key) {
                table.entry(alias.to_lowercase()).or_insert_with(|| key.to_string());
            }
        }
        *self.table.write().unwrap() = table;
    }

    pub fn first(&self, key: &str) -> String {
        self.cfg.read().unwrap().commands(key).into_iter().next().unwrap_or_else(|| format!("!{key}"))
    }

    /// Модератор, стрімерка або адмін з config.toml.
    pub fn privileged(&self, m: &ChatMessage) -> bool {
        m.is_broadcaster() || Access::new(&self.cfg.read().unwrap()).roles_of(m).contains("mods")
    }

    fn can(&self, m: &ChatMessage, key: &str) -> bool {
        Access::new(&self.cfg.read().unwrap()).can(m, key)
    }

    async fn reply(&self, m: &ChatMessage, text: impl Into<String>) {
        (self.say)(text.into(), Some(m.message_id.clone())).await;
    }

    /// Відповідь за шаблоном з налаштувань (⚙ → «Повідомлення в чат»); {нік} підставляється завжди.
    async fn reply_msg(&self, m: &ChatMessage, key: &str, mut vars: Vec<(&str, String)>) {
        vars.push(("нік", m.display_name.clone()));
        let text = crate::messages::render(&self.cfg.read().unwrap(), key, &vars);
        self.reply(m, text).await;
    }

    pub async fn handle(&self, m: ChatMessage) {
        if !m.text.starts_with('!') {
            return;
        }
        let mut parts = m.text.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next().unwrap_or("").trim().to_string();
        let Some(key) = self.table.read().unwrap().get(&name).cloned() else { return };
        tracing::info!("{}: {}", m.login, m.text);
        if !self.can(&m, &key) {
            return self.deny(&m, &key).await;
        }
        match key.as_str() {
            "request" => self.cmd_request(&m, &arg).await,
            "now" => self.cmd_now(&m).await,
            "skip" => self.cmd_skip(&m, &arg).await,
            "queue" => self.cmd_queue(&m).await,
            "wrong" => self.cmd_wrong(&m).await,
            "volume" => self.cmd_volume(&m, &arg).await,
            "seek" => self.cmd_seek(&m, &arg).await,
            "pause" => self.cmd_pause(&m).await,
            "play" => self.cmd_play(&m).await,
            "ban" => self.cmd_ban(&m, &arg).await,
            "ban_artist" => self.cmd_ban_artist(&m, &arg).await,
            "help" => self.cmd_help(&m).await,
            _ => {}
        }
    }

    // ── для всіх ──
    async fn cmd_request(&self, m: &ChatMessage, arg: &str) {
        if arg.is_empty() {
            return self.reply(m, format!("напиши назву: {} виконавець — пісня 🎵", self.first("request"))).await;
        }
        let lim = Access::new(&self.cfg.read().unwrap()).limits(m);
        if let Some(r) = self.jb.check_user(&m.user_id, false, Some(&lim)) {
            return self.reply(m, r).await;
        }
        let candidates: Vec<Track> = if let Some(vid) = extract_video_id(arg) {
            self.music.by_id(&vid).await.into_iter().collect()
        } else if arg.contains("list=") && arg.contains("http") {
            return self.reply(m, "плейлисти замовляти не можна — лише окремі треки 🙂").await;
        } else {
            let only = self.cfg.read().unwrap().raw["rules"]["only_songs"].as_bool().unwrap_or(true);
            self.music.search(arg, only).await
        };
        if candidates.is_empty() {
            let short: String = arg.chars().take(60).collect();
            return self.reply_msg(m, "not_found", vec![("запит", short)]).await;
        }
        let mut chosen = None;
        let mut first_reason = None;
        for t in candidates.into_iter().take(5) {
            match self.jb.check(&t, false, Some(&lim)) {
                None => {
                    if let Some(ru) = self.jb.filter_reason(&t).await {
                        // не підсовуємо інший трек замість забороненого — просто відмовляємо
                        self.jb.ban_track(&t, &ru);
                        return self.reply(m, format!("{} — {ru}. Російське тут не граємо 🇺🇦", t.label())).await;
                    }
                    chosen = Some(t);
                    break;
                }
                Some(r) => {
                    first_reason.get_or_insert(r);
                }
            }
        }
        let Some(chosen) = chosen else {
            return self.reply(m, first_reason.unwrap_or_else(|| "цей трек не можна додати".into())).await;
        };
        // повторна перевірка: поки шукали, ліміти могли змінитися
        if let Some(r) = self.jb.check_user(&m.user_id, false, Some(&lim)) {
            return self.reply(m, r).await;
        }
        let dur = if chosen.duration > 0 { fmt_time(chosen.duration as f64) } else { String::new() };
        let label = chosen.label();
        let pos = self.jb.add(chosen, &m.user_id, &m.display_name, false);
        let when = if self.jb.is_active() {
            let wait = self.jb.eta(pos);
            if wait < 15 { "грає наступним".to_string() } else { format!("заграє через {}", minutes(wait)) }
        } else {
            "заграє, коли почнеться музика".to_string()
        };
        self.reply_msg(m, "added", vec![("трек", label), ("тривалість", dur), ("місце", pos.to_string()), ("коли", when)]).await;
    }

    async fn cmd_now(&self, m: &ChatMessage) {
        let Some((_, t)) = self.jb.now() else {
            if !self.jb.is_active() {
                return self.reply(m, "музика зараз вимкнена 🔇").await;
            }
            return self.reply_msg(m, "now_empty", vec![("замовити", self.first("request"))]).await;
        };
        let pos = fmt_time(self.jb.player.position());
        let dur = if t.duration > 0 { fmt_time(t.duration as f64) } else { String::new() };
        let who = if t.source == "request" { format!("замовив(ла) @{}", t.requested_by) } else { "з плейлиста стримерки".into() };
        self.reply_msg(m, "now", vec![("трек", t.label()), ("позиція", pos), ("тривалість", dur), ("хто", who), ("посилання", t.url())]).await;
    }

    async fn cmd_queue(&self, m: &ChatMessage) {
        let q = self.jb.queue();
        if q.is_empty() {
            return self.reply_msg(m, "queue_empty", vec![("замовити", self.first("request"))]).await;
        }
        let items: Vec<String> = q.iter().take(4).enumerate().map(|(i, t)| format!("{}) {} (@{})", i + 1, t.label(), t.requested_by)).collect();
        let more = if q.len() > 4 { format!(" …і ще {}", q.len() - 4) } else { String::new() };
        self.reply_msg(m, "queue", vec![("список", format!("{}{more}", items.join(" · "))), ("кількість", q.len().to_string())]).await;
    }

    async fn cmd_wrong(&self, m: &ChatMessage) {
        match self.jb.remove_last_by(&m.user_id) {
            Some(t) => self.reply_msg(m, "wrong", vec![("трек", t.label())]).await,
            None => self.reply(m, "у тебе немає треків у черзі").await,
        }
    }

    pub fn help_text(&self, m: &ChatMessage) -> String {
        let c = |k: &str| self.first(k);
        let mut hints: Vec<(&str, String)> = vec![
            ("request", format!("{} назва — замовити", c("request"))),
            ("now", format!("{} — що грає", c("now"))),
            ("queue", format!("{} — черга", c("queue"))),
            ("wrong", format!("{} — прибрати свій трек", c("wrong"))),
            ("skip", format!("{} — пропустити", c("skip"))),
            ("volume", format!("{} 0-100", c("volume"))),
            ("seek", format!("{} 1:30 або +30", c("seek"))),
            ("pause", c("pause")),
            ("play", c("play")),
            ("ban", format!("{} [назва] — заборонити пісню", c("ban"))),
            ("ban_artist", format!("{} [виконавець] — заборонити виконавця", c("ban_artist"))),
        ];
        if self.privileged(m) {
            hints[4].1 = format!("{} [назва] — пропустити", c("skip"));
        }
        let cfg = self.cfg.read().unwrap().clone();
        let acc = Access::new(&cfg);
        // приховані стрімеркою команди не показуються в списку (але працюють, якщо дозволені)
        let hidden = cfg.str_list("help", "hidden");
        let parts: Vec<String> = hints
            .into_iter()
            .filter(|(k, _)| !cfg.commands(k).is_empty() && acc.can(m, k) && !hidden.iter().any(|h| h == k))
            .map(|(_, h)| h)
            .collect();
        let list = if parts.is_empty() { "тобі зараз доступна лише ця підказка".to_string() } else { parts.join(" · ") };
        let text = cfg.str("help", "text");
        let text = if text.trim().is_empty() { crate::config::DEFAULT_HELP.to_string() } else { text };
        text.replace(crate::config::HELP_PLACEHOLDER, &list).chars().take(500).collect()
    }

    /// Що побачить звичайний глядач (без значків) — для попереднього перегляду в застосунку.
    pub fn help_preview(&self) -> String {
        self.help_text(&ChatMessage { login: "viewer".into(), display_name: "viewer".into(), ..Default::default() })
    }

    async fn cmd_help(&self, m: &ChatMessage) {
        let text = self.help_text(m);
        self.reply(m, text).await;
    }

    // ── керування (хто саме може — налаштовується у вкладці «Команди») ──
    async fn deny(&self, m: &ChatMessage, key: &str) {
        if key == "request" {
            return self.reply(m, "замовлення зараз доступні не всім глядачам 🙈").await;
        }
        let allowed = Access::new(&self.cfg.read().unwrap()).roles_for(key);
        let names: HashMap<&str, &str> = [("subs", "саби"), ("vip", "VIP"), ("helpers", "помічники"), ("mods", "модератори")].into();
        let who: Vec<&str> = allowed.iter().filter_map(|r| names.get(r.as_str()).copied()).collect();
        let who = if who.is_empty() { String::new() } else { format!("{} і ", who.join(", ")) };
        self.reply(m, format!("це можуть лише {who}стрімер 😌")).await;
    }

    /// Без назви — пропускає те, що грає. З назвою (лише модератори й стрімер) — саме той трек.
    async fn cmd_skip(&self, m: &ChatMessage, arg: &str) {
        let cmd = m.text.split_whitespace().next().unwrap_or("").to_string();
        if !arg.is_empty() {
            if !self.privileged(m) {
                return self
                    .reply(m, format!("пропускати за назвою можуть лише модератори; просто {} — пропустити те, що грає", self.first("skip")))
                    .await;
            }
            return match self.jb.find(arg) {
                None | Some((Place::History, _)) => self.reply(m, format!("не знайшов «{arg}» ні серед того, що грає, ні в черзі 🤔")).await,
                Some((Place::Queue(idx), t)) => {
                    self.jb.remove_at(idx as i64);
                    self.reply(m, format!("🗑 прибрав з черги {}", t.label())).await
                }
                Some((Place::Now(id), t)) => {
                    if self.jb.skip(&format!("команда {cmd} від {}", m.login), Some(id)).await.is_none() {
                        return self.reply(m, "трек уже змінився — спробуй ще раз").await;
                    }
                    self.reply_msg(m, "skip", vec![("трек", t.label())]).await
                }
            };
        }
        if self.jb.now().is_some() && self.jb.recently_skipped() {
            return self.reply(m, "трек щойно пропустили — наступний уже вмикається ⏳").await;
        }
        match self.jb.skip(&format!("команда {cmd} від {}", m.login), None).await {
            Some(t) => self.reply_msg(m, "skip", vec![("трек", t.label())]).await,
            None => self.reply(m, "зараз нічого не грає").await,
        }
    }

    async fn cmd_volume(&self, m: &ChatMessage, arg: &str) {
        if arg.is_empty() {
            return self.reply_msg(m, "volume", vec![("гучність", self.jb.player.volume().to_string())]).await;
        }
        let Ok(v) = arg.trim_end_matches('%').trim().parse::<i64>() else {
            return self.reply(m, format!("формат: {} 0-100", self.first("volume"))).await;
        };
        self.jb.set_volume(v).await;
        self.reply_msg(m, "volume", vec![("гучність", self.jb.player.volume().to_string())]).await;
    }

    async fn cmd_seek(&self, m: &ChatMessage, arg: &str) {
        let Some((_, t)) = self.jb.now() else {
            return self.reply(m, "зараз нічого не грає").await;
        };
        let Some(target) = parse_seek(arg, self.jb.player.position()) else {
            return self.reply(m, format!("формат: {} 1:30 (на позицію) або +30 / -15 (секунд)", self.first("seek"))).await;
        };
        let Some(pos) = self.jb.seek(target).await else {
            return self.reply(m, "не вдалося перемотати 😕").await;
        };
        let dur = if t.duration > 0 { fmt_time(t.duration as f64) } else { String::new() };
        self.reply_msg(m, "seek", vec![("трек", t.label()), ("позиція", fmt_time(pos)), ("тривалість", dur)]).await;
    }

    async fn cmd_pause(&self, m: &ChatMessage) {
        if self.jb.now().is_none() {
            return self.reply(m, "зараз нічого не грає").await;
        }
        self.jb.set_pause(!self.jb.player.paused()).await;
        self.reply(m, if self.jb.player.paused() { "⏸ пауза" } else { "▶ граємо далі" }).await;
    }

    async fn cmd_play(&self, m: &ChatMessage) {
        if !self.jb.is_active() {
            // музика вмикається з початком трансляції (або вручну в застосунку — тестовий режим)
            return self.reply(m, "музика вмикається разом із трансляцією 🎬").await;
        }
        if self.jb.player.paused() {
            self.jb.set_pause(false).await;
        }
        self.reply(m, "▶ граємо").await;
    }

    /// Без імені — забороняє виконавця того, що грає. З іменем — саме цього виконавця.
    async fn cmd_ban_artist(&self, m: &ChatMessage, arg: &str) {
        let name = if arg.is_empty() {
            match self.jb.now().and_then(|(_, t)| crate::jukebox::main_artist(&t)) {
                Some(a) => a,
                None => return self.reply(m, format!("зараз нічого не грає. Щоб заборонити виконавця: {} ім'я", self.first("ban_artist"))).await,
            }
        } else {
            arg.to_string()
        };
        match self.jb.ban_artist_now(&name).await {
            None => self.reply(m, format!("напиши ім'я: {} виконавець", self.first("ban_artist"))).await,
            Some((removed, _)) => {
                let q = if removed > 0 { format!(", з черги прибрано треків: {removed}") } else { String::new() };
                self.reply_msg(m, "ban_artist", vec![("виконавець", name), ("прибрано", q)]).await
            }
        }
    }

    /// Без назви — забороняє те, що грає. З назвою — саме цю пісню (усі її версії).
    async fn cmd_ban(&self, m: &ChatMessage, arg: &str) {
        if arg.is_empty() {
            return match self.jb.ban_current().await {
                Some(t) => self.reply_msg(m, "ban", vec![("трек", t.label())]).await,
                None => self.reply(m, format!("зараз нічого не грає. Щоб заборонити конкретну пісню: {} назва", self.first("ban"))).await,
            };
        }
        if let Some((place, t)) = self.jb.find(arg) {
            if matches!(place, Place::Now(_)) {
                self.jb.ban_current().await;
            } else {
                self.jb.ban_song(&t);
            }
            return self.reply_msg(m, "ban", vec![("трек", t.label())]).await;
        }
        // у черзі й історії немає — шукаємо в YouTube Music
        let results = self.music.search(arg, true).await;
        let Some(t) = results.into_iter().next() else {
            return self.reply(m, format!("не знайшов «{arg}» 🤔 Спробуй: виконавець — назва")).await;
        };
        self.jb.ban_song(&t);
        self.reply_msg(m, "ban", vec![("трек", t.label())]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_parsing() {
        assert_eq!(parse_seek("1:30", 10.0), Some(90.0));
        assert_eq!(parse_seek("+30", 10.0), Some(40.0));
        assert_eq!(parse_seek("-1:00", 100.0), Some(40.0));
        assert_eq!(parse_seek("90", 0.0), Some(90.0));
        assert_eq!(parse_seek("1,5", 0.0), Some(1.5));
        assert_eq!(parse_seek("abc", 0.0), None);
        assert_eq!(parse_seek("", 0.0), None);
        assert_eq!(parse_seek("1:2:3:4", 0.0), None);
        assert_eq!(minutes(30), "менш ніж хвилину");
        assert_eq!(minutes(150), "~3 хв");
    }
}
