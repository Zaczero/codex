use super::*;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[tokio::test]
async fn task_parallelism_roundtrip_and_unfinished_tasks_pager() -> Result<()> {
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("../../snapshots");
    let _guard = settings.bind_to_scope();
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let started = server.start_thread(&app.config).await?;
    let thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    assert_eq!(
        server.thread_tasks_read(thread_id).await?.text,
        "**Parallelism:** off\n\nNo unfinished tasks."
    );
    while events.try_recv().is_ok() {}
    for parallelism in [None, Some(3), None, Some(0)] {
        app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::TaskParallelism {
                thread_id,
                parallelism,
            },
        )
        .await?;
    }
    let feedback = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 100)))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("task_parallelism_feedback", feedback);

    let dir = app
        .config
        .codex_home
        .join("tasks")
        .join(thread_id.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    let mut tasks = (1..=12).map(|id| serde_json::json!({
        "id": id.to_string(), "title": format!("Outcome {id}"),
        "status": if id == 1 { "active" } else { "pending" },
        "body": if id == 1 { "Keep the **public API** coherent across requests and authenticated retries.\n\n- Update the integration contract.\n- Inspect `request.account_id` before sending.\n\n```rust\nlet account = request.account_id();\n```" } else { "" },
        "after": if id == 12 { vec!["1"] } else { Vec::<&str>::new() },
        "owner": if id == 1 { Some("/root/worker") } else { None },
        "findings": if id == 1 { (1..=6).map(|n| serde_json::json!({
            "at": 1000,
            "text": if n == 1 { "A **routing change** must retain the producer identity.\n\n- Preserve plaintext.\n- Check [the contract](https://example.com/contract).".to_string() } else { format!("Finding {n}: request credentials remain account-bound.") },
            "from": if n == 1 { Some(serde_json::json!({"task": "9", "title": "Audit *old* records"})) } else { None },
        })).collect::<Vec<_>>() } else { vec![] },
        "created": 1000, "updated": 1000
    })).collect::<Vec<_>>();
    tasks.extend([
        serde_json::json!({"id":"13", "title":"Old completed work", "status":"done", "created":1000, "updated":1000}),
        serde_json::json!({"id":"14", "title":"Old cancelled work", "status":"pending", "cancelled":"Abandoned", "created":1000, "updated":1000}),
    ]);
    tokio::fs::write(
        dir.join("ledger.json"),
        serde_json::to_vec(&serde_json::json!({"tasks": tasks, "parallelism": 0}))?,
    )
    .await?;
    app.handle_event(&mut tui, &mut server, AppEvent::OpenTasks { thread_id })
        .await?;
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 86, /*height*/ 25,
    );
    let mut buffer = Buffer::empty(area);
    let Some(Overlay::Static(overlay)) = app.overlay.as_mut() else {
        panic!("expected task pager")
    };
    overlay.render(area, &mut buffer);
    insta::assert_snapshot!("unfinished_tasks_pager_top", format!("{buffer:?}"));
    for (name, width) in [
        ("unfinished_tasks_pager_narrow", 48),
        ("unfinished_tasks_pager_wide", 100),
    ] {
        let resized = Rect::new(/*x*/ 0, /*y*/ 0, width, /*height*/ 44);
        let mut resized_buffer = Buffer::empty(resized);
        overlay.render(resized, &mut resized_buffer);
        assert!(
            resized_buffer
                .content
                .iter()
                .any(|cell| cell.symbol().contains("https://example.com/contract\u{7}"))
        );
        insta::assert_snapshot!(
            name,
            crate::terminal_hyperlinks::strip_osc8(&format!("{resized_buffer:?}"))
        );
    }
    let mut restored = Buffer::empty(area);
    overlay.render(area, &mut restored);
    assert_eq!(restored, buffer);
    overlay.handle_event(
        &mut tui,
        TuiEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
    )?;
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    insta::assert_snapshot!("unfinished_tasks_pager_bottom", format!("{buffer:?}"));
    let full = server.thread_tasks_read(thread_id).await?.text;
    assert!(full.contains("#12 Outcome 12"));
    assert!(full.contains("Finding 6: request credentials remain account-bound."));
    assert!(!full.contains("Old completed work"));
    assert!(!full.contains("Old cancelled work"));
    app.overlay.as_mut().unwrap().handle_event(
        &mut tui,
        TuiEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
    )?;
    assert_eq!(app.overlay.as_ref().map(Overlay::is_done), Some(true));
    server.shutdown().await?;
    Ok(())
}
