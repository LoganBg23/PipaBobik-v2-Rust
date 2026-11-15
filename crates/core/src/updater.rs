//! Оновлення PipaBobik з GitHub прямо із застосунку.
//!
//! Як це працює:
//!   1. Раз на кілька годин бот питає GitHub, який останній випуск (Release) проєкту.
//!      Знайшов новіший — показує смугу «Нова версія» і одразу тихо завантажує архів
//!      з GitHub у data/update/, щоб оновлення потім не чекало на інтернет.
//!   2. Стрімерка натискає «Оновити». Бот запускає копію PipaBobik.exe з тимчасової папки
//!      у режимі `--update` і закривається. Копія відкриває вікно «Оновлення pipabobik»
//!      і по кроках показує, що робить: докачує архів з GitHub (якщо ще не завантажено),
//!      перевіряє його, за потреби закриває OBS, робить резервну копію, міняє файли
//!      і знову відкриває OBS (скрипт OBS сам запустить бота) або запускає бота й застосунок.
//!   3. Якщо щось пішло не так, файли повертаються з резервної копії, а бот запускається знову.
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

fn stage_dir(root: &Path) -> PathBuf {
    root.join("data").join("update")
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
            "downloaded": s.latest["tag"].as_str().map(|t| archive_ready(paths::root(), t, s.latest["size"].as_u64().unwrap_or(0))).unwrap_or(false),
            "source": format!("https://github.com/{REPO}/releases"),
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
                let lines: Vec<String> = body.lines().filter_map(|l| l.trim().strip_prefix("- ").map(first_line)).collect();
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
                "size": asset.map(|a| a["size"].clone()).unwrap_or(json!(0)),
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
            if self.check().await {
                self.prefetch().await; // натискання «Оновити» потім не чекатиме на завантаження
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    }

    /// Тихо завантажує архів нової версії з GitHub у data/update/<версія>.zip.
    pub async fn prefetch(&self) {
        let latest = self.latest();
        let (Some(tag), Some(url)) = (latest["tag"].as_str(), latest["url"].as_str()) else { return };
        let size = latest["size"].as_u64().unwrap_or(0);
        let root = paths::root();
        if archive_ready(root, tag, size) || self.state() != "idle" {
            return;
        }
        let dest = archive_path(root, tag);
        // архіви старіших версій більше не потрібні
        if let Ok(rd) = std::fs::read_dir(stage_dir(root)) {
            for e in rd.flatten() {
                if e.path() != dest && e.path().is_file() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        match download_progress(&self.http, url, &dest, |_, _| {}).await {
            Ok(()) if archive_ready(root, tag, size) => {
                tracing::info!("Оновлення {tag} завантажено з GitHub — його можна встановити кнопкою «Оновити»");
                self.notify();
            }
            Ok(()) => {
                let _ = std::fs::remove_file(&dest);
            }
            Err(e) => tracing::info!("Не вдалося заздалегідь завантажити оновлення ({e}) — завантажу під час встановлення"),
        }
    }
}

/// Куди складаємо архів нової версії.
pub fn archive_path(root: &Path, tag: &str) -> PathBuf {
    stage_dir(root).join(format!("{}.zip", tag.trim_start_matches('v')))
}

/// Архів уже завантажено повністю (розмір збігається з тим, що каже GitHub).
pub fn archive_ready(root: &Path, tag: &str, size: u64) -> bool {
    std::fs::metadata(archive_path(root, tag)).map(|m| m.len() > 0 && (size == 0 || m.len() == size)).unwrap_or(false)
}

/// Завантаження з GitHub із прогресом (завантажено, усього байт).
pub async fn download_progress(http: &reqwest::Client, url: &str, dest: &Path, progress: impl Fn(u64, u64)) -> Result<(), String> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    let r = http
        .get(url)
        .header("User-Agent", "pipabobik-updater")
        .header("Accept", "application/octet-stream")
        .timeout(Duration::from_secs(900))
        .send()
        .await
        .map_err(|e| format!("немає зв'язку з GitHub ({e})"))?;
    if !r.status().is_success() {
        return Err(format!("GitHub відповів {}", r.status().as_u16()));
    }
    let total = r.content_length().unwrap_or(0);
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = dest.with_extension("part");
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    let mut got = 0u64;
    let mut s = r.bytes_stream();
    progress(0, total);
    while let Some(chunk) = s.next().await {
        let chunk = chunk.map_err(|e| format!("завантаження обірвалося ({e})"))?;
        f.write_all(&chunk).await.map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        progress(got, total);
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    if total > 0 && got != total {
        return Err("завантаження обірвалося — спробуйте ще раз".into());
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
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

// ── встановлення (окремий процес з вікном «Оновлення pipabobik») ──

/// Що встановити. `staged` — файли вже розпаковані (так оновлюють старі версії: 2.0.1 і 2.0.2).
#[derive(Debug, Clone, Default)]
pub struct UpdateJob {
    pub target: PathBuf,
    pub tag: String,
    pub message: String,
    pub url: String,
    pub size: u64,
    pub bot_pid: u32,
    pub obs_pid: u32,
    pub obs_exe: String,
    pub bot_args: Vec<String>,
    pub staged: Option<PathBuf>,
}

/// Кроки, які бачить людина у вікні оновлення.
pub const STEPS: &[(&str, &str)] = &[
    ("wait", "Закриваю pipabobik"),
    ("download", "Завантажую з GitHub"),
    ("verify", "Перевіряю архів"),
    ("obs", "Закриваю OBS"),
    ("backup", "Резервна копія"),
    ("files", "Замінюю файли"),
    ("launch", "Запускаю знову"),
];

#[derive(Debug, Clone)]
pub enum Ev {
    /// крок (ключ із STEPS), стан: active | done | skip | fail, текст
    Step(&'static str, &'static str, String),
    Progress(u64, u64),
    Done(String),
    Fail(String),
}

impl Ev {
    pub fn json(&self) -> Value {
        match self {
            Ev::Step(k, st, t) => json!({"type": "step", "key": k, "state": st, "text": t}),
            Ev::Progress(a, b) => json!({"type": "progress", "done": a, "total": b}),
            Ev::Done(t) => json!({"type": "done", "text": t}),
            Ev::Fail(t) => json!({"type": "fail", "text": t}),
        }
    }
}

fn log(target: &Path, msg: &str) {
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
    log(target, &format!("Закриваю OBS (pid {pid})"));
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
    log(target, if ok { "OBS закрито" } else { "OBS не закрився за 45 с — не чіпаю його" });
    ok
}

/// Усі файли в папці, крім налаштувань і даних (так розпізнаємо, що несе нова версія).
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

fn manifest(target: &Path) -> Vec<String> {
    std::fs::read_to_string(target.join("data").join("update-manifest.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Резервна копія лише файлів pipabobik (і тих, які нова версія замінить) — чужі файли в папці не чіпаємо.
fn backup(target: &Path, new: &Path, label: &str) -> Result<PathBuf, String> {
    let folder = target.join("data").join("backups");
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let path = folder.join(format!("pipabobik-{}-{label}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut z = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut files: Vec<String> = code_files(new);
    files.extend(manifest(target));
    files.sort();
    files.dedup();
    for rel in files {
        let src = target.join(&rel);
        if !src.is_file() || rel.contains("..") {
            continue;
        }
        let data = std::fs::read(&src).map_err(|e| e.to_string())?;
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
    Ok(path)
}

/// Повертає файли з резервної копії (якщо заміна обірвалася посередині).
fn restore(backup: &Path, target: &Path) -> Result<(), String> {
    let f = std::fs::File::open(backup).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    for i in 0..z.len() {
        let mut file = z.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = file.enclosed_name() else { continue };
        let dest = target.join(rel);
        if let Some(d) = dest.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if dest.extension().is_some_and(|x| x == "exe") && dest.exists() {
            let _ = std::fs::rename(&dest, dest.with_extension("exe.old"));
        }
        std::fs::write(&dest, buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn install_files(new: &Path, target: &Path) -> Result<Vec<String>, String> {
    let files = code_files(new);
    for rel in &files {
        let src = new.join(rel);
        let dst = target.join(rel);
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        // запущений .exe не можна перезаписати, але можна перейменувати
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
    let mpath = target.join("data").join("update-manifest.json");
    for rel in manifest(target).iter().filter(|r| !files.contains(r) && !r.contains("..")) {
        let _ = std::fs::remove_file(target.join(rel));
    }
    let _ = std::fs::write(&mpath, serde_json::to_string(&files).unwrap_or_default());
    Ok(files)
}

fn launch_bot(t: &Path, bot_args: &[String]) -> std::io::Result<()> {
    let mut args = vec!["--bot".to_string()];
    args.extend(bot_args.iter().cloned());
    args.push("--restart".into());
    log(t, &format!("Запускаю бота: {}", args.join(" ")));
    crate::proc::spawn_detached(&t.join(EXE), &args, t)
}

fn launch_obs(t: &Path, exe: &str) -> std::io::Result<()> {
    let exe = PathBuf::from(exe);
    log(t, &format!("Відкриваю OBS: {}", exe.display()));
    let cwd = exe.parent().map(Path::to_path_buf).unwrap_or_else(|| t.to_path_buf()); // OBS має стартувати зі своєї папки
    crate::proc::spawn_detached(&exe, &[], &cwd)
}

fn mb(b: u64) -> String {
    format!("{:.1}", b as f64 / 1_048_576.0)
}

/// Установлює оновлення по кроках і повідомляє про кожен у вікно оновлення.
pub async fn run_job(job: UpdateJob, http: reqwest::Client, emit: impl Fn(Ev) + Send + Sync + 'static) -> bool {
    let emit = Arc::new(emit);
    let t = job.target.clone();
    let ver = job.tag.trim_start_matches('v').to_string();
    log(&t, &format!("Оновлення до {}: {}", job.tag, job.message));
    let mut result = json!({"ok": false, "tag": job.tag, "message": job.message, "warnings": [], "at": now_secs()});
    let mut obs_closed = false;
    let mut panel_open = false;

    let res: Result<(), String> = async {
        // 1. бот і вікно застосунку
        emit(Ev::Step("wait", "active", "Зупиняю музику й закриваю pipabobik (черга збережеться)".into()));
        let (tt, pid) = (t.clone(), job.bot_pid);
        if !tokio::task::spawn_blocking(move || wait_pid(pid, Duration::from_secs(30), &tt)).await.unwrap_or(false) {
            return Err("pipabobik не закрився за 30 секунд".into());
        }
        let tt = t.clone();
        panel_open = tokio::task::spawn_blocking(move || crate::uninstall::close_instances(&tt)).await.unwrap_or(false);
        emit(Ev::Step("wait", "done", "pipabobik закрито".into()));

        // 2. архів з GitHub
        let new = match &job.staged {
            Some(dir) => {
                emit(Ev::Step("download", "done", "Нову версію вже завантажено".into()));
                emit(Ev::Step("verify", "done", "Архів перевірено".into()));
                dir.clone()
            }
            None => {
                let archive = archive_path(&t, &job.tag);
                if archive_ready(&t, &job.tag, job.size) {
                    emit(Ev::Step("download", "done", format!("Уже завантажено з GitHub ({} МБ)", mb(job.size.max(1)))));
                } else {
                    if job.url.is_empty() {
                        return Err("невідомо, звідки завантажувати — перевірте оновлення ще раз".into());
                    }
                    emit(Ev::Step("download", "active", "Завантажую з GitHub…".into()));
                    let e2 = emit.clone();
                    download_progress(&http, &job.url, &archive, move |a, b| e2(Ev::Progress(a, b))).await?;
                    emit(Ev::Step("download", "done", format!("Завантажено з GitHub ({} МБ)", mb(std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0)))));
                }
                // 3. перевірка
                emit(Ev::Step("verify", "active", "Перевіряю архів…".into()));
                let dir = stage_dir(&t).join("new");
                let _ = std::fs::remove_dir_all(&dir);
                let (a2, d2) = (archive.clone(), dir.clone());
                match tokio::task::spawn_blocking(move || extract(&a2, &d2)).await.map_err(|e| e.to_string())? {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = std::fs::remove_file(&archive); // наступного разу завантажимо заново
                        return Err(e);
                    }
                }
            }
        };
        emit(Ev::Step("verify", "done", "Архів цілий".into()));

        // 4. OBS
        if job.obs_pid != 0 && !job.obs_exe.is_empty() {
            emit(Ev::Step("obs", "active", "Закриваю OBS (сцени й налаштування збережуться)…".into()));
            let tt = t.clone();
            let pid = job.obs_pid;
            obs_closed = tokio::task::spawn_blocking(move || close_obs(pid, &tt)).await.unwrap_or(false);
            if obs_closed {
                emit(Ev::Step("obs", "done", "OBS закрито".into()));
            } else {
                result["warnings"].as_array_mut().unwrap().push(json!("OBS не закрився сам — нова версія скрипта OBS запрацює після його перезапуску"));
                emit(Ev::Step("obs", "skip", "OBS не закрився — продовжую без нього".into()));
            }
        } else {
            emit(Ev::Step("obs", "skip", "OBS не чіпаю (трансляція або OBS не відкрито)".into()));
        }

        // 5–6. резервна копія і заміна
        emit(Ev::Step("backup", "active", "Зберігаю поточну версію…".into()));
        let (tt, nn) = (t.clone(), new.clone());
        let bk = tokio::task::spawn_blocking(move || backup(&tt, &nn, version())).await.map_err(|e| e.to_string())??;
        log(&t, &format!("Резервна копія: {}", bk.display()));
        emit(Ev::Step("backup", "done", "Поточну версію збережено в data\\backups".into()));
        emit(Ev::Step("files", "active", "Замінюю файли…".into()));
        let (tt, nn) = (t.clone(), new.clone());
        match tokio::task::spawn_blocking(move || install_files(&nn, &tt)).await.map_err(|e| e.to_string())? {
            Ok(files) => {
                log(&t, &format!("Оновлено файлів: {}", files.len()));
                emit(Ev::Step("files", "done", format!("Оновлено файлів: {}", files.len())));
            }
            Err(e) => {
                log(&t, &format!("Заміна не вдалася ({e}) — повертаю попередню версію"));
                let (bk2, tt) = (bk.clone(), t.clone());
                let back = tokio::task::spawn_blocking(move || restore(&bk2, &tt)).await.map_err(|e| e.to_string())?;
                return Err(match back {
                    Ok(()) => format!("не вдалося замінити файли ({e}). Попередню версію повернуто"),
                    Err(e2) => format!("не вдалося замінити файли ({e}), і не вдалося повернути попередню ({e2}). Копія — у data\\backups"),
                });
            }
        }
        result["ok"] = json!(true);
        Ok(())
    }
    .await;

    if let Err(e) = &res {
        result["error"] = json!(e);
        log(&t, &format!("ПОМИЛКА: {e}"));
    }
    let _ = std::fs::create_dir_all(t.join("data"));
    let _ = std::fs::write(t.join("data").join("update_result.json"), result.to_string());

    // 7. запуск: OBS, якщо ми його закривали (скрипт OBS запустить бота), інакше — бот і застосунок
    emit(Ev::Step("launch", "active", "Запускаю…".into()));
    let launched = if obs_closed {
        launch_obs(&t, &job.obs_exe).map(|_| "Відкриваю OBS — скрипт сам запустить бота".to_string())
    } else {
        let r = launch_bot(&t, &job.bot_args).map(|_| "pipabobik запущено".to_string());
        if panel_open {
            let _ = crate::proc::spawn_detached(&t.join(EXE), &[], &t); // вікно застосунку, якщо було відкрите
        }
        r
    };
    match launched {
        Ok(text) => emit(Ev::Step("launch", "done", text)),
        Err(e) => {
            log(&t, &format!("Не вдалося перезапустити: {e}"));
            emit(Ev::Step("launch", "fail", format!("Не вдалося запустити: {e}. Відкрийте pipabobik ярликом")));
        }
    }
    match res {
        Ok(()) => {
            let _ = std::fs::write(stage_dir(&t).join("done"), "");
            emit(Ev::Done(format!("Готово! Встановлено версію {ver}")));
            true
        }
        Err(e) => {
            emit(Ev::Fail(format!("Оновлення не вдалося: {e}. Працює попередня версія.")));
            false
        }
    }
}

/// Після оновлення: прибирає старий .exe і папку з архівом.
pub fn cleanup_after_update() {
    let root = paths::root();
    let _ = std::fs::remove_file(root.join(format!("{EXE}.old")));
    let _ = std::fs::remove_file(root.join("PipaBobik.exe.old"));
    if stage_dir(root).join("done").exists() {
        let _ = std::fs::remove_dir_all(stage_dir(root));
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
        let bk = backup(&target, &new, "t").unwrap();
        assert!(bk.to_string_lossy().ends_with("-t.zip"));
        std::fs::write(target.join("obs/pipabobik.lua"), "зіпсовано").unwrap();
        restore(&bk, &target).unwrap();
        assert_eq!(std::fs::read_to_string(target.join("obs/pipabobik.lua")).unwrap(), "lua");
    }

    #[tokio::test]
    async fn job_downloads_from_url_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        // архів «з GitHub»
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            for (n, d) in [(format!("PipaBobik/{EXE}"), "v9"), ("PipaBobik/obs/pipabobik.lua".to_string(), "lua9")] {
                z.start_file(n.as_str(), o).unwrap();
                z.write_all(d.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        let bytes = buf.into_inner();
        let size = bytes.len() as u64;
        let app = axum::Router::new().route("/a.zip", axum::routing::get(move || { let b = bytes.clone(); async move { b } }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/a.zip", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let target = dir.path().join("inst");
        std::fs::create_dir_all(target.join("data")).unwrap();
        std::fs::write(target.join(EXE), "v1").unwrap();
        std::fs::write(target.join("config.toml"), "мій").unwrap();
        let job = UpdateJob { target: target.clone(), tag: "v9.0.0".into(), message: "тест".into(), url, size, ..Default::default() };
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let ok = run_job(job, reqwest::Client::new(), move |e| ev.lock().unwrap().push(e.json())).await;
        assert!(ok);
        assert_eq!(std::fs::read_to_string(target.join(EXE)).unwrap(), "v9");
        assert_eq!(std::fs::read_to_string(target.join("obs/pipabobik.lua")).unwrap(), "lua9");
        assert_eq!(std::fs::read_to_string(target.join("config.toml")).unwrap(), "мій");
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| e["type"] == "progress" && e["done"] == size));
        assert!(ev.iter().any(|e| e["key"] == "files" && e["state"] == "done"));
        assert_eq!(ev.last().unwrap()["type"], "done");
        let res: Value = serde_json::from_str(&std::fs::read_to_string(target.join("data/update_result.json")).unwrap()).unwrap();
        assert_eq!(res["ok"], true);
        // архів пошкоджено — нічого не замінюється
        std::fs::remove_file(archive_path(&target, "v9.0.0")).ok();
        std::fs::write(archive_path(&target, "v9.0.1"), "не zip").unwrap();
        let job = UpdateJob { target: target.clone(), tag: "v9.0.1".into(), url: "http://127.0.0.1:1/x".into(), ..Default::default() };
        assert!(!run_job(job, reqwest::Client::new(), |_| {}).await);
        assert_eq!(std::fs::read_to_string(target.join(EXE)).unwrap(), "v9");
    }
}
