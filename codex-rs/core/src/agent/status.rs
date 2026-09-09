use codex_protocol::items::SubAgentRouting;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use serde::Deserialize;
use serde::Serialize;

/// Status and the routing of the execution that produced it, published atomically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentStatusSnapshot {
    pub(crate) status: AgentStatus,
    pub(crate) routing: Option<SubAgentRouting>,
}

impl From<AgentStatus> for AgentStatusSnapshot {
    fn from(status: AgentStatus) -> Self {
        Self {
            status,
            routing: None,
        }
    }
}

/// Derive the next agent status from a single emitted event.
/// Returns `None` when the event does not affect status tracking.
pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
        EventMsg::TurnComplete(ev) => Some(match &ev.error {
            Some(error) => AgentStatus::Errored(error.message.clone()),
            None => AgentStatus::Completed(ev.last_agent_message.clone()),
        }),
        EventMsg::TurnAborted(ev) => match ev.reason {
            codex_protocol::protocol::TurnAbortReason::Interrupted
            | codex_protocol::protocol::TurnAbortReason::BudgetLimited => {
                Some(AgentStatus::Interrupted)
            }
            _ => Some(AgentStatus::Errored(format!("{:?}", ev.reason))),
        },
        EventMsg::Error(ev) => Some(AgentStatus::Errored(ev.message.clone())),
        EventMsg::ShutdownComplete => Some(AgentStatus::Shutdown),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    !matches!(
        status,
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
    )
}
