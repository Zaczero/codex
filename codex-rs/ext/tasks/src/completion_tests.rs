use super::*;
use crate::ledger::Ledger;
use crate::ledger::Status;
use crate::ledger::Task;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;

#[test]
fn capacity_excludes_external_waits_and_zero_allows_independent_work() {
    let mut ledger = Ledger {
        parallelism: 1,
        ..Default::default()
    };
    let entries = [
        crate::ledger::Entry {
            title: Some("Await external review".to_owned()),
            status: Some("active".to_owned()),
            waiting: Some(Some("Review".to_owned())),
            ..Default::default()
        },
        crate::ledger::Entry {
            title: Some("Implement ready work".to_owned()),
            status: Some("active".to_owned()),
            ..Default::default()
        },
    ];
    assert!(ledger.upsert(&entries, 1).errors.is_empty());
    assert_eq!(ledger.counts().get(&crate::ledger::View::Active), Some(&1));
    ledger.parallelism = 0;
    let entries = (0..6)
        .map(|index| crate::ledger::Entry {
            title: Some(format!("Independent {index}")),
            status: Some("active".to_owned()),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    assert!(ledger.upsert(&entries, 2).errors.is_empty());
    assert_eq!(ledger.counts().get(&crate::ledger::View::Active), Some(&7));
}

#[tokio::test]
async fn completion_obeys_feature_capacity_pause_external_wait_and_child_return() {
    for child in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root_id = ThreadId::new();
        let root_dir = temporary.path().join(root_id.to_string());
        let child_dir = temporary.path().join(ThreadId::new().to_string());
        let dir = if child { &child_dir } else { &root_dir };
        let session = ExtensionData::new("session");
        let root = ExtensionData::new(root_id.to_string());
        root.insert(ThreadTasks {
            thread_id: root_id,
            dir: root_dir.clone(),
            parent_dir: None,
            agent_path: None,
            enabled: AtomicBool::new(/*v*/ true),
        });
        let thread = ExtensionData::new("thread");
        let turn = ExtensionData::new("turn");
        let identity = thread.get_or_init(|| ThreadTasks {
            thread_id: ThreadId::new(),
            dir: dir.clone(),
            parent_dir: child.then(|| root_dir.clone()),
            agent_path: None,
            enabled: AtomicBool::new(true),
        });
        storage::mutate(dir, move |ledger| {
            ledger.capacity_root = child.then_some(root_id);
            ledger.tasks.push(Task {
                id: "1".to_owned(),
                title: "Finish the implementation".to_owned(),
                status: Status::Active,
                after: Vec::new(),
                cancelled: None,
                waiting: None,
                owner: None,
                body: String::new(),
                findings: Vec::new(),
                created: 0,
                updated: 0,
            });
            ((), true)
        })
        .await
        .unwrap();
        let extension = TasksExtension::<()> {
            resolve: Arc::new(|_| TasksExtensionConfig::default()),
        };
        let attempt = || {
            extension.on_completion_attempt(TurnStopInput {
                session_store: &session,
                thread_store: &thread,
                turn_store: &turn,
            })
        };
        let ledger = storage::read(dir).await.unwrap();
        assert_eq!(ledger.parallelism, 0);
        assert_eq!(attempt().await, None);
        crate::parallelism(&root, Some(4)).await.unwrap();
        identity
            .enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(attempt().await, None);
        identity
            .enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        storage::mutate(dir, |ledger| {
            ledger.pause = Some("User pause".to_owned());
            ((), true)
        })
        .await
        .unwrap();
        assert_eq!(attempt().await, None);
        storage::mutate(dir, |ledger| {
            ledger.pause = None;
            ledger.tasks[0].waiting = Some("External review".to_owned());
            ((), true)
        })
        .await
        .unwrap();
        assert_eq!(attempt().await, None);
        storage::mutate(dir, |ledger| {
            ledger.tasks[0].waiting = None;
            ((), true)
        })
        .await
        .unwrap();
        if !child {
            storage::mutate(dir, |ledger| {
                ledger.tasks[0].owner = Some("/root/worker".to_owned());
                ledger.executions.insert(
                    "/root/worker".to_owned(),
                    crate::ledger::Execution {
                        thread_id: "worker".to_owned(),
                        running: true,
                        outcome: None,
                        updated: 0,
                    },
                );
                ((), true)
            })
            .await
            .unwrap();
            assert!(attempt().await.unwrap().contains("wait_agent"));
            storage::mutate(dir, |ledger| {
                ledger.executions.get_mut("/root/worker").unwrap().running = false;
                ((), true)
            })
            .await
            .unwrap();
            assert!(attempt().await.unwrap().contains("review and integration"));
        }
        assert!(attempt().await.is_some());
        assert_eq!(attempt().await.is_some(), !child);
        assert_eq!(
            crate::parallelism(&thread, /*update*/ None).await.unwrap(),
            if child { 1 } else { 4 }
        );
        crate::parallelism(&root, Some(0)).await.unwrap();
        assert_eq!(
            crate::parallelism(&thread, /*update*/ None).await.unwrap(),
            0
        );
        assert_eq!(attempt().await, None);
    }
}
