//! Portable authenticated backups. Full account backups require a stopped worker.
//! Restore never replaces an existing account. All database and media bytes are authenticated
//! and encrypted, including filenames and the randomly generated SQLCipher key.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    num::NonZeroU32,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use ring::{aead, pbkdf2};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{Archive, encryption};
use crate::paths::AccountDirs;

const MAGIC: &[u8; 8] = b"ZFBACK01";
const CHUNK: usize = 64 * 1024;
const ROUNDS: u32 = 600_000;
const MAX_FILES: usize = 1_000_000;
const MAX_FILE: u64 = 16 * 1024 * 1024 * 1024;
const MAX_TOTAL: u64 = 1024 * 1024 * 1024 * 1024;

/// Safe to carry in commands that derive Debug: the secret is always redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct BackupPassphrase(Zeroizing<String>);

impl BackupPassphrase {
    pub fn new(value: Zeroizing<String>) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BackupPassphrase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BackupPassphrase([redacted])")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupSummary {
    pub account: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    summary: BackupSummary,
    database_key: [u8; 32],
    files: Vec<(String, u64)>,
}

struct Cipher {
    key: aead::LessSafeKey,
    sequence: u64,
    header: [u8; 24],
}

impl Cipher {
    fn new(passphrase: &str, header: [u8; 24]) -> Result<Self> {
        ensure!(
            passphrase.chars().count() >= 12,
            "Use a backup passphrase with at least 12 characters"
        );
        let mut key = Zeroizing::new([0u8; 32]);
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(ROUNDS).unwrap(),
            &header[8..],
            passphrase.as_bytes(),
            key.as_mut(),
        );
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, key.as_ref())
            .map_err(|_| anyhow::anyhow!("Could not prepare backup encryption"))?;
        Ok(Self {
            key: aead::LessSafeKey::new(key),
            sequence: 0,
            header,
        })
    }

    fn nonce(&mut self) -> Result<aead::Nonce> {
        ensure!(self.sequence < u64::MAX, "Backup is too large");
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.sequence.to_be_bytes());
        self.sequence += 1;
        Ok(aead::Nonce::assume_unique_for_key(nonce))
    }

    fn write(&mut self, output: &mut impl Write, plaintext: &[u8]) -> Result<()> {
        ensure!(plaintext.len() <= CHUNK, "Backup frame is too large");
        let mut buffer = Zeroizing::new(plaintext.to_vec());
        let nonce = self.nonce()?;
        self.key
            .seal_in_place_append_tag(nonce, aead::Aad::from(self.header), &mut *buffer)
            .map_err(|_| anyhow::anyhow!("Could not encrypt backup"))?;
        output.write_all(&(buffer.len() as u32).to_le_bytes())?;
        output.write_all(&buffer)?;
        Ok(())
    }

    fn read(&mut self, input: &mut impl Read) -> Result<Zeroizing<Vec<u8>>> {
        let mut length = [0u8; 4];
        input
            .read_exact(&mut length)
            .context("Backup is incomplete")?;
        let length = u32::from_le_bytes(length) as usize;
        ensure!((16..=CHUNK + 16).contains(&length), "Invalid backup frame");
        let mut buffer = Zeroizing::new(vec![0u8; length]);
        input
            .read_exact(&mut buffer)
            .context("Backup is incomplete")?;
        let nonce = self.nonce()?;
        let plaintext = self
            .key
            .open_in_place(nonce, aead::Aad::from(self.header), &mut buffer)
            .map_err(|_| anyhow::anyhow!("Incorrect passphrase or damaged backup"))?;
        let length = plaintext.len();
        buffer.truncate(length);
        Ok(buffer)
    }
}

fn private_create(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

// Only local attachment paths are rewritten. All text, protocol data, quotes,
// and other message metadata remain intact.
fn map_paths(
    value: &mut serde_json::Value,
    map: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if key == "path" {
                    if let Some(path) = value.as_str() {
                        *value = serde_json::Value::String(map(path)?);
                    }
                } else {
                    map_paths(value, map)?;
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                map_paths(value, map)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn rewrite(
    connection: &rusqlite::Connection,
    mut map: impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    let mut statement = transaction.prepare("SELECT chat, id, content FROM messages WHERE EXISTS (SELECT 1 FROM json_tree(content) WHERE key = 'path' AND type = 'text')")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let chat: String = row.get(0)?;
        let id: String = row.get(1)?;
        let content: String = row.get(2)?;
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .context("Archive contains invalid attachment metadata")?;
        map_paths(&mut value, &mut map)?;
        transaction.execute(
            "UPDATE messages SET content = ?3 WHERE chat = ?1 AND id = ?2",
            rusqlite::params![chat, id, serde_json::to_string(&value)?],
        )?;
    }
    drop(rows);
    drop(statement);
    let mut statement =
        transaction.prepare("SELECT hash, path FROM stickers WHERE path IS NOT NULL")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let path: String = row.get(1)?;
        transaction.execute(
            "UPDATE stickers SET path = ?2 WHERE hash = ?1",
            rusqlite::params![hash, map(&path)?],
        )?;
    }
    drop(rows);
    drop(statement);
    transaction.commit()?;
    Ok(())
}

impl Archive {
    /// Independent connection for an already initialized archive. Does not run
    /// migrations, retry recovery, or alter outgoing-message state.
    pub fn open_for_backup(path: &Path) -> Result<Self> {
        ensure!(path.is_file(), "Archive is unavailable");
        let key = encryption::key_for(path)?;
        let connection = encryption::keyed(path, &key)?;
        connection.busy_timeout(std::time::Duration::from_secs(30))?;
        Ok(Self { connection })
    }

    /// Copies a consistent encrypted database without rewriting attachment paths.
    /// Merge previews use this to reconcile identities without touching current rows.
    pub(super) fn encrypted_snapshot(
        &self,
        database: &Path,
        database_key: &[u8; 32],
    ) -> Result<()> {
        private_create(database)?;
        self.connection.execute(
            "ATTACH DATABASE ?1 AS portable_backup KEY ?2",
            rusqlite::params![
                database
                    .to_str()
                    .context("Backup staging path is not UTF-8")?,
                &*encryption::key_literal(database_key)
            ],
        )?;
        let exported = (|| -> Result<()> {
            let transaction = self.connection.unchecked_transaction()?;
            transaction.query_row("SELECT sqlcipher_export('portable_backup')", [], |_| Ok(()))?;
            for pragma in ["user_version", "application_id"] {
                let value: i64 = transaction.pragma_query_value(None, pragma, |row| row.get(0))?;
                transaction.pragma_update(Some("portable_backup"), pragma, value)?;
            }
            transaction.commit()?;
            Ok(())
        })();
        let detached = self
            .connection
            .execute_batch("DETACH DATABASE portable_backup");
        exported?;
        detached?;
        Ok(())
    }
    /// Writes a portable password-protected snapshot and referenced attachments.
    /// Call on the account worker so archive updates cannot race the snapshot.
    /// Destination must not exist. Missing referenced attachments fail explicitly.
    pub fn export_backup(
        &self,
        destination: &Path,
        passphrase: &str,
        account: &str,
        staging_parent: &Path,
    ) -> Result<BackupSummary> {
        self.export_package(destination, passphrase, account, staging_parent, None)
    }

    /// The caller must stop the account worker, protocol store, and downloads
    /// before calling this and keep them stopped until the operation completes.
    pub fn export_account_backup(
        &self,
        destination: &Path,
        passphrase: &str,
        dirs: &AccountDirs,
        staging_parent: &Path,
    ) -> Result<BackupSummary> {
        self.export_package(
            destination,
            passphrase,
            dirs.id.as_str(),
            staging_parent,
            Some(dirs),
        )
    }

    fn export_package(
        &self,
        destination: &Path,
        passphrase: &str,
        account: &str,
        staging_parent: &Path,
        dirs: Option<&AccountDirs>,
    ) -> Result<BackupSummary> {
        fs::create_dir_all(staging_parent)?;
        let staging = tempfile::Builder::new()
            .prefix("backup-")
            .tempdir_in(staging_parent)?;
        let database = staging.path().join("archive.db");
        let mut database_key = Zeroizing::new([0u8; 32]);
        getrandom::fill(database_key.as_mut())?;
        self.encrypted_snapshot(&database, &database_key)?;
        let snapshot = encryption::keyed(&database, &database_key)?;
        let mut attachments = BTreeMap::<String, (String, u64)>::new();
        rewrite(&snapshot, |source| {
            if let Some((name, _)) = attachments.get(source) {
                return Ok(name.clone());
            }
            ensure!(attachments.len() < MAX_FILES, "Too many backup attachments");
            let metadata = fs::metadata(source)
                .context("A referenced attachment is missing or unavailable")?;
            ensure!(
                metadata.is_file() && metadata.len() <= MAX_FILE,
                "A referenced attachment cannot be backed up"
            );
            let extension = Path::new(source)
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 16
                        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
                })
                .unwrap_or("bin");
            let name = format!("media/backup-{}.{extension}", attachments.len());
            attachments.insert(source.into(), (name.clone(), metadata.len()));
            Ok(name)
        })?;
        snapshot.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")?;
        let integrity: String =
            snapshot.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        ensure!(
            integrity == "ok",
            "Backup snapshot failed its integrity check"
        );
        drop(snapshot);
        let mut files = vec![("archive.db".into(), fs::metadata(&database)?.len())];
        files.extend(attachments.values().cloned());
        let mut account_files = BTreeMap::new();
        if let Some(dirs) = dirs {
            validate_session(&dirs.session_db())?;
            collect_account_files(&dirs.state, &dirs.state, "state", &mut account_files)?;
            collect_account_files(&dirs.cache, &dirs.cache, "cache", &mut account_files)?;
            files.extend(
                account_files
                    .iter()
                    .map(|(name, source)| Ok((name.clone(), fs::metadata(source)?.len())))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        ensure!(files.len() <= MAX_FILES + 1, "Too many backup files");
        ensure!(
            files.iter().all(|(_, size)| *size <= MAX_FILE),
            "A backup file exceeds the supported size"
        );
        let bytes = files.iter().try_fold(0u64, |total, (_, size)| {
            total.checked_add(*size).context("Backup is too large")
        })?;
        ensure!(bytes <= MAX_TOTAL, "Backup exceeds the supported size");
        let summary = BackupSummary {
            account: account.into(),
            files: files.len(),
            bytes,
        };
        let mut manifest = Manifest {
            version: if dirs.is_some() { 2 } else { 1 },
            summary: summary.clone(),
            database_key: *database_key,
            files,
        };
        let metadata = Zeroizing::new(serde_json::to_vec(&manifest)?);
        manifest.database_key.fill(0);
        ensure!(
            metadata.len() <= 64 * 1024 * 1024,
            "Backup manifest is too large"
        );
        let mut header = [0u8; 24];
        header[..8].copy_from_slice(MAGIC);
        getrandom::fill(&mut header[8..])?;
        let mut cipher = Cipher::new(passphrase, header)?;
        let mut output = private_create(destination)?;
        let result = (|| -> Result<()> {
            output.write_all(&header)?;
            cipher.write(&mut output, &(metadata.len() as u64).to_le_bytes())?;
            for chunk in metadata.chunks(CHUNK) {
                cipher.write(&mut output, chunk)?;
            }
            write_file(&mut cipher, &mut output, &database, manifest.files[0].1)?;
            // Manifest and payload must use the same order, independent of source paths.
            let sources: BTreeMap<&str, (&str, u64)> = attachments
                .iter()
                .map(|(source, (name, size))| (name.as_str(), (source.as_str(), *size)))
                .collect();
            for (name, size) in manifest.files.iter().skip(1) {
                if let Some(source) = account_files.get(name) {
                    write_file(&mut cipher, &mut output, source, *size)?;
                } else {
                    let (source, size) = sources[name.as_str()];
                    write_file(&mut cipher, &mut output, Path::new(source), size)?;
                }
            }
            cipher.write(&mut output, b"complete")?;
            output.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            drop(output);
            let _ = fs::remove_file(destination);
        }
        result?;
        Ok(summary)
    }
}

fn safe_account_path(name: &str) -> bool {
    let Some((root, relative)) = name.split_once('/') else {
        return false;
    };
    matches!(root, "state" | "cache")
        && !relative.is_empty()
        && relative.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(['\\', ':'])
                && !part.chars().any(char::is_control)
                && !part.ends_with(['.', ' '])
        })
}

fn collect_account_files(
    root: &Path,
    current: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<()> {
    if !current.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Account backup cannot include symbolic links"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Account backup cannot include reparse points"
            );
        }
        let relative = path.strip_prefix(root)?;
        if relative.components().count() == 1 && entry.file_name() == OWNER_FILE {
            continue;
        }
        if prefix == "state" && relative.components().count() == 1 {
            let name = entry.file_name();
            let name = name.to_str().context("Account backup path is not UTF-8")?;
            if name.starts_with("archive.db")
                || name.starts_with("backup-")
                || name.starts_with("restore-")
            {
                continue;
            }
        }
        if metadata.is_dir() {
            collect_account_files(root, &path, prefix, files)?;
        } else {
            ensure!(
                metadata.is_file() && metadata.len() <= MAX_FILE,
                "Account contains a file that cannot be backed up"
            );
            ensure!(files.len() < MAX_FILES, "Too many backup files");
            let relative = relative
                .to_str()
                .context("Account backup path is not UTF-8")?
                .replace('\\', "/");
            let name = format!("{prefix}/{relative}");
            ensure!(
                safe_account_path(&name),
                "Account contains an unsupported backup filename"
            );
            files.insert(name, path);
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            let mut input = File::open(entry.path())?;
            let mut output = private_create(&target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
    }
    Ok(())
}

fn validate_session(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "Restored session failed its integrity check"
    );
    Ok(())
}

fn write_file(
    cipher: &mut Cipher,
    output: &mut impl Write,
    source: &Path,
    expected: u64,
) -> Result<()> {
    let mut input = File::open(source)?;
    let mut buffer = Zeroizing::new(vec![0u8; CHUNK]);
    let mut remaining = expected;
    while remaining > 0 {
        let count = remaining.min(CHUNK as u64) as usize;
        input
            .read_exact(&mut buffer[..count])
            .context("A backup attachment changed while copying")?;
        cipher.write(output, &buffer[..count])?;
        remaining -= count as u64;
    }
    let mut extra = [0u8; 1];
    ensure!(
        input.read(&mut extra)? == 0,
        "A backup attachment changed while copying"
    );
    Ok(())
}

/// Authenticated restore staged privately; drop it to discard without changing accounts.
pub struct PreparedRestore {
    pub(super) staging: tempfile::TempDir,
    pub(super) database_key: Zeroizing<[u8; 32]>,
    pub summary: BackupSummary,
    full_account: bool,
}

pub fn prepare_restore(
    source: &Path,
    passphrase: &str,
    staging_parent: &Path,
) -> Result<PreparedRestore> {
    fs::create_dir_all(staging_parent)?;
    let staging = tempfile::Builder::new()
        .prefix("restore-")
        .tempdir_in(staging_parent)?;
    let mut ownership = private_create(&staging.path().join(".zap-fasterk-restore-preparation"))?;
    let mut token = [0u8; 32];
    getrandom::fill(&mut token)?;
    let owner = PreparationOwner {
        version: 1,
        directory: staging
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid preparation directory")?
            .into(),
        token: token.iter().map(|byte| format!("{byte:02x}")).collect(),
    };
    serde_json::to_writer(&mut ownership, &owner)?;
    ownership.sync_all()?;
    sync_directory(staging.path())?;
    sync_directory(staging_parent)?;
    let mut input = File::open(source)?;
    let mut header = [0u8; 24];
    input.read_exact(&mut header)?;
    ensure!(&header[..8] == MAGIC, "Unsupported backup format");
    let mut cipher = Cipher::new(passphrase, header)?;
    let length = cipher.read(&mut input)?;
    ensure!(length.len() == 8, "Invalid backup manifest");
    let length = u64::from_le_bytes(length.as_slice().try_into().unwrap());
    ensure!(length <= 64 * 1024 * 1024, "Backup manifest is too large");
    let mut metadata = Zeroizing::new(Vec::with_capacity(length as usize));
    while metadata.len() < length as usize {
        let frame = cipher.read(&mut input)?;
        ensure!(
            !frame.is_empty() && metadata.len() + frame.len() <= length as usize,
            "Invalid backup manifest"
        );
        metadata.extend_from_slice(&frame);
    }
    let mut manifest: Manifest =
        serde_json::from_slice(&metadata).context("Invalid backup manifest")?;
    let database_key = Zeroizing::new(manifest.database_key);
    manifest.database_key.fill(0);
    ensure!(
        (manifest.version == 1 || manifest.version == 2)
            && !manifest.files.is_empty()
            && manifest.files.len() <= MAX_FILES + 1,
        "Unsupported backup manifest"
    );
    ensure!(
        manifest.files[0].0 == "archive.db",
        "Backup database is missing"
    );
    let mut total = 0u64;
    for (index, (name, size)) in manifest.files.iter().enumerate() {
        ensure!(
            *size <= MAX_FILE,
            "Backup attachment exceeds supported size"
        );
        total = total.checked_add(*size).context("Backup is too large")?;
        ensure!(total <= MAX_TOTAL, "Backup exceeds supported size");
        if index > 0 {
            let safe = name
                .strip_prefix("media/")
                .and_then(|name| name.split_once('.'))
                .is_some_and(|(stem, extension)| {
                    !stem.is_empty()
                        && stem
                            .strip_prefix("backup-")
                            .unwrap_or(stem)
                            .bytes()
                            .all(|byte| byte.is_ascii_digit())
                        && !extension.is_empty()
                        && extension.len() <= 16
                        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
                });
            ensure!(
                (safe && name.len() <= 40) || (manifest.version == 2 && safe_account_path(name)),
                "Invalid attachment path in backup"
            );
        }
        let destination = staging.path().join(name);
        fs::create_dir_all(destination.parent().context("Invalid backup path")?)?;
        let mut output = private_create(&destination)?;
        let mut remaining = *size;
        while remaining > 0 {
            let frame = cipher.read(&mut input)?;
            ensure!(
                !frame.is_empty() && frame.len() as u64 <= remaining,
                "Invalid backup attachment"
            );
            output.write_all(&frame)?;
            remaining -= frame.len() as u64;
        }
        output.sync_all()?;
    }
    ensure!(
        cipher.read(&mut input)?.as_slice() == b"complete",
        "Backup is incomplete"
    );
    let mut extra = [0u8; 1];
    ensure!(input.read(&mut extra)? == 0, "Unexpected data after backup");
    ensure!(
        manifest.summary.files == manifest.files.len() && manifest.summary.bytes == total,
        "Invalid backup summary"
    );
    let connection = encryption::keyed(&staging.path().join("archive.db"), &database_key)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "Restored archive failed its integrity check"
    );
    let known: std::collections::HashSet<&str> = manifest
        .files
        .iter()
        .skip(1)
        .map(|(name, _)| name.as_str())
        .collect();
    rewrite(&connection, |path| {
        ensure!(
            known.contains(path),
            "Archive references an attachment missing from backup"
        );
        Ok(path.into())
    })?;
    drop(connection);
    if manifest.version == 2 {
        validate_session(&staging.path().join("state/session.db"))?;
    }
    Ok(PreparedRestore {
        staging,
        database_key,
        summary: manifest.summary,
        full_account: manifest.version == 2,
    })
}

#[derive(Serialize, Deserialize)]
struct PreparationOwner {
    version: u32,
    directory: String,
    token: String,
}

fn reconcile_preparations(parent: &Path) -> Result<()> {
    if !parent.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                continue;
            }
        }
        let marker = path.join(".zap-fasterk-restore-preparation");
        if !present(&marker)? {
            continue;
        }
        let metadata = fs::symlink_metadata(&marker)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 1024,
            "Restore preparation marker is invalid; files were preserved"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Restore preparation marker is a reparse point"
            );
        }
        let owner: PreparationOwner = serde_json::from_reader(File::open(marker)?)
            .context("Restore preparation marker is invalid; files were preserved")?;
        ensure!(
            owner.version == 1
                && owner.directory.starts_with("restore-")
                && path.file_name().and_then(|name| name.to_str())
                    == Some(owner.directory.as_str())
                && owner.token.len() == 64
                && owner.token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Restore preparation ownership changed; files were preserved"
        );
        ensure!(
            path.canonicalize()?.parent() == Some(parent.canonicalize()?.as_path()),
            "Restore preparation is outside its folder; files were preserved"
        );
        fs::remove_dir_all(&path)?;
        sync_directory(parent)?;
    }
    Ok(())
}

const OWNER_FILE: &str = ".zap-fasterk-restore-owner";

#[derive(Serialize, Deserialize)]
struct RestoreTransaction {
    version: u32,
    account: String,
    token: String,
    state_stage: String,
    cache_stage: String,
    key_identity: String,
    key_digest: String,
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Metadata publication with a durability barrier. Non-replacing moves are atomic
/// on the three supported operating systems, including a racing destination.
pub(crate) fn durable_move(source: &Path, destination: &Path, replace: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        // SAFETY: both buffers are terminated UTF-16 paths and live through the call.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(target_os = "linux")]
    {
        if replace {
            fs::rename(source, destination)?;
        } else {
            use std::os::unix::ffi::OsStrExt;
            let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
            let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
            // SAFETY: terminated paths; AT_FDCWD supplies the standard working directory.
            if unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    source.as_ptr(),
                    libc::AT_FDCWD,
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if replace {
            fs::rename(source, destination)?;
        } else {
            use std::os::unix::ffi::OsStrExt;
            unsafe extern "C" {
                fn renamex_np(
                    from: *const std::ffi::c_char,
                    to: *const std::ffi::c_char,
                    flags: u32,
                ) -> i32;
            }
            let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
            let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
            // SAFETY: terminated paths. Darwin RENAME_EXCL atomically refuses an existing target.
            if unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), 0x00000004) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, destination, replace);
        anyhow::bail!("Durable restore publication is unsupported on this operating system");
    }
    sync_directory(
        source
            .parent()
            .context("Publication source has no parent")?,
    )?;
    if destination.parent() != source.parent() {
        sync_directory(
            destination
                .parent()
                .context("Publication destination has no parent")?,
        )?;
    }
    Ok(())
}

fn transaction_path(dirs: &AccountDirs) -> Result<std::path::PathBuf> {
    ensure!(
        crate::model::AccountId::is_safe(dirs.id.as_str()),
        "Invalid restore account identifier"
    );
    ensure!(
        dirs.state.file_name().and_then(|name| name.to_str()) == Some(dirs.id.as_str())
            && dirs.cache.file_name().and_then(|name| name.to_str()) == Some(dirs.id.as_str()),
        "Restore destinations do not match the account"
    );
    Ok(dirs
        .state
        .parent()
        .context("Restore state has no parent")?
        .join(format!("restore-{}.json", dirs.id.as_str())))
}

impl RestoreTransaction {
    fn reserve(dirs: &AccountDirs, state: &Path, cache: &Path, key: &[u8; 32]) -> Result<Self> {
        let mut token = [0u8; 32];
        getrandom::fill(&mut token)?;
        let token: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
        let transaction = Self {
            version: 1,
            account: dirs.id.as_str().into(),
            token,
            state_stage: state
                .file_name()
                .and_then(|name| name.to_str())
                .context("Invalid restore stage")?
                .into(),
            cache_stage: cache
                .file_name()
                .and_then(|name| name.to_str())
                .context("Invalid restore stage")?
                .into(),
            key_identity: encryption::restore_key_identity(
                dirs.state.parent().context("Restore state has no parent")?,
                dirs.id.as_str(),
            )?,
            key_digest: encryption::restore_key_digest(key),
        };
        for stage in [state, cache] {
            let mut owner = private_create(&stage.join(OWNER_FILE))?;
            owner.write_all(transaction.token.as_bytes())?;
            owner.sync_all()?;
            sync_directory(stage)?;
        }
        let path = transaction_path(dirs)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(path.parent().context("Transaction has no parent")?)?;
        serde_json::to_writer(&mut temporary, &transaction)?;
        temporary.as_file().sync_all()?;
        durable_move(temporary.path(), &path, false)?;
        Ok(transaction)
    }

    fn load(dirs: &AccountDirs) -> Result<Option<Self>> {
        let path = transaction_path(dirs)?;
        if !present(&path)? {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 16 * 1024,
            "Restore transaction ownership is invalid; nothing was removed"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Restore transaction is a reparse point; nothing was removed"
            );
        }
        let transaction: Self = serde_json::from_reader(File::open(&path)?)
            .context("Restore transaction is invalid")?;
        ensure!(
            transaction.version == 1
                && transaction.account == dirs.id.as_str()
                && transaction.token.len() == 64
                && transaction
                    .token
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && transaction.key_digest.len() == 64
                && transaction
                    .key_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Restore transaction ownership is invalid"
        );
        for (name, prefix) in [
            (&transaction.state_stage, "restore-state-"),
            (&transaction.cache_stage, "restore-cache-"),
        ] {
            ensure!(
                name.starts_with(prefix)
                    && Path::new(name).components().count() == 1
                    && !name.contains(['/', '\\', ':']),
                "Restore staging path is invalid"
            );
        }
        ensure!(
            transaction.key_identity
                == encryption::restore_key_identity(
                    dirs.state.parent().context("Restore state has no parent")?,
                    dirs.id.as_str()
                )?,
            "Restore credential identity changed; nothing was removed"
        );
        Ok(Some(transaction))
    }

    fn stages(&self, dirs: &AccountDirs) -> Result<[std::path::PathBuf; 2]> {
        Ok([
            dirs.state
                .parent()
                .context("Restore state has no parent")?
                .join(&self.state_stage),
            dirs.cache
                .parent()
                .context("Restore cache has no parent")?
                .join(&self.cache_stage),
        ])
    }

    fn owned(&self, path: &Path) -> Result<bool> {
        if !present(path)? {
            return Ok(false);
        }
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Restore destination ownership changed; it was preserved"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Restore destination is a reparse point; it was preserved"
            );
        }
        let parent = path
            .parent()
            .context("Restore folder has no parent")?
            .canonicalize()?;
        ensure!(
            path.canonicalize()?.parent() == Some(parent.as_path()),
            "Restore destination resolves outside its parent; it was preserved"
        );
        let owner = path.join(OWNER_FILE);
        let metadata = fs::symlink_metadata(&owner)
            .context("Restore destination has no ownership marker; it was preserved")?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == 64,
            "Restore ownership marker changed; directory was preserved"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Restore ownership marker is a reparse point; directory was preserved"
            );
        }
        ensure!(
            fs::read_to_string(owner)? == self.token,
            "Restore ownership marker changed; directory was preserved"
        );
        Ok(true)
    }

    fn finish_owned(&self, dirs: &AccountDirs) -> Result<()> {
        ensure!(
            self.owned(&dirs.state)? && self.owned(&dirs.cache)?,
            "Registered restore publication is incomplete"
        );
        // Removing the transaction first makes marker cleanup restart-safe and non-destructive.
        fs::remove_file(transaction_path(dirs)?)?;
        sync_directory(dirs.state.parent().context("Restore state has no parent")?)?;
        for path in [&dirs.state, &dirs.cache] {
            fs::remove_file(path.join(OWNER_FILE))?;
            sync_directory(path)?;
        }
        Ok(())
    }

    fn rollback_with(
        &self,
        dirs: &AccountDirs,
        forget_key: impl FnOnce(&str, &str) -> Result<()>,
    ) -> Result<()> {
        let stages = self.stages(dirs)?;
        let paths = [&dirs.state, &dirs.cache, &stages[0], &stages[1]];
        // Validate all ownership before the first deletion. Any conflict preserves every target.
        let owned: Vec<bool> = paths
            .iter()
            .map(|path| self.owned(path))
            .collect::<Result<_>>()?;
        for (path, owned) in paths.into_iter().zip(owned) {
            if owned {
                fs::remove_dir_all(path)?;
                sync_directory(path.parent().context("Restore folder has no parent")?)?;
            }
        }
        forget_key(&self.key_identity, &self.key_digest)?;
        fs::remove_file(transaction_path(dirs)?)?;
        sync_directory(dirs.state.parent().context("Restore state has no parent")?)?;
        Ok(())
    }
}

fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())?
                .sync_all()?;
        }
    }
    sync_directory(path)
}

/// Roll back only a durable, token-owned newly restored account, before registration.
pub fn rollback_restore(dirs: &AccountDirs) -> Result<()> {
    let transaction = RestoreTransaction::load(dirs)?
        .context("Restore transaction is missing; account files were preserved")?;
    encryption::validate_owned_restore_key(
        &transaction.key_identity,
        &transaction.key_digest,
        true,
    )?;
    transaction.rollback_with(dirs, encryption::forget_owned_restore_key)
}

/// The caller must durably save the roster first. Registered accounts are preserved on restart.
pub fn finish_restore(dirs: &AccountDirs) -> Result<()> {
    let Some(transaction) = RestoreTransaction::load(dirs)? else {
        return Ok(());
    };
    encryption::validate_owned_restore_key(
        &transaction.key_identity,
        &transaction.key_digest,
        false,
    )?;
    transaction.finish_owned(dirs)
}

/// Called before opening any account worker. Corrupt/conflicting ownership fails closed.
pub fn reconcile_restores(dirs: &crate::paths::AppDirs, registered: &[String]) -> Result<()> {
    reconcile_preparations(&dirs.state.join("backup-staging"))?;
    let parent = dirs.state.join("accounts");
    if !parent.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("restore-"))
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let id =
            crate::model::AccountId::parse(id).context("Invalid restore transaction filename")?;
        let account = dirs.account(&id);
        if registered
            .iter()
            .any(|registered| registered == id.as_str())
        {
            finish_restore(&account)?;
        } else {
            rollback_restore(&account)?;
        }
    }
    Ok(())
}

impl PreparedRestore {
    pub fn has_session(&self) -> bool {
        self.full_account && self.staging.path().join("state/session.db").is_file()
    }

    /// Install only into a new account with no session, archive, or media folder.
    /// Archive-only packages must be registered offline. Full packages retain
    /// a linked-device session; the caller must show a confirmation before resuming it.
    pub fn install_into_empty(self, dirs: &AccountDirs) -> Result<BackupSummary> {
        ensure!(
            !present(&dirs.state)? && !present(&dirs.cache)?,
            "Restore requires new account folders"
        );
        let state_parent = dirs.state.parent().context("Account state has no parent")?;
        let cache_parent = dirs.cache.parent().context("Account cache has no parent")?;
        fs::create_dir_all(state_parent)?;
        fs::create_dir_all(cache_parent)?;
        let state = tempfile::Builder::new()
            .prefix("restore-state-")
            .tempdir_in(state_parent)?;
        let cache = tempfile::Builder::new()
            .prefix("restore-cache-")
            .tempdir_in(cache_parent)?;
        let mut new_key = Zeroizing::new([0u8; 32]);
        getrandom::fill(new_key.as_mut())?;
        let identity = encryption::restore_key_identity(state_parent, dirs.id.as_str())?;
        encryption::ensure_restore_key_available(&identity)?;
        let transaction = RestoreTransaction::reserve(dirs, state.path(), cache.path(), &new_key)?;
        let result = (|| -> Result<()> {
            if self.full_account {
                copy_tree(&self.staging.path().join("state"), state.path())?;
                copy_tree(&self.staging.path().join("cache"), cache.path())?;
            }
            let settings_path = state.path().join("settings.json");
            let mut settings: crate::settings::AccountSettings = if settings_path.exists() {
                serde_json::from_reader(File::open(&settings_path)?)
                    .context("Backup account settings are invalid")?
            } else {
                Default::default()
            };
            if let Some(wallpaper) = &settings.wallpaper_image {
                let name = wallpaper
                    .file_name()
                    .context("Backup wallpaper path is invalid")?;
                ensure!(
                    state.path().join(name).is_file(),
                    "Backup wallpaper is missing"
                );
                settings.wallpaper_image = Some(dirs.state.join(name));
            }
            // The installed files are safe even if the process stops before the
            // app can register this account and offer explicit connection consent.
            settings.archive_only = true;
            settings.cli_access = false;
            settings.save(&settings_path)?;
            let attachment_name = (0u64..)
                .map(|index| format!("restored-attachments-{index}"))
                .find(|name| !cache.path().join(name).exists())
                .context("Could not reserve restore attachments")?;
            let attachment_target = dirs.cache.join(&attachment_name);
            copy_tree(
                &self.staging.path().join("media"),
                &cache.path().join(&attachment_name),
            )?;
            let database = self.staging.path().join("archive.db");
            let connection = encryption::keyed(&database, &self.database_key)?;
            rewrite(&connection, |path| {
                let name = Path::new(path)
                    .file_name()
                    .context("Invalid attachment reference")?;
                Ok(attachment_target
                    .join(name)
                    .to_str()
                    .context("Restore path is not UTF-8")?
                    .into())
            })?;
            connection.pragma_update(None, "rekey", &*encryption::key_literal(&new_key))?;
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")?;
            drop(connection);
            let verified = encryption::keyed(&database, &new_key)?;
            let integrity: String =
                verified.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            ensure!(
                integrity == "ok",
                "Restored archive failed its final integrity check"
            );
            drop(verified);
            let mut source = File::open(&database)?;
            let mut output = private_create(&state.path().join("archive.db"))?;
            std::io::copy(&mut source, &mut output)?;
            output.sync_all()?;
            drop(output);
            sync_tree(state.path())?;
            sync_tree(cache.path())?;
            // The durable record already identifies this exact key before it
            // reaches the OS store, making every crash boundary reconcilable.
            encryption::save_new_restore_key(&transaction.key_identity, &new_key)?;
            durable_move(cache.path(), &dirs.cache, false)?;
            durable_move(state.path(), &dirs.state, false)?;
            ensure!(
                encryption::archive_key_identity(&dirs.archive_db())? == transaction.key_identity,
                "Published restore credential identity changed"
            );
            Ok(())
        })();
        if let Err(error) = result {
            if let Err(rollback) = rollback_restore(dirs) {
                anyhow::bail!(
                    "Restore failed: {error}; ownership-checked rollback failed: {rollback}. Restart to retry reconciliation"
                );
            }
            return Err(error);
        }
        // Keep the journal and markers until the application durably commits
        // the account roster. Startup rolls back an unregistered publication.
        Ok(self.summary.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction_fixture(
        root: &Path,
    ) -> (
        AccountDirs,
        RestoreTransaction,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let dirs = crate::paths::AppDirs::under(root).account(&crate::model::AccountId("2".into()));
        let state_parent = dirs.state.parent().unwrap();
        let cache_parent = dirs.cache.parent().unwrap();
        fs::create_dir_all(state_parent).unwrap();
        fs::create_dir_all(cache_parent).unwrap();
        let state = tempfile::Builder::new()
            .prefix("restore-state-")
            .tempdir_in(state_parent)
            .unwrap();
        let cache = tempfile::Builder::new()
            .prefix("restore-cache-")
            .tempdir_in(cache_parent)
            .unwrap();
        let transaction =
            RestoreTransaction::reserve(&dirs, state.path(), cache.path(), &[7; 32]).unwrap();
        fs::write(
            state.path().join("archive.db"),
            b"synthetic encrypted archive bytes",
        )
        .unwrap();
        fs::write(cache.path().join("fixture.webp"), b"synthetic media").unwrap();
        (dirs, transaction, state.keep(), cache.keep())
    }

    #[test]
    fn restart_discards_only_owned_abandoned_preparations() {
        let root = tempfile::tempdir().unwrap();
        let source = Archive::in_memory().unwrap();
        let package = root.path().join("synthetic.zfbackup");
        source
            .export_backup(&package, "a synthetic passphrase", "fixture", root.path())
            .unwrap();
        let parent = root.path().join("backup-staging");
        let prepared = prepare_restore(&package, "a synthetic passphrase", &parent).unwrap();
        let abandoned = prepared.staging.keep();
        let unrelated = parent.join("restore-unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("fixture"), b"preserved unrelated content").unwrap();
        reconcile_preparations(&parent).unwrap();
        assert!(!abandoned.exists());
        assert_eq!(
            fs::read(unrelated.join("fixture")).unwrap(),
            b"preserved unrelated content"
        );
    }

    #[test]
    fn restart_rolls_back_every_unregistered_restore_publication_boundary() {
        for boundary in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let (dirs, transaction, state, cache) = transaction_fixture(root.path());
            // 0: journal only, 1: key persisted, 2: cache published, 3: both published.
            let mut key_present = boundary >= 1;
            if boundary >= 2 {
                durable_move(&cache, &dirs.cache, false).unwrap();
            }
            if boundary >= 3 {
                durable_move(&state, &dirs.state, false).unwrap();
            }
            let recovered = RestoreTransaction::load(&dirs).unwrap().unwrap();
            recovered
                .rollback_with(&dirs, |identity, digest| {
                    assert_eq!(identity, transaction.key_identity);
                    assert_eq!(digest, encryption::restore_key_digest(&[7; 32]));
                    assert!(
                        !dirs.archive_db().exists(),
                        "remove only owned archive before its key"
                    );
                    key_present = false;
                    Ok(())
                })
                .unwrap();
            assert!(!key_present);
            for path in [&dirs.state, &dirs.cache, &state, &cache] {
                assert!(!path.exists(), "boundary {boundary}: {path:?}");
            }
            assert!(RestoreTransaction::load(&dirs).unwrap().is_none());
        }
    }

    #[test]
    fn durable_registered_restore_keeps_files_and_only_removes_ownership_metadata() {
        let root = tempfile::tempdir().unwrap();
        let (dirs, transaction, state, cache) = transaction_fixture(root.path());
        durable_move(&cache, &dirs.cache, false).unwrap();
        durable_move(&state, &dirs.state, false).unwrap();
        transaction.finish_owned(&dirs).unwrap();
        assert_eq!(
            fs::read(dirs.archive_db()).unwrap(),
            b"synthetic encrypted archive bytes"
        );
        assert_eq!(
            fs::read(dirs.cache.join("fixture.webp")).unwrap(),
            b"synthetic media"
        );
        assert!(!dirs.state.join(OWNER_FILE).exists());
        assert!(!dirs.cache.join(OWNER_FILE).exists());
        assert!(RestoreTransaction::load(&dirs).unwrap().is_none());
    }

    #[test]
    fn conflicting_restore_destination_preserves_every_folder_and_credential() {
        let root = tempfile::tempdir().unwrap();
        let (dirs, transaction, state, cache) = transaction_fixture(root.path());
        fs::create_dir(&dirs.cache).unwrap();
        fs::write(
            dirs.cache.join("unrelated"),
            b"preserve this unrelated fixture",
        )
        .unwrap();
        assert!(durable_move(&cache, &dirs.cache, false).is_err());
        let mut key_touched = false;
        assert!(
            transaction
                .rollback_with(&dirs, |_, _| {
                    key_touched = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!key_touched);
        assert_eq!(
            fs::read(dirs.cache.join("unrelated")).unwrap(),
            b"preserve this unrelated fixture"
        );
        assert!(state.join("archive.db").exists());
        assert!(cache.join("fixture.webp").exists());
        assert!(transaction_path(&dirs).unwrap().exists());
    }

    #[test]
    fn modified_restore_owner_or_traversal_journal_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let (dirs, mut transaction, state, cache) = transaction_fixture(root.path());
        fs::write(cache.join(OWNER_FILE), "f".repeat(64)).unwrap();
        assert!(
            transaction
                .rollback_with(&dirs, |_, _| panic!("must not touch key"))
                .is_err()
        );
        assert!(state.join("archive.db").exists());
        transaction.state_stage = "../unrelated".into();
        fs::write(
            transaction_path(&dirs).unwrap(),
            serde_json::to_vec(&transaction).unwrap(),
        )
        .unwrap();
        assert!(RestoreTransaction::load(&dirs).is_err());
        assert!(state.join("archive.db").exists());
    }

    #[test]
    fn full_account_backup_preserves_session_side_files_settings_and_caches() {
        let root = tempfile::tempdir().unwrap();
        let dirs = AccountDirs {
            id: crate::model::AccountId::first(),
            state: root.path().join("state"),
            cache: root.path().join("cache"),
        };
        dirs.ensure().unwrap();
        let session = rusqlite::Connection::open(dirs.session_db()).unwrap();
        session.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE identity (fixture TEXT); INSERT INTO identity VALUES ('synthetic identity');").unwrap();
        let settings = crate::settings::AccountSettings::default();
        settings.save(&dirs.settings_file()).unwrap();
        fs::create_dir_all(dirs.avatar_cache_dir()).unwrap();
        fs::write(
            dirs.avatar_cache_dir().join("fixture.png"),
            b"synthetic avatar",
        )
        .unwrap();
        fs::create_dir_all(dirs.saved_sticker_dir()).unwrap();
        fs::write(
            dirs.saved_sticker_dir().join("fixture.webp"),
            b"synthetic sticker",
        )
        .unwrap();
        let archive = Archive::in_memory().unwrap();
        let destination = root.path().join("full.zfbackup");
        archive
            .export_account_backup(&destination, "a synthetic passphrase", &dirs, root.path())
            .unwrap();
        let restored =
            prepare_restore(&destination, "a synthetic passphrase", root.path()).unwrap();
        assert!(restored.has_session());
        assert!(
            restored
                .staging
                .path()
                .join("state/session.db-wal")
                .exists()
        );
        assert!(restored.staging.path().join("state/settings.json").exists());
        assert_eq!(
            fs::read(restored.staging.path().join("cache/avatars/fixture.png")).unwrap(),
            b"synthetic avatar"
        );
        assert_eq!(
            fs::read(restored.staging.path().join("state/stickers/fixture.webp")).unwrap(),
            b"synthetic sticker"
        );
        let restored_session =
            rusqlite::Connection::open(restored.staging.path().join("state/session.db")).unwrap();
        let count: i64 = restored_session
            .query_row("SELECT count(*) FROM identity", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(restored_session);
        drop(session);
    }

    #[test]
    fn restore_refuses_existing_account_without_touching_its_files() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::in_memory().unwrap();
        let package = root.path().join("fixture.zfbackup");
        archive
            .export_backup(&package, "a synthetic passphrase", "fixture", root.path())
            .unwrap();
        let restored = prepare_restore(&package, "a synthetic passphrase", root.path()).unwrap();
        let dirs = AccountDirs {
            id: crate::model::AccountId::first(),
            state: root.path().join("existing-state"),
            cache: root.path().join("existing-cache"),
        };
        dirs.ensure().unwrap();
        fs::write(dirs.session_db(), b"existing synthetic identity").unwrap();
        fs::write(dirs.archive_db(), b"existing synthetic archive").unwrap();
        assert!(restored.install_into_empty(&dirs).is_err());
        assert_eq!(
            fs::read(dirs.session_db()).unwrap(),
            b"existing synthetic identity"
        );
        assert_eq!(
            fs::read(dirs.archive_db()).unwrap(),
            b"existing synthetic archive"
        );
    }

    #[test]
    fn account_paths_cannot_escape_restore_roots() {
        for path in [
            "state/../other",
            "cache/../../file",
            "state/C:/file",
            "state/a\\b",
            "other/file",
            "state//file",
            "state/file.",
        ] {
            assert!(!safe_account_path(path), "{path}");
        }
        assert!(safe_account_path("cache/media/nested/file.webp"));
    }

    #[test]
    fn encrypted_frames_reject_wrong_password_tampering_and_reordering() {
        let mut header = [1u8; 24];
        header[..8].copy_from_slice(MAGIC);
        let mut output = Vec::new();
        Cipher::new("a synthetic passphrase", header)
            .unwrap()
            .write(&mut output, b"synthetic attachment")
            .unwrap();
        assert!(!output.windows(9).any(|bytes| bytes == b"synthetic"));
        let clear = Cipher::new("a synthetic passphrase", header)
            .unwrap()
            .read(&mut output.as_slice())
            .unwrap();
        assert_eq!(clear.as_slice(), b"synthetic attachment");
        assert!(
            Cipher::new("wrong synthetic passphrase", header)
                .unwrap()
                .read(&mut output.as_slice())
                .is_err()
        );
        *output.last_mut().unwrap() ^= 1;
        assert!(
            Cipher::new("a synthetic passphrase", header)
                .unwrap()
                .read(&mut output.as_slice())
                .is_err()
        );
    }

    #[test]
    fn short_password_and_existing_destination_are_refused() {
        assert!(Cipher::new("short", [0; 24]).is_err());
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("backup");
        fs::write(&destination, b"existing user file").unwrap();
        assert!(private_create(&destination).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"existing user file");
    }

    #[test]
    fn encrypted_snapshot_round_trip_is_complete_and_truncation_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let source = Archive::in_memory().unwrap();
        let attachment = root.path().join("synthetic.mp4");
        fs::write(&attachment, b"synthetic media fixture").unwrap();
        let mut message =
            super::super::tests::message("fixture@lid", "fixture-message", 100, false);
        message.content = crate::model::Content::Image {
            caption: None,
            media: crate::model::Media {
                mime: "video/mp4".into(),
                size: 23,
                width: None,
                height: None,
                album: None,
                path: Some(attachment),
                state: Default::default(),
            },
            motion: None,
        };
        source.insert_message(&message, None).unwrap();
        let destination = root.path().join("synthetic.zfbackup");
        let summary = source
            .export_backup(
                &destination,
                "a synthetic passphrase",
                "fixture-account",
                root.path(),
            )
            .unwrap();
        assert_eq!(summary.files, 2);
        let restored =
            prepare_restore(&destination, "a synthetic passphrase", root.path()).unwrap();
        assert_eq!(restored.summary.files, 2);
        let connection = encryption::keyed(
            &restored.staging.path().join("archive.db"),
            &restored.database_key,
        )
        .unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read(restored.staging.path().join("media/backup-0.mp4")).unwrap(),
            b"synthetic media fixture"
        );
        assert!(prepare_restore(&destination, "a different passphrase", root.path()).is_err());
        let mut bytes = fs::read(&destination).unwrap();
        bytes.truncate(bytes.len() - 4);
        fs::write(&destination, bytes).unwrap();
        assert!(prepare_restore(&destination, "a synthetic passphrase", root.path()).is_err());
    }
}
