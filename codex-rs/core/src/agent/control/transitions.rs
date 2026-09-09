use super::*;
use codex_protocol::protocol::AgentRerouteOutcome;
use tokio::sync::oneshot;

impl AgentControl {
    pub(crate) async fn interrupt_agent_and_wait(&self, agent_id: ThreadId) -> CodexResult<()> {
        let state = self.upgrade()?;
        let (reply, completion) = oneshot::channel();
        self.handle_thread_request_result(
            agent_id,
            &state,
            state
                .send_op(
                    agent_id,
                    Op::InterruptAndWait { reply },
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await,
        )
        .await?;
        completion
            .await
            .map_err(|_| CodexErr::Fatal("agent interruption reply was lost".to_string()))
    }

    pub(crate) async fn reroute_agent(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
        thread_settings: ThreadSettingsOverrides,
    ) -> CodexResult<AgentRerouteOutcome> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        self.ensure_execution_capacity_for_turn_start(&thread)
            .await?;
        let communication_for_log =
            crate::agent_communication::logging_enabled().then(|| communication.clone());
        let parent_turn_id = start_options.parent_turn_id.clone();
        let root_turn_id = start_options.root_turn_id.clone();
        let (reply, completion) = oneshot::channel();
        let submission_id = self
            .handle_thread_request_result(
                agent_id,
                &state,
                state
                    .send_op(
                        agent_id,
                        Op::RerouteAgent {
                            communication,
                            start_options,
                            thread_settings,
                            reply,
                        },
                        parent_turn_id,
                        root_turn_id,
                    )
                    .await,
            )
            .await?;
        let outcome = completion
            .await
            .map_err(|_| CodexErr::Fatal("agent routing reply was lost".to_string()))??;
        if let Some(communication) = communication_for_log {
            crate::agent_communication::emit_agent_communication_send(
                &submission_id,
                &context,
                &communication,
                agent_id,
            );
        }
        Ok(outcome)
    }
}
