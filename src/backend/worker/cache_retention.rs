//! Opt-in cleanup of old, unreferenced account media-cache files.
//! Referenced attachments and durable background recovery remain an archive.

use super::*;
use std::{collections::HashSet, io, time::SystemTime};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const ENTRY_LIMIT: usize = 4096;

impl Worker {
    fn cache_retention_idle(&self) -> bool {
        self.privacy_ready
            && !self.background_history.paused
            && !self.logout_cleanup_blocked
            && self.downloads.is_empty()
            && self.motion_controls.is_empty()
            && self.background_media.is_empty()
            && self.uploads.is_empty()
            && self.upload_pending.is_empty()
            && self.upload_running.is_none()
            && self.message_removals_in_flight.is_empty()
    }

    /// Call from the existing worker tick. None is due at startup; changes to
    /// the policy reset it to None. Busy/private workers retry on a later tick.
    pub(super) async fn expire_unreferenced_cache(&mut self) {
        let _timing = WorkerTiming::start("cache-retention");
        let Some(days) = self.cache_retention_days.filter(|days| *days > 0) else {
            return;
        };
        if self
            .cache_retention_next
            .is_some_and(|due| Instant::now() < due)
            || !self.cache_retention_idle()
        {
            return;
        }
        self.cache_retention_next = Some(Instant::now() + DAY);
        let references = match self.archive.cache_retention_references() {
            Ok(Some(references)) => references,
            Ok(None) => {
                log::warn!("cache cleanup skipped: archive exceeds the safe reference scan limit");
                return;
            }
            Err(_) => {
                log::warn!("cache cleanup skipped: archive references could not be read");
                return;
            }
        };
        let mut names = HashSet::new();
        for path in references.paths {
            // A restored archive may still contain another platform's path
            // spelling before relocation repairs it. Protect either separator.
            let Some(name) = path
                .to_str()
                .and_then(|path| path.rsplit(['/', '\\']).next())
                .filter(|name| !name.is_empty())
            else {
                log::warn!("cache cleanup skipped: an archive path could not be checked");
                return;
            };
            // Protect matching basenames even outside this root. This is more
            // conservative than resolving aliases, case and relocated paths.
            names.insert(name.to_ascii_lowercase());
        }
        let mut prefixes = HashSet::new();
        for job in references.jobs {
            // All states protect unpublished recovery files. Document names
            // begin with id-, ordinary media with chat-id, and card/motion
            // variants append a suffix after that same prefix.
            prefixes.insert(sanitize(&job.id).to_ascii_lowercase());
            prefixes.insert(format!(
                "{}-{}",
                sanitize(&job.chat).to_ascii_lowercase(),
                sanitize(&job.id).to_ascii_lowercase()
            ));
        }
        let root = self.dirs.media_cache_dir();
        let scan_root = root.clone();
        let export_roots: Vec<_> = references
            .export_roots
            .into_iter()
            .chain(self.download_folder.iter().cloned())
            .collect();
        let candidates = tokio::task::spawn_blocking(move || {
            check_export_roots(&scan_root, &export_roots)?;
            find_candidates(&scan_root, days, SystemTime::now(), &names, &prefixes)
        })
        .await;
        let candidates = match candidates {
            Ok(Ok(candidates)) => candidates,
            _ => {
                log::warn!("cache cleanup skipped: managed files could not be checked safely");
                return;
            }
        };
        // Recheck worker privacy/transfers after the filesystem scan. The
        // worker serializes archive writes, so the reference snapshot cannot
        // change while this method awaits its blocking filesystem tasks.
        if !self.cache_retention_idle() || self.cache_retention_days != Some(days) {
            return;
        }
        let result =
            tokio::task::spawn_blocking(move || remove_candidates(&root, candidates)).await;
        match result {
            Ok(Ok(removed)) if removed > 0 => {
                self.background_media_usage = None;
                log::info!("removed {removed} expired unreferenced media-cache files");
            }
            Ok(Ok(_)) => {}
            _ => log::warn!("cache cleanup stopped: a managed file could not be removed safely"),
        }
    }
}

struct Candidate {
    path: PathBuf,
    modified: SystemTime,
    size: u64,
}

/// Current and previously recorded export folders are user-owned. Check aliases
/// as well as lexical paths, and retain everything if an alias cannot be checked.
fn check_export_roots(root: &Path, folders: &[PathBuf]) -> io::Result<()> {
    let resolved_root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for folder in folders {
        if root.starts_with(folder) || folder.starts_with(root) {
            return Err(io::Error::other("Downloads overlap the managed cache"));
        }
        let resolved_folder = folder.canonicalize()?;
        if resolved_root.starts_with(&resolved_folder)
            || resolved_folder.starts_with(&resolved_root)
        {
            return Err(io::Error::other("Downloads overlap the managed cache"));
        }
    }
    Ok(())
}

/// Refuse redirected ancestors too, rather than checking only the leaf folder.
fn check_root(root: &Path) -> io::Result<()> {
    if !root.is_absolute()
        || root.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::Normal(_)
            )
        })
    {
        return Err(io::Error::other("Invalid managed cache root"));
    }
    for parent in root.ancestors() {
        let metadata = std::fs::symlink_metadata(parent)?;
        if crate::archive::storage::is_link(&metadata) || !metadata.is_dir() {
            return Err(io::Error::other("Managed cache root contains a link"));
        }
    }
    Ok(())
}

fn owned_name(name: &str) -> bool {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem.contains('-')
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        && !extension.is_empty()
        && extension.len() <= 16
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn find_candidates(
    root: &Path,
    days: u32,
    now: SystemTime,
    names: &HashSet<String>,
    prefixes: &HashSet<String>,
) -> io::Result<Vec<Candidate>> {
    match check_root(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result?,
    }
    let age = DAY.saturating_mul(days);
    let mut candidates = Vec::new();
    for (index, entry) in std::fs::read_dir(root)?.enumerate() {
        if index >= ENTRY_LIMIT {
            return Err(io::Error::other(
                "Managed cache exceeds the safe scan limit",
            ));
        }
        let entry = entry?;
        let filename = entry.file_name();
        let Some(name) = filename.to_str() else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        if !owned_name(&name) || names.contains(&name) {
            continue;
        }
        let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap();
        if prefixes.contains(stem)
            || stem.char_indices().any(|(position, character)| {
                matches!(character, '-' | '_') && prefixes.contains(&stem[..position])
            })
        {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || crate::archive::storage::is_link(&metadata) {
            continue;
        }
        let modified = metadata.modified()?;
        if now
            .duration_since(modified)
            .is_ok_and(|elapsed| elapsed >= age)
        {
            candidates.push(Candidate {
                path,
                modified,
                size: metadata.len(),
            });
        }
    }
    Ok(candidates)
}

fn remove_candidates(root: &Path, candidates: Vec<Candidate>) -> io::Result<usize> {
    let mut removed = 0;
    for candidate in candidates {
        check_root(root)?;
        if candidate.path.parent() != Some(root) {
            return Err(io::Error::other(
                "Cache candidate is outside the managed root",
            ));
        }
        let metadata = match std::fs::symlink_metadata(&candidate.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if crate::archive::storage::is_link(&metadata)
            || !metadata.is_file()
            || metadata.len() != candidate.size
            || metadata.modified()? != candidate.modified
        {
            continue;
        }
        std::fs::remove_file(&candidate.path)?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_file(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        let file = std::fs::File::create(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + DAY * 10))
            .unwrap();
        path
    }

    #[test]
    fn only_old_unreferenced_owned_files_expire() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let expired = old_file(&root, "fixture-unused.jpg");
        let archived = old_file(&root, "fixture-archived.jpg");
        let unpublished = old_file(&root, "fixture-job_motion.mp4");
        let carousel = old_file(&root, "fixture-job_card_1.jpg");
        let document = old_file(&root, "job-readable_name.pdf");
        let staging = old_file(&root, ".fixture-old.0000000000000001.part");
        let unknown = old_file(&root, "user.txt");
        let fresh = root.join("fixture-fresh.jpg");
        std::fs::write(&fresh, b"fixture").unwrap();
        std::fs::create_dir(root.join("edited")).unwrap();
        let nested = old_file(&root.join("edited"), "fixture-unused.jpg");
        let candidates = find_candidates(
            &root,
            30,
            SystemTime::UNIX_EPOCH + DAY * 100,
            &HashSet::from(["fixture-archived.jpg".into()]),
            &HashSet::from(["fixture-job".into(), "job".into()]),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, expired);
        assert_eq!(remove_candidates(&root, candidates).unwrap(), 1);
        assert!(!expired.exists());
        for path in [
            archived,
            unpublished,
            carousel,
            document,
            staging,
            unknown,
            fresh,
            nested,
        ] {
            assert!(path.exists());
        }
    }

    #[test]
    fn future_timestamps_and_replaced_candidates_are_retained() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let replaced = old_file(&root, "fixture-replaced.jpg");
        let future = root.join("fixture-future.jpg");
        std::fs::File::create(&future)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + DAY * 200))
            .unwrap();
        let candidates = find_candidates(
            &root,
            30,
            SystemTime::UNIX_EPOCH + DAY * 100,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        std::fs::write(&replaced, b"new contents").unwrap();
        assert_eq!(remove_candidates(&root, candidates).unwrap(), 0);
        assert!(future.exists());
        assert!(replaced.exists());
    }

    #[test]
    fn candidate_outside_account_root_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let outside = old_file(&root, "fixture-outside.jpg");
        let account = root.join("account");
        std::fs::create_dir(&account).unwrap();
        let metadata = std::fs::metadata(&outside).unwrap();
        assert!(
            remove_candidates(
                &account,
                vec![Candidate {
                    path: outside.clone(),
                    modified: metadata.modified().unwrap(),
                    size: metadata.len(),
                }]
            )
            .is_err()
        );
        assert!(outside.exists());
        // canonicalize gives Windows a verbatim prefix. PathBuf::join then
        // normalizes away `..`, so it cannot construct malformed fixture input.
        #[cfg(windows)]
        assert_eq!(account.join(".."), root);
        let mut malformed = account.as_os_str().to_os_string();
        malformed.push(std::path::MAIN_SEPARATOR_STR);
        malformed.push("..");
        let malformed = PathBuf::from(malformed);
        assert!(
            malformed
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        );
        assert!(check_root(&malformed).is_err());
    }

    #[test]
    fn current_and_previous_export_roots_block_overlapping_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let cache = root.join("cache");
        let exports = root.join("exports");
        let nested = cache.join("previous-exports");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(&exports).unwrap();
        assert!(check_export_roots(&cache, std::slice::from_ref(&exports)).is_ok());
        for folder in [root, cache.clone(), nested] {
            assert!(check_export_roots(&cache, &[exports.clone(), folder]).is_err());
        }
        assert!(check_export_roots(&cache, &[exports.join("missing")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn aliased_export_roots_block_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let cache = root.join("cache");
        let alias = root.join("exports-alias");
        std::fs::create_dir(&cache).unwrap();
        std::os::unix::fs::symlink(&cache, &alias).unwrap();
        assert!(check_export_roots(&cache, &[alias]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_roots_and_files_are_never_traversed_or_removed() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let outside = old_file(&root, "fixture-outside.jpg");
        let account = root.join("account");
        std::fs::create_dir(&account).unwrap();
        std::os::unix::fs::symlink(&outside, account.join("fixture-link.jpg")).unwrap();
        std::os::unix::fs::symlink(&account, root.join("linked-account")).unwrap();
        assert!(
            find_candidates(
                &root.join("linked-account"),
                30,
                SystemTime::UNIX_EPOCH + DAY * 100,
                &HashSet::new(),
                &HashSet::new()
            )
            .is_err()
        );
        assert!(
            find_candidates(
                &account,
                30,
                SystemTime::UNIX_EPOCH + DAY * 100,
                &HashSet::new(),
                &HashSet::new()
            )
            .unwrap()
            .is_empty()
        );
        assert!(outside.exists());
    }

    #[tokio::test]
    async fn previous_export_folder_remains_protected_after_changing_downloads() {
        let (mut worker, _, _, _) = super::super::offline_worker("fixture@s.whatsapp.net");
        let root = worker.dirs.media_cache_dir();
        std::fs::create_dir_all(&root).unwrap();
        let file = old_file(&root, "fixture-user-export.jpg");
        let other_downloads = tempfile::tempdir().unwrap();
        worker.download_folder = Some(other_downloads.path().to_path_buf());
        worker
            .archive
            .queue_managed_export(
                &crate::archive::MediaJob {
                    chat: "fixture".into(),
                    id: "old-export".into(),
                    card: -1,
                },
                &root,
                &root.join("different-export.jpg"),
            )
            .unwrap();
        worker.privacy_ready = true;
        worker.cache_retention_days = Some(30);
        worker.expire_unreferenced_cache().await;
        assert!(file.exists());
    }

    #[tokio::test]
    async fn default_and_private_accounts_never_delete_files() {
        let (mut worker, _, _, _) = super::super::offline_worker("fixture@s.whatsapp.net");
        let root = worker.dirs.media_cache_dir();
        std::fs::create_dir_all(&root).unwrap();
        let file = old_file(&root, "fixture-unused.jpg");
        worker.privacy_ready = true;
        worker.cache_retention_days = None;
        worker.expire_unreferenced_cache().await;
        assert!(file.exists());
        worker.cache_retention_days = Some(30);
        worker.privacy_ready = false;
        worker.expire_unreferenced_cache().await;
        assert!(file.exists());
        assert!(worker.cache_retention_next.is_none());
    }
}
