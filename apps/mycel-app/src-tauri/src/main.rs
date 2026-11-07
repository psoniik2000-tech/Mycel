//! Tauri shell: thin commands over mycel-core; new events go to the UI as `message`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mycel_core::mycel_proto::{BlobRef, Event, EventBody, MediaKind, Preview};
use mycel_core::{Node, Opts, media};
use serde::Serialize;
use std::path::{Path, PathBuf};
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
    /// Own messages: 0 ⏱, 1 ✓, 2 ✓✓ (§7.4).
    status: u8,
    media: Option<Media>,
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

fn msg(n: &Node, ev: &Event) -> Option<Msg> {
    let (text, media) = match &ev.body {
        EventBody::Text { text, .. } => (text.clone(), None),
        EventBody::Media {
            kind,
            blob,
            caption,
            ..
        } => (
            caption.clone().unwrap_or_default(),
            Some(media_view(n, *kind, blob)),
        ),
        _ => return None,
    };
    Some(Msg {
        id: short(&ev.id),
        author: short(&ev.author_device),
        mine: ev.author_device == n.device_id(),
        text,
        ts: ev.timestamp_ms,
        status: 0,
        media,
    })
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

/// The Media event with this short id, and its blob.
fn find_media(n: &Node, id: &str) -> R<(Event, BlobRef)> {
    let hist = n.history().map_err(err)?;
    let ev = hist
        .into_iter()
        .find(|e| short(&e.id) == id)
        .ok_or("no such message")?;
    let b = media(&ev).ok_or("not a media message")?.1.clone();
    Ok((ev, b))
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

/// (short id, status) of own messages.
fn statuses(n: &Node) -> Vec<(String, u8)> {
    let s = n.statuses().unwrap_or_default();
    s.iter().map(|(id, st)| (short(id), *st)).collect()
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
    // shortcut: no app setting for the n0 fallback yet; the relays come from the anchors via invite/peer exchange.
    let dir = data_dir(&app)?;
    let (node, mut rx) = Node::open(&dir, &password, &Opts::default())
        .await
        .map_err(err)?;
    // <img>/<video> load decrypted media through the asset protocol.
    let _ = app.asset_protocol_scope().allow_directory(
        std::path::absolute(dir.join("media")).map_err(|e| e.to_string())?,
        true,
    );
    let in_space = node.admin().is_ok();
    st.0.set(node.clone()).map_err(|_| "already unlocked")?;
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match msg(&node, &ev) {
                Some(m) => {
                    let _ = app.emit("message", m);
                }
                // A receipt (or other non-text event) may change the ticks.
                None => {
                    let _ = app.emit("status", statuses(&node));
                }
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
    let link = st.node()?.invite().await.map_err(err)?;
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
    let st: std::collections::HashMap<_, _> = statuses(n).into_iter().collect();
    Ok(n.history()
        .map_err(err)?
        .iter()
        .filter_map(|e| msg(n, e))
        .map(|m| Msg {
            status: st.get(&m.id).copied().unwrap_or(0),
            ..m
        })
        .collect())
}

#[tauri::command]
async fn send(st: State<'_, App>, text: String) -> R<Msg> {
    let n = st.node()?;
    let ev = n.send_text(&text).map_err(err)?;
    msg(n, &ev).ok_or_else(|| "not a text event".into())
}

/// 📎 / drag-and-drop: photos are re-encoded (no EXIF) unless `original`, then they go as a file.
#[tauri::command]
async fn send_file(st: State<'_, App>, path: String, original: bool) -> R<Msg> {
    let n = st.node()?;
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
        let tmp = std::env::temp_dir().join(format!("mycel-{}.jpg", std::process::id()));
        std::fs::write(&tmp, &ph.jpeg).map_err(|e| e.to_string())?;
        let mut m = blob_meta("image/jpeg", name.map(|n| format!("{n}.jpg")));
        m.preview = Some(Preview::Blurhash(ph.blurhash));
        m.dims = Some(ph.dims);
        let r = n.send_media(MediaKind::Photo, &tmp, m, None).await;
        let _ = std::fs::remove_file(&tmp);
        r.map_err(err)?
    } else {
        // shortcut: video goes as is, no re-encode or preview frame; ffmpeg when people complain about size.
        let kind = if mime.starts_with("video/") {
            MediaKind::Video
        } else {
            MediaKind::File
        };
        n.send_media(kind, src, blob_meta(mime, name), None)
            .await
            .map_err(err)?
    };
    msg(n, &ev).ok_or_else(|| "not a media event".into())
}

/// A recording from the WebView (raw body); headers: kind (voice|videonote), mime, duration, wave (csv).
#[tauri::command]
async fn send_rec(st: State<'_, App>, request: tauri::ipc::Request<'_>) -> R<Msg> {
    let n = st.node()?;
    let tauri::ipc::InvokeBody::Raw(data) = request.body() else {
        return Err("expected raw bytes".into());
    };
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
    let tmp = std::env::temp_dir().join(format!("mycel-rec-{}", std::process::id()));
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let r = n.send_media(kind, &tmp, m, None).await;
    let _ = std::fs::remove_file(&tmp);
    msg(n, &r.map_err(err)?).ok_or_else(|| "not a media event".into())
}

#[derive(Serialize, Clone)]
struct Progress {
    id: String,
    done: u64,
}

/// Downloads (resuming) and decrypts a media message; emits `progress`; returns the file path.
#[tauri::command]
async fn fetch_media(app: AppHandle, st: State<'_, App>, id: String) -> R<String> {
    let n = st.node()?;
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
    let n = st.node()?;
    let p = n.media_path(&find_media(n, &id)?.1).await.map_err(err)?;
    std::fs::copy(p, to).map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn open_media(st: State<'_, App>, id: String) -> R<()> {
    let n = st.node()?;
    let p = n.media_path(&find_media(n, &id)?.1).await.map_err(err)?;
    tauri_plugin_opener::open_path(p, None::<&str>).map_err(|e| e.to_string())
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
        .plugin(tauri_plugin_dialog::init())
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            status,
            unlock,
            create_space,
            join,
            invite,
            history,
            send,
            send_file,
            send_rec,
            fetch_media,
            save_media,
            open_media,
            members,
            peers,
            quit
        ])
        .run(tauri::generate_context!())
        .expect("tauri app failed");
}
