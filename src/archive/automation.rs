//! Durable automation request identities prevent repeated CLI commands from sending twice.
use super::*;
use crate::automation::ChatOrder;
use sha2::{Digest, Sha256};
pub type CliWatchPage = (Vec<Message>, Option<(i64, String)>);
impl Archive {
    pub(crate) fn cli_chat_page(
        &self,
        after: Option<&str>,
        limit: usize,
        kind: Option<ChatKind>,
        order: ChatOrder,
        unread_only: bool,
        include_archived: bool,
    ) -> Result<Vec<Chat>> {
        // `after` names the last chat of the previous page; resolve its sort
        // position so pages stay stable under the chosen order. A chat that
        // vanished since restarts from the top rather than failing the list.
        let anchor: Option<(i64, String, i64)> = match after {
            Some(id) => self
                .connection
                .query_row(
                    "SELECT last_activity, name, unread FROM chats WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?,
            None => None,
        };
        // The anchor id positions the keyset; unknown ids restart from the top.
        let anchor_id = match (&anchor, after) {
            (Some(_), Some(id)) => Some(id.to_owned()),
            _ => None,
        };
        let kind_filter = kind.map(kind_name);
        // Bound values only; the ORDER BY fragments below are fixed per order.
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        params.push(Box::new(kind_filter));
        params.push(Box::new(i64::from(unread_only)));
        params.push(Box::new(i64::from(include_archived)));
        let mut sql = String::from("SELECT id FROM chats WHERE (?1 IS NULL OR kind = ?1)");
        sql.push_str(" AND (?2 = 0 OR unread > 0 OR marked_unread != 0)");
        sql.push_str(" AND (?3 = 1 OR archived = 0)");
        let order_sql = match order {
            ChatOrder::Activity => {
                if let (Some((activity, _, _)), Some(id)) = (&anchor, &anchor_id) {
                    params.push(Box::new(*activity));
                    params.push(Box::new(*activity));
                    params.push(Box::new(id.clone()));
                    sql.push_str(
                        " AND (?4 IS NULL OR last_activity < ?4 OR (last_activity = ?5 AND id > ?6))",
                    );
                }
                " ORDER BY last_activity DESC, id ASC"
            }
            ChatOrder::Name => {
                if let (Some((_, name, _)), Some(id)) = (&anchor, &anchor_id) {
                    params.push(Box::new(name.clone()));
                    params.push(Box::new(name.clone()));
                    params.push(Box::new(id.clone()));
                    sql.push_str(
                        " AND (?4 IS NULL OR name COLLATE NOCASE > ?4 OR (name COLLATE NOCASE = ?5 AND id > ?6))",
                    );
                }
                " ORDER BY name COLLATE NOCASE ASC, id ASC"
            }
            ChatOrder::Unread => {
                if let (Some((activity, _, unread)), Some(id)) = (&anchor, &anchor_id) {
                    params.push(Box::new(*unread));
                    params.push(Box::new(*unread));
                    params.push(Box::new(*activity));
                    params.push(Box::new(*activity));
                    params.push(Box::new(id.clone()));
                    sql.push_str(
                        " AND (?4 IS NULL OR unread < ?4 OR (unread = ?5 AND (last_activity < ?6 OR (last_activity = ?7 AND id > ?8)))",
                    );
                }
                " ORDER BY unread DESC, last_activity DESC, id ASC"
            }
        };
        sql.push_str(order_sql);
        sql.push_str(" LIMIT ?");
        // The LIMIT placeholder follows however many anchor values were pushed.
        let limit_index = params.len() + 1;
        sql.push_str(&limit_index.to_string());
        params.push(Box::new(limit as i64));
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|param| param.as_ref()).collect();
        let ids = self
            .connection
            .prepare(&sql)?
            .query_map(refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| self.chat(&id))
            .collect::<Result<Vec<_>>>()
            .map(|rows| rows.into_iter().flatten().collect())
    }
    pub(crate) fn cli_contact_page(
        &self,
        all: bool,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Contact>> {
        self.connection.prepare("SELECT id,full_name,first_name,push_name FROM contacts WHERE id>?1 AND (?2 OR (full_name IS NOT NULL AND full_name!='')) ORDER BY id LIMIT ?3")?
            .query_map(params![after.unwrap_or(""),all,limit as i64], |row| Ok(Contact{id:row.get(0)?,full_name:row.get(1)?,first_name:row.get(2)?,push_name:row.get(3)?}))?.collect()
    }
    pub(crate) fn cli_search_page(
        &self,
        chat: &str,
        text: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Message>, Option<i64>)> {
        let pattern = search_pattern(text);
        let entries = self.connection.prepare(&format!("SELECT m.rowid,m.chat,m.id FROM messages m JOIN chats c ON c.id=m.chat WHERE c.locked=0 AND (?1='' OR m.chat=?1) AND m.rowid<?2 AND json_valid(m.content) AND {SEARCHED_TEXT} LIKE ?3 ESCAPE '\\' ORDER BY m.rowid DESC LIMIT ?4"))?
            .query_map(params![chat,before.unwrap_or(i64::MAX),pattern,limit as i64], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<Result<Vec<_>>>()?;
        let cursor = entries.last().map(|(rowid, _, _)| *rowid);
        let rows = entries
            .into_iter()
            .map(|(_, chat, id)| self.message(&chat, &id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok((rows, cursor))
    }
    pub(crate) fn cli_watch_account(
        &self,
        since: i64,
        after: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Message>, Option<i64>)> {
        let entries = self.connection.prepare("SELECT rowid,chat,id FROM messages WHERE rowid>?1 AND timestamp>=?2 AND from_me=0 ORDER BY rowid LIMIT ?3")?
            .query_map(params![after.unwrap_or(0),since,limit as i64], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<Result<Vec<_>>>()?;
        let cursor = entries.last().map(|(rowid, _, _)| *rowid).or(after);
        let rows = entries
            .into_iter()
            .map(|(_, chat, id)| self.message(&chat, &id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok((rows, cursor))
    }
    fn cli_schema(&self) -> Result<()> {
        self.connection.execute_batch("CREATE TABLE IF NOT EXISTS automation_sends(request_id TEXT PRIMARY KEY,chat TEXT NOT NULL,digest TEXT NOT NULL,message_id TEXT NOT NULL); CREATE TABLE IF NOT EXISTS automation_batches(request_id TEXT PRIMARY KEY,chat TEXT NOT NULL,digest TEXT NOT NULL,message_ids TEXT NOT NULL);")
    }
    pub(crate) fn cli_existing_batch(
        &self,
        request: &str,
        chat: &str,
        digest: Option<&str>,
    ) -> Result<Option<Vec<String>>> {
        self.cli_schema()?;
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT chat,digest,message_ids FROM automation_batches WHERE request_id=?1",
                [request],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((old_chat, old_digest, ids)) = row {
            if old_chat != chat || digest.is_some_and(|digest| digest != old_digest) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(Some(
                serde_json::from_str(&ids).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn cli_reserve_batch(
        &self,
        request: &str,
        chat: &str,
        digest: &str,
        ids: &[String],
    ) -> Result<()> {
        self.cli_schema()?;
        self.connection.execute("INSERT INTO automation_batches(request_id,chat,digest,message_ids) VALUES(?1,?2,?3,?4)",params![request,chat,digest,serde_json::to_string(ids).map_err(|_|rusqlite::Error::InvalidQuery)?])?;
        Ok(())
    }
    pub fn cli_existing_send(
        &self,
        request: &str,
        chat: &str,
        text: Option<&str>,
    ) -> Result<Option<String>> {
        self.cli_schema()?;
        if self.cli_existing_batch(request, chat, None)?.is_some() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT chat,digest,message_id FROM automation_sends WHERE request_id=?1",
                [request],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((old_chat, digest, id)) = row {
            if old_chat != chat
                || text.is_some_and(|text| {
                    Sha256::digest(text.as_bytes())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                        != digest
                })
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }
    pub fn cli_reserve_send(&self, request: &str, chat: &str, text: &str, id: &str) -> Result<()> {
        self.cli_schema()?;
        self.connection.execute(
            "INSERT INTO automation_sends(request_id,chat,digest,message_id) VALUES (?1,?2,?3,?4)",
            params![
                request,
                chat,
                Sha256::digest(text.as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                id
            ],
        )?;
        Ok(())
    }
    pub fn cli_watch(
        &self,
        chat: &str,
        cursor: Option<&(i64, String)>,
        limit: usize,
    ) -> Result<CliWatchPage> {
        let after = if let Some((rowid, id)) = cursor {
            let valid: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE chat=?1 AND id=?2 AND rowid=?3)",
                params![chat, id, rowid],
                |r| r.get(0),
            )?;
            if !valid {
                return Err(rusqlite::Error::InvalidQuery);
            }
            *rowid
        } else {
            0
        };
        let entries: Vec<(i64, String)> = self
            .connection
            .prepare(
                "SELECT rowid,id FROM messages WHERE chat=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3",
            )?
            .query_map(params![chat, after, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_>>()?;
        let next = entries.last().cloned().or_else(|| cursor.cloned());
        let rows = entries
            .into_iter()
            .map(|(_, id)| self.message(chat, &id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok((rows, next))
    }
    pub fn cli_exportable(&self, chat: &str, id: &str) -> Result<bool> {
        let disappearing:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM disappearing_messages WHERE chat=?1 AND id=?2 AND expires_at IS NOT NULL)",params![chat,id],|r|r.get(0))?;
        Ok(!disappearing)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachment_identity_preserves_all_children_and_rejects_payload_or_chat_changes() {
        let archive = Archive::in_memory().unwrap();
        let ids = vec!["synthetic-first".into(), "synthetic-second".into()];
        archive
            .cli_reserve_batch("fixture", "chat", "digest", &ids)
            .unwrap();
        assert_eq!(
            archive
                .cli_existing_batch("fixture", "chat", Some("digest"))
                .unwrap(),
            Some(ids)
        );
        assert!(
            archive
                .cli_existing_batch("fixture", "chat", Some("changed"))
                .is_err()
        );
        assert!(
            archive
                .cli_existing_batch("fixture", "other", None)
                .is_err()
        );
        assert!(archive.cli_existing_send("fixture", "chat", None).is_err());
        assert!(
            archive
                .cli_reserve_batch("fixture", "chat", "digest", &[])
                .is_err()
        );
    }
    #[test]
    fn repeated_request_ids_preserve_send_identity_and_reject_changed_payload() {
        let a = Archive::in_memory().unwrap();
        a.cli_reserve_send("fixture", "chat", "synthetic body", "original")
            .unwrap();
        assert_eq!(
            a.cli_existing_send("fixture", "chat", Some("synthetic body"))
                .unwrap(),
            Some("original".into())
        );
        assert!(
            a.cli_existing_send("fixture", "chat", Some("changed synthetic body"))
                .is_err()
        );
        assert!(a.cli_existing_send("fixture", "other-chat", None).is_err());
    }
}
