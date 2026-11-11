use crate::error::{ApiError, ApiResult};
use rusqlite::{Connection, params};
use spark_protocol::{Channel, ChatMessage, FileInfo};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Store {
    db: Mutex<Connection>,
    pub files_dir: PathBuf,
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
impl Store {
    pub fn open(root: &Path) -> ApiResult<Self> {
        std::fs::create_dir_all(root.join("files"))?;
        let db = Connection::open(root.join("history.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS channels (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, channel_id TEXT NOT NULL, created_at INTEGER NOT NULL UNIQUE, data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS messages_channel_time ON messages(channel_id,created_at);
            CREATE TABLE IF NOT EXISTS files (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            INSERT OR IGNORE INTO channels VALUES ('general','Общий','text');
            INSERT OR IGNORE INTO channels VALUES ('lounge','Голосовая','voice');")?;
        // Only private, generated temporary names are deleted after interrupted transfers.
        for entry in std::fs::read_dir(root.join("files"))? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|v| v.starts_with(".upload-") && v.ends_with(".part"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(Self {
            db: Mutex::new(db),
            files_dir: root.join("files"),
        })
    }
    pub fn channels(&self) -> ApiResult<Vec<Channel>> {
        let db = self.db.lock().map_err(ApiError::internal)?;
        let mut statement = db.prepare("SELECT id,name,kind FROM channels ORDER BY rowid")?;
        Ok(statement
            .query_map([], |r| {
                Ok(Channel {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }
    pub fn channel(&self, id: &str, kind: &str) -> ApiResult<()> {
        if self
            .channels()?
            .iter()
            .any(|c| c.id == id && c.kind == kind)
        {
            Ok(())
        } else {
            Err(ApiError::bad(
                "Канал не найден или не поддерживает это действие",
            ))
        }
    }
    pub fn create_channel(&self, name: String, kind: String) -> ApiResult<Channel> {
        let db = self.db.lock().map_err(ApiError::internal)?;
        let count: i64 = db.query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))?;
        if count >= 128 {
            return Err(ApiError::bad("Достигнут предел: 128 каналов"));
        }
        let channel = Channel {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            kind,
        };
        db.execute(
            "INSERT INTO channels(id,name,kind) VALUES (?1,?2,?3)",
            params![channel.id, channel.name, channel.kind],
        )?;
        Ok(channel)
    }
    pub fn history(
        &self,
        channel: Option<&str>,
        before: Option<i64>,
    ) -> ApiResult<Vec<ChatMessage>> {
        let db = self.db.lock().map_err(ApiError::internal)?;
        let mut statement = db.prepare("SELECT data FROM messages WHERE (?1 IS NULL OR channel_id=?1) AND created_at < ?2 ORDER BY created_at DESC LIMIT 200")?;
        let rows = statement
            .query_map(params![channel, before.unwrap_or(i64::MAX)], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut messages = rows
            .into_iter()
            .map(|s| serde_json::from_str(&s).map_err(ApiError::from))
            .collect::<ApiResult<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }
    pub fn insert_message(&self, mut message: ChatMessage) -> ApiResult<ChatMessage> {
        let mut db = self.db.lock().map_err(ApiError::internal)?;
        let transaction = db.transaction()?;
        let previous: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(created_at),0) FROM messages",
            [],
            |r| r.get(0),
        )?;
        message.created_at = now_ms().max(previous.saturating_add(1));
        if let Some(file) = &message.file {
            transaction.execute(
                "INSERT INTO files(id,data) VALUES (?1,?2)",
                params![file.id, serde_json::to_string(file)?],
            )?;
        }
        transaction.execute(
            "INSERT INTO messages(id,channel_id,created_at,data) VALUES (?1,?2,?3,?4)",
            params![
                message.id,
                message.channel_id,
                message.created_at,
                serde_json::to_string(&message)?
            ],
        )?;
        transaction.commit()?;
        Ok(message)
    }
    pub fn file(&self, id: &str) -> ApiResult<FileInfo> {
        crate::validation::id(id)?;
        let db = self.db.lock().map_err(ApiError::internal)?;
        let data: String = db
            .query_row("SELECT data FROM files WHERE id=?1", [id], |r| r.get(0))
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ApiError::new(axum::http::StatusCode::NOT_FOUND, "Файл не найден")
                }
                e => ApiError::from(e),
            })?;
        Ok(serde_json::from_str(&data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paged_history_does_not_lose_messages_with_the_same_clock_tick() {
        let root = tempfile::TempDir::new().unwrap();
        let store = Store::open(root.path()).unwrap();
        for i in 0..205 {
            store
                .insert_message(ChatMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    channel_id: "general".into(),
                    sender_id: "peer".into(),
                    sender_name: "Peer".into(),
                    text: i.to_string(),
                    created_at: 0,
                    file: None,
                })
                .unwrap();
        }
        let latest = store.history(Some("general"), None).unwrap();
        assert_eq!(latest.len(), 200);
        assert_eq!(latest.first().unwrap().text, "5");
        assert_eq!(latest.last().unwrap().text, "204");
        assert!(
            latest
                .windows(2)
                .all(|pair| pair[0].created_at < pair[1].created_at)
        );
        let older = store
            .history(Some("general"), Some(latest[0].created_at))
            .unwrap();
        assert_eq!(
            older.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["0", "1", "2", "3", "4"]
        );
        drop(store);
        assert_eq!(
            Store::open(root.path())
                .unwrap()
                .history(None, None)
                .unwrap(),
            latest
        );
    }
}
