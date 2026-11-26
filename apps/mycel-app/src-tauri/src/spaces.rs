//! Spaces on this device: each has its own data folder `<root>/spaces/<id>` (DB, node, media),
//! listed with names in `<root>/spaces.json`.

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone)]
pub struct Space {
    pub id: String,
    pub name: String,
    /// Chats without notifications on this device (the UI's chat ids).
    #[serde(default)]
    pub muted: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct List {
    pub spaces: Vec<Space>,
    /// Opened last: the app opens it at start.
    pub last: Option<String>,
    /// Connection settings of the last unlock: a push opens that space without the UI (lib.rs `pull`).
    #[serde(default)]
    pub n0: bool,
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub mailbox: Option<String>,
}

/// What a node keeps in its folder; `salt` goes last, so an interrupted move is redone next start.
const DATA: [&str; 8] = [
    "mycel.db",
    "mycel.db-wal",
    "mycel.db-shm",
    "mycel.db-journal",
    "blobs",
    "media",
    "tmp",
    "salt",
];

pub fn dir(root: &Path, id: &str) -> PathBuf {
    root.join("spaces").join(id)
}

pub fn new_id() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}", t.as_nanos())
}

impl List {
    /// Reads the list. Data from before spaces (right in `<root>`) becomes space `main`; folders of
    /// removed spaces that could not be deleted then (files still open) are deleted now.
    pub fn load(root: &Path) -> Result<Self> {
        let mut l: Self = match std::fs::read(root.join("spaces.json")) {
            Ok(b) => {
                serde_json::from_slice(&b).map_err(|e| Error::new(ErrorKind::InvalidData, e))?
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e),
        };
        if root.join("salt").exists() {
            let d = dir(root, "main");
            std::fs::create_dir_all(&d)?;
            for f in DATA {
                if root.join(f).exists() {
                    std::fs::rename(root.join(f), d.join(f))?;
                }
            }
            if !l.spaces.iter().any(|s| s.id == "main") {
                l.spaces.insert(
                    0,
                    Space {
                        id: "main".into(),
                        name: "Моё пространство".into(),
                        muted: vec![],
                    },
                );
                l.last = Some("main".into());
            }
            l.save(root)?;
        }
        if let Ok(rd) = std::fs::read_dir(root.join("spaces")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !l.spaces.iter().any(|s| s.id == name) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        Ok(l)
    }

    pub fn muted(&self, space: &str, chat: &str) -> bool {
        let s = self.spaces.iter().find(|s| s.id == space);
        s.is_some_and(|s| s.muted.iter().any(|c| c == chat))
    }

    pub fn set_muted(&mut self, space: &str, chat: &str, on: bool) {
        if let Some(s) = self.spaces.iter_mut().find(|s| s.id == space) {
            s.muted.retain(|c| c != chat);
            if on {
                s.muted.push(chat.into());
            }
        }
    }

    /// Written via a temp file, so a crash never leaves a broken list.
    pub fn save(&self, root: &Path) -> Result<()> {
        std::fs::create_dir_all(root)?;
        let tmp = root.join("spaces.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, root.join("spaces.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_old_data_into_main_and_drops_orphans() {
        let root = std::env::temp_dir().join(format!("mycel-spaces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::write(root.join("salt"), b"s").unwrap();
        std::fs::write(root.join("mycel.db"), b"db").unwrap();
        std::fs::write(root.join("blobs").join("x"), b"x").unwrap();
        std::fs::create_dir_all(dir(&root, "gone")).unwrap();

        let l = List::load(&root).unwrap();
        assert_eq!(l.spaces.len(), 1);
        assert_eq!(l.last.as_deref(), Some("main"));
        let d = dir(&root, "main");
        assert_eq!(std::fs::read(d.join("mycel.db")).unwrap(), b"db");
        assert_eq!(std::fs::read(d.join("blobs").join("x")).unwrap(), b"x");
        assert!(!root.join("salt").exists() && !dir(&root, "gone").exists());

        let mut l = List::load(&root).unwrap();
        assert_eq!(l.spaces.len(), 1);
        assert!(d.join("salt").exists());

        l.set_muted("main", "", true);
        l.set_muted("main", "", true);
        l.save(&root).unwrap();
        let mut l = List::load(&root).unwrap();
        assert!(l.muted("main", "") && !l.muted("main", "ab") && !l.muted("gone", ""));
        l.set_muted("main", "", false);
        assert!(!l.muted("main", ""));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
