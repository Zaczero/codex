use super::*;
use codex_app_server_protocol::NamedAccount;

#[tokio::test]
async fn named_account_picker_filters_by_label_and_selects_stable_id() {
    let (mut chat, mut events, _ops) = make_chatwidget_manual(None).await;
    chat.show_account_picker(vec![
        NamedAccount {
            id: "local-first".to_string(),
            label: "Personal".to_string(),
            is_active: true,
            account_id: None,
            email: Some("personal@example.com".to_string()),
            auth_mode: codex_app_server_protocol::AuthMode::ApiKey,
            plan_type: None,
        },
        NamedAccount {
            id: "local-second".to_string(),
            label: "Travel".to_string(),
            is_active: false,
            account_id: None,
            email: Some("other@example.com".to_string()),
            auth_mode: codex_app_server_protocol::AuthMode::ApiKey,
            plan_type: None,
        },
    ]);
    assert_chatwidget_snapshot!(
        "named_account_picker",
        render_bottom_popup(&chat, /*width*/ 90)
    );
    for ch in "Travel".chars() {
        chat.handle_key_event(KeyEvent::from(KeyCode::Char(ch)));
    }
    assert_chatwidget_snapshot!(
        "named_account_picker_filtered",
        render_bottom_popup(&chat, /*width*/ 90)
    );
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    let selected = std::iter::from_fn(|| events.try_recv().ok()).find_map(|event| match event {
        AppEvent::SwitchNamedAccount(id) => Some(id),
        _ => None,
    });
    assert_eq!(selected, Some("local-second".to_string()));
}
