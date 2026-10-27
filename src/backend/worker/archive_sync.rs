//! Archive requests share regular_low serialization with read state and stars.
use super::{Command, Duration, Event, Instant, LinkStatus, Worker};
use whatsapp_rust::Jid;
use whatsapp_rust::wacore_binary::jid::JidExt;

/// How many of a chat's newest messages are searched for one the phone has.
const PHONE_RANGE_LOOKBACK: usize = 20;

/// The range WhatsApp checks before it archives or unarchives a chat on the
/// phone: the chat's newest message. A phone left the chat in place when the
/// change came without one. Local call entries were never sent, so they
/// give their time but name no message, and a message without a time is
/// treated as none.
pub(super) fn archive_range(
    jid: &Jid,
    last: Option<&crate::model::Message>,
) -> whatsapp_rust::SyncActionMessageRange {
    let Some(last) = last.filter(|last| last.timestamp > 0) else {
        return whatsapp_rust::message_range(crate::util::now(), None, Vec::new());
    };
    let participant = (jid.is_group() && !last.from_me)
        .then(|| Worker::jid_of(&last.sender))
        .flatten();
    let key = (!is_local_call_entry(&last.id))
        .then(|| whatsapp_rust::MessageId::new(&last.id).ok())
        .flatten()
        .and_then(|id| {
            whatsapp_rust::MessageRef::new(jid, id, participant.as_ref(), last.from_me).ok()
        });
    whatsapp_rust::message_range(last.timestamp, None, key.map(|key| (key, last.timestamp)))
}

/// Local call entries were never sent, so the phone cannot know them.
fn is_local_call_entry(id: &str) -> bool {
    id.starts_with("local-call:") || id.starts_with("call-log:")
}

impl Worker {
    pub(super) fn request_archive_change(&mut self, chat: String, archived: bool) {
        let chat = self.canonical_str(&chat);
        match self.archive.request_archived(
            &chat,
            archived,
            jiff::Timestamp::now().as_millisecond(),
        ) {
            Ok(()) => {
                self.emit_chat(&chat);
                self.pump_archive_sync();
            }
            Err(_) => self.emit(Event::Error(
                "Could not save the archive change. Try again.".into(),
            )),
        }
    }

    /// The chat's newest message the phone has: a send still pending or
    /// failed here may never have reached it.
    pub(super) fn newest_on_phone(
        &self,
        chat: &str,
    ) -> Result<Option<crate::model::Message>, rusqlite::Error> {
        Ok(self
            .archive
            .messages(chat, None, PHONE_RANGE_LOOKBACK)?
            .into_iter()
            .rev()
            .find(|message| {
                !(message.from_me
                    && matches!(
                        message.status,
                        crate::model::Delivery::Pending | crate::model::Delivery::Failed
                    ))
            }))
    }
    pub(super) fn apply_remote_archive(&mut self, chat: String, mut archived: bool, mut at: i64) {
        let chat = self.canonical_str(&chat);
        if let Some(&(pending, timestamp)) = self.deferred_archive_updates.get(&chat)
            && timestamp > at
        {
            archived = pending;
            at = timestamp;
        }
        self.ensure_chat(&chat, None);
        if self.archive.set_archived_at(&chat, archived, at).is_ok() {
            self.deferred_archive_updates.remove(&chat);
            self.emit_chat(&chat);
            log::debug!("archive sync: remote update persisted");
        } else {
            let pending = self
                .deferred_archive_updates
                .entry(chat)
                .or_insert((archived, at));
            if pending.1 <= at {
                *pending = (archived, at);
            }
            log::warn!("archive sync: remote update could not be persisted; local retry retained");
        }
    }
    pub(super) fn pump_archive_sync(&mut self) {
        let pending = std::mem::take(&mut self.deferred_archive_updates);
        for (chat, (archived, at)) in pending {
            self.apply_remote_archive(chat, archived, at);
        }

        if !matches!(self.status, LinkStatus::Connected) || !self.read_sync.ready(Instant::now()) {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let change = match self.archive.next_archive_change() {
            Ok(change) => change,
            Err(_) => {
                log::warn!("archive sync: could not read pending changes");
                return;
            }
        };
        let Some((chat, archived, at)) = change else {
            return;
        };
        let Some(jid) = Self::jid_of(&chat) else {
            return;
        };
        if !self.read_sync.start_archive(&chat, at, Instant::now()) {
            return;
        }
        // The newest message the phone has travels with the change; a range
        // made up without it could move the phone past messages it has.
        let last = match self.newest_on_phone(&chat) {
            Ok(last) => last,
            Err(error) => {
                log::warn!("could not read a chat to archive it on the phone: {error}");
                None
            }
        };
        let commands = self.commands.clone();
        let generation = self.privacy_generation;
        tokio::spawn(async move {
            let request: std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<(), whatsapp_rust::AppStateError>>
                        + Send,
                >,
            > = Box::pin(async {
                let range = Some(archive_range(&jid, last.as_ref()));
                if archived {
                    client.chat_actions().archive_chat(&jid, range).await
                } else {
                    client.chat_actions().unarchive_chat(&jid, range).await
                }
            });
            let accepted = matches!(
                tokio::time::timeout(Duration::from_secs(30), request).await,
                Ok(Ok(()))
            );
            let _ = commands.send(Command::ArchiveChangeFinished {
                chat,
                at,
                generation,
                accepted,
            });
        });
    }
    pub(super) fn archive_change_finished(
        &mut self,
        chat: String,
        at: i64,
        generation: u64,
        accepted: bool,
    ) {
        if generation != self.privacy_generation || !self.read_sync.archive_matches(&chat, at) {
            return;
        }
        // Keep the transport owner's original key until its slot is released.
        // Only persistence follows a mapping learned while the write waited.
        let canonical = self.canonical_str(&chat);
        let persisted = !accepted || self.archive.finish_archive_change(&canonical, at).is_ok();
        if self
            .read_sync
            .finish_archive(&chat, at, accepted && persisted, Instant::now())
        {
            log::debug!("archive sync: completion accepted={accepted} persisted={persisted}");
            self.pump_read_sync();
            self.pump_star_sync();
            self.pump_archive_sync();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_archive_callback_keeps_its_owner_but_finishes_the_canonical_intent() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let lid = "9@lid";
        let phone = "1@s.whatsapp.net";
        worker.archive.ensure_chat(lid, "Synthetic").unwrap();
        worker.archive.request_archived(lid, true, 100).unwrap();
        assert!(worker.read_sync.start_archive(lid, 100, Instant::now()));
        worker.learn_lid("9", "1");
        assert!(worker.read_sync.archive_matches(lid, 100));
        worker.archive_change_finished(lid.into(), 100, worker.privacy_generation, true);
        assert!(worker.archive.next_archive_change().unwrap().is_none());
        assert!(worker.archive.chat(phone).unwrap().unwrap().archived);
        assert!(worker.read_sync.ready(Instant::now()));
    }

    #[test]
    fn mapped_old_callback_cannot_finish_a_newer_canonical_intent() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let lid = "9@lid";
        let phone = "1@s.whatsapp.net";
        worker.archive.ensure_chat(lid, "Synthetic").unwrap();
        worker.archive.ensure_chat(phone, "Synthetic").unwrap();
        worker.archive.request_archived(lid, true, 100).unwrap();
        assert!(worker.read_sync.start_archive(lid, 100, Instant::now()));
        worker.archive.request_archived(phone, false, 100).unwrap();
        worker
            .deferred_archive_updates
            .insert(lid.into(), (true, 200));
        worker
            .deferred_archive_updates
            .insert(phone.into(), (false, 300));
        worker.learn_lid("9", "1");
        assert_eq!(
            worker.deferred_archive_updates.get(phone),
            Some(&(false, 300))
        );
        assert!(!worker.deferred_archive_updates.contains_key(lid));
        worker.archive_change_finished(lid.into(), 100, worker.privacy_generation, true);
        assert_eq!(
            worker.archive.next_archive_change().unwrap(),
            Some((phone.into(), false, 101))
        );
        assert!(!worker.archive.chat(phone).unwrap().unwrap().archived);
        assert!(worker.read_sync.ready(Instant::now()));
    }

    #[test]
    fn failed_remote_archive_application_retains_the_latest_update_for_local_retry() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let chat = "4915700000002@s.whatsapp.net";
        worker.archive.ensure_chat(chat, "Synthetic").unwrap();
        crate::archive::tests::set_archive_update_failure(&worker.archive, true);
        worker.apply_remote_archive(chat.into(), true, 200);
        worker.apply_remote_archive(chat.into(), false, 100);
        assert_eq!(
            worker.deferred_archive_updates.get(chat),
            Some(&(true, 200))
        );
        crate::archive::tests::set_archive_update_failure(&worker.archive, false);
        worker.pump_archive_sync();
        assert!(worker.archive.chat(chat).unwrap().unwrap().archived);
        assert!(worker.deferred_archive_updates.is_empty());
    }

    #[test]
    fn unrelated_archive_callback_cannot_remove_a_durable_pending_intent() {
        let (mut worker, _, _, _) = super::super::receipt_tests::worker();
        let chat = "4915700000002@s.whatsapp.net";
        worker.archive.ensure_chat(chat, "Synthetic").unwrap();
        worker.archive.request_archived(chat, true, 100).unwrap();
        assert!(worker.read_sync.start_archive(chat, 200, Instant::now()));
        worker.archive_change_finished(chat.into(), 100, worker.privacy_generation, true);
        assert_eq!(
            worker.archive.next_archive_change().unwrap(),
            Some((chat.into(), true, 100))
        );
        assert!(worker.read_sync.archive_matches(chat, 200));
    }
}
