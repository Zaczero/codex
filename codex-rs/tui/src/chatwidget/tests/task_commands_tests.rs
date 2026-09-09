use super::*;

#[tokio::test]
async fn task_commands_dispatch_while_running_and_reject_invalid_capacity() {
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("../../snapshots");
    let _guard = settings.bind_to_scope();
    let (mut chat, mut events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.bottom_pane.set_task_running(/*running*/ true);
    while events.try_recv().is_ok() {}
    chat.dispatch_command(SlashCommand::Tasks);
    assert_matches!(events.try_recv().unwrap(), AppEvent::OpenTasks { thread_id: id } if id == thread_id);
    chat.dispatch_command(SlashCommand::Parallelism);
    assert_matches!(events.try_recv().unwrap(), AppEvent::TaskParallelism { thread_id: id, parallelism: None } if id == thread_id);
    for args in ["4", "0"] {
        chat.dispatch_command_with_args(SlashCommand::Parallelism, args.to_string(), Vec::new());
        assert_matches!(events.try_recv().unwrap(), AppEvent::TaskParallelism { thread_id: id, parallelism: Some(value) } if id == thread_id && value.to_string() == args);
    }
    for args in ["-1", "1.5", "4 extra", "999999999999999999999999999999"] {
        chat.dispatch_command_with_args(SlashCommand::Parallelism, args.to_string(), Vec::new());
    }
    let errors = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.display_lines(/*width*/ 100)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("invalid_task_parallelism", errors);
}
