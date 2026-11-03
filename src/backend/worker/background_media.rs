//! Persistent media recovery independent of visible chats and user exports.

use super::*;
use crate::archive::MediaJob;

impl Worker {
    pub(super) fn automatic_export_allowed(&self, chat: &str, id: &str) -> bool {
        self.archive
            .message(chat, id)
            .ok()
            .flatten()
            .is_some_and(|message| self.automatic_exports.includes(message.timestamp))
    }

    pub(super) fn managed_export_destination(
        &self,
        job: &MediaJob,
    ) -> crate::archive::Result<Option<(PathBuf, PathBuf)>> {
        let Some(root) = self.download_folder.as_ref() else {
            return Ok(None);
        };
        if !self.privacy_ready
            || self.background_history.paused
            || self.logout_cleanup_blocked
            || !self.automatic_export_allowed(&job.chat, &job.id)
        {
            return Ok(None);
        }
        let Some(chat) = self.archive.chat(&job.chat)? else {
            return Ok(None);
        };
        if chat.locked
            || chat.left
            || self.archive.message_removed(&job.chat, &job.id)?
            || self.archive.export_deletion_pending(&job.chat, &job.id)?
        {
            return Ok(None);
        }
        let Some(mut message) = self.archive.message(&job.chat, &job.id)? else {
            return Ok(None);
        };
        if self
            .archive
            .removal_point(&job.chat)?
            .is_some_and(|through| message.timestamp <= through)
        {
            return Ok(None);
        }
        let Some(raw) = self.archive.raw(&job.chat, &job.id)? else {
            return Ok(None);
        };
        let Ok(raw) = wa::Message::decode_from_slice(&raw) else {
            return Ok(None);
        };
        if raw.is_view_once()
            || raw
                .get_ephemeral_expiration()
                .is_some_and(|seconds| seconds > 0)
        {
            return Ok(None);
        }
        let file_name = match &message.content {
            Content::Document { file_name, .. } => Some(file_name.clone()),
            _ => None,
        };
        let mime = if job.card == -2 {
            if !matches!(
                &message.content,
                Content::Image {
                    motion: Some(_),
                    ..
                }
            ) {
                return Ok(None);
            }
            let Some(child_id) = self.archive.motion_child_id(&job.chat, &job.id)? else {
                return Ok(None);
            };
            if self.archive.message_removed(&job.chat, &child_id)?
                || self.archive.export_deletion_pending(&job.chat, &child_id)?
            {
                return Ok(None);
            }
            let Some(clip) = self.archive.motion_clip(&job.chat, &job.id)? else {
                return Ok(None);
            };
            let Ok(clip) = wa::Message::decode_from_slice(&clip) else {
                return Ok(None);
            };
            if clip.is_view_once()
                || clip
                    .get_ephemeral_expiration()
                    .is_some_and(|seconds| seconds > 0)
            {
                return Ok(None);
            }
            let Some(video) = clip.video_message.as_option() else {
                return Ok(None);
            };
            video.mimetype.clone().unwrap_or_else(|| "video/mp4".into())
        } else {
            let Some(media) = message
                .content
                .media_at_mut((job.card >= 0).then_some(job.card as usize))
            else {
                return Ok(None);
            };
            media.mime.clone()
        };
        let cache_id = if job.card == -2 {
            format!("{}-motion", job.id)
        } else if job.card >= 0 {
            format!("{}-card-{}", job.id, job.card)
        } else {
            job.id.clone()
        };
        let destination = download_organization::path_with_original_names(
            root,
            self.download_organization,
            self.descriptive_download_names,
            self.automatic_exports.original_names,
            download_organization::Metadata {
                account: self.dirs.id.as_str(),
                chat: &job.chat,
                chat_name: &chat.name,
                message: &cache_id,
                timestamp: message.timestamp,
                mime: &mime,
                original_name: if job.card == -2 {
                    Some("motion.mp4")
                } else {
                    file_name.as_deref()
                },
            },
        );
        Ok(Some((root.clone(), destination)))
    }

    async fn pump_managed_exports(&mut self) {
        let _timing = WorkerTiming::start("managed-exports");
        if !self.privacy_ready
            || !self.automatic_exports.enabled
            || self.background_history.paused
            || self.logout_cleanup_blocked
            || !self.status.is_connected()
        {
            return;
        }
        let now = crate::util::now();
        let exports = match self.archive.due_managed_exports(now) {
            Ok(exports) => exports,
            Err(_) => {
                log::warn!("could not inspect organized archive copies");
                return;
            }
        };
        for export in exports {
            let current = match self.managed_export_destination(&export.job) {
                Ok(current) => current,
                Err(_) => continue,
            };
            if current
                .as_ref()
                .is_none_or(|(root, _)| *root != export.root)
            {
                let _ = self.archive.forget_managed_export(&export.job);
                continue;
            }
            let source = self
                .archive
                .message(&export.job.chat, &export.job.id)
                .ok()
                .flatten()
                .and_then(|mut message| {
                    if export.job.card == -2 {
                        return match message.content {
                            Content::Image {
                                motion: Some(motion),
                                ..
                            } => motion.path,
                            _ => None,
                        };
                    }
                    message
                        .content
                        .media_at_mut((export.job.card >= 0).then_some(export.job.card as usize))
                        .and_then(|media| media.path.clone())
                });
            let Some(source) = source else {
                continue;
            };
            if !source.starts_with(self.dirs.media_cache_dir()) {
                let _ = self.archive.forget_managed_export(&export.job);
                continue;
            }
            let root = export.root;
            let destination = export.destination;
            let managed_root = self.dirs.media_cache_dir();
            let success = tokio::task::spawn_blocking(move || {
                if managed_file_missing(&managed_root, &source)? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Managed attachment is missing",
                    ));
                }
                download_organization::copy_verified(&source, &root, &destination)
            })
            .await
            .is_ok_and(|result| result.is_ok());
            if self
                .archive
                .finish_managed_export(&export.job, now, success)
                .is_err()
            {
                log::warn!("could not acknowledge organized archive copy");
            }
            if !success {
                self.emit_message(&export.job.chat, &export.job.id);
                let result = Err("Saved in archive; organized copy could not be saved".into());
                if export.job.card == -2 {
                    self.emit(Event::Motion {
                        chat: export.job.chat,
                        message: export.job.id,
                        result,
                    });
                } else {
                    self.emit(Event::Media {
                        chat: export.job.chat,
                        message: export.job.id,
                        card: (export.job.card >= 0).then_some(export.job.card as usize),
                        result,
                    });
                }
            }
        }
    }

    pub(super) async fn pump_background_media(&mut self) {
        let _timing = WorkerTiming::start("background-media");
        self.pump_managed_exports().await;
        if self
            .background_media_usage
            .as_ref()
            .is_none_or(|(checked, _)| checked.elapsed() >= Duration::from_secs(30))
        {
            let _timing = WorkerTiming::start("media-usage-scan");
            let root = self.dirs.media_cache_dir();
            let usage = tokio::task::spawn_blocking(move || {
                crate::archive::storage::managed_media_bytes(&root)
            })
            .await;
            self.background_media_usage =
                Some((Instant::now(), usage.ok().and_then(|result| result.ok())));
        }
        let bytes = self
            .background_media_usage
            .as_ref()
            .and_then(|(_, bytes)| *bytes);
        let limit = u64::from(self.background_media_budget_mib) * 1024 * 1024;
        let budget_reached = bytes.is_none_or(|bytes| {
            limit != 0
                && bytes.saturating_add(
                    (self.background_media.len() as u64 + 1) * ATTACHMENT_DOWNLOAD_LIMIT,
                ) > limit
        });
        if let Ok((pending, downloaded, unavailable)) = self.archive.media_job_counts() {
            let Ok(paused) = self.archive.paused_media_job_count() else {
                log::warn!("could not read cancelled background media count");
                return;
            };
            self.emit(Event::ArchiveProgress(
                crate::model::ArchiveRecoveryProgress {
                    pending,
                    paused,
                    downloaded,
                    unavailable,
                    bytes,
                    budget_reached,
                    oldest_saved: self.archive.oldest_saved_timestamp().ok().flatten(),
                    phone_history: self.background_history.response,
                    connected: self.status.is_connected(),
                },
            ));
        }
        if !self.background_media_enabled
            || self.background_history.paused
            || !self.privacy_ready
            || !self.status.is_connected()
            || self.client.is_none()
        {
            return;
        }
        if WorkerTiming::run("seed-media-jobs", || self.archive.seed_media_jobs()).is_err() {
            log::warn!("could not seed background media jobs");
            return;
        }
        self.audit_completed_media().await;
        if budget_reached {
            return;
        }
        let available = 2_usize.saturating_sub(self.background_media.len());
        if available == 0 || self.downloads.len() >= 6 {
            return;
        }
        let now = crate::util::now();
        let jobs = match self
            .archive
            .due_media_jobs(now, available + self.downloads.len())
        {
            Ok(jobs) => jobs,
            Err(_) => {
                log::warn!("could not read background media jobs");
                return;
            }
        };
        for job in jobs {
            if self.background_media.len() >= 2 || self.downloads.len() >= 6 {
                break;
            }
            if limit != 0
                && bytes.unwrap_or(0).saturating_add(
                    (self.background_media.len() as u64 + 1) * ATTACHMENT_DOWNLOAD_LIMIT,
                ) > limit
            {
                break;
            }
            let card = (job.card >= 0).then_some(job.card as usize);
            if self
                .background_media
                .contains(&(job.chat.clone(), job.id.clone(), job.card))
                || (job.card != -2
                    && self
                        .downloads
                        .contains(&(job.chat.clone(), job.id.clone(), card)))
            {
                continue;
            }
            let mut message = match self.archive.message(&job.chat, &job.id) {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(_) => {
                    log::warn!("could not read background media metadata");
                    break;
                }
            };
            let path = if job.card == -2 {
                match &message.content {
                    Content::Image {
                        motion: Some(motion),
                        ..
                    } => motion.path.as_ref(),
                    _ => None,
                }
            } else {
                message
                    .content
                    .media_at_mut(card)
                    .and_then(|media| media.path.as_ref())
            };
            if path.is_some_and(|path| path.is_file()) {
                if path.is_some_and(|path| path.starts_with(self.dirs.media_cache_dir()))
                    && let Ok(Some((root, destination))) = self.managed_export_destination(&job)
                {
                    let _ = self.archive.queue_managed_export(&job, &root, &destination);
                }
                if self
                    .archive
                    .finish_media_job(&job, now, true, false)
                    .is_err()
                {
                    log::warn!("could not complete cached background media job");
                }
                continue;
            }
            if self.archive.start_media_job(&job, now).is_err() {
                log::warn!("could not persist background media attempt");
                break;
            }
            self.background_media
                .insert((job.chat.clone(), job.id.clone(), job.card));
            if job.card == -2 {
                self.download_motion(job.chat, job.id);
            } else {
                self.download_media_managed(job.chat, job.id, card);
            }
        }
    }

    pub(super) async fn audit_completed_media(&mut self) {
        let _timing = WorkerTiming::start("completed-media-audit");
        // Keep the same account, lock, privacy and connection gates as scheduling.
        if !self.background_media_enabled
            || self.background_history.paused
            || !self.privacy_ready
            || !self.status.is_connected()
            || self.client.is_none()
            || self.logout_cleanup_blocked
        {
            return;
        }
        let jobs = match self.archive.completed_media_audit_batch(crate::util::now()) {
            Ok(jobs) => jobs,
            Err(_) => {
                log::warn!("could not read completed media audit");
                return;
            }
        };
        let root = self.dirs.media_cache_dir();
        for (rowid, job) in jobs {
            let mut message = match self.archive.message(&job.chat, &job.id) {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(_) => {
                    log::warn!("could not read completed media metadata");
                    return;
                }
            };
            let supported = if job.card == -2 {
                match &message.content {
                    Content::Image {
                        motion: Some(motion),
                        ..
                    } => Some(motion.path.clone()),
                    _ => None,
                }
            } else {
                message
                    .content
                    .media_at_mut((job.card >= 0).then_some(job.card as usize))
                    .map(|media| media.path.clone())
            };
            if let Some(path) = supported {
                let missing = match &path {
                    None => true,
                    Some(path) => {
                        let (root, path) = (root.clone(), path.clone());
                        tokio::task::spawn_blocking(move || managed_file_missing(&root, &path))
                            .await
                            .is_ok_and(|result| result.is_ok_and(|missing| missing))
                    }
                };
                if missing {
                    match self.archive.requeue_missing_media(&job, path.as_deref()) {
                        Ok(true) => {
                            self.background_media_usage = None;
                            self.emit_message(&job.chat, &job.id);
                        }
                        Ok(false) => {}
                        Err(_) => {
                            log::warn!("could not requeue missing managed media");
                            return;
                        }
                    }
                } else if path.as_ref().is_some_and(|path| path.starts_with(&root))
                    && let Ok(Some((root, destination))) = self.managed_export_destination(&job)
                {
                    // Historical opt-in can copy an already retained file without fetching it again.
                    let _ = self.archive.queue_managed_export(&job, &root, &destination);
                }
            }
            if self.archive.advance_completed_media_audit(rowid).is_err() {
                log::warn!("could not preserve completed media audit cursor");
                return;
            }
        }
    }

    pub(super) fn pause_background_media_intent(&self, key: &(ChatId, String, i64)) {
        let job = MediaJob {
            chat: key.0.clone(),
            id: key.1.clone(),
            card: key.2,
        };
        if self.archive.pause_media_job(&job).is_err() {
            log::warn!("could not preserve background media cancellation");
        }
    }

    pub(super) fn background_media_finished(
        &mut self,
        chat: &str,
        id: &str,
        card: i64,
        result: &Result<PathBuf, String>,
    ) {
        self.background_media_usage = None;
        let owned = self
            .background_media
            .remove(&(chat.to_owned(), id.to_owned(), card));
        let user_cancelled =
            self.background_media_cancelled
                .remove(&(chat.to_owned(), id.to_owned(), card));
        if !owned && result.is_err() {
            return;
        }
        let unavailable = result.as_ref().err().is_some_and(|error| {
            error.contains("No longer available")
                || error.contains("exceeds the download limit")
                || error == ATTACHMENT_LIMIT_ERROR
                || error.contains("download keys are missing")
                || error.contains("without its original download identity")
                || error.contains("no downloadable")
        });
        let job = MediaJob {
            chat: chat.to_owned(),
            id: id.to_owned(),
            card,
        };
        if owned && user_cancelled && result.is_err() {
            if self.archive.pause_media_job(&job).is_err() {
                log::warn!("could not preserve background media cancellation");
            }
            return;
        }
        if self
            .archive
            .finish_media_job(&job, crate::util::now(), result.is_ok(), unavailable)
            .is_err()
        {
            // Lease remains retriable; never pretend a failed archive write completed.
            log::warn!("could not persist background media completion");
        }
    }
}

/// Inspect metadata only, without following links or traversing outside this account.
fn managed_file_missing(root: &std::path::Path, path: &std::path::Path) -> std::io::Result<bool> {
    use std::{io, path::Component};
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "File is outside managed storage",
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid managed file path",
        ));
    }
    let mut current = root.to_path_buf();
    for part in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(part) = part {
            current.push(part.as_os_str());
        }
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        };
        #[cfg(windows)]
        let is_link = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        };
        if is_link {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Managed file path contains a link",
            ));
        }
        if current == path && !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Managed attachment is not a file",
            ));
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::managed_file_missing;

    #[test]
    fn managed_exports_require_selected_folder_preferences_and_current_privacy() {
        use super::*;
        let (mut worker, _, _, _) = super::super::offline_worker("me@s.whatsapp.net");
        let chat = "fixture@s.whatsapp.net";
        worker.background_media_enabled = true;
        worker.privacy_ready = true;
        worker.archive.ensure_chat(chat, "Synthetic").unwrap();
        let raw = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                mimetype: Some("image/jpeg".into()),
                file_length: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut row = crate::archive::tests::message(chat, "photo", 100, false);
        row.content = classify(&raw).unwrap();
        worker
            .archive
            .insert_message(&row, Some(&raw.encode_to_vec()))
            .unwrap();
        let job = MediaJob {
            chat: chat.into(),
            id: row.id.clone(),
            card: -1,
        };
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        worker.download_folder = Some(PathBuf::from("selected-user-folder"));
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        worker.automatic_exports = crate::settings::AutomaticExportPolicy {
            enabled: true,
            since: Some(101),
            ..Default::default()
        };
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        worker.automatic_exports.include_older = true;
        assert!(worker.managed_export_destination(&job).unwrap().is_some());
        worker.automatic_exports.include_older = false;
        worker.automatic_exports.since = Some(100);
        assert!(worker.managed_export_destination(&job).unwrap().is_some());
        worker.descriptive_download_names = true;
        let (root, destination) = worker.managed_export_destination(&job).unwrap().unwrap();
        assert!(destination.starts_with(root));
        worker.archive.set_locked(chat, true).unwrap();
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        worker.archive.set_locked(chat, false).unwrap();
        worker.privacy_ready = false;
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        worker.privacy_ready = true;
        worker
            .archive
            .queue_message_removal("1", chat, "photo")
            .unwrap();
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
    }

    #[test]
    fn organized_motion_uses_child_video_and_preserves_parent_image_path() {
        use super::*;
        let (mut worker, _, _, _) = super::super::offline_worker("me@s.whatsapp.net");
        let chat = "fixture@s.whatsapp.net";
        worker.background_media_enabled = true;
        worker.automatic_exports = crate::settings::AutomaticExportPolicy {
            enabled: true,
            since: Some(0),
            ..Default::default()
        };
        worker.privacy_ready = true;
        worker.download_folder = Some(PathBuf::from("selected-user-folder"));
        worker.descriptive_download_names = true;
        worker.archive.ensure_chat(chat, "Synthetic").unwrap();
        let raw = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                mimetype: Some("image/jpeg".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut row = crate::archive::tests::message(chat, "photo", 100, false);
        row.content = classify(&raw).unwrap();
        worker
            .archive
            .insert_message(&row, Some(&raw.encode_to_vec()))
            .unwrap();
        worker
            .archive
            .put_media_path_at(chat, "photo", None, Some(Path::new("parent.jpg")))
            .unwrap();
        let mut clip = wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                mimetype: Some("video/mp4".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        worker
            .archive
            .put_motion_clip_with_id(chat, "photo", &row.sender, &clip.encode_to_vec(), None)
            .unwrap();
        let job = MediaJob {
            chat: chat.into(),
            id: "photo".into(),
            card: -2,
        };
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        worker
            .archive
            .put_motion_clip_with_id(
                chat,
                "photo",
                &row.sender,
                &clip.encode_to_vec(),
                Some("child-video"),
            )
            .unwrap();
        let (root, destination) = worker.managed_export_destination(&job).unwrap().unwrap();
        assert_eq!(destination.extension().unwrap(), "mp4");
        assert!(
            destination
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .contains("motion")
        );
        worker
            .archive
            .put_media_path_and_export(&job, Path::new("clip.mp4"), Some((&root, &destination)))
            .unwrap();
        let saved = worker.archive.message(chat, "photo").unwrap().unwrap();
        let Content::Image {
            media,
            motion: Some(motion),
            ..
        } = saved.content
        else {
            panic!("motion image expected")
        };
        assert_eq!(media.path, Some(PathBuf::from("parent.jpg")));
        assert_eq!(motion.path, Some(PathBuf::from("clip.mp4")));
        assert_eq!(
            worker.archive.due_managed_exports(1).unwrap()[0].job.card,
            -2
        );
        clip.video_message.as_option_mut().unwrap().view_once = Some(true);
        worker
            .archive
            .put_motion_clip_with_id(
                chat,
                "photo",
                &row.sender,
                &clip.encode_to_vec(),
                Some("child-video"),
            )
            .unwrap();
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
        clip.video_message.as_option_mut().unwrap().view_once = Some(false);
        worker
            .archive
            .put_motion_clip_with_id(
                chat,
                "photo",
                &row.sender,
                &clip.encode_to_vec(),
                Some("child-video"),
            )
            .unwrap();
        worker
            .archive
            .queue_message_removal("1", chat, "child-video")
            .unwrap();
        assert!(worker.managed_export_destination(&job).unwrap().is_none());
    }

    #[test]
    fn missing_managed_file_probe_preserves_external_and_unexpected_paths() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("attachment");
        std::fs::write(&existing, b"synthetic").unwrap();
        assert!(!managed_file_missing(root.path(), &existing).unwrap());
        std::fs::remove_file(&existing).unwrap();
        assert!(managed_file_missing(root.path(), &existing).unwrap());
        let external = tempfile::tempdir().unwrap();
        assert!(managed_file_missing(root.path(), &external.path().join("missing")).is_err());
        assert!(managed_file_missing(root.path(), &root.path().join("../missing")).is_err());
        let directory = root.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(managed_file_missing(root.path(), &directory).is_err());
    }
}
