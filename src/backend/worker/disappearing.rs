//! A chat's disappearing-message timer: changing it, and the notice a change
//! leaves in the conversation.
//!
//! A direct change is an `EPHEMERAL_SETTING` message. The send's `Ok` only
//! means the stanza left the socket, so the timer is written from the server's
//! ack, through the worker's shared ack map (`AwaitingAck::Timer`). A group
//! change is an IQ, and the server's own group notification carries the
//! change's timestamp and actor; the accepted IQ only refreshes the metadata,
//! so a newer change is never overwritten and no notice is invented.
//!
//! Both paths file one system row, whose id is the change's own, so a
//! redelivery replaces it rather than making a second one.

use super::*;

pub(super) struct HistoryDisappearing {
    pub target: String,
    pub deadline: Option<i64>,
    pub keep: Option<(bool, i64)>,
}

/// How long a sent timer change waits for its server ack before the control
/// is re-enabled and the change is reported as failed.
const TIMER_ACK_PATIENCE: Duration = Duration::from_secs(30);
/// How long after that a late ack is still applied, without a second report.
const TIMER_LATE_ACK_PATIENCE: Duration = Duration::from_secs(5 * 60);
/// How long an ack that outran its `TimerSet` is held, and how many.
const EARLY_ACK_TTL: Duration = Duration::from_secs(60);
const EARLY_ACK_LIMIT: usize = 256;

/// A direct timer change whose sent message is waiting for the server's ack.
#[derive(Clone)]
pub(super) struct TimerAck {
    chat: ChatId,
    seconds: u32,
    /// When the send built the setting, the clock the archive compares against
    /// the phone's settings. The ack's own time is a different clock and
    /// ordering by it could overwrite a newer change made on the phone.
    sent_at: i64,
    /// When to report the change as unanswered.
    deadline: Instant,
    /// The deadline was already reported: the control is enabled again, and an
    /// ack that arrives later still applies the timer without a second report.
    reported: bool,
}

/// A server ack that arrived before its `TimerSet`, held until the send names
/// the id it belongs to. Exact id matching keeps an ordinary message's ack
/// from being mistaken for a timer change.
pub(super) struct EarlyAck {
    pub(super) error: Option<String>,
    at: Instant,
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

    /// Changes a chat's disappearing-message timer using the direct or group
    /// WhatsApp API. A direct change is written only once the server acks it;
    /// a group's is written from the server's group notification.
    pub(super) fn set_disappearing_timer(&mut self, chat: ChatId, seconds: u32) {
        if !matches!(seconds, 0 | 86400 | 604800 | 7776000) {
            self.emit(Event::Error(
                "Choose Off, 24 hours, 7 days, or 90 days.".into(),
            ));
            return;
        }
        let is_group = ChatKind::from_id(&chat) == ChatKind::Group;
        // A member of a locked group gets no menu, and one that asks anyway
        // gets nothing sent: WhatsApp would refuse the change.
        if is_group
            && !self
                .archive
                .chat(&chat)
                .ok()
                .flatten()
                .is_some_and(|row| row.can_edit_info())
        {
            self.emit(Event::Error(
                "Only admins can change disappearing messages.".to_owned(),
            ));
            return;
        }
        // A send on a dropped socket would fail as "client is not connected";
        // say to reconnect instead, as the other send paths do.
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error(
                "Connect to WhatsApp to change disappearing messages.".to_owned(),
            ));
            return;
        };
        if !(jid.is_group() || jid.is_pn() || jid.is_lid()) {
            self.emit(Event::Error(
                "Disappearing messages cannot be changed in this chat.".to_owned(),
            ));
            return;
        }
        self.timer_saving(&chat, true);
        let session = self.privacy_generation;
        let commands = self.commands.clone();
        let waker = self.waker.clone();
        tokio::spawn(async move {
            if is_group {
                let result = client
                    .groups()
                    .set_ephemeral(jid, seconds)
                    .await
                    .map_err(|error| error.to_string());
                let _ = commands.send(Command::GroupEdited {
                    chat,
                    edit: GroupEdit::Timer(seconds),
                    session,
                    result,
                });
            } else {
                // The timer's own setting timestamp versions it; the library
                // embeds it in the message it sent, so read it from there
                // rather than from any other clock.
                let result = client
                    .set_chat_disappearing_timer(jid, seconds)
                    .await
                    .map(|sent| {
                        let setting = sent
                            .message
                            .get_base_message()
                            .protocol_message
                            .as_option()
                            .and_then(|protocol| protocol.ephemeral_setting_timestamp);
                        (
                            sent.message_id.to_string(),
                            setting.unwrap_or_else(crate::util::now),
                        )
                    })
                    .map_err(|error| error.to_string());
                let _ = commands.send(Command::TimerSet {
                    chat,
                    session,
                    seconds,
                    result,
                });
            }
            waker.wake();
        });
    }

    /// Tells the interface whether a timer change for `chat` is in flight. A
    /// privacy-id mapping learned mid-flight can change the chat's id, so both
    /// the stored id and the canonical one hear about it.
    pub(super) fn timer_saving(&self, chat: &ChatId, saving: bool) {
        self.emit(Event::TimerSaving {
            chat: chat.clone(),
            saving,
        });
        let canonical = self.canonical_str(chat);
        if canonical != *chat {
            self.emit(Event::TimerSaving {
                chat: canonical,
                saving,
            });
        }
    }

    /// Takes a direct timer change off the wire. `Ok` carries the sent
    /// message's id and setting time: the change is written only when the
    /// server acks it, so the control stays disabled until then. A send that
    /// never left the socket reports and re-enables it.
    pub(super) fn timer_set(
        &mut self,
        chat: ChatId,
        session: u64,
        seconds: u32,
        result: Result<(String, i64), String>,
    ) {
        if session != self.privacy_generation {
            return;
        }
        match result {
            Ok((id, sent_at)) => {
                self.awaiting_ack.insert(
                    id.clone(),
                    AwaitingAck::Timer(TimerAck {
                        chat,
                        seconds,
                        sent_at,
                        deadline: Instant::now() + TIMER_ACK_PATIENCE,
                        reported: false,
                    }),
                );
                // An ack that arrived before this completion waited by id.
                if let Some(early) = self.take_early_ack(&id) {
                    self.resolve_timer_ack(&id, early.error.as_deref());
                }
            }
            Err(error) => {
                self.timer_saving(&chat, false);
                log::warn!("could not change disappearing messages ({error})");
                self.emit(Event::Error(
                    "Could not change disappearing messages.".to_owned(),
                ));
            }
        }
    }

    /// Applies the server's ack of a direct timer change, or reports its nack.
    /// Only an entry this map holds is settled, so an ordinary message's ack
    /// can never be taken for a timer change.
    pub(super) fn settle_timer_ack(&mut self, ack: &wa_events::ServerAck) {
        self.resolve_timer_ack(&ack.id, ack.error.as_deref());
    }

    /// Writes the change an ack accepted. A privacy-id mapping learned since
    /// the send may have merged the chat, so the destination is resolved
    /// again here.
    fn resolve_timer_ack(&mut self, id: &str, error: Option<&str>) {
        let Some(AwaitingAck::Timer(pending)) = self.awaiting_ack.remove(id) else {
            return;
        };
        // A retry started after this change timed out may still be waiting for
        // its own answer, and its answer or deadline re-enables the control.
        // Clearing it here would let a third change start over the retry.
        let chat = self.canonical_str(&pending.chat);
        let newer_waiting = self.awaiting_ack.values().any(|awaiting| match awaiting {
            AwaitingAck::Timer(other) => !other.reported && self.canonical_str(&other.chat) == chat,
            AwaitingAck::Message(_) => false,
        });
        // A timed-out attempt already cleared saving at its deadline. A retry
        // may now be sending but not yet registered in awaiting_ack.
        if !pending.reported && !newer_waiting {
            self.timer_saving(&pending.chat, false);
        }
        if let Some(error) = error {
            if !pending.reported {
                log::warn!("WhatsApp refused a disappearing-messages change ({error})");
                self.emit(Event::Error(
                    "Could not change disappearing messages.".to_owned(),
                ));
            }
            return;
        }
        if self
            .archive
            .set_ephemeral(&chat, pending.seconds, pending.sent_at)
            .unwrap_or(false)
        {
            self.file_timer_notice(
                &chat,
                &format!("{}{}", crate::model::TIMER_NOTICE_PREFIX, id),
                self.me(),
                true,
                pending.sent_at,
                pending.seconds,
                None,
            );
            self.emit_chat(&chat);
        }
    }

    /// A connection stop drops its pending acks, so no deadline remains to
    /// release their controls. Notify both aliases before discarding them.
    pub(super) fn clear_timer_saving(&self) {
        for awaiting in self.awaiting_ack.values() {
            if let AwaitingAck::Timer(timer) = awaiting {
                self.timer_saving(&timer.chat, false);
            }
        }
    }

    /// Reports timer changes the server never acked, re-enabling the control.
    /// The entry is kept a while longer, so an ack that arrives late still
    /// applies the timer it accepted without a second report; only then is the
    /// entry dropped. Held acks age out here too, so their bound holds without
    /// traffic.
    pub(super) fn expire_timer_acks(&mut self) {
        let now = Instant::now();
        self.early_acks
            .retain(|_, held| now.duration_since(held.at) < EARLY_ACK_TTL);
        let due: Vec<(String, ChatId, bool)> = self
            .awaiting_ack
            .iter()
            .filter_map(|(id, awaiting)| match awaiting {
                AwaitingAck::Timer(pending) if pending.deadline <= now => {
                    Some((id.clone(), pending.chat.clone(), pending.reported))
                }
                _ => None,
            })
            .collect();
        for (id, chat, reported) in due {
            if reported {
                self.awaiting_ack.remove(&id);
                continue;
            }
            if let Some(AwaitingAck::Timer(pending)) = self.awaiting_ack.get_mut(&id) {
                pending.reported = true;
                pending.deadline = now + TIMER_LATE_ACK_PATIENCE;
            }
            self.timer_saving(&chat, false);
            self.emit(Event::Error(
                "Could not change disappearing messages.".to_owned(),
            ));
        }
    }

    /// Holds a message ack that matches nothing yet, bounded in age and count,
    /// so an ack that outran its `TimerSet` still lands.
    pub(super) fn hold_early_ack(&mut self, ack: &wa_events::ServerAck) {
        let now = Instant::now();
        self.early_acks
            .retain(|_, held| now.duration_since(held.at) < EARLY_ACK_TTL);
        if self.early_acks.len() >= EARLY_ACK_LIMIT
            && let Some(oldest) = self
                .early_acks
                .iter()
                .min_by_key(|(_, held)| held.at)
                .map(|(id, _)| id.clone())
        {
            self.early_acks.remove(&oldest);
        }
        self.early_acks.insert(
            ack.id.clone(),
            EarlyAck {
                error: ack.error.clone(),
                at: now,
            },
        );
    }

    /// Takes a held ack for `id`, unless it sat past its age bound.
    fn take_early_ack(&mut self, id: &str) -> Option<EarlyAck> {
        let held = self.early_acks.remove(id)?;
        (Instant::now().duration_since(held.at) < EARLY_ACK_TTL).then_some(held)
    }

    /// Files the system row for a timer change the archive accepted. The row
    /// shares the change's stable id, so a redelivery replaces it rather than
    /// making a second notice.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn file_timer_notice(
        &mut self,
        chat: &str,
        id: &str,
        sender: String,
        from_me: bool,
        timestamp: i64,
        seconds: u32,
        push_name: Option<&str>,
    ) {
        // A notice removed by clearing or deleting the chat must stay gone,
        // even if the same change is delivered again.
        if self.predates_removal(chat, timestamp) {
            return;
        }
        let notice = Message {
            id: id.to_owned(),
            chat: chat.to_owned(),
            sender,
            sender_name: if from_me {
                None
            } else {
                push_name.map(str::to_owned)
            },
            from_me,
            timestamp,
            content: Content::DisappearingTimer { seconds },
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
            history_order: None,
        };
        if let Err(error) = self.archive.insert_message(&notice, None) {
            log::warn!("could not file a disappearing-messages notice: {error}");
            return;
        }
        self.emit(Event::Messages {
            chat: chat.to_owned(),
            messages: vec![notice],
            older: false,
            complete: false,
        });
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
    use super::super::receipt_tests::{PEER, worker};
    use super::*;

    /// A direct timer change is written only once the server acks the sent
    /// message. A send that never left the socket, a nack, and silence all
    /// leave the old timer and report.
    #[tokio::test]
    async fn a_direct_timer_change_lands_only_when_the_server_acks_it() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER, None);
        worker.archive.set_ephemeral(PEER, 86_400, 100).unwrap();
        let timer = |worker: &Worker| {
            worker
                .archive
                .chat(PEER)
                .unwrap()
                .unwrap()
                .disappearing_timer()
        };
        let ack = |id: &str, at: i64| {
            wa_events::ServerAck::builder()
                .id(id.to_owned())
                .class("message".to_owned())
                .timestamp(whatsapp_rust::wacore::time::from_secs(at).unwrap())
                .build()
        };

        // A send that reached the socket is not yet an accepted change.
        let session = worker.privacy_generation;
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session,
                seconds: 604_800,
                result: Ok(("sent-1".into(), 100)),
            })
            .await;
        assert_eq!(
            timer(&worker),
            Some(86_400),
            "nothing is written before ack"
        );

        // The server's ack applies it; the send's own time versions it.
        worker
            .handle_wa_event(Arc::new(wa_events::Event::ServerAck(ack("sent-1", 150))))
            .await;
        assert_eq!(timer(&worker), Some(604_800));

        // A newer phone change survives a late ack from before it.
        worker.archive.set_ephemeral(PEER, 0, 200).unwrap();
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session,
                seconds: 604_800,
                result: Ok(("sent-2".into(), 100)),
            })
            .await;
        worker
            .handle_wa_event(Arc::new(wa_events::Event::ServerAck(ack("sent-2", 250))))
            .await;
        assert_eq!(timer(&worker), None, "the phone's newer change wins");

        // A nack, and a send that never left the socket, change nothing.
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session,
                seconds: 86_400,
                result: Ok(("sent-3".into(), 100)),
            })
            .await;
        let nack = wa_events::ServerAck::builder()
            .id("sent-3".to_owned())
            .class("message".to_owned())
            .error("479".to_owned())
            .build();
        worker
            .handle_wa_event(Arc::new(wa_events::Event::ServerAck(nack)))
            .await;
        assert_eq!(timer(&worker), None);
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session,
                seconds: 86_400,
                result: Err("boom".into()),
            })
            .await;
        assert_eq!(timer(&worker), None);

        let errors: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::Error(message) => Some(message),
                _ => None,
            })
            .collect();
        assert!(
            errors.contains(&"Could not change disappearing messages.".to_owned()),
            "{errors:?}"
        );
    }

    /// An ack can outrun the send's own completion. It is held by id, then
    /// applied when the completion names it.
    #[tokio::test]
    async fn an_ack_that_outruns_the_send_completion_still_applies() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER, None);
        worker.archive.set_ephemeral(PEER, 86_400, 100).unwrap();
        let session = worker.privacy_generation;
        let early = wa_events::ServerAck::builder()
            .id("sent-early".to_owned())
            .class("message".to_owned())
            .timestamp(whatsapp_rust::wacore::time::from_secs(150).unwrap())
            .build();
        worker
            .handle_wa_event(Arc::new(wa_events::Event::ServerAck(early)))
            .await;
        assert_eq!(
            worker
                .archive
                .chat(PEER)
                .unwrap()
                .unwrap()
                .disappearing_timer(),
            Some(86_400),
            "an ack with no id matching yet is held, not applied"
        );
        // The completion names the id, so the held ack resolves it now.
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session,
                seconds: 604_800,
                result: Ok(("sent-early".into(), 100)),
            })
            .await;
        assert_eq!(
            worker
                .archive
                .chat(PEER)
                .unwrap()
                .unwrap()
                .disappearing_timer(),
            Some(604_800)
        );
        assert!(worker.awaiting_ack.is_empty());
    }

    /// A completion from a departed session is ignored rather than written to
    /// the account that replaced it.
    #[tokio::test]
    async fn a_timer_completion_from_an_old_session_is_ignored() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER, None);
        worker.archive.set_ephemeral(PEER, 86_400, 100).unwrap();
        let old = worker.privacy_generation;
        worker.privacy_generation = old.wrapping_add(1);
        // Only the account session check can reject the completion; without
        // it the sent id would be registered under the new account, and its
        // ack would write the old account's change to the new one.
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session: old,
                seconds: 604_800,
                result: Ok(("sent-old".into(), 100)),
            })
            .await;
        assert!(
            worker.awaiting_ack.is_empty(),
            "a stale completion must not register with the new account"
        );
        assert!(
            events.try_iter().next().is_none(),
            "a stale completion reports nothing"
        );
        assert_eq!(
            worker
                .archive
                .chat(PEER)
                .unwrap()
                .unwrap()
                .disappearing_timer(),
            Some(86_400)
        );
    }

    /// A group completion from a departed session is discarded: it clears no
    /// saving state and asks for no metadata.
    #[tokio::test]
    async fn a_group_completion_from_a_departed_session_is_discarded() {
        let (mut worker, events, _inbox, _wa) = worker();
        let chat = "fixture@g.us";
        worker.archive.ensure_chat(chat, "Weekend plans").unwrap();
        let old = worker.privacy_generation;
        worker.privacy_generation = old.wrapping_add(1);
        worker
            .handle_command(Command::GroupEdited {
                chat: chat.into(),
                edit: GroupEdit::Timer(604_800),
                session: old,
                result: Ok(()),
            })
            .await;
        assert!(
            events.try_iter().next().is_none(),
            "a stale group completion changes nothing"
        );
        assert!(
            worker.group_info_queue.is_empty(),
            "a stale group completion asks for no metadata"
        );
        // The current session's completion still lands.
        worker
            .handle_command(Command::GroupEdited {
                chat: chat.into(),
                edit: GroupEdit::Timer(604_800),
                session: worker.privacy_generation,
                result: Ok(()),
            })
            .await;
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, Event::TimerSaving { saving: false, .. })),
            "the current session's completion clears its saving state"
        );
    }

    /// A group timer notification files one notice naming the actor, and a
    /// refused group change relearns the rights while saying what it was.
    #[tokio::test]
    async fn a_group_timer_notification_files_a_notice_and_a_refusal_names_the_timer() {
        use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction;
        let (mut worker, events, _inbox, _wa) = worker();
        let group = "123-456@g.us";
        let update = wa_events::GroupUpdate::builder()
            .group_jid(group.parse().unwrap())
            .notification_id("notice-1".to_owned())
            .participant("111@s.whatsapp.net".parse().unwrap())
            .timestamp(whatsapp_rust::wacore::time::from_secs(200).unwrap())
            .is_lid_addressing_mode(false)
            .action(Box::new(GroupNotificationAction::Ephemeral {
                expiration: 86_400,
                trigger: None,
            }))
            .build();
        worker
            .handle_wa_event(Arc::new(wa_events::Event::GroupUpdate(update)))
            .await;
        let filed = worker
            .archive
            .message(
                group,
                &format!("{}notice-1-0", crate::model::TIMER_NOTICE_PREFIX),
            )
            .unwrap()
            .expect("a notice");
        assert_eq!(
            filed.content,
            Content::DisappearingTimer { seconds: 86_400 }
        );
        assert_eq!(filed.sender, "111@s.whatsapp.net");
        assert!(!filed.from_me);

        worker
            .archive
            .ensure_chat("fixture@g.us", "Weekend plans")
            .unwrap();
        worker
            .handle_command(Command::GroupEdited {
                chat: "fixture@g.us".into(),
                edit: GroupEdit::Timer(604_800),
                session: worker.privacy_generation,
                result: Err("received a server error response: code=403, text='forbidden'".into()),
            })
            .await;
        let said: Vec<_> = events.try_iter().collect();
        assert!(
            said.iter().any(|event| matches!(
                event,
                Event::Error(message) if message == "Only admins can change disappearing messages."
            )),
            "a refused timer names the timer, not the group name: {said:?}"
        );
    }

    /// A change and the ack it still waits for belong to the account that sent
    /// them, so logging out forgets both. The interface drops the control's
    /// saving state with the rest of that account's state.
    #[tokio::test]
    async fn logout_forgets_a_waiting_timer_change() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER, None);
        let session = worker.privacy_generation;
        worker
            .handle_command(Command::TimerSet {
                chat: PEER.into(),
                session,
                seconds: 604_800,
                result: Ok(("waiting-1".into(), 100)),
            })
            .await;
        assert_eq!(worker.awaiting_ack.len(), 1);
        worker.hold_early_ack(
            &wa_events::ServerAck::builder()
                .id("held".to_owned())
                .class("message".to_owned())
                .build(),
        );
        worker.on_logged_out().await;
        assert!(worker.awaiting_ack.is_empty());
        assert!(worker.early_acks.is_empty());
    }

    /// An accepted direct timer change files one centred system row; a stale
    /// change files none, and a redelivery replaces rather than duplicates it.
    #[test]
    fn a_protocol_timer_change_files_one_notice() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER, None);
        worker.archive.set_ephemeral(PEER, 604_800, 100).unwrap();
        let setting = |expiration: u32, setting_time: Option<i64>, envelope: i64, id: &str| {
            let raw = wa::Message {
                protocol_message: MessageField::some(wa::message::ProtocolMessage {
                    r#type: Some(wa::message::protocol_message::Type::EPHEMERAL_SETTING),
                    ephemeral_expiration: Some(expiration),
                    ephemeral_setting_timestamp: setting_time,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let info = MessageInfo {
                id: id.into(),
                source: MessageSource {
                    chat: PEER.parse().unwrap(),
                    sender: PEER.parse().unwrap(),
                    ..Default::default()
                },
                timestamp: whatsapp_rust::wacore::time::from_secs(envelope).unwrap(),
                ..Default::default()
            };
            (Arc::new(raw), info)
        };
        let notice = |id: &str| format!("{}{}", crate::model::TIMER_NOTICE_PREFIX, id);

        let (raw, info) = setting(86_400, Some(200), 200, "change-1");
        worker.ingest(&raw, &info);
        let filed = worker
            .archive
            .message(PEER, &notice("change-1"))
            .unwrap()
            .expect("an accepted change leaves a notice");
        assert_eq!(
            filed.content,
            Content::DisappearingTimer { seconds: 86_400 }
        );

        // A stale change is refused by the archive, so it leaves no notice.
        let (raw, info) = setting(7_776_000, Some(150), 150, "stale");
        worker.ingest(&raw, &info);
        assert!(
            worker
                .archive
                .message(PEER, &notice("stale"))
                .unwrap()
                .is_none(),
            "a stale change leaves no notice"
        );

        // The same change redelivered replaces its row rather than adding one.
        let (raw, info) = setting(86_400, Some(200), 200, "change-1");
        worker.ingest(&raw, &info);
        let notices = worker
            .archive
            .messages(PEER, None, 100)
            .unwrap()
            .iter()
            .filter(|message| matches!(message.content, Content::DisappearingTimer { .. }))
            .count();
        assert_eq!(notices, 1, "one change, one notice");

        // A change the reader cleared away stays gone when it is delivered
        // again: the removal barrier outlives the notice's own id.
        worker
            .archive
            .remove_chat_through(PEER, 300, false)
            .unwrap();
        worker.ingest(&raw, &info);
        assert!(
            worker
                .archive
                .message(PEER, &notice("change-1"))
                .unwrap()
                .is_none(),
            "a cleared notice stays gone"
        );
    }
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
