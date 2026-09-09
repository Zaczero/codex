use std::fmt;
use std::path::Path;

use codex_config::types::AuthCredentialsStoreMode;
use serde::Deserialize;
use serde::Serialize;

use super::manager::save_auth;
use super::storage::AuthDotJson;
use super::storage::AuthKeyringBackendKind;
use codex_protocol::auth::AuthMode;

/// Managed Amazon Bedrock API key persisted in `auth.json`.
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(from = "StoredBedrockApiKey")]
pub struct BedrockApiKeyAuth {
    api_key: String,
    pub region: String,
    #[serde(skip)]
    pub(super) account_scope: codex_protocol::auth::AccountScope,
}

#[derive(Deserialize)]
struct StoredBedrockApiKey {
    api_key: String,
    region: String,
}

impl From<StoredBedrockApiKey> for BedrockApiKeyAuth {
    fn from(stored: StoredBedrockApiKey) -> Self {
        Self::new(stored.api_key, stored.region)
    }
}

impl BedrockApiKeyAuth {
    pub fn new(api_key: String, region: String) -> Self {
        use sha2::Digest;
        let account_scope = codex_protocol::auth::AccountScope {
            local_account_id: None,
            account_id: format!(
                "bedrock-api-key:{:x}",
                sha2::Sha256::digest(api_key.as_bytes())
            ),
            user_id: None,
        };
        Self {
            api_key,
            region,
            account_scope,
        }
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

impl fmt::Debug for BedrockApiKeyAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockApiKeyAuth")
            .field("api_key", &"<redacted>")
            .field("region", &self.region)
            .finish()
    }
}

/// Writes an `auth.json` that contains only the Amazon Bedrock API key auth.
pub fn login_with_bedrock_api_key(
    codex_home: &Path,
    api_key: &str,
    region: &str,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<()> {
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::BedrockApiKey),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: Some(BedrockApiKeyAuth::new(
            api_key.to_string(),
            region.to_string(),
        )),
        bedrock_access_keys: None,
    };
    save_auth(
        codex_home,
        &auth_dot_json,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )
}

#[cfg(test)]
#[path = "bedrock_api_key_tests.rs"]
mod tests;
