//! Журнал помилок для вкладки «Помилки» в застосунку.
//!
//! Збирає попередження й помилки з усіх частин бота: що сталося, коли, у якій частині
//! й технічні подробиці. Однакові повідомлення підряд не дублюються — зростає лічильник.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::track::now_secs;

/// частина бота (модуль) → як її назвати людині
const SOURCES: &[(&str, &str)] = &[
    ("twitch", "Чат Twitch"),
    ("commands", "Команди в чаті"),
    ("music", "YouTube Music"),
    ("rufilter", "Фільтр"),
    ("player", "Програвач"),
    ("jukebox", "Черга й відтворення"),
    ("server", "Застосунок"),
    ("playlists", "Фонові плейлисти"),
    ("winproc", "Система"),
    ("updater", "Оновлення"),
];

pub fn source_name(target: &str) -> &'static str {
    let module = target.rsplit("::").next().unwrap_or(target);
    SOURCES.iter().find(|(k, _)| *k == module).map(|(_, v)| *v).unwrap_or("Бот")
}

#[derive(Debug, Clone)]
struct Item {
    id: u64,
    at: f64,
    last: f64,
    count: u64,
    level: &'static str,
    source: &'static str,
    message: String,
    details: String,
}

impl Item {
    fn json(&self) -> Value {
        json!({"id": self.id, "at": self.at, "last": self.last, "count": self.count, "level": self.level,
               "source": self.source, "message": self.message, "details": self.details})
    }
}

#[derive(Default)]
struct Inner {
    items: VecDeque<Item>,
    unseen: u64,
    next_id: u64,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

pub struct ErrorLog {
    inner: Mutex<Inner>,
    limit: usize,
    on_change: Mutex<Option<Hook>>,
}

pub static ERRORS: Lazy<ErrorLog> = Lazy::new(|| ErrorLog::new(200));

impl ErrorLog {
    pub fn new(limit: usize) -> Self {
        Self { inner: Mutex::new(Inner::default()), limit, on_change: Mutex::new(None) }
    }

    /// Будить застосунок, коли з'являється нова помилка.
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap() = Some(Arc::new(f));
    }

    fn notify(&self) {
        let hook = self.on_change.lock().unwrap().clone();
        if let Some(h) = hook {
            h();
        }
    }

    pub fn push(&self, error: bool, source: &'static str, message: String, details: String) {
        let now = now_secs();
        {
            let mut g = self.inner.lock().unwrap();
            let merged = match g.items.back_mut() {
                Some(last) if last.message == message && last.source == source && now - last.last < 600.0 => {
                    last.count += 1;
                    last.last = now;
                    true
                }
                _ => false,
            };
            if !merged {
                g.next_id += 1;
                let id = g.next_id;
                g.items.push_back(Item {
                    id,
                    at: now,
                    last: now,
                    count: 1,
                    level: if error { "error" } else { "warning" },
                    source,
                    message,
                    details: details.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect(),
                });
                while g.items.len() > self.limit {
                    g.items.pop_front();
                }
            }
            g.unseen += 1;
        }
        self.notify();
    }

    pub fn summary(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let last = g.items.back().map(|l| json!({"message": l.message, "level": l.level}));
        json!({"unseen": g.unseen, "total": g.items.len(), "last": last})
    }

    pub fn list(&self) -> Vec<Value> {
        let g = self.inner.lock().unwrap();
        g.items.iter().rev().map(Item::json).collect()
    }

    pub fn seen(&self) {
        self.inner.lock().unwrap().unseen = 0;
        self.notify();
    }

    pub fn clear(&self) {
        {
            let mut g = self.inner.lock().unwrap();
            g.items.clear();
            g.unseen = 0;
        }
        self.notify();
    }
}

#[derive(Default)]
struct MsgVisitor {
    message: String,
    details: String,
}

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else if field.name() == "details" {
            self.details = format!("{value:?}");
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if field.name() == "details" {
            self.details = value.to_string();
        }
    }
}

/// Шар tracing: попередження й помилки з модулів pipabobik потрапляють у журнал.
pub struct ErrorLayer;

impl<S: Subscriber> Layer<S> for ErrorLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = *meta.level();
        if level > Level::WARN || !meta.target().starts_with("pipabobik") {
            return;
        }
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        ERRORS.push(level == Level::ERROR, source_name(meta.target()), v.message, v.details);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_repeats() {
        let log = ErrorLog::new(3);
        log.push(false, "Чат Twitch", "a".into(), String::new());
        log.push(false, "Чат Twitch", "a".into(), String::new());
        log.push(true, "Програвач", "b".into(), String::new());
        let items = log.list();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["count"], 2);
        assert_eq!(log.summary()["unseen"], 3);
        log.seen();
        assert_eq!(log.summary()["unseen"], 0);
        for i in 0..5 {
            log.push(false, "Бот", format!("x{i}"), String::new());
        }
        assert_eq!(log.list().len(), 3);
        assert_eq!(source_name("pipabobik_core::jukebox"), "Черга й відтворення");
    }
}
