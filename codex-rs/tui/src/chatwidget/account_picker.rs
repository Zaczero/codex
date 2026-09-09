use super::*;

use codex_app_server_protocol::NamedAccount;

impl ChatWidget {
    pub(crate) fn show_account_picker(&mut self, accounts: Vec<NamedAccount>) {
        let items = accounts
            .into_iter()
            .map(|account| {
                let account_id = account.id.clone();
                let action_account_id = account_id.clone();
                let principal = account
                    .email
                    .clone()
                    .or(account.account_id.clone())
                    .unwrap_or_else(|| "no provider identity".to_string());
                let search_value = format!("{} {account_id} {principal}", account.label);
                SelectionItem {
                    name: account.label,
                    description: Some(format!("{principal} · {}", account.auth_mode)),
                    is_current: account.is_active,
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::SwitchNamedAccount(action_account_id.clone()));
                    })],
                    dismiss_on_select: true,
                    search_value: Some(search_value),
                    ..Default::default()
                }
            })
            .collect();

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Named accounts".to_string()),
            subtitle: Some("Choose the account for subsequent requests".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Filter accounts".to_string()),
            ..Default::default()
        });
        self.request_redraw();
    }
}
