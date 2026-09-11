use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::time::Instant;
use tokio::time::sleep;
use wiremock::MockServer;

const PARENT_MODEL: &str = "gpt-5.2";
const CHILD_MODEL: &str = "gpt-5.4";
const SEED_PROMPT: &str = "seed the parent thread";
const SPAWN_PROMPT: &str = "spawn a child on another model";
const CHILD_PROMPT: &str = "child: do work";
const SPAWN_CALL_ID: &str = "spawn-call-1";
const COMPACT_PROMPT: &str = "FORKED_HISTORY_COMPACTION_PROMPT";
const FORK_SUMMARY: &str = "FORKED_HISTORY_SUMMARY";

fn body_contains(req: &wiremock::Request, text: &str) -> bool {
    let is_zstd = req
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    let body = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&req.body)).ok()
    } else {
        Some(req.body.clone())
    };
    body.and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

/// A mock records every request evaluated against it, so select the child's own requests by
/// content rather than by mount order.
fn is_child_turn_request(request: &ResponsesRequest) -> bool {
    request.body_contains_text(CHILD_PROMPT)
        && !request.body_contains_text(SPAWN_CALL_ID)
        && !request.body_contains_text(COMPACT_PROMPT)
}

async fn wait_for_child_requests(mock: &ResponseMock) -> Result<Vec<serde_json::Value>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let requests = mock
            .requests()
            .into_iter()
            .filter(is_child_turn_request)
            .map(|request| request.body_json())
            .collect::<Vec<_>>();
        if !requests.is_empty() {
            return Ok(requests);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the child's model request");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn mount_spawn_flow(server: &MockServer) -> (ResponseMock, ResponseMock) {
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "worker",
        "fork_turns": "all",
        "model": CHILD_MODEL,
    }))
    .expect("serialize spawn args");
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, SEED_PROMPT),
        sse(vec![
            ev_response_created("resp-seed"),
            // Forks keep only final-answer assistant messages from the parent.
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": "msg-seed",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "seeded"}]
                }
            }),
            ev_completed("resp-seed"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, SPAWN_PROMPT),
        sse(vec![
            ev_response_created("resp-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                "codex_agents",
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-spawn"),
        ]),
    )
    .await;
    // Pre-sampling compaction carries the local compaction prompt; the child's own turn does not.
    let compaction = mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, COMPACT_PROMPT),
        sse(vec![
            ev_response_created("resp-compact"),
            ev_assistant_message("msg-compact", FORK_SUMMARY),
            ev_completed("resp-compact"),
        ]),
    )
    .await;
    let child = mount_sse_once_match(
        server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT)
                && !body_contains(req, SPAWN_CALL_ID)
                && !body_contains(req, COMPACT_PROMPT)
        },
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", "child done"),
            ev_completed("resp-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-spawn-2"),
            ev_assistant_message("msg-spawn-2", "parent done"),
            ev_completed("resp-spawn-2"),
        ]),
    )
    .await;
    (compaction, child)
}

/// A child forked from a parent whose history contains model output must translate that
/// output under the parent model before sampling under its own; a child forked from a
/// parent that has produced nothing yet must not pay for that request.
#[test_case(false; "fresh parent skips compaction")]
#[test_case(true; "answered parent compacts under the parent model")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_compacts_forked_history_only_when_the_parent_model_produced_it(
    parent_answered_first: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (compaction, child) = mount_spawn_flow(&server).await;
    let test = test_codex()
        .with_model_info_override(PARENT_MODEL, |model| {
            model.comp_hash = Some("hash-parent".to_string());
        })
        .with_model_info_override(CHILD_MODEL, |model| {
            model.comp_hash = Some("hash-child".to_string());
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.model = Some(PARENT_MODEL.to_string());
            // Local compaction lets the test observe the previous-model request directly.
            config.model_provider.name = "test-provider".to_string();
            config
                .features
                .disable(Feature::RemoteCompactionV2)
                .expect("test config should allow feature update");
            config.compact_prompt = Some(COMPACT_PROMPT.to_string());
        })
        .build_with_auto_env(&server)
        .await?;

    if parent_answered_first {
        test.submit_turn(SEED_PROMPT).await?;
    }
    test.submit_turn(SPAWN_PROMPT).await?;

    let child_requests = wait_for_child_requests(&child).await?;
    let compaction_models = compaction
        .requests()
        .into_iter()
        .filter(|request| request.body_contains_text(COMPACT_PROMPT))
        .map(|request| request.body_json()["model"].clone())
        .collect::<Vec<_>>();
    let expected_compaction_models = if parent_answered_first {
        vec![json!(PARENT_MODEL)]
    } else {
        Vec::new()
    };
    assert_eq!(compaction_models, expected_compaction_models);
    assert_eq!(
        (
            child_requests.len(),
            child_requests[0]["model"].clone(),
            child_requests[0].to_string().contains(FORK_SUMMARY),
        ),
        (1, json!(CHILD_MODEL), parent_answered_first)
    );
    Ok(())
}
