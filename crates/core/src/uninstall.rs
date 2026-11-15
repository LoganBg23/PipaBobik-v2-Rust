//! Ярлики в меню «Пуск», запис у «Параметри → Програми» і видалення pipabobik.
//!
//! Видалення (`PipaBobik.exe --uninstall`):
//!   1. питає, чи справді видаляти і чи лишити налаштування з даними;
//!   2. зупиняє бота, вікно застосунку й програвач;
//!   3. копіює себе в TEMP і передає роботу копії (запущений .exe не може видалити сам себе);
//!   4. копія прибирає ярлики, запис у «Програмах» і файли pipabobik, а потім і саму себе.
//!
//! Видаляються лише файли pipabobik: якщо PipaBobik.exe лежить у папці разом з чужими
//! файлами (скажімо, у «Завантаженнях»), ті лишаються на місці.

use std::path::{Path, PathBuf};

use crate::paths;
use crate::updater::{self, EXE};

/// Файли, які приносить архів pipabobik.
const OWN_FILES: &[&str] = &[
    EXE,
    "PipaBobik.exe.old",
    "config.example.toml",
    "install.bat",
    "README.md",
    "obs/pipabobik.lua",
    "Видалити pipabobik.lnk",
];
/// Що лишається, якщо стрімерка хоче зберегти налаштування: їх підхопить наступне встановлення.
const USER_FILES: &[&str] = &["config.toml", "cookies.txt"];
const MENU_FOLDER: &str = "pipabobik";
const UNINSTALL_SHORTCUT: &str = "Видалити pipabobik.lnk";
const REG_KEY: &str = r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\PipaBobik";

// ── вікна з питаннями ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
pub enum Buttons {
    Ok,
    YesNo,
    YesNoCancel,
}

/// Звичайне вікно Windows з питанням. Не на Windows — лише пише в консоль і відповідає «Так».
pub fn ask(title: &str, text: &str, buttons: Buttons) -> Answer {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (t, c) = (wide(text), wide(title));
        let style = match buttons {
            Buttons::Ok => MB_OK | MB_ICONINFORMATION,
            Buttons::YesNo => MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
            Buttons::YesNoCancel => MB_YESNOCANCEL | MB_ICONQUESTION,
        } | MB_SETFOREGROUND;
        let r = unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), style) };
        match r {
            IDYES => Answer::Yes,
            IDNO => Answer::No,
            IDCANCEL => Answer::Cancel,
            _ => Answer::Yes, // «OK»
        }
    }
    #[cfg(not(windows))]
    {
        let _ = buttons;
        println!("{title}\n{text}");
        Answer::Yes
    }
}

// ── PowerShell (ярлики й реєстр) ──

#[cfg_attr(not(windows), allow(dead_code))]
fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Рядок для PowerShell в одинарних лапках.
fn ps_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Запускає скрипт PowerShell без вікна. Скрипт передається закодованим, тож лапки,
/// пробіли й кирилиця в шляхах нічого не ламають.
pub fn run_powershell(script: &str) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &base64(&utf16)])
            .creation_flags(crate::proc::CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = script;
        false
    }
}

/// Скрипт: ярлики в меню «Пуск» і в папці бота + запис у «Параметри → Програми».
pub fn register_script(exe: &Path, root: &Path, version: &str) -> String {
    let (exe, root) = (exe.to_string_lossy(), root.to_string_lossy());
    format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$exe = {exe}
$dir = {root}
$sh = New-Object -ComObject WScript.Shell
$menu = Join-Path ([Environment]::GetFolderPath('Programs')) {menu}
New-Item -ItemType Directory -Force -Path $menu | Out-Null
function Link($path, $arguments, $desc) {{
  $s = $sh.CreateShortcut($path)
  $s.TargetPath = $exe; $s.Arguments = $arguments; $s.WorkingDirectory = $dir
  $s.IconLocation = "$exe,0"; $s.Description = $desc; $s.Save()
}}
Link (Join-Path $menu 'pipabobik.lnk') '' 'pipabobik — музика з чату Twitch'
Link (Join-Path $menu {un}) '--uninstall' 'Видалити pipabobik з комп''ютера'
Link (Join-Path $dir {un}) '--uninstall' 'Видалити pipabobik з комп''ютера'
$k = {key}
New-Item -Path $k -Force | Out-Null
Set-ItemProperty -Path $k -Name DisplayName -Value 'pipabobik'
Set-ItemProperty -Path $k -Name DisplayVersion -Value {version}
Set-ItemProperty -Path $k -Name Publisher -Value 'LoganBg23'
Set-ItemProperty -Path $k -Name DisplayIcon -Value "$exe,0"
Set-ItemProperty -Path $k -Name InstallLocation -Value $dir
Set-ItemProperty -Path $k -Name UninstallString -Value ('"' + $exe + '" --uninstall')
Set-ItemProperty -Path $k -Name URLInfoAbout -Value 'https://github.com/{repo}'
New-ItemProperty -Path $k -Name NoModify -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $k -Name NoRepair -Value 1 -PropertyType DWord -Force | Out-Null
"#,
        exe = ps_str(&exe),
        root = ps_str(&root),
        menu = ps_str(MENU_FOLDER),
        un = ps_str(UNINSTALL_SHORTCUT),
        key = ps_str(REG_KEY),
        version = ps_str(version),
        repo = updater::REPO,
    )
}

/// Скрипт: прибрати ярлики pipabobik і запис у «Програмах».
/// Ярлики на робочому столі прибираються, лише якщо ведуть саме на цей PipaBobik.exe.
pub fn unregister_script(exe: &Path) -> String {
    format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$exe = {exe}
$sh = New-Object -ComObject WScript.Shell
Remove-Item -Recurse -Force (Join-Path ([Environment]::GetFolderPath('Programs')) {menu})
$desk = [Environment]::GetFolderPath('Desktop')
Get-ChildItem -Path $desk -Filter '*.lnk' | ForEach-Object {{
  if ($sh.CreateShortcut($_.FullName).TargetPath -eq $exe) {{ Remove-Item -Force $_.FullName }}
}}
Remove-Item -Recurse -Force -Path {key}
"#,
        exe = ps_str(&exe.to_string_lossy()),
        menu = ps_str(MENU_FOLDER),
        key = ps_str(REG_KEY),
    )
}

/// Створює ярлики й запис у «Програмах», якщо їх ще немає для цієї папки й версії.
/// Викликається при запуску застосунку: так усе з'являється й без install.bat,
/// а після перенесення папки чи оновлення — оновлюється.
pub fn register_if_needed() {
    if !cfg!(windows) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let marker = paths::data_dir().join(".registered");
    let want = format!("{}|{}", exe.display(), updater::version());
    if std::fs::read_to_string(&marker).map(|s| s == want).unwrap_or(false) {
        return;
    }
    if run_powershell(&register_script(&exe, paths::root(), updater::version())) {
        let _ = std::fs::create_dir_all(paths::data_dir());
        let _ = std::fs::write(&marker, want);
    }
}

// ── видалення файлів ──

#[derive(Debug, Default)]
pub struct Removal {
    pub removed: usize,
    pub failed: Vec<String>,
    pub folder_removed: bool,
}

fn rm_file(p: &Path, r: &mut Removal) {
    if !p.exists() {
        return;
    }
    match std::fs::remove_file(p) {
        Ok(()) => r.removed += 1,
        Err(e) => r.failed.push(format!("{}: {e}", p.display())),
    }
}

fn rm_dir(p: &Path, r: &mut Removal) {
    if !p.is_dir() {
        return;
    }
    match std::fs::remove_dir_all(p) {
        Ok(()) => r.removed += 1,
        Err(e) => r.failed.push(format!("{}: {e}", p.display())),
    }
}

/// Порожні папки (obs\) прибираємо, непорожні — не чіпаємо.
fn rm_empty(p: &Path) {
    let _ = std::fs::remove_dir(p);
}

/// Видаляє файли pipabobik з папки `root`. keep_data — лишити config.toml і data\.
pub fn remove_files(root: &Path, keep_data: bool) -> Removal {
    let mut r = Removal::default();
    let data = root.join("data");
    // файли, які поставили оновлення (їх список веде updater)
    let manifest: Vec<String> = std::fs::read_to_string(data.join("update-manifest.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    for rel in OWN_FILES.iter().map(|s| s.to_string()).chain(manifest) {
        if rel.contains("..") {
            continue;
        }
        rm_file(&root.join(&rel), &mut r);
    }
    rm_empty(&root.join("obs"));
    rm_dir(&root.join("bin"), &mut r); // yt-dlp, який бот завантажив сам
    if keep_data {
        // службове, що не потрібне наступному встановленню
        for sub in ["update", "backups", "tmp"] {
            rm_dir(&data.join(sub), &mut r);
        }
        rm_file(&data.join(".registered"), &mut r);
    } else {
        for f in USER_FILES {
            rm_file(&root.join(f), &mut r);
        }
        // data\ видаляємо, лише якщо це справді папка pipabobik
        if data.join("panel.key").exists() || data.join("queue.json").exists() {
            rm_dir(&data, &mut r);
        }
    }
    rm_empty(root);
    r.folder_removed = !root.exists();
    r
}

/// Чи схожа папка на встановлений pipabobik (захист від видалення не тієї папки).
pub fn looks_installed(root: &Path) -> bool {
    root.join(EXE).is_file() && root.parent().is_some()
}

/// Копія PipaBobik.exe у TEMP, яка доробить видалення, коли цей процес закриється.
pub fn temp_copy() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dest = std::env::temp_dir().join(format!("pipabobik-uninstall-{}{}", std::process::id(), if cfg!(windows) { ".exe" } else { "" }));
    std::fs::copy(&exe, &dest)?;
    Ok(dest)
}

/// Тимчасова копія видаляє себе: cmd чекає кілька секунд і стирає файл.
pub fn delete_self_later() {
    #[cfg(windows)]
    if let Ok(me) = std::env::current_exe() {
        use std::os::windows::process::CommandExt;
        let cmd = format!("ping -n 4 127.0.0.1 >nul & del /f /q \"{}\"", me.display());
        let _ = std::process::Command::new("cmd")
            .args(["/d", "/c", &cmd])
            .creation_flags(crate::proc::CREATE_NO_WINDOW | crate::proc::DETACHED_PROCESS)
            .spawn();
    }
}

/// Інші запущені PipaBobik.exe з цієї папки (вікно застосунку, бот): закрити перед видаленням.
pub fn kill_other_instances(root: &Path) -> usize {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let me = std::process::id();
    let target = root.join(EXE);
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));
    let mut n = 0;
    for (pid, p) in sys.processes() {
        // на Linux sysinfo показує й потоки як окремі процеси — їх пропускаємо
        if pid.as_u32() != me && p.thread_kind().is_none() && p.exe().map(|e| same_path(e, &target)).unwrap_or(false) && p.kill() {
            n += 1;
        }
    }
    n
}

/// Закриває інші PipaBobik.exe з цієї папки. true — серед них було вікно застосунку
/// (тоді після оновлення його треба відкрити знову).
pub fn close_instances(root: &Path) -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let me = std::process::id();
    let target = root.join(EXE);
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always).with_cmd(UpdateKind::Always));
    let mut panel = false;
    for (pid, p) in sys.processes() {
        if pid.as_u32() == me || p.thread_kind().is_some() || !p.exe().map(|e| same_path(e, &target)).unwrap_or(false) {
            continue;
        }
        let modes = ["--bot", "--obs", "--live", "--paused", "--restart", "--update", "--apply-update", "--uninstall"];
        if !p.cmd().iter().any(|c| modes.contains(&c.to_string_lossy().as_ref())) {
            panel = true;
        }
        p.kill();
    }
    panel
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path().join("PipaBobik");
        for f in [EXE, "config.example.toml", "README.md", "obs/pipabobik.lua", "config.toml", "bin/yt-dlp.exe", "data/panel.key", "data/queue.json", "data/backups/a.zip", "new-from-update.txt", "мій-файл.txt"] {
            let p = r.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        std::fs::write(r.join("data/update-manifest.json"), r#"["new-from-update.txt"]"#).unwrap();
        dir
    }

    #[test]
    fn keeps_settings_and_foreign_files() {
        let d = setup();
        let r = d.path().join("PipaBobik");
        assert!(looks_installed(&r));
        let res = remove_files(&r, true);
        assert!(res.failed.is_empty(), "{:?}", res.failed);
        assert!(!r.join(EXE).exists() && !r.join("obs").exists() && !r.join("bin").exists());
        assert!(!r.join("new-from-update.txt").exists());
        assert!(r.join("config.toml").exists() && r.join("data/queue.json").exists());
        assert!(!r.join("data/backups").exists());
        assert!(r.join("мій-файл.txt").exists()); // чуже не чіпаємо
        assert!(!res.folder_removed);
    }

    #[test]
    fn removes_everything() {
        let d = setup();
        let r = d.path().join("PipaBobik");
        std::fs::remove_file(r.join("мій-файл.txt")).unwrap();
        let res = remove_files(&r, false);
        assert!(res.folder_removed, "{:?}", res.failed);
    }

    #[test]
    fn scripts() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        let s = register_script(Path::new(r"C:\Pipa's\PipaBobik.exe"), Path::new(r"C:\Pipa's"), "2.0.3");
        assert!(s.contains(r"$exe = 'C:\Pipa''s\PipaBobik.exe'"));
        assert!(s.contains("'--uninstall'") && s.contains("DisplayVersion -Value '2.0.3'"));
        assert!(unregister_script(Path::new(r"C:\x\PipaBobik.exe")).contains("Remove-Item -Recurse -Force -Path 'HKCU:"));
    }
}
