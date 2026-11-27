//! Tauri shell: thin commands over mycel-core; new events go to the UI as `message`.

use mycel_core::mycel_proto::{BlobRef, CallAction, Event, EventBody, EventId, MediaKind, Preview};
use mycel_core::{Chat, Node, Opts, edits_and_pins, media, reactions};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
    /// This device is an anchor: its relay's public URL.
    anchor: Option<String>,
    n0: bool,
    relays: Vec<String>,
    mailbox: Option<String>,
    /// The router opened the anchor's port (UPnP).
    mapped: bool,
    /// The anchor's relay, stopped when dropped.
    #[cfg(not(target_os = "android"))]
    _relay: Option<mycel_core::RelayServer>,
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

    /// The open node and its space id.
    fn open(&self) -> R<(Arc<Node>, String)> {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .map(|o| (o.node.clone(), o.id.clone()))
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

/// The system notification for a new event from someone else: (title, body).
fn note(n: &Node, ev: &Event, chat: &Chat) -> Option<(String, String)> {
    if ev.author_device == n.device_id() {
        return None;
    }
    let mut who = name_of(n, &ev.author_device);
    let text = match &ev.body {
        EventBody::Call {
            action: CallAction::Offer { .. },
            ..
        } => return Some((who, "Входящий звонок".into())),
        EventBody::Text { text, .. } => text.clone(),
        EventBody::Media { .. } => "Медиа".into(),
        _ => return None,
    };
    if chat.is_none() {
        who += " · Общий чат";
    }
    Some((who, text))
}

/// `space`, `chat`: Android groups a chat's messages and adds Reply / Read buttons (Push.kt).
fn notify(app: &AppHandle, space: &str, chat: &str, title: String, body: String) {
    if app.state::<Visible>().0.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (space, chat);
        let _ = app.notification().builder().title(title).body(body).show();
    }
    #[cfg(target_os = "android")]
    let _ = vault::with_class("org.mycel.app.Push", |env, class, ctx| {
        use jni::objects::{JObject, JValue};
        let s = [space, chat, &title, &body]
            .map(|t| env.new_string(t).map(JObject::from))
            .into_iter()
            .collect::<jni::errors::Result<Vec<_>>>()?;
        env.call_static_method(
            class,
            "show",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[
                JValue::Object(ctx),
                JValue::Object(&s[0]),
                JValue::Object(&s[1]),
                JValue::Object(&s[2]),
                JValue::Object(&s[3]),
            ],
        )?;
        Ok(())
    });
}

/// Set at startup: `Push.wake` (JNI) has no other way to reach the app.
#[cfg(target_os = "android")]
static APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

/// A push came (Push.kt). A space open in this process pulls now: it shows the message itself and
/// confirms delivery. 0 → no node here, Push.kt shows its content-free notice.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "C" fn Java_org_mycel_app_Push_wake(
    _env: *mut std::ffi::c_void,
    _cls: *mut std::ffi::c_void,
) -> u8 {
    let Some(n) = APP.get().and_then(|a| a.state::<App>().node().ok()) else {
        return 0;
    };
    tauri::async_runtime::spawn(async move { n.sync_now().await });
    1
}

/// A push came and no node lives in this process (Push.kt): opens the last space with its remembered
/// password, pulls the mailbox (which sends the Delivered receipts) and closes. Returns space, chat,
/// title, body of each new message for Push.kt to show; null → no remembered password or it failed.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
extern "system" fn Java_org_mycel_app_Push_pull<'l>(
    mut env: jni::JNIEnv<'l>,
    _cls: jni::objects::JClass<'l>,
    root: jni::objects::JString<'l>,
    cache: jni::objects::JString<'l>,
) -> jni::sys::jobjectArray {
    let res = (|| {
        let root = PathBuf::from(String::from(env.get_string(&root).ok()?));
        let cache = String::from(env.get_string(&cache).ok()?);
        tmpdir(cache);
        let list = spaces::List::load(&root).ok()?;
        let id = list.last.clone()?;
        let pw = vault::load_in(&mut env, &root, &id)?;
        let notes = tauri::async_runtime::block_on(pull(&root, &id, &pw, &list))?;
        let s = env.find_class("java/lang/String").ok()?;
        let arr = env
            .new_object_array(4 * notes.len() as i32, s, jni::objects::JObject::null())
            .ok()?;
        for (i, t) in notes.into_iter().flatten().enumerate() {
            let t = env.new_string(t).ok()?;
            env.set_object_array_element(&arr, i as i32, t).ok()?;
        }
        Some(arr.into_raw())
    })();
    if res.is_none() {
        let _ = env.exception_clear();
    }
    res.unwrap_or(std::ptr::null_mut())
}

#[cfg(target_os = "android")]
fn tmpdir(cache: String) {
    if std::env::var_os("TMPDIR").is_none() {
        // SAFETY: as in `run`: core reads it only once a node is open.
        unsafe { std::env::set_var("TMPDIR", cache) };
    }
}

/// A space opened without UI (a push or a notification button with no node in this process).
#[cfg(target_os = "android")]
async fn open_bg(
    root: &Path,
    id: &str,
    pw: &str,
    list: &spaces::List,
    end: tokio::time::Instant,
) -> Option<(Arc<Node>, tokio::sync::mpsc::Receiver<Event>)> {
    let opts = Opts {
        n0: list.n0,
        extra_relays: list.relays.clone(),
        mailbox: list.mailbox.clone(),
        ..Default::default()
    };
    let (node, rx) = tokio::time::timeout_at(end, Node::open(&spaces::dir(root, id), pw, &opts))
        .await
        .ok()?
        .ok()?;
    if let Ok(t) = std::fs::read_to_string(root.join("push_token")) {
        node.set_push_token(t.trim().to_string());
    }
    Some((node, rx))
}

/// `Push_pull`'s node: everything new within ~8 s (FCM gives onMessageReceived about 10);
/// None if it did not open or ran out of time with nothing.
#[cfg(target_os = "android")]
async fn pull(root: &Path, id: &str, pw: &str, list: &spaces::List) -> Option<Vec<[String; 4]>> {
    use tokio::time::{Duration, Instant, sleep, timeout_at};
    let end = Instant::now() + Duration::from_secs(8);
    let (node, mut rx) = open_bg(root, id, pw, list, end).await?;
    let mut notes = vec![];
    // Something of a muted chat came: no content-free notice for it either.
    let mut muted = false;
    let done = timeout_at(end, async {
        node.sync_now().await;
        // Then a moment more for peers online and the receipts' mailbox posts (sent in the background).
        let quiet = sleep(Duration::from_secs(2));
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                Some(ev) = rx.recv() => {
                    let chat = node.event(&ev.id).map(|e| e.0).unwrap_or(None);
                    if let Some((title, body)) = note(&node, &ev, &chat) {
                        if list.muted(id, &chat_id(&chat)) {
                            muted = true;
                        } else {
                            notes.push([id.to_string(), chat_id(&chat), title, body]);
                        }
                    }
                }
                _ = &mut quiet => break,
            }
        }
    })
    .await
    .is_ok();
    node.close().await;
    (done || muted || !notes.is_empty()).then_some(notes)
}

/// A notification's Reply (`text`) or Read (`text` null) button (Push.kt): through the node open in
/// this process, else the space is opened without UI as in `Push_pull`, given a moment to post, closed.
/// 0 → it failed.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
extern "system" fn Java_org_mycel_app_Push_act<'l>(
    mut env: jni::JNIEnv<'l>,
    _cls: jni::objects::JClass<'l>,
    root: jni::objects::JString<'l>,
    cache: jni::objects::JString<'l>,
    space: jni::objects::JString<'l>,
    chat: jni::objects::JString<'l>,
    text: jni::objects::JString<'l>,
) -> u8 {
    use tokio::time::{Duration, Instant, sleep, timeout_at};
    let res = (|| {
        let mut get = |s: &jni::objects::JString| env.get_string(s).ok().map(String::from);
        let (root, cache, space, chat) = (get(&root)?, get(&cache)?, get(&space)?, get(&chat)?);
        let text = if text.is_null() {
            None
        } else {
            Some(get(&text)?)
        };
        let act = |n: &Node| {
            let c = chat_of(n, &chat).ok()?;
            match &text {
                Some(t) => n.send_text(&c, t, None).map(|_| ()),
                None => n.mark_read(&c),
            }
            .ok()
        };
        // Sending posts to the mailbox with tokio::spawn: run inside the runtime.
        let live = APP.get().and_then(|a| a.state::<App>().open().ok());
        if let Some((n, _)) = live.filter(|(_, id)| *id == space) {
            return tauri::async_runtime::block_on(async { act(&n) });
        }
        tmpdir(cache);
        let root = PathBuf::from(root);
        let list = spaces::List::load(&root).ok()?;
        let pw = vault::load_in(&mut env, &root, &space)?;
        tauri::async_runtime::block_on(async {
            let end = Instant::now() + Duration::from_secs(15);
            let (node, _rx) = open_bg(&root, &space, &pw, &list, end).await?;
            // The newest messages first, so Read covers them.
            let _ = timeout_at(end, node.sync_now()).await;
            let r = act(&node);
            // Peers online get it at once; the mailbox post runs in the background.
            sleep(Duration::from_secs(3)).await;
            node.close().await;
            r
        })
    })();
    if res.is_none() {
        let _ = env.exception_clear();
    }
    res.is_some() as u8
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
    /// (emoji, short ids of who set it).
    reactions: Vec<(String, Vec<String>)>,
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
    reactions: HashMap<EventId, mycel_core::Reactions>,
    by_id: HashMap<EventId, &'a Event>,
}

impl<'a> Ctx<'a> {
    fn new(n: &Node, hist: &'a [Event], chat: &str) -> Self {
        let (edits, pins) = edits_and_pins(hist);
        Ctx {
            calls: calls(n, hist, chat),
            edits,
            pins,
            reactions: reactions(hist),
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

/// `typing` event for the UI: someone is typing (`voice`: recording) in `chat`.
#[derive(Serialize, Clone)]
struct TypingSignal {
    chat: String,
    author: String,
    voice: bool,
    /// Their message arrived: the indicator goes out at once.
    done: bool,
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
    /// "direct" / "relay" / None (no connection right now).
    link: Option<&'static str>,
    /// Unix ms of the last message received from this device.
    seen: Option<u64>,
}

/// A row of the chat list.
#[derive(Serialize)]
struct ChatRow {
    /// "" = the space chat, else the partner's short id.
    id: String,
    last: Option<Msg>,
    unread: usize,
    /// No notifications on this device.
    muted: bool,
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
    // Contacts added by link are not space members (§4.2).
    let mut all = n
        .members()
        .into_iter()
        .chain(n.contacts().unwrap_or_default());
    let m = all.find(|m| short(m) == s);
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
            reactions: vec![],
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
        reactions: cx.reactions.get(&ev.id).map_or(vec![], |r| {
            let r = r
                .iter()
                .map(|(e, who)| (e.clone(), who.iter().map(short).collect()));
            r.collect()
        }),
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
    anchor: Option<String>,
    relays: Option<Vec<String>>,
    mailbox: Option<String>,
) -> R<bool> {
    let relays = relays.unwrap_or_default();
    for r in &relays {
        r.parse::<mycel_core::RelayUrl>()
            .map_err(|_| format!("неверный адрес сервера: {r}"))?;
    }
    let root = data_dir(&app)?;
    let mut list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    let id = match id {
        Some(id) => id,
        None => {
            let id = spaces::new_id();
            list.spaces.push(spaces::Space {
                id: id.clone(),
                name: name.trim().to_string(),
                muted: vec![],
            });
            id
        }
    };
    // Open already: reopened only to apply changed connection settings (anchor, n0).
    let open =
        st.0.lock()
            .unwrap()
            .as_ref()
            .filter(|o| o.id == id)
            .map(|o| {
                (
                    o.password.clone(),
                    o.anchor == anchor && o.n0 == n0 && o.relays == relays && o.mailbox == mailbox,
                    o.anchor.clone(),
                )
            });
    if let Some((_, true, _)) = open {
        return Ok(st.node()?.admin().is_ok());
    }
    let was_anchor = open.as_ref().and_then(|o| o.2.clone());
    let pw = match (&password, open) {
        (Some(p), _) => p.clone(),
        (None, Some((p, ..))) => p,
        (None, None) => vault::load(&root, &id).ok_or("no saved password")?,
    };
    // Listed before the node creates its folder, so List::load never takes it for an orphan.
    list.last = Some(id.clone());
    (list.n0, list.relays, list.mailbox) = (n0, relays.clone(), mailbox.clone());
    list.save(&root).map_err(|e| e.to_string())?;
    st.close().await;
    #[cfg(not(target_os = "android"))]
    let (relay, mapped) = match &anchor {
        Some(u) => (
            Some(mycel_core::spawn_relay(u).await.map_err(err)?),
            upnp(port_of(u), true).await.is_ok(),
        ),
        None => {
            if let Some(u) = was_anchor {
                let _ = upnp(port_of(&u), false).await;
            }
            (None, false)
        }
    };
    #[cfg(target_os = "android")]
    let mapped = {
        let _ = was_anchor;
        if anchor.is_some() {
            return Err("якорь работает только на ПК".into());
        }
        false
    };
    // Anchors' relays come via invite/peer exchange; n0 (Settings, on by default) is the public fallback.
    // An anchor also keeps every blob for members who were offline when it was sent.
    let dir = spaces::dir(&root, &id);
    let (node, mut rx) = Node::open(
        &dir,
        &pw,
        &Opts {
            n0,
            own_relay: anchor.clone(),
            keep_blobs: anchor.is_some(),
            extra_relays: relays.clone(),
            mailbox: mailbox.clone(),
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
    // The FCM token MainActivity/PushService wrote (Android): the mailbox wakes this phone.
    if let Ok(t) = std::fs::read_to_string(root.join("push_token")) {
        node.set_push_token(t.trim().to_string());
    }
    let in_space = node.admin().is_ok();
    let space = id.clone();
    *st.0.lock().unwrap() = Some(Open {
        node: node.clone(),
        id,
        password: pw,
        anchor,
        n0,
        relays,
        mailbox,
        mapped,
        #[cfg(not(target_os = "android"))]
        _relay: relay,
    });
    let (mut typing, app2) = (node.typing_events(), app.clone());
    tauri::async_runtime::spawn(async move {
        while let Some(t) = typing.recv().await {
            let _ = app2.emit(
                "typing",
                TypingSignal {
                    chat: chat_id(&t.chat),
                    author: short(&t.from),
                    voice: t.voice,
                    done: false,
                },
            );
        }
    });
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            // Another space was opened since: its own loop reports.
            let current = app.state::<App>().node();
            if !current.is_ok_and(|n| Arc::ptr_eq(&n, &node)) {
                break;
            }
            let chat = node.event(&ev.id).map(|e| e.0).unwrap_or(None);
            if let Some((who, text)) = note(&node, &ev, &chat)
                && !spaces::List::load(&root).is_ok_and(|l| l.muted(&space, &chat_id(&chat)))
            {
                notify(&app, &space, &chat_id(&chat), who, text);
            }
            if let EventBody::Call { call_id, action } = &ev.body {
                let (action, video) = match action {
                    CallAction::Offer { video } => ("offer", *video),
                    CallAction::Accept => ("accept", false),
                    CallAction::Reject => ("reject", false),
                    CallAction::End => ("end", false),
                };
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
            if matches!(ev.body, EventBody::Text { .. } | EventBody::Media { .. }) {
                let _ = app.emit(
                    "typing",
                    TypingSignal {
                        chat: chat_id(&chat),
                        author: short(&ev.author_device),
                        voice: false,
                        done: true,
                    },
                );
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

fn port_of(url: &str) -> u16 {
    url.parse::<mycel_core::RelayUrl>()
        .ok()
        .and_then(|u| u.port())
        .unwrap_or(3340)
}

/// UPnP on the router: opens (or closes) the anchor's TCP `port` to this PC.
// shortcut: a 7-day lease where a permanent one is refused, renewed on each app start.
#[cfg(not(target_os = "android"))]
async fn upnp(port: u16, open: bool) -> anyhow::Result<()> {
    use igd_next::PortMappingProtocol::TCP;
    let gw = gateway().await?;
    if open {
        // The address this PC uses towards the router.
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        s.connect(gw.addr)?;
        let local = std::net::SocketAddr::new(s.local_addr()?.ip(), port);
        if gw.add_port(TCP, port, local, 0, "Mycel").await.is_err() {
            gw.add_port(TCP, port, local, 7 * 86400, "Mycel").await?;
        }
    } else {
        gw.remove_port(TCP, port).await?;
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
async fn gateway() -> anyhow::Result<igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>> {
    let mut o = igd_next::SearchOptions::default();
    o.timeout = Some(std::time::Duration::from_secs(3));
    Ok(igd_next::aio::tokio::search_gateway(o).await?)
}

/// Lets the "send" window show picked photos through the asset protocol.
#[tauri::command]
fn allow_preview(app: AppHandle, paths: Vec<String>) {
    for p in paths {
        let _ = app.asset_protocol_scope().allow_file(p);
    }
}

/// External IPv4 of this network, for the anchor's relay URL: from the router (UPnP), else from ipify.
#[tauri::command]
async fn public_ip() -> R<String> {
    use std::io::{Read, Write};
    #[cfg(not(target_os = "android"))]
    if let Ok(gw) = gateway().await
        && let Ok(std::net::IpAddr::V4(ip)) = gw.get_external_ip().await
        && public_v4(ip)
    {
        return Ok(ip.to_string());
    }
    tauri::async_runtime::spawn_blocking(|| -> std::io::Result<String> {
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&("api.ipify.org", 80))?
            .find(|a| a.is_ipv4())
            .ok_or(std::io::ErrorKind::NotFound)?;
        let t = std::time::Duration::from_secs(10);
        let mut s = std::net::TcpStream::connect_timeout(&addr, t)?;
        s.set_read_timeout(Some(t))?;
        s.write_all(
            b"GET / HTTP/1.0
Host: api.ipify.org

",
        )?;
        let mut r = String::new();
        s.read_to_string(&mut r)?;
        Ok(r.rsplit(
            "

",
        )
        .next()
        .unwrap_or("")
        .trim()
        .to_string())
    })
    .await
    .map_err(|e| e.to_string())?
    .ok()
    .filter(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
    .ok_or_else(|| "не удалось узнать внешний IP".into())
}

/// Pull new messages now (the window came back to the foreground).
#[tauri::command]
async fn sync_now(st: State<'_, App>) -> R<()> {
    st.node()?.sync_now().await;
    Ok(())
}

/// The relay this device is reachable through right now (waits up to 5 s for one).
#[tauri::command]
async fn home_relay(st: State<'_, App>) -> R<Option<String>> {
    let n = st.node()?;
    n.relay_connected().await;
    Ok(n.home_relay())
}

/// Settings → «Проверить»: per server, the round trip in ms or why it did not answer.
#[tauri::command]
async fn check_servers(urls: Vec<String>, mailbox: bool) -> Vec<(String, Option<u64>, String)> {
    let jobs: Vec<_> = urls
        .into_iter()
        .map(|u| {
            tauri::async_runtime::spawn(async move {
                match mycel_core::check_server(&u, mailbox).await {
                    Ok(ms) => (u, Some(ms), String::new()),
                    Err(e) => (u, None, err(e)),
                }
            })
        })
        .collect();
    let mut out = vec![];
    for j in jobs {
        if let Ok(r) = j.await {
            out.push(r);
        }
    }
    out
}

/// The router can open ports (UPnP) and has a public address: this PC can be an anchor on its own.
#[tauri::command]
async fn upnp_ok() -> bool {
    #[cfg(not(target_os = "android"))]
    if let Ok(gw) = gateway().await
        && let Ok(std::net::IpAddr::V4(ip)) = gw.get_external_ip().await
    {
        return public_v4(ip);
    }
    false
}

/// Not private and not CGNAT (100.64/10): reachable from outside.
fn public_v4(ip: std::net::Ipv4Addr) -> bool {
    !ip.is_private() && !ip.is_loopback() && !(ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64)
}

/// [reaches its own relay through the public URL (waits up to 5 s), the router opened the port].
#[tauri::command]
async fn anchor_check(st: State<'_, App>) -> R<(bool, bool)> {
    let mapped = st.0.lock().unwrap().as_ref().is_some_and(|o| o.mapped);
    Ok((st.node()?.relay_connected().await, mapped))
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
    qr(st.node()?.invite().await.map_err(err)?)
}

/// One-time contact link (DESIGN.md §4.2): anyone can make one.
#[tauri::command]
async fn contact_invite(st: State<'_, App>) -> R<Invite> {
    qr(st.node()?.contact_invite().await.map_err(err)?)
}

fn qr(link: String) -> R<Invite> {
    let svg = qrcode::QrCode::new(&link)
        .map_err(|e| e.to_string())?
        .render::<qrcode::render::svg::Color>()
        .quiet_zone(true)
        .build();
    Ok(Invite { link, svg })
}

/// Safety code of this device and a member (short id), with its QR (DESIGN.md §4.5).
#[tauri::command]
async fn safety_code(st: State<'_, App>, id: String) -> R<Invite> {
    let n = &st.node()?;
    let peer = chat_of(n, &id)?.ok_or("no such member")?;
    let link = mycel_core::safety_code(&n.device_id(), &peer);
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

/// The chat is open and the window visible: Read receipt up to its newest message (§7.4).
#[tauri::command]
async fn mark_read(st: State<'_, App>, chat: String) -> R<()> {
    let n = &st.node()?;
    n.mark_read(&chat_of(n, &chat)?).map_err(err)
}

/// The space chat and every 1:1 chat with its last message and unread count.
#[tauri::command]
async fn chats(app: AppHandle, st: State<'_, App>) -> R<Vec<ChatRow>> {
    let (n, space) = &st.open()?;
    let spaces = spaces::List::load(&data_dir(&app)?).map_err(|e| e.to_string())?;
    let mut list = vec![None];
    list.extend(n.dms().map_err(err)?.into_iter().map(Some));
    list.iter()
        .map(|c| {
            let id = chat_id(c);
            let msgs = chat_msgs(n, c)?;
            let new: HashSet<_> = n.unread(c).map_err(err)?.iter().map(|i| hex(i)).collect();
            Ok(ChatRow {
                // Only messages the feed shows (a call without its view is not one).
                unread: msgs.iter().filter(|m| new.contains(&m.id)).count(),
                last: msgs.into_iter().last(),
                muted: spaces.muted(space, &id),
                id,
            })
        })
        .collect()
}

/// "Без звука": no system notifications for this chat, on this device only.
#[tauri::command]
fn set_muted(app: AppHandle, st: State<App>, chat: String, on: bool) -> R<()> {
    let (_, space) = st.open()?;
    let root = data_dir(&app)?;
    let mut list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    list.set_muted(&space, &chat, on);
    list.save(&root).map_err(|e| e.to_string())
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

/// Toggles own reaction `emoji`.
#[tauri::command]
async fn react(st: State<'_, App>, id: String, emoji: String) -> R<()> {
    st.node()?.react(unhex(&id)?, &emoji).map_err(err)?;
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

/// The user is typing (`voice`: recording) in `chat`; the core throttles to once per 3 s.
#[tauri::command]
async fn typing(st: State<'_, App>, chat: String, voice: bool) -> R<()> {
    let n = &st.node()?;
    n.typing(&chat_of(n, &chat)?, voice).map_err(err)
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
    // Space members, then contacts added by link (§4.2).
    let mut all = n.members();
    for c in n.contacts().map_err(err)? {
        if !all.contains(&c) {
            all.push(c);
        }
    }
    Ok(all
        .iter()
        .map(|m| {
            let p = prof.remove(m);
            let (link, seen) = n.link(m);
            Member {
                link: link.map(|r| if r { "relay" } else { "direct" }),
                seen,
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

/// A file the user saves: the picked path, else (Android: the save dialog gives no path) Downloads.
// shortcut: a direct path into Download works on Android 11+ only; MediaStore if 10 matters.
fn out_path(to: Option<String>, name: &str) -> PathBuf {
    to.map_or_else(
        || PathBuf::from("/storage/emulated/0/Download").join(name),
        PathBuf::from,
    )
}

/// Settings → Резервная копия: every space whose password is known (open or remembered) into one
/// `.mycelbak` sealed by `password`. Returns the file and the names of spaces left out.
#[tauri::command]
async fn backup(
    app: AppHandle,
    st: State<'_, App>,
    to: Option<String>,
    password: String,
    media: bool,
) -> R<(String, Vec<String>)> {
    if password.is_empty() {
        return Err("Введите пароль".into());
    }
    let root = data_dir(&app)?;
    let list = spaces::List::load(&root).map_err(|e| e.to_string())?;
    let open =
        st.0.lock()
            .unwrap()
            .as_ref()
            .map(|o| (o.id.clone(), o.password.clone()));
    let (mut with, mut without) = (vec![], vec![]);
    for s in list.spaces {
        let pw = match &open {
            Some((id, pw)) if *id == s.id => Some(pw.clone()),
            _ => vault::load(&root, &s.id),
        };
        match pw {
            Some(pw) => with.push((s.name, spaces::dir(&root, &s.id), pw)),
            None => without.push(s.name),
        }
    }
    let out = out_path(to, &format!("mycel-{}.mycelbak", spaces::new_id()));
    let o = out.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let refs: Vec<_> = with
            .iter()
            .map(|(n, d, p)| (n.as_str(), d.as_path(), p.as_str()))
            .collect();
        mycel_core::backup::write(&o, &password, &refs, media)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(err)?;
    Ok((out.display().to_string(), without))
}

/// Restores a backup as new spaces of a new device (DESIGN §10), their password the backup's,
/// remembered; the first one opens next. Nothing is left behind on failure.
#[tauri::command]
async fn restore_backup(app: AppHandle, path: String, password: String) -> R<()> {
    let root = data_dir(&app)?;
    let r = root.clone();
    let pw = password.clone();
    let made = Arc::new(Mutex::new(Vec::<String>::new()));
    let m = made.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        mycel_core::backup::restore(Path::new(&path), &pw, &pw, |name| {
            let mut list = spaces::List::load(&r)?;
            let id = spaces::new_id();
            list.spaces.push(spaces::Space {
                id: id.clone(),
                name: name.into(),
                muted: vec![],
            });
            if m.lock().unwrap().is_empty() {
                list.last = Some(id.clone());
            }
            // Listed before the folder exists, so List::load never takes it for an orphan.
            list.save(&r)?;
            m.lock().unwrap().push(id.clone());
            Ok(spaces::dir(&r, &id))
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    let made = made.lock().unwrap().clone();
    if let Err(e) = res {
        let mut list = spaces::List::load(&root).map_err(|e| e.to_string())?;
        list.spaces.retain(|s| !made.contains(&s.id));
        if list.last.as_ref().is_some_and(|l| made.contains(l)) {
            list.last = None;
        }
        list.save(&root).map_err(|e| e.to_string())?;
        for id in &made {
            let _ = std::fs::remove_dir_all(spaces::dir(&root, id));
        }
        return Err(if e.to_string() == mycel_core::backup::BAD_PASSWORD {
            "Неверный пароль или файл повреждён".into()
        } else {
            err(e)
        });
    }
    for id in &made {
        vault::save(&root, id, &password)?;
    }
    Ok(())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A chat as a readable HTML page, unencrypted, its downloaded media in `<name>_files` beside it.
/// Returns the page's path.
#[tauri::command]
async fn export_chat(st: State<'_, App>, chat: String, to: Option<String>) -> R<String> {
    let n = &st.node()?;
    let c = chat_of(n, &chat)?;
    let title = match &c {
        None => "Общий чат".to_string(),
        Some(d) => name_of(n, d),
    };
    let out = out_path(to, &format!("mycel-chat-{}.html", spaces::new_id()));
    let stem = out
        .file_stem()
        .ok_or("bad name")?
        .to_string_lossy()
        .into_owned();
    let files = out.with_file_name(format!("{stem}_files"));
    let mut html = format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title>\
         <style>body{{font:15px sans-serif;max-width:46em;margin:auto;padding:1em}}\
         p{{margin:.6em 0}}small{{color:#888}}img,video{{max-width:100%}}</style><h1>{t}</h1>",
        t = esc(&title)
    );
    for m in chat_msgs(n, &c)? {
        html += &format!(
            "<p><b>{}</b> <small data-ts={}></small><br>",
            esc(&m.author),
            m.ts
        );
        if let Some(md) = &m.media {
            let name = md.name.clone().unwrap_or_else(|| md.kind.into());
            match &md.path {
                Some(p) => {
                    let src = Path::new(p);
                    let file = src.file_name().ok_or("bad media")?;
                    std::fs::create_dir_all(&files).map_err(|e| e.to_string())?;
                    std::fs::copy(src, files.join(file)).map_err(|e| e.to_string())?;
                    let enc = |s: &str| {
                        percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC)
                            .to_string()
                    };
                    let href = format!("{}/{}", enc(&stem), enc(&file.to_string_lossy()));
                    html += &match md.kind {
                        "photo" => format!("<img src=\"{href}\"><br>"),
                        "video" | "videonote" => {
                            format!("<video controls src=\"{href}\"></video><br>")
                        }
                        "voice" => format!("<audio controls src=\"{href}\"></audio><br>"),
                        _ => format!("📎 <a href=\"{href}\">{}</a><br>", esc(&name)),
                    };
                }
                None => html += &format!("📎 {} <small>(не скачано)</small><br>", esc(&name)),
            }
        }
        html += &esc(&m.text).replace('\n', "<br>");
        html += "</p>";
    }
    html += "<script>for(const e of document.querySelectorAll('[data-ts]'))e.textContent=new Date(+e.dataset.ts).toLocaleString()</script>";
    std::fs::write(&out, html).map_err(|e| e.to_string())?;
    Ok(out.display().to_string())
}

/// Brings the window back from the tray.
#[cfg(desktop)]
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    app.state::<Visible>().0.store(true, Ordering::Relaxed);
}

/// Like other messengers: lives in the tray, closing the window only hides it.
#[cfg(desktop)]
fn tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let show = MenuItem::with_id(app, "show", "Открыть Mycel", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
    let mut t = TrayIconBuilder::new();
    if let Some(i) = app.default_window_icon() {
        t = t.icon(i.clone());
    }
    t.tooltip("Mycel")
        .menu(&Menu::with_items(app, &[&show, &quit])?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id.as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|t, e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                show_main(t.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Start with Windows: `on` None only reads.
#[tauri::command]
fn autostart(app: AppHandle, on: Option<bool>) -> R<bool> {
    #[cfg(desktop)]
    {
        use tauri_plugin_autostart::ManagerExt;
        let m = app.autolaunch();
        match on {
            Some(true) => m.enable().map_err(|e| e.to_string())?,
            Some(false) => m.disable().map_err(|e| e.to_string())?,
            None => {}
        }
        m.is_enabled().map_err(|e| e.to_string())
    }
    #[cfg(mobile)]
    {
        let _ = (app, on);
        Ok(false)
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let b = tauri::Builder::default();
    // A second launch (shortcut, autostart) shows the running app instead of opening the database twice.
    #[cfg(desktop)]
    let b = b
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_main(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = w.hide();
                w.state::<Visible>().0.store(false, Ordering::Relaxed);
            }
        });
    b.plugin(tauri_plugin_dialog::init())
        // mycel://join/… links; a second launch hands them over through single-instance.
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .manage(App::default())
        .manage(Visible(AtomicBool::new(true)))
        .invoke_handler(tauri::generate_handler![
            status,
            unlock,
            backup,
            restore_backup,
            export_chat,
            public_ip,
            anchor_check,
            upnp_ok,
            home_relay,
            check_servers,
            sync_now,
            allow_preview,
            create_space,
            join,
            invite,
            contact_invite,
            safety_code,
            history,
            chats,
            open_dm,
            set_profile,
            send,
            edit,
            delete,
            forward,
            pin,
            react,
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
            typing,
            call_stats,
            members,
            peers,
            set_visible,
            mark_read,
            set_muted,
            stash,
            close_space,
            remove_space,
            set_remember,
            change_password,
            autostart
        ])
        .setup(|_app| {
            #[cfg(desktop)]
            {
                tray(_app)?;
                // Started with Windows: straight to the tray.
                if std::env::args().any(|a| a == "--hidden")
                    && let Some(w) = _app.get_webview_window("main")
                {
                    let _ = w.hide();
                    _app.state::<Visible>().0.store(false, Ordering::Relaxed);
                }
                // On by default, once: like other messengers (Settings turns it off).
                let mark = _app.path().app_data_dir()?.join("autostart-default");
                if !mark.exists() {
                    use tauri_plugin_autostart::ManagerExt;
                    let _ = _app.autolaunch().enable();
                    let _ = std::fs::create_dir_all(mark.parent().unwrap_or(&mark));
                    let _ = std::fs::write(&mark, b"");
                }
            }
            #[cfg(mobile)]
            _app.handle().plugin(tauri_plugin_barcode_scanner::init())?;
            // The installer registers the scheme; a dev build has none.
            #[cfg(all(debug_assertions, any(windows, target_os = "linux")))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let _ = _app.deep_link().register_all();
            }
            // Android has no writable temp dir by default; core and send_* use temp_dir().
            #[cfg(target_os = "android")]
            // SAFETY: set once at startup, before any command can read it.
            unsafe {
                std::env::set_var("TMPDIR", _app.path().app_cache_dir()?)
            };
            #[cfg(target_os = "android")]
            let _ = APP.set(_app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tauri app failed");
}
