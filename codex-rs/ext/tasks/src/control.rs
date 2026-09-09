use codex_extension_api::ExtensionData;

use crate::extension::thread_tasks;
use crate::ledger::Ledger;
use crate::storage;

/// Read or change the user's session-local capacity without changing task state or pause.
pub async fn parallelism(
    thread_store: &ExtensionData,
    update: Option<usize>,
) -> std::io::Result<usize> {
    let thread = thread_tasks(thread_store)
        .filter(|thread| thread.enabled())
        .ok_or_else(|| std::io::Error::other("The task ledger is unavailable for this session."))?;
    if update.is_some() && thread.parent_dir.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Change /parallelism in the root session. Subagents use one slot when enabled, or zero when off.",
        ));
    }
    storage::mutate(&thread.dir, move |ledger| {
        let changed = update.is_some_and(|capacity| capacity != ledger.parallelism);
        if let Some(capacity) = update {
            ledger.parallelism = capacity;
        }
        (ledger.parallelism, changed)
    })
    .await
}

/// Read the ledger for a human-facing session view.
pub async fn read(thread_store: &ExtensionData) -> std::io::Result<Ledger> {
    let thread = thread_tasks(thread_store)
        .filter(|thread| thread.enabled())
        .ok_or_else(|| std::io::Error::other("The task ledger is unavailable for this session."))?;
    storage::read(&thread.dir).await
}
