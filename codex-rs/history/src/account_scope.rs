use crate::ResponseItemEnvelope;
use codex_protocol::auth::AccountScope;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;

impl ResponseItemEnvelope {
    /// Projects history for the account actually sending a request. Unknown ownership is
    /// treated like a different account, including for rollouts created before provenance existed.
    pub fn for_account(&self, account: Option<&AccountScope>) -> Option<ResponseItem> {
        let mut item = self.item.clone();
        if account.is_some()
            && self
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.account_scope.as_ref())
                == account
        {
            return Some(item);
        }
        match &mut item {
            ResponseItem::Reasoning {
                encrypted_content: Some(_),
                ..
            }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction {
                encrypted_content: Some(_),
                ..
            } => return None,
            ResponseItem::Reasoning {
                encrypted_content: None,
                ..
            }
            | ResponseItem::ContextCompaction {
                encrypted_content: None,
                ..
            } => {}
            ResponseItem::FunctionCall {
                encrypted_function_args,
                ..
            } => *encrypted_function_args = None,
            ResponseItem::AgentMessage { content, .. } => {
                content.retain(|item| {
                    !matches!(item, AgentMessageInputContent::EncryptedContent { .. })
                });
                if content.is_empty() {
                    return None;
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    content.retain(|item| {
                        !matches!(item, FunctionCallOutputContentItem::EncryptedContent { .. })
                    });
                }
            }
            ResponseItem::Message { .. }
            | ResponseItem::AdditionalTools { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::Other => {}
        }
        Some(item)
    }
}

#[cfg(test)]
#[path = "account_scope_tests.rs"]
mod tests;
