//! Encrypted local storage (DESIGN.md §10).

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub struct Store {
    db: Connection,
}

impl Store {
    /// Opens (or creates) `dir/mycel.db`, keyed by Argon2id(password, salt).
    // shortcut: key derived from a CLI password, no OS keystore; wrap with DPAPI/Keystore in the app phase.
    pub fn open(dir: &Path, password: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
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
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();

        let db = Connection::open(dir.join("mycel.db"))?;
        db.pragma_update(None, "key", format!("x'{hex}'"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS events (
                 id BLOB PRIMARY KEY, lamport INTEGER NOT NULL, ts INTEGER NOT NULL, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS peers (id BLOB PRIMARY KEY, addr BLOB NOT NULL);",
        )
        .context("wrong password or corrupt database")?;
        Ok(Self { db })
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

    /// Returns false if the event was already stored.
    pub fn insert_event(&self, id: &[u8], lamport: u64, ts: u64, data: &[u8]) -> Result<bool> {
        let n = self.db.execute(
            "INSERT OR IGNORE INTO events (id, lamport, ts, data) VALUES (?1, ?2, ?3, ?4)",
            params![id, lamport as i64, ts as i64, data],
        )?;
        Ok(n == 1)
    }

    /// All events, oldest first.
    pub fn events(&self) -> Result<Vec<Vec<u8>>> {
        let mut st = self
            .db
            .prepare("SELECT data FROM events ORDER BY lamport, ts, id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// (id, lamport) of the latest event.
    pub fn head(&self) -> Result<Option<(Vec<u8>, u64)>> {
        Ok(self
            .db
            .query_row(
                "SELECT id, lamport FROM events ORDER BY lamport DESC, ts DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
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

    pub fn peers(&self) -> Result<Vec<Vec<u8>>> {
        let mut st = self.db.prepare("SELECT addr FROM peers")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
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
            assert!(s.insert_event(b"a", 1, 1, b"x").unwrap());
            assert!(!s.insert_event(b"a", 1, 1, b"x").unwrap());
        }
        assert_eq!(
            Store::open(&dir, "pw").unwrap().events().unwrap(),
            vec![b"x".to_vec()]
        );
        assert!(Store::open(&dir, "bad").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
