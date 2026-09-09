//! One ledger document per thread under `CODEX_HOME/tasks/<thread_id>/ledger.json`.
//!
//! Task writes, user-controlled parallelism, World State, and native completion checks share
//! this document. Writes replace it atomically and are serialized per path inside this process.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::ledger::Ledger;

pub const LEDGER_FILE: &str = "ledger.json";

pub fn ledger_dir(codex_home: &Path, thread_id: &str) -> PathBuf {
    codex_home.join("tasks").join(thread_id)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub async fn read(dir: &Path) -> std::io::Result<Ledger> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || read_blocking(&dir))
        .await
        .map_err(std::io::Error::other)?
}

fn lock_for(dir: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(locks.entry(dir.to_path_buf()).or_default())
}

fn read_blocking(dir: &Path) -> std::io::Result<Ledger> {
    let mut ledger = read_raw(dir)?;
    if let Some(root) = ledger.capacity_root {
        let root_dir = dir
            .parent()
            .ok_or_else(|| std::io::Error::other("missing ledger parent directory"))?
            .join(root.to_string());
        ledger.parallelism = usize::from(read_raw(&root_dir)?.parallelism != 0);
    }
    Ok(ledger)
}

fn read_raw(dir: &Path) -> std::io::Result<Ledger> {
    match std::fs::read(dir.join(LEDGER_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(err) => Err(err),
    }
}

fn write_blocking(dir: &Path, ledger: &Ledger) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let temporary = dir.join(format!("{LEDGER_FILE}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(ledger).map_err(std::io::Error::other)?;
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, dir.join(LEDGER_FILE))
}

/// Read, change, and write back one ledger under its lock. The callback reports whether the
/// document changed so an unchanged read never rewrites the file. The whole step runs on a
/// blocking thread so the lock is never held across an await.
pub async fn mutate<T>(
    dir: &Path,
    change: impl FnOnce(&mut Ledger) -> (T, bool) + Send + 'static,
) -> std::io::Result<T>
where
    T: Send + 'static,
{
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let lock = lock_for(&dir);
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ledger = read_blocking(&dir)?;
        let (value, changed) = change(&mut ledger);
        if changed {
            write_blocking(&dir, &ledger)?;
        }
        Ok(value)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Entry;

    #[tokio::test]
    async fn missing_ledger_reads_empty_and_writes_are_atomic() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = ledger_dir(home.path(), "thread-1");
        assert_eq!(read(&dir).await.expect("read"), Ledger::default());
        let applied = mutate(&dir, |ledger| {
            let applied = ledger.upsert(
                &[Entry {
                    title: Some("Write the storage".to_string()),
                    ..Entry::default()
                }],
                1,
            );
            let changed = applied.changed;
            (applied, changed)
        })
        .await
        .expect("mutate");
        assert!(applied.errors.is_empty());
        let stored = read(&dir).await.expect("read");
        assert_eq!(stored.tasks.len(), 1);
        assert!(
            !dir.join(format!("{LEDGER_FILE}.{}.tmp", std::process::id()))
                .exists()
        );
        mutate(&dir, |_| ((), false)).await.expect("mutate");
    }
}
