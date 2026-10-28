//! Опциональный singleton PID-lock (эталон — code-index `daemon_core/lock.rs`).
//!
//! Активен ТОЛЬКО когда задан путь к PID-файлу (env `MCP_PID_FILE`) — это
//! сценарий Windows под mcp-supervisor / Scheduled Task, где гонкой можно
//! стартовать второй экземпляр, а Windows ещё и переиспользует PID.
//!
//! В Docker (деплой на ВМ) переменная не задаётся → lock пропускается:
//! singleton там гарантирует сам контейнер (`container_name` + `restart` +
//! bind порта), а stale PID-файл при нечистом убийстве только мешал бы рестарту.

use std::path::PathBuf;

use anyhow::{bail, Result};

/// RAII-guard PID-lock. При drop удаляет PID-файл.
pub struct PidLock {
    path: PathBuf,
}

impl PidLock {
    /// Захватить lock, если задан путь. `None` → lock не нужен (Docker).
    ///
    /// Если файл есть и процесс с записанным PID жив — ошибка. Если процесс
    /// мёртв (или PID-файл устарел) — перезаписываем своим PID.
    pub fn acquire_optional(path: Option<PathBuf>, who: &str) -> Result<Option<PidLock>> {
        let Some(path) = path else {
            return Ok(None);
        };

        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    if is_process_alive(pid) {
                        bail!(
                            "{who} уже запущен (PID {pid}). PID-файл: {}",
                            path.display()
                        );
                    }
                }
            }
            eprintln!(
                "[pidlock] устаревший PID-файл, перезаписываю: {}",
                path.display()
            );
        }

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Some(PidLock { path }))
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Жив ли процесс с данным PID (кроссплатформенно через sysinfo 0.32).
fn is_process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let spid = Pid::from(pid as usize);
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), false);
    sys.process(spid).is_some()
}
