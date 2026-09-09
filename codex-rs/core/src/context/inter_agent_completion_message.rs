use codex_protocol::AgentPath;
use codex_protocol::items::SubAgentRouting;

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterAgentCompletionMessage {
    task_name: AgentPath,
    sender: AgentPath,
    payload: String,
    routing: Option<SubAgentRouting>,
}

impl InterAgentCompletionMessage {
    pub(crate) fn new(
        task_name: AgentPath,
        sender: AgentPath,
        payload: impl Into<String>,
        routing: Option<SubAgentRouting>,
    ) -> Self {
        Self {
            task_name,
            sender,
            payload: payload.into(),
            routing,
        }
    }
}

impl ContextualUserFragment for InterAgentCompletionMessage {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.inter_agent_completion_message".to_string())
    }

    fn role(&self) -> &'static str {
        "assistant"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        let routing = match &self.routing {
            Some(routing) => format!(
                "Model: {}\nReasoning effort: {}\n",
                routing.model,
                routing
                    .reasoning_effort
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unspecified".to_string())
            ),
            None => "Model and reasoning effort: unavailable\n".to_string(),
        };
        format!(
            "Message Type: FINAL_ANSWER\nTask name: {}\nSender: {}\n{routing}Payload:\n{}",
            self.task_name, self.sender, self.payload,
        )
    }
}
