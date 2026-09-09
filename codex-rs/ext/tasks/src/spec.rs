//! The `ledger` tool definition.

use std::collections::BTreeMap;

use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema;
use codex_tools::JsonSchema;
use serde_json::Value;
use serde_json::json;

pub const LEDGER_TOOL_NAME: &str = "ledger";

const DESCRIPTION: &str = "Keep this thread's work ledger: outcomes, current work, and findings.
Each task is one independently ownable outcome. Mark work active when taken up and done after review and integration. Use `after` and `before` for dependencies, `waiting` for an external condition or decision, and `cancelled` for abandoned work with its reason. For delegated work, set `owner` to the agent's canonical task path returned by spawn_agent; this is distinct from the ledger task ID.
The user's parallelism setting limits independent active outcomes. New root sessions start at zero, which disables limits and completion reminders. Only the user changes it with /parallelism. Child sessions follow the root: zero when off, otherwise one active outcome.
Writes are atomic. `split` keeps the original task's history on the first piece. `merge` moves absorbed tasks' findings and dependents to the survivor and leaves their IDs as cancelled references.
Continue executable work before ending the turn. While a spawned owner runs, use wait_agent: its result does not start an idle turn. Use `pause` with a reason when an external condition or the user's instruction requires stopping; null resumes. Task changes also clear a pause; reads and recall do not.
Keep `body` current with conclusions, constraints, remaining work, and next action. Append evidence and decisions as notes. Activation returns the full body and findings, paginated when necessary. Read every page using the same task IDs in `recall`; restart at page 1 if the records change. Empty recall returns the bounded overview. Recall named tasks as needed rather than every task after compaction.
Children own separate ledgers and may read named parent tasks with `parent:`; parent task writes are refused. Put lasting project guidance in AGENTS.md, where it remains discoverable after tasks close.";

fn ledger_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pause": {
                "type": ["string", "null"],
                "description": "Reason to pause this thread with unfinished work; null resumes. May accompany task changes; cannot accompany recall."
            },
            "tasks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Stable task id; an existing title also resolves." },
                        "title": { "type": "string", "description": "One-line label, at most 80 characters; creates a task when no existing task has it." },
                        "status": { "type": "string", "enum": ["pending", "active", "done"], "description": "Task state; new tasks start pending." },
                        "after": { "type": ["array", "null"], "items": { "type": "string" }, "description": "Complete prerequisite set; null clears it." },
                        "before": { "type": ["array", "null"], "items": { "type": "string" }, "description": "Complete direct-dependent set; null clears it." },
                        "waiting": { "type": ["string", "null"], "description": "Named external condition or decision this task waits for; null clears it." },
                        "owner": { "type": ["string", "null"], "description": "Canonical task name of the agent executing this task; null clears it." },
                        "cancelled": { "type": ["string", "null"], "description": "Why this task was abandoned; null or empty reopens it as pending." },
                        "split": { "type": "array", "items": { "type": "string" }, "description": "Replacement task titles; the first keeps this row and its history." },
                        "merge": { "type": "array", "items": { "type": "string" }, "description": "Tasks whose outcome this task absorbed; give the reconciled body alongside." },
                        "body": { "type": "string", "description": "The task's current conclusions, constraints, remaining work, and next action, replaced wholesale on each write; empty clears it." },
                        "note": { "type": "string", "description": "A finding: a discrete discovery or conclusion the rest of this plan needs. Appended with a timestamp, never replacing prior findings." }
                    },
                    "additionalProperties": false
                }
            },
            "recall": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Read-only task detail with every finding; cannot be combined with tasks or pause. Omit or leave empty for the bounded default view. Name local tasks by id or title and prefix parent tasks with `parent:`."
            },
            "page": {
                "type": "integer",
                "minimum": 1,
                "description": "One-based continuation page for named recall; defaults to 1. Read all pages supplied on activation before working."
            }
        },
        "additionalProperties": false
    })
}

pub fn create_ledger_tool() -> ToolSpec {
    let parameters = match parse_tool_input_schema(&ledger_input_schema()) {
        Ok(parameters) => parameters,
        Err(err) => {
            tracing::error!("ledger tool schema failed to parse: {err}");
            JsonSchema::object(BTreeMap::new(), None, Some(false.into()))
        }
    };
    ToolSpec::Function(ResponsesApiTool {
        name: LEDGER_TOOL_NAME.to_string(),
        description: DESCRIPTION.to_string(),
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_tool_schema_parses() {
        assert!(parse_tool_input_schema(&ledger_input_schema()).is_ok());
    }
}
