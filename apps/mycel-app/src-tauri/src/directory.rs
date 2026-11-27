//! Usernames on the main server and Telegram-style groups (DESIGN.md §4.6).

use super::{App, R, chat_id, chat_of, data_dir, err, name_of, short, spaces, unhex, vault};
use mycel_core::{Node, Opts, dir};
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Readable Russian text for directory errors.
fn dir_err(e: anyhow::Error) -> String {
    let t = err(e);
    if t.contains(dir::DIR_TAKEN) {
        "Это имя пользователя уже занято".into()
    } else if t.contains("main server unreachable") || t.contains("not a Mycel main server") {
        format!("Сервер имён недоступен ({t})")
    } else if t.contains("no such username") {
        "Нет такого имени пользователя".into()
    } else if t.starts_with("username:") {
        "Имя пользователя: 5–32 знака, латиница, цифры и _".into()
    } else {
        t
    }
}

/// Registers or changes our @username; returns the server address with its pinned id.
#[tauri::command]
pub async fn dir_register(st: State<'_, App>, server: String, username: String) -> R<String> {
    st.node()?
        .dir_register(&server, &username)
        .await
        .map_err(dir_err)
}

#[tauri::command]
pub async fn dir_search(
    st: State<'_, App>,
    server: String,
    prefix: String,
) -> R<Vec<(String, String)>> {
    st.node()?
        .dir_search(&server, &prefix)
        .await
        .map_err(dir_err)
}

/// Adds the user as a contact (unless we already chat) and returns the chat id.
#[tauri::command]
pub async fn dir_add(st: State<'_, App>, server: String, username: String) -> R<String> {
    let n = st.node()?;
    let p = n.dir_add(&server, &username).await.map_err(dir_err)?;
    Ok(chat_id(&Some(p)))
}

/// @username of a member or contact (UI id), or ours for "", if registered.
#[tauri::command]
pub async fn dir_name_of(st: State<'_, App>, server: String, id: String) -> R<Option<String>> {
    let n = st.node()?;
    let key = match chat_of(&n, &id)? {
        Some(k) => n.account_of(&k),
        None => n.account(),
    };
    Ok(n.dir_name_of(&server, key)
        .await
        .map_err(dir_err)?
        .map(|r| r.username))
}

/// The open space's id and password, and its connection settings for a space opened beside it.
fn current(st: &App) -> R<(String, String, Opts)> {
    let o = st.0.lock().unwrap();
    let o = o.as_ref().ok_or("locked")?;
    let opts = Opts {
        n0: o.n0,
        extra_relays: o.relays.clone(),
        mailbox: o.mailbox.clone(),
        ..Default::default()
    };
    Ok((o.id.clone(), o.password.clone(), opts))
}

/// Another of our spaces, opened beside the current one for a moment (the caller closes it).
// shortcut: with the open space's connection settings (per-space ones live in the UI), and
// entering that space fails until it is closed again.
async fn other_space(app: &AppHandle, st: &App, id: &str) -> R<Arc<Node>> {
    let root = data_dir(app)?;
    let (cur, pw, opts) = current(st)?;
    let list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    if id == cur || list.space(id).is_none() {
        return Err("no such space".into());
    }
    let pw = vault::load(&root, id).unwrap_or(pw);
    let (n, _rx) = Node::open(&spaces::dir(&root, id), &pw, &opts)
        .await
        .map_err(|e| format!("Не удалось открыть группу (другой пароль?): {}", err(e)))?;
    Ok(n)
}

/// (chat id, name) of the people in another space, for «Переслать».
#[tauri::command]
pub async fn space_chats(
    app: AppHandle,
    st: State<'_, App>,
    space: String,
) -> R<Vec<(String, String)>> {
    // Read first: nothing may fail between opening the node and closing it.
    let list = spaces::List::load(&data_dir(&app)?).map_err(|e| e.to_string())?;
    let n = other_space(&app, &st, &space).await?;
    let removed = list.space(&space).map_or(&[][..], |s| &s.removed);
    let mut all = n.members();
    all.extend(n.contacts().unwrap_or_default());
    all.sort();
    all.dedup();
    // One row per account, as in «Люди».
    all.retain(|m| n.account_of(m) == *m && *m != n.account() && !removed.contains(&short(m)));
    let out = all.iter().map(|m| (short(m), name_of(&n, m))).collect();
    n.close().await;
    Ok(out)
}

/// «Переслать» into chat `to` of another space (a group, or a contact there).
#[tauri::command]
pub async fn forward_to_space(
    app: AppHandle,
    st: State<'_, App>,
    ids: Vec<String>,
    space: String,
    to: String,
) -> R<()> {
    let cur = st.node()?;
    let ids = ids.iter().map(|i| unhex(i)).collect::<R<Vec<_>>>()?;
    let n = other_space(&app, &st, &space).await?;
    let sent = async {
        let to = chat_of(&n, &to)?;
        if let Some(p) = to {
            n.open_dm(p).map_err(err)?;
        }
        cur.forward_into(&ids, &n, &to).await.map_err(err)
    }
    .await;
    // Closing at once: give the copies a moment to reach someone (the mailbox post goes on).
    for _ in 0..30 {
        if !n.awaiting_receipt() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    n.close().await;
    sent
}

/// A new space beside the open one, with the same password and connection settings (and kept
/// in the keystore if the open one is), opened in the background.
async fn side_space(app: &AppHandle, st: &App, name: &str) -> R<(String, Arc<Node>)> {
    let root = data_dir(app)?;
    let (cur, pw, opts) = current(st)?;
    let id = spaces::new_id();
    spaces::List::edit(&root, |l| {
        l.spaces.push(spaces::Space {
            id: id.clone(),
            name: name.trim().chars().take(64).collect(),
            muted: vec![],
            pinned: vec![],
            archived: vec![],
            hidden: vec![],
            removed: vec![],
        })
    })
    .map_err(|e| e.to_string())?;
    if vault::load(&root, &cur).is_some() {
        vault::save(&root, &id, &pw)?;
    }
    match Node::open(&spaces::dir(&root, &id), &pw, &opts).await {
        Ok((n, _rx)) => Ok((id, n)),
        Err(e) => {
            drop_space(app, &id);
            Err(err(e))
        }
    }
}

fn drop_space(app: &AppHandle, id: &str) {
    let Ok(root) = data_dir(app) else { return };
    let _ = spaces::List::edit(&root, |l| l.spaces.retain(|s| s.id != id));
    vault::forget(&root, id);
    // Fails while the closed node still holds files; List::load cleans orphans at start.
    let _ = std::fs::remove_dir_all(spaces::dir(&root, id));
}

/// Our name, avatar and color from the open space, for a new one.
fn copy_profile(from: &Node, to: &Node) {
    let me = from
        .profiles()
        .ok()
        .and_then(|mut p| p.remove(&from.device_id()));
    if let Some((name, avatar, color, _)) = me {
        let _ = to.set_profile(&name, &avatar, color);
    }
}

/// «Новая группа»: a new space we admin; each picked contact gets a one-time invite as a
/// message in our chat (the UI shows «Вступить» on it). Returns the new space id; the UI opens
/// it, so the invites can be accepted.
// shortcut: the invite travels as a message with a link, not as a silent join.
#[tauri::command]
pub async fn create_group(
    app: AppHandle,
    st: State<'_, App>,
    name: String,
    members: Vec<String>,
) -> R<String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Введите название группы".into());
    }
    let cur = st.node()?;
    let peers = members
        .iter()
        .map(|m| chat_of(&cur, m)?.ok_or_else(|| "no such member".to_string()))
        .collect::<R<Vec<_>>>()?;
    let (id, g) = side_space(&app, &st, &name).await?;
    let made = async {
        g.create_space()?;
        copy_profile(&cur, &g);
        let mut links = vec![];
        for _ in &peers {
            links.push(g.invite().await?);
        }
        anyhow::Ok(links)
    }
    .await;
    g.close().await;
    let links = match made {
        Ok(l) => l,
        Err(e) => {
            drop_space(&app, &id);
            return Err(err(e));
        }
    };
    for (p, link) in peers.iter().zip(links) {
        let _ = cur.open_dm(*p);
        let chat = Some(cur.account_of(p));
        cur.send_text(&chat, &format!("Приглашаю в группу «{name}»\n{link}"), None)
            .map_err(err)?;
    }
    // The UI closes this space next: give the invites a moment to reach someone.
    for _ in 0..30 {
        if !cur.awaiting_receipt() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Ok(id)
}

/// «Вступить» on a group invite: joins it as a new space beside the open one (same password),
/// without leaving this one. Returns the new space id.
#[tauri::command]
pub async fn join_group(
    app: AppHandle,
    st: State<'_, App>,
    link: String,
    name: String,
) -> R<String> {
    let cur = st.node()?;
    let name = match name.trim() {
        "" => "Группа".to_string(),
        n => n.to_string(),
    };
    let (id, g) = side_space(&app, &st, &name).await?;
    let res = g.join(&link).await;
    if res.is_ok() {
        copy_profile(&cur, &g);
        g.sync_now().await;
    }
    g.close().await;
    match res {
        Ok(()) => Ok(id),
        Err(e) => {
            drop_space(&app, &id);
            let t = err(e);
            Err(if t.starts_with(mycel_core::JOIN_UNREACHABLE) {
                "Создатель группы сейчас не в сети. Попробуйте позже".into()
            } else if t.starts_with(mycel_core::JOIN_USED) {
                "Приглашение уже использовано".into()
            } else {
                t
            })
        }
    }
}
