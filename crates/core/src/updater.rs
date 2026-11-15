//! Оновлення PipaBobik з GitHub прямо із застосунку.
//!
//! Як це працює:
//!   1. Раз на кілька годин бот питає GitHub, який останній випуск (Release) проєкту,
//!      і порівнює з установленою версією.
//!   2. Стрімерка натискає «Оновити» — бот завантажує архів нової версії в data/update/ і
//!      перевіряє його. Поки що нічого не змінено: якщо завантаження не вдалося, все працює далі.
//!   3. Бот запускає НОВИЙ PipaBobik.exe з data/update/new у режимі `--apply-update` і закривається.
//!      Той чекає, поки бот вийде, за потреби закриває OBS, робить резервну копію, міняє файли
//!      і знову відкриває OBS (скрипт OBS сам запустить бота) або запускає бота напряму.
//!
//! config.toml, data/, bin/ і cookies.txt оновлення не чіпає.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::paths;
use crate::track::now_secs;

pub const REPO: &str = "LoganBg23/PipaBobik-v2-Rust";
pub const ASSET: &str = "PipaBobik-windows-x64.zip";
pub const EXE: &str = if cfg!(windows) { "PipaBobik.exe" } else { "pipabobik" };
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const KEEP: &[&str] = &["config.toml", "cookies.txt"];
const KEEP_DIRS: &[&str] = &["data", "bin", ".git", "target"];

/// Версія цієї збірки: GitHub Actions вписує номер збірки, інакше — версія з Cargo.toml.
pub fn version() -> &'static str {
    option_env!("PIPABOBIK_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn parse_version(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches(['v', 'V']).split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

pub fn newer(a: &str, b: &str) -> bool {
    let (a, b) = (parse_version(a), parse_version(b));
    !a.is_empty() && a > b
}

fn first_line(s: &str) -> String {
    s.trim().lines().next().unwrap_or("").chars().take(140).collect()
}

fn stage_dir() -> PathBuf {
    paths::data_dir().join("update")
}
fn result_file() -> PathBuf {
    paths::data_dir().join("update_result.json")
}

#[derive(Default)]
struct S {
    state: String,
    latest: Value,
    notes: Vec<String>,
    error: String,
    checked_at: f64,
    just_updated: Value,
}

pub struct Updater {
    s: Mutex<S>,
    http: reqwest::Client,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Updater {
    pub fn new(http: reqwest::Client) -> Self {
        let me = Self { s: Mutex::new(S { state: "idle".into(), ..Default::default() }), http, on_change: Mutex::new(None) };
        me.read_result();
        me
    }

    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap() = Some(Arc::new(f));
    }
    fn notify(&self) {
        let f = self.on_change.lock().unwrap().clone();
        if let Some(f) = f {
            f();
        }
    }

    pub fn state(&self) -> String {
        self.s.lock().unwrap().state.clone()
    }
    pub fn error(&self) -> String {
        self.s.lock().unwrap().error.clone()
    }
    pub fn latest(&self) -> Value {
        self.s.lock().unwrap().latest.clone()
    }
    pub fn set(&self, state: &str, error: &str) {
        {
            let mut s = self.s.lock().unwrap();
            s.state = state.into();
            s.error = error.into();
        }
        self.notify();
    }
    pub fn dismiss(&self) {
        self.s.lock().unwrap().just_updated = Value::Null;
        self.notify();
    }

    pub fn available(&self) -> bool {
        let s = self.s.lock().unwrap();
        s.latest["tag"].as_str().map(|t| newer(t, version())).unwrap_or(false) && s.latest["url"].is_string()
    }

    pub fn summary(&self) -> Value {
        let available = self.available();
        let s = self.s.lock().unwrap();
        json!({
            "state": s.state,
            "available": available,
            "current": version(),
            "current_known": true,
            "latest": s.latest["tag"].as_str().unwrap_or("").trim_start_matches('v'),
            "message": s.latest["message"],
            "date": s.latest["date"],
            "notes": s.notes.iter().take(12).collect::<Vec<_>>(),
            "error": s.error,
            "checked_at": s.checked_at,
            "just_updated": s.just_updated,
        })
    }

    /// Результат попереднього оновлення (його пише `--apply-update`).
    fn read_result(&self) {
        let Ok(text) = std::fs::read_to_string(result_file()) else { return };
        let _ = std::fs::remove_file(result_file());
        let Ok(res) = serde_json::from_str::<Value>(&text) else { return };
        if res["ok"].as_bool().unwrap_or(false) {
            let tag = res["tag"].as_str().unwrap_or("").trim_start_matches('v').to_string();
            self.s.lock().unwrap().just_updated = json!({"message": res["message"], "sha": tag});
            tracing::info!("PipaBobik оновлено до {tag}: {}", res["message"].as_str().unwrap_or(""));
            for w in res["warnings"].as_array().into_iter().flatten() {
                tracing::warn!("Оновлення: {}", w.as_str().unwrap_or(""));
            }
        } else {
            tracing::error!("Оновлення не вдалося: {}", res["error"].as_str().unwrap_or("невідома помилка"));
        }
    }

    async fn github(&self, path: &str) -> Result<Value, String> {
        let r = self
            .http
            .get(format!("https://api.github.com/repos/{REPO}{path}"))
            .header("User-Agent", "pipabobik-updater")
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("GitHub відповів {}", r.status().as_u16()));
        }
        r.json().await.map_err(|e| e.to_string())
    }

    pub async fn check(&self) -> bool {
        let st = self.state();
        if st == "downloading" || st == "installing" {
            return self.available();
        }
        self.set("checking", "");
        let res: Result<(), String> = async {
            let releases = self.github("/releases?per_page=30").await?;
            let list: Vec<&Value> = releases.as_array().map(|a| a.iter().filter(|r| !r["draft"].as_bool().unwrap_or(false) && !r["prerelease"].as_bool().unwrap_or(false)).collect()).unwrap_or_default();
            let Some(latest) = list.iter().max_by(|a, b| parse_version(a["tag_name"].as_str().unwrap_or("")).cmp(&parse_version(b["tag_name"].as_str().unwrap_or("")))) else {
                return Err("на GitHub ще немає жодного випуску".into());
            };
            let asset = latest["assets"].as_array().and_then(|a| a.iter().find(|x| x["name"] == ASSET));
            let message = first_line(latest["name"].as_str().filter(|s| !s.is_empty()).or(latest["body"].as_str()).unwrap_or(""));
            // що нового — заголовки всіх випусків між установленою та новою версією
            let mut newer_list: Vec<&&Value> = list.iter().filter(|r| newer(r["tag_name"].as_str().unwrap_or(""), version())).collect();
            newer_list.sort_by_key(|r| std::cmp::Reverse(parse_version(r["tag_name"].as_str().unwrap_or(""))));
            let mut notes = Vec::new();
            for r in newer_list {
                let body = r["body"].as_str().unwrap_or("");
                let lines: Vec<String> = body.lines().filter_map(|l| l.trim().strip_prefix("- ").map(|x| first_line(x))).collect();
                if lines.is_empty() {
                    notes.push(first_line(r["name"].as_str().unwrap_or("")));
                } else {
                    notes.extend(lines);
                }
            }
            let mut s = self.s.lock().unwrap();
            s.latest = json!({
                "tag": latest["tag_name"],
                "message": message,
                "date": latest["published_at"],
                "url": asset.map(|a| a["browser_download_url"].clone()).unwrap_or(Value::Null),
            });
            s.notes = notes;
            s.checked_at = now_secs();
            Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                if self.available() {
                    tracing::info!("Доступне оновлення PipaBobik: {}", self.latest()["message"].as_str().unwrap_or(""));
                }
                self.set("idle", "");
            }
            Err(e) => {
                tracing::warn!("Не вдалося перевірити оновлення: {e}");
                self.set("idle", &format!("Не вдалося перевірити оновлення: {e}"));
            }
        }
        self.available()
    }

    pub async fn run_loop(self: Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            self.check().await;
            tokio::time::sleep(CHECK_EVERY).await;
        }
    }

    /// Завантажує й розпаковує нову версію в data/update/new. Повертає цю папку.
    pub async fn stage(&self) -> Result<PathBuf, String> {
        if self.latest()["url"].is_null() {
            self.check().await;
        }
        let url = self.latest()["url"].as_str().map(String::from).ok_or_else(|| {
            let e = self.error();
            if e.is_empty() { "Не вдалося дізнатися нову версію".to_string() } else { e }
        })?;
        self.set("downloading", "");
        let dir = stage_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let archive = dir.join(ASSET);
        crate::music::download(&self.http, &url, &archive).await?;
        let target = dir.join("new");
        tokio::task::spawn_blocking(move || extract(&archive, &target)).await.map_err(|e| e.to_string())?
    }
}

/// Розпаковує архів випуску. Файли можуть лежати в корені архіву або в одній папці.
pub fn extract(archive: &Path, target: &Path) -> Result<PathBuf, String> {
    let f = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("Архів пошкоджено ({e})"))?;
    let names: Vec<String> = (0..z.len()).filter_map(|i| z.by_index(i).ok().map(|f| f.name().to_string())).collect();
    let top = names.first().and_then(|n| n.split_once('/').map(|(a, _)| format!("{a}/"))).filter(|t| names.iter().all(|n| n.starts_with(t.as_str())));
    for i in 0..z.len() {
        let mut file = z.by_index(i).map_err(|e| e.to_string())?;
        if file.is_dir() {
            continue;
        }
        let Some(rel) = file.enclosed_name() else { continue };
        let rel = match &top {
            Some(t) => match rel.strip_prefix(t.trim_end_matches('/')) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            },
            None => rel,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = target.join(&rel);
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| format!("Архів пошкоджено ({e})"))?;
        std::fs::write(&dest, buf).map_err(|e| e.to_string())?;
    }
    if !target.join(EXE).is_file() {
        return Err("В архіві немає файлів бота".into());
    }
    Ok(target.to_path_buf())
}

// ── застосування оновлення (режим `PipaBobik.exe --apply-update`) ──

pub struct ApplyArgs {
    pub staged: PathBuf,
    pub target: PathBuf,
    pub tag: String,
    pub message: String,
    pub bot_pid: u32,
    pub obs_pid: u32,
    pub obs_exe: String,
    pub bot_args: Vec<String>,
}

fn say(target: &Path, msg: &str) {
    let data = target.join("data");
    let _ = std::fs::create_dir_all(&data);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data.join("update.log")) {
        let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
    let _ = std::fs::write(data.join("restarting"), format!("{}", now_secs()));
}

/// Чекає, поки процес завершиться. true — завершився.
pub fn wait_pid(pid: u32, timeout: Duration, target: &Path) -> bool {
    if pid == 0 {
        return true;
    }
    let end = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < end {
        if !crate::system::pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::fs::write(target.join("data").join("restarting"), format!("{}", now_secs()));
    }
    false
}

/// Закриває OBS так само, як кнопка «×»: OBS зберігає сцени й налаштування.
fn close_obs(pid: u32, target: &Path) -> bool {
    say(target, &format!("Закриваю OBS (pid {pid})"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string()]).creation_flags(crate::proc::CREATE_NO_WINDOW).output();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).output();
    }
    let ok = wait_pid(pid, Duration::from_secs(45), target);
    say(target, if ok { "OBS закрито" } else { "OBS не закрився за 45 с — не чіпаю його" });
    ok
}

pub fn code_files(base: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(rel) = p.strip_prefix(base) else { continue };
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let first = rel_s.split('/').next().unwrap_or("");
            if KEEP_DIRS.contains(&first) || KEEP.contains(&rel_s.as_str()) || rel_s.ends_with(".old") || rel_s.starts_with(".pipabobik") {
                continue;
            }
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(rel_s);
            }
        }
    }
    let mut out = Vec::new();
    walk(base, base, &mut out);
    out.sort();
    out
}

fn backup(target: &Path, label: &str) -> Result<String, String> {
    let folder = target.join("data").join("backups");
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let name = format!("pipabobik-{}-{label}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let f = std::fs::File::create(folder.join(&name)).map_err(|e| e.to_string())?;
    let mut z = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for rel in code_files(target) {
        let data = std::fs::read(target.join(&rel)).map_err(|e| e.to_string())?;
        z.start_file(rel.as_str(), opts).map_err(|e| e.to_string())?;
        z.write_all(&data).map_err(|e| e.to_string())?;
    }
    z.finish().map_err(|e| e.to_string())?;
    let mut old: Vec<PathBuf> = std::fs::read_dir(&folder).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "zip")).collect()).unwrap_or_default();
    old.sort();
    let n = old.len();
    for p in old.into_iter().take(n.saturating_sub(3)) {
        let _ = std::fs::remove_file(p); // лишаємо 3 останні
    }
    Ok(name)
}

fn install_files(new: &Path, target: &Path) -> Result<Vec<String>, String> {
    let files = code_files(new);
    for rel in &files {
        let src = new.join(rel);
        let dst = target.join(rel);
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        // запущений .exe (вікно застосунку) не можна перезаписати, але можна перейменувати
        if dst.exists() && rel.ends_with(".exe") {
            let old = dst.with_extension("exe.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&dst, &old);
        }
        let tmp = dst.with_extension("pbnew");
        std::fs::copy(&src, &tmp).map_err(|e| format!("{rel}: {e}"))?;
        std::fs::rename(&tmp, &dst).map_err(|e| format!("{rel}: {e}"))?;
    }
    // прибираємо лише файли, які поставило попереднє оновлення і яких більше немає
    let manifest = target.join("data").join("update-manifest.json");
    let before: Vec<String> = std::fs::read_to_string(&manifest).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    for rel in before.iter().filter(|r| !files.contains(r)) {
        let _ = std::fs::remove_file(target.join(rel));
    }
    let _ = std::fs::write(&manifest, serde_json::to_string(&files).unwrap_or_default());
    Ok(files)
}

pub fn apply_update(a: ApplyArgs) -> i32 {
    let t = &a.target;
    let mut result = json!({"ok": false, "tag": a.tag, "message": a.message, "warnings": [], "at": now_secs()});
    let mut obs_closed = false;
    let res: Result<(), String> = (|| {
        say(t, &format!("Оновлення до {}: {}", a.tag, a.message));
        if !wait_pid(a.bot_pid, Duration::from_secs(30), t) {
            return Err("бот не закрився за 30 секунд".into());
        }
        if a.obs_pid != 0 && !a.obs_exe.is_empty() {
            obs_closed = close_obs(a.obs_pid, t);
            if !obs_closed {
                result["warnings"].as_array_mut().unwrap().push(json!("OBS не закрився сам — нова версія скрипта OBS запрацює після його перезапуску"));
            }
        }
        let name = backup(t, version())?;
        say(t, &format!("Резервна копія: {name}"));
        let files = install_files(&a.staged, t)?;
        say(t, &format!("Оновлено файлів: {}", files.len()));
        result["ok"] = json!(true);
        say(t, "Файли оновлено");
        Ok(())
    })();
    if let Err(e) = res {
        result["error"] = json!(e);
        say(t, &format!("ПОМИЛКА: {e}"));
    }
    let _ = std::fs::write(t.join("data").join("update_result.json"), result.to_string());

    // запуск: OBS, якщо ми його закривали (скрипт OBS запустить бота), інакше — сам бот
    let launch = if obs_closed {
        let exe = PathBuf::from(&a.obs_exe);
        say(t, &format!("Відкриваю OBS: {}", exe.display()));
        let cwd = exe.parent().map(Path::to_path_buf).unwrap_or_else(|| t.clone()); // OBS має стартувати зі своєї папки
        crate::proc::spawn_detached(&exe, &[], &cwd)
    } else {
        let mut args = vec!["--bot".to_string()];
        args.extend(a.bot_args.iter().cloned());
        args.push("--restart".into());
        say(t, &format!("Запускаю бота: {}", args.join(" ")));
        crate::proc::spawn_detached(&t.join(EXE), &args, t)
    };
    if let Err(e) = launch {
        say(t, &format!("Не вдалося перезапустити: {e}"));
    }
    if result["ok"].as_bool().unwrap_or(false) {
        // папку з архівом прибирає вже новий бот (цей процес запущено з неї)
        let _ = std::fs::write(t.join("data").join("update").join("done"), "");
        0
    } else {
        1
    }
}

/// Після оновлення: прибирає старий .exe і папку з архівом.
pub fn cleanup_after_update() {
    let root = paths::root();
    let _ = std::fs::remove_file(root.join(format!("{EXE}.old")));
    let _ = std::fs::remove_file(root.join("PipaBobik.exe.old"));
    if stage_dir().join("done").exists() {
        let _ = std::fs::remove_dir_all(stage_dir());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("v2.0.12", "2.0.9"));
        assert!(!newer("v2.0.9", "2.0.9"));
        assert!(!newer("", "2.0.0"));
        assert_eq!(parse_version("v2.1.3"), vec![2, 1, 3]);
    }

    #[test]
    fn stage_and_install() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut z = zip::ZipWriter::new(f);
            let o = zip::write::SimpleFileOptions::default();
            for (n, d) in [(format!("PipaBobik/{EXE}"), "new-exe"), ("PipaBobik/obs/pipabobik.lua".into(), "lua"), ("PipaBobik/config.toml".into(), "НЕ МОЖНА")] {
                z.start_file(n.as_str(), o).unwrap();
                z.write_all(d.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        let new = extract(&archive, &dir.path().join("new")).unwrap();
        let target = dir.path().join("inst");
        std::fs::create_dir_all(target.join("data")).unwrap();
        std::fs::write(target.join("config.toml"), "мій").unwrap();
        std::fs::write(target.join(EXE), "old-exe").unwrap();
        std::fs::write(target.join("gone.txt"), "x").unwrap();
        std::fs::write(target.join("data").join("update-manifest.json"), r#"["gone.txt"]"#).unwrap();
        let files = install_files(&new, &target).unwrap();
        assert!(files.contains(&"obs/pipabobik.lua".to_string()));
        assert_eq!(std::fs::read_to_string(target.join(EXE)).unwrap(), "new-exe");
        assert_eq!(std::fs::read_to_string(target.join("config.toml")).unwrap(), "мій");
        assert!(!target.join("gone.txt").exists());
        assert!(backup(&target, "t").unwrap().ends_with("-t.zip"));
    }
}
