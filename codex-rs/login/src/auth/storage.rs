use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

use super::BedrockAccessKeysAuth;
use super::BedrockApiKeyAuth;
use crate::token_data::TokenData;
use codex_agent_identity::AgentIdentityJwtClaims;
use codex_agent_identity::decode_agent_identity_jwt;
use codex_config::types::AuthCredentialsStoreMode;
pub use codex_config::types::AuthKeyringBackendKind;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use once_cell::sync::Lazy;

/// Expected structure for $CODEX_HOME/auth.json.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,

    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<AgentIdentityStorage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_access_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_api_key: Option<BedrockApiKeyAuth>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_access_keys: Option<BedrockAccessKeysAuth>,
}

/// Stable local identity for the account imported from the pre-bank auth format.
///
/// This value is deliberately not derived from provider account metadata: old auth files may not
/// contain a provider workspace ID, and every process must still agree on the imported record.
pub(super) const LEGACY_ACCOUNT_ID: &str = "legacy";
const AUTH_BANK_VERSION: u8 = 1;
const AUTH_CHANGE_FILE: &str = "auth.change";

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub(super) struct StoredAuthAccount {
    pub(super) label: String,
    pub(super) auth: AuthDotJson,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub(super) struct StoredAuthBank {
    pub(super) version: u8,
    pub(super) selected_account_id: Option<String>,
    pub(super) accounts: BTreeMap<String, StoredAuthAccount>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct StoredAuthSnapshot {
    pub(super) account_id: String,
    pub(super) label: String,
    pub(super) auth: AuthDotJson,
}

impl StoredAuthBank {
    fn empty() -> Self {
        Self {
            version: AUTH_BANK_VERSION,
            selected_account_id: None,
            accounts: BTreeMap::new(),
        }
    }

    pub(super) fn from_legacy(auth: AuthDotJson) -> Self {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            LEGACY_ACCOUNT_ID.to_string(),
            StoredAuthAccount {
                label: "default".to_string(),
                auth,
            },
        );
        Self {
            version: AUTH_BANK_VERSION,
            selected_account_id: Some(LEGACY_ACCOUNT_ID.to_string()),
            accounts,
        }
    }

    pub(super) fn selected(&self) -> Option<StoredAuthSnapshot> {
        let account_id = self.selected_account_id.as_ref()?;
        let account = self.accounts.get(account_id)?;
        Some(StoredAuthSnapshot {
            account_id: account_id.clone(),
            label: account.label.clone(),
            auth: account.auth.clone(),
        })
    }

    pub(super) fn snapshot(&self, account_id: &str) -> Option<StoredAuthSnapshot> {
        let account = self.accounts.get(account_id)?;
        Some(StoredAuthSnapshot {
            account_id: account_id.to_string(),
            label: account.label.clone(),
            auth: account.auth.clone(),
        })
    }
}

fn decode_stored_bank(serialized: &str) -> std::io::Result<StoredAuthBank> {
    let value: serde_json::Value = serde_json::from_str(serialized)?;
    if ["version", "accounts", "selected_account_id"]
        .iter()
        .any(|key| value.get(key).is_some())
    {
        let bank: StoredAuthBank = serde_json::from_value(value)?;
        if bank.version != AUTH_BANK_VERSION {
            return Err(std::io::Error::other(format!(
                "unsupported authentication bank version: {}",
                bank.version
            )));
        }
        if bank
            .selected_account_id
            .as_ref()
            .is_some_and(|id| !bank.accounts.contains_key(id))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "selected authentication account does not exist",
            ));
        }
        let mut labels = std::collections::BTreeSet::new();
        for (id, account) in &bank.accounts {
            if id.is_empty()
                || account.label.is_empty()
                || account.label.trim() != account.label
                || !labels.insert(&account.label)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "authentication bank contains an invalid ID or a non-unique, non-normalized label",
                ));
            }
        }
        return Ok(bank);
    }
    let auth = serde_json::from_value(value).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, InvalidLegacyAuth(error))
    })?;
    Ok(StoredAuthBank::from_legacy(auth))
}

#[derive(Debug, thiserror::Error)]
#[error("invalid legacy authentication: {0}")]
struct InvalidLegacyAuth(serde_json::Error);

fn encode_stored_bank(bank: &StoredAuthBank) -> std::io::Result<String> {
    serde_json::to_string_pretty(bank).map_err(std::io::Error::other)
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum AgentIdentityStorage {
    Jwt(String),
    Record(AgentIdentityAuthRecord),
}

impl AgentIdentityStorage {
    pub fn has_auth_material(&self) -> bool {
        match self {
            Self::Jwt(jwt) => !jwt.trim().is_empty(),
            Self::Record(record) => {
                !record.agent_runtime_id.trim().is_empty()
                    && !record.agent_private_key.trim().is_empty()
            }
        }
    }

    pub(crate) fn as_record(&self) -> Option<&AgentIdentityAuthRecord> {
        match self {
            Self::Jwt(_) => None,
            Self::Record(record) => Some(record),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentityAuthRecord {
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_empty_string",
        serialize_with = "serialize_optional_string_as_empty"
    )]
    pub email: Option<String>,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|value| value.filter(|value| !value.is_empty()))
}

fn serialize_optional_string_as_empty<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value.as_deref().unwrap_or_default().serialize(serializer)
}

impl AgentIdentityAuthRecord {
    pub(crate) fn from_agent_identity_jwt(jwt: &str) -> std::io::Result<Self> {
        let claims =
            decode_agent_identity_jwt(jwt, /*jwks*/ None).map_err(std::io::Error::other)?;

        Ok(claims.into())
    }
}

impl From<AgentIdentityJwtClaims> for AgentIdentityAuthRecord {
    fn from(claims: AgentIdentityJwtClaims) -> Self {
        Self {
            agent_runtime_id: claims.agent_runtime_id,
            agent_private_key: claims.agent_private_key,
            account_id: claims.account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            email: claims.email,
            plan_type: claims.plan_type.into(),
            chatgpt_account_is_fedramp: claims.chatgpt_account_is_fedramp,
            task_id: None,
        }
    }
}

pub(super) fn get_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

pub(super) fn delete_file_if_exists(codex_home: &Path) -> std::io::Result<bool> {
    let auth_file = get_auth_file(codex_home);
    match std::fs::remove_file(&auth_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

pub(super) trait AuthStorageBackend: Debug + Send + Sync {
    fn load(&self) -> std::io::Result<Option<StoredAuthBank>>;
    fn save(&self, bank: &StoredAuthBank) -> std::io::Result<()>;
    fn delete(&self) -> std::io::Result<bool>;
}

#[derive(Debug)]
pub(super) struct AuthStorage {
    backend: Arc<dyn AuthStorageBackend>,
    lock: StorageLock,
}

#[derive(Debug)]
enum StorageLock {
    Persistent(PathBuf),
    Ephemeral,
}

static EPHEMERAL_AUTH_TRANSACTION: Mutex<()> = Mutex::new(());
static EPHEMERAL_ACCOUNT_LOCKS: Lazy<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(super) enum AccountRefreshLock {
    Persistent {
        _file: File,
    },
    Ephemeral {
        _guard: tokio::sync::OwnedMutexGuard<()>,
    },
}

impl Debug for AccountRefreshLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountRefreshLock").finish_non_exhaustive()
    }
}

impl AuthStorage {
    fn transaction<T>(
        &self,
        action: impl FnOnce(&dyn AuthStorageBackend) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        match &self.lock {
            StorageLock::Persistent(codex_home) => {
                // A home that does not exist holds nothing to serialize against; a write
                // creates it, and a read must not leave an empty home behind.
                if !codex_home.is_dir() {
                    return action(self.backend.as_ref());
                }
                let mut options = OpenOptions::new();
                options.read(true).write(true).create(true).truncate(false);
                #[cfg(unix)]
                options.mode(0o600);
                // Keep the lock inode separate from the replaceable credential file.
                let lock = options.open(codex_home.join("auth.lock"))?;
                lock.lock()?;
                action(self.backend.as_ref())
            }
            StorageLock::Ephemeral => {
                let _guard = EPHEMERAL_AUTH_TRANSACTION.lock().map_err(|_| {
                    std::io::Error::other("failed to lock ephemeral auth transaction")
                })?;
                action(self.backend.as_ref())
            }
        }
    }

    pub(super) fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.transaction(|backend| {
            Ok(backend
                .load()?
                .and_then(|bank| bank.selected().map(|snapshot| snapshot.auth)))
        })
    }

    pub(super) fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.mutate(|backend| {
            let mut bank = match backend.load() {
                Ok(bank) => bank.unwrap_or_else(StoredAuthBank::empty),
                // Explicit login can replace a broken single-account payload, never a broken bank.
                Err(error) if error.get_ref().is_some_and(<dyn std::error::Error + std::marker::Send + std::marker::Sync + 'static >::is::<InvalidLegacyAuth>) => {
                    StoredAuthBank::empty()
                }
                Err(error) => return Err(error),
            };
            if let Some(selected_account_id) = bank.selected_account_id.as_ref()
                && let Some(account) = bank.accounts.get_mut(selected_account_id)
            {
                account.auth = auth.clone();
            } else {
                let account_id = new_account_id(&bank.accounts);
                let label = unique_default_label(&bank);
                bank.accounts.insert(
                    account_id.clone(),
                    StoredAuthAccount { label, auth: auth.clone() },
                );
                bank.selected_account_id = Some(account_id);
            }
            backend.save(&bank)
        })
    }

    pub(super) fn delete(&self) -> std::io::Result<bool> {
        self.update_bank(|bank| {
            let Some(account_id) = bank.selected_account_id.take() else {
                return Ok(false);
            };
            Ok(bank.accounts.remove(&account_id).is_some())
        })
    }

    pub(super) fn replace_if_unchanged_for_account(
        &self,
        account_id: &str,
        expected: &AuthDotJson,
        updated: &AuthDotJson,
    ) -> std::io::Result<()> {
        self.update_bank(|bank| {
            let account = bank.accounts.get_mut(account_id).ok_or_else(|| {
                std::io::Error::other("Authentication account was removed during an update.")
            })?;
            if account.auth != *expected {
                return Err(std::io::Error::other(
                    "Authentication changed during an account update; retry with the current account.",
                ));
            }
            account.auth = updated.clone();
            Ok(())
        })
    }

    pub(super) fn refresh_tokens_for_account(
        &self,
        account_id: &str,
        expected: &AuthDotJson,
        updated: &AuthDotJson,
    ) -> std::io::Result<AuthDotJson> {
        self.update_bank(|bank| {
            let account = bank.accounts.get_mut(account_id).ok_or_else(|| {
                std::io::Error::other("Authentication was removed during token refresh.")
            })?;
            let mut current = account.auth.clone();
            // Preserve enrollment that completed while this account's OAuth request was in flight.
            let agent_identity = current.agent_identity.take();
            current.agent_identity.clone_from(&expected.agent_identity);
            if &current != expected {
                return Err(std::io::Error::other(
                    "Authentication changed during token refresh; retry with the current account.",
                ));
            }
            let mut refreshed = updated.clone();
            refreshed.agent_identity = agent_identity;
            account.auth = refreshed.clone();
            Ok(refreshed)
        })
    }

    fn mutate<T>(
        &self,
        action: impl FnOnce(&dyn AuthStorageBackend) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let result = self.transaction(action);
        if result.is_ok() {
            self.publish_change();
        }
        result
    }

    fn update_bank<T>(
        &self,
        action: impl FnOnce(&mut StoredAuthBank) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        self.mutate(|backend| {
            let mut bank = backend.load()?.unwrap_or_else(StoredAuthBank::empty);
            let result = action(&mut bank)?;
            if bank.accounts.is_empty() {
                backend.delete()?;
            } else {
                backend.save(&bank)?;
            }
            Ok(result)
        })
    }

    pub(super) fn load_bank(&self) -> std::io::Result<StoredAuthBank> {
        self.transaction(|backend| Ok(backend.load()?.unwrap_or_else(StoredAuthBank::empty)))
    }

    pub(super) fn update_bank_with<T>(
        &self,
        action: impl FnOnce(&mut StoredAuthBank) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        self.update_bank(action)
    }

    pub(super) fn load_account(
        &self,
        account_id: &str,
    ) -> std::io::Result<Option<StoredAuthSnapshot>> {
        self.transaction(|backend| Ok(backend.load()?.and_then(|bank| bank.snapshot(account_id))))
    }

    pub(super) fn change_marker(&self) -> std::io::Result<Option<StoredAuthSnapshot>> {
        // The notification file is only a wakeup. Read the authoritative bank so a missed
        // notification or a direct credential-file replacement cannot hide an account change.
        self.transaction(|backend| Ok(backend.load()?.and_then(|bank| bank.selected())))
    }

    pub(super) async fn acquire_account_lock(
        &self,
        account_id: &str,
    ) -> std::io::Result<AccountRefreshLock> {
        match &self.lock {
            StorageLock::Persistent(codex_home) => {
                let codex_home = codex_home.clone();
                let account_id = account_id.to_string();
                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&codex_home)?;
                    let mut options = OpenOptions::new();
                    options.read(true).write(true).create(true).truncate(false);
                    #[cfg(unix)]
                    options.mode(0o600);
                    let path = account_lock_path(&codex_home, &account_id);
                    let file = options.open(path)?;
                    file.lock()?;
                    Ok(AccountRefreshLock::Persistent { _file: file })
                })
                .await
                .map_err(|err| std::io::Error::other(format!("account lock task failed: {err}")))?
            }
            StorageLock::Ephemeral => {
                let key = account_id.to_string();
                let lock = {
                    let mut locks = EPHEMERAL_ACCOUNT_LOCKS.lock().map_err(|_| {
                        std::io::Error::other("failed to lock ephemeral account locks")
                    })?;
                    Arc::clone(
                        locks
                            .entry(key)
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
                    )
                };
                Ok(AccountRefreshLock::Ephemeral {
                    _guard: lock.lock_owned().await,
                })
            }
        }
    }

    fn publish_change(&self) {
        let StorageLock::Persistent(codex_home) = &self.lock else {
            return;
        };
        if let Err(err) = publish_change(codex_home) {
            warn!("failed to publish auth change marker: {err}");
        }
    }
}

fn account_lock_path(codex_home: &Path, account_id: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(account_id.as_bytes());
    codex_home.join(format!(".auth-account-lock-{:x}", hasher.finalize()))
}

fn publish_change(codex_home: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = codex_home.join(AUTH_CHANGE_FILE);
    let temporary = codex_home.join(format!(".auth-change-{:016x}.tmp", rand::random::<u64>()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    let result = (|| {
        writeln!(file, "{:016x}", rand::random::<u64>())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(codex_home)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub(super) fn new_account_id(accounts: &BTreeMap<String, StoredAuthAccount>) -> String {
    loop {
        let id = format!("{:032x}", rand::random::<u128>());
        if id != LEGACY_ACCOUNT_ID && !accounts.contains_key(&id) {
            return id;
        }
    }
}

fn unique_default_label(bank: &StoredAuthBank) -> String {
    if !bank
        .accounts
        .values()
        .any(|account| account.label == "default")
    {
        return "default".to_string();
    }
    let mut suffix = 2;
    loop {
        let label = format!("default-{suffix}");
        if !bank.accounts.values().any(|account| account.label == label) {
            return label;
        }
        suffix += 1;
    }
}

#[derive(Clone, Debug)]
pub(super) struct FileAuthStorage {
    codex_home: PathBuf,
}

impl FileAuthStorage {
    pub(super) fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }
}

impl AuthStorageBackend for FileAuthStorage {
    fn load(&self) -> std::io::Result<Option<StoredAuthBank>> {
        let auth_file = get_auth_file(&self.codex_home);
        let mut file = match File::open(&auth_file) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(Some(decode_stored_bank(&contents)?))
    }

    fn save(&self, bank: &StoredAuthBank) -> std::io::Result<()> {
        let auth_file = get_auth_file(&self.codex_home);

        if let Some(parent) = auth_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json_data = encode_stored_bank(bank)?;
        let temporary = self
            .codex_home
            .join(format!(".auth-{:016x}.tmp", rand::random::<u64>()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = (|| {
            file.write_all(json_data.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, auth_file)?;
            #[cfg(unix)]
            File::open(&self.codex_home)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn delete(&self) -> std::io::Result<bool> {
        delete_file_if_exists(&self.codex_home)
    }
}

static CODEX_AUTH_SECRET_NAME: Lazy<SecretName> =
    Lazy::new(|| match SecretName::new("CODEX_AUTH") {
        Ok(name) => name,
        Err(err) => unreachable!("CODEX_AUTH should be a valid secret name: {err}"),
    });
const KEYRING_SERVICE: &str = "Codex Auth";

// turns codex_home path into a stable, short key string
fn compute_store_key(codex_home: &Path) -> std::io::Result<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = hex.get(..16).unwrap_or(&hex);
    Ok(format!("cli|{truncated}"))
}

#[derive(Clone, Debug)]
struct DirectKeyringAuthStorage {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    store_key: once_cell::sync::OnceCell<String>,
}

impl DirectKeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            codex_home,
            keyring_store,
            store_key: once_cell::sync::OnceCell::new(),
        }
    }

    fn store_key(&self) -> std::io::Result<&str> {
        self.store_key
            .get_or_try_init(|| compute_store_key(&self.codex_home))
            .map(String::as_str)
    }

    fn load_from_keyring(&self, key: &str) -> std::io::Result<Option<String>> {
        match self.keyring_store.load(KEYRING_SERVICE, key) {
            Ok(Some(serialized)) => Ok(Some(serialized)),
            Ok(None) => Ok(None),
            Err(error) => Err(std::io::Error::other(format!(
                "failed to load CLI auth from keyring: {}",
                error.message()
            ))),
        }
    }

    fn save_to_keyring(&self, key: &str, value: &str) -> std::io::Result<()> {
        match self.keyring_store.save(KEYRING_SERVICE, key, value) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!(
                    "failed to write OAuth tokens to keyring: {}",
                    error.message()
                );
                warn!("{message}");
                Err(std::io::Error::other(message))
            }
        }
    }
}

impl AuthStorageBackend for DirectKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<StoredAuthBank>> {
        let key = self.store_key()?;
        match self.load_from_keyring(key)? {
            Some(serialized) => decode_stored_bank(&serialized).map(Some),
            None => Ok(None),
        }
    }

    fn save(&self, bank: &StoredAuthBank) -> std::io::Result<()> {
        let key = self.store_key()?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = encode_stored_bank(bank)?;
        self.save_to_keyring(key, &serialized)?;
        if let Err(err) = delete_file_if_exists(&self.codex_home) {
            warn!("failed to remove CLI auth fallback file: {err}");
        }
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let key = self.store_key()?;
        let keyring_removed = self
            .keyring_store
            .delete(KEYRING_SERVICE, key)
            .map_err(|err| {
                std::io::Error::other(format!("failed to delete auth from keyring: {err}"))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        Ok(keyring_removed || file_removed)
    }
}

#[derive(Clone)]
struct SecretsKeyringAuthStorage {
    codex_home: PathBuf,
    direct_storage: DirectKeyringAuthStorage,
    secrets_manager: SecretsManager,
}

impl Debug for SecretsKeyringAuthStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsKeyringAuthStorage")
            .field("codex_home", &self.codex_home)
            .finish_non_exhaustive()
    }
}

impl SecretsKeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        let direct_storage =
            DirectKeyringAuthStorage::new(codex_home.clone(), Arc::clone(&keyring_store));
        let secrets_manager = SecretsManager::new_with_keyring_store_and_namespace(
            codex_home.clone(),
            SecretsBackendKind::Local,
            keyring_store,
            LocalSecretsNamespace::CodexAuth,
        );
        Self {
            codex_home,
            direct_storage,
            secrets_manager,
        }
    }
}

impl AuthStorageBackend for SecretsKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<StoredAuthBank>> {
        match self
            .secrets_manager
            .get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to load CLI auth from encrypted auth storage: {err}"
                ))
            })? {
            Some(serialized) => decode_stored_bank(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from encrypted auth storage: {err}"
                ))
            }),
            None => Ok(None),
        }
    }

    fn save(&self, bank: &StoredAuthBank) -> std::io::Result<()> {
        let serialized = encode_stored_bank(bank)?;
        self.secrets_manager
            .set(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME, &serialized)
            .map_err(|err| {
                let message =
                    format!("failed to write OAuth tokens to encrypted auth storage: {err}");
                warn!("{message}");
                std::io::Error::other(message)
            })?;
        if let Err(err) = delete_file_if_exists(&self.codex_home) {
            warn!("failed to remove CLI auth fallback file: {err}");
        }
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let keyring_removed = self
            .secrets_manager
            .delete(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to delete auth from encrypted auth storage: {err}"
                ))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        let direct_removed = self.direct_storage.delete()?;
        Ok(keyring_removed || file_removed || direct_removed)
    }
}

#[derive(Clone, Debug)]
struct AutoAuthStorage {
    keyring_storage: Arc<dyn AuthStorageBackend>,
    file_storage: Arc<FileAuthStorage>,
}

impl AutoAuthStorage {
    fn new(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            keyring_storage: create_keyring_auth_storage(
                codex_home.clone(),
                keyring_store,
                keyring_backend_kind,
            ),
            file_storage: Arc::new(FileAuthStorage::new(codex_home)),
        }
    }
}

impl AuthStorageBackend for AutoAuthStorage {
    fn load(&self) -> std::io::Result<Option<StoredAuthBank>> {
        match self.keyring_storage.load() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.file_storage.load(),
            Err(err) => {
                warn!("failed to load CLI auth from keyring, falling back to file storage: {err}");
                self.file_storage.load()
            }
        }
    }

    fn save(&self, bank: &StoredAuthBank) -> std::io::Result<()> {
        match self.keyring_storage.save(bank) {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!("failed to save auth to keyring, falling back to file storage: {err}");
                self.file_storage.save(bank)
            }
        }
    }

    fn delete(&self) -> std::io::Result<bool> {
        // Keyring storage will delete from disk as well
        self.keyring_storage.delete()
    }
}

// A global in-memory store for mapping codex_home -> AuthDotJson.
static EPHEMERAL_AUTH_STORE: Lazy<Mutex<HashMap<String, StoredAuthBank>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug)]
struct EphemeralAuthStorage {
    codex_home: PathBuf,
}

impl EphemeralAuthStorage {
    fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn with_store<F, T>(&self, action: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut HashMap<String, StoredAuthBank>, String) -> std::io::Result<T>,
    {
        let key = compute_store_key(&self.codex_home)?;
        let mut store = EPHEMERAL_AUTH_STORE
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock ephemeral auth storage"))?;
        action(&mut store, key)
    }
}

impl AuthStorageBackend for EphemeralAuthStorage {
    fn load(&self) -> std::io::Result<Option<StoredAuthBank>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save(&self, bank: &StoredAuthBank) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, bank.clone());
            Ok(())
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.with_store(|store, key| Ok(store.remove(&key).is_some()))
    }
}

pub(super) fn create_auth_storage(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<AuthStorage> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    create_auth_storage_with_store(codex_home, mode, keyring_store, keyring_backend_kind)
}

fn create_auth_storage_with_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<AuthStorage> {
    let lock = match mode {
        AuthCredentialsStoreMode::Ephemeral => StorageLock::Ephemeral,
        _ => StorageLock::Persistent(codex_home.clone()),
    };
    let backend: Arc<dyn AuthStorageBackend> = match mode {
        AuthCredentialsStoreMode::File => Arc::new(FileAuthStorage::new(codex_home)),
        AuthCredentialsStoreMode::Keyring => {
            create_keyring_auth_storage(codex_home, keyring_store, keyring_backend_kind)
        }
        AuthCredentialsStoreMode::Auto => Arc::new(AutoAuthStorage::new(
            codex_home,
            keyring_store,
            keyring_backend_kind,
        )),
        AuthCredentialsStoreMode::Ephemeral => Arc::new(EphemeralAuthStorage::new(codex_home)),
    };
    Arc::new(AuthStorage { backend, lock })
}

fn create_keyring_auth_storage(
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            Arc::new(DirectKeyringAuthStorage::new(codex_home, keyring_store))
        }
        AuthKeyringBackendKind::Secrets => {
            Arc::new(SecretsKeyringAuthStorage::new(codex_home, keyring_store))
        }
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
