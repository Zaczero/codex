use super::*;
use codex_app_server_protocol::SubAgentActivityKind;
use codex_app_server_protocol::SubAgentRouting;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn agent_picker_and_history_keep_routing_for_each_interaction() {
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("../../snapshots");
    let _guard = settings.bind_to_scope();
    let mut app = Box::pin(make_test_app()).await;
    let thread_id = ThreadId::from_string("01940000-0000-7000-8000-000000000001").unwrap();
    let mut history = Vec::new();
    for (index, (kind, model, effort)) in [
        (
            SubAgentActivityKind::Started,
            "gpt-5.6-luna",
            ReasoningEffort::XHigh,
        ),
        (
            SubAgentActivityKind::Interacted,
            "gpt-5.6-luna",
            ReasoningEffort::XHigh,
        ),
        (
            SubAgentActivityKind::Interacted,
            "gpt-5.6-sol",
            ReasoningEffort::High,
        ),
        (
            SubAgentActivityKind::Completed,
            "gpt-5.6-sol",
            ReasoningEffort::High,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let routing = SubAgentRouting {
            model: model.to_string(),
            reasoning_effort: Some(effort),
        };
        let item = ThreadItem::SubAgentActivity {
            id: format!("activity-{index}"),
            kind,
            agent_thread_id: thread_id.to_string(),
            agent_path: "/root/reviewer".to_string(),
            routing: Some(routing.clone()),
        };
        let notification = ServerNotification::ItemCompleted(
            codex_app_server_protocol::ItemCompletedNotification {
                thread_id: ThreadId::new().to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: index as i64,
                item: item.clone(),
            },
        );
        app.cache_collab_receiver_threads_for_notification(&notification);
        assert_eq!(
            app.agent_navigation
                .get(&thread_id)
                .unwrap()
                .routing
                .as_ref(),
            Some(&routing)
        );
        history.push(item);
    }
    let params = app.agent_picker_selection_view_params(/*selected*/ None);
    app.chat_widget.show_selection_view(params);
    insta::assert_snapshot!(
        "agent_picker_with_routing",
        render_bottom_popup(&app.chat_widget, /*width*/ 100)
    );
    let history = history
        .iter()
        .flat_map(|item| {
            crate::multi_agents::sub_agent_activity_history_cell(item)
                .unwrap()
                .display_lines(/*width*/ 100)
        })
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("subagent_routing_history", history);
}
