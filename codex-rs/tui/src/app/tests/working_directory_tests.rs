use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cd_keeps_empty_session_identity_ledger_and_transcript() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let destination = tempdir()?;
    let home = tempdir()?;
    std::fs::create_dir(destination.path().join(".git"))?;
    std::fs::create_dir(destination.path().join(".codex"))?;
    std::fs::write(
        destination.path().join(".codex/config.toml"),
        "approval_policy = 'on-request'\nsandbox_mode = 'read-only'\n",
    )?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "approval_policy = 'never'\nsandbox_mode = 'danger-full-access'\n[projects.{}]\ntrust_level = 'trusted'\n",
            serde_json::to_string(destination.path())?
        ),
    )?;
    app.config = ConfigBuilder::default()
        .codex_home(home.path().into())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let started = server.start_thread(&app.config).await?;
    let id = started.session.thread_id;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.replace_chat_widget_with_app_server_thread(
        &mut tui,
        started,
        super::super::session_lifecycle::ThreadAttachPresentation::SessionLineage,
        /*initial_user_message*/ None,
    )
    .await?;
    let ledger_dir = home.path().join("tasks").join(id.to_string());
    std::fs::create_dir_all(&ledger_dir)?;
    let ledger = serde_json::json!({"parallelism": 8, "pause": "Waiting on the user", "tasks": [
        {"id": "7", "title": "Retained outcome", "status": "active", "body": "Current state",
         "created": 1000, "updated": 1001, "findings": [{"at": 1001, "text": "Retained evidence"}]}
    ]});
    std::fs::write(ledger_dir.join("ledger.json"), serde_json::to_vec(&ledger)?)?;
    let cell: Arc<dyn HistoryCell> = Arc::new(history_cell::new_info_event(
        "Retained transcript".into(),
        /*hint*/ None,
    ));
    app.transcript_cells.push(cell.clone());
    while events.try_recv().is_ok() {}
    app.change_working_directory(&mut tui, &mut server, destination.path().abs())
        .await;
    assert_eq!(app.chat_widget.thread_id(), Some(id));
    assert_eq!(app.config.cwd, destination.path().abs());
    assert_eq!(app.chat_widget.config_ref().cwd, destination.path().abs());
    assert_eq!(
        app.config.permissions.approval_policy.value(),
        codex_protocol::protocol::AskForApproval::Never
    );
    assert!(
        app.transcript_cells
            .iter()
            .any(|item| Arc::ptr_eq(item, &cell))
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(
            ledger_dir.join("ledger.json")
        )?)?,
        ledger
    );
    assert_eq!(
        server
            .thread_task_parallelism(codex_app_server_protocol::ThreadTaskParallelismParams {
                thread_id: id.to_string(),
                parallelism: None,
            })
            .await?
            .parallelism,
        8
    );
    let messages = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 120)))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("../../snapshots");
    let _guard = settings.bind_to_scope();
    insta::assert_snapshot!(
        "same_session_directory_change",
        messages.replace(&destination.path().display().to_string(), "<destination>")
    );
    server.shutdown().await?;
    Ok(())
}
