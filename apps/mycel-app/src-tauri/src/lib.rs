//! Tauri shell: thin commands over mycel-core; new events go to the UI as `message`.

use mycel_core::mycel_proto::{BlobRef, CallAction, Event, EventBody, EventId, MediaKind, Preview};
use mycel_core::{Chat, Node, Opts, edits_and_pins, media};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

mod spaces;
mod vault;

type R<T> = Result<T, String>;

/// The open space: one node at a time, swapped without restarting the app.
#[derive(Default)]
struct App(Mutex<Option<Open>>);

struct Open {
    node: Arc<Node>,
    id: String,
    /// Kept to turn "remember" on later and to check the old one on change.
    password: String,
}

impl App {
    fn node(&self) -> R<Arc<Node>> {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .map(|o| o.node.clone())
            .ok_or_else(|| "locked".into())
    }

    fn is_open(&self, id: &str) -> bool {
        self.0.lock().unwrap().as_ref().is_some_and(|o| o.id == id)
    }

    /// Ends the call and closes the open node, if any.
    async fn close(&self) {
        let open = self.0.lock().unwrap().take();
        if let Some(o) = open {
            let _ = o.node.call_end();
            o.node.close().await;
        }
    }
}

/// The window is on screen (the UI reports `visibilitychange`); hidden → system notifications.
struct Visible(AtomicBool);

fn notify(app: &AppHandle, title: String, body: String) {
    if !app.state::<Visible>().0.load(Ordering::Relaxed) {
        let _ = app.notification().builder().title(title).body(body).show();
    }
}

#[derive(Serialize, Clone)]
struct Msg {
    id: String,
    author: String,
    mine: bool,
    text: String,
    ts: u64,
    /// Own messages: 0 ⏱, 1 ✓, 2 ✓✓ (§7.4).
    status: u8,
    media: Option<Media>,
    call: Option<CallView>,
    reply: Option<Reply>,
    edited: bool,
    pinned: bool,
}

/// The quoted message above a reply.
#[derive(Serialize, Clone)]
struct Reply {
    id: String,
    author: String,
    text: String,
}

/// What a history says about its messages beyond their own events.
struct Ctx<'a> {
    calls: HashMap<String, CallView>,
    edits: HashMap<EventId, String>,
    pins: Vec<EventId>,
    by_id: HashMap<EventId, &'a Event>,
}

impl<'a> Ctx<'a> {
    fn new(n: &Node, hist: &'a [Event], chat: &str) -> Self {
        let (edits, pins) = edits_and_pins(hist);
        Ctx {
            calls: calls(n, hist, chat),
            edits,
            pins,
            by_id: hist.iter().map(|e| (e.id, e)).collect(),
        }
    }

    /// Text (or caption) after edits.
    fn text(&self, ev: &Event) -> String {
        if let Some(t) = self.edits.get(&ev.id) {
            return t.clone();
        }
        match &ev.body {
            EventBody::Text { text, .. } => text.clone(),
            EventBody::Media { caption, .. } => caption.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// A call as the chat feed and the Calls tab show it (built from its Call events).
#[derive(Serialize, Clone)]
struct CallView {
    id: String,
    video: bool,
    /// The offerer.
    author: String,
    /// "" = the space chat (group call), else the 1:1 partner.
    chat: String,
    mine: bool,
    ts: u64,
    /// From the first answer to the last hang-up.
    duration_ms: Option<u64>,
    /// Nobody answered.
    missed: bool,
    /// No hang-up yet.
    active: bool,
}

/// `call` event for the UI: one Call event from someone else.
#[derive(Serialize, Clone)]
struct CallSignal {
    id: String,
    /// offer | accept | reject | end
    action: &'static str,
    video: bool,
    author: String,
    chat: String,
    ts: u64,
}

#[derive(Serialize, Clone)]
struct Media {
    /// file | photo | video | voice | videonote
    kind: &'static str,
    name: Option<String>,
    mime: String,
    size: u64,
    blurhash: Option<String>,
    /// 64 levels 0..255 (voice).
    wave: Option<Vec<u8>>,
    duration_ms: Option<u32>,
    dims: Option<(u32, u32)>,
    /// The decrypted file, once downloaded.
    path: Option<String>,
}

#[derive(Serialize)]
struct Status {
    spaces: Vec<SpaceRow>,
    last: Option<String>,
    /// The open space, if any.
    current: Option<String>,
    /// Its data folder.
    dir: String,
    in_space: bool,
}

#[derive(Serialize)]
struct SpaceRow {
    id: String,
    name: String,
    /// The password is in the OS keystore: opens without asking.
    remembered: bool,
}

#[derive(Serialize)]
struct Member {
    id: String,
    me: bool,
    admin: bool,
    /// From the member's profile; None until they set one.
    name: Option<String>,
    avatar: String,
    color: Option<u8>,
}

/// A row of the chat list.
#[derive(Serialize)]
struct ChatRow {
    /// "" = the space chat, else the partner's short id.
    id: String,
    last: Option<Msg>,
    unread: usize,
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

/// The UI names a chat by "" (space chat) or the partner's short id.
fn chat_id(c: &Chat) -> String {
    c.as_ref().map(short).unwrap_or_default()
}

fn chat_of(n: &Node, s: &str) -> R<Chat> {
    if s.is_empty() {
        return Ok(None);
    }
    let m = n.members().into_iter().find(|m| short(m) == s);
    m.map(Some).ok_or_else(|| "no such member".into())
}

/// Display name of a device (notifications).
fn name_of(n: &Node, dev: &[u8; 32]) -> String {
    let p = n.profiles().unwrap_or_default();
    p.get(dev)
        .map(|p| p.0.clone())
        .unwrap_or_else(|| format!("Участник {}", short(dev)))
}

fn msg(n: &Node, ev: &Event, cx: &Ctx) -> Option<Msg> {
    if let EventBody::Call {
        call_id,
        action: CallAction::Offer { .. },
    } = &ev.body
    {
        let c = cx.calls.get(&hex(call_id))?.clone();
        return Some(Msg {
            id: hex(&ev.id),
            author: short(&ev.author_device),
            mine: c.mine,
            text: String::new(),
            ts: ev.timestamp_ms,
            status: 2,
            media: None,
            call: Some(c),
            reply: None,
            edited: false,
            pinned: false,
        });
    }
    let (mv, reply_to) = match &ev.body {
        EventBody::Text { reply_to, .. } => (None, reply_to),
        EventBody::Media {
            kind,
            blob,
            reply_to,
            ..
        } => (Some(media_view(n, *kind, blob)), reply_to),
        _ => return None,
    };
    let reply = reply_to.map(|r| match cx.by_id.get(&r) {
        Some(t) => Reply {
            id: hex(&r),
            author: short(&t.author_device),
            text: match (cx.text(t), media(t)) {
                (s, Some((_, b))) if s.is_empty() => b.name.clone().unwrap_or("Медиа".into()),
                (s, _) => s,
            },
        },
        None => Reply {
            id: hex(&r),
            author: String::new(),
            text: "Сообщение удалено".into(),
        },
    });
    Some(Msg {
        id: hex(&ev.id),
        author: short(&ev.author_device),
        mine: ev.author_device == n.device_id(),
        text: cx.text(ev),
        ts: ev.timestamp_ms,
        status: 0,
        media: mv,
        call: None,
        reply,
        edited: cx.edits.contains_key(&ev.id),
        pinned: cx.pins.contains(&ev.id),
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> R<[u8; N]> {
    let v: Vec<u8> = (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    v.try_into().map_err(|_| "bad id".into())
}

fn call_id(s: &str) -> R<[u8; 16]> {
    unhex(s)
}

/// Every call in the history of `chat` by hex id.
fn calls(n: &Node, hist: &[Event], chat: &str) -> HashMap<String, CallView> {
    let me = n.device_id();
    let mut out: HashMap<String, CallView> = HashMap::new();
    // shortcut: a group call counts as ended at its last hang-up event seen so far; track per-member presence if that misleads.
    let mut answered: HashMap<String, u64> = HashMap::new();
    for e in hist {
        let EventBody::Call { call_id, action } = &e.body else {
            continue;
        };
        let id = hex(call_id);
        match action {
            CallAction::Offer { video } => {
                out.insert(
                    id.clone(),
                    CallView {
                        id,
                        video: *video,
                        author: short(&e.author_device),
                        chat: chat.into(),
                        mine: e.author_device == me,
                        ts: e.timestamp_ms,
                        duration_ms: None,
                        missed: true,
                        active: true,
                    },
                );
            }
            CallAction::Accept => {
                if let Some(c) = out.get_mut(&id) {
                    c.missed = false;
                    c.active = true;
                    answered.entry(id).or_insert(e.timestamp_ms);
                }
            }
            CallAction::End => {
                if let Some(c) = out.get_mut(&id) {
                    c.active = false;
                    c.duration_ms = answered.get(&id).map(|t| e.timestamp_ms.saturating_sub(*t));
                }
            }
            CallAction::Reject => {}
        }
    }
    out
}

fn media_view(n: &Node, kind: MediaKind, b: &BlobRef) -> Media {
    let path = n.media_file(b);
    Media {
        kind: match kind {
            MediaKind::File => "file",
            MediaKind::Photo => "photo",
            MediaKind::Video => "video",
            MediaKind::Voice => "voice",
            MediaKind::VideoNote => "videonote",
        },
        name: b.name.clone(),
        mime: b.mime.clone(),
        size: b.size,
        blurhash: match &b.preview {
            Some(Preview::Blurhash(h)) => Some(h.clone()),
            _ => None,
        },
        wave: match &b.preview {
            Some(Preview::Waveform(w)) => Some(w.clone()),
            _ => None,
        },
        duration_ms: b.duration_ms,
        dims: b.dims,
        path: path.exists().then(|| path.display().to_string()),
    }
}

/// The Media event with this id, and its blob.
fn find_media(n: &Node, id: &str) -> R<(Event, BlobRef)> {
    let ev = n.event(&unhex(id)?).map_err(err)?.1;
    let b = media(&ev).ok_or("not a media message")?.1.clone();
    Ok((ev, b))
}

/// A fresh temp path: parallel sends must not share one.
fn tmp_file(ext: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("mycel-{}-{n}.{ext}", std::process::id()))
}

fn blob_meta(mime: &str, name: Option<String>) -> BlobRef {
    BlobRef {
        hash: [0; 32],
        size: 0,
        key: [0; 32],
        mime: mime.into(),
        name,
        preview: None,
        duration_ms: None,
        dims: None,
    }
}

fn mime_of(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// (id, status) of own messages in a chat.
fn statuses(n: &Node, chat: &Chat) -> Vec<(String, u8)> {
    let s = n.statuses(chat).unwrap_or_default();
    s.iter().map(|(id, st)| (hex(id), *st)).collect()
}

/// A chat's messages as the UI shows them.
fn chat_msgs(n: &Node, chat: &Chat) -> R<Vec<Msg>> {
    let st: HashMap<_, _> = statuses(n, chat).into_iter().collect();
    let hist = n.history(chat).map_err(err)?;
    let cx = Ctx::new(n, &hist, &chat_id(chat));
    Ok(hist
        .iter()
        .filter_map(|e| msg(n, e, &cx))
        .map(|m| Msg {
            status: st.get(&m.id).copied().unwrap_or(0),
            ..m
        })
        .collect())
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
    let root = data_dir(&app)?;
    let list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    let open = st.0.lock().unwrap();
    let current = open.as_ref().map(|o| o.id.clone());
    Ok(Status {
        spaces: list
            .spaces
            .into_iter()
            .map(|s| SpaceRow {
                remembered: vault::load(&root, &s.id).is_some(),
                id: s.id,
                name: s.name,
            })
            .collect(),
        last: list.last,
        dir: spaces::dir(&root, current.as_deref().unwrap_or(""))
            .display()
            .to_string(),
        in_space: open.as_ref().is_some_and(|o| o.node.admin().is_ok()),
        current,
    })
}

/// Opens space `id` (a new one named `name` if None), closing the open one. Without `password` it
/// comes from the keystore; with it, `remember` puts it there or takes it out.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn unlock(
    app: AppHandle,
    st: State<'_, App>,
    id: Option<String>,
    name: String,
    password: Option<String>,
    remember: bool,
    n0: bool,
) -> R<bool> {
    let root = data_dir(&app)?;
    let mut list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    let id = match id {
        Some(id) => id,
        None => {
            let id = spaces::new_id();
            list.spaces.push(spaces::Space {
                id: id.clone(),
                name: name.trim().to_string(),
            });
            id
        }
    };
    if st.is_open(&id) {
        return Ok(st.node()?.admin().is_ok());
    }
    let pw = match &password {
        Some(p) => p.clone(),
        None => vault::load(&root, &id).ok_or("no saved password")?,
    };
    // Listed before the node creates its folder, so List::load never takes it for an orphan.
    list.last = Some(id.clone());
    list.save(&root).map_err(|e| e.to_string())?;
    st.close().await;
    // Anchors' relays come via invite/peer exchange; n0 (Settings, on by default) is the public fallback.
    let dir = spaces::dir(&root, &id);
    let (node, mut rx) = Node::open(
        &dir,
        &pw,
        &Opts {
            n0,
            ..Default::default()
        },
    )
    .await
    .map_err(err)?;
    if password.is_some() {
        if remember {
            vault::save(&root, &id, &pw)?;
        } else {
            vault::forget(&root, &id);
        }
    }
    // <img>/<video> load decrypted media through the asset protocol.
    let _ = app.asset_protocol_scope().allow_directory(
        std::path::absolute(dir.join("media")).map_err(|e| e.to_string())?,
        true,
    );
    let in_space = node.admin().is_ok();
    *st.0.lock().unwrap() = Some(Open {
        node: node.clone(),
        id,
        password: pw,
    });
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            // Another space was opened since: its own loop reports.
            let current = app.state::<App>().node();
            if !current.is_ok_and(|n| Arc::ptr_eq(&n, &node)) {
                break;
            }
            let chat = node.event(&ev.id).map(|e| e.0).unwrap_or(None);
            if let EventBody::Call { call_id, action } = &ev.body {
                let (action, video) = match action {
                    CallAction::Offer { video } => ("offer", *video),
                    CallAction::Accept => ("accept", false),
                    CallAction::Reject => ("reject", false),
                    CallAction::End => ("end", false),
                };
                if action == "offer" {
                    let who = name_of(&node, &ev.author_device);
                    notify(&app, who, "Входящий звонок".into());
                }
                let _ = app.emit(
                    "call",
                    CallSignal {
                        id: hex(call_id),
                        action,
                        video,
                        author: short(&ev.author_device),
                        chat: chat_id(&chat),
                        ts: ev.timestamp_ms,
                    },
                );
                continue;
            }
            if ev.author_device != node.device_id() {
                let mut who = name_of(&node, &ev.author_device);
                if chat.is_none() {
                    who += " · Общий чат";
                }
                match &ev.body {
                    EventBody::Text { text, .. } => notify(&app, who, text.clone()),
                    EventBody::Media { .. } => notify(&app, who, "Медиа".into()),
                    _ => {}
                }
            }
            // The UI reloads on `message` / `changed` (payload: the chat); receipts only move the ticks.
            let _ = match &ev.body {
                EventBody::Text { .. } | EventBody::Media { .. } => {
                    app.emit("message", chat_id(&chat))
                }
                EventBody::Receipt { .. } => app.emit("status", statuses(&node, &chat)),
                _ => app.emit("changed", chat_id(&chat)),
            };
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
    st.node()?.join(&link).await.map_err(|e| {
        let t = err(e);
        let why = if t.starts_with(mycel_core::JOIN_BAD_LINK) {
            "Это не ссылка-приглашение Mycel: скопируйте её целиком"
        } else if t.starts_with(mycel_core::JOIN_UNREACHABLE) {
            "Нет связи с тем, кто пригласил: его Mycel должен быть открыт и в сети. Попробуйте ещё раз"
        } else if t.starts_with(mycel_core::JOIN_USED) {
            "Приглашение уже использовано или отменено: попросите новое"
        } else {
            return t;
        };
        format!("{why}
({t})")
    })
}

#[tauri::command]
async fn invite(st: State<'_, App>) -> R<Invite> {
    let link = st.node()?.invite().await.map_err(err)?;
    let svg = qrcode::QrCode::new(&link)
        .map_err(|e| e.to_string())?
        .render::<qrcode::render::svg::Color>()
        .quiet_zone(true)
        .build();
    Ok(Invite { link, svg })
}

#[tauri::command]
async fn history(st: State<'_, App>, chat: String) -> R<Vec<Msg>> {
    let n = &st.node()?;
    chat_msgs(n, &chat_of(n, &chat)?)
}

/// The space chat and every 1:1 chat with its last message; `read`: chat → ts read up to.
#[tauri::command]
async fn chats(st: State<'_, App>, read: HashMap<String, u64>) -> R<Vec<ChatRow>> {
    let n = &st.node()?;
    let mut list = vec![None];
    list.extend(n.dms().map_err(err)?.into_iter().map(Some));
    list.iter()
        .map(|c| {
            let id = chat_id(c);
            let msgs = chat_msgs(n, c)?;
            let seen = read.get(&id).copied().unwrap_or(0);
            Ok(ChatRow {
                unread: msgs.iter().filter(|m| !m.mine && m.ts > seen).count(),
                last: msgs.into_iter().last(),
                id,
            })
        })
        .collect()
}

#[tauri::command]
async fn open_dm(st: State<'_, App>, peer: String) -> R<()> {
    let n = &st.node()?;
    let p = chat_of(n, &peer)?.ok_or("no such member")?;
    n.open_dm(p).map_err(err)
}

#[tauri::command]
async fn set_profile(st: State<'_, App>, name: String, avatar: String, color: u8) -> R<()> {
    st.node()?
        .set_profile(&name, &avatar, color)
        .map(|_| ())
        .map_err(err)
}

#[tauri::command]
/// Returns the new event's id (the UI swaps its pending bubble for it).
async fn send(
    st: State<'_, App>,
    chat: String,
    text: String,
    reply_to: Option<String>,
) -> R<String> {
    let n = &st.node()?;
    let r = reply_to.map(|r| unhex(&r)).transpose()?;
    let ev = n.send_text(&chat_of(n, &chat)?, &text, r).map_err(err)?;
    Ok(hex(&ev.id))
}

#[tauri::command]
async fn edit(st: State<'_, App>, id: String, text: String) -> R<()> {
    st.node()?.edit(unhex(&id)?, &text).map_err(err)?;
    Ok(())
}

/// Several at once (selection mode); stops at the first refusal.
#[tauri::command]
async fn delete(st: State<'_, App>, ids: Vec<String>, all: bool) -> R<()> {
    let n = &st.node()?;
    for id in ids {
        n.delete(unhex(&id)?, all).map_err(err)?;
    }
    Ok(())
}

/// To any chat: `to` is "" or a member's short id (a 1:1 chat is opened if there is none).
#[tauri::command]
async fn forward(st: State<'_, App>, ids: Vec<String>, to: String) -> R<()> {
    let n = &st.node()?;
    let to = chat_of(n, &to)?;
    if let Some(p) = to {
        n.open_dm(p).map_err(err)?;
    }
    for id in ids {
        n.forward(unhex(&id)?, &to).map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
async fn pin(st: State<'_, App>, id: String, pinned: bool) -> R<()> {
    st.node()?.pin(unhex(&id)?, pinned).map_err(err)?;
    Ok(())
}

/// 📎 / drag-and-drop: photos are re-encoded (no EXIF) unless `original`, then they go as a file.
#[tauri::command]
async fn send_file(st: State<'_, App>, chat: String, path: String, original: bool) -> R<String> {
    let n = &st.node()?;
    let chat = &chat_of(n, &chat)?;
    let src = Path::new(&path);
    let name = src.file_name().map(|s| s.to_string_lossy().into_owned());
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mime = mime_of(&ext);
    let ev = if !original && matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp") {
        let s = src.to_owned();
        let ph = tauri::async_runtime::spawn_blocking(move || mycel_media::photo(&s))
            .await
            .map_err(|e| e.to_string())?
            .map_err(err)?;
        let tmp = tmp_file("jpg");
        std::fs::write(&tmp, &ph.jpeg).map_err(|e| e.to_string())?;
        let mut m = blob_meta("image/jpeg", name.map(|n| format!("{n}.jpg")));
        m.preview = Some(Preview::Blurhash(ph.blurhash));
        m.dims = Some(ph.dims);
        let r = n.send_media(chat, MediaKind::Photo, &tmp, m, None).await;
        let _ = std::fs::remove_file(&tmp);
        r.map_err(err)?
    } else {
        // shortcut: video goes as is, no re-encode or preview frame; ffmpeg when people complain about size.
        let kind = if mime.starts_with("video/") {
            MediaKind::Video
        } else {
            MediaKind::File
        };
        n.send_media(chat, kind, src, blob_meta(mime, name), None)
            .await
            .map_err(err)?
    };
    Ok(hex(&ev.id))
}

/// Raw IPC body; Android's IPC delivers a Uint8Array as a JSON array of numbers instead.
fn body<'a>(request: &'a tauri::ipc::Request<'_>) -> R<std::borrow::Cow<'a, [u8]>> {
    match request.body() {
        tauri::ipc::InvokeBody::Raw(d) => Ok(d.into()),
        tauri::ipc::InvokeBody::Json(v) => serde_json::from_value::<Vec<u8>>(v.clone())
            .map(Into::into)
            .map_err(|e| e.to_string()),
    }
}

#[tauri::command]
fn set_visible(v: bool, vis: State<Visible>) {
    vis.0.store(v, Ordering::Relaxed);
}

/// A file picked in the WebView (Android has no paths): raw body saved as temp/`name`; returns the path for `send_file`.
#[tauri::command]
fn stash(request: tauri::ipc::Request<'_>) -> R<String> {
    let data = &body(&request)?;
    // Header values are ASCII: the UI sends encodeURIComponent(name).
    let name = request.headers().get("name").and_then(|v| v.to_str().ok());
    let name = percent_encoding::percent_decode_str(name.unwrap_or("file")).decode_utf8_lossy();
    // Only the last path part: the name comes from the WebView.
    let name = Path::new(name.as_ref()).file_name().ok_or("bad name")?;
    let p = std::env::temp_dir().join(name);
    std::fs::write(&p, data).map_err(|e| e.to_string())?;
    Ok(p.display().to_string())
}

/// A recording from the WebView (raw body); headers: chat, kind (voice|videonote), mime, duration, wave (csv).
#[tauri::command]
async fn send_rec(st: State<'_, App>, request: tauri::ipc::Request<'_>) -> R<String> {
    let n = &st.node()?;
    let data = &body(&request)?;
    let h = |k: &str| {
        request
            .headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let kind = if h("kind") == "videonote" {
        MediaKind::VideoNote
    } else {
        MediaKind::Voice
    };
    let mime = h("mime");
    let mut m = blob_meta(&mime, None);
    m.duration_ms = h("duration").parse().ok();
    let wave: Vec<u8> = h("wave")
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    if !wave.is_empty() {
        m.preview = Some(Preview::Waveform(wave));
    }
    let chat = chat_of(n, &h("chat"))?;
    let tmp = tmp_file("rec");
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let r = n.send_media(&chat, kind, &tmp, m, None).await;
    let _ = std::fs::remove_file(&tmp);
    Ok(hex(&r.map_err(err)?.id))
}

#[derive(Serialize, Clone)]
struct Progress {
    id: String,
    done: u64,
}

/// Downloads (resuming) and decrypts a media message; emits `progress`; returns the file path.
#[tauri::command]
async fn fetch_media(app: AppHandle, st: State<'_, App>, id: String) -> R<String> {
    let n = &st.node()?;
    let (ev, b) = find_media(n, &id)?;
    let mut last = 0;
    n.fetch(&b, ev.author_device, |done| {
        // ~1% steps keep the IPC quiet.
        if done - last > b.size / 100 {
            last = done;
            let _ = app.emit(
                "progress",
                Progress {
                    id: id.clone(),
                    done,
                },
            );
        }
    })
    .await
    .map_err(err)?;
    let p = n.media_path(&b).await.map_err(err)?;
    Ok(p.display().to_string())
}

#[tauri::command]
async fn save_media(st: State<'_, App>, id: String, to: String) -> R<()> {
    let n = &st.node()?;
    let p = n.media_path(&find_media(n, &id)?.1).await.map_err(err)?;
    std::fs::copy(p, to).map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn open_media(st: State<'_, App>, id: String) -> R<()> {
    let n = &st.node()?;
    let p = n.media_path(&find_media(n, &id)?.1).await.map_err(err)?;
    tauri_plugin_opener::open_path(p, None::<&str>).map_err(|e| e.to_string())
}

/// Calls tab: newest first.
#[tauri::command]
async fn calls_list(st: State<'_, App>) -> R<Vec<CallView>> {
    let n = &st.node()?;
    let mut list = vec![None];
    list.extend(n.dms().map_err(err)?.into_iter().map(Some));
    let mut v = vec![];
    for c in list {
        let hist = n.history(&c).map_err(err)?;
        v.extend(calls(n, &hist, &chat_id(&c)).into_values());
    }
    v.sort_by_key(|c| std::cmp::Reverse(c.ts));
    Ok(v)
}

/// 📞/📹: sends an Offer to a chat (space chat: group call; 1:1 chat: just that member); returns the call id.
#[tauri::command]
async fn call_start(st: State<'_, App>, chat: String, video: bool) -> R<String> {
    let n = &st.node()?;
    Ok(hex(&n
        .call_offer(&chat_of(n, &chat)?, video)
        .map_err(err)?
        .0))
}

#[tauri::command]
/// Returns the other participants already in the call (short ids).
async fn call_accept(st: State<'_, App>, id: String) -> R<Vec<String>> {
    let n = &st.node()?;
    let id = call_id(&id)?;
    // Answering while still in another call: leave that one.
    n.call_end().map_err(err)?;
    n.call_accept(id).map_err(err)?;
    let me = n.device_id();
    let others = n.call_members(id).map_err(err)?.into_iter();
    Ok(others.filter(|m| *m != me).map(|m| short(&m)).collect())
}

#[tauri::command]
async fn call_reject(st: State<'_, App>, id: String) -> R<()> {
    st.node()?
        .call_reject(call_id(&id)?)
        .map(|_| ())
        .map_err(err)
}

#[tauri::command]
async fn call_end(st: State<'_, App>) -> R<()> {
    st.node()?.call_end().map(|_| ()).map_err(err)
}

/// One encoded frame from the WebView (raw body); header `kind`: 0 audio, 1 video key, 2 video delta.
#[tauri::command]
async fn call_send(st: State<'_, App>, request: tauri::ipc::Request<'_>) -> R<()> {
    let data = &body(&request)?;
    let kind = request
        .headers()
        .get("kind")
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .ok_or("no kind")?;
    st.node()?.call_send(kind, data).map_err(err)
}

/// Incoming frames go to `ch` as raw bytes: kind (1), sender short id (4), seq (8, BE), data.
#[tauri::command]
async fn call_listen(st: State<'_, App>, ch: Channel<InvokeResponseBody>) -> R<()> {
    let mut rx = st.node()?.call_frames();
    tauri::async_runtime::spawn(async move {
        while let Some(f) = rx.recv().await {
            let mut b = vec![f.kind];
            b.extend_from_slice(&f.from[..4]);
            b.extend_from_slice(&f.seq.to_be_bytes());
            b.extend_from_slice(&f.data);
            if ch.send(InvokeResponseBody::Raw(b)).is_err() {
                break;
            }
        }
    });
    Ok(())
}

#[derive(Serialize)]
struct Link {
    id: String,
    rtt: u32,
    loss: f64,
    relay: bool,
}

/// Link quality per participant (short id); the UI shows it and adapts bitrates to the loss.
#[tauri::command]
async fn call_stats(st: State<'_, App>) -> R<Vec<Link>> {
    Ok(st
        .node()?
        .call_stats()
        .into_iter()
        .map(|l| Link {
            id: hex(&l.peer[..4]),
            rtt: l.rtt_ms,
            loss: l.loss,
            relay: l.relay,
        })
        .collect())
}

#[tauri::command]
async fn members(st: State<'_, App>) -> R<Vec<Member>> {
    let n = &st.node()?;
    let (me, admin) = (n.device_id(), n.admin().map_err(err)?);
    let mut prof = n.profiles().map_err(err)?;
    Ok(n.members()
        .iter()
        .map(|m| {
            let p = prof.remove(m);
            Member {
                id: short(m),
                me: *m == me,
                admin: *m == admin,
                avatar: p.as_ref().map(|p| p.1.clone()).unwrap_or_default(),
                color: p.as_ref().map(|p| p.2),
                name: p.map(|p| p.0),
            }
        })
        .collect())
}

#[tauri::command]
async fn peers(st: State<'_, App>) -> R<usize> {
    Ok(st.node()?.peers_online())
}

#[tauri::command]
async fn close_space(st: State<'_, App>) -> R<()> {
    st.close().await;
    Ok(())
}

/// Deletes a space's data from this device (the space itself lives on with the others).
#[tauri::command]
async fn remove_space(app: AppHandle, st: State<'_, App>, id: String) -> R<()> {
    let root = data_dir(&app)?;
    if st.is_open(&id) {
        st.close().await;
    }
    let mut list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    list.spaces.retain(|s| s.id != id);
    if list.last.as_deref() == Some(id.as_str()) {
        list.last = None;
    }
    list.save(&root).map_err(|e| e.to_string())?;
    vault::forget(&root, &id);
    // Fails while the closed node's last references still hold files; List::load retries at start.
    let _ = std::fs::remove_dir_all(spaces::dir(&root, &id));
    Ok(())
}

/// Settings: remember the open space's password on this device (no password at start).
#[tauri::command]
fn set_remember(app: AppHandle, st: State<App>, on: bool) -> R<()> {
    let root = data_dir(&app)?;
    let open = st.0.lock().unwrap();
    let o = open.as_ref().ok_or("locked")?;
    if on {
        vault::save(&root, &o.id, &o.password)
    } else {
        vault::forget(&root, &o.id);
        Ok(())
    }
}

#[tauri::command]
fn change_password(app: AppHandle, st: State<App>, old: String, new: String) -> R<()> {
    let root = data_dir(&app)?;
    let mut open = st.0.lock().unwrap();
    let o = open.as_mut().ok_or("locked")?;
    if o.password != old {
        return Err("Неверный текущий пароль".into());
    }
    o.node.change_password(&new).map_err(err)?;
    o.password = new;
    if vault::load(&root, &o.id).is_some() {
        vault::save(&root, &o.id, &o.password)?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(App::default())
        .manage(Visible(AtomicBool::new(true)))
        .invoke_handler(tauri::generate_handler![
            status,
            unlock,
            create_space,
            join,
            invite,
            history,
            chats,
            open_dm,
            set_profile,
            send,
            edit,
            delete,
            forward,
            pin,
            send_file,
            send_rec,
            fetch_media,
            save_media,
            open_media,
            calls_list,
            call_start,
            call_accept,
            call_reject,
            call_end,
            call_send,
            call_listen,
            call_stats,
            members,
            peers,
            set_visible,
            stash,
            close_space,
            remove_space,
            set_remember,
            change_password
        ])
        .setup(|_app| {
            #[cfg(mobile)]
            _app.handle().plugin(tauri_plugin_barcode_scanner::init())?;
            // Android has no writable temp dir by default; core and send_* use temp_dir().
            #[cfg(target_os = "android")]
            // SAFETY: set once at startup, before any command can read it.
            unsafe {
                std::env::set_var("TMPDIR", _app.path().app_cache_dir()?)
            };
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tauri app failed");
}
