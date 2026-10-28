// Adapted from crmne/zapfast PR #220 (alitura1), with account-local ownership.
//! The worker's side of a 1:1 call: commands, signaling routing, and media events.
//!
//! One call at a time. [`Worker::call`] holds the only one, and every entry point here refuses to
//! start or accept a second while it exists, so a double click, a second chat's call button, and a
//! second inbound offer all land on the same refusal.
//!
//! The state the UI renders is never computed here: it comes from the backend. `<accept>` moves an
//! outgoing call out of dialing, the media plane's `RelayAllocated` is what makes a call Active and
//! starts the timer, and a `<reject>`/`<terminate>`/relay loss ends it with the reason the peer
//! gave. Nothing is inferred from the button that was pressed.

use super::*;
use crate::calls::{self, Call, CallUpdate};
use whatsapp_rust::types::call::IncomingCall;
use whatsapp_rust::voip::CallEvent;

/// The call this worker owns: its state, and the channels the tasks around it feed.
///
/// Dropped the moment the call reaches a terminal phase, which is what releases the media tasks and
/// the microphone and speaker streams.
pub(super) struct CallRuntime {
    pub(super) call: Call,
    /// Media-plane events, and the signal that the media task finished.
    pub(super) events: async_channel::Receiver<CallRuntimeEvent>,
    /// Whether the media-plane watcher was started for this call's handle.
    pub(super) watching: bool,
    created_at: i64,
    /// The snapshot the UI was last handed, so the periodic check publishes only real changes.
    last: CallUpdate,
    ringback: Option<crate::call_audio::Ringback>,
    video_used: bool,
}

/// What a call's background tasks report back to the worker loop.
pub(super) enum CallRuntimeEvent {
    /// One event off the call's media plane.
    Media(CallEvent),
    /// The media task is gone: relay disconnect, send failure, or hangup.
    Ended,
}

pub(super) enum PreparationSignal {
    Action(whatsapp_rust::types::call::CallAction),
    Resolved(String),
}

fn ringing_tone(
    direction: crate::model::CallDirection,
    phase: calls::CallPhase,
    preparing: bool,
) -> Option<crate::call_audio::CallTone> {
    use crate::call_audio::CallTone;
    use crate::model::CallDirection;
    if preparing {
        return None;
    }
    match (direction, phase) {
        (CallDirection::Incoming, calls::CallPhase::Incoming) => Some(CallTone::Incoming),
        (CallDirection::Outgoing, calls::CallPhase::Ringing) => Some(CallTone::Outgoing),
        _ => None,
    }
}

impl CallRuntime {
    /// A runtime whose media plane is not watched yet: a call that is still ringing has no handle
    /// to watch, and gets one the moment it is placed or answered.
    pub(super) fn new(call: Call) -> Self {
        let (_tx, events) = async_channel::bounded(64);
        let last = call.update();
        let video_used = last.video_requested || last.video_active;
        Self {
            call,
            events,
            watching: false,
            created_at: crate::util::now(),
            last,
            ringback: None,
            video_used,
        }
    }

    /// Starts forwarding this call's media events and its media-finished signal, once.
    pub(super) fn watch(&mut self) {
        if self.watching {
            return;
        }
        let Some(handle) = self.call.handle() else {
            return;
        };
        self.watching = true;
        let (tx, events) = async_channel::bounded(64);
        self.events = events;
        tokio::spawn(async move {
            let Some(mut media) = handle.take_events() else {
                // Only one task may own the protocol event stream. Completion remains sticky
                // even if a previous owner has already consumed the terminal event.
                handle.wait_ended().await;
                let _ = tx.send(CallRuntimeEvent::Ended).await;
                return;
            };
            loop {
                tokio::select! {
                    biased;
                    event = media.recv() => match event {
                        Ok(event) => {
                            if tx.send(CallRuntimeEvent::Media(event)).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(CallRuntimeEvent::Ended).await;
                            return;
                        }
                    },
                    _ = handle.wait_ended() => {
                        let _ = tx.send(CallRuntimeEvent::Ended).await;
                        return;
                    }
                }
            }
        });
    }
}

impl Worker {
    pub(super) fn backfill_call_entries(&self) {
        if self
            .archive
            .meta("call_entries_derived")
            .ok()
            .flatten()
            .as_deref()
            == Some("1")
        {
            return;
        }
        let Ok(rows) = self.archive.unsupported_call_rows() else {
            log::warn!("could not read archived call entries");
            return;
        };
        for (chat, id, raw) in rows {
            let Ok(message) = wa::Message::decode_from_slice(&raw) else {
                continue;
            };
            let Some(call) = message.get_base_message().call_log_messsage.as_option() else {
                continue;
            };
            if self
                .archive
                .backfill_call_content(&chat, &id, &message_content(call))
                .is_err()
            {
                log::warn!("could not derive archived call entry");
                return;
            }
        }
        if self.archive.set_meta("call_entries_derived", "1").is_err() {
            log::warn!("could not checkpoint call entry backfill");
        }
    }

    /// Official history/app-state call metadata has no live-message side effects.
    pub(super) fn file_synced_call(
        &mut self,
        record: &wa::CallLogRecord,
        creator: Option<&Jid>,
        from_me: Option<bool>,
        call_id: Option<&str>,
    ) {
        use sha2::{Digest, Sha256};
        let raw = record.encode_to_vec();
        let creator = creator.map(|jid| self.canonical(jid));
        // Identity is creator + call id, not the call id alone. Unresolved records stay encrypted.
        let identity = match (creator.as_deref(), call_id.filter(|id| !id.is_empty())) {
            (Some(creator), Some(id)) => format!("{creator}\0{id}"),
            _ => format!("raw:{}", crate::settings::hex(&Sha256::digest(&raw))),
        };
        let id = format!(
            "call-log:{}",
            crate::settings::hex(&Sha256::digest(identity.as_bytes()))
        );
        if self
            .archive
            .save_synced_call_raw(&id, creator.as_deref(), from_me, &raw)
            .is_err()
        {
            log::warn!("could not retain synced call metadata");
            return;
        }
        if creator.is_none() || call_id.is_none_or(str::is_empty) {
            return;
        }
        let (Some(from_me), Some(at)) = (from_me, record.start_time.filter(|at| *at > 0)) else {
            return;
        };
        let chat = if let Some(group) = record
            .group_jid
            .as_deref()
            .filter(|jid| jid.ends_with("@g.us"))
        {
            Some(self.canonical_str(group))
        } else if !from_me {
            creator
                .clone()
                .filter(|jid| callable_chat(jid) && !self.is_me(jid))
        } else {
            let mut peers: Vec<_> = record
                .participants
                .iter()
                .filter_map(|p| p.user_jid.as_deref())
                .map(|jid| self.canonical_str(jid))
                .filter(|jid| callable_chat(jid) && !self.is_me(jid))
                .collect();
            peers.sort();
            peers.dedup();
            (peers.len() == 1).then(|| peers.remove(0))
        };
        let Some(chat) = chat else {
            return;
        };
        if self.predates_removal(&chat, at) || self.message_was_removed(&chat, &id) {
            return;
        }
        use wa::call_log_record::CallResult;
        let outcome = match record.call_result {
            Some(CallResult::CONNECTED) => calls::CallOutcome::Answered,
            Some(CallResult::REJECTED) => calls::CallOutcome::Declined,
            Some(CallResult::CANCELLED | CallResult::ABANDONED) => calls::CallOutcome::NoAnswer,
            Some(CallResult::ACCEPTEDELSEWHERE) => calls::CallOutcome::AnsweredElsewhere,
            Some(CallResult::MISSED) => calls::CallOutcome::Missed,
            Some(CallResult::FAILED | CallResult::INVALID | CallResult::UNAVAILABLE) => {
                calls::CallOutcome::Failed
            }
            _ => return,
        };
        let direction = if from_me {
            crate::model::CallDirection::Outgoing
        } else {
            crate::model::CallDirection::Incoming
        };
        // The raw record's duration units are unspecified. Only durationSecs messages prove seconds.
        let saved = crate::archive::CallRecord {
            id: id.clone(),
            chat: chat.clone(),
            direction,
            outcome,
            at,
            duration: None,
        };
        let local = match call_id
            .map(|id| self.archive.has_local_call(id))
            .transpose()
        {
            Ok(local) => local.unwrap_or(false),
            Err(_) => {
                log::warn!("could not inspect local call outcome");
                return;
            }
        };
        if !local && self.archive.save_call_record(&saved).is_err() {
            log::warn!("could not store synced call outcome");
            return;
        }
        if local
            && self
                .archive
                .message(
                    &chat,
                    &format!("local-call:{}", call_id.unwrap_or_default()),
                )
                .is_ok_and(|row| row.is_some())
        {
            // Keep the locally measured duration; a later app-state replay is not another call.
            return;
        }
        let row = Message {
            id,
            chat: chat.clone(),
            sender: if from_me {
                self.me()
            } else {
                creator.unwrap_or_else(|| chat.clone())
            },
            sender_name: None,
            from_me,
            timestamp: at,
            history_order: None,
            content: Content::Call {
                video: record.is_video.unwrap_or(false),
                outcome: outcome_text(outcome).into(),
                seconds: None,
            },
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        };
        // insert_message enforces the archive's deletion barriers. Never replace an existing edit/revoke.
        match self.archive.message(&chat, &row.id) {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => {
                log::warn!("could not inspect synced call entry");
                return;
            }
        }
        if self
            .archive
            .ensure_chat(
                &chat,
                &self.name_for(&chat).unwrap_or_else(|| "Chat".into()),
            )
            .is_err()
            || self.archive.insert_message(&row, None).is_err()
        {
            log::warn!("could not store synced call entry");
            return;
        }
        // Read back because a local deletion barrier can have refused the insertion.
        let visible = self.privacy_ready
            && !self.call_privacy_paused
            && self
                .archive
                .chat(&chat)
                .is_ok_and(|chat| chat.is_some_and(|chat| !chat.locked));
        if visible && let Ok(Some(row)) = self.archive.message(&chat, &row.id) {
            self.emit(Event::Messages {
                chat,
                messages: vec![row],
                older: true,
                complete: false,
            });
        }
        if visible && let Ok(records) = self.archive.call_records() {
            self.emit(Event::CallLog(records));
        }
    }

    pub(super) fn begin_call_device_discovery(&mut self) {
        self.call_device_discovery.cancel();
        let generation = self.call_device_discovery.generation;
        let commands = self.commands.clone();
        self.call_device_discovery.task = Some(tokio::spawn(async move {
            let mut devices = discover_devices().await;
            devices.camera_discovery_generation = Some(generation);
            let _ = commands.send(Command::CallDevicesDiscovered(devices));
        }));
    }

    pub(super) fn load_call_log(&mut self) {
        if !self.privacy_ready || self.call_privacy_paused {
            return;
        }
        self.begin_call_device_discovery();
        match self.archive.call_records() {
            Ok(records) => self.emit(Event::CallLog(records)),
            Err(_) => self.emit(Event::Error("Could not read call history".into())),
        }
    }

    // -----------------------------------------------------------------------
    // Command entry points
    // -----------------------------------------------------------------------

    fn begin_call_audio_device_discovery(&self) {
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let devices = discover_audio_devices().await;
            let _ = commands.send(Command::CallDevicesDiscovered(devices));
        });
    }

    /// Stores and publishes a device list, so the pickers and the fallback naming share one
    /// snapshot of the machine.
    pub(super) fn publish_call_devices(&mut self, mut devices: calls::DeviceList) {
        if devices
            .camera_discovery_generation
            .is_some_and(|generation| generation != self.call_device_discovery.generation)
        {
            return;
        }
        // Request provenance is only for accepting the result, not replaying the cached snapshot.
        devices.camera_discovery_generation = None;
        if !devices.cameras_discovered {
            devices.cameras = self.call_devices.cameras.clone();
            devices.camera_error = self.call_devices.camera_error.clone();
            devices.cameras_discovered = self.call_devices.cameras_discovered;
        }
        log::info!(
            "call devices published: microphones={} speakers={} cameras={} microphone_failed={} speaker_failed={} camera_failed={} cameras_discovered={}",
            devices.microphones.len(),
            devices.speakers.len(),
            devices.cameras.len(),
            devices.microphone_error.is_some(),
            devices.speaker_error.is_some(),
            devices.camera_error.is_some(),
            devices.cameras_discovered,
        );
        self.call_devices = devices.clone();
        self.emit(Event::CallDevices(Box::new(devices)));
    }

    /// Re-sends the current call after privacy recovery, since call updates are withheld while the
    /// lock state is unknown.
    ///
    /// Nothing is rediscovered: the last published list is what the picker already had, and the
    /// runtime's own snapshot is what the surface should draw. The chat can finally be judged
    /// private or not, so a call that arrived mid-recovery rings behind the right lock state rather
    /// than being dropped for the whole recovery.
    pub(super) fn replay_call(&mut self) {
        let Some(runtime) = self.call.as_ref() else {
            return;
        };
        let devices = self.call_devices.clone();
        let update = runtime.last.clone();
        self.publish_call_devices(devices);
        self.emit_call(update);
    }

    /// Publishes one call state, and lets the call go once it reaches a terminal phase.
    ///
    /// The UI keeps rendering the snapshot it was handed; what is released here is the media, so a
    /// finished call holds no child process and no task.
    pub(super) fn emit_call(&mut self, update: CallUpdate) {
        let finished = !update.phase.is_live();
        if finished && let Some((_, task)) = self.call_preparing.take() {
            // Dropping the pending future also drops its capture and the facade's setup guard.
            self.call_prepare_generation = self.call_prepare_generation.wrapping_add(1);
            task.abort();
            self.emit(Event::CallPreparing(false));
        }
        if finished {
            self.cancel_video_preparation();
        }
        // Remembered so the periodic check can tell a real change from a snapshot it already sent.
        if let Some(runtime) = self.call.as_mut()
            && runtime.call.generation() == update.generation
        {
            if let Some(tone) = ringing_tone(
                update.direction,
                update.phase,
                self.call_preparing.is_some(),
            ) {
                if let Some(ringback) = &runtime.ringback {
                    if runtime.last.speaker != update.speaker {
                        ringback.bind(update.speaker.clone());
                    }
                } else {
                    runtime.ringback = runtime.call.ringtone(tone);
                }
            } else {
                // Stop at either answer, not only after relay allocation or hangup.
                runtime.ringback = None;
            }
            runtime.video_used |= update.video_requested || update.video_active;
            runtime.last = update.clone();
        }
        if finished && let Some(runtime) = self.call.as_ref() {
            let record = crate::archive::CallRecord {
                id: runtime.call.call_id().to_owned(),
                chat: self.canonical_str(&update.chat),
                direction: update.direction,
                outcome: update.outcome.unwrap_or(calls::CallOutcome::Failed),
                at: runtime.created_at,
                duration: update.started.map(|started| started.elapsed().as_secs()),
            };
            if self.archive.save_call_record(&record).is_err() {
                self.emit(Event::Error("Could not preserve the call outcome".into()));
            } else {
                self.file_local_call(&record, runtime.video_used);
            }
        }
        self.emit(Event::Call(Box::new(update)));
        if finished {
            self.call = None;
        }
    }

    fn file_local_call(&mut self, record: &crate::archive::CallRecord, video: bool) {
        let from_me = record.direction == crate::model::CallDirection::Outgoing;
        let row = Message {
            id: format!("local-call:{}", record.id),
            chat: record.chat.clone(),
            sender: if from_me {
                self.me()
            } else {
                record.chat.clone()
            },
            sender_name: None,
            from_me,
            timestamp: record.at,
            history_order: None,
            content: Content::Call {
                video,
                outcome: outcome_text(record.outcome).into(),
                seconds: record.duration,
            },
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        };
        if self.predates_removal(&record.chat, record.at)
            || self.message_was_removed(&record.chat, &row.id)
        {
            return;
        }
        if self
            .archive
            .ensure_chat(
                &record.chat,
                &self.name_for(&record.chat).unwrap_or_else(|| "Chat".into()),
            )
            .is_err()
            || self.archive.insert_message(&row, None).is_err()
        {
            self.emit(Event::Error(
                "Could not preserve the conversation call entry".into(),
            ));
            return;
        }
        if self.privacy_ready
            && !self.call_privacy_paused
            && self
                .archive
                .chat(&record.chat)
                .is_ok_and(|chat| chat.is_some_and(|chat| !chat.locked))
            && let Ok(Some(row)) = self.archive.message(&record.chat, &row.id)
        {
            self.emit(Event::Messages {
                chat: record.chat.clone(),
                messages: vec![row],
                older: false,
                complete: false,
            });
        }
    }

    /// Whether a call is already up, which every entry point refuses to double.
    pub(super) fn call_busy(&mut self) -> bool {
        if self.call.is_some() || self.call_preparing.is_some() {
            log::warn!("[CALL] refusing a new call: one is already up");
            return true;
        }
        false
    }

    pub(super) async fn start_call_mode(
        &mut self,
        chat: ChatId,
        with_video: bool,
        request: calls::CallRequest,
    ) {
        log::info!("call request received by worker");
        if request.is_cancelled() {
            self.emit(Event::CallPreparing(false));
            return;
        }
        if !self.privacy_ready || self.call_privacy_paused {
            self.emit(Event::CallPreparing(false));
            self.emit(Event::Error(
                "Calling is unavailable while account privacy protection is active".into(),
            ));
            return;
        }
        if self.call_chat_hidden(&chat, false) {
            self.emit(Event::CallPreparing(false));
            return;
        }
        if self.call_busy() {
            self.emit(Event::CallPreparing(false));
            self.emit(Event::Error("Finish the current call first".into()));
            return;
        }
        if !callable_chat(&chat) {
            self.emit(Event::CallPreparing(false));
            log::warn!("[CALL] refusing a call to a chat that is not one to one");
            self.emit(Event::Error("Calls are one to one only".to_owned()));
            return;
        }
        let Some(client) = self.client.clone() else {
            self.emit(Event::CallPreparing(false));
            log::warn!("[CALL] cannot start a call: WhatsApp is not connected");
            self.emit(Event::Error(
                "Zap Fasterk is not connected to WhatsApp".to_owned(),
            ));
            return;
        };
        // The devices the settings remember, checked against the machine first: a headset that was
        // switched off since the last call falls back to the system default and says so, instead of
        // opening a stream that can never deliver a frame.
        let wanted = self.call_defaults.clone();
        let camera = self.call_camera.clone();
        self.call_preparing_signals.clear();
        let commands = self.commands.clone();
        self.call_prepare_generation = self.call_prepare_generation.wrapping_add(1);
        let generation = self.call_prepare_generation;
        self.outgoing_call_request = Some(request.clone());
        self.emit(Event::CallPreparing(true));
        self.call_preparing = Some((
            generation,
            tokio::spawn(async move {
                let preparation_started = Instant::now();
                let preparation = tokio::time::timeout(Duration::from_secs(45), async {
                    let devices = discover_audio_devices().await;
                    log::info!(
                        "call device discovery finished: {} ms",
                        preparation_started.elapsed().as_millis()
                    );
                    let resolved =
                        calls::resolve_devices(&devices, wanted.microphone, wanted.speaker);
                    let _ = commands.send(Command::CallDevicesDiscovered(devices));
                    let mut call = Call::place_mode(
                        &client,
                        chat,
                        resolved.microphone,
                        resolved.speaker,
                        with_video,
                        camera,
                    )
                    .await
                    .map_err(|error| call_preparation_error(error,
                        "The call could not be started. Check your connection and audio devices."))?;
                    call.set_lost_devices(resolved.lost_devices);
                    Ok(Box::new(call))
                });
                let result = tokio::select! {
                    biased;
                    _ = request.cancelled() => Err("Call cancelled".to_owned()),
                    result = preparation => result.unwrap_or_else(|_| Err("The call did not connect in time".into())),
                };
                log::info!(
                    "call preparation finished: {} ms, success={}",
                    preparation_started.elapsed().as_millis(),
                    result.is_ok()
                );
                let _ = commands.send(Command::CallPrepared { generation, result });
            }),
        ));
    }

    pub(super) async fn call_prepared(
        &mut self,
        generation: u64,
        result: Result<Box<Call>, String>,
    ) {
        if self
            .call_preparing
            .as_ref()
            .is_none_or(|(pending, _)| *pending != generation)
        {
            if let Ok(mut call) = result {
                call.hangup(self.client.as_ref()).await;
            }
            return;
        }
        self.call_preparing = None;
        if self
            .outgoing_call_request
            .take()
            .is_some_and(|request| request.is_cancelled())
        {
            if let Ok(mut call) = result {
                call.hangup(self.client.as_ref()).await;
            }
            self.call_preparing_signals.clear();
            self.emit(Event::CallPreparing(false));
            return;
        }
        self.emit(Event::CallPreparing(false));
        match result {
            Ok(call)
                if generation == self.call_prepare_generation
                    && !self.call_privacy_paused
                    && self.privacy_ready =>
            {
                let mut runtime = CallRuntime::new(*call);
                if let Some(ringing) = self.call.as_ref() {
                    runtime.created_at = ringing.created_at;
                }
                for signal in self.call_preparing_signals.drain(..) {
                    match signal {
                        PreparationSignal::Action(action) => {
                            runtime.call.signaling(&action);
                        }
                        PreparationSignal::Resolved(id) if id == runtime.call.call_id() => {
                            runtime.call.resolved_elsewhere();
                        }
                        PreparationSignal::Resolved(_) => {}
                    }
                }
                runtime.watch();
                let update = runtime.call.update();
                self.call = Some(runtime);
                self.emit_call(update);
            }
            Ok(mut call) => {
                call.hangup(self.client.as_ref()).await;
                if self.call.is_none() {
                    self.emit(Event::CallDismissed);
                }
            }
            Err(error) => {
                if let Some(runtime) = self.call.as_ref() {
                    self.emit(Event::Call(Box::new(runtime.call.update())));
                } else {
                    self.emit(Event::CallDismissed);
                }
                self.emit(Event::Error(error));
            }
        }
        self.call_preparing_signals.clear();
    }

    pub(super) async fn answer_call(&mut self) {
        self.answer_call_mode(false).await;
    }

    pub(super) async fn answer_video_call(&mut self) {
        self.answer_call_mode(true).await;
    }

    async fn answer_call_mode(&mut self, with_video: bool) {
        if !self.privacy_ready || self.call_privacy_paused || self.call_preparing.is_some() {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(runtime) = self.call.as_ref() else {
            return;
        };
        let Some(mut call) = runtime.call.prepare_answer() else {
            return;
        };
        // Answering admits capture preparation immediately. Stop the chime before
        // opening the microphone, so local ringing cannot leak into the call.
        if let Some(runtime) = self.call.as_mut() {
            runtime.ringback = None;
        }
        let camera = self.call_camera.clone();
        let commands = self.commands.clone();
        self.call_prepare_generation = self.call_prepare_generation.wrapping_add(1);
        let generation = self.call_prepare_generation;
        self.emit(Event::CallPreparing(true));
        self.call_preparing = Some((
            generation,
            tokio::spawn(async move {
                let result = tokio::time::timeout(Duration::from_secs(45), async {
                    let devices = discover_audio_devices().await;
                    let (microphone, speaker) = call.selections();
                    let resolved = calls::resolve_devices(&devices, microphone, speaker);
                    let _ = commands.send(Command::CallDevicesDiscovered(devices));
                    if let Err(error) = call
                        .answer_mode(&client, resolved.microphone, resolved.speaker, with_video, camera)
                        .await
                    {
                        call.cleanup();
                        return Err(call_preparation_error(error,
                            "The call could not be answered. Check your connection and audio devices."));
                    }
                    call.set_lost_devices(resolved.lost_devices);
                    Ok(Box::new(call))
                })
                .await
                .unwrap_or_else(|_| Err("The call did not connect in time".into()));
                let _ = commands.send(Command::CallPrepared { generation, result });
            }),
        ));
    }

    pub(super) async fn decline_call(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let update = match self.call.as_mut() {
            Some(runtime) => match runtime.call.decline(&client).await {
                Ok(()) => Some(runtime.call.update()),
                Err(_error) => {
                    log::error!("call device or transport operation failed");
                    self.emit(Event::Error(
                        "The call could not be declined. Try again.".into(),
                    ));
                    return;
                }
            },
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn hangup_call(&mut self) {
        if let Some(request) = self.outgoing_call_request.take() {
            request.cancel();
        }
        if let Some((_, task)) = self.call_preparing.take() {
            self.call_prepare_generation = self.call_prepare_generation.wrapping_add(1);
            task.abort();
            self.call_preparing_signals.clear();
            self.emit(Event::CallPreparing(false));
            self.emit(Event::CallDismissed);
        }
        self.cancel_video_preparation();
        let client = self.client.clone();
        let update = match self.call.as_mut() {
            Some(runtime) => {
                runtime.ringback = None;
                Some(runtime.call.hangup(client.as_ref()).await)
            }
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn set_call_muted(&mut self, muted: bool) {
        let update = match self.call.as_mut() {
            Some(runtime) => runtime.call.set_muted(muted).await,
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    fn cancel_video_preparation(&mut self) {
        if let Some((_, task)) = self.call_video_preparing.take() {
            task.abort();
        }
        if let Some(runtime) = self.call.as_mut() {
            runtime.call.video_preparation_finished();
        }
    }

    pub(super) async fn set_call_video(&mut self, enabled: bool) {
        if !enabled {
            self.cancel_video_preparation();
            if let Some(runtime) = self.call.as_mut() {
                let result = runtime.call.stop_video().await;
                let update = runtime.call.update();
                self.emit_call(update);
                if result.is_err() {
                    self.emit(Event::Error(
                        "The camera is off, but WhatsApp did not confirm the video change.".into(),
                    ));
                }
            }
            return;
        }
        if !self.privacy_ready || self.call_privacy_paused || self.call_video_preparing.is_some() {
            return;
        }
        let Some(runtime) = self.call.as_mut() else {
            return;
        };
        let Some((handle, request)) = runtime.call.prepare_video() else {
            return;
        };
        let generation = runtime.call.generation();
        let update = runtime.call.update();
        self.emit_call(update);
        let commands = self.commands.clone();
        let camera = self.call_camera.clone();
        self.call_video_preparing = Some((
            generation,
            tokio::spawn(async move {
                let result = tokio::time::timeout(Duration::from_secs(30), async {
                    let mut video = crate::camera::VideoSession::open(camera)
                        .await
                        .map_err(|error| error.to_string())?;
                    video.handle = Some(handle.clone());
                    let result = if let Some(request) = request {
                        handle
                            .accept_video(request, video.source(), video.sink())
                            .await
                    } else {
                        handle.start_video(video.source(), video.sink()).await
                    };
                    if result.is_err() {
                        video.stop();
                        return Err(
                            "WhatsApp could not enable video. The camera is off.".to_owned()
                        );
                    }
                    Ok(Box::new(video))
                })
                .await
                .unwrap_or_else(|_| Err("Video setup timed out. The camera is off.".into()));
                let _ = commands.send(Command::CallVideoPrepared { generation, result });
            }),
        ));
    }

    pub(super) async fn call_video_prepared(
        &mut self,
        generation: u64,
        result: Result<Box<crate::camera::VideoSession>, String>,
    ) {
        let fresh = self
            .call_video_preparing
            .as_ref()
            .is_some_and(|(pending, _)| *pending == generation);
        if fresh {
            self.call_video_preparing = None;
        }
        match result {
            Ok(video)
                if fresh
                    && self.privacy_ready
                    && !self.call_privacy_paused
                    && self.call.as_ref().is_some_and(|runtime| {
                        runtime.call.generation() == generation
                            && runtime.call.phase() == calls::CallPhase::Active
                    }) =>
            {
                if let Some(runtime) = self.call.as_mut() {
                    runtime.call.attach_video(*video);
                    let update = runtime.call.update();
                    self.emit_call(update);
                }
            }
            Ok(video) => {
                video.stop();
                if let Some(handle) = &video.handle {
                    let _ = handle.stop_video().await;
                }
            }
            Err(error) if fresh => {
                if let Some(runtime) = self.call.as_mut() {
                    runtime.call.video_preparation_finished();
                    let update = runtime.call.update();
                    self.emit_call(update);
                }
                self.emit(Event::Error(error));
            }
            Err(_) => {}
        }
    }

    pub(super) fn set_call_microphone(&mut self, device: Option<String>) {
        // The picker is also the preference: the next call opens where this one was left.
        self.call_defaults.microphone = device.clone();
        let update = self
            .call
            .as_mut()
            .map(|runtime| runtime.call.set_microphone(device));
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) fn set_call_speaker(&mut self, device: Option<String>) {
        self.call_defaults.speaker = device.clone();
        let update = self
            .call
            .as_mut()
            .map(|runtime| runtime.call.set_speaker(device));
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    // -----------------------------------------------------------------------
    // Backend events
    // -----------------------------------------------------------------------

    /// One event from the current call's media plane.
    pub(super) fn call_runtime(&mut self, event: CallRuntimeEvent) {
        let update = match self.call.as_mut() {
            Some(runtime) => match event {
                CallRuntimeEvent::Ended => runtime.call.media_ended(),
                CallRuntimeEvent::Media(media) => runtime.call.media(&media),
            },
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    /// Routes one inbound `<call>` stanza: either it belongs to the call we own, or it is a new
    /// offer that should ring.
    ///
    /// This is where an outgoing call learns it was answered, rejected, or ended, so the phase the
    /// UI shows is the peer's, not the dialer's.
    pub(super) async fn call_signaling(&mut self, incoming: &IncomingCall) {
        let action = &incoming.action;
        let mine = self
            .call
            .as_ref()
            .is_some_and(|runtime| runtime.call.call_id() == action.call_id());
        if mine {
            let update = self
                .call
                .as_mut()
                .and_then(|runtime| runtime.call.signaling(action));
            if let Some(update) = update {
                self.emit_call(update);
            }
            return;
        }
        if !calls::is_offer(action) {
            if self.call_preparing.is_some() && self.call.is_none() {
                self.buffer_preparation_signal(PreparationSignal::Action(action.clone()));
            }
            return;
        }
        if self.call_busy() {
            // Refuse it to the caller rather than dropping it. An offer that is only dropped leaves
            // the other side ringing until its own protocol timeout, believing this device might
            // still answer; a real `<reject>` ends it there and then. The call that is already up
            // is untouched: this is the second offer's own call id being refused, not ours.
            match self.client.as_ref() {
                Some(client) => match client.voip().reject(incoming).await {
                    Ok(()) => log::info!("call state updated"),
                    Err(_error) => {
                        log::warn!("call device or transport operation failed")
                    }
                },
                None => log::warn!("[CALL] no client to refuse a second offer with"),
            }
            return;
        }
        let chat = self.canonical(&incoming.from);
        // A group, broadcast or newsletter offer never reaches the 1:1 call surface. The interface
        // hides the buttons for those chats, but the worker is the boundary: a non-direct offer is
        // refused here rather than rung, so no call is created and no history entry is written.
        //
        // Refuse it to the caller instead of only dropping it: a dropped offer leaves the other
        // side ringing until its own protocol timeout, believing this device might still answer,
        // while a real `<reject>` ends it there and then. Only a 1:1 offer rings, so an offer that
        // reaches here is one the caller is told we will not take.
        if !callable_chat(&chat) {
            match self.client.as_ref() {
                Some(client) => match client.voip().reject(incoming).await {
                    Ok(()) => log::warn!(
                        "[CALL] refused an incoming offer from a chat that is not one to one"
                    ),
                    Err(_error) => {
                        log::warn!("call device or transport operation failed")
                    }
                },
                None => log::warn!("[CALL] no client to refuse a non-direct offer with"),
            }
            return;
        }
        // The remembered devices are pre-selected on the prompt, so answering picks up right where
        // the last call left off.
        if self.call_privacy_paused || !self.privacy_ready || self.call_chat_hidden(&chat, true) {
            return;
        }
        let call = Call::ringing(chat, Box::new(incoming.clone()), self.call_defaults.clone());
        let update = call.update();
        self.call = Some(CallRuntime::new(call));
        self.begin_call_device_discovery();
        self.emit_call(update);
    }

    /// A storage error cannot establish that it is safe to reveal a caller or open capture.
    fn call_chat_hidden(&mut self, chat: &str, hide_archived: bool) -> bool {
        match self.archive.chat(chat) {
            Ok(chat) => chat.is_some_and(|chat| chat.locked || (hide_archived && chat.archived)),
            Err(_) => {
                self.emit(Event::Error(
                    "Could not check the call's privacy settings".into(),
                ));
                true
            }
        }
    }

    /// A call we were ringing or talking on was resolved on another of the account's devices, or
    /// the caller gave up before anyone answered.
    pub(super) fn call_resolved(&mut self, call_id: &str) {
        if self.call_preparing.is_some() && self.call.is_none() {
            self.buffer_preparation_signal(PreparationSignal::Resolved(call_id.into()));
        }
        let update = match self.call.as_mut() {
            Some(runtime) if runtime.call.call_id() == call_id => runtime.call.resolved_elsewhere(),
            _ => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    /// Releases the call on the way out, so quitting leaves no child process or task behind.
    pub(super) async fn shutdown_call(&mut self) {
        if let Some(request) = self.outgoing_call_request.take() {
            request.cancel();
        }
        self.call_prepare_generation = self.call_prepare_generation.wrapping_add(1);
        if let Some((_, task)) = self.call_preparing.take() {
            task.abort();
        }
        self.call_preparing_signals.clear();
        self.cancel_video_preparation();
        self.emit(Event::CallPreparing(false));
        if let Some(runtime) = self.call.as_mut() {
            runtime.ringback = None;
            let update = runtime.call.hangup(self.client.as_ref()).await;
            self.emit_call(update);
        }
    }

    /// Keeps a live call's picture of the machine honest.
    ///
    /// A device the user picked can disappear mid-call (a Bluetooth headset switching off, a sound
    /// card taken over by another app) and nothing here ends the call over it. The stream behind it
    /// is rebound to the system default, and the snapshot carries the reason, so the picker stops
    /// claiming a device that is not there.
    ///
    /// The fresh list is also published when it changed, so a device that came or went shows up in
    /// the pickers without anyone pressing anything.
    pub(super) async fn reconcile_call(&mut self) {
        if self
            .call
            .as_ref()
            .is_none_or(|runtime| !runtime.call.phase().is_live())
        {
            return;
        }
        // Once per heartbeat, the engine's own counters for this call. A call that is up but carrying
        // no audio is otherwise indistinguishable in a bug report from one where nobody spoke, and
        // these numbers are what tell the two apart.
        if let Some(runtime) = self.call.as_mut() {
            runtime.call.refresh_audio_health();
            runtime.call.log_media_stats();
        }
        // Discovery opens every audio device to name it, so it runs on a blocking thread rather
        // than on the worker's async loop: a slow device must not hold up message handling or the
        // call's own events.
        self.begin_call_audio_device_discovery();
        let devices = self.call_devices.clone();
        // Read before the fresh list replaces it: a device that just went away is still named in
        // here by the description the user saw.
        let known = self.call_devices.clone();
        let mut lost = Vec::new();
        if let Some(runtime) = self.call.as_mut() {
            lost.extend(runtime.call.take_stream_fallbacks());
            lost.extend(runtime.call.verify_devices(&devices).await);
        }
        if !lost.is_empty() {
            // Worded by the call screen, which knows the reader's language; the log says it plainly.
            let lost: Vec<calls::LostDevice> = lost
                .into_iter()
                .map(|(kind, id)| {
                    let name = calls::device_name(&known, kind, &id);
                    log::warn!("call device or transport operation failed");
                    calls::LostDevice { kind, name }
                })
                .collect();
            if let Some(runtime) = self.call.as_mut() {
                runtime.call.set_lost_devices(lost);
            }
        }
        // Compared against the snapshot the UI was handed, so a mute another device set reaches the
        // screen even though no user pressed anything.
        let changed = self.call.as_mut().and_then(|runtime| {
            let current = runtime.call.update();
            let changed = current != runtime.last;
            runtime.last = current.clone();
            changed.then_some(current)
        });
        if let Some(update) = changed {
            self.emit_call(update);
        }
    }

    fn buffer_preparation_signal(&mut self, signal: PreparationSignal) {
        if self.call_preparing_signals.len() >= 64 {
            // Never silently discard an end event and then publish an apparently live call.
            self.call_prepare_generation = self.call_prepare_generation.wrapping_add(1);
            self.emit(Event::CallPreparing(false));
            self.emit(Event::Error(
                "The call could not be prepared safely. Try again.".into(),
            ));
            return;
        }
        self.call_preparing_signals.push_back(signal);
    }
}

fn call_preparation_error(error: anyhow::Error, fallback: &'static str) -> String {
    error
        .downcast_ref::<calls::CameraSetupError>()
        .map_or_else(|| fallback.to_owned(), ToString::to_string)
}

/// Account-owned discovery: refreshing and dropping the worker cancel the native helper future.
#[derive(Default)]
pub(super) struct DeviceDiscovery {
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl DeviceDiscovery {
    fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for DeviceDiscovery {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Device discovery off the worker thread: enumerating opens every audio device to name it, so it
/// runs on a blocking thread rather than on the async loop, where a slow device would hold up
/// message handling and the call's own events.
async fn discover_audio_devices() -> calls::DeviceList {
    tokio::task::spawn_blocking(calls::devices)
        .await
        .unwrap_or_else(|_| calls::DeviceList {
            microphone_error: Some("Microphone discovery failed. Try refreshing devices.".into()),
            speaker_error: Some("Speaker discovery failed. Try refreshing devices.".into()),
            ..Default::default()
        })
}

async fn discover_devices() -> calls::DeviceList {
    let (audio, cameras) = tokio::join!(discover_audio_devices(), crate::camera::devices(),);
    let mut devices = audio;
    devices.cameras_discovered = true;
    match cameras {
        Ok(cameras) => devices.cameras = cameras,
        Err(_) => {
            devices.camera_error = Some("Camera discovery failed. Try refreshing devices.".into())
        }
    }
    devices
}

/// Whether a chat can be called at all.
///
/// The interface only draws the phone button on a one-to-one chat, but the worker is the boundary
/// rather than the interface: a group, a channel or a broadcast list reaching the 1:1 builder would
/// be refused by the protocol at best and misbehave at worst, so the JID is checked here as well.
fn callable_chat(chat: &str) -> bool {
    chat.rsplit_once('@').is_some_and(|(user, server)| {
        !user.is_empty() && matches!(server, "s.whatsapp.net" | "lid")
    }) && chat.parse::<Jid>().is_ok()
}

fn outcome_text(outcome: calls::CallOutcome) -> &'static str {
    match outcome {
        calls::CallOutcome::Answered => "Connected",
        calls::CallOutcome::Missed => "Missed",
        calls::CallOutcome::Declined => "Declined",
        calls::CallOutcome::Busy => "Busy",
        calls::CallOutcome::Failed => "Failed",
        calls::CallOutcome::NoAnswer => "No answer",
        calls::CallOutcome::ConnectionLost => "Connection lost",
        calls::CallOutcome::AnsweredElsewhere => "Answered on another device",
        calls::CallOutcome::DeclinedElsewhere => "Declined on another device",
    }
}

pub(super) fn message_content(call: &wa::message::CallLogMessage) -> Content {
    use wa::message::call_log_message::CallOutcome;
    let outcome = match call.call_outcome {
        Some(CallOutcome::CONNECTED) => "Connected",
        Some(CallOutcome::MISSED) => "Missed",
        Some(CallOutcome::FAILED) => "Failed",
        Some(CallOutcome::REJECTED) => "Declined",
        Some(CallOutcome::ACCEPTED_ELSEWHERE) => "Answered on another device",
        Some(CallOutcome::ONGOING) => "Ongoing",
        Some(CallOutcome::SILENCED_BY_DND) => "Silenced by Do Not Disturb",
        Some(CallOutcome::SILENCED_UNKNOWN_CALLER) => "Silenced unknown caller",
        _ => "Unknown outcome",
    };
    Content::Call {
        video: call.is_video.unwrap_or(false),
        outcome: outcome.into(),
        seconds: call
            .duration_secs
            .and_then(|seconds| u64::try_from(seconds).ok()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn call_feedback_rings_incoming_offers_and_stops_for_answers_or_preparation() {
        use crate::call_audio::CallTone;
        use crate::calls::CallPhase;
        use crate::model::CallDirection;
        assert_eq!(
            super::ringing_tone(CallDirection::Incoming, CallPhase::Incoming, false),
            Some(CallTone::Incoming)
        );
        assert_eq!(
            super::ringing_tone(CallDirection::Outgoing, CallPhase::Dialing, false),
            None
        );
        assert_eq!(
            super::ringing_tone(CallDirection::Outgoing, CallPhase::Ringing, false),
            Some(CallTone::Outgoing)
        );
        for direction in [CallDirection::Incoming, CallDirection::Outgoing] {
            for phase in [
                CallPhase::Incoming,
                CallPhase::Dialing,
                CallPhase::Ringing,
                CallPhase::Accepted,
                CallPhase::Connecting,
                CallPhase::Active,
                CallPhase::Ended,
                CallPhase::Failed,
            ] {
                assert_eq!(
                    super::ringing_tone(direction, phase, true),
                    None,
                    "{direction:?} {phase:?}"
                );
            }
            for phase in [
                CallPhase::Accepted,
                CallPhase::Connecting,
                CallPhase::Active,
                CallPhase::Ended,
                CallPhase::Failed,
            ] {
                assert_eq!(
                    super::ringing_tone(direction, phase, false),
                    None,
                    "{direction:?} {phase:?}"
                );
            }
        }
    }
    #[tokio::test]
    async fn rejected_call_requests_clear_the_immediate_preparing_indicator() {
        for reason in 0..5 {
            let (mut worker, events, _commands, _runtime) =
                super::super::offline_worker("15550000000@s.whatsapp.net");
            worker.privacy_ready = reason != 0;
            worker.call_privacy_paused = reason == 1;
            if reason == 2 {
                worker.call = Some(CallRuntime::new(Call::test_snapshot(
                    "15550000001@s.whatsapp.net",
                )));
            }
            let chat = if reason == 3 {
                "fixture@unknown"
            } else {
                "15550000001@s.whatsapp.net"
            };
            worker
                .start_call_mode(chat.into(), false, calls::CallRequest::default())
                .await;
            let mut cleared = false;
            let mut explained = false;
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(event, Event::CallPreparing(true)));
                cleared |= matches!(event, Event::CallPreparing(false));
                explained |= matches!(event, Event::Error(_));
            }
            assert!(
                cleared && explained,
                "rejection {reason} must complete the UI request"
            );
            assert!(worker.call_preparing.is_none());
        }
    }
    use super::*;

    #[tokio::test]
    async fn cancelled_queued_call_never_prepares_devices_or_sends_an_offer() {
        for video in [false, true] {
            let (mut worker, events, _commands, _runtime) =
                super::super::offline_worker("15550000000@s.whatsapp.net");
            let request = calls::CallRequest::default();
            request.cancel();
            worker
                .start_call_mode("15550000001@s.whatsapp.net".into(), video, request)
                .await;
            assert!(worker.call_preparing.is_none());
            assert!(worker.call.is_none());
            assert!(matches!(events.try_recv(), Ok(Event::CallPreparing(false))));
            assert!(
                events.try_recv().is_err(),
                "no discovery, offer, error or live call is published"
            );
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_preparation_without_worker_dispatch() {
        let request = calls::CallRequest::default();
        let waiting = request.clone();
        let task = tokio::spawn(async move { waiting.cancelled().await });
        tokio::task::yield_now().await;
        request.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(request.is_cancelled());
    }

    #[test]
    fn call_preparation_keeps_protocol_details_out_of_interface_errors() {
        assert_eq!(
            call_preparation_error(
                anyhow::anyhow!("private protocol diagnostic"),
                "Could not start the call"
            ),
            "Could not start the call"
        );
    }

    #[test]
    fn device_discovery_failures_reach_the_account_event_and_a_refresh_clears_them() {
        let (mut worker, events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        let failed = calls::DeviceList {
            microphone_error: Some("Microphone discovery failed. Try refreshing devices.".into()),
            speaker_error: Some("Speaker discovery failed. Try refreshing devices.".into()),
            ..Default::default()
        };
        worker.publish_call_devices(failed.clone());
        assert!(matches!(events.try_recv(), Ok(Event::CallDevices(list)) if *list == failed));
        worker.publish_call_devices(calls::DeviceList {
            microphones: vec![calls::AudioDevice {
                id: "fixture-mic".into(),
                label: "Synthetic microphone".into(),
            }],
            speakers: vec![calls::AudioDevice {
                id: "fixture-speaker".into(),
                label: "Synthetic speaker".into(),
            }],
            ..Default::default()
        });
        assert!(
            matches!(events.try_recv(), Ok(Event::CallDevices(list)) if list.microphones.len() == 1 && list.speakers.len() == 1 && list.microphone_error.is_none() && list.speaker_error.is_none())
        );
    }

    #[test]
    fn audio_refresh_preserves_camera_snapshot_and_explicit_empty_camera_refresh_clears_it() {
        let (mut worker, _events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        let camera = crate::camera::CameraDevice {
            id: "invented-device".into(),
            label: "Invented camera".into(),
        };
        worker.publish_call_devices(calls::DeviceList {
            cameras: vec![camera.clone()],
            cameras_discovered: true,
            ..Default::default()
        });
        worker.publish_call_devices(calls::DeviceList::default());
        assert_eq!(worker.call_devices.cameras, vec![camera]);
        assert!(worker.call_devices.cameras_discovered);
        worker.publish_call_devices(calls::DeviceList {
            cameras_discovered: true,
            ..Default::default()
        });
        assert!(worker.call_devices.cameras.is_empty());
        assert!(worker.call_devices.cameras_discovered);
    }

    #[test]
    fn superseded_camera_discovery_cannot_replace_the_latest_device_snapshot() {
        let (mut worker, _events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        worker.call_device_discovery.cancel();
        let old_generation = worker.call_device_discovery.generation;
        worker.call_device_discovery.cancel();
        let camera = crate::camera::CameraDevice {
            id: "latest-device".into(),
            label: "Latest synthetic camera".into(),
        };
        worker.publish_call_devices(calls::DeviceList {
            cameras: vec![camera.clone()],
            cameras_discovered: true,
            camera_discovery_generation: Some(worker.call_device_discovery.generation),
            ..Default::default()
        });
        worker.publish_call_devices(calls::DeviceList {
            cameras_discovered: true,
            camera_discovery_generation: Some(old_generation),
            ..Default::default()
        });
        assert_eq!(worker.call_devices.cameras, vec![camera]);
        assert_eq!(worker.call_devices.camera_discovery_generation, None);
        // Cached snapshots remain replayable while another refresh is pending.
        let cached = worker.call_devices.clone();
        worker.call_device_discovery.cancel();
        worker.publish_call_devices(cached);
        assert!(!worker.call_devices.cameras.is_empty());
    }

    #[tokio::test]
    async fn dropping_account_discovery_cancels_its_pending_task() {
        struct Cancelled(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (started, running) = tokio::sync::oneshot::channel();
        let (cancelled, stopped) = tokio::sync::oneshot::channel();
        let discovery = DeviceDiscovery {
            task: Some(tokio::spawn(async move {
                let _guard = Cancelled(Some(cancelled));
                let _ = started.send(());
                std::future::pending::<()>().await;
            })),
            ..Default::default()
        };
        running.await.unwrap();
        drop(discovery);
        tokio::time::timeout(Duration::from_secs(1), stopped)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn synced_calls_use_creator_direction_and_start_time_without_live_side_effects() {
        let (mut worker, events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        worker.privacy_ready = true;
        let creator: Jid = "15550000001@s.whatsapp.net".parse().unwrap();
        let record = wa::CallLogRecord {
            call_id: Some("fixture-call".into()),
            call_result: Some(wa::call_log_record::CallResult::MISSED),
            start_time: Some(1234),
            is_incoming: Some(false),
            is_video: Some(true),
            duration: Some(999),
            ..Default::default()
        };
        for _ in 0..2 {
            worker.file_synced_call(
                &record,
                Some(&creator),
                Some(false),
                record.call_id.as_deref(),
            );
        }
        let calls = worker.archive.call_records().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].at, 1234);
        assert_eq!(calls[0].direction, crate::model::CallDirection::Incoming);
        assert_eq!(calls[0].duration, None);
        let row = worker
            .archive
            .message(&calls[0].chat, &calls[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(row.timestamp, 1234);
        assert!(!row.from_me);
        assert!(matches!(row.content, Content::Call { video: true, .. }));
        assert!(worker.call.is_none());
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, Event::Incoming { .. }));
        }
        let mut unknown = record.clone();
        unknown.start_time = None;
        worker.file_synced_call(&unknown, Some(&creator), None, Some("unresolved"));
        assert_eq!(worker.archive.call_records().unwrap().len(), 1);
        worker.learn_lid("9000002", "15550000001");
        let alias: Jid = "9000002@lid".parse().unwrap();
        worker.file_synced_call(
            &record,
            Some(&alias),
            Some(false),
            record.call_id.as_deref(),
        );
        assert_eq!(worker.archive.call_records().unwrap().len(), 1);
    }

    #[test]
    fn call_message_seconds_are_explicit_and_negative_duration_stays_unknown() {
        let mut call = wa::message::CallLogMessage {
            is_video: Some(true),
            duration_secs: Some(91),
            call_outcome: Some(wa::message::call_log_message::CallOutcome::ACCEPTED_ELSEWHERE),
            ..Default::default()
        };
        assert!(matches!(
            message_content(&call),
            Content::Call {
                seconds: Some(91),
                video: true,
                ..
            }
        ));
        call.duration_secs = Some(-1);
        assert!(matches!(
            message_content(&call),
            Content::Call { seconds: None, .. }
        ));
    }

    #[test]
    fn outgoing_sync_canonicalizes_peer_and_does_not_replace_local_measurement() {
        let (mut worker, events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        worker.privacy_ready = true;
        worker.learn_lid("9000001", "15550000001");
        let creator: Jid = "15550000000@s.whatsapp.net".parse().unwrap();
        let record = wa::CallLogRecord {
            call_id: Some("measured".into()),
            start_time: Some(4567),
            call_result: Some(wa::call_log_record::CallResult::CONNECTED),
            participants: vec![wa::call_log_record::ParticipantInfo {
                user_jid: Some("9000001@lid".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        worker
            .archive
            .save_call_record(&crate::archive::CallRecord {
                id: "measured".into(),
                chat: "15550000001@s.whatsapp.net".into(),
                direction: crate::model::CallDirection::Outgoing,
                outcome: calls::CallOutcome::Answered,
                at: 4567,
                duration: Some(42),
            })
            .unwrap();
        worker.file_synced_call(&record, Some(&creator), Some(true), Some("measured"));
        let calls = worker.archive.call_records().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].duration, Some(42));
        let rows: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                Event::Messages { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat, "15550000001@s.whatsapp.net");
        assert!(rows[0].from_me);
        assert_eq!(rows[0].timestamp, 4567);
    }

    #[test]
    fn missed_history_stubs_are_chronological_entries() {
        let info = wa::WebMessageInfo {
            key: Some(wa::MessageKey {
                id: Some("fixture-stub".into()),
                remote_jid: Some("15550000001@s.whatsapp.net".into()),
                from_me: Some(false),
                ..Default::default()
            })
            .into(),
            message_timestamp: Some(1234),
            message_stub_type: Some(wa::web_message_info::StubType::CALL_MISSED_VIDEO),
            ..Default::default()
        };
        let chat = super::super::parse_conversation(wa::Conversation {
            id: "15550000001@s.whatsapp.net".into(),
            messages: vec![wa::HistorySyncMsg {
                message: Some(info).into(),
                msg_order_id: Some(7),
            }],
            ..Default::default()
        });
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].timestamp, 1234);
        assert_eq!(chat.messages[0].history_order, Some(7));
        assert!(matches!(
            chat.messages[0].content,
            Content::Call { video: true, .. }
        ));
    }

    #[test]
    fn only_a_one_to_one_chat_can_be_called() {
        assert!(callable_chat("15551234567@s.whatsapp.net"));
        assert!(callable_chat("123456789012345@lid"));
        assert!(!callable_chat("12345-67890@g.us"));
        assert!(!callable_chat("1234567890@broadcast"));
        assert!(!callable_chat("1234567890@newsletter"));
        assert!(!callable_chat("invalid"));
        assert!(!callable_chat("@lid"));
        assert!(!callable_chat("fixture@unknown"));
    }

    #[tokio::test]
    async fn early_remote_end_is_replayed_before_a_prepared_call_is_published() {
        let (mut worker, _events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        worker.privacy_ready = true;
        worker.call_prepare_generation = 1;
        worker.call_preparing = Some((1, tokio::spawn(async {})));
        worker.buffer_preparation_signal(PreparationSignal::Resolved("test-call".into()));
        worker
            .call_prepared(
                1,
                Ok(Box::new(Call::test_snapshot("15550000001@s.whatsapp.net"))),
            )
            .await;
        assert!(worker.call.is_none());
        assert!(worker.call_preparing.is_none());
        assert!(worker.call_preparing_signals.is_empty());
        assert_eq!(
            worker.archive.call_records().unwrap()[0].outcome,
            calls::CallOutcome::Missed
        );
    }

    #[tokio::test]
    async fn incoming_remote_end_invalidates_a_pending_answer() {
        let (mut worker, _events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        worker.privacy_ready = true;
        worker.call_prepare_generation = 1;
        worker.call_preparing = Some((1, tokio::spawn(async {})));
        let call = Call::test_snapshot("15550000001@s.whatsapp.net");
        let prepared = call.prepare_answer().unwrap();
        worker.call = Some(CallRuntime::new(call));
        worker.call_resolved("test-call");
        assert_ne!(worker.call_prepare_generation, 1);
        worker.call_prepared(1, Ok(Box::new(prepared))).await;
        assert!(worker.call.is_none());
        assert_eq!(worker.archive.call_records().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn completed_local_call_has_one_conversation_entry_and_synced_replay_keeps_duration() {
        let (mut worker, events, _commands, _runtime) =
            super::super::offline_worker("15550000000@s.whatsapp.net");
        worker.privacy_ready = true;
        let chat = "15550000001@s.whatsapp.net";
        worker.learn_lid("9000001", "15550000001");
        let call = Call::test_snapshot("9000001@lid");
        let mut update = call.update();
        update.direction = crate::model::CallDirection::Outgoing;
        update.phase = calls::CallPhase::Ended;
        update.outcome = Some(calls::CallOutcome::Answered);
        update.started = Some(std::time::Instant::now() - Duration::from_secs(42));
        let mut runtime = CallRuntime::new(call);
        runtime.video_used = true;
        worker.call = Some(runtime);
        while events.try_recv().is_ok() {}
        worker.emit_call(update);
        let row = worker
            .archive
            .message(chat, "local-call:test-call")
            .unwrap()
            .unwrap();
        assert!(row.from_me);
        assert!(matches!(
            row.content,
            Content::Call {
                video: true,
                seconds: Some(42),
                ..
            }
        ));
        assert!(!row.allows_reaction());
        assert!(!crate::app::can_select(&row.content));
        assert!(
            worker.archive.oldest(chat).unwrap().is_none(),
            "local call identifiers cannot anchor phone-history requests"
        );
        let mut phone_message = row.clone();
        phone_message.id = "fixture-phone-message".into();
        phone_message.timestamp += 1;
        phone_message.from_me = false;
        phone_message.sender = chat.into();
        worker.archive.insert_message(&phone_message, None).unwrap();
        assert_eq!(
            worker.archive.oldest(chat).unwrap().unwrap().id,
            phone_message.id
        );
        let mut incoming_call = phone_message.clone();
        incoming_call.id = "local-call:fixture-missed".into();
        incoming_call.timestamp += 1;
        worker.archive.insert_message(&incoming_call, None).unwrap();
        worker.archive.set_unread(chat, 1).unwrap();
        assert_eq!(
            worker.archive.unread_incoming(chat, 1).unwrap(),
            vec![(phone_message.id.clone(), chat.into())]
        );
        worker
            .archive
            .mark_read_to(chat, &phone_message.id)
            .unwrap();
        assert_eq!(worker.archive.chat(chat).unwrap().unwrap().unread, 0);
        assert_eq!(worker.archive.history_unread(chat, 1).unwrap(), 0);
        let creator: Jid = worker.me().parse().unwrap();
        let synced = wa::CallLogRecord {
            call_result: Some(wa::call_log_record::CallResult::CONNECTED),
            start_time: Some(row.timestamp),
            participants: vec![wa::call_log_record::ParticipantInfo {
                user_jid: Some(chat.into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        worker.file_synced_call(&synced, Some(&creator), Some(true), Some("test-call"));
        assert_eq!(worker.archive.call_records().unwrap().len(), 1);
        assert_eq!(worker.archive.call_records().unwrap()[0].duration, Some(42));
        let mut message_events = 0;
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, Event::Incoming { .. }));
            if let Event::Messages { older, .. } = event {
                assert!(
                    !older,
                    "a local call must not look like a phone-history response"
                );
                message_events += 1;
            }
        }
        assert_eq!(message_events, 1);
    }
}
