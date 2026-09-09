use std::time::Duration;

use codex_api::ReqwestTransport;
use codex_client::HttpTransport;
use codex_client::RequestBody;
use codex_login::default_client::create_client;
use codex_model_provider::SharedModelProvider;
use codex_utils_output_truncation::TruncationPolicy;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use serde_json::json;

const HISTORY_NOTES_BACKEND_TIMEOUT: Duration = Duration::from_secs(35);
const ENCRYPTED_TOOL_ARGUMENTS_HEADER: &str = "x-openai-encrypted-tool-arguments";
const TOOL_OUTPUT_TRUNCATION_POLICY_HEADER: &str = "x-openai-tool-output-truncation-policy";
const OPERATION_ERROR_PREFIX: &str = "Unable to perform operation:";

pub(crate) struct HistoryNotesResponse {
    pub(crate) value: Value,
    pub(crate) account_scope: Option<codex_protocol::auth::AccountScope>,
}

#[derive(Clone)]
pub(crate) struct HistoryNotesBackend {
    provider: SharedModelProvider,
}

impl HistoryNotesBackend {
    pub(crate) fn new(provider: SharedModelProvider) -> Self {
        Self { provider }
    }

    pub(crate) async fn call(
        &self,
        path: &str,
        session_id: &str,
        current_agent_name: &str,
        mut arguments: Value,
        truncation_policy: TruncationPolicy,
        originating_account: Option<&codex_protocol::auth::AccountScope>,
    ) -> Result<HistoryNotesResponse, String> {
        let Some(arguments_object) = arguments.as_object_mut() else {
            return Err("History tool arguments must be a JSON object".to_string());
        };
        arguments_object.insert(
            "context".to_string(),
            json!({
                "session_id": session_id,
                "current_agent_name": current_agent_name,
            }),
        );

        let setup = self
            .provider
            .request_setup(
                self.provider.auth().await,
                codex_model_provider::ProviderAuthScope {
                    agent_identity_policy: codex_login::AgentIdentityAuthPolicy::JwtOnly,
                    session_source: codex_protocol::protocol::SessionSource::Exec,
                    agent_identity_session_fallback: Default::default(),
                },
            )
            .await
            .map_err(|_| {
                format!("{OPERATION_ERROR_PREFIX} Could not resolve backend authentication.")
            })?;
        let account_scope = setup
            .auth
            .as_ref()
            .and_then(codex_login::CodexAuth::account_scope);

        let mut request = setup.api_provider.build_request(Method::POST, path);
        let encoded_truncation_policy =
            serde_json::to_string(&truncation_policy).map_err(|_| {
                format!("{OPERATION_ERROR_PREFIX} Could not encode the output truncation policy.")
            })?;
        request.headers.insert(
            TOOL_OUTPUT_TRUNCATION_POLICY_HEADER,
            HeaderValue::from_str(&encoded_truncation_policy).map_err(|_| {
                format!(
                    "{OPERATION_ERROR_PREFIX} Could not construct the output truncation policy header."
                )
            })?,
        );
        if matches!(
            path,
            "alpha/history/v2/search_contents"
                | "alpha/notes/v2/search_contents"
                | "alpha/notes/v2/append_to_file"
                | "alpha/notes/v2/write_file"
        ) {
            if originating_account.is_none() || originating_account != account_scope.as_ref() {
                return Err(format!(
                    "{OPERATION_ERROR_PREFIX} Encrypted arguments belong to a different or unknown account."
                ));
            }
            request.headers.insert(
                ENCRYPTED_TOOL_ARGUMENTS_HEADER,
                HeaderValue::from_static("true"),
            );
        }
        request.body = Some(RequestBody::Json(arguments));
        request.timeout = Some(HISTORY_NOTES_BACKEND_TIMEOUT);
        let request = setup
            .resolved_auth
            .auth
            .apply_auth(request)
            .await
            .map_err(|_| {
                format!("{OPERATION_ERROR_PREFIX} Could not apply backend authentication.")
            })?;
        let response = ReqwestTransport::from_http_client(create_client())
            .execute(request)
            .await
            .map_err(|_| format!("{OPERATION_ERROR_PREFIX} The backend request failed."))?;

        serde_json::from_slice(&response.body)
            .map(|value| HistoryNotesResponse {
                value,
                account_scope,
            })
            .map_err(|_| format!("{OPERATION_ERROR_PREFIX} The backend returned invalid JSON."))
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
