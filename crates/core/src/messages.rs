//! Тексти, які бот пише в чат: стримерка може змінити кожен у застосунку
//! (⚙ → «Повідомлення в чат»). Зберігаються в config.toml, розділ [messages].
//! Порожній текст — повертається стандартний.

use crate::config::Config;

pub struct Msg {
    pub key: &'static str,
    /// Коли бот це пише.
    pub label: &'static str,
    pub default: &'static str,
    /// Які підстановки можна вживати (через кому, без дужок).
    pub vars: &'static str,
}

pub const MESSAGES: &[Msg] = &[
    Msg { key: "added", label: "Трек додано в чергу", default: "🎶 додано {трек} ({тривалість}) — №{місце} у черзі, {коли}", vars: "трек, тривалість, місце, коли, нік" },
    Msg { key: "now", label: "Відповідь на «що грає»", default: "🎧 зараз грає {трек} [{позиція} / {тривалість}] · {хто} → {посилання}", vars: "трек, позиція, тривалість, хто, посилання, нік" },
    Msg { key: "now_empty", label: "«Що грає», коли тиша", default: "зараз тиша 🤫 Замов щось: {замовити} назва", vars: "замовити, нік" },
    Msg { key: "queue", label: "Відповідь на «черга»", default: "далі: {список}", vars: "список, кількість, нік" },
    Msg { key: "queue_empty", label: "«Черга», коли вона порожня", default: "черга порожня — замов: {замовити} назва 🎵", vars: "замовити, нік" },
    Msg { key: "wrong", label: "Глядач прибрав свій трек", default: "прибрав з черги {трек} 👌", vars: "трек, нік" },
    Msg { key: "skip", label: "Трек пропущено", default: "⏭ скіпнуто {трек}", vars: "трек, нік" },
    Msg { key: "volume", label: "Гучність", default: "🔊 гучність: {гучність}", vars: "гучність, нік" },
    Msg { key: "seek", label: "Перемотано", default: "⏩ {трек}: {позиція} / {тривалість}", vars: "трек, позиція, тривалість, нік" },
    Msg { key: "ban", label: "Трек заборонено", default: "🚫 {трек} більше не гратиме на каналі", vars: "трек, нік" },
    Msg { key: "ban_artist", label: "Виконавця заборонено", default: "🚫 {виконавець} більше не гратиме на каналі{прибрано}", vars: "виконавець, прибрано, нік" },
    Msg { key: "not_found", label: "Нічого не знайдено", default: "нічого не знайшов за «{запит}» 🤷 Спробуй «виконавець — назва»", vars: "запит, нік" },
    Msg { key: "failed", label: "Замовлений трек не відтворився", default: "@{нік} {трек} не відтворився — пропускаю 😔", vars: "нік, трек" },
    Msg { key: "announce", label: "Про кожен трек (якщо ввімкнено)", default: "🎶 Грає {трек} · {хто}", vars: "трек, хто, тривалість, посилання" },
    Msg { key: "greet", label: "Вітання на початку трансляції", default: "🎀 pipabobik на місці! Замовляй музику: {замовити} назва · що грає: {щограє}", vars: "замовити, щограє" },
];

pub const MAX_LEN: usize = 450;

pub fn find(key: &str) -> Option<&'static Msg> {
    MESSAGES.iter().find(|m| m.key == key)
}

/// Текст повідомлення з config.toml (або стандартний) з підстановками.
pub fn render(cfg: &Config, key: &str, vars: &[(&str, String)]) -> String {
    let custom = cfg.str("messages", key);
    let tpl = if custom.trim().is_empty() { find(key).map(|m| m.default.to_string()).unwrap_or_default() } else { custom };
    fill(&tpl, vars)
}

/// Підставляє {змінні} і прибирає те, що лишилось порожнім: «()», «[ / ]», подвійні пробіли.
pub fn fill(tpl: &str, vars: &[(&str, String)]) -> String {
    let mut out = tpl.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    for (from, to) in [(" / ]", "]"), ("[ / ", "["), (" ()", ""), ("()", ""), (" []", ""), ("[]", ""), (" · ·", " ·")] {
        out = out.replace(from, to);
    }
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    let out = out.trim().trim_end_matches(" ·").trim();
    out.chars().take(MAX_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_and_cleans_empty_parts() {
        let v = [("трек", "Go_A — Шум".to_string()), ("тривалість", String::new()), ("місце", "2".into()), ("коли", "грає наступним".into())];
        assert_eq!(fill(find("added").unwrap().default, &v), "🎶 додано Go_A — Шум — №2 у черзі, грає наступним");
        let v = [("трек", "X".to_string()), ("позиція", "0:10".into()), ("тривалість", String::new()), ("хто", "з плейлиста стримерки".into()), ("посилання", "u".into())];
        assert_eq!(fill(find("now").unwrap().default, &v), "🎧 зараз грає X [0:10] · з плейлиста стримерки → u");
    }

    #[test]
    fn custom_text_from_config() {
        let mut cfg = Config::default();
        assert_eq!(render(&cfg, "skip", &[("трек", "A".into())]), "⏭ скіпнуто A");
        cfg.set("messages", "skip", serde_json::json!("пропускаємо {трек}, @{нік}"));
        assert_eq!(render(&cfg, "skip", &[("трек", "A".into()), ("нік", "pipa".into())]), "пропускаємо A, @pipa");
        cfg.set("messages", "skip", serde_json::json!("  "));
        assert_eq!(render(&cfg, "skip", &[("трек", "A".into())]), "⏭ скіпнуто A");
    }
}
