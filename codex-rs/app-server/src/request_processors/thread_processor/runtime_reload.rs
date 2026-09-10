use super::*;
use codex_app_server_protocol::ThreadCwdSetParams;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_protocol::models::PermissionProfileSnapshot;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelections;

impl ThreadRequestProcessor {
    pub(crate) async fn thread_cwd_set(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadCwdSetParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let cwd = AbsolutePathBuf::from_absolute_path_checked(&params.cwd)
            .map_err(|err| invalid_params(format!("invalid working directory: {err}")))?;
        if !cwd.as_path().is_dir() {
            return Err(invalid_params(format!(
                "not a directory: {}",
                cwd.display()
            )));
        }
        let (thread_id, thread) = self.load_thread(&params.thread_id).await?;
        ensure_direct_input_allowed(thread.as_ref()).await?;
        let snapshot = thread.config_snapshot().await;
        let [environment] = snapshot.environment_selections() else {
            return Err(invalid_request(
                "directory changes require one local environment",
            ));
        };
        if snapshot.ephemeral
            || snapshot.parent_thread_id.is_some()
            || environment.environment_id != LOCAL_ENVIRONMENT_ID
            || environment.config != EnvironmentConfigState::FromThread
        {
            return Err(invalid_request(
                "directory changes require a persistent local root thread",
            ));
        }
        let previous_cwd = environment
            .cwd
            .to_abs_path()
            .map_err(|err| invalid_request(err.to_string()))?;
        let original = ThreadRuntimeSnapshot {
            config: thread.config().await.as_ref().clone(),
            settings: thread.restorable_thread_settings().await,
            client_mcp_extensions: thread.client_mcp_extensions(),
        };
        let mut retargeted = original.config.clone();
        retargeted.cwd = cwd.clone();
        // Config loading may call back into a host. Hold the lifecycle permit only after loading.
        let mut config = self
            .config_manager
            .load_latest_config_for_thread(&retargeted)
            .await
            .map_err(|err| config_load_error(&err))?;
        config.developer_instructions = params
            .developer_instructions
            .or(config.developer_instructions);
        config.bypass_hook_trust = original.config.bypass_hook_trust;
        if config.active_project.trust_level.is_none() {
            return Err(invalid_request(
                "directory is not trusted; start Codex there first",
            ));
        }
        let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
        let (_, current) = self.load_thread(&params.thread_id).await?;
        if !Arc::ptr_eq(&thread, &current)
            || current.config_snapshot().await.cwd() != snapshot.cwd()
        {
            return Err(invalid_request(
                "thread changed while loading the working directory; retry",
            ));
        }
        for id in self
            .thread_manager
            .list_agent_subtree_thread_ids(thread_id)
            .await
            .map_err(|err| internal_error(err.to_string()))?
        {
            if let Ok(agent) = self.thread_manager.get_thread(id).await
                && (matches!(agent.agent_status().await, AgentStatus::Running)
                    || !agent.list_background_terminals().await.is_empty())
            {
                return Err(invalid_request(
                    "wait for the thread, its agents, and background terminals to finish before changing directories",
                ));
            }
        }
        let roots = path_utils::replace_path_and_deduplicate(
            snapshot.workspace_roots.clone(),
            previous_cwd.as_path(),
            cwd.clone(),
        );
        let environments = TurnEnvironmentSelections::new(
            cwd.clone(),
            self.thread_manager
                .default_environment_selections(&cwd, &roots),
        );
        let destination = thread
            .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                environments: Some(environments.clone()),
                runtime_workspace_roots: Some(roots.clone()),
                ..Default::default()
            })
            .await
            .map_err(|err| invalid_request(err.to_string()))?;
        config
            .permissions
            .approval_policy
            .set(destination.approval_policy)
            .map_err(|err| invalid_request(err.to_string()))?;
        let permission_snapshot = match destination.active_permission_profile.clone() {
            Some(active) => PermissionProfileSnapshot::active_with_profile_workspace_roots(
                destination.permission_profile.clone(),
                active,
                destination.profile_workspace_roots.clone(),
            ),
            None => PermissionProfileSnapshot::legacy(destination.permission_profile.clone()),
        };
        config
            .permissions
            .set_permission_profile_from_session_snapshot(permission_snapshot)
            .map_err(|err| invalid_request(err.to_string()))?;
        if let Some(active) = &destination.active_permission_profile {
            config.permissions.network = config
                .network_proxy_spec_for_active_permission_profile(
                    active,
                    &destination.permission_profile,
                )
                .map_err(|err| invalid_request(err.to_string()))?;
        }
        config
            .config_layer_stack
            .requirements()
            .approvals_reviewer
            .can_set(&destination.approvals_reviewer)
            .map_err(|err| invalid_request(err.to_string()))?;
        config.approvals_reviewer = destination.approvals_reviewer;
        config.workspace_roots = roots.clone();
        config.permissions.set_workspace_roots(roots.clone());
        config.model = Some(destination.model);
        config.model_reasoning_effort = destination.reasoning_effort;
        config.model_reasoning_summary = destination.reasoning_summary;
        config.service_tier = destination.service_tier;
        config.personality = destination.personality;
        let mut settings = original.settings.clone();
        settings.environments = Some(environments);
        settings.runtime_workspace_roots = Some(roots);
        settings.permission_profile = Some(destination.permission_profile);
        settings.profile_workspace_roots = Some(destination.profile_workspace_roots);
        // Materialize even a thread that has never submitted a user turn.
        thread.checkpoint_thread_settings().await.map_err(|err| {
            internal_error(format!(
                "failed to save thread before directory change: {err}"
            ))
        })?;
        self.thread_store
            .persist_thread(thread_id, PersistContext::Standard)
            .await
            .map_err(thread_store_resume_read_error)?;
        self.stop_thread_for_reload(request_id, thread_id, &thread)
            .await?;
        let result = match self
            .reload_thread_runtime(
                request_id,
                thread_id,
                ThreadRuntimeSnapshot {
                    config,
                    settings,
                    client_mcp_extensions: original.client_mcp_extensions.clone(),
                },
                app_server_client_name.clone(),
                app_server_client_version.clone(),
            )
            .await
        {
            Ok((replacement, response)) => replacement
                .reconcile_developer_instructions(original.config.developer_instructions.as_deref())
                .await
                .map(|()| (replacement, response))
                .map_err(|err| internal_error(err.to_string())),
            Err(error) => Err(error),
        };
        let (thread, response) = match result {
            Ok(reloaded) => reloaded,
            Err(error) => {
                if let Ok(replacement) = self.thread_manager.get_thread(thread_id).await {
                    self.stop_thread_for_reload(request_id, thread_id, &replacement)
                        .await?;
                }
                self.reload_thread_runtime(
                    request_id,
                    thread_id,
                    original,
                    app_server_client_name,
                    app_server_client_version,
                )
                .await
                .map_err(|restore| {
                    internal_error(format!(
                        "directory change failed: {}; restoring the original runtime failed: {}",
                        error.message, restore.message
                    ))
                })?;
                return Err(error);
            }
        };
        let settings = super::super::thread_summary::thread_settings_from_config_snapshot(
            &thread.config_snapshot().await,
        );
        let connections = self
            .thread_state_manager
            .subscribed_connection_ids(thread_id)
            .await;
        self.outgoing
            .send_server_notification_to_connections(
                &connections,
                ServerNotification::ThreadSettingsUpdated(ThreadSettingsUpdatedNotification {
                    thread_id: thread_id.to_string(),
                    thread_settings: settings,
                }),
            )
            .await;
        Ok(Some(ClientResponsePayload::ThreadCwdSet(response)))
    }

    pub(super) async fn stop_thread_for_reload(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: ThreadId,
        thread: &Arc<CodexThread>,
    ) -> Result<(), JSONRPCErrorError> {
        // Subscribe before shutdown to exclude idle unload, then drain the old listener before
        // installing a runtime under the same identity and subscriptions.
        if matches!(
            self.ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false
            )
            .await?,
            EnsureConversationListenerResult::ConnectionClosed
        ) {
            return Err(internal_error(format!(
                "connection closed before reloading thread {thread_id}"
            )));
        }
        let state = self.thread_state_manager.thread_state(thread_id).await;
        let drained = state.lock().await.register_shutdown_drain_waiter();
        match wait_for_thread_shutdown(thread).await {
            ThreadShutdownResult::Complete => {}
            ThreadShutdownResult::SubmitFailed | ThreadShutdownResult::TimedOut => {
                state.lock().await.take_shutdown_drain_waiter();
                return Err(internal_error(format!(
                    "failed to shut down thread {thread_id} before reload"
                )));
            }
        }
        let drain_result = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), drained)
            .await
            .map_err(|_| {
                internal_error(format!(
                    "timed out draining thread {thread_id} before reload"
                ))
            })
            .and_then(|result| {
                result.map_err(|_| {
                    internal_error(format!(
                        "listener stopped before draining thread {thread_id}"
                    ))
                })
            });
        if let Err(error) = drain_result {
            state.lock().await.take_shutdown_drain_waiter();
            return Err(error);
        }
        if self
            .thread_manager
            .remove_thread_if_matches(&thread_id, thread)
            .await
            .is_none()
        {
            return Err(internal_error(format!(
                "thread {thread_id} changed before reload"
            )));
        }
        self.outgoing
            .cancel_requests_for_thread(thread_id, /*error*/ None)
            .await;
        Ok(())
    }
}
