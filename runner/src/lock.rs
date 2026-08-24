//! One daemon per user session.
//!
//! A second `errandd` must not fight the first over the database, the API port,
//! or a browser profile. It exits cleanly rather than crashing, because
//! `KeepAlive: {SuccessfulExit: false}` means launchd leaves a clean exit alone
//! instead of thrashing on a crash loop.

use anyhow::{Context, Result};
use std::fs::File;

/// Held for the lifetime of the process. The advisory lock is released by the
/// kernel when the fd closes, so a hard kill cannot leave a stale lock behind.
pub struct RunnerLock {
    _guard: fd_lock::RwLockWriteGuard<'static, File>,
}

impl RunnerLock {
    /// Try to become the one daemon. `Ok(None)` means another instance already
    /// holds the lock, which is a normal outcome rather than an error.
    pub fn acquire() -> Result<Option<Self>> {
        errand_core::paths::ensure_dirs()?;
        let path = errand_core::paths::runner_lock()?;

        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        // The guard borrows the lock, and the lock must outlive the process, so
        // it is deliberately leaked. There is exactly one per process.
        let rw: &'static mut fd_lock::RwLock<File> =
            Box::leak(Box::new(fd_lock::RwLock::new(file)));

        match rw.try_write() {
            Ok(mut guard) => {
                use std::io::{Seek, SeekFrom, Write};
                // Record the pid so a human debugging a stuck runner can find it.
                let _ = guard.set_len(0);
                let _ = guard.seek(SeekFrom::Start(0));
                let _ = write!(guard, "{}", std::process::id());
                let _ = guard.flush();
                Ok(Some(Self { _guard: guard }))
            }
            Err(_) => Ok(None),
        }
    }

    /// Pid recorded in the lock file, for diagnostics when acquisition fails.
    pub fn holder_pid() -> Option<String> {
        let path = errand_core::paths::runner_lock().ok()?;
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}
