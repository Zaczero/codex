use super::handlers;
use super::session::Session;
use super::thread_settings;
use super::turn_context::TurnContext;
use crate::codex_thread::ThreadConfigSnapshot;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::SubAgentRouting;
use codex_protocol::protocol::AgentRerouteOutcome;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnStartOptions;
use std::sync::Arc;

impl From<&TurnContext> for SubAgentRouting {
    fn from(turn: &TurnContext) -> Self {
        Self {
            model: turn.model_info().slug.clone(),
            reasoning_effort: turn.effective_reasoning_effort(),
        }
    }
}

impl From<&ThreadConfigSnapshot> for SubAgentRouting {
    fn from(snapshot: &ThreadConfigSnapshot) -> Self {
        Self {
            model: snapshot.model.clone(),
            reasoning_effort: snapshot.reasoning_effort.clone(),
        }
    }
}

pub(super) async fn reroute(
    session: &Arc<Session>,
    submission_id: String,
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    overrides: ThreadSettingsOverrides,
) -> CodexResult<AgentRerouteOutcome> {
    let updates = thread_settings::prepare_update(overrides);
    session
        .preview_settings(&updates)
        .await
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    let interrupted = session
        .abort_tasks_for_transition(TurnAbortReason::Interrupted)
        .await;
    thread_settings::apply_update(session, submission_id.clone(), updates)
        .await
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    handlers::inter_agent_communication(session, submission_id, communication, start_options).await;
    let snapshot = session.thread_config_snapshot().await;
    Ok(AgentRerouteOutcome {
        model: snapshot.model,
        reasoning_effort: snapshot.reasoning_effort,
        interrupted,
    })
}
