use super::*;
use crate::ledger::Entry;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn root_capacity_reaches_existing_children_and_grandchildren_without_restarting() {
    let home = tempfile::tempdir().unwrap();
    let config = TasksExtensionConfig {
        codex_home: home.path().to_path_buf(),
        ..Default::default()
    };
    let extension = TasksExtension {
        resolve: Arc::new(Clone::clone),
    };
    let session = ExtensionData::new("session");
    let ids = [ThreadId::new(), ThreadId::new(), ThreadId::new()];
    let threads = ids.map(|id| ExtensionData::new(id.to_string()));
    for (index, thread) in threads.iter().enumerate() {
        let source = if index == 0 {
            SessionSource::Cli
        } else {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ids[index - 1],
                depth: index as i32,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })
        };
        extension
            .on_thread_start(ThreadStartInput {
                config: &config,
                session_source: &source,
                persistent_thread_state_available: true,
                environments: &[],
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &session,
                thread_store: thread,
            })
            .await;
    }
    for capacity in [0, 4, 2, 0] {
        crate::parallelism(&threads[0], Some(capacity))
            .await
            .unwrap();
        for (index, thread) in threads.iter().enumerate() {
            let expected = if index == 0 {
                capacity
            } else {
                usize::from(capacity != 0)
            };
            assert_eq!(
                crate::parallelism(thread, /*update*/ None).await.unwrap(),
                expected
            );
        }
    }
    assert_eq!(
        crate::parallelism(&threads[1], Some(3))
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn lowering_capacity_preserves_active_work_and_pause_but_rejects_new_work() {
    let home = tempfile::tempdir().unwrap();
    let thread = ExtensionData::new("thread");
    thread.insert(ThreadTasks {
        thread_id: ThreadId::new(),
        dir: home.path().to_path_buf(),
        parent_dir: None,
        agent_path: None,
        enabled: std::sync::atomic::AtomicBool::new(/*v*/ true),
    });
    let before = storage::mutate(home.path(), |ledger| {
        let entries = ["First outcome", "Second outcome"].map(|title| Entry {
            title: Some(title.to_string()),
            status: Some("active".to_string()),
            ..Default::default()
        });
        assert!(ledger.upsert(&entries, /*now*/ 1000).errors.is_empty());
        ledger.pause = Some("User requested a pause".to_string());
        (ledger.clone(), true)
    })
    .await
    .unwrap();
    crate::parallelism(&thread, Some(1)).await.unwrap();
    let mut expected = before;
    expected.parallelism = 1;
    assert_eq!(crate::read(&thread).await.unwrap(), expected);
    let applied = storage::mutate(home.path(), |ledger| {
        let applied = ledger.upsert(
            &[Entry {
                title: Some("Third outcome".to_string()),
                status: Some("active".to_string()),
                ..Default::default()
            }],
            /*now*/ 2000,
        );
        let changed = applied.changed;
        (applied, changed)
    })
    .await
    .unwrap();
    assert!(!applied.errors.is_empty());
    assert_eq!(crate::read(&thread).await.unwrap(), expected);
}
