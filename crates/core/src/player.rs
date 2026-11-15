//! Відтворення через mpv: окремий процес на кожен трек + IPC для паузи, гучності й перемотки.
//!
//! В OBS музика захоплюється джерелом «Захоплення звуку програми» → mpv.exe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Notify};

use crate::paths;
use crate::proc::hidden;

pub fn find_mpv(configured: &str) -> Option<PathBuf> {
    if !configured.is_empty() && configured != "mpv" && Path::new(configured).exists() {
        return Some(PathBuf::from(configured));
    }
    if let Some(p) = paths::which("mpv") {
        return Some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for env in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA", "USERPROFILE"] {
            let Ok(base) = std::env::var(env) else { continue };
            let b = PathBuf::from(base);
            candidates.push(b.join("mpv").join("mpv.exe"));
            candidates.push(b.join("scoop").join("shims").join("mpv.exe"));
            // «MPV Player», «mpv.net», «Programs\mpv» тощо
            for dir in [b.clone(), b.join("Programs")] {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().to_lowercase().contains("mpv") {
                            candidates.push(e.path().join("mpv.exe"));
                        }
                    }
                }
            }
        }
        if let Ok(la) = std::env::var("LOCALAPPDATA") {
            let winget = PathBuf::from(la).join("Microsoft").join("WinGet");
            candidates.push(winget.join("Links").join("mpv.exe"));
            if let Ok(rd) = std::fs::read_dir(winget.join("Packages")) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().to_lowercase().contains("mpv") {
                        candidates.push(e.path().join("mpv.exe"));
                        if let Ok(sub) = std::fs::read_dir(e.path()) {
                            candidates.extend(sub.flatten().map(|s| s.path().join("mpv.exe")));
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    candidates.extend(["/opt/homebrew/bin/mpv", "/usr/local/bin/mpv", "/usr/bin/mpv"].map(PathBuf::from));
    candidates.into_iter().find(|c| c.is_file())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ended,
    Stopped,
    Error,
}

#[derive(Default)]
struct Clock {
    started: Option<Instant>,
    offset: f64,
    paused_total: f64,
    paused_since: Option<Instant>,
}

impl Clock {
    fn position(&self) -> f64 {
        let Some(s) = self.started else { return 0.0 };
        let now = self.paused_since.unwrap_or_else(Instant::now);
        (now.duration_since(s).as_secs_f64() + self.offset - self.paused_total).max(0.0)
    }
}

struct State {
    clock: Clock,
    paused: bool,
    volume: i64,
    pid: Option<u32>,
    stop: Option<Arc<Notify>>,
    stopped_by_us: bool,
    last_played: f64,
    last_error: String,
}

pub struct Player {
    pub mpv: Option<PathBuf>,
    pub show_window: bool,
    pub normalize: std::sync::atomic::AtomicBool,
    pub audio_device: String,
    ipc: String,
    st: Mutex<State>,
    running: watch::Sender<bool>,
}

impl Player {
    pub fn new(mpv_path: &str, volume: i64, normalize: bool, audio_device: &str, show_window: bool) -> Self {
        #[cfg(windows)]
        let ipc = r"\\.\pipe\pipabobik-mpv".to_string();
        #[cfg(not(windows))]
        let ipc = format!("/tmp/pipabobik-mpv-{}.sock", std::process::id());
        Self {
            mpv: find_mpv(mpv_path),
            // OBS «Захоплення звуку програми» бачить лише програми з вікном,
            // тому на Windows mpv відкривається згорнутим вікном «pipabobik».
            show_window: show_window && cfg!(windows),
            normalize: std::sync::atomic::AtomicBool::new(normalize),
            audio_device: audio_device.into(),
            ipc,
            st: Mutex::new(State {
                clock: Clock::default(),
                paused: false,
                volume: volume.clamp(0, 100),
                pid: None,
                stop: None,
                stopped_by_us: false,
                last_played: 0.0,
                last_error: String::new(),
            }),
            running: watch::channel(false).0,
        }
    }

    pub fn available(&self) -> bool {
        self.mpv.is_some()
    }
    pub fn position(&self) -> f64 {
        self.st.lock().unwrap().clock.position()
    }
    pub fn paused(&self) -> bool {
        self.st.lock().unwrap().paused
    }
    pub fn volume(&self) -> i64 {
        self.st.lock().unwrap().volume
    }
    pub fn pid(&self) -> Option<u32> {
        self.st.lock().unwrap().pid
    }
    pub fn last_played(&self) -> f64 {
        self.st.lock().unwrap().last_played
    }
    pub fn last_error(&self) -> String {
        self.st.lock().unwrap().last_error.clone()
    }
    pub fn is_playing(&self) -> bool {
        *self.running.borrow()
    }

    pub fn command_args(&self, url: &str, headers: &HashMap<String, String>, start: f64) -> Vec<String> {
        let volume = self.volume();
        let mut cmd: Vec<String> = vec!["--no-video".into(), "--no-terminal".into()];
        if self.show_window {
            cmd.extend(["--force-window=yes", "--window-minimized=yes", "--geometry=320x80"].map(String::from));
        } else {
            cmd.push("--force-window=no".into());
        }
        cmd.extend(
            [
                "--audio-display=no",
                "--idle=no",
                "--title=pipabobik",
                // нічого зайвого: без конфігів, скриптів, OSD та ytdl-хука (посилання вже пряме)
                "--config=no",
                "--load-scripts=no",
                "--osc=no",
                "--ytdl=no",
                "--input-default-bindings=no",
                "--cache=yes",
                "--demuxer-max-bytes=16MiB",
                "--demuxer-readahead-secs=30",
            ]
            .map(String::from),
        );
        cmd.push(format!("--volume={volume}"));
        cmd.push(format!("--input-ipc-server={}", self.ipc));
        if self.normalize.load(std::sync::atomic::Ordering::Relaxed) {
            // dynaudnorm працює на рідній частоті — у рази легший за loudnorm
            cmd.push("--af=lavfi=[dynaudnorm=f=300:g=15:p=0.9:m=6]".into());
        }
        if !self.audio_device.is_empty() {
            cmd.push(format!("--audio-device={}", self.audio_device));
        }
        let mut keys: Vec<&String> = headers.keys().collect();
        keys.sort();
        for k in keys {
            let v = &headers[k];
            match k.to_lowercase().as_str() {
                "user-agent" => cmd.push(format!("--user-agent={v}")),
                "accept-encoding" => {}
                _ => cmd.push(format!("--http-header-fields-append={k}: {v}")),
            }
        }
        if start > 0.0 {
            cmd.push(format!("--start=+{start:.1}"));
        }
        cmd.push(url.into());
        cmd
    }

    /// Грає трек до кінця (з позиції start, секунд).
    pub async fn play(&self, url: &str, headers: &HashMap<String, String>, start: f64) -> Outcome {
        let Some(mpv) = self.mpv.clone() else {
            tracing::error!("mpv не знайдено — встановіть його (див. README)");
            tokio::time::sleep(Duration::from_secs(5)).await;
            return Outcome::Error;
        };
        self.stop().await;
        let args = self.command_args(url, headers, start);
        let mut cmd = hidden(&mpv);
        cmd.args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Не вдалося запустити mpv: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                return Outcome::Error;
            }
        };
        let stop = Arc::new(Notify::new());
        {
            let mut st = self.st.lock().unwrap();
            st.stopped_by_us = false;
            st.paused = false;
            st.clock = Clock { started: Some(Instant::now()), offset: start.max(0.0), paused_total: 0.0, paused_since: None };
            st.pid = child.id();
            st.stop = Some(stop.clone());
        }
        self.running.send_replace(true);
        let mut stderr = child.stderr.take();
        let err_task = tokio::spawn(async move {
            let mut s = String::new();
            if let Some(e) = stderr.as_mut() {
                let _ = e.read_to_string(&mut s).await;
            }
            s
        });
        let status = tokio::select! {
            s = child.wait() => s,
            _ = stop.notified() => {
                let _ = child.start_kill();
                match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                    Ok(s) => s,
                    Err(_) => { let _ = child.kill().await; child.wait().await }
                }
            }
        };
        let err = err_task.await.unwrap_or_default();
        let err = err.split_whitespace().collect::<Vec<_>>().join(" ");
        let err: String = err.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        let (played, stopped) = {
            let mut st = self.st.lock().unwrap();
            let played = st.clock.position();
            st.last_played = played;
            st.last_error = err.clone();
            st.clock = Clock::default();
            st.pid = None;
            st.stop = None;
            st.paused = false;
            (played, st.stopped_by_us)
        };
        self.running.send_replace(false);
        if stopped {
            return Outcome::Stopped;
        }
        let ok = status.map(|s| s.success()).unwrap_or(false);
        if !ok && played - start < 3.0 {
            tracing::warn!("mpv завершився з помилкою: {err}");
            return Outcome::Error;
        }
        Outcome::Ended
    }

    pub async fn stop(&self) {
        let notify = {
            let mut st = self.st.lock().unwrap();
            match st.stop.take() {
                Some(n) => {
                    st.stopped_by_us = true;
                    n
                }
                None => return,
            }
        };
        notify.notify_one();
        let mut rx = self.running.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|r| !*r)).await;
    }

    // ── IPC ──
    async fn send_ipc(&self, command: serde_json::Value) -> bool {
        let payload = format!("{}\n", json!({ "command": command }));
        let res: std::io::Result<()> = async {
            use tokio::io::AsyncWriteExt;
            #[cfg(windows)]
            {
                let mut pipe = tokio::net::windows::named_pipe::ClientOptions::new().open(&self.ipc)?;
                pipe.write_all(payload.as_bytes()).await?;
                pipe.flush().await?;
            }
            #[cfg(unix)]
            {
                let mut s = tokio::time::timeout(Duration::from_secs(1), tokio::net::UnixStream::connect(&self.ipc))
                    .await
                    .map_err(|_| std::io::Error::other("timeout"))??;
                s.write_all(payload.as_bytes()).await?;
                s.flush().await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = &res {
            tracing::debug!("IPC mpv недоступний: {e}");
        }
        res.is_ok()
    }

    pub async fn set_volume(&self, vol: i64) {
        let v = vol.clamp(0, 100);
        self.st.lock().unwrap().volume = v;
        if self.is_playing() {
            self.send_ipc(json!(["set_property", "volume", v])).await;
        }
    }

    /// Перемотує на позицію в секундах від початку треку.
    pub async fn seek(&self, position: f64) -> bool {
        if !self.is_playing() {
            return false;
        }
        let position = position.max(0.0);
        if !self.send_ipc(json!(["seek", position, "absolute"])).await {
            return false;
        }
        // підлаштовуємо власний облік позиції під нову точку
        let mut st = self.st.lock().unwrap();
        let c = &mut st.clock;
        if let Some(s) = c.started {
            let now = c.paused_since.unwrap_or_else(Instant::now);
            c.offset = position - now.duration_since(s).as_secs_f64() + c.paused_total;
        }
        true
    }

    pub async fn set_pause(&self, paused: bool) {
        if !self.is_playing() || paused == self.paused() {
            return;
        }
        if self.send_ipc(json!(["set_property", "pause", paused])).await {
            let mut st = self.st.lock().unwrap();
            st.paused = paused;
            if paused {
                st.clock.paused_since = Some(Instant::now());
            } else if let Some(since) = st.clock.paused_since.take() {
                st.clock.paused_total += since.elapsed().as_secs_f64();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let p = Player::new("mpv", 150, true, "", true);
        assert_eq!(p.volume(), 100);
        let mut h = HashMap::new();
        h.insert("User-Agent".to_string(), "UA".to_string());
        h.insert("Accept-Encoding".to_string(), "gzip".to_string());
        h.insert("Referer".to_string(), "x".to_string());
        let a = p.command_args("http://u", &h, 12.34);
        assert!(a.contains(&"--user-agent=UA".to_string()));
        assert!(a.contains(&"--http-header-fields-append=Referer: x".to_string()));
        assert!(!a.iter().any(|x| x.contains("gzip")));
        assert!(a.contains(&"--start=+12.3".to_string()));
        assert_eq!(a.last().unwrap(), "http://u");
        assert!(a.iter().any(|x| x.starts_with("--af=")));
    }

    #[tokio::test]
    async fn stop_without_play_is_noop() {
        let p = Player::new("/nonexistent/mpv", 50, false, "", false);
        p.stop().await;
        assert!(!p.seek(10.0).await);
        assert_eq!(p.position(), 0.0);
    }
}
