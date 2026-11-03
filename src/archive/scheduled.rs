//! Text messages scheduled for a later send, kept in the encrypted archive.
//!
//! Scheduling is client-side: unlike a server-held Telegram reminder, the
//! message only goes out while this client runs linked. Rows survive
//! restarts; the worker sends what is due on its maintenance tick and drops
//! a row once its send is filed, so a delivered message is never repeated.

use rusqlite::params;

use super::{Archive, Result};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scheduled_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat TEXT NOT NULL,
    text TEXT NOT NULL,
    quoted TEXT,
    mentions TEXT NOT NULL DEFAULT '[]',
    due_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS scheduled_messages_due ON scheduled_messages (due_at);
";

/// A text message waiting for its send time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledMessage {
    pub id: i64,
    pub chat: String,
    pub text: String,
    /// Same-chat message id this answers, if the composer held a quote.
    pub quoted: Option<String>,
    /// Phone numbers mentioned, as archived for ordinary sends.
    pub mentions: Vec<String>,
    /// Unix seconds when the worker may send it.
    pub due_at: i64,
    pub created_at: i64,
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledMessage> {
    let mentions: String = row.get(4)?;
    Ok(ScheduledMessage {
        id: row.get(0)?,
        chat: row.get(1)?,
        text: row.get(2)?,
        quoted: row.get(3)?,
        mentions: serde_json::from_str(&mentions).unwrap_or_default(),
        due_at: row.get(5)?,
        created_at: row.get(6)?,
    })
}

const COLUMNS: &str = "id, chat, text, quoted, mentions, due_at, created_at";

impl Archive {
    /// Stores a scheduled text; returns its row id for cancellation.
    pub fn schedule_message(
        &self,
        chat: &str,
        text: &str,
        quoted: Option<&str>,
        mentions: &[String],
        due_at: i64,
        created_at: i64,
    ) -> Result<i64> {
        let mentions = serde_json::to_string(mentions).unwrap_or_default();
        self.connection.execute(
            "INSERT INTO scheduled_messages (chat, text, quoted, mentions, due_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![chat, text, quoted, mentions, due_at, created_at],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Pending scheduled texts of one chat, earliest first.
    pub fn scheduled_messages(&self, chat: &str) -> Result<Vec<ScheduledMessage>> {
        let mut query = self.connection.prepare(&format!(
            "SELECT {COLUMNS} FROM scheduled_messages WHERE chat = ?1 ORDER BY due_at, id"
        ))?;
        query
            .query_map([chat], row)?
            .collect::<rusqlite::Result<_>>()
    }

    /// Drops a scheduled text before it sends; true when a row was pending.
    pub fn cancel_scheduled(&self, id: i64) -> Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM scheduled_messages WHERE id = ?1", [id])?
            > 0)
    }

    /// Deletes a scheduled row once its send is filed; never resends it.
    pub fn delete_scheduled(&self, id: i64) -> Result<()> {
        self.connection
            .execute("DELETE FROM scheduled_messages WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Every scheduled text whose time has come, earliest first.
    pub fn due_scheduled(&self, now: i64) -> Result<Vec<ScheduledMessage>> {
        let mut query = self.connection.prepare(&format!(
            "SELECT {COLUMNS} FROM scheduled_messages WHERE due_at <= ?1 ORDER BY due_at, id"
        ))?;
        query
            .query_map([now], row)?
            .collect::<rusqlite::Result<_>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive() -> Archive {
        Archive::in_memory().unwrap()
    }

    #[test]
    fn scheduled_texts_list_earliest_first_and_cancel_drops_them() {
        let archive = archive();
        let later = archive
            .schedule_message("chat", "later", None, &[], 200, 10)
            .unwrap();
        let sooner = archive
            .schedule_message("chat", "sooner", Some("quoted"), &["1".into()], 100, 10)
            .unwrap();
        archive
            .schedule_message("other", "elsewhere", None, &[], 50, 10)
            .unwrap();
        let items = archive.scheduled_messages("chat").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, sooner);
        assert_eq!(items[0].text, "sooner");
        assert_eq!(items[0].quoted.as_deref(), Some("quoted"));
        assert_eq!(items[0].mentions, vec!["1".to_owned()]);
        assert_eq!(items[1].id, later);
        assert!(archive.cancel_scheduled(sooner).unwrap());
        assert!(!archive.cancel_scheduled(sooner).unwrap());
        assert_eq!(archive.scheduled_messages("chat").unwrap().len(), 1);
    }

    #[test]
    fn only_due_texts_come_up_and_deleting_never_resends() {
        let archive = archive();
        let due = archive
            .schedule_message("chat", "due", None, &[], 100, 10)
            .unwrap();
        archive
            .schedule_message("chat", "later", None, &[], 300, 10)
            .unwrap();
        let due_items = archive.due_scheduled(100).unwrap();
        assert_eq!(due_items.len(), 1);
        assert_eq!(due_items[0].id, due);
        archive.delete_scheduled(due).unwrap();
        assert!(
            archive.due_scheduled(100).unwrap().is_empty(),
            "a sent row never comes up again"
        );
        assert_eq!(
            archive.due_scheduled(300).unwrap().len(),
            1,
            "a later text stays pending"
        );
    }
}
