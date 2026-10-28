//! SQLCipher archive storage and automatic OS-keyring unlock.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use keyring_core::api::CredentialStoreApi;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const HEADER: &[u8; 16] = b"SQLite format 3\0";

fn plaintext(path: &Path) -> Result<bool> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    let mut header = [0; 16];
    file.read_exact(&mut header)?;
    Ok(&header == HEADER)
}

/// Keyring service of this install. The previous install's service follows
/// below; entries move over on adoption and are never read afterwards.
const KEYRING_SERVICE: &str = "rocks.zapfasterk.ZapFasterk";
/// Keyring service of Zap Faster releases; entries move to KEYRING_SERVICE.
const PREVIOUS_KEYRING_SERVICE: &str = "rocks.zapfast.ZapFast";

pub(super) fn key_for(path: &Path) -> Result<Zeroizing<[u8; 32]>> {
    let parent = path.parent().context("Archive has no parent directory")?;
    fs::create_dir_all(parent)?;
    let store = platform_store()?;
    let entry = store
        .build(KEYRING_SERVICE, &identity_for(parent)?, None)
        .map_err(keyring_error)
        .context("The OS keyring could not open Zap Fasterk's archive key")?;
    key_from_entry(path, &entry)
}

/// Identity reserved for a new account folder without publishing that folder.
pub(super) fn restore_key_identity(parent: &Path, account: &str) -> Result<String> {
    ensure!(
        crate::model::AccountId::is_safe(account),
        "Invalid restored account identifier"
    );
    let resolved = parent.canonicalize()?.join(account);
    Ok(identity_for_resolved(&resolved))
}

pub(super) fn restore_key_digest(key: &[u8]) -> String {
    Sha256::digest(key)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn ensure_restore_key_available(identity: &str) -> Result<()> {
    let entry = platform_store()?
        .build(KEYRING_SERVICE, identity, None)
        .map_err(keyring_error)?;
    match entry.get_secret() {
        Err(keyring_core::Error::NoEntry) => Ok(()),
        Ok(secret) => {
            let _secret = Zeroizing::new(secret);
            anyhow::bail!(
                "The restore destination already has an archive credential; choose another account"
            )
        }
        Err(error) => Err(keyring_error(error)),
    }
}

pub(super) fn validate_owned_restore_key(
    identity: &str,
    digest: &str,
    allow_missing: bool,
) -> Result<()> {
    let entry = platform_store()?
        .build(KEYRING_SERVICE, identity, None)
        .map_err(keyring_error)?;
    match entry.get_secret() {
        Err(keyring_core::Error::NoEntry) if allow_missing => Ok(()),
        Err(keyring_core::Error::NoEntry) => {
            anyhow::bail!("The registered restore key is missing; its account files were preserved")
        }
        Err(error) => Err(keyring_error(error)),
        Ok(secret) => {
            let secret = decoded_key(secret)?;
            ensure!(
                restore_key_digest(secret.as_ref()) == digest,
                "Restore credential ownership changed; account files were preserved"
            );
            Ok(())
        }
    }
}

/// The transaction is durable before this write. Existing credentials are never replaced.
pub(super) fn save_new_restore_key(identity: &str, key: &[u8; 32]) -> Result<()> {
    let entry = platform_store()?
        .build(KEYRING_SERVICE, identity, None)
        .map_err(keyring_error)?;
    save_new_restore_key_in(&entry, key)
}

fn save_new_restore_key_in(entry: &keyring_core::Entry, key: &[u8; 32]) -> Result<()> {
    match entry.get_secret() {
        Err(keyring_core::Error::NoEntry) => {}
        Ok(secret) => {
            let _secret = Zeroizing::new(secret);
            anyhow::bail!(
                "The restore destination already has an archive credential; choose another account"
            )
        }
        Err(error) => return Err(keyring_error(error)),
    }
    entry.set_secret(&stored_key(key)).map_err(keyring_error)?;
    let saved = decoded_key(entry.get_secret().map_err(keyring_error)?)?;
    ensure!(
        saved.as_slice() == key,
        "The OS keyring did not retain the restore key"
    );
    Ok(())
}

/// Deletes only the credential whose digest was reserved by this transaction.
pub(super) fn forget_owned_restore_key(identity: &str, digest: &str) -> Result<()> {
    let entry = platform_store()?
        .build(KEYRING_SERVICE, identity, None)
        .map_err(keyring_error)?;
    forget_owned_restore_key_in(&entry, digest)
}

fn forget_owned_restore_key_in(entry: &keyring_core::Entry, digest: &str) -> Result<()> {
    match entry.get_secret() {
        Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_error(error)),
        Ok(secret) => {
            let secret = decoded_key(secret)?;
            ensure!(
                restore_key_digest(secret.as_ref()) == digest,
                "Restore credential ownership changed; it was preserved"
            );
            entry.delete_credential().map_err(keyring_error)
        }
    }
}

/// Copies the keyring entry when an archive moves to another folder.
pub fn copy_archive_key(old_archive: &Path, new_archive: &Path) -> Result<()> {
    if plaintext(old_archive)? {
        return Ok(());
    }
    copy_archive_key_in(old_archive, new_archive, &*platform_store()?)
}

fn copy_archive_key_in(
    old_archive: &Path,
    new_archive: &Path,
    store: &dyn CredentialStoreApi,
) -> Result<()> {
    let old_parent = old_archive
        .parent()
        .context("Archive has no parent directory")?;
    let new_parent = new_archive
        .parent()
        .context("Archive has no parent directory")?;
    if !old_parent.exists() {
        return Ok(());
    }
    fs::create_dir_all(new_parent)?;
    if old_parent.canonicalize()? == new_parent.canonicalize()? {
        return Ok(());
    }
    let old_entry = store
        .build(KEYRING_SERVICE, &identity_for(old_parent)?, None)
        .map_err(keyring_error)
        .context("The OS keyring could not open Zap Fasterk's archive key")?;
    let key = match old_entry.get_secret() {
        Ok(secret) => decoded_key(secret)?,
        Err(keyring_core::Error::NoEntry) => {
            ensure!(
                plaintext(old_archive)?,
                "The archive is encrypted but its OS keyring key is missing. Restore the original keyring; the archive has not been changed"
            );
            return Ok(());
        }
        Err(error) => {
            return Err(keyring_error(error)).context(keyring_recovery());
        }
    };
    let new_entry = store
        .build(KEYRING_SERVICE, &identity_for(new_parent)?, None)
        .map_err(keyring_error)
        .context("The OS keyring could not open Zap Fasterk's archive key")?;
    new_entry
        .set_secret(&stored_key(&key))
        .map_err(keyring_error)
        .context("Could not save the archive key in the OS keyring")?;
    // Read back before the only copy of the history moves under this key.
    let saved = decoded_key(
        new_entry
            .get_secret()
            .map_err(keyring_error)
            .context("Could not verify the copied archive key")?,
    )?;
    ensure!(
        saved.as_slice() == key.as_ref(),
        "The OS keyring did not retain the copied archive key"
    );
    Ok(())
}

/// An archive key read from the previous install, waiting for its folder to
/// move. Identities digest the folder, so they are captured before the move
/// and resolved after it.
pub struct StagedKeyMove {
    old_identity: String,
    new_archive: PathBuf,
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for StagedKeyMove {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StagedKeyMove")
            .field("old_identity", &self.old_identity)
            .field("new_archive", &self.new_archive)
            .finish_non_exhaustive()
    }
}

/// Reads every linked account's archive key before adoption renames their
/// folders. Plaintext archives need no key. A missing key on an encrypted
/// archive fails, like a single-account adoption, rather than orphaning
/// history. Touches the OS store only when an archive actually needs moving.
pub fn stage_key_move(old_state: &Path, new_state: &Path) -> Result<Vec<StagedKeyMove>> {
    let mut pending: Vec<(PathBuf, PathBuf)> = Vec::new();
    let accounts = match fs::read_dir(old_state.join("accounts")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    for entry in accounts {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let old_archive = dir.join("archive.db");
        if !old_archive.try_exists()? {
            continue;
        }
        let name = dir
            .file_name()
            .context("Account folder has no name")?
            .to_os_string();
        pending.push((
            old_archive,
            new_state.join("accounts").join(name).join("archive.db"),
        ));
    }
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let store = platform_store()?;
    stage_key_move_in(pending, &*store)
}

fn stage_key_move_in(
    pending: Vec<(PathBuf, PathBuf)>,
    store: &dyn CredentialStoreApi,
) -> Result<Vec<StagedKeyMove>> {
    let mut staged = Vec::new();
    for (old_archive, new_archive) in pending {
        let old_identity = archive_key_identity(&old_archive)?;
        if let Some(key) = read_key_for_move(&old_archive, &old_identity, store)? {
            staged.push(StagedKeyMove {
                old_identity,
                new_archive,
                key,
            });
        }
    }
    Ok(staged)
}

fn read_key_for_move(
    old_archive: &Path,
    old_identity: &str,
    store: &dyn CredentialStoreApi,
) -> Result<Option<Zeroizing<[u8; 32]>>> {
    for service in [KEYRING_SERVICE, PREVIOUS_KEYRING_SERVICE] {
        let entry = store
            .build(service, old_identity, None)
            .map_err(keyring_error)?;
        match entry.get_secret() {
            Ok(secret) => return decoded_key(secret).map(Some),
            Err(keyring_core::Error::NoEntry) => continue,
            Err(error) => return Err(keyring_error(error)),
        }
    }
    ensure!(
        plaintext(old_archive)?,
        "The archive is encrypted but its OS keyring key is missing. Restore the original keyring; the archive has not been changed"
    );
    Ok(None)
}

/// Writes a staged key under its new folder's identity, reads it back, and
/// drops the previous install's entry. An existing new entry wins: another
/// setup already claimed this folder, and nothing is replaced.
pub fn commit_key_move(staged: &StagedKeyMove) -> Result<()> {
    commit_key_move_in(staged, &*platform_store()?)
}

fn commit_key_move_in(staged: &StagedKeyMove, store: &dyn CredentialStoreApi) -> Result<()> {
    let new_identity = archive_key_identity(&staged.new_archive)?;
    let new_entry = store
        .build(KEYRING_SERVICE, &new_identity, None)
        .map_err(keyring_error)?;
    match new_entry.get_secret() {
        Ok(_) => return Ok(()),
        Err(keyring_core::Error::NoEntry) => {}
        Err(error) => return Err(keyring_error(error)),
    }
    new_entry
        .set_secret(&stored_key(&staged.key))
        .map_err(keyring_error)
        .context("Could not save the archive key in the OS keyring")?;
    let saved = decoded_key(
        new_entry
            .get_secret()
            .map_err(keyring_error)
            .context("Could not verify the copied archive key")?,
    )?;
    ensure!(
        saved.as_slice() == staged.key.as_ref(),
        "The OS keyring did not retain the copied archive key"
    );
    // Best effort: the entry is inert once the new one exists (the previous
    // service is never read again), so litter must not block startup.
    let old_entry = store
        .build(PREVIOUS_KEYRING_SERVICE, &staged.old_identity, None)
        .map_err(keyring_error)?;
    match old_entry.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => {
            log::warn!("could not remove the previous install's archive key: {error:#}");
            Ok(())
        }
    }
}

/// Deletes the keyring entry of an archive folder that is gone for good,
/// such as a removed account's. Never call it while the archive exists.
pub fn forget_archive_key(identity: &str) -> Result<()> {
    let entry = platform_store()?
        .build(KEYRING_SERVICE, identity, None)
        .map_err(keyring_error)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_error(error)),
    }
}

/// The keyring identity of the archive at `path`, while its folder exists.
pub fn archive_key_identity(path: &Path) -> Result<String> {
    identity_for(path.parent().context("Archive has no parent directory")?)
}

fn identity_for(parent: &Path) -> Result<String> {
    // Separate profiles must not overwrite each other's keys. The credential
    // label contains a digest, never a user path, phone number or message data.
    Ok(identity_for_resolved(&parent.canonicalize()?))
}

fn identity_for_resolved(parent: &Path) -> String {
    format!(
        "archive-{}",
        restore_key_digest(parent.as_os_str().as_encoded_bytes())
    )
}

// KDE Wallet's Secret Service bridge stores UTF-8 secrets only. Keep legacy
// binary credentials readable; new Linux credentials use a versioned text form.
// The previous install's Linux prefix follows; it still decodes on read.
const TEXT_KEY_PREFIX: &[u8] = b"zapfasterk-key-v1:";
const PREVIOUS_TEXT_KEY_PREFIX: &[u8] = b"zapfast-key-v1:";

fn text_key(key: &[u8; 32]) -> Zeroizing<Vec<u8>> {
    use std::fmt::Write;
    let mut text =
        Zeroizing::new(String::from_utf8(TEXT_KEY_PREFIX.to_vec()).expect("ASCII prefix"));
    for byte in key {
        write!(&mut *text, "{byte:02x}").expect("writing to String");
    }
    Zeroizing::new(text.as_bytes().to_vec())
}

fn stored_key(key: &[u8; 32]) -> Zeroizing<Vec<u8>> {
    if cfg!(target_os = "linux") {
        text_key(key)
    } else {
        Zeroizing::new(key.to_vec())
    }
}

fn decoded_key(secret: Vec<u8>) -> Result<Zeroizing<[u8; 32]>> {
    let secret = Zeroizing::new(secret);
    let mut key = Zeroizing::new([0; 32]);
    if secret.len() == 32 {
        key.copy_from_slice(&secret);
        return Ok(key);
    }
    let text = [TEXT_KEY_PREFIX, PREVIOUS_TEXT_KEY_PREFIX]
        .into_iter()
        .find_map(|prefix| secret.strip_prefix(prefix))
        .filter(|text| text.len() == 64)
        .context("The archive key in the OS keyring is invalid")?;
    for (output, pair) in key.iter_mut().zip(text.as_chunks::<2>().0) {
        let pair =
            std::str::from_utf8(pair).context("The archive key in the OS keyring is invalid")?;
        *output =
            u8::from_str_radix(pair, 16).context("The archive key in the OS keyring is invalid")?;
    }
    Ok(key)
}

fn keyring_recovery() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "Start a Secret Service provider in your desktop session (GNOME Keyring, or KWallet with Secret Service support), unlock its login collection, then choose Try again. Zap Fasterk needs the session D-Bus service org.freedesktop.secrets; the archive has not been replaced"
    }
    #[cfg(not(target_os = "linux"))]
    {
        "Unlock your OS keyring, then choose Try again"
    }
}

fn platform_store() -> Result<std::sync::Arc<dyn CredentialStoreApi>> {
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(windows)]
    let store = windows_native_keyring_store::Store::new();
    Ok(store.map_err(keyring_error).context(keyring_recovery())?
        as std::sync::Arc<dyn CredentialStoreApi>)
}

fn key_from_entry(path: &Path, entry: &keyring_core::Entry) -> Result<Zeroizing<[u8; 32]>> {
    match entry.get_secret() {
        Ok(secret) => match decoded_key(secret) {
            Ok(key) => Ok(key),
            Err(_) if !path.try_exists()? => create_key(entry),
            Err(error) => Err(error),
        },
        Err(keyring_core::Error::NoEntry) => {
            ensure!(
                plaintext(path)?,
                "The archive is encrypted but its OS keyring key is missing. Restore the original keyring; the archive has not been changed"
            );
            create_key(entry)
        }
        Err(error) => Err(keyring_error(error)).context(keyring_recovery()),
    }
}

/// How the Windows store names `ERROR_NOT_ENOUGH_MEMORY`, which `CredWriteW`
/// returns when Credential Manager holds as many credentials as it can take.
/// The store keeps its error type private, so its text is what can be matched.
const WINDOWS_CREDENTIALS_FULL: &str = "Windows error code 8";

/// A keyring error as one message.
///
/// keyring-core's text already includes the error underneath and also reports
/// it as the source, so a chain printed with `{:#}` repeated it ("Platform
/// failure: Windows error code 8: Windows error code 8"). A full Credential
/// Manager is also named for what it is, with what to do about it.
fn keyring_error(error: keyring_core::Error) -> anyhow::Error {
    match &error {
        keyring_core::Error::PlatformFailure(inner)
            if inner.to_string() == WINDOWS_CREDENTIALS_FULL =>
        {
            anyhow::anyhow!(
                "Windows Credential Manager is full. Remove entries you no longer need in Credential Manager and try again"
            )
        }
        _ => anyhow::anyhow!("{error}"),
    }
}

fn create_key(entry: &keyring_core::Entry) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    getrandom::fill(key.as_mut()).context("Could not generate an archive key")?;
    entry
        .set_secret(&stored_key(&key))
        .map_err(keyring_error)
        .context("Could not save the archive key in the OS keyring")?;
    // Read back before touching the only copy of the message history.
    let saved = decoded_key(
        entry
            .get_secret()
            .map_err(keyring_error)
            .context("Could not verify the saved archive key")?,
    )?;
    ensure!(
        saved.as_slice() == key.as_ref(),
        "The OS keyring did not retain the archive key"
    );
    Ok(key)
}

pub(super) fn key_literal(key: &[u8; 32]) -> Zeroizing<String> {
    use std::fmt::Write;
    let mut literal = Zeroizing::new(String::with_capacity(67));
    literal.push_str("x'");
    for byte in key {
        write!(&mut *literal, "{byte:02x}").expect("writing to a String");
    }
    literal.push('\'');
    literal
}

pub(super) fn keyed(path: &Path, key: &[u8; 32]) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "key", &*key_literal(key))?;
    let version: String = connection
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .context("This build does not support encrypted archives")?;
    ensure!(
        !version.is_empty(),
        "This build does not support encrypted archives"
    );
    // PRAGMA key alone does not verify a key. Read a page before any migration.
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .context("The archive could not be unlocked with its OS keyring key")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(connection)
}

fn private_file(path: &Path) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.sync_all()?;
    Ok(())
}

pub(super) fn open(path: &Path, key: &[u8; 32]) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("db.encrypting");
    if !path.exists() {
        private_file(path)?;
    } else if fs::metadata(path)?.len() > 0 && plaintext(path)? {
        // Keep the original authoritative until the exported copy is complete.
        // Switching away from WAL folds in committed pages and removes sidecars
        // before replacing the main file. An interruption leaves the original
        // usable, with at most an encrypted staging file to discard next time.
        let source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let mode: String =
            source.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        ensure!(
            mode == "delete",
            "Close other programs using the archive before migrating it"
        );
        source.pragma_update(None, "temp_store", "MEMORY")?;
        if staging.try_exists()? {
            fs::remove_file(&staging)?;
        }
        private_file(&staging)?;
        source.execute(
            "ATTACH DATABASE ?1 AS encrypted KEY ?2",
            rusqlite::params![
                staging.to_str().context("Archive path is not UTF-8")?,
                &*key_literal(key)
            ],
        )?;
        source.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
        // sqlcipher_export does not copy SQLite's application/user version.
        for pragma in ["user_version", "application_id"] {
            let value: i64 = source.pragma_query_value(None, pragma, |row| row.get(0))?;
            source.pragma_update(Some("encrypted"), pragma, value)?;
        }
        source.execute_batch("DETACH DATABASE encrypted")?;
        let verified = keyed(&staging, key)?;
        let integrity: String =
            verified.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        ensure!(
            integrity == "ok",
            "The encrypted archive failed its integrity check"
        );
        drop(verified);
        drop(source);
        // FlushFileBuffers on Windows requires a writable handle.
        fs::OpenOptions::new()
            .write(true)
            .open(&staging)?
            .sync_all()?;
        fs::rename(&staging, path)
            .context("Could not replace the archive with its encrypted copy")?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    let connection = keyed(path, key)?;
    if staging.try_exists()? {
        fs::remove_file(staging)?;
    }
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_text_roundtrips_all_bytes_and_keeps_legacy_binary_keys() {
        let key = std::array::from_fn(|index| (index * 8) as u8);
        let text = text_key(&key);
        assert!(std::str::from_utf8(&text).is_ok());
        assert_eq!(*decoded_key(text.to_vec()).unwrap(), key);
        assert_eq!(*decoded_key(key.to_vec()).unwrap(), key);
        assert!(decoded_key(b"zapfasterk-key-v1:bad".to_vec()).is_err());
        let mut invalid = text.to_vec();
        *invalid.last_mut().unwrap() = b'z';
        assert!(decoded_key(invalid).is_err());
    }

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn read_secret(connection: &Connection) -> String {
        connection
            .query_row("SELECT value FROM secrets", [], |row| row.get(0))
            .unwrap()
    }

    fn moved_account(
        old_root: &tempfile::TempDir,
        new_root: &tempfile::TempDir,
    ) -> (PathBuf, PathBuf) {
        let old_archive = old_root
            .path()
            .join("accounts")
            .join("1")
            .join("archive.db");
        let new_archive = new_root
            .path()
            .join("accounts")
            .join("1")
            .join("archive.db");
        fs::create_dir_all(old_archive.parent().unwrap()).unwrap();
        fs::create_dir_all(new_archive.parent().unwrap()).unwrap();
        (old_archive, new_archive)
    }

    #[test]
    fn staged_keys_move_between_services_and_clean_up() {
        let store = keyring_core::mock::Store::new().unwrap();
        let old_root = directory();
        let new_root = directory();
        let (old_archive, new_archive) = moved_account(&old_root, &new_root);
        fs::write(&old_archive, b"encrypted fixture archive").unwrap();
        let old_identity = archive_key_identity(&old_archive).unwrap();
        store
            .build(PREVIOUS_KEYRING_SERVICE, &old_identity, None)
            .unwrap()
            .set_secret(&[4; 32])
            .unwrap();
        let staged = stage_key_move_in(vec![(old_archive, new_archive)], &*store).unwrap();
        assert_eq!(staged.len(), 1);
        commit_key_move_in(&staged[0], &*store).unwrap();
        let new_identity = archive_key_identity(&staged[0].new_archive).unwrap();
        let secret = store
            .build(KEYRING_SERVICE, &new_identity, None)
            .unwrap()
            .get_secret()
            .unwrap();
        assert_eq!(*decoded_key(secret).unwrap(), [4; 32]);
        assert!(matches!(
            store
                .build(PREVIOUS_KEYRING_SERVICE, &old_identity, None)
                .unwrap()
                .get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
        // A second adoption is a no-op: the new entry wins.
        commit_key_move_in(&staged[0], &*store).unwrap();
    }

    #[test]
    fn staged_keys_accept_the_previous_prefix_and_rewrite_it() {
        let store = keyring_core::mock::Store::new().unwrap();
        let old_root = directory();
        let new_root = directory();
        let (old_archive, new_archive) = moved_account(&old_root, &new_root);
        fs::write(&old_archive, b"encrypted fixture archive").unwrap();
        let old_identity = archive_key_identity(&old_archive).unwrap();
        let hex: String = [9; 32].iter().map(|byte| format!("{byte:02x}")).collect();
        store
            .build(PREVIOUS_KEYRING_SERVICE, &old_identity, None)
            .unwrap()
            .set_secret(format!("zapfast-key-v1:{hex}").as_bytes())
            .unwrap();
        let staged = stage_key_move_in(vec![(old_archive, new_archive)], &*store).unwrap();
        assert_eq!(*staged[0].key, [9; 32]);
        commit_key_move_in(&staged[0], &*store).unwrap();
        let new_identity = archive_key_identity(&staged[0].new_archive).unwrap();
        let secret = store
            .build(KEYRING_SERVICE, &new_identity, None)
            .unwrap()
            .get_secret()
            .unwrap();
        #[cfg(target_os = "linux")]
        assert!(secret.starts_with(b"zapfasterk-key-v1:"));
        assert_eq!(*decoded_key(secret).unwrap(), [9; 32]);
    }

    #[test]
    fn staged_keys_skip_plaintext_and_fail_loudly_without_one() {
        let store = keyring_core::mock::Store::new().unwrap();
        let old_root = directory();
        let new_root = directory();
        // An empty archive needs no key at all.
        let (old_archive, new_archive) = moved_account(&old_root, &new_root);
        fs::write(&old_archive, b"").unwrap();
        assert!(
            stage_key_move_in(vec![(old_archive, new_archive)], &*store)
                .unwrap()
                .is_empty()
        );
        // An encrypted archive without any key must not move silently.
        let (old_archive, new_archive) = moved_account(&old_root, &new_root);
        fs::write(&old_archive, b"encrypted fixture archive").unwrap();
        assert!(
            stage_key_move_in(vec![(old_archive, new_archive)], &*store)
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
    }

    #[test]
    fn restore_credentials_refuse_existing_keys_and_only_rollback_their_own_key() {
        let store = keyring_core::mock::Store::new().unwrap();
        let entry = store
            .build("restore-fixture", "synthetic-key", None)
            .unwrap();
        save_new_restore_key_in(&entry, &[7; 32]).unwrap();
        assert!(save_new_restore_key_in(&entry, &[8; 32]).is_err());
        assert!(forget_owned_restore_key_in(&entry, &restore_key_digest(&[8; 32])).is_err());
        assert_eq!(*decoded_key(entry.get_secret().unwrap()).unwrap(), [7; 32]);
        forget_owned_restore_key_in(&entry, &restore_key_digest(&[7; 32])).unwrap();
        assert!(matches!(
            entry.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
    }

    #[test]
    fn reserved_restore_identity_matches_the_published_account_folder() {
        let directory = directory();
        let reserved = restore_key_identity(directory.path(), "2").unwrap();
        fs::create_dir(directory.path().join("2")).unwrap();
        assert_eq!(
            reserved,
            archive_key_identity(&directory.path().join("2/archive.db")).unwrap()
        );
    }

    #[test]
    fn keyring_unlock_reuses_keys_and_never_replaces_a_missing_key() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let store = keyring_core::mock::Store::new().unwrap();
        let entry = store.build("zapfasterk-test", "archive", None).unwrap();
        let key = key_from_entry(&path, &entry).unwrap();
        assert_eq!(*key, *key_from_entry(&path, &entry).unwrap());
        let connection = open(&path, &key).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE example(value TEXT); INSERT INTO example VALUES ('fixture');",
            )
            .unwrap();
        drop(connection);
        let original = fs::read(&path).unwrap();
        entry.delete_credential().unwrap();
        assert!(
            key_from_entry(&path, &entry)
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        assert!(matches!(
            entry.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        entry.set_secret(&[1; 12]).unwrap();
        assert!(
            key_from_entry(&path, &entry)
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
        let mock = entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap();
        mock.set_error(keyring_core::Error::PlatformFailure(Box::new(
            std::io::Error::other("locked"),
        )));
        assert_eq!(
            key_from_entry(&path, &entry).unwrap_err().to_string(),
            keyring_recovery()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    /// Stands in for the Windows store's private error type, whose text for
    /// code 8 is "Windows error code 8".
    #[derive(Debug)]
    struct WindowsCode(u32);

    impl std::fmt::Display for WindowsCode {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Windows error code {}", self.0)
        }
    }

    impl std::error::Error for WindowsCode {}

    fn saving(error: keyring_core::Error) -> String {
        let error: Result<()> =
            Err(keyring_error(error)).context("Could not save the archive key in the OS keyring");
        format!("{:#}", error.unwrap_err())
    }

    #[test]
    fn a_full_credential_manager_says_what_to_do() {
        assert_eq!(
            saving(keyring_core::Error::PlatformFailure(Box::new(WindowsCode(
                8
            )))),
            "Could not save the archive key in the OS keyring: Windows Credential Manager is full. Remove entries you no longer need in Credential Manager and try again"
        );
        // Any other platform failure is reported once, as the keyring put it.
        assert_eq!(
            saving(keyring_core::Error::PlatformFailure(Box::new(WindowsCode(
                5
            )))),
            "Could not save the archive key in the OS keyring: Platform failure: Windows error code 5"
        );
        assert_eq!(
            saving(keyring_core::Error::NoStorageAccess(Box::new(WindowsCode(
                8
            )))),
            "Could not save the archive key in the OS keyring: Couldn't access platform storage: Windows error code 8"
        );
    }

    #[test]
    fn starting_over_replaces_an_invalid_key_only_after_preserving_the_archive() {
        for invalid in [vec![], vec![1; 12], vec![2; 64]] {
            let directory = directory();
            let path = directory.path().join("archive.db");
            let kept = directory.path().join("archive-unreadable.db");
            let store = keyring_core::mock::Store::new().unwrap();
            let entry = store.build("zapfasterk-test", "archive", None).unwrap();
            let original_key = key_from_entry(&path, &entry).unwrap();
            let connection = open(&path, &original_key).unwrap();
            connection
                .execute_batch("CREATE TABLE example(value TEXT);")
                .unwrap();
            drop(connection);
            let original = fs::read(&path).unwrap();
            entry.set_secret(&invalid).unwrap();

            assert!(key_from_entry(&path, &entry).is_err());
            assert_eq!(entry.get_secret().unwrap(), invalid);
            assert_eq!(fs::read(&path).unwrap(), original);

            // The recovery action keeps the old archive before retrying unlock.
            fs::rename(&path, &kept).unwrap();
            let key = key_from_entry(&path, &entry).unwrap();
            assert_eq!(*decoded_key(entry.get_secret().unwrap()).unwrap(), *key);
            assert_eq!(*key_from_entry(&path, &entry).unwrap(), *key);
            assert!(!path.exists());
            assert_eq!(fs::read(&kept).unwrap(), original);
            assert!(open(&kept, &original_key).is_ok());
            assert!(open(&path, &key).is_ok());
        }
    }

    #[test]
    fn invalid_key_is_not_replaced_for_an_existing_plaintext_or_empty_archive() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let store = keyring_core::mock::Store::new().unwrap();
        let entry = store.build("zapfasterk-test", "archive", None).unwrap();
        entry.set_secret(&[]).unwrap();
        for contents in [b"".as_slice(), HEADER.as_slice()] {
            fs::write(&path, contents).unwrap();
            assert!(key_from_entry(&path, &entry).is_err());
            assert!(entry.get_secret().unwrap().is_empty());
            assert_eq!(fs::read(&path).unwrap(), contents);
        }
    }

    #[test]
    fn encrypted_database_and_wal_reject_missing_or_wrong_keys() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let connection = open(&path, &[7; 32]).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE secrets (value TEXT); INSERT INTO secrets VALUES ('private archive marker');").unwrap();
        for file in [&path, &path.with_extension("db-wal")] {
            let bytes = fs::read(file).unwrap();
            assert!(
                !bytes
                    .windows(22)
                    .any(|bytes| bytes == b"private archive marker")
            );
        }
        assert!(!plaintext(&path).unwrap());
        assert!(open(&path, &[8; 32]).is_err());
        assert!(
            Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM sqlite_master", [], |row| row
                    .get::<_, i64>(0))
                .is_err()
        );
        drop(connection);
        assert_eq!(
            read_secret(&open(&path, &[7; 32]).unwrap()),
            "private archive marker"
        );
    }

    #[test]
    fn migration_preserves_wal_data_schema_and_version() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let source = Connection::open(&path).unwrap();
        source.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA user_version=9; PRAGMA application_id=42; CREATE TABLE secrets (value TEXT); CREATE INDEX secret_values ON secrets(value); INSERT INTO secrets VALUES ('from the phone');").unwrap();
        // Leave the committed WAL on disk as after an interrupted old process.
        source
            .set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )
            .unwrap();
        drop(source);
        assert!(path.with_extension("db-wal").exists());
        fs::write(path.with_extension("db.encrypting"), b"interrupted export").unwrap();
        let encrypted = open(&path, &[9; 32]).unwrap();
        assert_eq!(read_secret(&encrypted), "from the phone");
        assert_eq!(
            encrypted
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
        assert_eq!(
            encrypted
                .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        assert_eq!(
            encrypted
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='secret_values'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert!(!plaintext(&path).unwrap());
        assert!(!path.with_extension("db.encrypting").exists());
        assert!(!path.with_extension("db-wal").exists());
    }

    #[test]
    fn a_failed_migration_leaves_the_original_readable() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let source = Connection::open(&path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE secrets(value TEXT); INSERT INTO secrets VALUES ('keep me');",
            )
            .unwrap();
        drop(source);
        // A blocked staging path simulates a filesystem failure before replace.
        fs::create_dir(path.with_extension("db.encrypting")).unwrap();
        assert!(open(&path, &[9; 32]).is_err());
        assert!(plaintext(&path).unwrap());
        assert_eq!(read_secret(&Connection::open(&path).unwrap()), "keep me");
        fs::remove_dir(path.with_extension("db.encrypting")).unwrap();
        assert_eq!(read_secret(&open(&path, &[9; 32]).unwrap()), "keep me");
    }
}
