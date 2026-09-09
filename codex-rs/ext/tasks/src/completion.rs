use crate::ledger::Ledger;
use crate::ledger::View;

#[derive(Default)]
pub(crate) struct ChildCorrection(pub(crate) std::sync::atomic::AtomicBool);

pub(crate) fn reason(ledger: &Ledger, child: bool) -> Option<String> {
    if ledger.pause.is_some() || ledger.outstanding().is_empty() {
        return None;
    }
    let executable = ledger
        .tasks
        .iter()
        .filter(|task| matches!(ledger.view(task).state, View::Active | View::Ready))
        .collect::<Vec<_>>();
    let running = ledger
        .executions
        .iter()
        .filter(|(_, execution)| execution.running)
        .collect::<Vec<_>>();
    if executable.is_empty() && running.is_empty() {
        return None;
    }
    if child {
        return Some("Your ledger holds unfinished work. Finish what is executable. If a premise is false or a decision belongs to the parent, preserve partial work, record the external wait or pause, and report what remains and what unblocks it.".to_owned());
    }
    let mut message = if running.is_empty() {
        "Unfinished work remains. Finish the next coherent batch, review it, and mark accepted work done.".to_owned()
    } else {
        "Delegated work is running. Call wait_agent on its owners or take up independent ready work before finishing.".to_owned()
    };
    if executable.iter().any(|task| {
        task.owner.as_ref().is_some_and(|owner| {
            ledger
                .executions
                .get(owner)
                .is_some_and(|execution| !execution.running)
        })
    }) {
        message.push_str(" Returned worker results need review and integration.");
    }
    message.push_str(" Record external waits on tasks; use ledger pause for a session-wide stop.");
    for task in executable.into_iter().take(3) {
        message.push_str(&format!(
            "\n{} {}: {}",
            ledger.view(task).state.label(),
            task.id,
            task.title
        ));
    }
    Some(message)
}
