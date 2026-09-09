use super::session::Session;
use super::turn_context::TurnContext;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::auth::AccountScope;
use codex_protocol::models::ResponseItem;

impl Session {
    pub(crate) async fn record_account_response_item(
        &self,
        turn_context: &TurnContext,
        item: &ResponseItem,
        account_scope: Option<&AccountScope>,
    ) {
        let (items, images) =
            self.prepare_conversation_items_for_history(turn_context, std::slice::from_ref(item));
        let items = items
            .into_owned()
            .into_iter()
            .map(|item| ResponseItemEnvelope {
                item,
                metadata: Some(CodexHarnessMetadata {
                    account_scope: account_scope.cloned(),
                    ..Default::default()
                }),
            })
            .collect();
        self.record_prepared_conversation_items(turn_context, items, images)
            .await;
    }
}
