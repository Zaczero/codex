use super::*;
use codex_config::ManagedAuthPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

fn config(home: &Path) -> AuthConfig {
    AuthConfig {
        codex_home: home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::default(),
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    }
}

fn credentials(key: &str) -> AuthDotJson {
    serde_json::from_value(json!({"auth_mode": "apikey", "OPENAI_API_KEY": key})).unwrap()
}

fn add(config: &AuthConfig, label: &str, key: &str) -> NamedAccountMetadata {
    save_named(
        &config.codex_home,
        label,
        &credentials(key),
        config.auth_credentials_store_mode,
        config.keyring_backend_kind,
    )
    .unwrap()
}

#[test]
fn rename_preserves_identity_and_switch_selects_its_credentials() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    let default = add(&config, "default", "default-secret");
    let kate = add(&config, "kate", "kate-secret");
    add(&config, "backup", "backup-secret");

    let renamed = config.rename_named_account(&kate.id, "travel")?;
    assert_eq!(renamed.id, kate.id);
    assert_eq!(renamed.label, "travel");
    config.switch_named_account("travel")?;
    assert_eq!(
        storage_for(&config).load()?,
        Some(credentials("kate-secret"))
    );
    assert_eq!(
        config
            .list_named_accounts()?
            .iter()
            .map(|account| (account.label.as_str(), account.is_active))
            .collect::<Vec<_>>(),
        vec![("backup", false), ("default", false), ("travel", true)]
    );
    config.switch_named_account(&default.id)?;
    assert_eq!(
        storage_for(&config).load()?,
        Some(credentials("default-secret"))
    );
    Ok(())
}

#[test]
fn rejected_add_and_rename_preserve_the_bank() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    let default = add(&config, "default", "default-secret");
    add(&config, "kate", "kate-secret");
    let before = storage_for(&config).load_bank()?;
    for label in ["kate", "   "] {
        assert!(
            save_named(
                home.path(),
                label,
                &credentials("uncommitted-secret"),
                config.auth_credentials_store_mode,
                config.keyring_backend_kind,
            )
            .is_err()
        );
        assert!(config.rename_named_account(&default.id, label).is_err());
        assert_eq!(storage_for(&config).load_bank()?, before);
    }
    Ok(())
}

#[test]
fn logout_preserves_other_accounts_without_selecting_one() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    let default = add(&config, "default", "default-secret");
    add(&config, "kate", "kate-secret");
    assert!(crate::logout(
        home.path(),
        config.auth_credentials_store_mode,
        config.keyring_backend_kind
    )?);
    assert_eq!(storage_for(&config).load()?, None);
    let accounts = config.list_named_accounts()?;
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, default.id);
    assert!(!accounts[0].is_active);
    config.switch_named_account(&default.id)?;
    assert_eq!(
        storage_for(&config).load()?,
        Some(credentials("default-secret"))
    );
    Ok(())
}

#[tokio::test]
async fn removing_last_active_account_leaves_no_credentials() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    let account = add(&config, "only", "only-secret");
    assert!(config.remove_named_account(&account.id).await?);
    assert_eq!(config.list_named_accounts()?, Vec::new());
    assert_eq!(storage_for(&config).load()?, None);
    Ok(())
}

#[test]
fn account_list_contains_metadata_without_secrets() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    let account = add(&config, "kate", "credential-must-not-be-listed");
    assert_eq!(
        serde_json::to_value(config.list_named_accounts()?)?,
        json!([{
            "id": account.id,
            "label": "kate",
            "is_active": true,
            "account_id": null,
            "email": null,
            "auth_mode": "apikey",
            "plan_type": null
        }])
    );
    Ok(())
}

#[test]
fn ambiguous_selector_does_not_switch_or_rename_an_account() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    let first = add(&config, "first", "first-secret");
    add(&config, &first.id, "second-secret");
    let before = storage_for(&config).load_bank()?;
    assert!(config.switch_named_account(&first.id).is_err());
    assert!(config.rename_named_account(&first.id, "renamed").is_err());
    assert_eq!(storage_for(&config).load_bank()?, before);
    Ok(())
}

#[test]
fn rejected_switch_preserves_selection() -> anyhow::Result<()> {
    let home = tempdir()?;
    let mut config = config(home.path());
    add(&config, "first", "first-secret");
    add(&config, "second", "second-secret");
    let before = storage_for(&config).load_bank()?;
    config.forced_login_method = Some(codex_protocol::config_types::ForcedLoginMethod::Chatgpt);
    assert_eq!(
        config.switch_named_account("first").unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(storage_for(&config).load_bank()?, before);
    Ok(())
}

#[tokio::test]
async fn idle_managers_observe_switches_in_both_directions() -> anyhow::Result<()> {
    const CHILD_HOME: &str = "CODEX_NAMED_ACCOUNT_TEST_HOME";
    const CHILD_ACCOUNT: &str = "CODEX_NAMED_ACCOUNT_TEST_ID";
    if let Some(home) = std::env::var_os(CHILD_HOME) {
        config(Path::new(&home)).switch_named_account(&std::env::var(CHILD_ACCOUNT)?)?;
        return Ok(());
    }
    let home = tempdir()?;
    let config = config(home.path());
    let first = add(&config, "first", "first-secret");
    let second = add(&config, "second", "second-secret");
    let left = crate::AuthManager::shared_from_auth_config(
        config.clone(),
        /*enable_codex_api_key_env*/ false,
    )
    .await?;
    let right = crate::AuthManager::shared_from_auth_config(
        config.clone(),
        /*enable_codex_api_key_env*/ false,
    )
    .await?;
    let mut left_changes = left.auth_change_receiver();
    let mut right_changes = right.auth_change_receiver();

    for (id, key) in [(&first.id, "first-secret"), (&second.id, "second-secret")] {
        let mut child = tokio::process::Command::new(std::env::current_exe()?);
        let output = child
            .args([
                "--exact",
                "auth::named_accounts::tests::idle_managers_observe_switches_in_both_directions",
            ])
            .env(CHILD_HOME, home.path())
            .env(CHILD_ACCOUNT, id)
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), output).await??;
        assert!(
            output.status.success(),
            "child account switch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            left_changes.changed().await?;
            right_changes.changed().await
        })
        .await??;
        assert_eq!(
            left.auth_cached()
                .as_ref()
                .and_then(crate::CodexAuth::api_key),
            Some(key)
        );
        assert_eq!(
            right
                .auth_cached()
                .as_ref()
                .and_then(crate::CodexAuth::api_key),
            Some(key)
        );
    }
    Ok(())
}

#[tokio::test]
async fn direct_file_changes_reconcile_without_a_notification_marker() -> anyhow::Result<()> {
    let home = tempdir()?;
    let config = config(home.path());
    add(&config, "first", "first-secret");
    let manager = crate::AuthManager::shared_from_auth_config(
        config.clone(),
        /*enable_codex_api_key_env*/ false,
    )
    .await?;
    let auth_file = home.path().join("auth.json");
    std::fs::write(&auth_file, b"invalid-json")?;
    assert!(manager.auth().await.is_none());
    std::fs::write(
        &auth_file,
        serde_json::to_vec(&credentials("replacement-secret"))?,
    )?;
    assert_eq!(
        manager
            .auth()
            .await
            .as_ref()
            .and_then(crate::CodexAuth::api_key),
        Some("replacement-secret")
    );
    Ok(())
}
