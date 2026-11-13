//! Bounded archive reads and idempotent text sends. No raw protocol/key/media export.
use super::*;
use crate::automation::{Pending, Request, error};
impl Worker {
    pub(super) fn automation(&mut self, pending: Pending) {
        if !pending.request.valid()
            || pending.request.account() != self.dirs.id.as_str()
            || pending.blocked()
        {
            pending.answer(error("Application locked or request expired"));
            return;
        }
        let chat = pending.request.chat().to_owned();
        let collection = match pending.request.base() {
            Request::Chats { after, limit, .. } => Some(self.archive.cli_chat_page(after.as_deref(), *limit).map(|rows| {
                let next = rows.last().map(|row| row.id.clone());
                let rows:Vec<_> = rows.into_iter().filter(|row| !row.locked && row.disappearing_timer().is_none()).map(|row|serde_json::json!({"id":row.id,"name":row.name,"kind":row.kind,"unread":row.unread,"last_activity":row.last_activity})).collect();
                serde_json::json!({"chats":rows,"cursor":next}).to_string()
            })),
            Request::Contacts { all, after, limit, .. } => Some(self.archive.cli_contact_page(*all,after.as_deref(),*limit).map(|rows| {
                let next = rows.last().map(|row|row.id.clone());
                let rows:Vec<_> = rows.into_iter().filter(|row| match self.archive.chat(&self.canonical_str(&row.id)) { Ok(Some(chat)) => !chat.locked && chat.disappearing_timer().is_none(), Ok(None) => true, Err(_) => false }).map(|row|serde_json::json!({"id":row.id,"full_name":row.full_name,"push_name":row.push_name,"address_book":row.full_name.as_ref().is_some_and(|name|!name.is_empty())})).collect();
                serde_json::json!({"contacts":rows,"cursor":next}).to_string()
            })),
            Request::WatchAccount { since, after, limit, .. } => Some(self.archive.cli_watch_account(*since,*after,*limit).map(|(rows,cursor)|serde_json::json!({"messages":self.cli_message_values(rows),"cursor":cursor}).to_string())),
            Request::Search { text, before, limit, .. } => Some(self.archive.cli_search_page(&chat,text,*before,*limit).map(|(rows,cursor)|serde_json::json!({"messages":self.cli_message_values(rows),"cursor":cursor}).to_string())),
            _ => None,
        };
        if let Some(reply) = collection {
            self.cli_answer(
                &pending,
                reply.unwrap_or_else(|_| error("Could not read archive")),
            );
            return;
        }
        let known = match self.archive.chat(&chat) {
            Ok(Some(known)) => known,
            _ => {
                pending.answer(error("Unknown conversation"));
                return;
            }
        };
        if known.locked || known.disappearing_timer().is_some() {
            pending.answer(error(
                "Locked or disappearing conversations are unavailable to automation",
            ));
            return;
        }
        let reply = match pending.request.base() {
            Request::Fragment { .. }
            | Request::Chats { .. }
            | Request::Contacts { .. }
            | Request::Search { .. }
            | Request::WatchAccount { .. } => unreachable!("collection handled above"),
            Request::SendFiles {
                request_id,
                text,
                files,
                ..
            } => self.cli_send_files(&pending, &chat, request_id, text, files),
            Request::Read { before, limit, .. } => self
                .archive
                .messages(
                    &chat,
                    before.as_ref().map(|(time, id)| (*time, id.as_str())),
                    *limit,
                )
                .map(|rows| self.cli_rows(&chat, rows))
                .unwrap_or_else(|_| error("Could not read archive")),
            Request::Watch { after, limit, .. } => {
                // Polls archived arrival order, including late history. The cursor is archive rowid,
                // carried as the first tuple value; the second is the message id for validation.
                self.archive.cli_watch(&chat,after.as_ref(),*limit)
                    .map(|(rows,cursor)|serde_json::json!({"page":serde_json::from_str::<serde_json::Value>(&self.cli_rows(&chat,rows)).unwrap_or_default(),"cursor":cursor}).to_string())
                    .unwrap_or_else(|_|error("Invalid cursor or archive read failed"))
            }
            Request::Send {
                request_id, text, ..
            } => {
                if self.archive_only || !self.link_up() || !self.destination_writable(&chat) {
                    error("Account offline or conversation read-only")
                } else if let Ok(Some(id)) =
                    self.archive
                        .cli_existing_send(request_id, &chat, Some(text))
                {
                    self.cli_status(&chat, request_id, &id)
                } else if self
                    .archive
                    .cli_existing_send(request_id, &chat, Some(text))
                    .is_err()
                {
                    error("Request id already belongs to different content")
                } else if pending.blocked() {
                    error("Application locked or request expired")
                } else if let Some(Some(id)) = pending.permit.execute(|| {
                    if pending.expired() {
                        return None;
                    }
                    self.send_text_inner(
                        chat.clone(),
                        text.clone(),
                        None,
                        Vec::new(),
                        Some(request_id.clone()),
                    )
                }) {
                    self.cli_status(&chat, request_id, &id)
                } else {
                    error("Send was not confirmed. Query request_id; never repeat with a new id")
                }
            }
            Request::Status { request_id, .. } => {
                if let Ok(Some(ids)) = self.archive.cli_existing_batch(request_id, &chat, None) {
                    self.cli_batch_status(&chat, request_id, &ids)
                } else {
                    match self.archive.cli_existing_send(request_id, &chat, None) {
                        Ok(Some(id)) => self.cli_status(&chat, request_id, &id),
                        Ok(None) => {
                            serde_json::json!({"request_id":request_id,"status":"not_found"})
                                .to_string()
                        }
                        Err(_) => error("Request id belongs to another conversation"),
                    }
                }
            }
        };
        self.cli_answer(&pending, reply);
    }
    fn cli_answer(&self, pending: &Pending, reply: String) {
        pending.answer(reply);
    }
    fn cli_rows(&self, chat: &str, rows: Vec<Message>) -> String {
        let before = rows.first().map(|row| (row.timestamp, row.id.clone()));
        let _ = chat;
        let rows = self.cli_message_values(rows);
        serde_json::json!({"messages":rows,"next_before":before}).to_string()
    }
    fn cli_message_values(&self, rows: Vec<Message>) -> Vec<serde_json::Value> {
        rows.into_iter().filter(|row| !matches!(row.content,Content::PhoneOnly{..}) && self.archive.cli_exportable(&row.chat,&row.id).unwrap_or(false)
            && self.archive.chat(&row.chat).ok().flatten().is_some_and(|chat|!chat.locked && chat.disappearing_timer().is_none()))
            .map(|row| {
                let kind = serde_json::to_value(&row.content).ok().and_then(|value|value.get("kind").cloned());
                let text = match &row.content {
                    Content::Text { text,.. } | Content::Interactive { text,.. } => text.clone(),
                    Content::Image { caption,.. } | Content::Video { caption,.. } | Content::Document { caption,.. } => caption.clone().unwrap_or_default(),
                    _ => row.content.full_summary(),
                };
                serde_json::json!({"chat":row.chat,"id":row.id,"sender":row.sender,"sender_name":row.sender_name,"timestamp":row.timestamp,"from_me":row.from_me,"kind":kind,"text":text,"status":row.status})
            }).collect()
    }
    fn cli_batch_status(&self, chat: &str, request: &str, ids: &[String]) -> String {
        let rows: Vec<_> = ids
            .iter()
            .map(|id| {
                serde_json::from_str::<serde_json::Value>(&self.cli_status(chat, request, id))
                    .unwrap_or_default()
            })
            .collect();
        serde_json::json!({"request_id":request,"messages":rows}).to_string()
    }
    fn cli_send_files(
        &mut self,
        pending: &Pending,
        chat: &str,
        request: &str,
        text: &str,
        files: &[crate::automation::Attachment],
    ) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(serde_json::to_vec(&(text, files)).unwrap_or_default())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        match self
            .archive
            .cli_existing_batch(request, chat, Some(&digest))
        {
            Ok(Some(ids)) => return self.cli_batch_status(chat, request, &ids),
            Err(_) => return error("Request id already belongs to different content"),
            _ => {}
        }
        if self.archive.cli_existing_send(request, chat, None).is_err()
            || self
                .archive
                .cli_existing_send(request, chat, None)
                .ok()
                .flatten()
                .is_some()
        {
            return error("Request id already belongs to different content");
        }
        if self.archive_only
            || !self.link_up()
            || !self.destination_writable(chat)
            || pending.blocked()
        {
            return error("Account offline, locked or conversation read-only");
        }
        let Some(client) = self.client.clone() else {
            return error("Account offline");
        };
        let ids: Vec<_> = files
            .iter()
            .map(|_| client.generate_message_id().into_string())
            .collect();
        if pending.permit.execute(|| {
            !pending.expired()
                && self
                    .archive
                    .cli_reserve_batch(request, chat, &digest, &ids)
                    .is_ok()
        }) != Some(true)
        {
            return error("Could not reserve attachment send");
        }
        for (index, (file, id)) in files.iter().cloned().zip(ids.iter().cloned()).enumerate() {
            let client = client.clone();
            let commands = self.commands.clone();
            let chat = chat.to_owned();
            let me = self.me();
            let dir = self.dirs.media_cache_dir();
            let caption = (index == 0 && !text.trim().is_empty()).then(|| text.to_owned());
            let permit = pending.permit.clone();
            let locked = pending.locked.clone();
            self.spawn_upload(chat.clone(), async move {
                let allowed =
                    || !permit.blocked() && !locked.load(std::sync::atomic::Ordering::SeqCst);
                if !allowed() {
                    return;
                }
                let outcome = async {
                    use tokio::io::AsyncReadExt;
                    let source = tokio::fs::File::open(&file.path).await.map_err(|_| ())?;
                    let metadata = source.metadata().await.map_err(|_| ())?;
                    if !metadata.is_file() || metadata.len() != file.size {
                        return Err(());
                    }
                    let mut bytes = Vec::new();
                    source
                        .take(crate::automation::FILE_LIMIT + 1)
                        .read_to_end(&mut bytes)
                        .await
                        .map_err(|_| ())?;
                    let actual = Sha256::digest(&bytes)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    if bytes.len() as u64 != file.size || actual != file.sha256 || !allowed() {
                        return Err(());
                    }
                    // Documents preserve captions for every file type, including audio.
                    let name = file.path.file_name().and_then(|name| name.to_str());
                    let prepared =
                        prepare_media(&client, bytes, "application/octet-stream", name, false)
                            .await
                            .map_err(|_| ())?;
                    if !allowed() {
                        return Err(());
                    }
                    file_outbound(&client, &chat, &me, &dir, prepared, caption, Vec::new())
                        .await
                        .map_err(|_| ())
                }
                .await;
                if let Ok((mut row, raw)) = outcome {
                    row.id = id;
                    let _ = commands.send(Command::AutomationOutbound {
                        permit,
                        locked,
                        chat,
                        row: Box::new(row),
                        raw,
                        upload: transfers::upload_token(),
                    });
                }
            });
        }
        self.cli_batch_status(chat, request, &ids)
    }
    fn cli_status(&self, chat: &str, request: &str, id: &str) -> String {
        let status = self
            .archive
            .message(chat, id)
            .ok()
            .flatten()
            .map(|row| row.status)
            .unwrap_or(Delivery::Unconfirmed);
        serde_json::json!({"request_id":request,"message_id":id,"status":status}).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_collection_pages_filter_private_rows_and_advance_raw_cursors() {
        let (mut worker, _, _, _) = super::super::offline_worker("fixture@s.whatsapp.net");
        for (chat, id, text) in [
            ("1@s.whatsapp.net", "visible", "Synthetic visible"),
            ("2@s.whatsapp.net", "hidden", "Synthetic secret"),
        ] {
            worker
                .archive
                .ensure_chat(chat, "Synthetic contact")
                .unwrap();
            let mut row = crate::archive::tests::message(chat, id, 10, false);
            row.content = Content::text(text);
            worker.archive.insert_message(&row, None).unwrap();
        }
        worker.archive.set_locked("2@s.whatsapp.net", true).unwrap();
        let (pending, received) = request(Request::Chats {
            account: "1".into(),
            after: None,
            limit: 25,
        });
        worker.automation(pending);
        let value: serde_json::Value = serde_json::from_str(&received.recv().unwrap()).unwrap();
        assert_eq!(value["chats"].as_array().unwrap().len(), 1);
        assert_eq!(value["cursor"], "2@s.whatsapp.net");
        let (pending, received) = request(Request::Search {
            account: "1".into(),
            chat: String::new(),
            text: "Synthetic".into(),
            before: None,
            limit: 25,
        });
        worker.automation(pending);
        let reply = received.recv().unwrap();
        assert!(reply.contains("Synthetic visible"));
        assert!(!reply.contains("Synthetic secret"));
        let (pending, received) = request(Request::WatchAccount {
            account: "1".into(),
            since: 0,
            after: Some(1),
            limit: 1,
        });
        worker.automation(pending);
        let value: serde_json::Value = serde_json::from_str(&received.recv().unwrap()).unwrap();
        assert!(value["messages"].as_array().unwrap().is_empty());
        assert_eq!(value["cursor"], 2);
    }
    #[tokio::test]
    async fn attachment_outbound_is_refused_after_permit_revocation() {
        let (mut worker, _, _, _) = super::super::offline_worker("fixture@s.whatsapp.net");
        let chat = "1@s.whatsapp.net";
        worker.archive.ensure_chat(chat, "Synthetic").unwrap();
        let mut row = crate::archive::tests::message(chat, "revoked-file", 10, true);
        row.sender = worker.me();
        let permit = std::sync::Arc::new(crate::automation::Permit::allowed());
        permit.revoke();
        worker
            .handle_command(Command::AutomationOutbound {
                permit,
                locked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                chat: chat.into(),
                row: Box::new(row),
                raw: Vec::new(),
                upload: None,
            })
            .await;
        assert!(
            worker
                .archive
                .message(chat, "revoked-file")
                .unwrap()
                .is_none()
        );
    }
    fn request(request: Request) -> (Pending, std::sync::mpsc::Receiver<String>) {
        let (reply, received) = std::sync::mpsc::channel();
        (
            Pending {
                request,
                permit: std::sync::Arc::new(crate::automation::Permit::allowed()),
                reply,
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                locked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            received,
        )
    }
    #[tokio::test]
    async fn sdk_text_send_is_queued_once_and_uncertain_completion_never_retries() {
        let directory = tempfile::tempdir().unwrap();
        let store = whatsapp_rust::store::SqliteStore::open(
            &directory
                .path()
                .join("synthetic-session.db")
                .to_string_lossy(),
        )
        .await
        .unwrap();
        let bot = Bot::builder().with_backend(store).build().await.unwrap();
        let (mut worker, _events, _commands, _wa) =
            super::super::offline_worker("9@s.whatsapp.net");
        worker.client = Some(bot.client());
        // The official SDK is built but never connected. This existing queue hook intercepts all transport starts.
        worker.outgoing.synthetic = Some(Vec::new());
        let chat = "1@s.whatsapp.net";
        worker
            .archive
            .ensure_chat(chat, "Synthetic contact")
            .unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let original = Request::Send {
            account: "1".into(),
            chat: chat.into(),
            request_id: token.into(),
            text: "Synthetic CLI send".into(),
        };
        let (pending, received) = request(original.clone());
        worker.handle_command(Command::Automation(pending)).await;
        let reply: serde_json::Value = serde_json::from_str(&received.recv().unwrap()).unwrap();
        let id = reply["message_id"].as_str().unwrap().to_owned();
        assert_eq!(worker.outgoing.synthetic.as_ref().unwrap().len(), 1);
        let row = worker.archive.message(chat, &id).unwrap().unwrap();
        assert_eq!(row.content, Content::text("Synthetic CLI send"));
        assert_eq!(
            worker
                .archive
                .cli_existing_send(token, chat, Some("Synthetic CLI send"))
                .unwrap(),
            Some(id.clone())
        );
        let (pending, received) = request(original.clone());
        worker.handle_command(Command::Automation(pending)).await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&received.recv().unwrap()).unwrap()["message_id"],
            id
        );
        assert_eq!(worker.outgoing.synthetic.as_ref().unwrap().len(), 1);
        worker
            .handle_command(Command::OutgoingFinished {
                chat: chat.into(),
                id: id.clone(),
                result: Err(crate::backend::SendFailure::Failed {
                    kind: "iq timeout",
                    code: None,
                }),
            })
            .await;
        assert_eq!(
            worker.archive.message(chat, &id).unwrap().unwrap().status,
            Delivery::Unconfirmed
        );
        let (pending, received) = request(original);
        worker.handle_command(Command::Automation(pending)).await;
        assert!(received.recv().unwrap().contains("unconfirmed"));
        assert_eq!(worker.outgoing.synthetic.as_ref().unwrap().len(), 1);
        let altered = Request::Send {
            account: "1".into(),
            chat: chat.into(),
            request_id: token.into(),
            text: "Altered synthetic content".into(),
        };
        let (pending, received) = request(altered);
        worker.handle_command(Command::Automation(pending)).await;
        assert!(received.recv().unwrap().contains("error"));
        assert_eq!(worker.outgoing.synthetic.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn misrouted_account_and_revoked_read_are_refused_without_content() {
        let (mut worker, _events, _commands, _wa) =
            super::super::offline_worker("9@s.whatsapp.net");
        let chat = "1@s.whatsapp.net";
        worker.archive.ensure_chat(chat, "Synthetic").unwrap();
        worker
            .archive
            .insert_message(
                &crate::archive::tests::message(chat, "private-visible", 10, false),
                None,
            )
            .unwrap();
        let (pending, received) = request(Request::Read {
            account: "2".into(),
            chat: chat.into(),
            before: None,
            limit: 1,
        });
        worker.automation(pending);
        assert!(!received.recv().unwrap().contains("private-visible"));
        let (pending, received) = request(Request::Read {
            account: "1".into(),
            chat: chat.into(),
            before: None,
            limit: 1,
        });
        pending.permit.revoke();
        worker.automation(pending);
        assert!(!received.recv().unwrap().contains("private-visible"));
    }

    #[test]
    fn worker_read_and_watch_preserve_lock_expiry_and_phone_only_privacy() {
        let (mut worker, _events, _commands, _wa) =
            super::super::offline_worker("fixture@s.whatsapp.net");
        let chat = "1@s.whatsapp.net";
        worker.archive.ensure_chat(chat, "Synthetic chat").unwrap();
        let row = crate::archive::tests::message(chat, "visible", 10, false);
        worker.archive.insert_message(&row, None).unwrap();
        let mut private = row.clone();
        private.id = "disappearing".into();
        private.content = Content::text("synthetic disappearing secret");
        worker.archive.insert_message(&private, None).unwrap();
        worker
            .archive
            .remember_message_expiry(chat, &private.id, 999999)
            .unwrap();
        let (pending, received) = request(Request::Read {
            account: "1".into(),
            chat: chat.into(),
            before: None,
            limit: 10,
        });
        worker.automation(pending);
        let reply = received.recv().unwrap();
        assert!(reply.contains("visible"));
        assert!(!reply.contains("synthetic disappearing secret"));
        worker.archive.set_locked(chat, true).unwrap();
        let (pending, received) = request(Request::Watch {
            account: "1".into(),
            chat: chat.into(),
            after: None,
            limit: 10,
        });
        worker.automation(pending);
        let reply = received.recv().unwrap();
        assert!(reply.contains("error"));
        assert!(!reply.contains("visible"));
    }
}
