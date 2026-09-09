use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStopInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;

struct UnfinishedWork(AtomicBool);

impl TurnLifecycleContributor for UnfinishedWork {
    fn on_completion_attempt<'a>(
        &'a self,
        _: TurnStopInput<'a>,
    ) -> ExtensionFuture<'a, Option<String>> {
        Box::pin(std::future::ready(
            (!self.0.swap(true, Ordering::Relaxed))
                .then(|| "Finish the remaining work. ".repeat(100)),
        ))
    }
}

#[tokio::test]
async fn unfinished_work_continues_the_same_turn_with_bounded_context() -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("first", "Attempted completion"),
                responses::ev_completed("first"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("second", "Work completed"),
                responses::ev_completed("second"),
            ]),
        ],
    )
    .await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(Arc::new(UnfinishedWork(AtomicBool::new(false))));
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Finish the work").await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].body_json();
    let reminder = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|content| content["text"].as_str())
        .find(|text| text.contains("<completion-continuation>"))
        .unwrap();
    assert!(reminder.contains("Finish the remaining work."));
    assert!(reminder.len() < 1_100);
    let first_metadata: serde_json::Value = serde_json::from_str(
        requests[0].body_json()["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap(),
    )?;
    let second_metadata: serde_json::Value = serde_json::from_str(
        body["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap(),
    )?;
    assert_eq!(
        first_metadata["turn_id"].as_str().unwrap(),
        second_metadata["turn_id"].as_str().unwrap()
    );
    Ok(())
}
