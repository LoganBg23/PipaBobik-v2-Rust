//! Процеси системи: OBS, «загублені» mpv, навантаження на процесор, Job Object на Windows.

use std::sync::Mutex;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const OBS_NAMES: &[&str] = &["obs64.exe", "obs32.exe", "obs.exe", "obs"];

fn is_obs(name: &str) -> bool {
    OBS_NAMES.contains(&name.to_lowercase().as_str())
}

pub fn pid_alive(pid: u32) -> bool {
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
    sys.process(p).is_some()
}

/// Стежить за OBS. Спершу перевіряє вже знайдений процес — це майже безкоштовно;
/// повний перебір процесів — лише якщо OBS ще не знаходили або його перезапустили.
pub struct ObsWatch {
    sys: Mutex<System>,
    pid: Mutex<Option<Pid>>,
}

impl Default for ObsWatch {
    fn default() -> Self {
        Self { sys: Mutex::new(System::new()), pid: Mutex::new(None) }
    }
}

impl ObsWatch {
    pub fn running(&self) -> bool {
        let mut sys = self.sys.lock().unwrap();
        let mut cached = self.pid.lock().unwrap();
        if let Some(p) = *cached {
            sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
            if sys.process(p).map(|x| is_obs(&x.name().to_string_lossy())).unwrap_or(false) {
                return true;
            }
            *cached = None;
        }
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let found = sys.processes().iter().find(|(_, p)| is_obs(&p.name().to_string_lossy())).map(|(pid, _)| *pid);
        *cached = found;
        found.is_some()
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid.lock().unwrap().map(|p| p.as_u32())
    }

    pub fn exe(&self) -> Option<String> {
        let p = (*self.pid.lock().unwrap())?;
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));
        sys.process(p).and_then(|x| x.exe().map(|e| e.to_string_lossy().into_owned()))
    }
}

/// Закриває mpv від попереднього запуску бота (якщо той аварійно завершився).
pub fn kill_orphan_players() -> usize {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
    let mut n = 0;
    for p in sys.processes().values() {
        let name = p.name().to_string_lossy().to_lowercase();
        let cmd: String = p.cmd().iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>().join(" ");
        if name.contains("mpv") && cmd.contains("pipabobik-mpv") && p.kill() {
            n += 1;
        }
    }
    n
}

/// Навантаження на процесор (як у Диспетчері завдань): середнє за `secs` секунд.
pub async fn cpu_percent(pids: &[u32], secs: u64) -> Vec<f32> {
    let pids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    let mut sys = System::new();
    let kind = ProcessRefreshKind::nothing().with_cpu();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
    pids.iter().map(|p| sys.process(*p).map(|x| x.cpu_usage() / cores).unwrap_or(0.0)).collect()
}

/// Поміщає цей процес у Job Object з KILL_ON_JOB_CLOSE: усі дочірні процеси (mpv, yt-dlp, Deno)
/// гарантовано закриються разом із ним — навіть якщо його вбили через Диспетчер завдань.
#[cfg(windows)]
pub fn kill_children_with_me() -> bool {
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static DONE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DONE.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return false;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // KILL_ON_JOB_CLOSE | BREAKAWAY_OK (щоб за потреби процес міг запустити щось незалежне)
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            return false;
        }
        // дескриптор job свідомо не закриваємо: він живе, доки живе процес
        AssignProcessToJobObject(job, GetCurrentProcess()) != 0
    })
}

#[cfg(not(windows))]
pub fn kill_children_with_me() -> bool {
    false
}
