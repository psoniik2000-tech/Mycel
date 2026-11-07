import { createEffect, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import {
  ArrowLeft, Check, CheckCheck, Clock, Copy, Hash, Link, LogOut, MessageCircle, Mic, MicOff, Paperclip, Phone, PhoneMissed, PhoneOff, Plus, Send,
  Settings, UserPlus, Users, Video, VideoOff, X,
} from "lucide-solid";
import logo from "../app-icon.svg";
import { label, Media, MediaView } from "./media";
import { record, Recording, waveform } from "./capture";
import { CallMedia, Peer, ring } from "./call";

// status (own messages, §7.4): 0 ⏱ sending, 1 ✓ some member has it, 2 ✓✓ every member has it.
type Msg = { id: string; author: string; mine: boolean; text: string; ts: number; status: number; media: Media | null; call: CallView | null };
type CallView = { id: string; video: boolean; author: string; mine: boolean; ts: number; duration_ms: number | null; missed: boolean; active: boolean };
type CallSignal = { id: string; action: "offer" | "accept" | "reject" | "end"; video: boolean; author: string; ts: number };
type Active = { id: string; video: boolean; started: number; peers: string[] };
type Member = { id: string; me: boolean; admin: boolean };
type Status = { dir: string; exists: boolean; unlocked: boolean; in_space: boolean };
type Tab = "chats" | "calls" | "people" | "settings";

const time = (ts: number) => new Date(ts).toLocaleTimeString("ru", { hour: "2-digit", minute: "2-digit" });
const hue = (id: string) => parseInt(id.slice(0, 4), 16) % 360;
const errText = (e: unknown) => String(e);
const clock = (ms: number) => `${Math.floor(ms / 60000)}:${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}`;
const callLabel = (c: CallView) =>
  c.missed ? (c.mine ? "Без ответа" : "Пропущенный звонок")
  : c.active ? "Идёт звонок"
  : `${c.video ? "Видеозвонок" : "Звонок"}, ${clock(c.duration_ms ?? 0)}`;

function Avatar(p: { id: string }) {
  return (
    <div class="avatar" style={{ background: `hsl(${hue(p.id)} 45% 55%)` }}>
      {p.id.slice(0, 2).toUpperCase()}
    </div>
  );
}

// shortcut: 🟡 (Tor / LAN only) needs those transports (phase 4); for now online = ≥1 open peer connection.
function NetDot(p: { n: number }) {
  return (
    <span class="net" title={p.n ? `Подключено участников: ${p.n}` : "Никого не найдено, сообщения ждут в очереди"}>
      <i classList={{ on: p.n > 0 }} />
      {p.n ? `На связи · ${p.n}` : "Нет связи"}
    </span>
  );
}

export default function App() {
  const [status, setStatus] = createSignal<Status>();
  const [stage, setStage] = createSignal<"lock" | "onboard" | "main">("lock");
  const [theme, setTheme] = createSignal(localStorage.getItem("theme") ?? "system");
  createEffect(() => {
    document.documentElement.dataset.theme = theme();
    localStorage.setItem("theme", theme());
  });
  onMount(async () => {
    const s = await invoke<Status>("status");
    if (s.unlocked) setStage(s.in_space ? "main" : "onboard");
    setStatus(s);
  });
  return (
    <Show when={status()}>
      {(s) => (
        <Switch>
          <Match when={stage() === "lock"}>
            <Lock exists={s().exists} onDone={(inSpace) => setStage(inSpace ? "main" : "onboard")} />
          </Match>
          <Match when={stage() === "onboard"}>
            <Onboard onDone={() => setStage("main")} />
          </Match>
          <Match when={stage() === "main"}>
            <Main dir={s().dir} theme={theme()} setTheme={setTheme} />
          </Match>
        </Switch>
      )}
    </Show>
  );
}

function Lock(p: { exists: boolean; onDone: (inSpace: boolean) => void }) {
  const [pw, setPw] = createSignal("");
  const [pw2, setPw2] = createSignal("");
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const submit = async (e: Event) => {
    e.preventDefault();
    if (!p.exists && pw() !== pw2()) return setErr("Пароли не совпадают");
    if (!pw()) return setErr("Введите пароль");
    setBusy(true);
    setErr("");
    try {
      p.onDone(await invoke<boolean>("unlock", { password: pw(), n0: localStorage.getItem("n0") !== "0" }));
    } catch (e) {
      setErr(p.exists ? "Неверный пароль" : errText(e));
    }
    setBusy(false);
  };
  return (
    <div class="center">
      <form class="card intro" onSubmit={submit}>
        <img src={logo} class="logo" alt="" />
        <h1>Mycel</h1>
        <p class="muted">
          {p.exists ? "Введите пароль, чтобы открыть переписку" : "Придумайте пароль: им шифруются все данные на этом устройстве"}
        </p>
        <input type="password" placeholder="Пароль" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} autofocus />
        <Show when={!p.exists}>
          <input type="password" placeholder="Повторите пароль" value={pw2()} onInput={(e) => setPw2(e.currentTarget.value)} />
        </Show>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <button class="primary" disabled={busy()}>{busy() ? "Открываем…" : p.exists ? "Открыть" : "Продолжить"}</button>
      </form>
    </div>
  );
}

function Onboard(p: { onDone: () => void }) {
  const [link, setLink] = createSignal("");
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal("");
  const run = async (what: string, cmd: string, args?: Record<string, unknown>) => {
    setBusy(what);
    setErr("");
    try {
      await invoke(cmd, args);
      p.onDone();
    } catch (e) {
      setErr(errText(e));
    }
    setBusy("");
  };
  return (
    <div class="center">
      <div class="card intro">
        <img src={logo} class="logo" alt="" />
        <h1>Добро пожаловать</h1>
        <p class="muted">Создайте своё пространство или вступите в существующее по ссылке-приглашению</p>
        <button class="primary" disabled={!!busy()} onClick={() => run("create", "create_space")}>
          <Plus size={18} /> {busy() === "create" ? "Создаём…" : "Создать пространство"}
        </button>
        <div class="or"><span>или</span></div>
        <textarea rows={3} placeholder="mycel://join/…" value={link()} onInput={(e) => setLink(e.currentTarget.value)} />
        <button disabled={!!busy() || !link().trim()} onClick={() => run("join", "join", { link: link() })}>
          <Link size={18} /> {busy() === "join" ? "Вступаем…" : "Вступить по ссылке"}
        </button>
        <Show when={err()}><p class="error">{err()}</p></Show>
      </div>
    </div>
  );
}

function Main(p: { dir: string; theme: string; setTheme: (t: string) => void }) {
  const [tab, setTab] = createSignal<Tab>("chats");
  const [chatOpen, setChatOpen] = createSignal(false);
  const [msgs, setMsgs] = createSignal<Msg[]>([]);
  const [members, setMembers] = createSignal<Member[]>([]);
  const [peers, setPeers] = createSignal(0);
  const [unread, setUnread] = createSignal(0);
  const [focused, setFocused] = createSignal(document.hasFocus());
  const wideQuery = matchMedia("(min-width: 900px)");
  const [wide, setWide] = createSignal(wideQuery.matches);
  const chatVisible = () => (wide() || chatOpen()) && focused();
  createEffect(() => chatVisible() && setUnread(0));

  const loadMembers = () => invoke<Member[]>("members").then(setMembers);
  const reload = () => invoke<Msg[]>("history").then(setMsgs);

  // Calls (§9): `incoming` rings full screen; `active` is the call screen (started = 0 while ringing out).
  const [incoming, setIncoming] = createSignal<CallSignal>();
  const [active, setActive] = createSignal<Active>();
  const [callErr, setCallErr] = createSignal("");
  let media: CallMedia | undefined;
  const [tiles, setTiles] = createSignal<Peer[]>([]);
  const endCall = async () => {
    media?.stop();
    media = undefined;
    setTiles([]);
    setActive(undefined);
    await invoke("call_end").catch(() => {});
    reload();
  };
  const openMedia = async (video: boolean) => {
    media = new CallMedia(setTiles);
    await media.start(video);
  };
  const startCall = async (video: boolean) => {
    if (active()) return;
    setCallErr("");
    try {
      await openMedia(video);
      setActive({ id: await invoke<string>("call_start", { video }), video, started: 0, peers: [] });
      reload();
      // Nobody answers in 45 s: hang up (the feed shows "no answer").
      const id = active()!.id;
      setTimeout(() => active()?.id === id && !active()!.started && endCall(), 45_000);
    } catch (e) {
      setCallErr("Не удалось начать звонок: " + errText(e));
      endCall();
    }
  };
  const acceptCall = async () => {
    const c = incoming()!;
    setIncoming(undefined);
    if (active()) await endCall();
    setCallErr("");
    try {
      await openMedia(c.video);
      const peers = await invoke<string[]>("call_accept", { id: c.id });
      setActive({ id: c.id, video: c.video, started: Date.now(), peers });
    } catch (e) {
      setCallErr("Не удалось ответить: " + errText(e));
      endCall();
    }
  };
  const rejectCall = () => {
    const c = incoming()!;
    setIncoming(undefined);
    invoke("call_reject", { id: c.id });
  };
  const onCall = (c: CallSignal) => {
    const a = active();
    if (c.action === "offer" && Date.now() - c.ts < 60_000 && !incoming()) setIncoming(c);
    if ((c.action === "end" || c.action === "reject") && incoming()?.id === c.id && incoming()!.author === c.author) setIncoming(undefined);
    if (a?.id === c.id) {
      if (c.action === "accept") setActive({ ...a, started: a.started || Date.now(), peers: [...new Set([...a.peers, c.author])] });
      if (c.action === "end" || c.action === "reject") {
        const peers = a.peers.filter((p) => p !== c.author);
        setActive({ ...a, peers });
        // Everyone left (or the only other member declined): hang up.
        if ((a.started && !peers.length) || (c.action === "reject" && members().length <= 2)) endCall();
      }
    }
    reload();
  };
  const pollPeers = () => invoke<number>("peers").then(setPeers);
  onMount(async () => {
    setMsgs(await invoke<Msg[]>("history"));
    loadMembers();
    pollPeers();
    // A join comes as an MLS commit, not a message: poll members with peers.
    const timer = setInterval(() => (pollPeers(), loadMembers()), 3000);
    const un = await listen<Msg>("message", (e) => {
      setMsgs((m) => [...m, e.payload]);
      if (!chatVisible()) setUnread((n) => n + 1);
      loadMembers();
    });
    const unCall = await listen<CallSignal>("call", (e) => onCall(e.payload));
    const unStatus = await listen<[string, number][]>("status", (e) => {
      const st = new Map(e.payload);
      setMsgs((all) => all.map((m) => (st.has(m.id) ? { ...m, status: st.get(m.id)! } : m)));
    });
    const onWide = () => setWide(wideQuery.matches);
    const onFocus = () => setFocused(true);
    const onBlur = () => setFocused(false);
    wideQuery.addEventListener("change", onWide);
    addEventListener("focus", onFocus);
    addEventListener("blur", onBlur);
    onCleanup(() => {
      clearInterval(timer);
      un();
      unCall();
      unStatus();
      media && endCall();
      wideQuery.removeEventListener("change", onWide);
      removeEventListener("focus", onFocus);
      removeEventListener("blur", onBlur);
    });
  });

  const last = () => msgs().at(-1);
  const titles = { chats: "Чаты", calls: "Звонки", people: "Люди", settings: "Настройки" };
  const navItem = (t: Tab, label: string, Icon: typeof Users) => (
    <button classList={{ nav: true, active: tab() === t }} onClick={() => { setTab(t); setChatOpen(false); }} title={label}>
      <Icon size={22} />
      <span>{label}</span>
      <Show when={t === "chats" && unread()}><b class="badge">{unread()}</b></Show>
    </button>
  );

  return (
    <div class="app" classList={{ "chat-open": chatOpen() }}>
      <nav class="rail">
        <img src={logo} class="rail-logo" alt="Mycel" />
        {navItem("chats", "Чаты", MessageCircle)}
        {navItem("calls", "Звонки", Phone)}
        {navItem("people", "Люди", Users)}
        {navItem("settings", "Настройки", Settings)}
      </nav>

      <section class="panel">
        <header class="bar">
          <h2>{titles[tab()]}</h2>
          <NetDot n={peers()} />
        </header>
        <Switch>
          <Match when={tab() === "chats"}>
            <button class="row chat-row" classList={{ active: wide() }} onClick={() => setChatOpen(true)}>
              <div class="avatar group"><Hash size={20} /></div>
              <div class="grow">
                <div class="row-top"><b>Общий чат</b><small class="muted">{last() ? time(last()!.ts) : ""}</small></div>
                <div class="row-bottom">
                  <span class="muted ellipsis">{last() ? (last()!.mine ? "Вы: " : "") + (last()!.call ? callLabel(last()!.call!) : last()!.media ? label(last()!.media!) : last()!.text) : "Сообщений пока нет"}</span>
                  <Show when={unread()}><b class="badge">{unread()}</b></Show>
                </div>
              </div>
            </button>
          </Match>
          <Match when={tab() === "calls"}>
            <Calls msgs={msgs()} />
          </Match>
          <Match when={tab() === "people"}>
            <People members={members()} />
          </Match>
          <Match when={tab() === "settings"}>
            <div class="scroll pad">
              <h3>Тема</h3>
              <div class="segmented">
                <For each={[["system", "Как в системе"], ["light", "Светлая"], ["dark", "Тёмная"]]}>
                  {([v, label]) => (
                    <button classList={{ active: p.theme === v }} onClick={() => p.setTheme(v)}>{label}</button>
                  )}
                </For>
              </div>
              <h3>Связь</h3>
              <label class="check">
                <input type="checkbox" checked={localStorage.getItem("n0") !== "0"} onChange={(e) => localStorage.setItem("n0", e.currentTarget.checked ? "1" : "0")} />
                Резервный публичный сервер связи (n0)
              </label>
              <p class="muted">Помогает соединиться, когда нет своего якоря. Можно выключить, если якорь настроен. Применяется после перезапуска.</p>
              <h3>Данные</h3>
              <p class="muted">Зашифрованная база хранится здесь:</p>
              <code class="path">{p.dir}</code>
              <button class="danger" onClick={() => invoke("quit")}><LogOut size={18} /> Выйти</button>
            </div>
          </Match>
        </Switch>
      </section>

      <Chat msgs={msgs()} members={members().length} peers={peers()} back={() => setChatOpen(false)}
        onSent={(m) => setMsgs((all) => [...all, m])} inCall={!!active()} onCall={startCall} callErr={callErr()} />
      <Show when={incoming()}>
        <Incoming c={incoming()!} onAccept={acceptCall} onReject={rejectCall} />
      </Show>
      <Show when={active()}>
        <CallScreen a={active()!} tiles={tiles()} media={media!} onEnd={endCall} />
      </Show>
    </div>
  );
}

function People(p: { members: Member[] }) {
  const [inv, setInv] = createSignal<{ link: string; svg: string }>();
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal("");
  const [copied, setCopied] = createSignal(false);
  const amAdmin = () => p.members.some((m) => m.me && m.admin);
  const invite = async () => {
    setBusy(true);
    setErr("");
    try {
      setInv(await invoke<{ link: string; svg: string }>("invite"));
    } catch (e) {
      setErr(errText(e));
    }
    setBusy(false);
  };
  const copy = async () => {
    await navigator.clipboard.writeText(inv()!.link);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };
  return (
    <div class="scroll">
      <div class="pad">
        <button class="primary" disabled={!amAdmin() || busy()} onClick={invite}>
          <UserPlus size={18} /> {busy() ? "Готовим ссылку…" : "Пригласить"}
        </button>
        <Show when={!amAdmin()}><p class="muted small">Приглашать может только администратор</p></Show>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <Show when={inv()}>
          <div class="card invite">
            <div class="qr" innerHTML={inv()!.svg} />
            <p class="muted small">Ссылка одноразовая: по ней сможет вступить один человек</p>
            <code class="path">{inv()!.link}</code>
            <button onClick={copy}>{copied() ? <Check size={18} /> : <Copy size={18} />} {copied() ? "Скопировано" : "Копировать"}</button>
          </div>
        </Show>
      </div>
      <p class="section muted">Участники · {p.members.length}</p>
      <For each={p.members}>
        {(m) => (
          <div class="row">
            <Avatar id={m.id} />
            <div class="grow">
              <b>{m.me ? "Вы" : `Участник ${m.id}`}</b>
              <div class="muted small mono">{m.id}</div>
            </div>
            <Show when={m.admin}><span class="tag">админ</span></Show>
          </div>
        )}
      </For>
    </div>
  );
}

function Chat(p: {
  msgs: Msg[]; members: number; peers: number; back: () => void; onSent: (m: Msg) => void;
  inCall: boolean; onCall: (video: boolean) => void; callErr: string;
}) {
  const [text, setText] = createSignal("");
  const [err, setErr] = createSignal("");
  const [view, setView] = createSignal("");
  const [original, setOriginal] = createSignal(false);
  const [drag, setDrag] = createSignal(false);
  const [video, setVideo] = createSignal(false);
  const [rec, setRec] = createSignal<Recording>();
  const [elapsed, setElapsed] = createSignal(0);
  const [cancelling, setCancelling] = createSignal(false);
  let preview!: HTMLVideoElement;

  const sendFiles = async (paths: string[]) => {
    for (const path of paths) {
      try {
        p.onSent(await invoke<Msg>("send_file", { path, original: original() }));
        setErr("");
      } catch (e) {
        setErr(errText(e));
      }
    }
  };
  const attach = async () => {
    const paths = await open({ multiple: true });
    if (paths) sendFiles(paths);
  };
  onMount(async () => {
    // Media that loads later grows the list: keep the newest message in view.
    const bottom = () => (list.scrollTop = list.scrollHeight);
    list.addEventListener("load", bottom, true);
    list.addEventListener("loadedmetadata", bottom, true);
    const un = await getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "drop") {
        setDrag(false);
        sendFiles(e.payload.paths);
      } else setDrag(e.payload.type !== "leave");
    });
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setView("");
      if (rec()) stopRec(false);
    };
    addEventListener("keydown", esc);
    onCleanup(() => {
      un();
      removeEventListener("keydown", esc);
    });
  });

  // 🎤/📹: a tap switches the mode, holding records; release sends, slide left (or Esc) cancels.
  let holdTimer = 0, tick = 0, startX = 0, holding = false;
  const stopRec = async (send: boolean) => {
    const r = rec();
    clearInterval(tick);
    setRec(undefined);
    setCancelling(false);
    if (!r) return;
    if (!send || Date.now() - r.started < 500) return r.cancel();
    const blob = await r.stop();
    const kind = video() ? "videonote" : "voice";
    const wave = kind === "voice" ? await waveform(blob) : [];
    try {
      p.onSent(await invoke<Msg>("send_rec", new Uint8Array(await blob.arrayBuffer()), {
        headers: { kind, mime: r.mime, duration: String(Date.now() - r.started), wave: wave.join(",") },
      }));
    } catch (e) {
      setErr(errText(e));
    }
  };
  const down = (e: PointerEvent) => {
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    startX = e.clientX;
    holding = true;
    holdTimer = window.setTimeout(async () => {
      try {
        const r = await record(video());
        if (!holding) return r.cancel();
        setRec(r);
        setElapsed(0);
        if (video()) preview.srcObject = r.stream;
        tick = window.setInterval(() => {
          setElapsed(Date.now() - r.started);
          if (video() && Date.now() - r.started >= 60_000) stopRec(true);
        }, 200);
      } catch (e) {
        setErr("Нет доступа к микрофону или камере: " + errText(e));
      }
    }, 300);
  };
  const up = () => {
    holding = false;
    clearTimeout(holdTimer);
    if (rec()) stopRec(!cancelling());
    else setVideo(!video());
  };
  const move = (e: PointerEvent) => rec() && setCancelling(startX - e.clientX > 80);
  let list!: HTMLDivElement;
  let input!: HTMLTextAreaElement;
  createEffect(() => {
    p.msgs.length;
    queueMicrotask(() => (list.scrollTop = list.scrollHeight));
  });
  const grow = () => {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  };
  const send = async () => {
    const t = text().trim();
    if (!t) return;
    try {
      p.onSent(await invoke<Msg>("send", { text: t }));
      setText("");
      setErr("");
      queueMicrotask(grow);
    } catch (e) {
      setErr(errText(e));
    }
  };
  return (
    <section class="chat">
      <header class="bar">
        <button class="icon back" onClick={p.back} title="Назад"><ArrowLeft size={20} /></button>
        <div class="avatar group small-av"><Hash size={18} /></div>
        <div class="grow">
          <b>Общий чат</b>
          <div class="muted small">{p.members} участн.</div>
        </div>
        <NetDot n={p.peers} />
        <button class="icon" disabled={p.inCall || p.members < 2} onClick={() => p.onCall(false)}
          title={p.members > 2 ? "Групповой звонок" : "Звонок"}><Phone size={20} /></button>
        <button class="icon" disabled={p.inCall || p.members < 2} onClick={() => p.onCall(true)}
          title={p.members > 2 ? "Групповой видеозвонок" : "Видеозвонок"}><Video size={20} /></button>
      </header>
      <div class="msgs" ref={list}>
        <Show when={!p.msgs.length}><p class="empty muted">Здесь пока тихо. Напишите первым!</p></Show>
        <For each={p.msgs}>
          {(m, i) => m.call ? (
            <div class="call-line" classList={{ missed: m.call.missed }}>
              {m.call.missed ? <PhoneMissed size={14} /> : m.call.video ? <Video size={14} /> : <Phone size={14} />}
              {m.mine ? "" : `Участник ${m.author} · `}{callLabel(m.call)} · {time(m.ts)}
            </div>
          ) : (
            <div class="msg" classList={{ mine: m.mine, cont: p.msgs[i() - 1]?.author === m.author }}>
              <div class="bubble">
                <Show when={!m.mine && p.msgs[i() - 1]?.author !== m.author}>
                  <div class="author" style={{ color: `hsl(${hue(m.author)} 50% 50%)` }}>Участник {m.author}</div>
                </Show>
                <Show when={m.media}><MediaView id={m.id} m={m.media!} onView={setView} /></Show>
                <Show when={m.text}><span class="text">{m.text}</span></Show>
                <span class="meta">
                  {time(m.ts)}
                  <Show when={m.mine}>
                    <Switch>
                      <Match when={m.status === 0}><span title="Отправляется"><Clock size={14} /></span></Match>
                      <Match when={m.status === 1}><span title="Ушло в сеть"><Check size={14} /></span></Match>
                      <Match when={m.status === 2}><span title="Доставлено всем"><CheckCheck size={14} /></span></Match>
                    </Switch>
                  </Show>
                </span>
              </div>
            </div>
          )}
        </For>
      </div>
      <Show when={err() || p.callErr}><p class="error pad-x">{err() || p.callErr}</p></Show>
      <Show when={drag()}><div class="drop">Отпустите, чтобы отправить</div></Show>
      <Show when={view()}><div class="viewer" onClick={() => setView("")}><img src={view()} alt="" /></div></Show>
      <Show when={rec() && video()}><video ref={preview} class="circle live" autoplay muted playsinline /></Show>
      <div class="composer">
        <button class="icon" onClick={attach} title="Прикрепить файл (или перетащите в окно)"><Paperclip size={20} /></button>
        <label class="orig muted small" title="Фото уйдёт файлом как есть, с метаданными">
          <input type="checkbox" checked={original()} onChange={(e) => setOriginal(e.currentTarget.checked)} /> оригинал
        </label>
        <Show when={rec()}>
          <div class="recording grow" classList={{ cancel: cancelling() }}>
            <i class="dot" /> {Math.floor(elapsed() / 60000)}:{String(Math.floor(elapsed() / 1000) % 60).padStart(2, "0")}
            <span class="muted small grow">{cancelling() ? "Отпустите — отмена" : "← влево или Esc — отмена"}</span>
            <X size={18} />
          </div>
        </Show>
        <textarea ref={input} rows={1} placeholder="Сообщение" value={text()} style={{ display: rec() ? "none" : undefined }}
          onInput={(e) => { setText(e.currentTarget.value); grow(); }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
              e.preventDefault();
              send();
            }
          }} />
        <button class="icon mic" classList={{ live: !!rec() }} onPointerDown={down} onPointerUp={up} onPointerMove={move}
          title={video() ? "Удерживайте — видеосообщение; нажмите — голосовое" : "Удерживайте — голосовое; нажмите — видеосообщение"}>
          {video() ? <Video size={20} /> : <Mic size={20} />}
        </button>
        <button class="icon send" disabled={!text().trim()} onClick={send} title="Отправить"><Send size={20} /></button>
      </div>
    </section>
  );
}

function Calls(p: { msgs: Msg[] }) {
  const calls = () => p.msgs.filter((m) => m.call).reverse();
  return (
    <div class="scroll">
      <Show when={!calls().length}><p class="empty muted">Звонков пока не было</p></Show>
      <For each={calls()}>
        {(m) => (
          <div class="row">
            <Avatar id={m.author} />
            <div class="grow">
              <b>{m.mine ? "Исходящий" : `Участник ${m.author}`}</b>
              <div class="muted small" classList={{ error: m.call!.missed && !m.mine }}>{callLabel(m.call!)}</div>
            </div>
            <small class="muted">{new Date(m.ts).toLocaleString("ru", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })}</small>
          </div>
        )}
      </For>
    </div>
  );
}

function Incoming(p: { c: CallSignal; onAccept: () => void; onReject: () => void }) {
  onCleanup(ring("in"));
  return (
    <div class="call-screen incoming">
      <Avatar id={p.c.author} />
      <h2>Участник {p.c.author}</h2>
      <p class="muted">{p.c.video ? "Входящий видеозвонок" : "Входящий звонок"}</p>
      <div class="call-buttons">
        <button class="round hang" onClick={p.onReject} title="Отклонить"><PhoneOff size={26} /></button>
        <button class="round accept" onClick={p.onAccept} title="Принять">{p.c.video ? <Video size={26} /> : <Phone size={26} />}</button>
      </div>
    </div>
  );
}

function CallScreen(p: { a: Active; tiles: Peer[]; media: CallMedia; onEnd: () => void }) {
  const [now, setNow] = createSignal(Date.now());
  const [mic, setMic] = createSignal(true);
  const [cam, setCam] = createSignal(p.a.video);
  const t = setInterval(() => setNow(Date.now()), 500);
  onCleanup(() => clearInterval(t));
  // Ringback until someone answers.
  createEffect(() => {
    if (!p.a.started) onCleanup(ring("out"));
  });
  const toggleCam = async () => {
    try {
      await p.media.setCam(!cam());
      setCam(!cam());
    } catch {
      /* no camera: stays off */
    }
  };
  return (
    <div class="call-screen">
      <div class="muted">{p.a.started ? clock(now() - p.a.started) : "Вызов…"}</div>
      <div class="call-grid">
        <For each={p.tiles}>
          {(t) => (
            <div class="tile">
              {/* Video shows while frames come; otherwise the avatar. */}
              <div style={{ display: now() - t.videoAt < 1500 ? "contents" : "none" }}>{t.canvas}</div>
              <Show when={now() - t.videoAt >= 1500}><Avatar id={t.id} /></Show>
              <span class="tile-name">Участник {t.id}</span>
            </div>
          )}
        </For>
        <Show when={!p.tiles.length}>
          <div class="tile"><span class="muted">{p.a.started ? "Соединяемся…" : "Ждём ответа"}</span></div>
        </Show>
      </div>
      <Show when={cam()}>
        <video class="self" autoplay muted playsinline ref={(v) => (v.srcObject = p.media.local)} />
      </Show>
      <div class="call-buttons">
        <button class="round" classList={{ off: !mic() }} onClick={() => { p.media.setMic(!mic()); setMic(!mic()); }}
          title={mic() ? "Выключить микрофон" : "Включить микрофон"}>{mic() ? <Mic size={24} /> : <MicOff size={24} />}</button>
        <button class="round" classList={{ off: !cam() }} onClick={toggleCam}
          title={cam() ? "Выключить камеру" : "Включить камеру"}>{cam() ? <Video size={24} /> : <VideoOff size={24} />}</button>
        <button class="round hang" onClick={p.onEnd} title="Завершить"><PhoneOff size={24} /></button>
      </div>
    </div>
  );
}
