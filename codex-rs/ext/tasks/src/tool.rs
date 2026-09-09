//! The `ledger` tool: one call records a batch of task changes, a pause, or a recall.
//!
//! Refusals are ordinary results that say what was wrong; nothing here reaches the model as a
//! fatal error except a broken ledger file.

use std::sync::Arc;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use serde_json::Value;

use crate::extension::ThreadTasks;
use crate::ledger::Entry;
use crate::ledger::Ledger;
use crate::ledger::View;
use crate::ledger::render;
use crate::ledger::render_recall;
use crate::spec::LEDGER_TOOL_NAME;
use crate::spec::create_ledger_tool;
use crate::storage;

pub(crate) struct LedgerTool {
    pub(crate) thread: Arc<ThreadTasks>,
}

/// Plain text for the model; the ledger speaks prose, not JSON.
struct TextToolOutput(String, Option<codex_protocol::plan_tool::UpdatePlanArgs>);

impl ToolOutput for TextToolOutput {
    fn plan_update(&self) -> Option<codex_protocol::plan_tool::UpdatePlanArgs> {
        self.1.clone()
    }
    fn log_output(&self) -> String {
        self.0.clone()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn fallback_token_limit_override(&self) -> Option<usize> {
        Some(8_000)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let output = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(self.0.clone()),
            success: Some(true),
        };
        if matches!(payload, ToolPayload::Custom { .. }) {
            return ResponseInputItem::CustomToolCallOutput {
                call_id: call_id.to_string(),
                name: None,
                output,
            };
        }
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }
}

fn text(body: impl Into<String>) -> Box<dyn ToolOutput> {
    Box::new(TextToolOutput(body.into(), None))
}

impl<'call> ToolExecutor<ToolCall<'call>> for LedgerTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LEDGER_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_ledger_tool()
    }

    fn handle<'a>(
        &'a self,
        invocation: ToolCall<'call>,
    ) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let arguments = invocation.function_arguments()?;
            let input: Value = serde_json::from_str(arguments)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            self.handle_input(input).await
        })
    }
}

/// Every field an entry may carry, so one sent without its `tasks` wrapper is still one.
const FIELDS: [&str; 12] = [
    "id",
    "title",
    "status",
    "after",
    "before",
    "waiting",
    "owner",
    "cancelled",
    "split",
    "merge",
    "body",
    "note",
];

fn string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => Some(items.iter().filter_map(|item| string(Some(item))).collect()),
        Value::String(_) => Some(string(Some(value)).into_iter().collect()),
        _ => None,
    }
}

/// `Some(None)` clears, `Some(Some(..))` sets, `None` leaves alone.
fn clearable_list(value: Option<&Value>) -> Option<Option<Vec<String>>> {
    match value {
        None => None,
        Some(Value::Null) => Some(None),
        Some(other) => string_list(other).map(Some),
    }
}

fn clearable_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(string(Some(&Value::String(text.clone())))),
        Some(_) => None,
    }
}

pub(crate) fn parse_entry(raw: &Value) -> Entry {
    let Some(object) = raw.as_object() else {
        return Entry {
            invalid: Some("each task entry must be an object.".to_string()),
            ..Entry::default()
        };
    };
    Entry {
        invalid: None,
        id: string(object.get("id")),
        title: string(object.get("title")),
        status: object
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string),
        after: clearable_list(object.get("after")),
        before: clearable_list(object.get("before")),
        waiting: clearable_string(object.get("waiting")),
        owner: clearable_string(object.get("owner")),
        cancelled: clearable_string(object.get("cancelled")),
        split: object.get("split").and_then(string_list),
        merge: object.get("merge").and_then(string_list),
        body: object
            .get("body")
            .and_then(Value::as_str)
            .map(str::to_string),
        note: object
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn list(ledger: &Ledger, ids: &[String]) -> String {
    ids.iter()
        .filter_map(|id| ledger.find_by_id(id))
        .map(|task| format!("{} {}", task.id, task.title))
        .collect::<Vec<_>>()
        .join(", ")
}

impl LedgerTool {
    async fn handle_input(&self, input: Value) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let object = input.as_object().cloned().unwrap_or_default();
        let entries: Vec<Entry> = match object.get("tasks") {
            Some(Value::Array(items)) => items.iter().map(parse_entry).collect(),
            _ if FIELDS.iter().any(|field| object.contains_key(*field)) => {
                vec![parse_entry(&input)]
            }
            _ => Vec::new(),
        };
        let recall: Option<Vec<String>> = object.get("recall").and_then(string_list);
        let page = match object.get("page") {
            None => 1,
            Some(value) => match value.as_u64().and_then(|page| usize::try_from(page).ok()) {
                Some(page) if page > 0 => page,
                _ => return Ok(text("Page must be a positive integer.")),
            },
        };
        if object.contains_key("page") && recall.as_ref().is_none_or(Vec::is_empty) {
            return Ok(text("Page requires named task references in recall."));
        }
        let pausing = object.contains_key("pause");
        let pause = match object.get("pause") {
            None | Some(Value::Null) => None,
            Some(Value::String(reason)) if !reason.trim().is_empty() => {
                Some(reason.trim().to_string())
            }
            Some(_) => {
                return Ok(text(
                    "Pause requires a nonempty reason; use null to resume.",
                ));
            }
        };
        if recall.is_some() && (object.contains_key("tasks") || !entries.is_empty() || pausing) {
            return Ok(text(
                "Recall is read-only and cannot be combined with tasks or pause.",
            ));
        }
        if recall.is_some() || (entries.is_empty() && !pausing) {
            return self.recall(recall.unwrap_or_default(), page).await;
        }
        self.write(entries, pausing, pause).await
    }

    async fn recall(
        &self,
        references: Vec<String>,
        page: usize,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let own = storage::read(&self.thread.dir).await.map_err(unreadable)?;
        if references.is_empty() {
            return Ok(text(format!(
                "Ledger file: {}\n{}",
                self.thread.dir.join(storage::LEDGER_FILE).display(),
                render(&own)
            )));
        }
        let needs_parent = references
            .iter()
            .any(|reference| reference.starts_with("parent:"));
        let parent = match (&self.thread.parent_dir, needs_parent) {
            (Some(dir), true) => Some(storage::read(dir).await.map_err(unreadable)?),
            (None, true) => return Ok(text("This thread has no parent ledger to recall.")),
            _ => None,
        };
        let mut unknown: Vec<String> = Vec::new();
        let mut chunks = std::collections::BTreeMap::new();
        let mut ordered: Vec<&String> = references.iter().collect();
        ordered.sort();
        ordered.dedup();
        for reference in ordered {
            let (source, wanted, prefix) = match reference.strip_prefix("parent:") {
                Some(rest) => {
                    if rest.trim().is_empty() {
                        return Ok(text(
                            "Parent recall requires a task id or title; it never reads the whole parent ledger.",
                        ));
                    }
                    (parent.as_ref(), rest.trim(), "parent:")
                }
                None => (Some(&own), reference.as_str(), ""),
            };
            let Some((source, task)) =
                source.and_then(|ledger| ledger.resolve(wanted).map(|task| (ledger, task)))
            else {
                unknown.push(reference.clone());
                continue;
            };
            let key = format!("{prefix}{}", task.id);
            chunks
                .entry(key)
                .or_insert_with(|| render_recall(source, task, prefix));
        }
        let mut messages: Vec<String> = Vec::new();
        if !unknown.is_empty() {
            messages.push(format!("Unknown task references: {}.", unknown.join(", ")));
            if let Some(parent_dir) = &self.thread.parent_dir
                && unknown
                    .iter()
                    .any(|reference| !reference.starts_with("parent:"))
            {
                let parent = match parent {
                    Some(parent) => parent,
                    None => storage::read(parent_dir).await.map_err(unreadable)?,
                };
                let hints: Vec<String> = unknown
                    .iter()
                    .filter(|reference| !reference.starts_with("parent:"))
                    .filter_map(|reference| parent.resolve(reference))
                    .map(|task| format!("parent:{}", task.id))
                    .collect();
                if !hints.is_empty() {
                    messages.push(format!(
                        "These tasks are in the parent ledger. Retry them as: {}.",
                        hints.join(", ")
                    ));
                }
            }
        }
        if !chunks.is_empty() {
            messages.push(chunks.into_values().collect::<Vec<_>>().join("\n\n"));
        }
        if messages.is_empty() {
            return Ok(text("No tasks found for the requested references."));
        }
        Ok(text(crate::details::render(&messages.join("\n\n"), page)))
    }

    async fn write(
        &self,
        entries: Vec<Entry>,
        pausing: bool,
        pause: Option<String>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let now = storage::now_ms();
        let thread = Arc::clone(&self.thread);
        struct Written {
            plan_update: Option<codex_protocol::plan_tool::UpdatePlanArgs>,
            errors: Vec<String>,
            summary_line: String,
            pause_line: Option<String>,
            activated: std::collections::BTreeMap<String, String>,
            attempted_unknown: Vec<String>,
        }
        let written = storage::mutate(&self.thread.dir, move |ledger| {
            let previous_plan = crate::presentation::plan(ledger);
            let prior: std::collections::HashMap<String, (crate::ledger::Status, View)> = ledger
                .tasks
                .iter()
                .map(|task| (task.id.clone(), (task.status, ledger.view(task).state)))
                .collect();
            let applied = ledger.upsert(&entries, now);
            let mut pause_error = None;
            let previous_pause = ledger.pause.clone();
            if applied.errors.is_empty() {
                if pause.is_some() && ledger.outstanding().is_empty() {
                    pause_error = Some(
                        "No unfinished work to pause. Record the remaining tasks first."
                            .to_string(),
                    );
                } else if pausing && pause.is_some() {
                    ledger.pause.clone_from(&pause);
                } else if pausing || applied.changed {
                    ledger.pause = None;
                }
            }
            let pause_changed = previous_pause != ledger.pause;
            let mut errors = applied.errors.clone();
            if let Some(error) = pause_error {
                errors.push(error);
            }
            let attempted_unknown: Vec<String> =
                if thread.parent_dir.is_some() && !errors.is_empty() {
                    entries
                        .iter()
                        .filter_map(|entry| entry.id.clone())
                        .filter(|id| ledger.resolve(id).is_none())
                        .collect()
                } else {
                    Vec::new()
                };
            let taken: Vec<(usize, String)> = applied
                .outcomes
                .iter()
                .enumerate()
                .filter(|(_, outcome)| !outcome.refused)
                .filter_map(|(at, outcome)| outcome.task.clone().map(|id| (at, id)))
                .collect();
            let mut added = Vec::new();
            let mut updated = Vec::new();
            let mut finished = Vec::new();
            let mut activated = std::collections::BTreeMap::new();
            for (at, id) in &taken {
                let outcome = &applied.outcomes[*at];
                let Some(task) = ledger.find_by_id(id) else {
                    continue;
                };
                let previous = prior.get(id);
                if outcome.created && !added.contains(id) {
                    added.push(id.clone());
                } else if outcome.changed && !updated.contains(id) {
                    updated.push(id.clone());
                }
                if task.status == crate::ledger::Status::Done
                    && previous.is_none_or(|(status, _)| *status != crate::ledger::Status::Done)
                    && !finished.contains(id)
                {
                    finished.push(id.clone());
                }
                if ledger.view(task).state == View::Active
                    && previous.is_none_or(|(_, state)| *state != View::Active)
                    && !activated.contains_key(id)
                {
                    activated.insert(id.clone(), render_recall(ledger, task, ""));
                }
            }
            let detailed: Vec<&String> = added
                .iter()
                .chain(applied.divided.iter().map(|division| &division.task))
                .chain(applied.merged.iter().map(|merge| &merge.survivor))
                .chain(finished.iter())
                .chain(activated.keys())
                .collect();
            let generic_updated: Vec<String> = updated
                .iter()
                .filter(|id| !detailed.contains(id))
                .cloned()
                .collect();
            let mut parts: Vec<String> = Vec::new();
            if !added.is_empty() {
                parts.push(format!("added {}", list(ledger, &added)));
            }
            if !generic_updated.is_empty() {
                parts.push(format!("updated {}", list(ledger, &generic_updated)));
            }
            if !entries.is_empty() && taken.is_empty() && errors.is_empty() {
                parts.push("nothing taken".to_string());
            }
            if !entries.is_empty() && !taken.is_empty() && !applied.changed {
                parts.push("nothing changed".to_string());
            }
            for division in &applied.divided {
                let mut pieces = vec![division.task.clone()];
                pieces.extend(division.into.iter().cloned());
                parts.push(format!("split into {}", list(ledger, &pieces)));
            }
            for merge in &applied.merged {
                parts.push(format!(
                    "merged {} into {}",
                    list(ledger, &merge.absorbed),
                    list(ledger, std::slice::from_ref(&merge.survivor))
                ));
            }
            if !finished.is_empty() {
                parts.push(format!("done {}", list(ledger, &finished)));
            }
            parts.push(ledger.summary());
            let pause_line = if errors.is_empty() && (pausing || pause_changed) {
                Some(match &ledger.pause {
                    Some(reason) => format!("Paused: {reason}"),
                    None => "Resumed.".to_string(),
                })
            } else {
                None
            };
            let changed = errors.is_empty() && (applied.changed || pause_changed);
            let plan = crate::presentation::plan(ledger);
            let plan_update = (changed && previous_plan != plan).then_some(plan);
            (
                Written {
                    plan_update,
                    errors,
                    summary_line: parts.join(" · "),
                    pause_line,
                    activated,
                    attempted_unknown,
                },
                changed,
            )
        })
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("Unable to update the task ledger: {err}"))
        })?;

        let mut lines: Vec<String> = written.errors.clone();
        if !written.attempted_unknown.is_empty()
            && let Some(parent_dir) = &self.thread.parent_dir
            && let Ok(parent) = storage::read(parent_dir).await
        {
            let mut seen: Vec<String> = Vec::new();
            for id in &written.attempted_unknown {
                if let Some(task) = parent.resolve(id)
                    && !seen.contains(&task.id)
                {
                    seen.push(task.id.clone());
                    lines.push(format!(
                        "#{} is in the parent ledger. Child ledgers are separate: use recall [\"parent:{}\"] to read it, then record child-local work here; a child cannot change its parent's tasks.",
                        task.id, task.id
                    ));
                }
            }
        }
        if !written.errors.is_empty() {
            lines.push("No task entry changed because this batch is transactional.".to_string());
        }
        lines.push(written.summary_line);
        if let Some(pause_line) = written.pause_line {
            lines.push(pause_line);
        }
        let mut feedback = lines.join("\n");
        if !written.activated.is_empty() {
            if feedback.len() > 1_024 {
                feedback.truncate(feedback.floor_char_boundary(/*index*/ 1_024));
                feedback.push_str("\nWrite summary shortened; all task records are retained.");
            }
            feedback.push_str("\nStarting tasks. ");
            feedback.push_str(&crate::details::render(
                &written
                    .activated
                    .into_values()
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                /*page*/ 1,
            ));
        }
        Ok(Box::new(TextToolOutput(feedback, written.plan_update)))
    }
}

fn unreadable(err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("Unable to read the task ledger: {err}"))
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
