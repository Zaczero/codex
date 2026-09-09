use crate::ledger::Ledger;
use crate::ledger::View;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;

/// Complete unfinished-task details as Markdown for the human pager.
pub fn open_tasks(ledger: &Ledger) -> String {
    let tasks = ledger.arrange(&ledger.outstanding());
    let summary = [View::Active, View::Ready, View::Blocked]
        .into_iter()
        .filter_map(|state| {
            let count = tasks
                .iter()
                .filter(|task| ledger.view(task).state == state)
                .count();
            (count > 0).then(|| format!("{count} {}", state.label()))
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let mut sections = vec![match ledger.parallelism {
        0 => "**Parallelism:** off".to_string(),
        capacity => format!("**Parallelism:** {capacity}"),
    }];
    if let Some(reason) = &ledger.pause {
        sections.push(format!("**Paused** — {}", inline_text(reason)));
    }
    if tasks.is_empty() {
        sections.push("No unfinished tasks.".to_string());
    } else {
        sections.insert(
            0,
            format!("**{} unfinished tasks** · {summary}", tasks.len()),
        );
        sections.extend(tasks.into_iter().map(|task| {
            let state = ledger.view(task).state;
            let mut parts = vec![format!(
                "## {} #{} {} · `{}`",
                state.glyph(),
                task.id,
                inline_text(&task.title),
                state.label()
            )];
            let mut metadata = Vec::new();
            if let Some(owner) = &task.owner {
                metadata.push(format!("**Owner:** {}", inline_text(owner)));
            }
            if !task.after.is_empty() {
                metadata.push(format!(
                    "**Prerequisites:** {}",
                    task.after
                        .iter()
                        .map(|id| format!("`#{id}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let dependents = ledger
                .tasks
                .iter()
                .filter(|dependent| !dependent.terminal() && dependent.after.contains(&task.id))
                .map(|task| format!("`#{}`", task.id))
                .collect::<Vec<_>>();
            if !dependents.is_empty() {
                metadata.push(format!("**Unblocks:** {}", dependents.join(", ")));
            }
            if !metadata.is_empty() {
                parts.push(metadata.join("  \n"));
            }
            if let Some(waiting) = &task.waiting {
                parts.push(format!("**Waiting:** {}", inline_text(waiting)));
            }
            if !task.body.is_empty() {
                parts.push(task.body.clone());
            }
            if !task.findings.is_empty() {
                parts.push(format!("### Notes ({})", task.findings.len()));
                for finding in &task.findings {
                    let mut caption = crate::ledger::format_timestamp(finding.at);
                    if let Some(origin) = &finding.from {
                        caption.push_str(&format!(" · from #{} {}", origin.task, origin.title));
                    }
                    let text = finding
                        .text
                        .lines()
                        .map(|line| format!("> {line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    parts.push(format!("*{}*\n\n{text}", inline_text(&caption)));
                }
            }
            parts.join("\n\n")
        }));
    }
    sections.join("\n\n")
}

fn inline_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !escaped.is_empty() {
            escaped.push(' ');
        }
        for ch in word.chars() {
            if ch.is_ascii_punctuation() {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

pub(crate) fn plan(ledger: &Ledger) -> UpdatePlanArgs {
    let mut tasks = ledger.tasks.iter().collect::<Vec<_>>();
    tasks.sort_by_key(|task| match ledger.view(task).state {
        View::Active => 0,
        View::Ready => 1,
        View::Blocked => 2,
        View::Done | View::Cancelled => 3,
    });
    let plan = tasks
        .into_iter()
        .take(8)
        .map(|task| {
            let state = ledger.view(task).state;
            let suffix = match state {
                View::Blocked => " (waiting)",
                View::Cancelled => " (cancelled)",
                View::Ready | View::Active | View::Done => "",
            };
            PlanItemArg {
                step: format!("#{} {}{suffix}", task.id, task.title),
                status: match state {
                    View::Active => StepStatus::InProgress,
                    View::Ready | View::Blocked => StepStatus::Pending,
                    View::Done | View::Cancelled => StepStatus::Completed,
                },
            }
        })
        .collect::<Vec<_>>();
    let mut explanation = ledger.summary();
    if let Some(reason) = &ledger.pause {
        explanation = format!(
            "Paused: {} · {explanation}",
            reason.chars().take(160).collect::<String>()
        );
    }
    UpdatePlanArgs {
        explanation: Some(explanation),
        plan,
    }
}
