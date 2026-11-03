//! Read-only, bounded, session-local stories received through normal delivery.
use super::*;
use crate::model::MediaState;

const LIFETIME: i64 = 24 * 60 * 60;
const LIMIT: usize = 256;
const RAW_LIMIT: usize = 128 * 1024;
const REVOCATION_LIMIT: usize = 1024;

struct StatusDownload {
    task: tokio::task::JoinHandle<()>,
    control: transfers::Control,
    path: PathBuf,
}

#[derive(Default)]
pub(super) struct Stories {
    rows: HashMap<String, (Message, Arc<wa::Message>)>,
    downloads: HashMap<String, StatusDownload>,
    directory: Option<tempfile::TempDir>,
    revoked: HashMap<String, i64>,
    suppressed_until: i64,
    last_publication: Option<Instant>,
}

impl Drop for Stories {
    fn drop(&mut self) {
        for download in self.downloads.values() {
            download.control.cancel();
            download.task.abort();
        }
    }
}
impl Stories {
    pub(super) fn rows_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
impl Worker {
    /// Marks one update seen here, stamping its local row. The phone is
    /// not told. True when the update was still unseen.
    ///
    /// The key's sender is canonicalized before comparing: a privacy-id
    /// mapping learned after delivery changes the sender a later key
    /// carries, while the stored row keeps the earlier one.
    pub(super) fn mark_status_read(&mut self, key: &str) -> bool {
        let Some((sender, id)) = key.split_once(':') else {
            return false;
        };
        let wanted = self.canonical_str(sender);
        let found = self
            .stories
            .rows
            .keys()
            .find(|stored| {
                stored
                    .split_once(':')
                    .is_some_and(|(stored_sender, stored_id)| {
                        stored_id == id && self.canonical_str(stored_sender) == wanted
                    })
            })
            .cloned();
        let Some(found) = found else {
            return false;
        };
        let Some((row, _)) = self.stories.rows.get_mut(&found) else {
            return false;
        };
        if row.read_at.is_some() {
            return false;
        }
        row.read_at = Some(crate::util::now());
        true
    }
    pub(super) fn ingest_status(&mut self, raw: &Arc<wa::Message>, info: &MessageInfo) {
        let timestamp = info.timestamp.timestamp();
        let now = crate::util::now();
        if timestamp <= now - LIFETIME
            || timestamp > now + 300
            || info.id.len() > 256
            || !raw
                .try_encoded_len()
                .is_ok_and(|size| size as usize <= RAW_LIMIT)
        {
            return;
        }
        let sender = if info.source.is_from_me {
            self.me()
        } else {
            self.canonical(&info.source.sender)
        };
        let key = format!("{sender}:{}", info.id);
        self.stories.revoked.retain(|_, expires| *expires > now);
        let base = raw.get_base_message();
        if let Some(protocol) = base.protocol_message.as_option() {
            if protocol.r#type == Some(wa::message::protocol_message::Type::REVOKE)
                && let Some(id) = protocol.key.as_option().and_then(|key| key.id.as_ref())
            {
                if id.len() > 256 {
                    return;
                }
                let key = format!("{sender}:{id}");
                if !self.stories.revoked.contains_key(&key)
                    && self.stories.revoked.len() >= REVOCATION_LIMIT
                {
                    // Never evict a live barrier and accidentally restore revoked content.
                    self.stories.suppressed_until = now + LIFETIME;
                    let keys: Vec<_> = self.stories.rows.keys().cloned().collect();
                    for key in keys {
                        self.remove_status(&key);
                    }
                    self.stories.revoked.clear();
                } else {
                    self.stories.revoked.insert(key.clone(), now + LIFETIME);
                    let matching: Vec<_> = self
                        .stories
                        .rows
                        .iter()
                        .filter(|(key, (row, _))| {
                            key.split_once(':').is_some_and(|(_, old_id)| old_id == id)
                                && self.canonical_str(&row.sender) == sender
                        })
                        .map(|(key, _)| key.clone())
                        .collect();
                    for key in matching {
                        self.remove_status(&key);
                    }
                }
                self.emit_statuses();
            }
            return;
        }
        if now < self.stories.suppressed_until
            || self.stories.revoked.keys().any(|key| {
                key.split_once(':').is_some_and(|(old_sender, id)| {
                    id == info.id && self.canonical_str(old_sender) == sender
                })
            })
        {
            return;
        }
        if self.stories.rows.iter().any(|(old_key, (row, _))| {
            old_key.split_once(':').is_some_and(|(_, id)| id == info.id)
                && self.canonical_str(&row.sender) == sender
        }) {
            return;
        }
        let Some(content) = classify(raw) else {
            return;
        };
        if !matches!(
            content,
            Content::Text { .. } | Content::Image { .. } | Content::Video { .. }
        ) {
            return;
        }
        if !self.stories.rows.contains_key(&key)
            && self.stories.rows.len() >= LIMIT
            && let Some(oldest) = self
                .stories
                .rows
                .iter()
                .min_by_key(|(_, (row, _))| row.timestamp)
                .map(|(key, _)| key.clone())
        {
            self.remove_status(&oldest);
        }
        let row = Message {
            id: key.clone(),
            chat: sender.clone(),
            sender,
            sender_name: (!info.push_name.is_empty())
                .then(|| info.push_name.chars().take(96).collect()),
            from_me: info.source.is_from_me,
            timestamp,
            content,
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            history_order: None,
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: thumbnail_of(base),
        };
        self.stories.rows.insert(key, (row, raw.clone()));
        // Coalesce arrival bursts. Scheduled worker refresh and an explicit Status
        // request always publish the newest list, while revocation remains immediate.
        if self
            .stories
            .last_publication
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(100))
        {
            self.emit_statuses();
        }
    }
    fn remove_status(&mut self, key: &str) {
        if let Some(download) = self.stories.downloads.remove(key) {
            download.control.cancel();
            download.task.abort();
            let _ = std::fs::remove_file(download.path);
        }
        if let Some((row, _)) = self.stories.rows.remove(key)
            && let Some(path) = row.content.media().and_then(|media| media.path.as_ref())
        {
            let _ = std::fs::remove_file(path);
        }
    }
    pub(super) fn emit_statuses(&mut self) {
        self.stories.last_publication = Some(Instant::now());
        let expired: Vec<_> = self
            .stories
            .rows
            .iter()
            .filter(|(_, (row, _))| {
                row.timestamp <= crate::util::now() - LIFETIME
                    || !self.status_sender_visible(&row.sender)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.remove_status(&key);
        }
        let mut rows: Vec<_> = self
            .stories
            .rows
            .values()
            .map(|(row, _)| {
                let mut row = row.clone();
                row.sender = self.canonical_str(&row.sender);
                row.chat = row.sender.clone();
                row
            })
            .collect();
        rows.sort_by_key(|row| row.timestamp);
        self.emit(Event::Statuses(rows));
    }
    pub(super) fn download_status(&mut self, key: String) {
        let Some((row, raw)) = self.stories.rows.get(&key) else {
            return;
        };
        if self.stories.downloads.contains_key(&key)
            || self.stories.downloads.len() >= 2
            || row.timestamp <= crate::util::now() - LIFETIME
            || row
                .content
                .media()
                .is_some_and(|media| media.path.is_some())
            || !self.status_sender_visible(&row.sender)
        {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let base = raw.get_base_message();
        let downloadable: Box<dyn Downloadable> =
            if let Some(image) = base.image_message.as_option() {
                Box::new(image.clone())
            } else if let Some(video) = base.video_message.as_option() {
                Box::new(video.clone())
            } else {
                return;
            };
        if downloadable
            .file_length()
            .is_some_and(|size| size > ATTACHMENT_DOWNLOAD_LIMIT)
        {
            return;
        }
        let suffix = if base.image_message.is_set() {
            "jpg"
        } else {
            "mp4"
        };
        if self.stories.directory.is_none() {
            self.stories.directory = tempfile::tempdir().ok();
        }
        let Some(directory) = self.stories.directory.as_ref() else {
            return;
        };
        let dir = directory.path().to_owned();
        self.transfer_sequence += 1;
        let path = dir.join(format!("{}.{}", self.transfer_sequence, suffix));
        let control = transfers::Control::new(
            self.transfer_sequence,
            self.commands.clone(),
            "status@broadcast".into(),
            key.clone(),
            None,
            downloadable.file_length(),
            ATTACHMENT_DOWNLOAD_LIMIT,
        );
        let commands = self.commands.clone();
        let result_key = key.clone();
        let tracked_control = control.clone();
        let tracked_path = path.clone();
        let task = tokio::spawn(async move {
            let token = control.token;
            let result = control
                .run(
                    ATTACHMENT_TIMEOUT,
                    transfers::download(&client, &*downloadable, &dir, &path, control.clone()),
                )
                .await;
            let _ = commands.send(Command::StatusDownloaded {
                key: result_key,
                token,
                result,
            });
        });
        self.stories.downloads.insert(
            key.clone(),
            StatusDownload {
                task,
                control: tracked_control,
                path: tracked_path,
            },
        );
        if let Some((row, _)) = self.stories.rows.get_mut(&key)
            && let Some(media) = row.content.media_mut()
        {
            media.state = MediaState::Downloading;
        }
        self.emit_statuses();
    }
    pub(super) fn status_downloaded(
        &mut self,
        key: String,
        token: u64,
        result: Result<PathBuf, String>,
    ) {
        if !self
            .stories
            .downloads
            .get(&key)
            .is_some_and(|download| download.control.token == token)
        {
            if let Ok(path) = result {
                self.remove_owned_status_file(&path);
            }
            return;
        }
        let Some(download) = self.stories.downloads.remove(&key) else {
            // Old completions cannot affect a new link or a revoked/replaced row.
            if let Ok(path) = result {
                self.remove_owned_status_file(&path);
            }
            return;
        };
        let expected = download.path;
        let permitted = self.stories.rows.get(&key).is_some_and(|(row, _)| {
            row.timestamp > crate::util::now() - LIFETIME && self.status_sender_visible(&row.sender)
        });
        if !permitted {
            if let Ok(path) = result {
                self.remove_owned_status_file(&path);
            }
            return;
        }
        if let Some((row, _)) = self.stories.rows.get_mut(&key)
            && let Some(media) = row.content.media_mut()
        {
            match result {
                Ok(path)
                    if path == expected
                        && self
                            .stories
                            .directory
                            .as_ref()
                            .is_some_and(|directory| path.parent() == Some(directory.path())) =>
                {
                    media.path = Some(path);
                    media.state = MediaState::Idle;
                }
                Ok(_) | Err(_) => {
                    media.state =
                        MediaState::Failed("Could not download status. Click to retry.".into())
                }
            }
        }
        self.emit_statuses();
    }
    fn remove_owned_status_file(&self, path: &Path) {
        if self
            .stories
            .directory
            .as_ref()
            .is_some_and(|directory| path.parent() == Some(directory.path()))
        {
            let _ = std::fs::remove_file(path);
        }
    }
    fn status_sender_visible(&self, sender: &str) -> bool {
        self.archive
            .chat(&self.canonical_str(sender))
            .is_ok_and(|chat| chat.is_none_or(|chat| !chat.locked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn revoke(id: &str) -> Arc<wa::Message> {
        Arc::new(wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                r#type: Some(wa::message::protocol_message::Type::REVOKE),
                key: MessageField::some(wa::MessageKey {
                    id: Some(id.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    #[test]
    fn marking_a_status_seen_stamps_its_local_row_only() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let (raw, info) = delivery("story", crate::util::now());
        worker.ingest(&raw, &info);
        let key = worker.stories.rows.keys().next().unwrap().clone();
        // The interface names an update by its row id alone: it already is
        // the worker's `sender:id` key.
        assert_eq!(worker.stories.rows.values().next().unwrap().0.id, key);
        assert!(
            worker
                .stories
                .rows
                .values()
                .next()
                .unwrap()
                .0
                .read_at
                .is_none()
        );
        assert!(worker.mark_status_read(&key));
        assert!(
            worker
                .stories
                .rows
                .values()
                .next()
                .unwrap()
                .0
                .read_at
                .is_some()
        );
        assert!(!worker.mark_status_read(&key), "a repeat changes nothing");
        assert!(!worker.mark_status_read("nobody:missing"));
        // A privacy-id mapping learned after delivery changes the sender a
        // later key carries; the stored row keeps the earlier one.
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let (raw, mut info) = delivery("story", crate::util::now());
        info.source.sender = "98765@lid".parse().unwrap();
        worker.ingest(&raw, &info);
        let stored = worker.stories.rows.keys().next().unwrap().clone();
        worker.lid_to_pn.insert("98765".into(), "12345".into());
        let later = format!("12345@s.whatsapp.net:{}", stored.split_once(':').unwrap().1);
        assert_ne!(stored, later);
        assert!(worker.mark_status_read(&later));
    }
    #[test]
    fn revocations_survive_duplicate_delivery_and_later_privacy_id_mapping() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let (raw, mut info) = delivery("story", crate::util::now());
        info.source.sender = "98765@lid".parse().unwrap();
        worker.ingest(&raw, &info);
        assert_eq!(worker.stories.rows.len(), 1);
        worker.lid_to_pn.insert("98765".into(), "12345".into());
        let (_, info) = delivery("revoke", crate::util::now());
        worker.ingest(&revoke("story"), &info);
        assert!(worker.stories.rows.is_empty());
        let (raw, info) = delivery("story", crate::util::now());
        worker.ingest(&raw, &info);
        assert!(worker.stories.rows.is_empty());
    }
    #[test]
    fn oversized_raw_and_full_revocation_barriers_fail_closed() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let (_, info) = delivery("too-large", crate::util::now());
        let oversized = Arc::new(wa::Message {
            conversation: Some("x".repeat(RAW_LIMIT + 1)),
            ..Default::default()
        });
        worker.ingest(&oversized, &info);
        assert!(worker.stories.rows.is_empty());
        for n in 0..REVOCATION_LIMIT {
            worker.stories.revoked.insert(
                format!("12345@s.whatsapp.net:{n}"),
                crate::util::now() + LIFETIME,
            );
        }
        worker.ingest(&revoke("new-revocation"), &info);
        assert!(worker.stories.revoked.len() <= REVOCATION_LIMIT);
        let (raw, info) = delivery("new-story", crate::util::now());
        worker.ingest(&raw, &info);
        assert!(worker.stories.rows.is_empty());
    }
    #[tokio::test]
    async fn stale_completion_does_not_consume_new_transfer_and_removal_cleans_owned_file() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let (raw, info) = delivery("story", crate::util::now());
        worker.ingest(&raw, &info);
        let key = worker.stories.rows.keys().next().unwrap().clone();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.jpg");
        std::fs::write(&path, b"synthetic").unwrap();
        worker.stories.directory = Some(directory);
        let control = transfers::Control::new(
            2,
            worker.commands.clone(),
            "status@broadcast".into(),
            key.clone(),
            None,
            None,
            ATTACHMENT_DOWNLOAD_LIMIT,
        );
        let task = tokio::spawn(std::future::pending::<()>());
        worker.stories.downloads.insert(
            key.clone(),
            StatusDownload {
                task,
                control: control.clone(),
                path: path.clone(),
            },
        );
        worker.status_downloaded(key.clone(), 1, Err("synthetic old failure".into()));
        assert!(worker.stories.downloads.contains_key(&key));
        assert!(path.is_file());
        worker.remove_status(&key);
        assert!(control.cancelled());
        assert!(!path.exists());
    }
    fn delivery(id: &str, at: i64) -> (Arc<wa::Message>, MessageInfo) {
        let raw = Arc::new(wa::Message {
            conversation: Some("Synthetic story".into()),
            ..Default::default()
        });
        let info = MessageInfo {
            id: id.into(),
            source: MessageSource {
                chat: "status@broadcast".parse().unwrap(),
                sender: "12345@s.whatsapp.net".parse().unwrap(),
                ..Default::default()
            },
            timestamp: whatsapp_rust::wacore::time::from_secs(at).unwrap(),
            ..Default::default()
        };
        (raw, info)
    }
    #[test]
    fn status_delivery_is_ephemeral_and_does_not_create_a_chat_or_message() {
        let (mut worker, events, _, _) = super::super::receipt_tests::worker();
        let now = crate::util::now();
        let (raw, info) = delivery("story", now);
        worker.ingest(&raw, &info);
        worker.ingest(&raw, &info);
        assert_eq!(worker.stories.rows.len(), 1);
        assert!(worker.archive.chats().unwrap().is_empty());
        assert!(
            worker
                .archive
                .message("status@broadcast", "story")
                .unwrap()
                .is_none()
        );
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, Event::Statuses(rows) if rows.len() == 1))
        );
        let (raw, info) = delivery("expired", now - LIFETIME);
        worker.ingest(&raw, &info);
        assert_eq!(worker.stories.rows.len(), 1);
        worker.stories.rows.values_mut().next().unwrap().0.timestamp = now - LIFETIME;
        worker.emit_statuses();
        assert!(worker.stories.rows.is_empty());
    }
    #[test]
    fn a_locked_sender_is_filtered_from_statuses() {
        let (mut worker, events, _, _) = super::super::receipt_tests::worker();
        let (raw, info) = delivery("private", crate::util::now());
        worker.ingest(&raw, &info);
        let _ = events.try_iter().count();
        worker
            .archive
            .ensure_chat("12345@s.whatsapp.net", "Synthetic sender")
            .unwrap();
        worker
            .archive
            .set_locked("12345@s.whatsapp.net", true)
            .unwrap();
        worker.emit_statuses();
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, Event::Statuses(rows) if rows.is_empty()))
        );
    }
}
