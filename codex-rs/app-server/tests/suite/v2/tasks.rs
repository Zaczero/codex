use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadTaskParallelismParams;
use codex_app_server_protocol::ThreadTaskParallelismResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnPlanStep;
use codex_app_server_protocol::TurnPlanStepStatus;
use codex_app_server_protocol::TurnPlanUpdatedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_activation_sends_complete_old_notes_and_readable_overflow_pages() -> Result<()> {
    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_function_call(
                "activate",
                "ledger",
                &json!({"tasks": [{"id": "1", "status": "active"}]}).to_string(),
            ),
            responses::ev_completed("activated"),
        ]),
    )
    .await;
    let activation_request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_function_call(
                "next-page",
                "ledger",
                &json!({"recall": ["1"], "page": 2}).to_string(),
            ),
            responses::ev_completed("read-next-page"),
        ]),
    )
    .await;
    let final_request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("done", "All task evidence read."),
            responses::ev_completed("done"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_provider_config("supports_websockets = false")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let old_notes = (0..6)
        .map(|n| {
            format!(
                "Historic finding {n}: {} decisive tail {n}",
                "full evidence ".repeat(30)
            )
        })
        .collect::<Vec<_>>();
    let large_note = format!(
        "{} final evidence marker",
        "Retained 🧭 evidence. ".repeat(1_200)
    );
    let body = format!(
        "Current contract: {} end of current contract",
        "All details matter. ".repeat(120)
    );
    let mut findings = old_notes
        .iter()
        .map(|note| json!({"at": 1000, "text": note}))
        .collect::<Vec<_>>();
    findings.push(json!({"at": 2000, "text": large_note, "from": {"task": "2", "title": "Earlier investigation"}}));
    let directory = home.path().join("tasks").join(&thread.id);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("ledger.json"),
        serde_json::to_vec(&json!({"tasks": [{
            "id": "1", "title": "Use the complete evidence", "status": "pending",
            "body": body, "findings": findings, "created": 1000, "updated": 2000,
        }]}))?,
    )?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id,
        input: vec![UserInput::Text {
            text: "Activate the recorded task and read all its evidence.".to_string(),
            text_elements: vec![],
        }],
        ..Default::default()
    })
    .await?;
    let first = activation_request
        .single_request()
        .function_call_output_text("activate")
        .expect("activation tool output");
    let last = final_request
        .single_request()
        .function_call_output_text("next-page")
        .expect("continuation tool output");
    assert!(first.contains(&body));
    for note in old_notes {
        assert!(first.contains(&note), "missing full old note: {note}");
    }
    assert!(first.contains("Read every remaining page before working"));
    assert!(!last.contains("Read every remaining page before working"));
    let first_part = first
        .split_once(":\n\n")
        .expect("first page header")
        .1
        .split_once("\n\nRead every remaining page")
        .expect("continuation instruction")
        .0;
    let last_part = last.split_once(":\n\n").expect("last page header").1;
    assert!(format!("{first_part}{last_part}").contains(&large_note));
    assert!(first.contains("from 2 Earlier investigation"));
    assert!(app.shutdown_gracefully().await?.success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_ledger_capacity_allows_open_work_and_session_changes_survive_resume() -> Result<()>
{
    let server = responses::start_mock_server().await;
    let tasks = (1..=9)
        .map(|id| json!({"title": format!("Outcome {id}"), "status": "active"}))
        .collect::<Vec<_>>();
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_function_call(
                "record-tasks",
                "ledger",
                &json!({"tasks": tasks}).to_string(),
            ),
            responses::ev_completed("tasks-recorded"),
        ]),
    )
    .await;
    let completed_request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("message", "Work recorded."),
            responses::ev_completed("turn-complete"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_provider_config("supports_websockets = false")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let TurnStartResponse { turn } = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Record the independent outcomes.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let plan: TurnPlanUpdatedNotification = timeout(
        Duration::from_secs(/*secs*/ 30),
        app.read_notification("turn/plan/updated"),
    )
    .await??;
    assert_eq!(
        plan,
        TurnPlanUpdatedNotification {
            thread_id: thread.id.clone(),
            turn_id: turn.id,
            explanation: Some("9 active · 0 ready".to_string()),
            plan: (1..=8)
                .map(|id| TurnPlanStep {
                    step: format!("#{id} Outcome {id}"),
                    status: TurnPlanStepStatus::InProgress,
                })
                .collect(),
        }
    );
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(/*secs*/ 30),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let request = completed_request.single_request();
    assert!(
        request
            .function_call_output("record-tasks")
            .to_string()
            .contains("9 active")
    );
    let response: ThreadTaskParallelismResponse = app
        .request(|request_id| ClientRequest::ThreadTaskParallelism {
            request_id,
            params: ThreadTaskParallelismParams {
                thread_id: thread.id.clone(),
                parallelism: Some(4),
            },
        })
        .await?;
    assert_eq!(response, ThreadTaskParallelismResponse { parallelism: 4 });
    let other = app.start_thread(ThreadStartParams::default()).await?.thread;
    let response: ThreadTaskParallelismResponse = app
        .request(|request_id| ClientRequest::ThreadTaskParallelism {
            request_id,
            params: ThreadTaskParallelismParams {
                thread_id: other.id,
                parallelism: None,
            },
        })
        .await?;
    assert_eq!(response, ThreadTaskParallelismResponse { parallelism: 0 });
    assert!(app.shutdown_gracefully().await?.success());
    let mut resumed = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let _: ThreadResumeResponse = resumed
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: thread.id.clone(),
                ..Default::default()
            },
        })
        .await?;
    let response: ThreadTaskParallelismResponse = resumed
        .request(|request_id| ClientRequest::ThreadTaskParallelism {
            request_id,
            params: ThreadTaskParallelismParams {
                thread_id: thread.id,
                parallelism: None,
            },
        })
        .await?;
    assert_eq!(response, ThreadTaskParallelismResponse { parallelism: 4 });
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallelism_changes_during_a_turn_notify_the_model_and_control_admission() -> Result<()> {
    let server = responses::start_mock_server().await;
    let question = json!({"questions": [{"id": "continue", "header": "Continue", "question": "Continue?", "options": [
        {"label": "Yes", "description": "Continue working."},
        {"label": "No", "description": "Stop working."}
    ]}]}).to_string();
    let tasks = json!({"tasks": [
        {"title": "First outcome", "status": "active"},
        {"title": "Second outcome", "status": "active"}
    ]})
    .to_string();
    let mut requests = Vec::new();
    for (id, tool, args) in [
        ("wait-for-limit", "request_user_input", question.as_str()),
        ("limited-write", "ledger", tasks.as_str()),
        ("wait-for-zero", "request_user_input", question.as_str()),
        ("unlimited-write", "ledger", tasks.as_str()),
    ] {
        requests.push(
            responses::mount_sse_once(
                &server,
                responses::sse(vec![
                    responses::ev_function_call(id, tool, args),
                    responses::ev_completed(id),
                ]),
            )
            .await,
        );
    }
    let final_request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("done", "Recorded."),
            responses::ev_completed("done"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_provider_config("supports_websockets = false")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            config: Some(std::collections::HashMap::from([(
                "features.default_mode_request_user_input".to_string(),
                json!(true),
            )])),
            ..Default::default()
        })
        .await?
        .thread;
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Record work after I adjust capacity.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    for parallelism in [1, 0] {
        let request = timeout(
            Duration::from_secs(/*secs*/ 30),
            app.read_stream_until_request_message(),
        )
        .await??;
        let ServerRequest::ToolRequestUserInput { request_id, .. } = request else {
            panic!("expected user input request: {request:?}");
        };
        let response: ThreadTaskParallelismResponse = app
            .request(|request_id| ClientRequest::ThreadTaskParallelism {
                request_id,
                params: ThreadTaskParallelismParams {
                    thread_id: thread.id.clone(),
                    parallelism: Some(parallelism),
                },
            })
            .await?;
        assert_eq!(response, ThreadTaskParallelismResponse { parallelism });
        app.send_response(
            request_id,
            json!({"answers": {"continue": {"answers": ["Yes"]}}}),
        )
        .await?;
    }
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(/*secs*/ 30),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert!(
        requests[1]
            .single_request()
            .body_json()
            .to_string()
            .contains("Task parallelism: 1 independent active outcomes.")
    );
    let rejected = requests[2]
        .single_request()
        .function_call_output("limited-write")
        .to_string();
    assert!(
        rejected.contains("task slots are already busy"),
        "{rejected}"
    );
    assert!(
        requests[3]
            .single_request()
            .body_json()
            .to_string()
            .contains("Task parallelism: off.")
    );
    let accepted = final_request
        .single_request()
        .function_call_output("unlimited-write")
        .to_string();
    assert!(accepted.contains("2 active"), "{accepted}");
    Ok(())
}
