//! Complete task records, paged only at the model-visible output boundary.

use crate::ledger::CONTEXT_BYTES;

// Leave room for write feedback and the page navigation text within the context budget.
const PAGE_BYTES: usize = CONTEXT_BYTES - 2_048;

pub(crate) fn render(records: &str, page: usize) -> String {
    let pages = records.len().div_ceil(PAGE_BYTES).max(/*other*/ 1);
    if page == 0 || page > pages {
        return format!("Task details have {pages} pages; request a page from 1 to {pages}.");
    }
    let offset = (page - 1) * PAGE_BYTES;
    let start = records.floor_char_boundary(offset);
    let end = records.floor_char_boundary(offset.saturating_add(PAGE_BYTES).min(records.len()));
    let mut output = format!(
        "Task details page {page}/{pages}:\n\n{}",
        &records[start..end]
    );
    if page < pages {
        let next = page + 1;
        output.push_str(&format!(
            "\n\nRead every remaining page before working on these tasks. Call ledger with the same task IDs in `recall` and `page: {next}`. Pages read the current records; restart at page 1 if those records change."
        ));
    }
    output
}
