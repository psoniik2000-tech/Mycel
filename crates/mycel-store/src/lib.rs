//! Encrypted local storage (DESIGN.md §10).

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub struct Store {
    db: Connection,
}

impl Store {
    /// Opens (or creates) `dir/mycel.db`, keyed by Argon2id(password, salt).
    /// The app keeps the password in the OS keystore (DPAPI / Android Keystore).
    pub fn open(dir: &Path, password: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let hex = derive(dir, password)?;
        let db = Connection::open(dir.join("mycel.db"))?;
        db.pragma_update(None, "key", format!("x'{hex}'"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS events (
                 id BLOB PRIMARY KEY, lamport INTEGER NOT NULL, ts INTEGER NOT NULL, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS envelopes (id BLOB PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS peers (id BLOB PRIMARY KEY, addr BLOB NOT NULL);",
        )
        .context("wrong password or corrupt database")?;
        // Receive time for retention (§7.3); fails harmlessly once the column exists.
        if db
            .execute(
                "ALTER TABLE envelopes ADD COLUMN ts INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .is_ok()
        {
            db.execute("UPDATE envelopes SET ts = ?1", [now_ms()])?;
        }
        // Chat of an event: empty = the space chat, else the 1:1 partner's device id.
        let _ = db.execute(
            "ALTER TABLE events ADD COLUMN chat BLOB NOT NULL DEFAULT x''",
            [],
        );
        Ok(Self { db })
    }

    /// Re-encrypts the database under a new password (the salt stays).
    pub fn rekey(&self, dir: &Path, password: &str) -> Result<()> {
        self.db
            .pragma_update(None, "rekey", format!("x'{}'", derive(dir, password)?))?;
        Ok(())
    }

    pub fn meta(&self, k: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, k: &str, v: &[u8]) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO meta (k, v) VALUES (?1, ?2)",
            params![k, v],
        )?;
        Ok(())
    }

    /// Deletes the key; true if it existed (one-time tokens).
    pub fn take_meta(&self, k: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM meta WHERE k = ?1", [k])? == 1)
    }

    pub fn insert_envelope(&self, id: &[u8], data: &[u8]) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO envelopes (id, data, ts) VALUES (?1, ?2, ?3)",
            params![id, data, now_ms()],
        )?;
        Ok(())
    }

    pub fn has_envelope(&self, id: &[u8]) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM envelopes WHERE id = ?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// All (id, data) envelopes.
    pub fn envelopes(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut st = self.db.prepare("SELECT id, data FROM envelopes")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns false if the event was already stored.
    pub fn insert_event(
        &self,
        id: &[u8],
        chat: &[u8],
        lamport: u64,
        ts: u64,
        data: &[u8],
    ) -> Result<bool> {
        let n = self.db.execute(
            "INSERT OR IGNORE INTO events (id, chat, lamport, ts, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, chat, lamport as i64, ts as i64, data],
        )?;
        Ok(n == 1)
    }

    /// (chat, data) of an event.
    pub fn event(&self, id: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self
            .db
            .query_row("SELECT chat, data FROM events WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    }

    /// Removes an event; returns its data if it was here.
    pub fn take_event(&self, id: &[u8]) -> Result<Option<Vec<u8>>> {
        let data = self
            .db
            .query_row("SELECT data FROM events WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        self.db.execute("DELETE FROM events WHERE id = ?1", [id])?;
        Ok(data)
    }

    /// Events of one chat, oldest first.
    pub fn events(&self, chat: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut st = self
            .db
            .prepare("SELECT data FROM events WHERE chat = ?1 ORDER BY lamport, ts, id")?;
        let rows = st.query_map([chat], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// (chat, data) of every event, oldest first.
    pub fn all_events(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut st = self
            .db
            .prepare("SELECT chat, data FROM events ORDER BY lamport, ts, id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Chats that have events.
    pub fn chats(&self) -> Result<Vec<Vec<u8>>> {
        let mut st = self.db.prepare("SELECT DISTINCT chat FROM events")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// (id, lamport) of the latest event of a chat.
    pub fn head(&self, chat: &[u8]) -> Result<Option<(Vec<u8>, u64)>> {
        Ok(self
            .db
            .query_row(
                "SELECT id, lamport FROM events WHERE chat = ?1 ORDER BY lamport DESC, ts DESC LIMIT 1",
                [chat],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
    }

    /// Deletes envelopes received and events written before `before_ms` (anchor retention).
    pub fn prune(&self, before_ms: i64) -> Result<usize> {
        Ok(self
            .db
            .execute("DELETE FROM envelopes WHERE ts < ?1", [before_ms])?
            + self
                .db
                .execute("DELETE FROM events WHERE ts < ?1", [before_ms])?)
    }

    pub fn upsert_peer(&self, id: &[u8], addr: &[u8]) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO peers (id, addr) VALUES (?1, ?2)",
            params![id, addr],
        )?;
        Ok(())
    }

    pub fn has_peer(&self, id: &[u8]) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM peers WHERE id = ?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn peer(&self, id: &[u8]) -> Result<Vec<u8>> {
        Ok(self
            .db
            .query_row("SELECT addr FROM peers WHERE id = ?1", [id], |r| r.get(0))?)
    }

    pub fn peers(&self) -> Result<Vec<Vec<u8>>> {
        let mut st = self.db.prepare("SELECT addr FROM peers")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Hex SQLCipher key: Argon2id(password, `dir/salt`), the salt made on first use.
fn derive(dir: &Path, password: &str) -> Result<String> {
    let salt_path = dir.join("salt");
    let salt = match std::fs::read(&salt_path) {
        Ok(s) => s,
        Err(_) => {
            let mut s = vec![0u8; 16];
            getrandom::fill(&mut s).map_err(|e| anyhow::anyhow!("rng: {e}"))?;
            std::fs::write(&salt_path, &s)?;
            s
        }
    };
    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(key.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_rejects_wrong_password() {
        let dir = std::env::temp_dir().join(format!("mycel-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = Store::open(&dir, "pw").unwrap();
            assert!(s.insert_event(b"a", b"", 1, 1, b"x").unwrap());
            assert!(!s.insert_event(b"a", b"", 1, 1, b"x").unwrap());
            assert!(s.insert_event(b"b", b"dm", 2, 1, b"y").unwrap());
        }
        let s = Store::open(&dir, "pw").unwrap();
        assert_eq!(s.events(b"").unwrap(), vec![b"x".to_vec()]);
        assert_eq!(
            s.event(b"b").unwrap(),
            Some((b"dm".to_vec(), b"y".to_vec()))
        );
        drop(s);
        assert!(Store::open(&dir, "bad").is_err());
        let s = Store::open(&dir, "pw").unwrap();
        s.insert_envelope(b"e", b"y").unwrap();
        assert_eq!(s.prune(2).unwrap(), 2); // old events ts=1 go, fresh envelope stays
        assert_eq!(s.prune(now_ms() + 1).unwrap(), 1);
        assert!(s.envelopes().unwrap().is_empty());
        s.set_meta("k", b"v").unwrap();
        s.rekey(&dir, "new").unwrap();
        drop(s);
        assert!(Store::open(&dir, "pw").is_err());
        let s = Store::open(&dir, "new").unwrap();
        assert_eq!(s.meta("k").unwrap(), Some(b"v".to_vec()));
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
