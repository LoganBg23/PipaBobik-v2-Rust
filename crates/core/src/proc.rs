//! Запуск дочірніх процесів без вікон консолі і їх облік.

use std::ffi::OsStr;

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Команда без чорного вікна консолі.
pub fn hidden(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut c = tokio::process::Command::new(program);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    c.kill_on_drop(true);
    c.stdin(std::process::Stdio::null());
    c
}

/// Запускає незалежний процес (переживе цей): перезапуск, оновлення, OBS.
pub fn spawn_detached(program: &std::path::Path, args: &[String], cwd: &std::path::Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut c = Command::new(program);
    c.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        // новий процес не має закритися разом із цим (див. winproc: Job Object)
        c.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        if c.spawn().is_ok() {
            return Ok(());
        }
        c.creation_flags(base);
        c.spawn().map(|_| ())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
        c.spawn().map(|_| ())
    }
}
