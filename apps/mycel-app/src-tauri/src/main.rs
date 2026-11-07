//! Tauri shell: thin commands over mycel-core; new events go to the UI as `message`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mycel_core::Node;
use mycel_core::mycel_proto::{Event, EventBody};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tauri::{AppHandle, Emitter, Manager, State};

type R<T> = Result<T, String>;

#[derive(Default)]
struct App(OnceLock<Arc<Node>>);

impl App {
    fn node(&self) -> R<&Arc<Node>> {
        self.0.get().ok_or_else(|| "locked".into())
    }
}

#[derive(Serialize, Clone)]
struct Msg {
    id: String,
    author: String,
    mine: bool,
    text: String,
    ts: u64,
}

#[derive(Serialize)]
struct Status {
    dir: String,
    exists: bool,
    unlocked: bool,
    in_space: bool,
}

#[derive(Serialize)]
struct Member {
    id: String,
    me: bool,
    admin: bool,
}

#[derive(Serialize)]
struct Invite {
    link: String,
    svg: String,
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn short(id: &[u8; 32]) -> String {
    id[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn msg(ev: &Event, me: [u8; 32]) -> Option<Msg> {
    let EventBody::Text { text, .. } = &ev.body else {
        return None;
    };
    Some(Msg {
        id: short(&ev.id),
        author: short(&ev.author_device),
        mine: ev.author_device == me,
        text: text.clone(),
        ts: ev.timestamp_ms,
    })
}

/// `MYCEL_DIR` overrides the data folder (a second profile on one PC, tests).
fn data_dir(app: &AppHandle) -> R<PathBuf> {
    match std::env::var_os("MYCEL_DIR") {
        Some(d) => Ok(d.into()),
        None => app.path().app_data_dir().map_err(|e| e.to_string()),
    }
}

// Commands that touch the node are async: they run on the tokio runtime, which core needs for spawn.
#[tauri::command]
fn status(app: AppHandle, st: State<App>) -> R<Status> {
    let dir = data_dir(&app)?;
    let node = st.0.get();
    Ok(Status {
        exists: dir.join("mycel.db").exists(),
        dir: dir.display().to_string(),
        unlocked: node.is_some(),
        in_space: node.is_some_and(|n| n.admin().is_ok()),
    })
}

#[tauri::command]
async fn unlock(app: AppHandle, st: State<'_, App>, password: String) -> R<bool> {
    if let Some(n) = st.0.get() {
        return Ok(n.admin().is_ok());
    }
    let (node, mut rx) = Node::open(&data_dir(&app)?, &password, 0, true)
        .await
        .map_err(err)?;
    let me = node.device_id();
    let in_space = node.admin().is_ok();
    st.0.set(node).map_err(|_| "already unlocked")?;
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Some(m) = msg(&ev, me) {
                let _ = app.emit("message", m);
            }
        }
    });
    Ok(in_space)
}

#[tauri::command]
async fn create_space(st: State<'_, App>) -> R<()> {
    st.node()?.create_space().map_err(err)
}

#[tauri::command]
async fn join(st: State<'_, App>, link: String) -> R<()> {
    st.node()?.join(&link).await.map_err(err)
}

#[tauri::command]
async fn invite(st: State<'_, App>) -> R<Invite> {
    let link = st.node()?.invite(true).await.map_err(err)?;
    let svg = qrcode::QrCode::new(&link)
        .map_err(|e| e.to_string())?
        .render::<qrcode::render::svg::Color>()
        .quiet_zone(true)
        .build();
    Ok(Invite { link, svg })
}

#[tauri::command]
async fn history(st: State<'_, App>) -> R<Vec<Msg>> {
    let n = st.node()?;
    let me = n.device_id();
    Ok(n.history()
        .map_err(err)?
        .iter()
        .filter_map(|e| msg(e, me))
        .collect())
}

#[tauri::command]
async fn send(st: State<'_, App>, text: String) -> R<Msg> {
    let n = st.node()?;
    let ev = n.send_text(&text).map_err(err)?;
    msg(&ev, n.device_id()).ok_or_else(|| "not a text event".into())
}

#[tauri::command]
async fn members(st: State<'_, App>) -> R<Vec<Member>> {
    let n = st.node()?;
    let (me, admin) = (n.device_id(), n.admin().map_err(err)?);
    Ok(n.members()
        .iter()
        .map(|m| Member {
            id: short(m),
            me: *m == me,
            admin: *m == admin,
        })
        .collect())
}

#[tauri::command]
async fn peers(st: State<'_, App>) -> R<usize> {
    Ok(st.node()?.peers_online())
}

#[tauri::command]
async fn quit(app: AppHandle, st: State<'_, App>) -> R<()> {
    if let Some(n) = st.0.get() {
        n.close().await;
    }
    app.exit(0);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            status,
            unlock,
            create_space,
            join,
            invite,
            history,
            send,
            members,
            peers,
            quit
        ])
        .run(tauri::generate_context!())
        .expect("tauri app failed");
}
