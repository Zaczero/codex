//! Wires the ledger into the thread: the tool, the World State section, and the execution
//! records that let completion checks tell running delegated work from abandoned work.

use std::path::PathBuf;
use std::sync::Arc;

use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadIdleCause;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

use crate::ledger::Execution;
use crate::ledger::render;
use crate::storage;
use crate::tool::LedgerTool;
use crate::world_state;

/// What the host config says about the ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TasksExtensionConfig {
    pub enabled: bool,
    pub codex_home: PathBuf,
}

impl Default for TasksExtensionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            codex_home: PathBuf::new(),
        }
    }
}

/// Per-thread ledger identity, settled at thread start.
#[derive(Debug)]
pub(crate) struct ThreadTasks {
    pub(crate) thread_id: ThreadId,
    pub(crate) dir: PathBuf,
    /// Present for a spawned agent: where its parent keeps the ledger this worker is recorded in.
    pub(crate) parent_dir: Option<PathBuf>,
    pub(crate) agent_path: Option<String>,
    pub(crate) enabled: std::sync::atomic::AtomicBool,
}

impl ThreadTasks {
    fn is_child(&self) -> bool {
        self.parent_dir.is_some()
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn record_execution(&self, running: bool, outcome: Option<&str>) {
        let (Some(parent_dir), Some(agent_path)) = (&self.parent_dir, &self.agent_path) else {
            return;
        };
        let thread_id = self.thread_id.to_string();
        let outcome = outcome.map(str::to_string);
        let owner = agent_path.clone();
        let result = storage::mutate(parent_dir, move |ledger| {
            let now = storage::now_ms();
            let execution = Execution {
                thread_id,
                running,
                outcome,
                updated: now,
            };
            let unchanged = ledger.executions.get(&owner).is_some_and(|current| {
                current.thread_id == execution.thread_id
                    && current.running == execution.running
                    && current.outcome == execution.outcome
            });
            if unchanged {
                return ((), false);
            }
            ledger.executions.insert(owner, execution);
            ((), true)
        })
        .await;
        if let Err(err) = result {
            tracing::warn!("failed to record execution state for {agent_path}: {err}");
        }
    }
}

pub struct TasksExtension<C> {
    resolve: Arc<dyn Fn(&C) -> TasksExtensionConfig + Send + Sync>,
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod completion_tests;
#[cfg(test)]
#[path = "control_tests.rs"]
mod control_tests;

impl<C> std::fmt::Debug for TasksExtension<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TasksExtension").finish_non_exhaustive()
    }
}

pub(crate) fn thread_tasks(thread_store: &ExtensionData) -> Option<Arc<ThreadTasks>> {
    thread_store.get::<ThreadTasks>()
}

impl<C> ThreadLifecycleContributor<C> for TasksExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let config = (self.resolve)(input.config);
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            let (parent_dir, agent_path) = match input.session_source {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    agent_path,
                    ..
                }) => (
                    Some(storage::ledger_dir(
                        &config.codex_home,
                        &parent_thread_id.to_string(),
                    )),
                    agent_path.as_ref().map(ToString::to_string),
                ),
                // Review, compaction, and other internal sessions keep no ledger.
                SessionSource::SubAgent(_) | SessionSource::Internal(_) => return,
                _ => (None, None),
            };
            let thread = input
                .thread_store
                .get_or_init::<ThreadTasks>(|| ThreadTasks {
                    thread_id,
                    dir: storage::ledger_dir(&config.codex_home, &thread_id.to_string()),
                    parent_dir,
                    agent_path,
                    enabled: std::sync::atomic::AtomicBool::new(config.enabled),
                });
            if thread.enabled() {
                if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    ..
                }) = input.session_source
                {
                    let parent_dir =
                        storage::ledger_dir(&config.codex_home, &parent_thread_id.to_string());
                    let capacity_root = match storage::read(&parent_dir).await {
                        Ok(parent) => Some(parent.capacity_root.unwrap_or(*parent_thread_id)),
                        Err(error) => {
                            tracing::warn!("cannot initialize child task capacity: {error}");
                            return;
                        }
                    };
                    if let Err(error) = storage::mutate(&thread.dir, move |ledger| {
                        let changed = ledger.capacity_root != capacity_root;
                        ledger.capacity_root = capacity_root;
                        ((), changed)
                    })
                    .await
                    {
                        tracing::warn!("cannot persist child task capacity: {error}");
                    }
                }
                thread.record_execution(/*running*/ true, None).await;
            }
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(thread) = thread_tasks(input.thread_store) else {
                return;
            };
            if !thread.enabled() {
                return;
            }
            let outcome = match input.cause {
                ThreadIdleCause::Completed => "completed",
                ThreadIdleCause::Interrupted => "interrupted",
                ThreadIdleCause::Failed => "failed",
            };
            thread
                .record_execution(/*running*/ false, Some(outcome))
                .await;
        })
    }
}

impl<C> TurnLifecycleContributor for TasksExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_completion_attempt<'a>(
        &'a self,
        input: TurnStopInput<'a>,
    ) -> ExtensionFuture<'a, Option<String>> {
        Box::pin(async move {
            let thread = thread_tasks(input.thread_store)?;
            if !thread.enabled() {
                return None;
            }
            let ledger = match storage::read(&thread.dir).await {
                Ok(ledger) => ledger,
                Err(error) => {
                    tracing::warn!("cannot check unfinished tasks: {error}");
                    return None;
                }
            };
            if ledger.parallelism == 0 {
                return None;
            }
            let reason = crate::completion::reason(&ledger, thread.is_child())?;
            if thread.is_child() {
                let correction = input
                    .turn_store
                    .get_or_init::<crate::completion::ChildCorrection>(Default::default);
                if correction
                    .0
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    return None;
                }
            }
            Some(reason)
        })
    }

    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(thread) = thread_tasks(input.thread_store) else {
                return;
            };
            if !thread.enabled() {
                return;
            }
            thread.record_execution(/*running*/ true, None).await;
        })
    }
}

impl<C> ConfigContributor<C> for TasksExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        _previous_config: &C,
        new_config: &C,
    ) {
        let config = (self.resolve)(new_config);
        if let Some(thread) = thread_tasks(thread_store) {
            thread
                .enabled
                .store(config.enabled, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl<C> ContextContributor for TasksExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let Some(thread) = thread_tasks(input.thread_store) else {
                return Vec::new();
            };
            if !thread.enabled() {
                return Vec::new();
            }
            let ledger = match storage::read(&thread.dir).await {
                Ok(ledger) => ledger,
                Err(err) => {
                    tracing::warn!(
                        "failed to read the task ledger for {}: {err}",
                        thread.thread_id
                    );
                    return Vec::new();
                }
            };
            if ledger.tasks.is_empty()
                && ledger.pause.is_none()
                && !tokio::fs::try_exists(thread.dir.join(storage::LEDGER_FILE))
                    .await
                    .unwrap_or(false)
            {
                return Vec::new();
            }
            vec![world_state::section(format!(
                "Ledger file: {}\n{}",
                thread.dir.join(storage::LEDGER_FILE).display(),
                render(&ledger)
            ))]
        })
    }
}

impl<C> ToolContributor for TasksExtension<C>
where
    C: Send + Sync + 'static,
{
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(thread) = thread_tasks(thread_store) else {
            return Vec::new();
        };
        if !thread.enabled() {
            return Vec::new();
        }
        vec![Arc::new(LedgerTool { thread })]
    }
}

pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    resolve: impl Fn(&C) -> TasksExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(TasksExtension {
        resolve: Arc::new(resolve),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.prompt_contributor(extension.clone());
    registry.tool_contributor(extension);
}
