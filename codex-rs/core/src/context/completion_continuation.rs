use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

pub(crate) struct CompletionContinuation(String);

impl CompletionContinuation {
    pub(crate) fn new(mut reason: String) -> Self {
        let end = reason.floor_char_boundary(1_000.min(reason.len()));
        reason.truncate(end);
        Self(reason)
    }
}

impl ContextualUserFragment for CompletionContinuation {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("task.completion_continuation".to_owned())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<completion-continuation>", "</completion-continuation>")
    }

    fn body(&self) -> String {
        self.0.clone()
    }
}
