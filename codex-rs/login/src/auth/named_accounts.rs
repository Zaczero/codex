use std::path::Path;
use std::sync::Arc;

use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::PlanType;
use serde::Serialize;
use thiserror::Error;

use super::manager::AuthConfig;
use super::revoke::revoke_auth_tokens;
use super::storage::AuthKeyringBackendKind;
use super::storage::AuthStorage;
use super::storage::StoredAuthAccount;
use super::storage::StoredAuthBank;
use super::storage::create_auth_storage;
use super::storage::new_account_id;
use crate::auth::storage::AuthDotJson;
use crate::outbound_proxy::AuthRouteConfig;

#[cfg(test)]
#[path = "named_accounts_tests.rs"]
mod tests;

/// Metadata for one locally named credential record.
///
/// This type intentionally contains no access, refresh, API, or identity tokens. The local ID is
/// stable across label changes and is unrelated to the provider workspace ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NamedAccountMetadata {
    pub id: String,
    pub label: String,
    pub is_active: bool,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub auth_mode: AuthMode,
    pub plan_type: Option<String>,
}

#[derive(Debug, Error)]
pub enum NamedAccountError {
    #[error("account label must not be empty")]
    EmptyLabel,
    #[error("account label already exists: {0}")]
    DuplicateLabel(String),
    #[error("named account not found: {0}")]
    NotFound(String),
    #[error("account selector matches both an ID and another account's label: {0}")]
    AmbiguousSelector(String),
}

fn storage_for(config: &AuthConfig) -> Arc<AuthStorage> {
    create_auth_storage(
        config.codex_home.clone(),
        config.auth_credentials_store_mode,
        config.keyring_backend_kind,
    )
}

pub(super) fn normalize_label(label: &str) -> Result<String, NamedAccountError> {
    let label = label.trim();
    if label.is_empty() {
        return Err(NamedAccountError::EmptyLabel);
    }
    Ok(label.to_string())
}

fn resolve_account_id(bank: &StoredAuthBank, selector: &str) -> Result<String, NamedAccountError> {
    let label_match = bank
        .accounts
        .iter()
        .find(|(_, account)| account.label == selector);
    if bank.accounts.contains_key(selector) {
        if label_match.is_some_and(|(id, _)| id != selector) {
            return Err(NamedAccountError::AmbiguousSelector(selector.to_string()));
        }
        return Ok(selector.to_string());
    }
    label_match
        .map(|(id, _)| id.clone())
        .ok_or_else(|| NamedAccountError::NotFound(selector.to_string()))
}

fn plan_type_from_auth(auth: &AuthDotJson) -> Option<String> {
    match auth.resolved_mode() {
        AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.chatgpt_plan_type.as_ref())
            .map(|plan| match plan {
                PlanType::Known(plan) => plan.raw_value().to_string(),
                PlanType::Unknown(plan) => plan.clone(),
            }),
        AuthMode::AgentIdentity => {
            auth.agent_identity
                .as_ref()
                .and_then(|identity| match identity {
                    super::storage::AgentIdentityStorage::Jwt(jwt) => {
                        super::storage::AgentIdentityAuthRecord::from_agent_identity_jwt(jwt)
                            .ok()
                            .map(|record| account_plan_type_name(record.plan_type))
                    }
                    super::storage::AgentIdentityStorage::Record(record) => {
                        Some(account_plan_type_name(record.plan_type))
                    }
                })
        }
        _ => None,
    }
}

fn account_plan_type_name(plan: codex_protocol::account::PlanType) -> String {
    serde_json::to_value(plan)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn account_id_from_auth(auth: &AuthDotJson) -> Option<String> {
    match auth.resolved_mode() {
        AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => {
            auth.tokens.as_ref().and_then(|tokens| {
                tokens
                    .account_id
                    .clone()
                    .filter(|account_id| !account_id.is_empty())
                    .or_else(|| tokens.id_token.chatgpt_account_id.clone())
            })
        }
        AuthMode::AgentIdentity => {
            auth.agent_identity
                .as_ref()
                .and_then(|identity| match identity {
                    super::storage::AgentIdentityStorage::Jwt(jwt) => {
                        super::storage::AgentIdentityAuthRecord::from_agent_identity_jwt(jwt)
                            .ok()
                            .map(|record| record.account_id)
                    }
                    super::storage::AgentIdentityStorage::Record(record) => {
                        Some(record.account_id.clone())
                    }
                })
        }
        _ => None,
    }
}

fn email_from_auth(auth: &AuthDotJson) -> Option<String> {
    match auth.resolved_mode() {
        AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.email.clone()),
        AuthMode::AgentIdentity => {
            auth.agent_identity
                .as_ref()
                .and_then(|identity| match identity {
                    super::storage::AgentIdentityStorage::Jwt(jwt) => {
                        super::storage::AgentIdentityAuthRecord::from_agent_identity_jwt(jwt)
                            .ok()
                            .and_then(|record| record.email)
                    }
                    super::storage::AgentIdentityStorage::Record(record) => record.email.clone(),
                })
        }
        _ => None,
    }
}

fn metadata_for(
    id: String,
    account: &StoredAuthAccount,
    selected_id: Option<&str>,
) -> NamedAccountMetadata {
    NamedAccountMetadata {
        id: id.clone(),
        label: account.label.clone(),
        is_active: selected_id == Some(id.as_str()),
        account_id: account_id_from_auth(&account.auth),
        email: email_from_auth(&account.auth),
        auth_mode: account.auth.resolved_mode(),
        plan_type: plan_type_from_auth(&account.auth),
    }
}

pub(super) fn list(config: &AuthConfig) -> std::io::Result<Vec<NamedAccountMetadata>> {
    let bank = storage_for(config).load_bank()?;
    let selected_id = bank.selected_account_id.as_deref();
    let mut accounts = bank
        .accounts
        .into_iter()
        .map(|(id, account)| metadata_for(id, &account, selected_id))
        .collect::<Vec<_>>();
    accounts.sort_by(|left, right| left.label.cmp(&right.label).then(left.id.cmp(&right.id)));
    Ok(accounts)
}

#[expect(
    clippy::expect_used,
    reason = "the resolved account remains present throughout the bank transaction"
)]
pub(super) fn rename(
    config: &AuthConfig,
    selector: &str,
    label: &str,
) -> std::io::Result<NamedAccountMetadata> {
    let label = normalize_label(label).map_err(std::io::Error::other)?;
    let storage = storage_for(config);
    storage.update_bank_with(|bank| {
        let account_id = resolve_account_id(bank, selector).map_err(std::io::Error::other)?;
        if bank
            .accounts
            .iter()
            .any(|(id, account)| id != &account_id && account.label == label)
        {
            return Err(std::io::Error::other(NamedAccountError::DuplicateLabel(
                label,
            )));
        }
        bank.accounts
            .get_mut(&account_id)
            .expect("resolved account must exist")
            .label = label;
        Ok(metadata_for(
            account_id.clone(),
            bank.accounts
                .get(&account_id)
                .expect("renamed account must exist"),
            bank.selected_account_id.as_deref(),
        ))
    })
}

#[expect(
    clippy::expect_used,
    reason = "the resolved account remains present throughout the bank transaction"
)]
pub(super) fn switch(config: &AuthConfig, selector: &str) -> std::io::Result<NamedAccountMetadata> {
    let storage = storage_for(config);
    storage.update_bank_with(|bank| {
        let account_id = resolve_account_id(bank, selector).map_err(std::io::Error::other)?;
        let target = metadata_for(
            account_id.clone(),
            bank.accounts
                .get(&account_id)
                .expect("resolved account must exist"),
            bank.selected_account_id.as_deref(),
        );
        let login_method = if target.auth_mode.uses_codex_backend() {
            codex_protocol::config_types::ForcedLoginMethod::Chatgpt
        } else {
            codex_protocol::config_types::ForcedLoginMethod::Api
        };
        if !config.is_login_method_allowed(login_method) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "authentication requirements do not permit this named account",
            ));
        }
        if let Some(workspaces) = config.effective_chatgpt_workspaces()
            && target.auth_mode.uses_codex_backend()
            && !target
                .account_id
                .as_ref()
                .is_some_and(|id| workspaces.contains(id))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "named account is outside the configured workspace restriction",
            ));
        }
        bank.selected_account_id = Some(account_id.clone());
        Ok(metadata_for(
            account_id.clone(),
            bank.accounts
                .get(&account_id)
                .expect("switched account must exist"),
            bank.selected_account_id.as_deref(),
        ))
    })
}

#[expect(
    clippy::expect_used,
    reason = "the account was inserted in the same bank transaction"
)]
pub(super) fn save_named(
    codex_home: &Path,
    label: &str,
    auth: &AuthDotJson,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<NamedAccountMetadata> {
    let label = normalize_label(label).map_err(std::io::Error::other)?;
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.update_bank_with(|bank| {
        if bank.accounts.values().any(|account| account.label == label) {
            return Err(std::io::Error::other(NamedAccountError::DuplicateLabel(
                label,
            )));
        }
        let account_id = new_account_id(&bank.accounts);
        bank.accounts.insert(
            account_id.clone(),
            StoredAuthAccount {
                label,
                auth: auth.clone(),
            },
        );
        bank.selected_account_id = Some(account_id.clone());
        Ok(metadata_for(
            account_id.clone(),
            bank.accounts
                .get(&account_id)
                .expect("added account must exist"),
            bank.selected_account_id.as_deref(),
        ))
    })
}

pub(super) async fn remove(
    config: &AuthConfig,
    selector: &str,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<bool> {
    let storage = storage_for(config);
    let bank = storage.load_bank()?;
    let account_id = resolve_account_id(&bank, selector).map_err(std::io::Error::other)?;
    remove_from_storage(&storage, &account_id, auth_route_config).await
}

pub(super) async fn remove_from_storage(
    storage: &Arc<AuthStorage>,
    account_id: &str,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<bool> {
    let _account_lock = storage.acquire_account_lock(account_id).await?;
    let bank = storage.load_bank()?;
    let Some(snapshot) = bank.snapshot(account_id) else {
        return Ok(false);
    };
    let removed = storage.update_bank_with(|bank| {
        let removed = bank.accounts.remove(account_id).is_some();
        if bank.selected_account_id.as_deref() == Some(account_id) {
            bank.selected_account_id = None;
        }
        Ok(removed)
    })?;
    if removed && let Err(err) = revoke_auth_tokens(Some(&snapshot.auth), auth_route_config).await {
        tracing::warn!(account_id = %account_id, "failed to revoke named account during removal: {err}");
    }
    Ok(removed)
}
