//! Client-side scheduled texts: rows survive restarts, and the worker files
//! each send once its time comes on the maintenance tick. Anything due only
//! goes out while linked; an offline client keeps its rows for the next tick.

use super::Event;

impl super::Worker {
    /// Files every due scheduled text and drops its row, earliest first.
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
                let _ = self.archive.delete_scheduled(item.id);
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
            );
            if let Err(error) = self.archive.delete_scheduled(item.id) {
                log::warn!("scheduled text not dropped: {error}");
            }
            self.emit_scheduled(&item.chat);
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

    pub(super) fn cancel_scheduled(&mut self, id: i64) {
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
}
