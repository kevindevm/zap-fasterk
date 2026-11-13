//! Client-side scheduled texts: rows survive restarts, and the worker files
//! each send once its time comes on the maintenance tick. Anything due only
//! goes out while linked; an offline client keeps its rows for the next tick.

use super::Event;

impl super::Worker {
    /// Files every due scheduled message and drops its row, earliest first.
    /// Offline rows wait: refusing them would hand text back to a composer
    /// the reader never opened.
    pub(super) fn pump_scheduled(&mut self) {
        if self.client.is_none() {
            return;
        }
        let now = crate::util::now();
        let due = match self.archive.due_scheduled(now) {
            Ok(due) => due,
            Err(error) => {
                log::warn!("scheduled texts not read: {error}");
                return;
            }
        };
        for item in due {
            if self.client.is_none() {
                return;
            }
            // A chat deleted after scheduling drops its rows without sending.
            if self.archive.chat(&item.chat).ok().flatten().is_none() {
                self.drop_scheduled(item.id, item.media_path.as_deref());
                continue;
            }
            if item.media_path.is_some() {
                self.pump_scheduled_photo(item);
                continue;
            }
            // A quote answered or deleted meanwhile sends as plain text.
            let quoting = item.quoted.as_deref().filter(|id| {
                self.archive
                    .message(&item.chat, id)
                    .ok()
                    .flatten()
                    .is_some()
            });
            self.send_text(
                item.chat.clone(),
                item.text.clone(),
                quoting.map(crate::model::ReplyTarget::from),
                item.mentions.clone(),
                None,
            );
            if let Err(error) = self.archive.delete_scheduled(item.id) {
                log::warn!("scheduled text not dropped: {error}");
            }
            self.emit_scheduled(&item.chat);
        }
    }

    /// Directory holding staged photo copies waiting for their send time.
    fn scheduled_media_dir(&self) -> std::path::PathBuf {
        self.dirs.state.join("scheduled_media")
    }

    /// Files one due scheduled photo, or its caption as plain text when the
    /// staged copy is gone, so nothing scheduled is ever dropped silently.
    /// The row and the copy go away either way; a failed upload retries
    /// through the normal transfer bubble.
    fn pump_scheduled_photo(&mut self, item: crate::archive::ScheduledMessage) {
        let quoting = item.quoted.as_deref().filter(|id| {
            self.archive
                .message(&item.chat, id)
                .ok()
                .flatten()
                .is_some()
        });
        let caption = (!item.text.trim().is_empty()).then(|| item.text.clone());
        let path = item
            .media_path
            .as_deref()
            .map(|name| self.scheduled_media_dir().join(name));
        let filed = path
            .filter(|path| path.is_file())
            .map(|path| {
                self.send_files(
                    item.chat.clone(),
                    vec![path],
                    false,
                    caption,
                    item.mentions.clone(),
                    quoting.map(crate::model::ReplyTarget::from),
                );
                true
            })
            .unwrap_or(false);
        if !filed {
            log::warn!("scheduled photo copy is gone; sending its caption");
            self.send_text(
                item.chat.clone(),
                item.text.clone(),
                quoting.map(crate::model::ReplyTarget::from),
                item.mentions.clone(),
                None,
            );
        }
        self.drop_scheduled(item.id, item.media_path.as_deref());
        self.emit_scheduled(&item.chat);
    }

    /// Drops a scheduled row and its staged photo copy, if any.
    fn drop_scheduled(&self, id: i64, media: Option<&str>) {
        if let Some(name) = media {
            let _ = std::fs::remove_file(self.scheduled_media_dir().join(name));
        }
        if let Err(error) = self.archive.delete_scheduled(id) {
            log::warn!("scheduled message not dropped: {error}");
        }
    }

    pub(super) fn emit_scheduled(&self, chat: &str) {
        match self.archive.scheduled_messages(chat) {
            Ok(items) => self.emit(Event::Scheduled {
                chat: chat.to_owned(),
                items,
            }),
            Err(error) => log::warn!("scheduled texts not listed: {error}"),
        }
    }

    pub(super) fn schedule_text(
        &mut self,
        chat: String,
        text: String,
        quoted: Option<String>,
        mentions: Vec<String>,
        due_at: i64,
    ) {
        let created_at = crate::util::now();
        match self.archive.schedule_message(
            &chat,
            &text,
            quoted.as_deref(),
            &mentions,
            due_at,
            created_at,
        ) {
            Ok(_) => {
                self.emit_scheduled(&chat);
                self.pump_scheduled();
            }
            Err(error) => self.emit(Event::Error(format!("scheduled text not stored: {error}"))),
        }
    }

    pub(super) fn schedule_photo(&mut self, photo: crate::backend::ScheduledPhoto) {
        use crate::backend::ScheduledPhoto;
        let ScheduledPhoto {
            chat,
            text,
            quoted,
            mentions,
            due_at,
            file_name,
            bytes,
        } = photo;
        if bytes.len() as u64 > crate::model::MANUAL_DOWNLOAD_LIMIT {
            self.emit(Event::Error("scheduled photo is too large".into()));
            return;
        }
        let created_at = crate::util::now();
        let id = match self.archive.schedule_message(
            &chat,
            &text,
            quoted.as_deref(),
            &mentions,
            due_at,
            created_at,
        ) {
            Ok(id) => id,
            Err(error) => {
                self.emit(Event::Error(format!("scheduled photo not stored: {error}")));
                return;
            }
        };
        let mut extension: String = std::path::Path::new(&file_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        if extension.is_empty() {
            extension.push_str("bin");
        }
        let name = format!("{id}.{extension}");
        if std::fs::create_dir_all(self.scheduled_media_dir()).is_err()
            || std::fs::write(self.scheduled_media_dir().join(&name), &bytes).is_err()
        {
            let _ = self.archive.delete_scheduled(id);
            self.emit(Event::Error("scheduled photo not stored".into()));
            return;
        }
        if let Err(error) = self.archive.set_scheduled_media(id, &name) {
            let _ = self.archive.delete_scheduled(id);
            let _ = std::fs::remove_file(self.scheduled_media_dir().join(&name));
            self.emit(Event::Error(format!("scheduled photo not stored: {error}")));
            return;
        }
        self.emit_scheduled(&chat);
        self.pump_scheduled();
    }

    pub(super) fn cancel_scheduled(&mut self, id: i64) {
        if let Ok(media) = self.archive.scheduled_media(id)
            && let Some(name) = media
        {
            let _ = std::fs::remove_file(self.scheduled_media_dir().join(name));
        }
        match self.archive.cancel_scheduled(id) {
            Ok(true) => self.emit(Event::ScheduledCancelled { id }),
            Ok(false) => {}
            Err(error) => self.emit(Event::Error(format!("scheduled text not dropped: {error}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::receipt_tests::{PEER, worker};
    use super::super::{Event, Worker};
    use crate::backend::Command;

    async fn attach_offline_client(worker: &mut Worker) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let store = whatsapp_rust::store::SqliteStore::open(
            directory.path().join("fixture.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        let bot = super::super::Bot::builder()
            .with_backend(store)
            .build()
            .await
            .unwrap();
        // Never run or connect this bot: its client only proves linkage.
        worker.client = Some(bot.client());
        directory
    }

    fn scheduled(events: &std::sync::mpsc::Receiver<Event>, chat: &str) -> Vec<Event> {
        events
            .try_iter()
            .filter(|event| matches!(event, Event::Scheduled { chat: known, .. } if known == chat))
            .collect()
    }

    #[tokio::test]
    async fn due_texts_wait_offline_and_file_once_online() {
        let (mut worker, events, _, _) = worker();
        worker
            .archive
            .upsert_chat(&crate::model::Chat::new(PEER.into(), "Peer".into()))
            .unwrap();
        let past = crate::util::now() - 10;
        worker
            .handle_command(Command::ScheduleText {
                chat: PEER.into(),
                text: "later".into(),
                quoted: None,
                mentions: Vec::new(),
                due_at: past,
            })
            .await;
        assert_eq!(scheduled(&events, PEER).len(), 1);
        // Offline the row waits instead of refusing into a composer.
        worker.pump_scheduled();
        assert_eq!(worker.archive.scheduled_messages(PEER).unwrap().len(), 1);
        let _directory = attach_offline_client(&mut worker).await;
        worker.pump_scheduled();
        assert!(worker.archive.scheduled_messages(PEER).unwrap().is_empty());
        worker
            .handle_command(Command::LoadChat {
                chat: PEER.into(),
                before: None,
            })
            .await;
        let filed: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::Messages { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            filed.iter().any(|message| matches!(
                &message.content,
                crate::model::Content::Text { text, .. } if text == "later"
            )),
            "the due text is filed as a message"
        );
    }

    #[tokio::test]
    async fn cancel_drops_a_pending_text() {
        let (mut worker, events, _, _) = worker();
        worker
            .handle_command(Command::ScheduleText {
                chat: PEER.into(),
                text: "never".into(),
                quoted: None,
                mentions: Vec::new(),
                due_at: crate::util::now() + 3_600,
            })
            .await;
        let listed = scheduled(&events, PEER);
        assert_eq!(listed.len(), 1);
        let id = match &listed[0] {
            Event::Scheduled { items, .. } => items[0].id,
            _ => unreachable!(),
        };
        worker.handle_command(Command::CancelScheduled { id }).await;
        assert!(
            events.try_iter().any(
                |event| matches!(event, Event::ScheduledCancelled { id: known } if known == id)
            )
        );
        assert!(worker.archive.scheduled_messages(PEER).unwrap().is_empty());
    }

    #[tokio::test]
    async fn scheduled_photos_wait_offline_and_cancel_removes_the_copy() {
        let (mut worker, events, _, _) = worker();
        worker
            .handle_command(Command::SchedulePhoto(crate::backend::ScheduledPhoto {
                chat: PEER.into(),
                text: "look".into(),
                quoted: None,
                mentions: Vec::new(),
                due_at: crate::util::now() + 3_600,
                file_name: "photo.JPG".into(),
                bytes: vec![1, 2, 3],
            }))
            .await;
        let listed = scheduled(&events, PEER);
        assert_eq!(listed.len(), 1);
        let (id, media) = match &listed[0] {
            Event::Scheduled { items, .. } => (items[0].id, items[0].media_path.clone()),
            _ => unreachable!(),
        };
        let name = media.expect("the staged copy is recorded");
        assert!(name.ends_with(".jpg"), "extensions normalize: {name}");
        let path = worker.scheduled_media_dir().join(&name);
        assert!(path.is_file());
        // Offline the row waits instead of refusing into a composer.
        worker.pump_scheduled();
        assert_eq!(worker.archive.scheduled_messages(PEER).unwrap().len(), 1);
        worker.handle_command(Command::CancelScheduled { id }).await;
        assert!(!path.exists(), "cancelling removes the staged copy");
        assert!(worker.archive.scheduled_messages(PEER).unwrap().is_empty());
    }
}
