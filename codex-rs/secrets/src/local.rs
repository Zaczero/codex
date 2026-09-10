use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::atomic::compiler_fence;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use age::Decryptor;
use age::encrypt;
use age::scrypt::Identity as ScryptIdentity;
use age::secrecy::ExposeSecret;
use age::secrecy::ExposeSecretMut;
use age::secrecy::SecretBox;
use age::secrecy::SecretSlice;
use age::secrecy::SecretString;
use age::x25519::Identity;
use anyhow::Context;
use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bech32::ToBase32;
use codex_keyring_store::KeyringStore;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use super::SecretListEntry;
use super::SecretName;
use super::SecretScope;
use super::SecretsBackend;
use super::compute_keyring_account;
use super::keyring_service;

const SECRETS_VERSION: u8 = 1;
const LOCAL_SECRETS_FILENAME: &str = "local.age";
const CODEX_AUTH_SECRETS_FILENAME: &str = "codex_auth.age";
const MCP_OAUTH_SECRETS_FILENAME: &str = "mcp_oauth.age";

/// Selects the local encrypted file used by a `LocalSecretsBackend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalSecretsNamespace {
    /// General managed secrets stored in `local.age`.
    #[default]
    ManagedSecrets,
    /// Codex authentication credentials used by the CLI, TUI, app server, and other clients.
    CodexAuth,
    /// OAuth credentials for external MCP servers.
    McpOAuth,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SecretsFile {
    version: u8,
    secrets: BTreeMap<String, String>,
}

impl SecretsFile {
    fn new_empty() -> Self {
        Self {
            version: SECRETS_VERSION,
            secrets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalSecretsBackend {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    namespace: LocalSecretsNamespace,
}

impl LocalSecretsBackend {
    pub fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self::new_with_namespace(
            codex_home,
            keyring_store,
            LocalSecretsNamespace::ManagedSecrets,
        )
    }

    pub fn new_with_namespace(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        namespace: LocalSecretsNamespace,
    ) -> Self {
        Self {
            codex_home,
            keyring_store,
            namespace,
        }
    }

    pub fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        anyhow::ensure!(!value.is_empty(), "secret value must not be empty");
        let _lock = self.lock()?;
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        file.secrets.insert(canonical_key, value.to_string());
        self.save_file(&file)
    }

    pub fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        let _lock = self.lock()?;
        let canonical_key = scope.canonical_key(name);
        let file = self.load_file()?;
        Ok(file.secrets.get(&canonical_key).cloned())
    }

    pub fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        let _lock = self.lock()?;
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        let removed = file.secrets.remove(&canonical_key).is_some();
        if removed {
            self.save_file(&file)?;
        }
        Ok(removed)
    }

    pub fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        let _lock = self.lock()?;
        let file = self.load_file()?;
        let mut entries = Vec::new();
        for canonical_key in file.secrets.keys() {
            let Some(entry) = parse_canonical_key(canonical_key) else {
                warn!("skipping invalid canonical secret key: {canonical_key}");
                continue;
            };
            if let Some(scope) = scope_filter
                && entry.scope != *scope
            {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    fn secrets_dir(&self) -> PathBuf {
        self.codex_home.join("secrets")
    }

    fn lock(&self) -> Result<File> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create secrets dir {}", dir.display()))?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // All namespaces share the keyring key; serialize its creation as well as file updates.
        let lock = options
            .open(dir.join(".lock"))
            .context("failed to open secrets lock")?;
        lock.lock().context("failed to lock secrets store")?;
        Ok(lock)
    }

    fn secrets_path(&self) -> PathBuf {
        let filename = match self.namespace {
            LocalSecretsNamespace::ManagedSecrets => LOCAL_SECRETS_FILENAME,
            LocalSecretsNamespace::CodexAuth => CODEX_AUTH_SECRETS_FILENAME,
            LocalSecretsNamespace::McpOAuth => MCP_OAUTH_SECRETS_FILENAME,
        };
        self.secrets_dir().join(filename)
    }

    fn load_file(&self) -> Result<SecretsFile> {
        let path = self.secrets_path();
        let ciphertext = match fs::read(&path) {
            Ok(ciphertext) => ciphertext,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SecretsFile::new_empty());
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read secrets file at {}", path.display()));
            }
        };
        let key = self
            .load_key()?
            .context("secrets file exists but its keyring key is missing")?;
        let identity = identity_from_key(&key)?;
        let decryptor =
            Decryptor::new_buffered(ciphertext.as_slice()).context("invalid secrets file")?;
        let migrate = decryptor.is_scrypt();
        let passphrase_identity;
        let decryption_identity: &dyn age::Identity = if migrate {
            passphrase_identity = ScryptIdentity::new(key);
            &passphrase_identity
        } else {
            &identity
        };
        let mut plaintext = SecretBox::new(Box::new(Vec::new()));
        decryptor
            .decrypt(std::iter::once(decryption_identity))
            .context("failed to decrypt secrets file")?
            .read_to_end(plaintext.expose_secret_mut())
            .context("failed to read decrypted secrets file")?;
        let mut parsed: SecretsFile = serde_json::from_slice(plaintext.expose_secret())
            .with_context(|| {
                format!(
                    "failed to deserialize decrypted secrets file at {}",
                    path.display()
                )
            })?;
        if parsed.version == 0 {
            parsed.version = SECRETS_VERSION;
        }
        anyhow::ensure!(
            parsed.version <= SECRETS_VERSION,
            "secrets file version {} is newer than supported version {}",
            parsed.version,
            SECRETS_VERSION
        );
        if migrate {
            // Keep the shared keyring key unchanged so each namespace can upgrade independently.
            let ciphertext = encrypt(&identity.to_public(), plaintext.expose_secret())
                .context("failed to upgrade secrets encryption")?;
            write_file_atomically(&path, &ciphertext)?;
        }
        Ok(parsed)
    }

    fn save_file(&self, file: &SecretsFile) -> Result<()> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create secrets dir {}", dir.display()))?;

        let key = match self.load_key()? {
            Some(key) => key,
            None => {
                for entry in fs::read_dir(&dir)? {
                    anyhow::ensure!(
                        entry?
                            .path()
                            .extension()
                            .is_none_or(|extension| extension != "age"),
                        "secrets file exists but its keyring key is missing"
                    );
                }
                let key = generate_key()?;
                self.keyring_store
                    .save(
                        keyring_service(),
                        &compute_keyring_account(&self.codex_home),
                        key.expose_secret(),
                    )
                    .map_err(|err| anyhow::anyhow!(err.message()))
                    .context("failed to persist secrets key in keyring")?;
                key
            }
        };
        let identity = identity_from_key(&key)?;
        let plaintext = SecretSlice::from(
            serde_json::to_vec(file).context("failed to serialize secrets file")?,
        );
        let ciphertext = encrypt(&identity.to_public(), plaintext.expose_secret())
            .context("failed to encrypt secrets file")?;
        let path = self.secrets_path();
        write_file_atomically(&path, &ciphertext)
    }

    fn load_key(&self) -> Result<Option<SecretString>> {
        let account = compute_keyring_account(&self.codex_home);
        self.keyring_store
            .load(keyring_service(), &account)
            .map(|value| value.map(SecretString::from))
            .map_err(|err| anyhow::anyhow!(err.message()))
            .with_context(|| format!("failed to load secrets key from keyring for {account}"))
    }
}

impl SecretsBackend for LocalSecretsBackend {
    fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        LocalSecretsBackend::set(self, scope, name, value)
    }

    fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        LocalSecretsBackend::get(self, scope, name)
    }

    fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        LocalSecretsBackend::delete(self, scope, name)
    }

    fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        LocalSecretsBackend::list(self, scope_filter)
    }
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().with_context(|| {
        format!(
            "failed to compute parent directory for secrets file at {}",
            path.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let filename = path.file_name().with_context(|| {
        format!(
            "failed to compute filename for secrets file at {}",
            path.display()
        )
    })?;
    let tmp_path = dir.join(format!(
        ".{}.tmp-{}-{nonce}",
        filename.to_string_lossy(),
        std::process::id()
    ));

    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut tmp_file = options.open(&tmp_path).with_context(|| {
        format!(
            "failed to create temp secrets file at {}",
            tmp_path.display()
        )
    })?;
    let result = (|| {
        tmp_file.write_all(contents).with_context(|| {
            format!(
                "failed to write temp secrets file at {}",
                tmp_path.display()
            )
        })?;
        tmp_file.sync_all().with_context(|| {
            format!("failed to sync temp secrets file at {}", tmp_path.display())
        })?;
        drop(tmp_file);
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to atomically replace secrets file at {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        File::open(dir)?
            .sync_all()
            .context("failed to sync secrets directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn generate_key() -> Result<SecretString> {
    let mut bytes = [0_u8; 32];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut bytes)
        .context("failed to generate random secrets key")?;
    // Base64 keeps the keyring payload ASCII-safe without reducing entropy.
    let encoded = BASE64_STANDARD.encode(bytes);
    wipe_bytes(&mut bytes);
    Ok(SecretString::from(encoded))
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // Volatile writes make it much harder for the compiler to elide the wipe.
        // SAFETY: `byte` is a valid mutable reference into `bytes`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

fn identity_from_key(key: &SecretString) -> Result<Identity> {
    let bytes = SecretSlice::from(
        BASE64_STANDARD
            .decode(key.expose_secret())
            .context("invalid secrets key encoding")?,
    );
    anyhow::ensure!(
        bytes.expose_secret().len() == 32,
        "invalid secrets key length"
    );
    // age's public identity constructor accepts Bech32, not raw key bytes.
    let encoded = SecretString::from(bech32::encode(
        "age-secret-key-",
        bytes.expose_secret().to_base32(),
        bech32::Variant::Bech32,
    )?);
    encoded.expose_secret().parse().map_err(anyhow::Error::msg)
}

fn parse_canonical_key(canonical_key: &str) -> Option<SecretListEntry> {
    let mut parts = canonical_key.split('/');
    let scope_kind = parts.next()?;
    match scope_kind {
        "global" => {
            let name = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let name = SecretName::new(name).ok()?;
            Some(SecretListEntry {
                scope: SecretScope::Global,
                name,
            })
        }
        "env" => {
            let environment_id = parts.next()?;
            let name = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let name = SecretName::new(name).ok()?;
            let scope = SecretScope::environment(environment_id.to_string()).ok()?;
            Some(SecretListEntry { scope, name })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::decrypt;
    use age::scrypt::Recipient as ScryptRecipient;
    use bech32::FromBase32;
    use codex_keyring_store::tests::MockKeyringStore;
    use keyring::Error as KeyringError;
    use pretty_assertions::assert_eq;

    #[test]
    fn load_file_rejects_newer_schema_versions() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);

        let file = SecretsFile {
            version: SECRETS_VERSION + 1,
            secrets: BTreeMap::new(),
        };
        backend.save_file(&file)?;

        let error = backend
            .load_file()
            .expect_err("must reject newer schema version");
        assert!(
            error.to_string().contains("newer than supported version"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn set_fails_when_keyring_is_unavailable() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let account = compute_keyring_account(codex_home.path());
        keyring.set_error(
            &account,
            KeyringError::Invalid("error".into(), "load".into()),
        );

        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);
        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        let error = backend
            .set(&scope, &name, "secret-value")
            .expect_err("must fail when keyring load fails");
        assert!(
            error
                .to_string()
                .contains("failed to load secrets key from keyring"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn save_file_does_not_leave_temp_files() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);

        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        backend.set(&scope, &name, "one")?;
        backend.set(&scope, &name, "two")?;

        let secrets_dir = backend.secrets_dir();
        let entries = fs::read_dir(&secrets_dir)
            .with_context(|| format!("failed to read {}", secrets_dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to enumerate {}", secrets_dir.display()))?;

        let filenames: Vec<String> = entries
            .into_iter()
            .filter_map(|entry| entry.file_name().to_str().map(ToString::to_string))
            .collect();
        let mut filenames = filenames;
        filenames.sort();
        assert_eq!(
            filenames,
            vec![".lock".to_string(), LOCAL_SECRETS_FILENAME.to_string()]
        );
        assert_eq!(backend.get(&scope, &name)?, Some("two".to_string()));
        Ok(())
    }

    #[test]
    fn failed_replacement_preserves_the_destination_and_removes_its_temp_file() -> Result<()> {
        let home = tempfile::tempdir()?;
        let destination = home.path().join("destination");
        fs::create_dir(&destination)?;
        fs::write(destination.join("original"), b"retained")?;
        assert!(write_file_atomically(&destination, b"replacement").is_err());
        assert_eq!(fs::read(destination.join("original"))?, b"retained");
        let paths = fs::read_dir(home.path())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(paths, vec![destination]);
        Ok(())
    }

    #[test]
    fn local_namespaces_write_separate_files() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let codex_auth_backend = LocalSecretsBackend::new_with_namespace(
            codex_home.path().to_path_buf(),
            keyring.clone(),
            LocalSecretsNamespace::CodexAuth,
        );
        let mcp_backend = LocalSecretsBackend::new_with_namespace(
            codex_home.path().to_path_buf(),
            keyring,
            LocalSecretsNamespace::McpOAuth,
        );
        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;

        codex_auth_backend.set(&scope, &name, "codex-auth-value")?;
        mcp_backend.set(&scope, &name, "mcp-value")?;

        assert_eq!(
            codex_auth_backend.get(&scope, &name)?,
            Some("codex-auth-value".to_string())
        );
        assert_eq!(
            mcp_backend.get(&scope, &name)?,
            Some("mcp-value".to_string())
        );
        assert!(
            codex_home
                .path()
                .join("secrets")
                .join("codex_auth.age")
                .exists()
        );
        assert!(
            codex_home
                .path()
                .join("secrets")
                .join("mcp_oauth.age")
                .exists()
        );
        assert!(!codex_home.path().join("secrets").join("local.age").exists());
        Ok(())
    }

    #[test]
    fn independent_backends_observe_updates_and_removals() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let first = LocalSecretsBackend::new_with_namespace(
            codex_home.path().to_path_buf(),
            keyring.clone(),
            LocalSecretsNamespace::McpOAuth,
        );
        let second = LocalSecretsBackend::new_with_namespace(
            codex_home.path().to_path_buf(),
            keyring,
            LocalSecretsNamespace::McpOAuth,
        );
        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        first.set(&scope, &name, "one")?;
        std::thread::scope(|threads| {
            let first_reader = threads.spawn(|| {
                assert_eq!(first.get(&scope, &name)?, Some("one".to_string()));
                Ok::<_, anyhow::Error>(())
            });
            let second_reader = threads.spawn(|| {
                assert_eq!(second.get(&scope, &name)?, Some("one".to_string()));
                Ok::<_, anyhow::Error>(())
            });
            Ok::<_, anyhow::Error>((
                first_reader.join().expect("first credential reader")?,
                second_reader.join().expect("second credential reader")?,
            ))
        })?;
        assert_eq!(second.get(&scope, &name)?, Some("one".to_string()));
        first.set(&scope, &name, "two")?;
        assert_eq!(second.get(&scope, &name)?, Some("two".to_string()));
        assert!(first.delete(&scope, &name)?);
        assert_eq!(second.get(&scope, &name)?, None);

        Ok(())
    }

    fn seed_key(codex_home: &Path, keyring: &MockKeyringStore) -> Result<Identity> {
        let identity = Identity::generate();
        let (_, encoded, _) = bech32::decode(identity.to_string().expose_secret())?;
        let bytes = Vec::<u8>::from_base32(&encoded)?;
        keyring.save(
            keyring_service(),
            &compute_keyring_account(codex_home),
            &BASE64_STANDARD.encode(bytes),
        )?;
        Ok(identity)
    }

    #[test]
    fn files_use_key_based_encryption() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let identity = seed_key(home.path(), &keyring)?;
        let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring);
        backend.set(
            &SecretScope::Global,
            &SecretName::new("API_TOKEN")?,
            "private-token",
        )?;
        let ciphertext = fs::read(backend.secrets_path())?;
        let plaintext = decrypt(&identity, &ciphertext)?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&plaintext)?,
            serde_json::json!({
                "version": 1, "secrets": { "global/API_TOKEN": "private-token" },
            })
        );
        assert!(decrypt(&Identity::generate(), &ciphertext).is_err());
        Ok(())
    }

    #[test]
    fn passphrase_files_upgrade_independently_without_rotating_the_shared_key() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let identity = seed_key(home.path(), &keyring)?;
        let account = compute_keyring_account(home.path());
        let key = keyring.saved_value(&account).expect("seeded key");
        let mut recipient = ScryptRecipient::new(SecretString::from(key.clone()));
        recipient.set_work_factor(/*log_n*/ 1);
        let plaintext =
            br#"{"version":1,"secrets":{"global/ALPHA":"one","env/server/BETA":"two"}}"#;
        let original = encrypt(&recipient, plaintext)?;
        fs::create_dir(home.path().join("secrets"))?;
        let backends = [
            LocalSecretsNamespace::ManagedSecrets,
            LocalSecretsNamespace::CodexAuth,
            LocalSecretsNamespace::McpOAuth,
        ]
        .map(|namespace| {
            LocalSecretsBackend::new_with_namespace(
                home.path().to_path_buf(),
                keyring.clone(),
                namespace,
            )
        });
        for backend in &backends {
            fs::write(backend.secrets_path(), &original)?;
        }
        for backend in &backends {
            assert_eq!(
                backend.get(&SecretScope::Global, &SecretName::new("ALPHA")?)?,
                Some("one".to_string())
            );
            let upgraded = fs::read(backend.secrets_path())?;
            assert_eq!(decrypt(&identity, &upgraded)?, plaintext);
            assert_eq!(keyring.saved_value(&account), Some(key.clone()));
            let reopened = LocalSecretsBackend::new_with_namespace(
                home.path().to_path_buf(),
                keyring.clone(),
                backend.namespace,
            );
            assert_eq!(
                reopened.get(
                    &SecretScope::environment("server")?,
                    &SecretName::new("BETA")?
                )?,
                Some("two".to_string())
            );
            assert_eq!(fs::read(backend.secrets_path())?, upgraded);
        }
        Ok(())
    }

    #[test]
    fn missing_key_never_replaces_credentials_in_another_namespace() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring.clone());
        let name = SecretName::new("TOKEN")?;
        backend.set(&SecretScope::Global, &name, "retained")?;
        let original = fs::read(backend.secrets_path())?;
        let account = compute_keyring_account(home.path());
        keyring.delete(keyring_service(), &account)?;
        let sibling = LocalSecretsBackend::new_with_namespace(
            home.path().to_path_buf(),
            keyring.clone(),
            LocalSecretsNamespace::CodexAuth,
        );
        assert!(backend.get(&SecretScope::Global, &name).is_err());
        assert!(
            sibling
                .set(&SecretScope::Global, &name, "replacement")
                .is_err()
        );
        assert_eq!(keyring.saved_value(&account), None);
        assert_eq!(fs::read(backend.secrets_path())?, original);
        Ok(())
    }

    #[test]
    fn failed_upgrade_leaves_the_original_file_untouched() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        seed_key(home.path(), &keyring)?;
        let account = compute_keyring_account(home.path());
        let key = keyring.saved_value(&account).expect("seeded key");
        let mut recipient = ScryptRecipient::new(SecretString::from(key.clone()));
        recipient.set_work_factor(/*log_n*/ 1);
        let plaintext = br#"{"version":1,"secrets":{"global/TOKEN":"retained"}}"#;
        let original = encrypt(&recipient, plaintext)?;
        let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring.clone());
        fs::create_dir(backend.secrets_dir())?;
        fs::write(backend.secrets_path(), &original)?;
        seed_key(home.path(), &keyring)?;
        assert!(
            backend
                .get(&SecretScope::Global, &SecretName::new("TOKEN")?)
                .is_err()
        );
        assert_eq!(fs::read(backend.secrets_path())?, original);
        keyring.save(keyring_service(), &account, &key)?;
        let mut damaged = original;
        *damaged.last_mut().expect("encrypted payload") ^= 1;
        fs::write(backend.secrets_path(), &damaged)?;
        assert!(
            backend
                .get(&SecretScope::Global, &SecretName::new("TOKEN")?)
                .is_err()
        );
        assert_eq!(fs::read(backend.secrets_path())?, damaged);
        Ok(())
    }

    #[test]
    fn concurrent_writers_preserve_all_secrets() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring);
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|threads| {
            let workers: Vec<_> = (0..8)
                .map(|index| {
                    let backend = &backend;
                    let start = &start;
                    threads.spawn(move || {
                        start.wait();
                        backend.set(
                            &SecretScope::Global,
                            &SecretName::new(format!("TOKEN_{index}").as_str())?,
                            &format!("value-{index}"),
                        )
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("secret writer")?;
            }
            Ok::<_, anyhow::Error>(())
        })?;
        let values = backend
            .list(/*scope_filter*/ None)?
            .iter()
            .map(|entry| backend.get(&entry.scope, &entry.name))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            values,
            (0..8)
                .map(|index| Some(format!("value-{index}")))
                .collect::<Vec<_>>()
        );
        Ok(())
    }
}
