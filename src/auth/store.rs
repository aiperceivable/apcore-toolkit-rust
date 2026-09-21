// TokenStore protocol and the portable file-backed implementation.
//
// The toolkit ships the `0600` file half only. OS keychains are a consumer
// concern -- macOS Keychain, Windows Credential Manager, and Linux Secret
// Service have different semantics reached through three unrelated libraries,
// and cross-language behavioural parity is not achievable there. See "Why the
// toolkit does not ship a keychain integration" in the spec.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::auth::error::TokenStoreError;
use crate::auth::token::TokenSet;

/// The canonical store key for an authorization server plus client.
///
/// `"<issuer>|<client_id>"`, so credentials for several authorization servers
/// coexist in one store without collision. MCP requires credentials keyed by
/// the issuing server's `issuer`; this satisfies that by construction.
pub fn store_key(issuer: &str, client_id: &str) -> String {
    format!("{issuer}|{client_id}")
}

/// Persistence for a [`TokenSet`].
///
/// Implement this to back credentials with an OS keychain. A keychain store
/// MUST define its degraded path: silently failing to persist is the worst
/// outcome, because the user appears to log in and is prompted again on every
/// invocation with no indication why.
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Read the record for `key`. A missing store is not an error: it returns
    /// `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`TokenStoreError::Permission`] when an existing file is
    /// readable by users other than its owner, and propagates I/O errors.
    async fn load(&self, key: &str) -> Result<Option<TokenSet>, TokenStoreError>;

    /// Replace the record for `key`. MUST be atomic and MUST create with
    /// `0600` on POSIX.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors.
    async fn save(&self, key: &str, tokens: &TokenSet) -> Result<(), TokenStoreError>;

    /// Remove the record for `key`. Idempotent: clearing an absent key is not
    /// an error.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors.
    async fn clear(&self, key: &str) -> Result<(), TokenStoreError>;
}

/// A store that keeps nothing. Useful for daemons that hold credentials in
/// memory only, and as the default when a caller supplies no store.
#[derive(Debug, Default)]
pub struct NullTokenStore;

#[async_trait]
impl TokenStore for NullTokenStore {
    async fn load(&self, _key: &str) -> Result<Option<TokenSet>, TokenStoreError> {
        Ok(None)
    }

    async fn save(&self, _key: &str, _tokens: &TokenSet) -> Result<(), TokenStoreError> {
        Ok(())
    }

    async fn clear(&self, _key: &str) -> Result<(), TokenStoreError> {
        Ok(())
    }
}

/// The portable, file-backed store: one JSON object keyed by
/// `"<issuer>|<client_id>"`, mode `0600`, replaced atomically.
///
/// The default path is normative and documented, because other tools maintain
/// credential-path baselines that need to include it:
///
/// | Platform | Path |
/// |---|---|
/// | Linux / BSD | `$XDG_CONFIG_HOME/apcore/credentials.json`, else `~/.config/apcore/credentials.json` |
/// | macOS | `~/.config/apcore/credentials.json` |
/// | Windows | `%APPDATA%\apcore\credentials.json` |
///
/// macOS deliberately uses the XDG-style path rather than
/// `~/Library/Application Support`, so cross-platform tooling sees one
/// location.
#[derive(Debug, Clone)]
pub struct FileTokenStore {
    path: PathBuf,
}

impl Default for FileTokenStore {
    fn default() -> Self {
        Self {
            path: default_credentials_path(),
        }
    }
}

/// The documented default credentials path for this platform.
pub fn default_credentials_path() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("apcore").join(CREDENTIALS_FILE);
        }
    }
    #[cfg(not(windows))]
    {
        if !cfg!(target_os = "macos") {
            if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                if !xdg.is_empty() {
                    return PathBuf::from(xdg).join("apcore").join(CREDENTIALS_FILE);
                }
            }
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("apcore").join(CREDENTIALS_FILE)
}

/// File name of the credentials document, shared by every platform path.
const CREDENTIALS_FILE: &str = "credentials.json";

/// POSIX permission bits that any other user could use. Anything set here on
/// an existing file is grounds for refusing to read it.
#[cfg(unix)]
const BROADER_THAN_OWNER: u32 = 0o077;

impl FileTokenStore {
    /// A store at an explicit path, for tests and for consumers that manage
    /// their own configuration directory.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Refuse to read a file other local users can see.
    ///
    /// Checked before the read, not after, so an over-permissive file never
    /// has its contents loaded into the process at all.
    #[cfg(unix)]
    fn check_permissions(&self) -> Result<(), TokenStoreError> {
        use std::os::unix::fs::PermissionsExt;

        let metadata = std::fs::metadata(&self.path).map_err(|source| TokenStoreError::Io {
            path: self.path.display().to_string(),
            source,
        })?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & BROADER_THAN_OWNER != 0 {
            return Err(TokenStoreError::Permission {
                path: self.path.display().to_string(),
                mode,
            });
        }
        Ok(())
    }

    /// On Windows the file inherits the ACL of `%APPDATA%`, which is already
    /// user-scoped, so there is no mode to check.
    #[cfg(not(unix))]
    fn check_permissions(&self) -> Result<(), TokenStoreError> {
        Ok(())
    }

    /// Read the whole document. A missing file is an empty document.
    fn read_all(&self) -> Result<BTreeMap<String, TokenSet>, TokenStoreError> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        self.check_permissions()?;
        let text = std::fs::read_to_string(&self.path).map_err(|source| TokenStoreError::Io {
            path: self.path.display().to_string(),
            source,
        })?;
        if text.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_str(&text).map_err(|e| TokenStoreError::Corrupt {
            path: self.path.display().to_string(),
            message: e.to_string(),
        })
    }

    /// Write the whole document by atomic replace: a temporary file in the
    /// same directory, created `0600`, then renamed over the target.
    ///
    /// Same directory matters -- `rename` is only atomic within one
    /// filesystem. Created with the mode rather than chmod'd afterwards, so
    /// there is no window in which the file is world-readable.
    fn write_all(&self, records: &BTreeMap<String, TokenSet>) -> Result<(), TokenStoreError> {
        let directory = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(directory).map_err(|source| TokenStoreError::Io {
            path: directory.display().to_string(),
            source,
        })?;

        let text = serde_json::to_string_pretty(records).map_err(|e| TokenStoreError::Corrupt {
            path: self.path.display().to_string(),
            message: e.to_string(),
        })?;

        let temp_path = directory.join(format!(".{}.{}.tmp", CREDENTIALS_FILE, std::process::id()));
        write_owner_only(&temp_path, &text)?;

        std::fs::rename(&temp_path, &self.path).map_err(|source| {
            let _ = std::fs::remove_file(&temp_path);
            TokenStoreError::Io {
                path: self.path.display().to_string(),
                source,
            }
        })
    }
}

/// Create `path` with owner-only permissions and write `text` to it.
fn write_owner_only(path: &Path, text: &str) -> Result<(), TokenStoreError> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| TokenStoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    file.write_all(text.as_bytes())
        .map_err(|source| TokenStoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
    file.sync_all().map_err(|source| TokenStoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

#[async_trait]
impl TokenStore for FileTokenStore {
    async fn load(&self, key: &str) -> Result<Option<TokenSet>, TokenStoreError> {
        Ok(self.read_all()?.remove(key))
    }

    async fn save(&self, key: &str, tokens: &TokenSet) -> Result<(), TokenStoreError> {
        let mut records = self.read_all()?;
        records.insert(key.to_string(), tokens.clone());
        self.write_all(&records)
    }

    async fn clear(&self, key: &str) -> Result<(), TokenStoreError> {
        let mut records = self.read_all()?;
        if records.remove(key).is_none() {
            return Ok(());
        }
        self.write_all(&records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn token(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.to_string(),
            token_type: "Bearer".to_string(),
            expires_at: Some(4600),
            refresh_token: Some("r1".to_string()),
            scope: vec!["openid".to_string()],
            obtained_at: 1000,
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        futures::executor::block_on(future)
    }

    #[test]
    fn test_store_key_joins_issuer_and_client_id() {
        assert_eq!(
            store_key("https://a.example", "cid"),
            "https://a.example|cid"
        );
    }

    #[test]
    fn test_file_store_load_missing_file_returns_none() {
        let dir = TempDir::new().expect("temp dir");
        let store = FileTokenStore::at(dir.path().join("nested").join("credentials.json"));
        assert!(block_on(store.load("k")).expect("load").is_none());
    }

    #[test]
    fn test_file_store_save_then_load_roundtrip() {
        let dir = TempDir::new().expect("temp dir");
        let store = FileTokenStore::at(dir.path().join("credentials.json"));
        block_on(store.save("k", &token("t1"))).expect("save");
        let loaded = block_on(store.load("k")).expect("load").expect("some");
        assert_eq!(loaded, token("t1"));
    }

    #[test]
    fn test_file_store_keeps_other_keys_on_save() {
        let dir = TempDir::new().expect("temp dir");
        let store = FileTokenStore::at(dir.path().join("credentials.json"));
        block_on(store.save("a|1", &token("t1"))).expect("save a");
        block_on(store.save("b|2", &token("t2"))).expect("save b");
        assert_eq!(
            block_on(store.load("a|1")).expect("load").expect("some"),
            token("t1")
        );
        assert_eq!(
            block_on(store.load("b|2")).expect("load").expect("some"),
            token("t2")
        );
    }

    #[test]
    fn test_file_store_clear_is_idempotent() {
        let dir = TempDir::new().expect("temp dir");
        let store = FileTokenStore::at(dir.path().join("credentials.json"));
        block_on(store.clear("absent")).expect("clear absent is not an error");
        block_on(store.save("k", &token("t1"))).expect("save");
        block_on(store.clear("k")).expect("clear");
        assert!(block_on(store.load("k")).expect("load").is_none());
        block_on(store.clear("k")).expect("second clear is not an error");
    }

    #[cfg(unix)]
    #[test]
    fn test_file_store_creates_file_with_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("credentials.json");
        let store = FileTokenStore::at(&path);
        block_on(store.save("k", &token("t1"))).expect("save");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "created mode was {mode:04o}");
    }

    #[cfg(unix)]
    #[test]
    fn test_file_store_refuses_to_read_over_permissive_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("credentials.json");
        let store = FileTokenStore::at(&path);
        block_on(store.save("k", &token("t1"))).expect("save");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("widen permissions");

        match block_on(store.load("k")) {
            Err(TokenStoreError::Permission { mode, .. }) => assert_eq!(mode, 0o644),
            other => panic!("expected a Permission error, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_file_store_refuses_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("credentials.json");
        let store = FileTokenStore::at(&path);
        block_on(store.save("k", &token("t1"))).expect("save");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("widen permissions");
        assert!(matches!(
            block_on(store.load("k")),
            Err(TokenStoreError::Permission { .. })
        ));
    }

    #[test]
    fn test_file_store_save_leaves_no_temp_file_behind() {
        let dir = TempDir::new().expect("temp dir");
        let store = FileTokenStore::at(dir.path().join("credentials.json"));
        block_on(store.save("k", &token("t1"))).expect("save");

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn test_file_store_atomic_replace_never_exposes_partial_content() {
        // The replaced file is the rename target, so a reader either sees the
        // previous complete document or the next one -- never a truncated
        // write. Asserted by checking the target is never opened for writing:
        // the temp file carries the new bytes and the target is swapped in.
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("credentials.json");
        let store = FileTokenStore::at(&path);
        block_on(store.save("k", &token("first"))).expect("save first");

        let before = std::fs::metadata(&path).expect("metadata before");
        block_on(store.save("k", &token("second"))).expect("save second");
        let after = std::fs::metadata(&path).expect("metadata after");

        // A fresh inode means the content was swapped in, not written in place.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                before.ino(),
                after.ino(),
                "save must replace the file, not rewrite it in place"
            );
        }
        #[cfg(not(unix))]
        {
            let _ = (before, after);
        }

        assert_eq!(
            block_on(store.load("k")).expect("load").expect("some"),
            token("second")
        );
    }

    #[test]
    fn test_file_store_corrupt_document_reports_path() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("credentials.json");
        write_owner_only(&path, "{ not json").expect("write");
        let store = FileTokenStore::at(&path);
        match block_on(store.load("k")) {
            Err(TokenStoreError::Corrupt { path: reported, .. }) => {
                assert!(reported.ends_with("credentials.json"));
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn test_null_store_never_persists() {
        let store = NullTokenStore;
        block_on(store.save("k", &token("t"))).expect("save");
        assert!(block_on(store.load("k")).expect("load").is_none());
        block_on(store.clear("k")).expect("clear");
    }

    #[test]
    fn test_default_credentials_path_ends_in_documented_file() {
        let path = default_credentials_path();
        assert!(
            path.ends_with("apcore/credentials.json") || path.ends_with("apcore\\credentials.json")
        );
    }
}
