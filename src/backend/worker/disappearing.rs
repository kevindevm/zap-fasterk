//! Supported timer operations and account-local disappearing-message lifecycle.
use super::*;

pub(super) struct HistoryDisappearing {
    pub target: String,
    pub deadline: Option<i64>,
    pub keep: Option<(bool, i64)>,
}

impl Worker {
    pub(super) fn apply_history_disappearing(&self, chat: &str, metadata: &HistoryDisappearing) {
        if let Some(deadline) = metadata.deadline
            && self
                .archive
                .remember_message_expiry(chat, &metadata.target, deadline)
                .is_err()
        {
            self.history_storage_ok.set(false);
            log::warn!("could not retain history disappearing deadline");
        }
        if let Some((kept, at)) = metadata.keep
            && self
                .archive
                .remember_message_keep(chat, &metadata.target, kept, at)
                .is_err()
        {
            self.history_storage_ok.set(false);
            log::warn!("could not retain history keep decision");
        }
    }
    pub(super) fn remember_disappearing_raw(
        &self,
        chat: &str,
        id: &str,
        at: i64,
        message: &wa::Message,
    ) {
        if let Some(deadline) = message
            .get_base_message()
            .get_ephemeral_expiration()
            .filter(|duration| *duration > 0)
            .and_then(|duration| at.checked_add(i64::from(duration)))
            && at > 0
            && self
                .archive
                .remember_message_expiry(chat, id, deadline)
                .is_err()
        {
            log::warn!("could not retain disappearing-message deadline");
        }
    }

    pub(super) fn remember_keep_update(&self, chat: &str, message: &wa::Message) -> bool {
        let Some(keep) = message.get_base_message().keep_in_chat_message.as_option() else {
            return false;
        };
        let Some(target) = keep
            .key
            .as_option()
            .and_then(|key| key.id.as_deref())
            .filter(|id| !id.is_empty())
        else {
            return true;
        };
        if let Some(kept) = keep_value(keep.keep_type)
            && self
                .archive
                .remember_message_keep(
                    chat,
                    target,
                    kept,
                    keep.timestamp_ms.filter(|at| *at > 0).unwrap_or(0),
                )
                .is_err()
        {
            log::warn!("could not retain disappearing-message keep decision");
        }
        true
    }

    pub(super) fn expire_disappearing_messages(&mut self) {
        // Wait for authenticated phone replay; unseen keep decisions must not race initial sync.
        if !self.privacy_ready || self.syncing {
            return;
        }
        let now = whatsapp_rust::wacore::time::now_secs_u64().min(i64::MAX as u64) as i64;
        let due = match self.archive.due_disappearing_messages(now) {
            Ok(due) => due,
            Err(_) => {
                log::warn!("could not inspect disappearing-message deadlines");
                return;
            }
        };
        for (chat, id) in due {
            let paths = match self.archive.disappearing_cached_media(&chat, &id) {
                Ok(paths) => paths,
                Err(_) => {
                    log::warn!("could not inspect expired attachment paths");
                    continue;
                }
            };
            for ((target, message, _), control) in &self.transfer_controls {
                if target == &chat && message == &id {
                    control.cancel();
                }
            }
            if let Some(control) = self.motion_controls.get(&(chat.clone(), id.clone())) {
                control.cancel();
            }
            if self.archive.delete_message(&chat, &id).is_err() {
                log::warn!("could not expire disappearing message");
                continue;
            }
            self.drop_cached_media(&paths);
            self.pending_edits.remove(&(chat.clone(), id.clone()));
            self.emit(Event::MessageDeleted {
                chat: chat.clone(),
                id,
            });
            self.emit_chat(&chat);
        }
    }

    pub(super) fn set_disappearing_timer(&mut self, chat: ChatId, seconds: u32) {
        if !matches!(seconds, 0 | 86400 | 604800 | 7776000) {
            self.emit(Event::Error(
                "Choose Off, 24 hours, 7 days, or 90 days.".into(),
            ));
            return;
        }
        let allowed = self.privacy_ready
            && self.archive.chat(&chat).is_ok_and(|chat| {
                chat.is_some_and(|chat| {
                    !chat.locked
                        && !chat.left
                        && !chat.is_channel()
                        && (chat.kind == ChatKind::Direct || chat.can_edit_info())
                })
            });
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error(
                "Connect to WhatsApp to change disappearing messages.".into(),
            ));
            return;
        };
        if !allowed || !(jid.is_group() || jid.is_pn() || jid.is_lid()) {
            self.emit(Event::Error(
                "Disappearing messages cannot be changed in this chat.".into(),
            ));
            return;
        }
        if !self.disappearing_pending.insert(chat.clone()) {
            self.emit(Event::Error(
                "The disappearing-message change is still pending.".into(),
            ));
            return;
        }
        let generation = self.privacy_generation;
        let requested_at = whatsapp_rust::wacore::time::now_secs_u64().min(i64::MAX as u64) as i64;
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(45), async {
                if jid.is_group() {
                    client
                        .groups()
                        .set_ephemeral(jid, seconds)
                        .await
                        .map_err(|_| {
                            "WhatsApp could not change the disappearing-message timer.".to_owned()
                        })
                } else {
                    client
                        .set_chat_disappearing_timer(jid, seconds)
                        .await
                        .map(|_| ())
                        .map_err(|_| {
                            "WhatsApp could not change the disappearing-message timer.".to_owned()
                        })
                }
            })
            .await;
            let error = match result {
                Ok(result) => result.err(),
                Err(_) => Some(
                    "The timer change is unconfirmed. Check it on your phone before trying again."
                        .into(),
                ),
            };
            let _ = commands.send(Command::DisappearingTimerSet {
                generation,
                chat,
                seconds,
                requested_at,
                error,
            });
        });
    }

    pub(super) fn disappearing_timer_set(
        &mut self,
        generation: u64,
        chat: ChatId,
        seconds: u32,
        requested_at: i64,
        error: Option<String>,
    ) {
        self.disappearing_pending.remove(&chat);
        if generation != self.privacy_generation || !self.privacy_ready {
            return;
        }
        if let Some(error) = error {
            self.emit(Event::Error(error));
            return;
        }
        match self.archive.set_ephemeral(&chat, seconds, requested_at) {
            Ok(true) => self.emit_chat(&chat),
            Ok(false) => {}
            Err(_) => self.emit(Event::Error(
                "WhatsApp changed the timer, but the archive could not save it.".into(),
            )),
        }
    }
}

pub(super) fn keep_value(value: Option<wa::KeepType>) -> Option<bool> {
    match value {
        Some(wa::KeepType::KEEP_FOR_ALL) => Some(true),
        Some(wa::KeepType::UNDO_KEEP_FOR_ALL) => Some(false),
        _ => None,
    }
}

pub(super) fn history_metadata(
    id: &str,
    timestamp: i64,
    info: &wa::WebMessageInfo,
    message: &wa::Message,
) -> Option<HistoryDisappearing> {
    let base = message.get_base_message();
    if let Some(update) = base.keep_in_chat_message.as_option() {
        let target = update
            .key
            .as_option()?
            .id
            .clone()
            .filter(|id| !id.is_empty())?;
        let keep = keep_value(update.keep_type)
            .map(|kept| (kept, update.timestamp_ms.filter(|at| *at > 0).unwrap_or(0)));
        return Some(HistoryDisappearing {
            target,
            deadline: None,
            keep,
        });
    }
    let start = info
        .ephemeral_start_timestamp
        .and_then(|start| i64::try_from(start).ok())
        .unwrap_or(timestamp);
    let duration = info
        .ephemeral_duration
        .or_else(|| base.get_ephemeral_expiration())
        .filter(|seconds| *seconds > 0);
    let deadline = duration
        .and_then(|seconds| start.checked_add(i64::from(seconds)))
        .filter(|_| start > 0);
    let keep = info.keep_in_chat.as_option().and_then(|keep| {
        let at = keep
            .server_timestamp_ms
            .or(keep.client_timestamp_ms)
            .or_else(|| keep.server_timestamp.and_then(|at| at.checked_mul(1000)))
            .filter(|at| *at > 0)
            .unwrap_or(0);
        keep_value(keep.keep_type).map(|kept| (kept, at))
    });
    (deadline.is_some() || keep.is_some()).then(|| HistoryDisappearing {
        target: id.into(),
        deadline,
        keep,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_uses_message_start_and_seconds_with_conservative_keep_baseline() {
        let info = wa::WebMessageInfo {
            ephemeral_start_timestamp: Some(100),
            ephemeral_duration: Some(86400),
            keep_in_chat: Some(wa::KeepInChat {
                keep_type: Some(wa::KeepType::KEEP_FOR_ALL),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let metadata =
            history_metadata("fixture", 999, &info, &wa::Message::text("fixture")).unwrap();
        assert_eq!(metadata.deadline, Some(86500));
        assert_eq!(metadata.keep, Some((true, 0)));
        assert!(
            history_metadata(
                "fixture",
                0,
                &wa::WebMessageInfo::default(),
                &wa::Message::default()
            )
            .is_none()
        );
    }
    #[test]
    fn expiry_waits_for_authenticated_sync_and_emits_only_local_deletion() {
        let (mut worker, events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        let row =
            crate::archive::tests::message("15550000001@s.whatsapp.net", "fixture", 100, false);
        worker.archive.insert_message(&row, None).unwrap();
        worker
            .archive
            .remember_message_expiry(&row.chat, &row.id, 200)
            .unwrap();
        worker.privacy_ready = false;
        worker.syncing = false;
        worker.expire_disappearing_messages();
        assert!(
            worker
                .archive
                .message(&row.chat, &row.id)
                .unwrap()
                .is_some()
        );
        worker.privacy_ready = true;
        worker.syncing = true;
        worker.expire_disappearing_messages();
        assert!(
            worker
                .archive
                .message(&row.chat, &row.id)
                .unwrap()
                .is_some()
        );
        worker.syncing = false;
        worker.expire_disappearing_messages();
        assert!(
            worker
                .archive
                .message(&row.chat, &row.id)
                .unwrap()
                .is_none()
        );
        assert!(worker.archive.message_removed(&row.chat, &row.id).unwrap());
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, Event::MessageDeleted { .. }))
        );
    }
}
