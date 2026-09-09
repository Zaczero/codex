use crate::*;
use codex_protocol::auth::AccountScope;
use pretty_assertions::assert_eq;
use serde_json::json;

fn account(name: &str) -> AccountScope {
    AccountScope {
        local_account_id: Some(name.to_string()),
        account_id: "workspace".to_string(),
        user_id: Some(format!("user-{name}")),
    }
}

#[test]
fn account_projection_keeps_owned_state_and_removes_unknown_or_foreign_ciphertext() {
    let a = account("a");
    let b = account("b");
    for (input, expected) in [
        (
            json!({"type":"reasoning","summary":[],"encrypted_content":"secret-a"}),
            None,
        ),
        (
            json!({"type":"compaction","encrypted_content":"secret-a"}),
            None,
        ),
        (
            json!({"type":"context_compaction","encrypted_content":"secret-a"}),
            None,
        ),
        (
            json!({"type":"function_call","name":"read","arguments":"{}","call_id":"call-1","encrypted_function_args":["secret-a"]}),
            Some(json!({"type":"function_call","name":"read","arguments":"{}","call_id":"call-1"})),
        ),
        (
            json!({"type":"function_call_output","call_id":"call-1","output":[{"type":"input_text","text":"public result"},{"type":"encrypted_content","encrypted_content":"secret-a"}]}),
            Some(
                json!({"type":"function_call_output","call_id":"call-1","output":[{"type":"input_text","text":"public result"}]}),
            ),
        ),
        (
            json!({"type":"custom_tool_call_output","call_id":"call-2","output":[{"type":"encrypted_content","encrypted_content":"secret-a"}]}),
            Some(json!({"type":"custom_tool_call_output","call_id":"call-2","output":[]})),
        ),
        (
            json!({"type":"agent_message","author":"worker","recipient":"root","content":[{"type":"input_text","text":"public finding"},{"type":"encrypted_content","encrypted_content":"secret-a"}]}),
            Some(
                json!({"type":"agent_message","author":"worker","recipient":"root","content":[{"type":"input_text","text":"public finding"}]}),
            ),
        ),
        (
            json!({"type":"agent_message","author":"worker","recipient":"root","content":[{"type":"encrypted_content","encrypted_content":"secret-a"}]}),
            None,
        ),
    ] {
        let item: ResponseItem = serde_json::from_value(input).unwrap();
        let expected = expected.map(|value| serde_json::from_value::<ResponseItem>(value).unwrap());
        let owned = ResponseItemEnvelope {
            item: item.clone(),
            metadata: Some(CodexHarnessMetadata {
                account_scope: Some(a.clone()),
                ..Default::default()
            }),
        };
        assert_eq!(owned.for_account(Some(&a)), Some(item.clone()));
        assert_eq!(owned.for_account(Some(&b)), expected);
        assert_eq!(owned.for_account(None), expected);
        assert_eq!(
            ResponseItemEnvelope::new(item).for_account(Some(&a)),
            expected
        );
    }
}

#[test]
fn account_provenance_survives_rollout_serialization() {
    let envelope = ResponseItemEnvelope {
        item: serde_json::from_value(json!({"type":"compaction","encrypted_content":"secret-a"}))
            .unwrap(),
        metadata: Some(CodexHarnessMetadata {
            account_scope: Some(account("a")),
            ..Default::default()
        }),
    };
    let serialized = serde_json::to_string(&RolloutItem::ResponseItem(envelope.clone())).unwrap();
    let RolloutItem::ResponseItem(restored) = serde_json::from_str(&serialized).unwrap() else {
        panic!("expected response item");
    };
    assert_eq!(restored, envelope);
    assert_eq!(restored.for_account(Some(&account("b"))), None);
}
