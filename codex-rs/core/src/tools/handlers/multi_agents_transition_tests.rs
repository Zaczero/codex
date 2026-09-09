use super::*;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct HeldTask {
    pub(super) release: Arc<Notify>,
    pub(super) cleaned: Arc<AtomicBool>,
    cleanup_started: Arc<Notify>,
    hold_after_cancel: bool,
}

impl SessionTask for HeldTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.held_cleanup"
    }
    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation: CancellationToken,
    ) -> SessionTaskResult {
        cancellation.cancelled().await;
        if self.hold_after_cancel {
            std::future::pending::<()>().await;
        }
        Ok(None)
    }
    async fn abort(&self, _session: Arc<Session>, _ctx: Arc<TurnContext>) {
        self.cleanup_started.notify_one();
        self.release.notified().await;
        self.cleaned.store(true, Ordering::SeqCst);
    }
}

#[test_case::test_case(false; "running")]
#[test_case::test_case(true; "already_cancelled")]
#[tokio::test]
async fn interrupt_acknowledges_cleanup_not_the_early_interrupted_status(already_cancelled: bool) {
    let (manager, session, turn, agent_id) = spawn_v2_worker_on_gpt54().await;
    let thread = manager.get_thread(agent_id).await.unwrap();
    let task = HeldTask {
        hold_after_cancel: already_cancelled,
        ..Default::default()
    };
    let cleanup_started = task.cleanup_started.clone();
    let release = task.release.clone();
    let cleaned = task.cleaned.clone();
    thread
        .session
        .start_task(thread.session.new_default_turn().await, Vec::new(), task)
        .await;
    if already_cancelled {
        thread
            .session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .task
            .as_ref()
            .unwrap()
            .cancellation_token
            .cancel();
    }
    let handler = InterruptAgentHandler;
    let mut interrupt = std::pin::pin!(handler.handle(invocation(
        session,
        turn,
        "interrupt_agent",
        function_payload(json!({"target": agent_id.to_string()}))
    )));
    tokio::select! {
        result = &mut interrupt => panic!("interrupt returned before teardown finished: {:?}", result.err()),
        _ = cleanup_started.notified() => {}
    }
    assert_eq!(thread.agent_status().await, AgentStatus::Interrupted);
    assert!(!cleaned.load(Ordering::SeqCst));
    release.notify_one();
    let (text, _) = expect_text_output(interrupt.await.unwrap());
    let output: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(output["settled"], true);
    assert!(cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn sibling_cannot_interrupt_or_reroute_another_worker_by_uuid() {
    let (manager, session, _turn, agent_id) = spawn_v2_worker_on_gpt54().await;
    let (_, mut child_turn) = make_session_and_context().await;
    child_turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: 1,
        agent_path: Some(AgentPath::from_string("/root/sibling".to_string()).unwrap()),
        agent_nickname: None,
        agent_role: None,
    });
    let child_turn = Arc::new(child_turn);
    let error = InterruptAgentHandler
        .handle(invocation(
            session.clone(),
            child_turn.clone(),
            "interrupt_agent",
            function_payload(json!({"target": agent_id.to_string()})),
        ))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("not a direct child"), "{error}");
    let error = FollowupTaskHandlerV2::default().handle(invocation(session, child_turn, "followup_task", function_payload(json!({"target": agent_id.to_string(), "message": "change work", "reasoning_effort": "high"})))).await.err().unwrap();
    assert!(error.to_string().contains("not a direct child"), "{error}");
    assert_eq!(worker_op_kinds(&manager, agent_id), Vec::<&str>::new());
}
