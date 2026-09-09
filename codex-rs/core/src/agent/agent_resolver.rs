use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;

/// Resolves a single tool-facing agent target to a thread id.
pub(crate) async fn resolve_agent_target(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    register_session_root(session, turn);
    if let Ok(thread_id) = ThreadId::from_string(target) {
        return Ok(thread_id);
    }

    session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, target)
        .await
        .map_err(|err| match err.details() {
            CodexErrorDetails::UnsupportedOperation(message) => {
                FunctionCallError::RespondToModel(message.clone())
            }
            _ => FunctionCallError::RespondToModel(err.to_string()),
        })
}

fn register_session_root(session: &Arc<Session>, turn: &Arc<TurnContext>) {
    session
        .services
        .agent_control
        .register_session_root(session.thread_id, turn.parent_thread_id);
}

pub(crate) fn require_direct_child(
    source: &SessionSource,
    target: &AgentPath,
) -> Result<(), FunctionCallError> {
    let parent = source.get_agent_path().unwrap_or_else(AgentPath::root);
    if target.as_str().rsplit_once('/').map(|(parent, _)| parent) != Some(parent.as_str()) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{target} is not a direct child of {parent}"
        )));
    }
    Ok(())
}
