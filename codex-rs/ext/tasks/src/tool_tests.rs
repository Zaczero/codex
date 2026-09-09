use super::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::AtomicBool;

#[tokio::test]
async fn activation_delivers_current_body_and_every_full_note_without_repeating_on_updates() {
    let home = tempfile::tempdir().unwrap();
    let tool = LedgerTool {
        thread: Arc::new(ThreadTasks {
            thread_id: ThreadId::new(),
            dir: home.path().to_path_buf(),
            parent_dir: None,
            agent_path: None,
            enabled: AtomicBool::new(true),
        }),
    };
    tool.handle_input(json!({"tasks": [{"title": "Publish the index", "body": "Old context"}]}))
        .await
        .map(|output| assert!(output.plan_update().is_some()))
        .unwrap();
    for n in 0..6 {
        tool.handle_input(json!({"tasks": [{"id": "1", "note": format!("Evidence {n}: {} decisive tail {n}", "complete evidence ".repeat(20))}]}))
            .await
            .map(|output| assert_eq!(output.plan_update(), None))
            .unwrap();
    }
    let output = tool
        .handle_input(json!({"tasks": [{
            "id": "1", "status": "active", "body": "Reconciled publication contract",
            "note": "Activation decision"
        }]}))
        .await
        .unwrap()
        .log_output();
    assert!(
        output.contains("Reconciled publication contract"),
        "{output}"
    );
    assert!(output.contains("Activation decision"), "{output}");
    for n in 0..6 {
        assert!(
            output.contains(&format!(
                "Evidence {n}: {} decisive tail {n}",
                "complete evidence ".repeat(20)
            )),
            "{output}"
        );
    }
    assert!(!output.contains("Old context"), "{output}");
    let output = tool.handle_input(json!({"recall": ["1"]})).await.unwrap();
    assert_eq!(output.plan_update(), None);
    let output = output.log_output();
    for n in 0..6 {
        assert!(output.contains(&format!("Evidence {n}")), "{output}");
    }
    assert!(output.contains("Activation decision"), "{output}");
    assert!(
        output.contains("Reconciled publication contract"),
        "{output}"
    );

    tool.handle_input(json!({"pause": "User requested a stop"}))
        .await
        .unwrap();
    tool.handle_input(json!({"recall": ["1"]})).await.unwrap();
    assert_eq!(
        storage::read(home.path()).await.unwrap().pause.as_deref(),
        Some("User requested a stop")
    );
    let feedback = tool
        .handle_input(json!({"tasks": [{"id": "1", "note": "Work resumed"}]}))
        .await
        .unwrap()
        .log_output();
    assert!(
        !feedback.contains("Reconciled publication contract"),
        "{feedback}"
    );
    assert_eq!(storage::read(home.path()).await.unwrap().pause, None);
}

#[tokio::test]
async fn activation_and_parent_recall_page_every_byte_of_oversized_merged_notes() {
    let home = tempfile::tempdir().unwrap();
    let tool = LedgerTool {
        thread: Arc::new(ThreadTasks {
            thread_id: ThreadId::new(),
            dir: home.path().to_path_buf(),
            parent_dir: None,
            agent_path: None,
            enabled: AtomicBool::new(true),
        }),
    };
    let large_note = format!("{} decisive tail", "Evidence 🧭 ".repeat(4_000));
    tool.handle_input(json!({"tasks": [
        {"title": "Reconcile the evidence", "body": "Read the entire record first."},
        {"title": "Merged investigation", "note": large_note},
    ]}))
    .await
    .unwrap();
    let activation = tool
        .handle_input(json!({"tasks": [{
            "id": "1", "status": "active", "merge": ["2"], "note": "Final conclusion",
        }]}))
        .await
        .unwrap()
        .log_output();
    assert!(activation.contains("Evidence 🧭"), "{activation}");
    assert!(
        activation.contains("Read every remaining page before working"),
        "{activation}"
    );
    let child_home = tempfile::tempdir().unwrap();
    let child = LedgerTool {
        thread: Arc::new(ThreadTasks {
            thread_id: ThreadId::new(),
            dir: child_home.path().to_path_buf(),
            parent_dir: Some(home.path().to_path_buf()),
            agent_path: Some("/root/child".to_string()),
            enabled: AtomicBool::new(true),
        }),
    };
    tool.handle_input(json!({"pause": "Waiting on external review"}))
        .await
        .unwrap();
    for (reader, reference) in [(&tool, "1"), (&child, "parent:1")] {
        let mut collected = String::new();
        for page in 1.. {
            let output = reader
                .handle_input(json!({"recall": [reference], "page": page}))
                .await
                .unwrap();
            assert_eq!(output.plan_update(), None);
            assert_eq!(output.fallback_token_limit_override(), Some(8_000));
            let message = output.log_output();
            assert!(message.len() <= crate::ledger::CONTEXT_BYTES);
            let (_, content) = message.split_once(":\n\n").unwrap();
            if let Some((content, _)) = content.split_once("\n\nRead every remaining page") {
                collected.push_str(content);
            } else {
                collected.push_str(content);
                break;
            }
        }
        assert!(collected.contains(&large_note));
        assert!(collected.contains("from 2 Merged investigation"));
        assert!(collected.contains("Final conclusion"));
        assert!(collected.contains("Read the entire record first."));
    }
    assert_eq!(
        storage::read(home.path()).await.unwrap().pause.as_deref(),
        Some("Waiting on external review")
    );
    let stored = storage::read(home.path()).await.unwrap();
    assert_eq!(stored.tasks[0].findings[0].text, large_note);
    let first = tool
        .handle_input(json!({"recall": ["1"]}))
        .await
        .unwrap()
        .log_output();
    let activation_details = activation.split_once("Starting tasks. ").unwrap().1;
    assert_eq!(activation_details, first);
    for input in [
        json!({"page": 2}),
        json!({"recall": ["1"], "page": 0}),
        json!({"recall": ["1"], "page": "2"}),
        json!({"recall": ["1"], "page": 2, "tasks": [{"id": "1", "status": "done"}]}),
    ] {
        tool.handle_input(input).await.unwrap();
        assert_eq!(storage::read(home.path()).await.unwrap(), stored);
    }
    let missing_page = tool
        .handle_input(json!({"recall": ["1"], "page": 1_000_000}))
        .await
        .unwrap()
        .log_output();
    assert!(missing_page.contains("request a page from 1 to"));
}
