//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::ThreadId;
use codex_protocol::items::SubAgentRouting;
use codex_protocol::protocol::ThreadSettingsOverrides;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageDeliveryMode {
    QueueOnly,
    TriggerTurn,
}

impl MessageDeliveryMode {
    fn trigger_turn(self) -> bool {
        match self {
            Self::QueueOnly => false,
            Self::TriggerTurn => true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

/// Routing requested for the child's next turn; both fields omitted means "keep".
#[derive(Debug, Default, Clone)]
pub(crate) struct RequestedRouting {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

impl RequestedRouting {
    fn is_empty(&self) -> bool {
        self.model.is_none() && self.reasoning_effort.is_none()
    }
}

/// Routing at message delivery, including an interruption requested by a follow-up.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct AgentMessageResult {
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// The child's active turn was interrupted so the new routing could take effect.
    pub(crate) interrupted: bool,
}

impl ToolOutput for AgentMessageResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "agent message")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "agent message")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "agent message")
    }
}

pub(super) fn message_content(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Handles the shared MultiAgentV2 message flow for both `send_message` and `followup_task`.
pub(super) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    requested_routing: RequestedRouting,
    analytics: &mut ToolCallAnalytics,
) -> Result<AgentMessageResult, FunctionCallError> {
    let message = message_content(message)?;
    let account_scope = invocation.originating_account_scope().await;
    let ToolInvocation {
        session,
        turn,
        call_id,
        source,
        ..
    } = invocation;
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    analytics.set_receiver(receiver_thread_id);
    let receiver_agent = session
        .services
        .agent_control
        .ensure_agent_known(receiver_thread_id)
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    if mode == MessageDeliveryMode::TriggerTurn
        && receiver_agent
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root)
    {
        return Err(FunctionCallError::RespondToModel(
            "Follow-up tasks can't target the root agent".to_string(),
        ));
    }
    let receiver_agent_path = receiver_agent.agent_path.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    })?;
    if mode == MessageDeliveryMode::TriggerTurn && !requested_routing.is_empty() {
        crate::agent::agent_resolver::require_direct_child(
            &turn.session_source,
            &receiver_agent_path,
        )?;
    }
    let resume_config = build_agent_resume_config(turn.as_ref())?;
    session
        .services
        .agent_control
        .ensure_v2_agent_loaded(
            resume_config.clone(),
            receiver_thread_id,
            /*parent*/ None,
        )
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    let mut routing = reroute_agent(
        &session,
        &turn,
        &resume_config,
        receiver_thread_id,
        requested_routing,
    )
    .await?;
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let mut communication = communication_from_tool_message(
        author,
        receiver_agent_path.clone(),
        message,
        &source,
        mode.trigger_turn(),
    );
    communication.account_scope = account_scope;
    let kind = match mode {
        MessageDeliveryMode::QueueOnly => AgentCommunicationKind::Message,
        MessageDeliveryMode::TriggerTurn => AgentCommunicationKind::Followup,
    };
    let context = AgentCommunicationContext::new(kind, session.thread_id);
    let parent_turn_id =
        matches!(mode, MessageDeliveryMode::TriggerTurn).then(|| turn.sub_id.clone());
    let start_options = crate::TurnStartOptions {
        parent_turn_id,
        root_turn_id: turn.turn_metadata_state.root_turn_id(),
        cyber_access_program: turn.cyber_access_program,
        ..Default::default()
    };
    if routing.changed {
        let outcome = session
            .services
            .agent_control
            .reroute_agent(
                receiver_thread_id,
                communication,
                context,
                start_options,
                ThreadSettingsOverrides {
                    model: Some(routing.model.clone()),
                    effort: Some(routing.reasoning_effort.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
        routing.model = outcome.model;
        routing.reasoning_effort = outcome.reasoning_effort;
        routing.interrupted = outcome.interrupted;
    } else {
        session
            .services
            .agent_control
            .send_inter_agent_communication(
                receiver_thread_id,
                communication,
                context,
                start_options,
            )
            .await
            .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    }
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            id: call_id,
            agent_thread_id: receiver_thread_id,
            agent_path: receiver_agent_path,
            kind: SubAgentActivityKind::Interacted,
            routing: Some(SubAgentRouting {
                model: routing.model.clone(),
                reasoning_effort: routing.reasoning_effort.clone(),
            }),
        },
    )
    .await;

    Ok(AgentMessageResult {
        model: routing.model,
        reasoning_effort: routing.reasoning_effort,
        interrupted: routing.interrupted,
    })
}

struct AppliedRouting {
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
    changed: bool,
    interrupted: bool,
}

/// Resolves requested routing without mutating or interrupting the child.
async fn reroute_agent(
    session: &Session,
    turn: &TurnContext,
    config: &Config,
    receiver_thread_id: ThreadId,
    requested: RequestedRouting,
) -> Result<AppliedRouting, FunctionCallError> {
    let agent_control = &session.services.agent_control;
    let snapshot = agent_control
        .get_agent_config_snapshot(receiver_thread_id)
        .await
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "agent with id {receiver_thread_id} not found"
            ))
        })?;
    if requested.is_empty() {
        return Ok(AppliedRouting {
            model: snapshot.model,
            reasoning_effort: snapshot.reasoning_effort,
            changed: false,
            interrupted: false,
        });
    }
    let resolved = resolve_agent_routing(
        session,
        turn,
        config,
        &snapshot.model,
        snapshot.reasoning_effort.as_ref(),
        requested.model.as_deref(),
        requested.reasoning_effort,
    )
    .await?;
    let changed =
        resolved.model != snapshot.model || resolved.reasoning_effort != snapshot.reasoning_effort;
    if !changed {
        return Ok(AppliedRouting {
            model: resolved.model,
            reasoning_effort: resolved.reasoning_effort,
            changed: false,
            interrupted: false,
        });
    }
    Ok(AppliedRouting {
        model: resolved.model,
        reasoning_effort: resolved.reasoning_effort,
        changed: true,
        interrupted: false,
    })
}
