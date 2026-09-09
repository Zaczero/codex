//! The ledger as a World State section: rendered once per change, re-rendered in full after
//! compaction, and only when the model-visible copy is still in retained history.

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use serde_json::json;

pub const TASKS_WORLD_STATE_ID: &str = "tasks";
pub const TASK_LEDGER_OPEN_TAG: &str = "<task-ledger>";
pub const TASK_LEDGER_CLOSE_TAG: &str = "</task-ledger>";
const HEADER: &str =
    "Ledger at the start of this step. Later tool results take precedence over this snapshot.";

pub(crate) fn section(rendered: String) -> WorldStateSectionContribution {
    let body = format!("{HEADER}\n{rendered}");
    let snapshot = json!({ "body": body });
    let retained = body.clone();
    WorldStateSectionContribution::new(TASKS_WORLD_STATE_ID, snapshot, move |previous| {
        if let PreviousWorldStateSection::Known(previous) = &previous
            && previous.get("body").and_then(serde_json::Value::as_str) == Some(body.as_str())
        {
            return None;
        }
        Some(RenderedWorldStateFragment::new(
            "developer",
            (TASK_LEDGER_OPEN_TAG, TASK_LEDGER_CLOSE_TAG),
            body.clone(),
        ))
    })
    .with_retained_fragment_matcher(move |role, text| {
        role == "developer" && text.contains(&retained)
    })
}
