//! Tracks credential changes separately from ownership changes, before notifications coalesce.

use super::CodexAuth;

impl CodexAuth {
    /// Whether request-scoped server state can be reused with these credentials.
    pub fn same_account_as(&self, other: &Self) -> bool {
        match (self.account_scope(), other.account_scope()) {
            (Some(previous), Some(current)) => return previous == current,
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        if same_owner(Some(self), Some(other)) {
            return true;
        }
        match (self, other) {
            (Self::ApiKey(_), Self::ApiKey(_)) => self.api_key() == other.api_key(),
            (Self::Chatgpt(_), Self::Chatgpt(_)) => {
                self.get_current_auth_json() == other.get_current_auth_json()
            }
            (Self::ChatgptAuthTokens(_), Self::ChatgptAuthTokens(_)) => {
                self.get_current_token_data() == other.get_current_token_data()
            }
            (Self::Headers(a), Self::Headers(b)) => a == b,
            (Self::AgentIdentity(a), Self::AgentIdentity(b)) => a.record() == b.record(),
            (Self::PersonalAccessToken(a), Self::PersonalAccessToken(b)) => a == b,
            (Self::BedrockApiKey(a), Self::BedrockApiKey(b)) => a == b,
            (Self::BedrockAccessKeys(a), Self::BedrockAccessKeys(b)) => a == b,
            _ => false,
        }
    }
}

/// Opaque revisions local to one auth manager. Consumers must reset on reconnect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthChangeState {
    /// Advances whenever cached credentials change.
    pub generation: u64,
    /// Advances on login, logout, or changes of user, workspace, or auth mode.
    /// Credential changes with incomplete owner identity also advance this revision.
    pub owner_generation: u64,
}

pub(super) fn same_owner(previous: Option<&CodexAuth>, current: Option<&CodexAuth>) -> bool {
    let (Some(previous), Some(current)) = (previous, current) else {
        return false;
    };
    if previous.api_auth_mode() != current.api_auth_mode() {
        return false;
    }
    let (Some(user), Some(workspace)) = (previous.get_chatgpt_user_id(), previous.get_account_id())
    else {
        return false;
    };
    !user.trim().is_empty()
        && !workspace.trim().is_empty()
        && current.get_chatgpt_user_id().as_ref() == Some(&user)
        && current.get_account_id().as_ref() == Some(&workspace)
}
