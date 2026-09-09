//! The work ledger: one list of task outcomes per thread and the prose each accumulates.
//!
//! Tasks are the whole model. A task is one independently ownable outcome; `after` is the
//! prerequisite graph; a `cancelled` reason is how abandoned work stays visible; `owner` names the
//! native agent path that is executing it. Every write is a batch that either applies whole or
//! leaves the ledger untouched, because a half-applied batch describes a plan nobody wrote.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;

pub const MAX_TITLE_LENGTH: usize = 80;
/// Finished rows kept visible after the open ones.
const TERMINAL_TAIL: usize = 5;
/// Findings previewed for each active task; the rest stay stored for recall.
pub const FINDING_TAIL: usize = 4;
const LINE_LIMIT: usize = 160;
const BODY_PREVIEW_LENGTH: usize = 1_600;
pub(crate) const CONTEXT_BYTES: usize = 24_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Active,
    Done,
}

impl Status {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// Where a finding came from when it was carried across a merge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingOrigin {
    pub task: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Unix milliseconds.
    pub at: i64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<FindingOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: Status,
    /// Stable ids of tasks that must settle before this one can run.
    #[serde(default)]
    pub after: Vec<String>,
    /// Why the task was abandoned. Its presence produces the cancelled view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<String>,
    /// A named external condition or decision this task waits for; not a prerequisite task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
    /// Canonical agent path of the worker executing this task, when it is delegated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    pub created: i64,
    pub updated: i64,
}

impl Task {
    pub fn terminal(&self) -> bool {
        self.status == Status::Done || self.cancelled.is_some()
    }
}

/// What the runtime knows about one delegated worker, keyed by its agent path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub thread_id: String,
    pub running: bool,
    /// The idle cause reported when the worker last stopped: completed, interrupted, or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    pub updated: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// Spawned threads derive their capacity from this root session on every access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_root: Option<codex_protocol::ThreadId>,
    #[serde(default)]
    pub parallelism: usize,
    /// An explicit reason to yield with unfinished work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<String>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub executions: BTreeMap<String, Execution>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum View {
    Ready,
    Active,
    Blocked,
    Done,
    Cancelled,
}

impl View {
    pub fn glyph(self) -> &'static str {
        match self {
            View::Ready => "○",
            View::Active => "●",
            View::Blocked => "◌",
            View::Done => "✓",
            View::Cancelled => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Ready => "ready",
            View::Active => "active",
            View::Blocked => "blocked",
            View::Done => "done",
            View::Cancelled => "cancelled",
        }
    }
}

/// The order these states are worth reading in: in flight, could start, cannot, behind you.
const ORDER: [View; 5] = [
    View::Active,
    View::Ready,
    View::Blocked,
    View::Done,
    View::Cancelled,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewed {
    pub state: View,
    pub note: Option<String>,
}

impl Ledger {
    pub fn find_by_id(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn waiting_for(&self, task: &Task) -> Vec<&Task> {
        task.after
            .iter()
            .filter_map(|id| self.find_by_id(id))
            .filter(|prerequisite| !prerequisite.terminal())
            .collect()
    }

    pub fn view(&self, task: &Task) -> Viewed {
        if let Some(reason) = &task.cancelled {
            return Viewed {
                state: View::Cancelled,
                note: Some(reason.clone()),
            };
        }
        if task.status == Status::Done {
            return Viewed {
                state: View::Done,
                note: None,
            };
        }
        let waiting = self.waiting_for(task);
        if !waiting.is_empty() {
            return Viewed {
                state: View::Blocked,
                note: Some(format!(
                    "waiting for {}",
                    waiting
                        .iter()
                        .map(|item| format!("{} {}", item.id, item.title))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            };
        }
        if let Some(condition) = &task.waiting {
            return Viewed {
                state: View::Blocked,
                note: Some(format!("waiting: {condition}")),
            };
        }
        if task.status == Status::Active {
            return Viewed {
                state: View::Active,
                note: None,
            };
        }
        Viewed {
            state: View::Ready,
            note: None,
        }
    }

    /// Open work is what arms the continuation check.
    pub fn outstanding(&self) -> Vec<&Task> {
        self.tasks.iter().filter(|task| !task.terminal()).collect()
    }

    pub fn counts(&self) -> BTreeMap<View, usize> {
        let mut tally: BTreeMap<View, usize> = BTreeMap::new();
        for task in &self.tasks {
            *tally.entry(self.view(task).state).or_default() += 1;
        }
        tally
    }

    /// The summary shared by context snapshots and tool acknowledgements.
    pub fn summary(&self) -> String {
        let tally = self.counts();
        let count = |view: View| tally.get(&view).copied().unwrap_or(0);
        let active = count(View::Active);
        let free = self.parallelism.saturating_sub(active);
        let mut parts = vec![format!("{active} active")];
        if self.parallelism > 1 {
            parts.push(format!("{free} of {} slots free", self.parallelism));
        }
        parts.push(format!("{} ready", count(View::Ready)));
        if count(View::Blocked) > 0 {
            parts.push(format!("{} blocked", count(View::Blocked)));
        }
        if count(View::Done) > 0 {
            parts.push(format!("{} done", count(View::Done)));
        }
        parts.join(" · ")
    }

    /// Stable topological order among `tasks`; plan order breaks ties.
    fn topological<'a>(&'a self, tasks: &[&'a Task]) -> Result<Vec<&'a Task>, String> {
        let included: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
        let mut remaining: BTreeSet<&str> = included.iter().copied().collect();
        let candidates: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|task| included.contains(task.id.as_str()))
            .collect();
        let mut ordered = Vec::with_capacity(candidates.len());
        while !remaining.is_empty() {
            let next = candidates.iter().find(|task| {
                remaining.contains(task.id.as_str())
                    && task.after.iter().all(|id| {
                        !included.contains(id.as_str()) || !remaining.contains(id.as_str())
                    })
            });
            let Some(next) = next else {
                return Err(format!(
                    "task dependency cycle: {}.",
                    self.cycle_route(&remaining)
                ));
            };
            remaining.remove(next.id.as_str());
            ordered.push(*next);
        }
        Ok(ordered)
    }

    fn cycle_route(&self, remaining: &BTreeSet<&str>) -> String {
        fn walk<'a>(
            ledger: &'a Ledger,
            remaining: &BTreeSet<&str>,
            task: &'a Task,
            path: &mut Vec<&'a Task>,
            visited: &mut HashSet<&'a str>,
        ) -> Option<Vec<&'a Task>> {
            if let Some(at) = path.iter().position(|item| item.id == task.id) {
                let mut cycle = path[at..].to_vec();
                cycle.push(task);
                return Some(cycle);
            }
            if !visited.insert(task.id.as_str()) {
                return None;
            }
            path.push(task);
            for id in &task.after {
                if !remaining.contains(id.as_str()) {
                    continue;
                }
                if let Some(prerequisite) = ledger.find_by_id(id)
                    && let Some(found) = walk(ledger, remaining, prerequisite, path, visited)
                {
                    return Some(found);
                }
            }
            path.pop();
            None
        }
        let mut visited = HashSet::new();
        for task in self
            .tasks
            .iter()
            .filter(|task| remaining.contains(task.id.as_str()))
        {
            let mut path = Vec::new();
            if let Some(cycle) = walk(self, remaining, task, &mut path, &mut visited) {
                return cycle
                    .iter()
                    .map(|item| format!("{} {}", item.id, item.title))
                    .collect::<Vec<_>>()
                    .join(" -> ");
            }
        }
        remaining.iter().copied().collect::<Vec<_>>().join(" -> ")
    }

    /// Operational priority first, graph order second.
    pub fn arrange<'a>(&'a self, tasks: &[&'a Task]) -> Vec<&'a Task> {
        let ordered = self.topological(tasks).unwrap_or_else(|_| tasks.to_vec());
        ORDER
            .iter()
            .flat_map(|state| {
                ordered
                    .iter()
                    .copied()
                    .filter(|task| self.view(task).state == *state)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn next_id_number(&self) -> u64 {
        self.tasks
            .iter()
            .filter_map(|task| task.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1
    }

    fn next_id(&self) -> String {
        self.next_id_number().to_string()
    }

    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    fn titled(&self, reference: &str) -> Vec<&Task> {
        let wanted = fold(reference);
        self.tasks
            .iter()
            .filter(|task| fold(&task.title) == wanted)
            .collect()
    }

    /// A reference to a task: its id, or its title. Id wins; among tasks sharing a title the
    /// live one wins, so a reference written now points at the work still to do.
    pub fn resolve(&self, reference: &str) -> Option<&Task> {
        let wanted = self.normalized_reference(reference);
        if let Some(task) = self.find_by_id(&wanted) {
            return Some(task);
        }
        let named = self.titled(&wanted);
        named
            .iter()
            .copied()
            .find(|task| !task.terminal())
            .or_else(|| named.first().copied())
    }

    /// Display labels may be stale: `12 Title` names task 12 only when 12 is an id here.
    fn normalized_reference(&self, reference: &str) -> String {
        let wanted = reference.trim();
        if let Some((head, rest)) = wanted.split_once(char::is_whitespace)
            && head.chars().all(|c| c.is_ascii_digit())
            && !rest.trim().is_empty()
            && self.find_by_id(head).is_some()
        {
            return head.to_string();
        }
        wanted.to_string()
    }
}

/// A name as it will be stored and matched: one line, trimmed.
fn name(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fold(raw: &str) -> String {
    name(raw).to_lowercase()
}

fn title_problem(title: &str) -> Option<String> {
    if title.is_empty() {
        return Some("a new task needs a title.".to_string());
    }
    if title.chars().all(|c| c.is_ascii_digit()) {
        return Some(format!(
            "{title} cannot be a title — ids look like that, so nothing could refer to it."
        ));
    }
    let length = title.chars().count();
    if length > MAX_TITLE_LENGTH {
        return Some(format!(
            "the title is {length} characters; titles must be at most {MAX_TITLE_LENGTH}. Put detail in body."
        ));
    }
    None
}

// -- writes -------------------------------------------------------------------------------------

/// One entry as the model wrote it, already narrowed to typed fields. `Some(None)` on a
/// clearable field means "clear it"; `None` means "leave it alone".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Set when the wire value was not an object; refused with this message.
    pub invalid: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    /// Complete prerequisite set.
    pub after: Option<Option<Vec<String>>>,
    /// Complete direct-dependent set.
    pub before: Option<Option<Vec<String>>>,
    pub cancelled: Option<Option<String>>,
    pub waiting: Option<Option<String>>,
    pub owner: Option<Option<String>>,
    /// The pieces a task turned out to be. The first keeps the row, id and findings.
    pub split: Option<Vec<String>>,
    /// Tasks whose outcome this one absorbed; their findings and dependents move here.
    pub merge: Option<Vec<String>>,
    /// Replaces the task body. An explicitly empty body clears it.
    pub body: Option<String>,
    /// Appends one finding, preserving its text exactly.
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub task: Option<String>,
    pub created: bool,
    pub refused: bool,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Division {
    pub task: String,
    pub from: String,
    pub into: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Absorption {
    pub survivor: String,
    pub absorbed: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub errors: Vec<String>,
    /// Aligned with the entries.
    pub outcomes: Vec<Outcome>,
    pub divided: Vec<Division>,
    pub merged: Vec<Absorption>,
    pub changed: bool,
}

impl Ledger {
    /// Apply a batch. Every rejection names the entry, and any rejection leaves the ledger as it
    /// was. Relations, state, and prose commit together.
    pub fn upsert(&mut self, entries: &[Entry], now: i64) -> Applied {
        let before = self.clone();
        let mut applied = self.upsert_inner(entries, now);
        if !applied.errors.is_empty() {
            *self = before;
            applied.outcomes = entries
                .iter()
                .map(|_| Outcome {
                    refused: true,
                    ..Outcome::default()
                })
                .collect();
            applied.divided.clear();
            applied.merged.clear();
            applied.changed = false;
        }
        applied
    }

    fn upsert_inner(&mut self, entries: &[Entry], now: i64) -> Applied {
        let mut applied = Applied {
            outcomes: entries.iter().map(|_| Outcome::default()).collect(),
            ..Applied::default()
        };
        let previously_active = self
            .tasks
            .iter()
            .filter(|task| self.view(task).state == View::Active)
            .count();
        let label = |position: usize, entry: &Entry| match &entry.id {
            Some(id) => format!("#{id}"),
            None => format!("entry {}", position + 1),
        };

        // Pass one: create or resolve every task first, so a plan can name a task added later
        // in the same call.
        for (position, entry) in entries.iter().enumerate() {
            let where_ = label(position, entry);
            let refuse = |applied: &mut Applied, message: String| {
                applied.errors.push(message);
                applied.outcomes[position].refused = true;
            };
            if let Some(invalid) = &entry.invalid {
                refuse(&mut applied, format!("{where_}: {invalid}"));
                continue;
            }
            if let Some(status) = &entry.status
                && Status::parse(status).is_none()
            {
                refuse(
                    &mut applied,
                    format!("{where_}: {status} is not a status — use pending, active, done."),
                );
                continue;
            }
            let existing: Option<String> = if let Some(id) = &entry.id {
                match self.resolve(id) {
                    Some(task) => Some(task.id.clone()),
                    None => {
                        let highest = self
                            .tasks
                            .iter()
                            .filter_map(|task| task.id.parse::<u64>().ok())
                            .max()
                            .unwrap_or(0);
                        let range = if highest > 0 {
                            format!(" The ids here run 1 to {highest}.")
                        } else {
                            String::new()
                        };
                        refuse(
                            &mut applied,
                            format!(
                                "{where_}: no such task, and nothing here is called that.{range}"
                            ),
                        );
                        continue;
                    }
                }
            } else if let Some(title) = &entry.title {
                let named = self.titled(title);
                let live: Vec<&Task> = named
                    .iter()
                    .copied()
                    .filter(|task| !task.terminal())
                    .collect();
                if live.len() > 1 {
                    refuse(
                        &mut applied,
                        format!(
                            "{where_}: {} are both called that. Name one by id, or give this one a title of its own.",
                            live.iter()
                                .map(|task| format!("#{}", task.id))
                                .collect::<Vec<_>>()
                                .join(" and ")
                        ),
                    );
                    continue;
                }
                live.first()
                    .or(named.first())
                    .map(|task| task.id.clone())
                    .or_else(|| {
                        applied
                            .divided
                            .iter()
                            .find(|division| fold(&division.from) == fold(title))
                            .map(|division| division.task.clone())
                    })
            } else {
                None
            };

            if let Some(id) = existing {
                let mut structural = false;
                if let Some(pieces) = &entry.split {
                    match self.divide(&id, pieces, now) {
                        Ok(division) => {
                            applied.divided.push(division);
                            structural = true;
                        }
                        Err(message) => {
                            refuse(&mut applied, format!("{where_}: {message}"));
                            continue;
                        }
                    }
                }
                if let Some(sources) = &entry.merge {
                    match self.absorb(&id, sources, now) {
                        Ok(absorption) => {
                            applied.merged.push(absorption);
                            structural = true;
                        }
                        Err(message) => {
                            refuse(&mut applied, format!("{where_}: {message}"));
                            continue;
                        }
                    }
                }
                applied.outcomes[position] = Outcome {
                    task: Some(id),
                    changed: structural,
                    ..Outcome::default()
                };
                if structural {
                    applied.changed = true;
                }
                continue;
            }

            if entry.split.is_some() {
                refuse(
                    &mut applied,
                    format!(
                        "{where_}: split names the pieces of a task that is already here. Give the id or title of the one that turned out to be several."
                    ),
                );
                continue;
            }
            if entry.merge.is_some() {
                refuse(
                    &mut applied,
                    format!(
                        "{where_}: merge absorbs other tasks into one that is already here. Give the id or title of the survivor."
                    ),
                );
                continue;
            }
            let fresh = entry.title.as_deref().map(name).unwrap_or_default();
            if let Some(problem) = title_problem(&fresh) {
                refuse(&mut applied, format!("{where_}: {problem}"));
                continue;
            }
            let id = self.next_id();
            self.tasks.push(Task {
                id: id.clone(),
                title: fresh,
                status: Status::Pending,
                after: Vec::new(),
                cancelled: None,
                waiting: None,
                owner: None,
                body: String::new(),
                findings: Vec::new(),
                created: now,
                updated: now,
            });
            applied.outcomes[position] = Outcome {
                task: Some(id),
                created: true,
                changed: true,
                refused: false,
            };
            applied.changed = true;
        }

        // Pass two: fields.
        for (position, entry) in entries.iter().enumerate() {
            let Some(id) = applied.outcomes[position].task.clone() else {
                continue;
            };
            if applied.outcomes[position].refused {
                continue;
            }
            let where_ = label(position, entry);
            let by_id = entry.id.as_deref() == Some(id.as_str());
            let current_title = self
                .find_by_id(&id)
                .map(|task| task.title.clone())
                .unwrap_or_default();
            let named = match (&entry.split, &entry.title) {
                (Some(_), _) | (_, None) => None,
                (None, Some(title)) => Some(name(title)),
            };
            let names_it = named.as_ref().is_some_and(|named| {
                fold(named) == fold(&current_title)
                    || applied
                        .divided
                        .iter()
                        .any(|division| division.task == id && fold(&division.from) == fold(named))
            });
            if let Some(named) = &named
                && !by_id
                && !names_it
            {
                applied.errors.push(format!("{where_}: to rename it to {named}, give its id ({id}) — a title on its own names a task rather than changing one."));
                applied.outcomes[position].refused = true;
                continue;
            }
            let renamed = if by_id { named.clone() } else { None };
            if let Some(renamed) = &renamed {
                if let Some(problem) = title_problem(renamed) {
                    applied.errors.push(format!("{where_}: {problem}"));
                    applied.outcomes[position].refused = true;
                    continue;
                }
                if applied.divided.iter().any(|division| division.task == id) {
                    applied.errors.push(format!("{where_}: it was split earlier in this call, so renaming it to {renamed} would leave the other pieces orphaned."));
                    applied.outcomes[position].refused = true;
                    continue;
                }
                if self
                    .titled(renamed)
                    .iter()
                    .any(|other| other.id != id && !other.terminal())
                {
                    applied.errors.push(format!("{where_}: another task is already called {renamed}. Two live tasks with one name cannot be told apart afterwards."));
                    applied.outcomes[position].refused = true;
                    continue;
                }
            }
            let status = entry.status.as_deref().and_then(Status::parse);
            let Some(task) = self.task_mut(&id) else {
                continue;
            };
            let mut entry_changed = false;
            if let Some(renamed) = renamed
                && task.title != renamed
            {
                task.title = renamed;
                entry_changed = true;
            }
            if let Some(status) = status
                && task.status != status
            {
                task.status = status;
                entry_changed = true;
            }
            match &entry.cancelled {
                Some(Some(reason)) if !reason.trim().is_empty() => {
                    let reason = reason.trim().to_string();
                    if task.status != Status::Pending || task.cancelled.as_deref() != Some(&reason)
                    {
                        entry_changed = true;
                    }
                    task.status = Status::Pending;
                    task.cancelled = Some(reason);
                }
                Some(_) => {
                    if task.cancelled.take().is_some() {
                        entry_changed = true;
                    }
                }
                None => {
                    if status.is_some() && task.cancelled.take().is_some() {
                        entry_changed = true;
                    }
                }
            }
            if let Some(waiting) = &entry.waiting {
                let next = waiting
                    .as_ref()
                    .map(|w| w.trim().to_string())
                    .filter(|w| !w.is_empty());
                if task.waiting != next {
                    task.waiting = next;
                    entry_changed = true;
                }
            } else if matches!(status, Some(Status::Done)) && task.waiting.take().is_some() {
                entry_changed = true;
            }
            if let Some(owner) = &entry.owner {
                let next = owner
                    .as_ref()
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty());
                if task.owner != next {
                    task.owner = next;
                    entry_changed = true;
                }
            }
            if let Some(body) = &entry.body
                && task.body != *body
            {
                task.body.clone_from(body);
                entry_changed = true;
            }
            if let Some(note) = &entry.note
                && !note.trim().is_empty()
            {
                task.findings.push(Finding {
                    at: now,
                    text: note.clone(),
                    from: None,
                });
                entry_changed = true;
            }
            if entry_changed {
                task.updated = now;
                applied.changed = true;
                applied.outcomes[position].changed = true;
            }
        }

        // Pass three: relations, once every task in the batch exists.
        for (position, entry) in entries.iter().enumerate() {
            let Some(id) = applied.outcomes[position].task.clone() else {
                continue;
            };
            if applied.outcomes[position].refused {
                continue;
            }
            let where_ = label(position, entry);
            let mut relation_changed = false;
            if let Some(after) = &entry.after {
                let resolved = match after {
                    None => Some(Vec::new()),
                    Some(references) => {
                        self.resolve_all(&id, references, "after", &where_, &mut applied.errors)
                    }
                };
                let Some(ids) = resolved else {
                    applied.outcomes[position].refused = true;
                    continue;
                };
                if let Some(task) = self.task_mut(&id)
                    && task.after != ids
                {
                    task.after = ids;
                    relation_changed = true;
                }
            }
            if let Some(before) = &entry.before {
                let resolved = match before {
                    None => Some(Vec::new()),
                    Some(references) => {
                        self.resolve_all(&id, references, "before", &where_, &mut applied.errors)
                    }
                };
                let Some(ids) = resolved else {
                    applied.outcomes[position].refused = true;
                    continue;
                };
                let wanted: HashSet<String> = ids.into_iter().collect();
                for candidate in self.tasks.iter_mut().filter(|candidate| candidate.id != id) {
                    let has = candidate.after.contains(&id);
                    let should = wanted.contains(&candidate.id);
                    if has == should {
                        continue;
                    }
                    if should {
                        candidate.after.push(id.clone());
                    } else {
                        candidate.after.retain(|item| item != &id);
                    }
                    candidate.updated = now;
                    relation_changed = true;
                }
            }
            if relation_changed {
                if let Some(task) = self.task_mut(&id) {
                    task.updated = now;
                }
                applied.changed = true;
                applied.outcomes[position].changed = true;
            }
        }

        if applied.errors.is_empty() {
            let all: Vec<&Task> = self.tasks.iter().collect();
            match self.topological(&all) {
                Ok(ordered) => {
                    let order: Vec<String> = ordered.iter().map(|task| task.id.clone()).collect();
                    self.tasks
                        .sort_by_key(|task| order.iter().position(|id| id == &task.id));
                }
                Err(message) => applied.errors.push(message),
            }
            let requested_active: HashSet<String> = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.status.as_deref() == Some("active"))
                .filter_map(|(at, _)| applied.outcomes[at].task.clone())
                .collect();
            let demoted: Vec<String> = self
                .tasks
                .iter()
                .filter(|task| task.status == Status::Active && !self.waiting_for(task).is_empty())
                .map(|task| task.id.clone())
                .collect();
            for id in demoted {
                if requested_active.contains(&id) {
                    applied.errors.push(format!(
                        "#{id}: an active task cannot wait for unfinished prerequisites."
                    ));
                } else if let Some(task) = self.task_mut(&id) {
                    task.status = Status::Pending;
                    task.updated = now;
                    applied.changed = true;
                }
            }
            let running: Vec<&Task> = self
                .tasks
                .iter()
                .filter(|task| self.view(task).state == View::Active)
                .collect();
            let admitted = self.parallelism.max(previously_active);
            if self.parallelism > 0 && running.len() > admitted {
                applied.errors.push(format!(
                    "{admitted} of {} task slots are already busy ({}). Finish one, set one back to pending, or ask the user to use /parallelism.",
                    self.parallelism,
                    running
                        .iter()
                        .take(admitted)
                        .map(|task| task.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        applied
    }

    fn resolve_all(
        &self,
        task_id: &str,
        references: &[String],
        side: &str,
        where_: &str,
        errors: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        let mut resolved: Vec<String> = Vec::new();
        for reference in references {
            let Some(found) = self.resolve(reference) else {
                errors.push(format!("{where_}: {side} names unknown task {reference}."));
                return None;
            };
            if found.id == task_id {
                errors.push(format!("{where_}: a task cannot run {side} itself."));
                return None;
            }
            if !resolved.contains(&found.id) {
                resolved.push(found.id.clone());
            }
        }
        Some(resolved)
    }

    /// One task that turned out to be several. The row is kept and retitled to the first piece so
    /// its id, findings, and place in the plan survive the discovery; the pieces go right after it.
    fn divide(&mut self, id: &str, pieces: &[String], now: i64) -> Result<Division, String> {
        let titles: Vec<String> = pieces
            .iter()
            .map(|piece| name(piece))
            .filter(|t| !t.is_empty())
            .collect();
        if titles.len() < 2 {
            return Err(
                "splitting takes at least two pieces — for one, just change the title.".to_string(),
            );
        }
        if let Some(problem) = titles.iter().find_map(|title| title_problem(title)) {
            return Err(problem);
        }
        let clash: Vec<&String> = titles
            .iter()
            .enumerate()
            .filter(|(at, title)| {
                titles.iter().position(|other| fold(other) == fold(title)) != Some(*at)
                    || self.titled(title).iter().any(|other| other.id != id)
            })
            .map(|(_, title)| title)
            .collect();
        if !clash.is_empty() {
            return Err(format!(
                "{} already names another task — every piece needs its own.",
                clash
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let Some(index) = self.tasks.iter().position(|task| task.id == id) else {
            return Err(format!("no such task #{id}."));
        };
        let from = self.tasks[index].title.clone();
        let after = self.tasks[index].after.clone();
        let next = self.next_id_number();
        let mut into = Vec::new();
        let mut created = Vec::new();
        for (offset, title) in titles[1..].iter().enumerate() {
            let piece_id = (next + offset as u64).to_string();
            into.push(piece_id.clone());
            created.push(Task {
                id: piece_id,
                title: title.clone(),
                status: Status::Pending,
                after: after.clone(),
                cancelled: None,
                waiting: None,
                owner: None,
                body: String::new(),
                findings: Vec::new(),
                created: now,
                updated: now,
            });
        }
        {
            let task = &mut self.tasks[index];
            task.title = titles[0].clone();
            task.updated = now;
            task.findings.push(Finding {
                at: now,
                text: format!(
                    "Split from \"{from}\"; the rest became {}.",
                    created
                        .iter()
                        .map(|piece| format!("{} {}", piece.id, piece.title))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                from: None,
            });
        }
        for dependent in self.tasks.iter_mut() {
            if dependent.after.iter().any(|item| item == id) {
                for piece in &into {
                    if !dependent.after.contains(piece) {
                        dependent.after.push(piece.clone());
                    }
                }
            }
        }
        let splice_at = index + 1;
        self.tasks.splice(splice_at..splice_at, created);
        Ok(Division {
            task: id.to_string(),
            from,
            into,
        })
    }

    /// Several tasks that turned out to be one outcome. The survivor keeps its identity; the
    /// absorbed rows stay as cancelled references so their ids still resolve to their history,
    /// their findings move over with provenance, and their dependents and prerequisites are
    /// rewired to the survivor. Unfinished work absorbed into a finished survivor reopens it.
    fn absorb(
        &mut self,
        survivor_id: &str,
        sources: &[String],
        now: i64,
    ) -> Result<Absorption, String> {
        let mut absorbed: Vec<String> = Vec::new();
        for reference in sources {
            let Some(found) = self.resolve(reference) else {
                return Err(format!("merge names unknown task {reference}."));
            };
            if found.id == survivor_id {
                return Err("a task cannot absorb itself.".to_string());
            }
            if found.cancelled.is_some() {
                return Err(format!(
                    "#{} is already cancelled; merge live or finished work, not abandoned work.",
                    found.id
                ));
            }
            if !absorbed.contains(&found.id) {
                absorbed.push(found.id.clone());
            }
        }
        if absorbed.is_empty() {
            return Err("merge takes at least one other task.".to_string());
        }
        let survivor_owner = self
            .find_by_id(survivor_id)
            .and_then(|task| task.owner.clone());
        for id in &absorbed {
            let Some(source) = self.find_by_id(id) else {
                continue;
            };
            if let Some(owner) = &source.owner
                && Some(owner) != survivor_owner.as_ref()
                && self
                    .executions
                    .get(owner)
                    .is_some_and(|execution| execution.running)
            {
                return Err(format!(
                    "#{id} is still being executed by {owner}; settle that worker before merging its outcome into another task."
                ));
            }
        }
        // Validate the contracted graph before touching anything.
        let mut trial = self.clone();
        trial.rewire_merge(survivor_id, &absorbed, now);
        let all: Vec<&Task> = trial.tasks.iter().collect();
        trial.topological(&all)?;
        self.rewire_merge(survivor_id, &absorbed, now);
        Ok(Absorption {
            survivor: survivor_id.to_string(),
            absorbed,
        })
    }

    fn rewire_merge(&mut self, survivor_id: &str, absorbed: &[String], now: i64) {
        let survivor_title = self
            .find_by_id(survivor_id)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        let mut carried: Vec<Finding> = Vec::new();
        let mut prerequisites: Vec<String> = Vec::new();
        let mut reopen = false;
        for id in absorbed {
            let Some(source) = self.find_by_id(id).cloned() else {
                continue;
            };
            if !source.terminal() {
                reopen = true;
            }
            for finding in &source.findings {
                carried.push(Finding {
                    at: finding.at,
                    text: finding.text.clone(),
                    from: Some(finding.from.clone().unwrap_or(FindingOrigin {
                        task: source.id.clone(),
                        title: source.title.clone(),
                    })),
                });
            }
            for prerequisite in &source.after {
                if !prerequisites.contains(prerequisite) {
                    prerequisites.push(prerequisite.clone());
                }
            }
        }
        for dependent in self.tasks.iter_mut() {
            if dependent.id == survivor_id {
                continue;
            }
            let had_any = dependent.after.iter().any(|item| absorbed.contains(item));
            if had_any {
                dependent.after.retain(|item| !absorbed.contains(item));
                if dependent.id != survivor_id
                    && !dependent.after.contains(&survivor_id.to_string())
                {
                    dependent.after.push(survivor_id.to_string());
                }
                dependent.updated = now;
            }
        }
        for id in absorbed {
            let Some(source) = self.task_mut(id) else {
                continue;
            };
            source.status = Status::Pending;
            source.cancelled = Some(format!("merged into {survivor_id} {survivor_title}"));
            source.owner = None;
            source.waiting = None;
            source.updated = now;
        }
        let Some(survivor) = self.task_mut(survivor_id) else {
            return;
        };
        for prerequisite in prerequisites {
            if prerequisite != survivor_id
                && !absorbed.contains(&prerequisite)
                && !survivor.after.contains(&prerequisite)
            {
                survivor.after.push(prerequisite);
            }
        }
        survivor.after.retain(|item| !absorbed.contains(item));
        survivor.findings.extend(carried);
        survivor.findings.sort_by_key(|finding| finding.at);
        if reopen && survivor.status == Status::Done {
            survivor.status = Status::Pending;
        }
        survivor.updated = now;
    }
}

// -- rendering ----------------------------------------------------------------------------------

pub fn format_timestamp(at: i64) -> String {
    let seconds = at.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant), enough for a UTC minute stamp without a date crate.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn clamp(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > limit {
        let mut cut: String = chars[..limit.saturating_sub(1)].iter().collect();
        let trimmed = cut.trim_end().len();
        cut.truncate(trimmed);
        cut.push('…');
        cut
    } else {
        text.to_string()
    }
}

fn one_line(text: &str, limit: usize) -> String {
    clamp(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        limit,
    )
}

/// The bounded finding previews carried with active work: the latest few, one line each.
pub fn finding_previews(task: &Task) -> Vec<String> {
    let skip = task.findings.len().saturating_sub(FINDING_TAIL);
    task.findings
        .iter()
        .skip(skip)
        .map(|finding| {
            let origin = finding
                .from
                .as_ref()
                .map(|from| format!(" (from {} {})", from.task, from.title))
                .unwrap_or_default();
            format!(
                "{} · {}{origin}",
                format_timestamp(finding.at),
                one_line(&finding.text, LINE_LIMIT)
            )
        })
        .collect()
}

pub(crate) fn finding_context(task: &Task) -> String {
    let previews = finding_previews(task);
    if previews.is_empty() {
        return String::new();
    }
    let shown = previews.len();
    let total = task.findings.len();
    let mut lines = vec![format!(
        "Findings: {shown} of {total} shown (previews). Recall {} for complete context and notes.",
        task.id
    )];
    lines.extend(previews.into_iter().map(|preview| format!("- {preview}")));
    lines.join("\n")
}

pub(crate) fn task_context(task: &Task) -> String {
    let mut body = clamp(&task.body, BODY_PREVIEW_LENGTH);
    if body != task.body {
        body.push_str(&format!(
            "\nBody shortened. Recall {} for complete context.",
            task.id
        ));
    }
    let findings = finding_context(task);
    match (body.is_empty(), findings.is_empty()) {
        (false, false) => format!("{body}\n{findings}"),
        (false, true) => body,
        (true, _) => findings,
    }
}

fn bounded_lines(text: &str, max_lines: usize, max_chars: usize) -> Vec<String> {
    let source: Vec<&str> = text.lines().collect();
    let shown: Vec<String> = source
        .iter()
        .take(max_lines)
        .map(|line| clamp(line, LINE_LIMIT))
        .collect();
    let mut output = clamp(&shown.join("\n"), max_chars);
    let truncated = source.len() > shown.len()
        || shown
            .iter()
            .zip(source.iter())
            .any(|(line, original)| line != original);
    if truncated {
        output = clamp(&format!("{output}…"), max_chars);
    }
    if output.is_empty() {
        Vec::new()
    } else {
        output.lines().map(str::to_string).collect()
    }
}

/// Everything still open plus the most recently settled few.
pub fn visible(ledger: &Ledger, tail: usize) -> (Vec<&Task>, usize) {
    let finished: Vec<(usize, &Task)> = ledger
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.terminal())
        .collect();
    let mut recent = finished.clone();
    recent.sort_by(|(ai, a), (bi, b)| b.updated.cmp(&a.updated).then(bi.cmp(ai)));
    let keep: HashSet<&str> = recent
        .iter()
        .take(tail)
        .map(|(_, task)| task.id.as_str())
        .collect();
    let rows = ledger
        .tasks
        .iter()
        .filter(|task| !task.terminal() || keep.contains(task.id.as_str()))
        .collect();
    (rows, finished.len().saturating_sub(keep.len()))
}

/// Full rows for open work, prose only for what is being worked right now, findings as bounded
/// previews with a recall pointer, and terminal rows collapsed to a tail.
pub fn render(ledger: &Ledger) -> String {
    let mut lines = vec![match ledger.parallelism {
        0 => "Task parallelism: off. No capacity limits or completion reminders.".to_owned(),
        capacity => format!("Task parallelism: {capacity} independent active outcomes."),
    }];
    if let Some(pause) = &ledger.pause {
        lines.push(format!("Paused: {pause}"));
    }
    let (rows, _) = visible(ledger, TERMINAL_TAIL);
    let open_rows: Vec<&Task> = rows
        .iter()
        .copied()
        .filter(|task| !task.terminal())
        .collect();
    let open = ledger.arrange(&open_rows);
    if !open.is_empty() {
        lines.push(format!("Open ({})", open.len()));
    }
    for task in &open {
        let viewed = ledger.view(task);
        let owner = task
            .owner
            .as_ref()
            .map(|owner| format!(" @{owner}"))
            .unwrap_or_default();
        let note = viewed
            .note
            .as_ref()
            .map(|note| format!(" — {note}"))
            .unwrap_or_default();
        lines.push(format!(
            "{} {} {}{owner}{note}",
            viewed.state.glyph(),
            task.id,
            task.title
        ));
        if viewed.state == View::Active {
            for line in task_context(task).lines() {
                lines.push(format!("    {line}"));
            }
        } else if viewed.state == View::Ready && !task.body.is_empty() {
            for line in bounded_lines(&task.body, 2, 240) {
                lines.push(format!("    {line}"));
            }
        }
    }
    let recent: Vec<&Task> = rows
        .iter()
        .copied()
        .filter(|task| task.terminal())
        .collect();
    if !recent.is_empty() {
        lines.push(format!("Recent ({})", recent.len()));
        for task in recent {
            lines.push(format!(
                "{} [{}] {}",
                task.id,
                ledger.view(task).state.label(),
                task.title
            ));
        }
    }
    if !ledger.executions.is_empty() {
        let mut workers = Vec::new();
        for (owner, execution) in &ledger.executions {
            if !execution.running && !open.iter().any(|task| task.owner.as_ref() == Some(owner)) {
                continue;
            }
            let state = if execution.running {
                "running".to_string()
            } else {
                execution
                    .outcome
                    .clone()
                    .unwrap_or_else(|| "settled".to_string())
            };
            workers.push(format!("{owner} {state}"));
        }
        if !workers.is_empty() {
            lines.push(format!("Workers: {}", workers.join(" · ")));
        }
    }
    let summary = ledger.summary();
    let mut rendered = lines.join("\n");
    let suffix = format!(
        "\nOverview shortened. Read the ledger file to locate omitted tasks; recall named tasks for complete notes.\n{summary}"
    );
    if rendered.len() + summary.len() + 1 > CONTEXT_BYTES {
        let mut end = CONTEXT_BYTES - suffix.len();
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        if let Some(line_end) = rendered[..end].rfind('\n') {
            end = line_end;
        }
        rendered.truncate(end);
        rendered.push_str(&suffix);
    } else {
        rendered.push('\n');
        rendered.push_str(&summary);
    }
    rendered
}

/// Complete detail for one task, the answer to a recall.
pub fn render_recall(ledger: &Ledger, task: &Task, prefix: &str) -> String {
    let viewed = ledger.view(task);
    let mut chunk = vec![
        format!("# {prefix}{} {}", task.id, task.title),
        format!("State: [{}]", viewed.state.label()),
    ];
    if let Some(owner) = &task.owner {
        chunk.push(format!("Owner: {owner}"));
    }
    if !task.after.is_empty() {
        chunk.push(format!(
            "Prerequisites: {}",
            task.after
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(waiting) = &task.waiting {
        chunk.push(format!("Waiting: {waiting}"));
    }
    if let Some(cancelled) = &task.cancelled {
        chunk.push(format!("Cancelled by: {cancelled}"));
    }
    if !task.body.is_empty() {
        chunk.push(format!("Context:\n{}", task.body));
    }
    if !task.findings.is_empty() {
        let findings: Vec<String> = task
            .findings
            .iter()
            .map(|finding| {
                let origin = finding
                    .from
                    .as_ref()
                    .map(|from| format!(" (from {} {})", from.task, from.title))
                    .unwrap_or_default();
                format!(
                    "- {} ·{origin} {}",
                    format_timestamp(finding.at),
                    finding.text
                )
            })
            .collect();
        chunk.push(format!("Findings:\n{}", findings.join("\n")));
    }
    chunk.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str) -> Entry {
        Entry {
            title: Some(title.to_string()),
            ..Entry::default()
        }
    }

    fn plan() -> Ledger {
        let mut ledger = Ledger::default();
        let applied = ledger.upsert(
            &[
                entry("Design the seam"),
                Entry {
                    after: Some(Some(vec!["Design the seam".to_string()])),
                    ..entry("Implement it")
                },
                Entry {
                    after: Some(Some(vec!["Implement it".to_string()])),
                    ..entry("Validate it")
                },
            ],
            1_000,
        );
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        ledger
    }

    #[test]
    fn titles_resolve_and_ids_are_stable() {
        let ledger = plan();
        assert_eq!(
            ledger.resolve("implement it").map(|t| t.id.as_str()),
            Some("2")
        );
        assert_eq!(
            ledger.resolve("2 Implement it").map(|t| t.id.as_str()),
            Some("2")
        );
        assert_eq!(ledger.view(&ledger.tasks[1]).state, View::Blocked);
        assert_eq!(ledger.view(&ledger.tasks[0]).state, View::Ready);
    }

    #[test]
    fn batches_are_atomic_and_cycles_are_refused() {
        let mut ledger = plan();
        let before = ledger.clone();
        let applied = ledger.upsert(
            &[
                Entry {
                    id: Some("1".to_string()),
                    after: Some(Some(vec!["3".to_string()])),
                    ..Entry::default()
                },
                entry("Unrelated"),
            ],
            2_000,
        );
        assert!(
            applied
                .errors
                .iter()
                .any(|e| e.contains("dependency cycle")),
            "{:?}",
            applied.errors
        );
        assert_eq!(ledger, before);
        assert!(applied.outcomes.iter().all(|o| o.refused));
    }

    #[test]
    fn capacity_refuses_a_fifth_active_outcome() {
        let mut ledger = Ledger {
            parallelism: 4,
            ..Ledger::default()
        };
        let entries: Vec<Entry> = (1..=5)
            .map(|n| Entry {
                status: Some("active".to_string()),
                ..entry(&format!("Outcome {n}"))
            })
            .collect();
        let applied = ledger.upsert(&entries, 1);
        assert!(
            applied
                .errors
                .iter()
                .any(|e| e.contains("task slots are already busy"))
        );
        assert!(ledger.tasks.is_empty());
        let applied = ledger.upsert(&entries[..4], 1);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    }

    #[test]
    fn split_keeps_history_and_rewires_dependents() {
        let mut ledger = plan();
        let applied = ledger.upsert(
            &[Entry {
                id: Some("2".to_string()),
                split: Some(vec![
                    "Implement storage".to_string(),
                    "Implement tool".to_string(),
                ]),
                ..Entry::default()
            }],
            3_000,
        );
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert_eq!(ledger.tasks[1].title, "Implement storage");
        assert_eq!(ledger.tasks[2].title, "Implement tool");
        assert_eq!(ledger.tasks[2].after, vec!["1".to_string()]);
        let validate = ledger.find_by_id("3").unwrap();
        assert_eq!(validate.after, vec!["2".to_string(), "4".to_string()]);
        assert!(ledger.tasks[1].findings[0].text.starts_with("Split from"));
    }

    #[test]
    fn merge_carries_findings_rewires_graph_and_keeps_sources_resolvable() {
        let mut ledger = plan();
        ledger.upsert(
            &[
                Entry {
                    id: Some("1".to_string()),
                    note: Some("seam is the extension api".to_string()),
                    status: Some("done".to_string()),
                    ..Entry::default()
                },
                Entry {
                    id: Some("2".to_string()),
                    note: Some("half written".to_string()),
                    ..Entry::default()
                },
            ],
            4_000,
        );
        let applied = ledger.upsert(
            &[Entry {
                id: Some("1".to_string()),
                merge: Some(vec!["2".to_string()]),
                body: Some("design and implementation are one change".to_string()),
                ..Entry::default()
            }],
            5_000,
        );
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        let survivor = ledger.find_by_id("1").unwrap();
        assert_eq!(
            survivor.status,
            Status::Pending,
            "unfinished work reopens a finished survivor"
        );
        assert_eq!(survivor.findings.len(), 2);
        assert_eq!(
            survivor.findings[1].from,
            Some(FindingOrigin {
                task: "2".to_string(),
                title: "Implement it".to_string()
            })
        );
        let absorbed = ledger.find_by_id("2").unwrap();
        assert_eq!(
            absorbed.cancelled.as_deref(),
            Some("merged into 1 Design the seam")
        );
        assert_eq!(ledger.find_by_id("3").unwrap().after, vec!["1".to_string()]);
        assert!(ledger.resolve("2").is_some());
    }

    #[test]
    fn merge_refuses_a_cycle_and_a_running_foreign_owner() {
        let mut ledger = plan();
        // 3 after 2 after 1; absorbing 1 into 3 would make 3 wait on 2 which waits on 3.
        let before = ledger.clone();
        let applied = ledger.upsert(
            &[Entry {
                id: Some("3".to_string()),
                merge: Some(vec!["1".to_string()]),
                ..Entry::default()
            }],
            6_000,
        );
        assert!(
            applied
                .errors
                .iter()
                .any(|e| e.contains("dependency cycle")),
            "{:?}",
            applied.errors
        );
        assert_eq!(ledger, before);

        ledger.upsert(
            &[Entry {
                id: Some("2".to_string()),
                owner: Some(Some("/root/worker".to_string())),
                ..Entry::default()
            }],
            6_500,
        );
        ledger.executions.insert(
            "/root/worker".to_string(),
            Execution {
                thread_id: "t".to_string(),
                running: true,
                outcome: None,
                updated: 6_500,
            },
        );
        let applied = ledger.upsert(
            &[Entry {
                id: Some("1".to_string()),
                merge: Some(vec!["2".to_string()]),
                ..Entry::default()
            }],
            7_000,
        );
        assert!(
            applied
                .errors
                .iter()
                .any(|e| e.contains("still being executed")),
            "{:?}",
            applied.errors
        );
    }

    #[test]
    fn render_bounds_findings_and_points_at_recall() {
        let mut ledger = plan();
        let notes: Vec<Entry> = (0..6)
            .map(|n| Entry {
                id: Some("1".to_string()),
                status: Some("active".to_string()),
                note: Some(format!("finding {n}")),
                ..Entry::default()
            })
            .collect();
        ledger.upsert(&notes, 8_000);
        let rendered = render(&ledger);
        assert!(
            rendered.contains(
                "Findings: 4 of 6 shown (previews). Recall 1 for complete context and notes."
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("finding 0"));
        assert!(rendered.contains("finding 5"));
        assert!(
            rendered.ends_with("1 active · 0 ready · 2 blocked"),
            "{rendered}"
        );
    }

    #[test]
    fn external_wait_blocks_without_a_prerequisite() {
        let mut ledger = plan();
        let applied = ledger.upsert(
            &[Entry {
                id: Some("1".to_string()),
                waiting: Some(Some("user decides the storage backend".to_string())),
                ..Entry::default()
            }],
            9_000,
        );
        assert!(applied.errors.is_empty());
        let viewed = ledger.view(ledger.find_by_id("1").unwrap());
        assert_eq!(viewed.state, View::Blocked);
        assert_eq!(
            viewed.note.as_deref(),
            Some("waiting: user decides the storage backend")
        );
    }

    #[test]
    fn overview_keeps_running_and_unaccepted_workers_but_omits_old_workers() {
        let mut ledger = plan();
        ledger.tasks[0].owner = Some("review-needed".to_string());
        for (owner, running) in [("old", false), ("review-needed", false), ("running", true)] {
            ledger.executions.insert(
                owner.to_string(),
                Execution {
                    thread_id: owner.to_string(),
                    running,
                    outcome: Some("completed".to_string()),
                    updated: 1,
                },
            );
        }
        let rendered = render(&ledger);
        assert!(rendered.contains("review-needed completed"), "{rendered}");
        assert!(rendered.contains("running running"), "{rendered}");
        assert!(!rendered.contains("old completed"), "{rendered}");
        assert_eq!(ledger.executions.len(), 3);
    }

    #[test]
    fn large_overview_is_bounded_without_losing_stored_context() {
        let mut ledger = Ledger {
            parallelism: 100,
            ..Default::default()
        };
        let entries: Vec<_> = (0..60)
            .map(|n| Entry {
                status: Some("active".to_string()),
                body: Some(format!("{} decisive ending {n}", "長い本文".repeat(1000))),
                ..entry(&format!("Outcome {n}"))
            })
            .collect();
        let applied = ledger.upsert(&entries, 1);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        let rendered = render(&ledger);
        assert!(rendered.len() <= 24_000);
        assert!(rendered.contains("Body shortened. Recall 1"));
        assert!(rendered.contains("Overview shortened."));
        let recalled = render_recall(&ledger, &ledger.tasks[59], "");
        assert!(recalled.contains(&ledger.tasks[59].body));
        assert!(recalled.contains("decisive ending 59"));
    }

    #[test]
    fn timestamps_render_in_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_timestamp(1_788_978_227_000), "2026-09-09 18:23 UTC");
    }
}
