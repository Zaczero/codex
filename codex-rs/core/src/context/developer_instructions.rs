use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeveloperInstructions {
    instructions: String,
}

impl DeveloperInstructions {
    pub(crate) fn new(instructions: impl Into<String>) -> Self {
        Self {
            instructions: instructions.into(),
        }
    }

    pub(crate) fn replacement(instructions: &str) -> codex_protocol::error::Result<Self> {
        let text = if instructions.is_empty() {
            "The previously provided workspace and client developer instructions no longer apply."
                .to_string()
        } else {
            format!(
                "These workspace and client developer instructions replace all previously provided workspace and client developer instructions.\n\n{instructions}"
            )
        };
        if text.len() > codex_utils_string::approx_bytes_for_tokens(/*tokens*/ 10_000) {
            return Err(codex_protocol::error::CodexErr::InvalidRequest(
                "workspace developer instructions exceed the 10000-token context limit".to_string(),
            ));
        }
        Ok(Self::new(text))
    }
}

impl ContextualUserFragment for DeveloperInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("generic.developer_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.instructions.clone()
    }
}
