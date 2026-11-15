//! Хто що може: ролі глядачів, дозволи на команди, ліміти замовлень.
//!
//! Ролі накопичуються: саб-VIP має і роль «Саби», і роль «VIP», а дозволи й ліміти
//! беруться найщедріші з усіх його ролей. Стрімерка може все завжди.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::twitch::ChatMessage;

pub const ROLES: &[(&str, &str)] = &[
    ("all", "Усі"),
    ("subs", "Саби"),
    ("vip", "VIP"),
    ("helpers", "Помічники"),
    ("mods", "Модератори"),
];

/// key → (назва в застосунку, що робить, хто може за замовчуванням)
pub const COMMANDS: &[(&str, &str, &str, &[&str])] = &[
    ("request", "Замовити трек", "назва пісні або посилання YouTube", &["all"]),
    ("now", "Що грає", "трек, хто замовив і посилання", &["all"]),
    ("queue", "Черга", "наступні треки", &["all"]),
    ("wrong", "Прибрати свій трек", "останнє власне замовлення", &["all"]),
    ("help", "Підказка", "список доступних команд", &["all"]),
    ("skip", "Пропустити", "увімкнути наступний трек", &["helpers", "mods"]),
    ("volume", "Гучність", "число від 0 до 100", &["helpers", "mods"]),
    ("seek", "Перемотати", "1:30 — на позицію, +30 або -15 — на секунди", &["mods"]),
    ("pause", "Пауза", "поставити на паузу або продовжити", &["mods"]),
    ("play", "Продовжити", "зняти з паузи (музика сама вмикається з трансляцією)", &["mods"]),
    ("ban", "Заборонити трек", "назавжди, і одразу пропустити", &["mods"]),
    ("ban_artist", "Заборонити виконавця", "усі його треки; без імені — того, хто грає", &["mods"]),
];

pub const LIMIT_KEYS: &[&str] = &["max_per_user", "cooldown", "max_duration", "bypass_queue"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Limits {
    pub max_per_user: u64,
    pub cooldown: u64,
    pub max_duration: u64,
    pub bypass_queue: bool,
}

pub const UNLIMITED: Limits = Limits { max_per_user: 0, cooldown: 0, max_duration: 0, bypass_queue: true };

pub fn command_label(key: &str) -> &'static str {
    COMMANDS.iter().find(|c| c.0 == key).map(|c| c.1).unwrap_or("Команда")
}

pub fn default_limits() -> Value {
    let base = json!({"custom": false, "max_per_user": 3, "cooldown": 15, "max_duration": 600, "bypass_queue": false});
    json!({
        "subs": base.clone(),
        "vip": base.clone(),
        "helpers": base,
        "mods": {"custom": true, "max_per_user": 0, "cooldown": 0, "max_duration": 0, "bypass_queue": true},
    })
}

/// «!Трек» → «!трек»; «трек» → «!трек».
pub fn normalize_name(name: &str) -> String {
    let n = name.trim().to_lowercase();
    if !n.is_empty() && !n.starts_with('!') {
        format!("!{n}")
    } else {
        n
    }
}

pub fn valid_name(name: &str) -> bool {
    let rest: Vec<char> = name.chars().skip(1).collect();
    name.starts_with('!')
        && (1..=24).contains(&rest.len())
        && !rest.iter().any(|c| c.is_whitespace() || matches!(c, '!' | '@' | '#'))
}

pub fn normalize_roles(v: &Value) -> Vec<String> {
    let items: Vec<String> = match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    };
    let mut out: Vec<String> = Vec::new();
    for r in items {
        let r = r.trim().to_lowercase();
        if ROLES.iter().any(|(k, _)| *k == r) && !out.contains(&r) {
            out.push(r);
        }
    }
    if out.iter().any(|r| r == "all") {
        vec!["all".into()]
    } else {
        out
    }
}

/// Типові дозволи; старий `who_can_request` переноситься в «хто замовляє».
pub fn default_permissions(rules: Option<&Value>) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, _, _, roles) in COMMANDS {
        m.insert((*k).into(), json!(roles));
    }
    if let Some(who) = rules.and_then(|r| r.get("who_can_request")).and_then(|w| w.as_str()) {
        let roles: Option<&[&str]> = match who {
            "all" => Some(&["all"]),
            "subs" => Some(&["subs", "vip", "mods"]),
            "vip" => Some(&["vip", "mods"]),
            "mods" => Some(&["mods"]),
            _ => None,
        };
        if let Some(r) = roles {
            m.insert("request".into(), json!(r));
        }
    }
    m
}

fn num(v: Option<&Value>, default: u64) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f.max(0.0) as u64).unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse::<f64>().map(|f| f.max(0.0) as u64).unwrap_or(0),
        Some(Value::Bool(b)) => *b as u64,
        None => default,
        _ => 0,
    }
}

pub fn normalize_limits(raw: Option<&Value>) -> Value {
    let defaults = default_limits();
    let mut out = Map::new();
    for (role, base) in defaults.as_object().unwrap() {
        let src = raw.and_then(|r| r.get(role));
        let get = |k: &str| src.and_then(|s| s.get(k)).or_else(|| base.get(k));
        out.insert(
            role.clone(),
            json!({
                "custom": get("custom").and_then(|v| v.as_bool()).unwrap_or(false),
                "max_per_user": num(get("max_per_user"), 0),
                "cooldown": num(get("cooldown"), 0),
                "max_duration": num(get("max_duration"), 0),
                "bypass_queue": get("bypass_queue").and_then(|v| v.as_bool()).unwrap_or(false),
            }),
        );
    }
    Value::Object(out)
}

pub fn limits_for_all(rules: &Value) -> Limits {
    Limits {
        max_per_user: num(rules.get("max_per_user"), 0),
        cooldown: num(rules.get("cooldown"), 0),
        max_duration: num(rules.get("max_duration"), 0),
        bypass_queue: false,
    }
}

/// Перевірки доступу. Читає конфіг щоразу, тож зміни з застосунку діють одразу.
pub struct Access<'a> {
    pub cfg: &'a Config,
}

impl<'a> Access<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Self { cfg }
    }

    pub fn roles_of(&self, m: &ChatMessage) -> BTreeSet<&'static str> {
        let mut roles = BTreeSet::from(["all"]);
        if m.is_sub() {
            roles.insert("subs");
        }
        if m.is_vip() {
            roles.insert("vip");
        }
        let login = m.login.to_lowercase();
        if self.cfg.str_list("twitch", "helpers").contains(&login) {
            roles.insert("helpers");
        }
        if m.is_mod() || self.cfg.str_list("twitch", "admins").contains(&login) {
            roles.insert("mods");
        }
        roles
    }

    pub fn roles_for(&self, key: &str) -> Vec<String> {
        if let Some(v) = self.cfg.raw.get("permissions").and_then(|p| p.get(key)) {
            return normalize_roles(v);
        }
        COMMANDS
            .iter()
            .find(|c| c.0 == key)
            .map(|c| c.3.iter().map(|s| s.to_string()).collect())
            .unwrap_or_else(|| vec!["mods".into()])
    }

    pub fn can(&self, m: &ChatMessage, key: &str) -> bool {
        if m.is_broadcaster() {
            return true;
        }
        let allowed = self.roles_for(key);
        if allowed.iter().any(|r| r == "all") {
            return true;
        }
        let mine = self.roles_of(m);
        allowed.iter().any(|r| mine.contains(r.as_str()))
    }

    pub fn role_limits(&self, role: &str) -> Limits {
        let base = limits_for_all(&self.cfg.raw["rules"]);
        if role == "all" {
            return base;
        }
        let lim = self.cfg.raw.get("limits").and_then(|l| l.get(role)).cloned().unwrap_or_else(|| default_limits()[role].clone());
        if !lim.get("custom").and_then(|v| v.as_bool()).unwrap_or(false) {
            return base;
        }
        Limits {
            max_per_user: num(lim.get("max_per_user"), base.max_per_user),
            cooldown: num(lim.get("cooldown"), base.cooldown),
            max_duration: num(lim.get("max_duration"), base.max_duration),
            bypass_queue: lim.get("bypass_queue").and_then(|v| v.as_bool()).unwrap_or(false),
        }
    }

    /// Найщедріші ліміти серед усіх ролей глядача. 0 означає «без обмежень».
    pub fn limits(&self, m: &ChatMessage) -> Limits {
        if m.is_broadcaster() {
            return UNLIMITED;
        }
        let mut best: Option<Limits> = None;
        for role in self.roles_of(m) {
            let lim = self.role_limits(role);
            best = Some(match best {
                None => lim,
                Some(mut b) => {
                    if b.max_per_user != 0 && (lim.max_per_user == 0 || lim.max_per_user > b.max_per_user) {
                        b.max_per_user = lim.max_per_user;
                    }
                    if b.max_duration != 0 && (lim.max_duration == 0 || lim.max_duration > b.max_duration) {
                        b.max_duration = lim.max_duration;
                    }
                    b.cooldown = b.cooldown.min(lim.cooldown);
                    b.bypass_queue = b.bypass_queue || lim.bypass_queue;
                    b
                }
            });
        }
        best.unwrap_or_else(|| limits_for_all(&self.cfg.raw["rules"]))
    }
}
