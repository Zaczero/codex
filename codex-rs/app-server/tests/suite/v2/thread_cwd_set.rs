use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadCwdSetParams;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadTaskParallelismParams;
use codex_app_server_protocol::ThreadTaskParallelismResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test_case::test_case(ThreadHistoryMode::Legacy, false; "legacy_empty")]
#[test_case::test_case(ThreadHistoryMode::Legacy, true; "legacy_history")]
#[test_case::test_case(ThreadHistoryMode::Paginated, false; "paginated_empty")]
#[test_case::test_case(ThreadHistoryMode::Paginated, true; "paginated_history")]
#[tokio::test]
async fn cwd_reload_preserves_thread_state_and_loads_project_configuration(
    history_mode: ThreadHistoryMode,
    has_history: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let home = TempDir::new()?;
    let source = TempDir::new()?;
    let destination = TempDir::new()?;
    fs::create_dir(destination.path().join(".git"))?;
    fs::create_dir(destination.path().join(".codex"))?;
    fs::write(
        destination.path().join("AGENTS.md"),
        "DESTINATION_WORKSPACE_INSTRUCTIONS",
    )?;
    fs::write(
        destination.path().join(".codex/config.toml"),
        "developer_instructions = 'PROJECT_WORKSPACE_DEVELOPER_INSTRUCTIONS'\n[shell_environment_policy.set]\nCWD_RELOAD_MARKER = 'project-environment-reloaded'\n",
    )?;
    MockResponsesConfig::new(&server.uri())
        .with_sandbox_mode("danger-full-access")
        .disable_feature(Feature::ShellSnapshot)
        .with_provider_config("supports_websockets = false")
        .with_extra_config(&format!(
            "[projects.{}]\ntrust_level = 'trusted'\n",
            json!(destination.path())
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let started = app
        .start_thread(ThreadStartParams {
            cwd: Some(source.path().to_string_lossy().into_owned()),
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?;
    let id = started.thread.id.clone();
    let _: ThreadSetNameResponse = app
        .request(|request_id| ClientRequest::ThreadSetName {
            request_id,
            params: ThreadSetNameParams {
                thread_id: id.clone(),
                name: "Persistent working session".into(),
            },
        })
        .await?;
    if has_history {
        responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("before", "SAVED_HISTORY_MARKER"),
                responses::ev_completed("before-response"),
            ]),
        )
        .await;
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: id.clone(),
            input: vec![UserInput::Text {
                text: "Keep this conversation.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    }
    let ledger_dir = home.path().join("tasks").join(&id);
    fs::create_dir_all(&ledger_dir)?;
    let ledger = json!({"parallelism": 8, "pause": "Waiting for an external decision", "tasks": [
        {"id": "17", "title": "Integrated outcome", "status": "active", "body": "Preserve current conclusions",
         "created": 1000, "updated": 1200, "findings": [{"at": 1100, "text": "Evidence that must survive"}]},
        {"id": "18", "title": "Dependent outcome", "status": "pending", "after": ["17"], "created": 1000, "updated": 1000}
    ]});
    fs::write(ledger_dir.join("ledger.json"), serde_json::to_vec(&ledger)?)?;
    let before_requests = server
        .received_requests()
        .await
        .expect("recorded requests")
        .len();
    let moved: ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadCwdSet {
            request_id,
            params: ThreadCwdSetParams {
                thread_id: id.clone(),
                cwd: destination.path().into(),
                developer_instructions: has_history.then(|| "CLIENT_WORKSPACE_INSTRUCTIONS".into()),
            },
        })
        .await?;
    assert_eq!(moved.thread.id, id);
    assert_eq!(
        moved.thread.name.as_deref(),
        Some("Persistent working session")
    );
    assert_eq!(moved.thread.forked_from_id, started.thread.forked_from_id);
    assert_eq!(moved.cwd.as_path(), destination.path());
    assert_eq!(moved.runtime_workspace_roots, vec![moved.cwd.clone()]);
    assert_eq!(moved.thread.path, started.thread.path);
    assert!(moved.thread.turns.is_empty());
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .len(),
        before_requests
    );
    let updated: ThreadSettingsUpdatedNotification =
        app.read_notification("thread/settings/updated").await?;
    assert_eq!(updated.thread_id, id);
    assert_eq!(updated.thread_settings.cwd, moved.cwd);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(ledger_dir.join("ledger.json"))?)?,
        ledger
    );
    let capacity: ThreadTaskParallelismResponse = app
        .request(|request_id| ClientRequest::ThreadTaskParallelism {
            request_id,
            params: ThreadTaskParallelismParams {
                thread_id: id.clone(),
                parallelism: None,
            },
        })
        .await?;
    assert_eq!(capacity.parallelism, 8);

    if !has_history {
        // No user turn may be needed to make the directory change durable.
        app.shutdown_gracefully().await?;
        app = TestAppServer::builder()
            .with_codex_home(home.path())
            .without_managed_config()
            .build_initialized()
            .await?;
        let resumed: ThreadResumeResponse = app
            .request(|request_id| ClientRequest::ThreadResume {
                request_id,
                params: ThreadResumeParams {
                    thread_id: id.clone(),
                    exclude_turns: true,
                    ..Default::default()
                },
            })
            .await?;
        assert_eq!(resumed.cwd, moved.cwd);
    }

    let call = responses::mount_sse_once(
        &server,
        responses::sse(vec![
        responses::ev_function_call("check-directory", "exec_command", &json!({
            "cmd": if cfg!(windows) { "Write-Output $env:CWD_RELOAD_MARKER; (Get-Location).Path" }
                else { "printf '%s\\n' \"$CWD_RELOAD_MARKER\"; pwd" }, "yield_time_ms": 1000
        }).to_string()), responses::ev_completed("call-response"),
    ]),
    )
    .await;
    let completed = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("after", "Directory verified."),
            responses::ev_completed("after-response"),
        ]),
    )
    .await;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: id.clone(),
        input: vec![UserInput::Text {
            text: "Verify the destination environment.".into(),
            text_elements: vec![],
        }],
        ..Default::default()
    })
    .await?;
    let prompt = call.single_request().message_input_texts("user").join("\n");
    assert!(
        prompt.contains("DESTINATION_WORKSPACE_INSTRUCTIONS"),
        "{prompt}"
    );
    let developer = call
        .single_request()
        .message_input_texts("developer")
        .join("\n");
    let expected = if has_history {
        "CLIENT_WORKSPACE_INSTRUCTIONS"
    } else {
        "PROJECT_WORKSPACE_DEVELOPER_INSTRUCTIONS"
    };
    assert!(developer.contains(expected), "{developer}");
    if has_history {
        let input = call.single_request().inputs_of_type("message");
        assert!(
            input.iter().any(|item| item["role"] == "assistant"
                && item["content"][0]["text"] == "SAVED_HISTORY_MARKER"),
            "{input:?}"
        );
    }
    let output = completed
        .single_request()
        .function_call_output_text("check-directory")
        .expect("shell output");
    assert!(output.contains("project-environment-reloaded"), "{output}");
    assert!(
        output.contains(&destination.path().to_string_lossy().to_string()),
        "{output}"
    );
    if has_history {
        let _: ThreadResumeResponse = app
            .request(|request_id| ClientRequest::ThreadCwdSet {
                request_id,
                params: ThreadCwdSetParams {
                    thread_id: id.clone(),
                    cwd: destination.path().into(),
                    developer_instructions: Some(String::new()),
                },
            })
            .await?;
        let cleared = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("cleared", "Instructions cleared."),
                responses::ev_completed("cleared-response"),
            ]),
        )
        .await;
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: id.clone(),
            input: vec![UserInput::Text {
                text: "Continue without the prior client instructions.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
        assert!(cleared.single_request().message_input_texts("developer").iter().any(|text|
            text.contains("previously provided workspace and client developer instructions no longer apply")));
    }
    app.shutdown_gracefully().await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let resumed: ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: id.clone(),
                exclude_turns: true,
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(resumed.cwd, moved.cwd);
    assert_eq!(resumed.thread.id, id);
    let capacity: ThreadTaskParallelismResponse = app
        .request(|request_id| ClientRequest::ThreadTaskParallelism {
            request_id,
            params: ThreadTaskParallelismParams {
                thread_id: id,
                parallelism: None,
            },
        })
        .await?;
    assert_eq!(capacity.parallelism, 8);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(ledger_dir.join("ledger.json"))?)?,
        ledger
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

#[test_case::test_case("invalid = [", "config.toml"; "invalid_configuration")]
#[test_case::test_case("[mcp_servers.required_broken]\ncommand = 'codex-missing-mcp-fixture'\nrequired = true\n", "required MCP servers failed to initialize"; "runtime_startup_failure")]
#[tokio::test]
async fn destination_failure_leaves_original_thread_usable(
    config: &str,
    expected_error: &str,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let home = TempDir::new()?;
    let source = TempDir::new()?;
    let destination = TempDir::new()?;
    fs::create_dir(destination.path().join(".git"))?;
    fs::create_dir(destination.path().join(".codex"))?;
    fs::write(destination.path().join(".codex/config.toml"), config)?;
    MockResponsesConfig::new(&server.uri())
        .with_provider_config("supports_websockets = false")
        .with_extra_config(&format!(
            "[projects.{}]\ntrust_level = 'trusted'\n",
            json!(destination.path())
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let started = app
        .start_thread(ThreadStartParams {
            cwd: Some(source.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let original_environment = started
        .thread
        .environments
        .as_ref()
        .expect("local environment")[0]
        .cwd
        .clone();
    let request_id = app
        .send_raw_request(
            "thread/cwd/set",
            Some(json!({
                "threadId": started.thread.id, "cwd": destination.path()
            })),
        )
        .await?;
    let error = app
        .read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(request_id))
        .await?;
    assert!(error.error.message.contains(expected_error), "{error:?}");
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
    let response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("still-usable", "Original session still usable."),
            responses::ev_completed("original-response"),
        ]),
    )
    .await;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: started.thread.id,
        input: vec![UserInput::Text {
            text: "Continue in the original workspace.".into(),
            text_elements: vec![],
        }],
        ..Default::default()
    })
    .await?;
    let prompt = response
        .single_request()
        .message_input_texts("user")
        .join("\n");
    assert!(
        prompt.contains(&format!("<cwd>{original_environment}</cwd>")),
        "{prompt}"
    );
    app.shutdown_gracefully().await?;
    Ok(())
}
