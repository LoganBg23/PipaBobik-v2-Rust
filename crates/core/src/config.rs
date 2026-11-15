//! config.toml: типові значення, міграції зі старих версій і збереження змін без втрати коментарів.
//!
//! Формат той самий, що в pipabobik v1, тож config.toml можна просто скопіювати.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::access::{default_limits, default_permissions, normalize_limits, normalize_roles};

pub fn defaults() -> Value {
    json!({
        "twitch": {"channel": "", "client_id": "", "client_secret": "", "admins": [], "helpers": []},
        "google": {"cookies_from_browser": "firefox", "cookies_file": "", "fallback_playlist": "LM", "fallback_shuffle": true},
        "player": {"mpv_path": "mpv", "volume": 60, "normalize": true, "audio_device": "", "show_window": true},
        "rules": {
            "max_duration": 420, "max_per_user": 2, "cooldown": 30, "max_queue": 30, "only_songs": true,
            "block_explicit": false, "banned_words": [], "interrupt_fallback": true,
            "block_russian": true, "russian_artists": []
        },
        "commands": {
            "request": ["!трек", "!sr", "!songrequest", "!request", "!req", "!track", "!замовити", "!ік", "!nhtr"],
            "now": ["!шоце", "!song"],
            "skip": ["!скіп", "!скип", "!skip", "!next"],
            "queue": ["!черга", "!queue"],
            "wrong": ["!нетой", "!wrongsong"],
            "volume": ["!гучність", "!vol"],
            "seek": ["!перемотка", "!мотай", "!seek"],
            "pause": ["!пауза", "!pause"],
            "play": ["!грай", "!play"],
            "ban": ["!бантрек"],
            "help": ["!музика", "!pipabobik"]
        },
        "permissions": Value::Object(default_permissions(None)),
        "limits": default_limits(),
        "chat": {"announce_tracks": false, "greet_on_stream_start": true},
        "server": {"port": 17777},
        "update": {"auto_check": true}
    })
}

/// Нік Twitch у нижньому регістрі, без @ і пробілів.
pub fn nick(s: &str) -> String {
    s.trim().trim_start_matches('@').trim().to_lowercase()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub raw: Value,
    pub path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self { raw: defaults(), path: PathBuf::from("config.toml") }
    }
}

impl Config {
    pub fn port(&self) -> u16 {
        self.raw["server"]["port"].as_u64().unwrap_or(17777) as u16
    }
    pub fn channel(&self) -> String {
        self.str("twitch", "channel").trim().trim_start_matches('#').to_lowercase()
    }
    pub fn str(&self, sec: &str, key: &str) -> String {
        match &self.raw[sec][key] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            v => v.to_string(),
        }
    }
    pub fn bool(&self, sec: &str, key: &str) -> bool {
        self.raw[sec][key].as_bool().unwrap_or(false)
    }
    pub fn int(&self, sec: &str, key: &str) -> i64 {
        self.raw[sec][key].as_i64().or_else(|| self.raw[sec][key].as_f64().map(|f| f as i64)).unwrap_or(0)
    }
    pub fn str_list(&self, sec: &str, key: &str) -> Vec<String> {
        self.raw[sec][key]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }
    pub fn commands(&self, key: &str) -> Vec<String> {
        self.str_list("commands", key)
    }
    pub fn set(&mut self, sec: &str, key: &str, v: Value) {
        if !self.raw[sec].is_object() {
            self.raw[sec] = json!({});
        }
        self.raw[sec][key] = v;
    }
}

fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(bv) if bv.is_object() && v.is_object() => merge(bv, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Старі типові набори команд: якщо в config.toml саме вони, замінюємо на нові.
fn migrate_commands(commands: &mut Value, defaults: &Value) -> Map<String, Value> {
    let mut changed = Map::new();
    let olds: &[(&str, &[&str])] = &[("request", &["!трек", "!sr"]), ("skip", &["!хуйня", "!skip"])];
    for (key, old) in olds {
        let cur: Vec<String> = commands[*key].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let new: Vec<String> = defaults["commands"][*key].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        if cur.iter().map(String::as_str).collect::<Vec<_>>() == *old {
            if cur != new {
                commands[*key] = json!(new);
                changed.insert((*key).into(), json!(new));
            }
            continue;
        }
        if *key == "skip" && cur.iter().any(|c| c == "!хуйня") {
            let mut v = vec![new[0].clone()];
            v.extend(cur.into_iter().filter(|c| c != "!хуйня" && *c != new[0]));
            commands[*key] = json!(v);
            changed.insert((*key).into(), json!(v));
        }
    }
    changed
}

fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect()),
    }
}

pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    let mut raw = defaults();
    let mut data = json!({});
    if path.exists() {
        let text = std::fs::read_to_string(path)?;
        let parsed: toml::Value = toml::from_str(&text).map_err(|e| anyhow::anyhow!("config.toml: {e}"))?;
        data = toml_to_json(&parsed);
        merge(&mut raw, &data);
    }
    let defs = defaults();
    let changed = migrate_commands(&mut raw["commands"], &defs);
    if !changed.is_empty() && path.exists() {
        let _ = set_values(path, &[("commands", Value::Object(changed))]);
    }
    // хто що може: [permissions] і [limits.<роль>]; без них — типові (з урахуванням старого who_can_request)
    let mut perms = default_permissions(Some(&raw["rules"]));
    if let Some(p) = data.get("permissions").and_then(|p| p.as_object()) {
        for (k, v) in p {
            if perms.contains_key(k) {
                perms.insert(k.clone(), json!(normalize_roles(v)));
            }
        }
    }
    raw["permissions"] = Value::Object(perms);
    raw["limits"] = normalize_limits(data.get("limits"));
    for k in ["admins", "helpers"] {
        let list: Vec<String> = match &raw["twitch"][k] {
            Value::String(s) => s.split(',').map(nick).collect(),
            Value::Array(a) => a.iter().filter_map(|v| v.as_str()).map(nick).collect(),
            _ => vec![],
        };
        let mut uniq: Vec<String> = Vec::new();
        for n in list {
            if !n.is_empty() && !uniq.contains(&n) {
                uniq.push(n);
            }
        }
        raw["twitch"][k] = json!(uniq);
    }
    Ok(Config { raw, path: path.to_path_buf() })
}

fn to_item(v: &Value) -> toml_edit::Item {
    toml_edit::value(to_value(v))
}

fn to_value(v: &Value) -> toml_edit::Value {
    match v {
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else {
                n.as_f64().unwrap_or(0.0).into()
            }
        }
        Value::String(s) => s.as_str().into(),
        Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(to_value(x));
            }
            toml_edit::Value::Array(arr)
        }
        Value::Null => "".into(),
        Value::Object(_) => v.to_string().into(),
    }
}

/// Записує значення в config.toml, зберігаючи коментарі й порядок.
/// `sections` — пари (секція, {ключ: значення}); секція може бути вкладеною: "limits.subs".
pub fn set_values(path: &Path, sections: &[(&str, Value)]) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    for (sec, vals) in sections {
        let mut table = doc.as_table_mut();
        for part in sec.split('.') {
            if !table.contains_key(part) {
                let mut t = toml_edit::Table::new();
                t.set_implicit(sec.contains('.') && !sec.ends_with(part));
                table.insert(part, toml_edit::Item::Table(t));
            }
            table = table[part].as_table_mut().ok_or_else(|| anyhow::anyhow!("{sec} — не таблиця"))?;
        }
        if let Some(obj) = vals.as_object() {
            for (k, v) in obj {
                table[k.as_str()] = to_item(v);
            }
        }
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_v1_config_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "# мій конфіг\n[twitch]\nchannel = \"Pipa\"\nadmins = [\"@Boss\"]\n\n[rules]\nwho_can_request = \"subs\"\n\n[commands]\n# команди\nskip = [\"!хуйня\", \"!skip\"]\n").unwrap();
        let c = load_config(&p).unwrap();
        assert_eq!(c.channel(), "pipa");
        assert_eq!(c.str_list("twitch", "admins"), vec!["boss"]);
        assert_eq!(c.commands("skip")[0], "!скіп");
        assert_eq!(c.raw["permissions"]["request"], json!(["subs", "vip", "mods"]));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# мій конфіг") && text.contains("# команди") && !text.contains("хуйня"));
        set_values(&p, &[("limits.subs", json!({"custom": true, "max_per_user": 5})), ("rules", json!({"cooldown": 60}))]).unwrap();
        let c = load_config(&p).unwrap();
        assert_eq!(c.raw["limits"]["subs"]["max_per_user"], json!(5));
        assert_eq!(c.int("rules", "cooldown"), 60);
        assert!(std::fs::read_to_string(&p).unwrap().contains("# мій конфіг"));
    }
}
