import { createEffect, createSignal, For, Match, onCleanup, onMount, Show, Switch, untrack } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import {
  ArrowLeft, Check, CheckCheck, ChevronDown, CircleCheck, Clock, Copy, Forward, Hash, Link, LogOut, MessageCircle, Mic, MicOff, Paperclip, Pencil, Phone, PhoneMissed, PhoneOff, Pin, PinOff, Plus, Reply, Send,
  Settings, Trash2, UserPlus, Users, Video, VideoOff, X,
} from "lucide-solid";
import logo from "../app-icon.svg";
import { label, Media, MediaView } from "./media";
import { record, Recording, waveform } from "./capture";
import { CallMedia, Peer, ring } from "./call";
import { Signal, VoiceSettings } from "./voice";

// status (own messages, §7.4): 0 ⏱ sending, 1 ✓ some member has it, 2 ✓✓ every member has it.
type Msg = { id: string; author: string; mine: boolean; text: string; ts: number; status: number; media: Media | null; call: CallView | null;
  reply: { id: string; author: string; text: string } | null; edited: boolean; pinned: boolean };
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
  // Latest call wins; unchanged messages keep their objects, so their bubbles (and media) don't re-render.
  let seq = 0;
  const reload = async () => {
    const s = ++seq;
    const fresh = await invoke<Msg[]>("history");
    if (s !== seq) return;
    const old = new Map(msgs().map((m) => [m.id, m]));
    setMsgs(fresh.map((m) => {
      const o = old.get(m.id);
      return o && JSON.stringify(o) === JSON.stringify(m) ? o : m;
    }));
  };

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
    loadMembers();
    pollPeers();
    // A join comes as an MLS commit, not a message: poll members with peers.
    const timer = setInterval(() => (pollPeers(), loadMembers()), 3000);
    // Listen before the first load, so nothing that arrives in between is lost.
    const un = await listen("message", () => {
      reload();
      if (!chatVisible()) setUnread((n) => n + 1);
      loadMembers();
    });
    const unChanged = await listen("changed", reload);
    const unCall = await listen<CallSignal>("call", (e) => onCall(e.payload));
    const unStatus = await listen<[string, number][]>("status", (e) => {
      const st = new Map(e.payload);
      setMsgs((all) => all.map((m) => (st.has(m.id) && st.get(m.id) !== m.status ? { ...m, status: st.get(m.id)! } : m)));
    });
    await reload();
    const onWide = () => setWide(wideQuery.matches);
    const onFocus = () => setFocused(true);
    const onBlur = () => setFocused(false);
    wideQuery.addEventListener("change", onWide);
    addEventListener("focus", onFocus);
    addEventListener("blur", onBlur);
    onCleanup(() => {
      clearInterval(timer);
      un();
      unChanged();
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
              <VoiceSettings />
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

      <Chat msgs={msgs()} members={members().length} me={members().find((m) => m.me)?.id ?? ""} admin={members().some((m) => m.me && m.admin)}
        peers={peers()} back={() => setChatOpen(false)} reload={reload} inCall={!!active()} onCall={startCall} callErr={callErr()} />
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

const dayKey = (ts: number) => new Date(ts).toDateString();
const dayLabel = (ts: number) => {
  const d = new Date(ts), now = new Date(), y = new Date();
  y.setDate(now.getDate() - 1);
  if (dayKey(ts) === now.toDateString()) return "Сегодня";
  if (dayKey(ts) === y.toDateString()) return "Вчера";
  return d.toLocaleDateString("ru", { day: "numeric", month: "long", year: d.getFullYear() === now.getFullYear() ? undefined : "numeric" });
};
// One bubble group: same sender, same day, within 10 minutes.
const same = (a?: Msg, b?: Msg) =>
  !!a && !!b && !a.call && !b.call && a.mine === b.mine && (a.mine || a.author === b.author)
  && dayKey(a.ts) === dayKey(b.ts) && Math.abs(b.ts - a.ts) < 600_000;
const plain = (m: Msg) => m.text || (m.media ? label(m.media) : "");
type Dialog = { kind: "delete" | "forward"; ids: string[] };

function Chat(p: {
  msgs: Msg[]; members: number; me: string; admin: boolean; peers: number; back: () => void; reload: () => Promise<void>;
  inCall: boolean; onCall: (video: boolean) => void; callErr: string;
}) {
  const [text, setText] = createSignal("");
  // Sent but not in the history yet (⏱); `real` is the event id once the node has it.
  const [pending, setPending] = createSignal<(Msg & { real?: string })[]>([]);
  const [reply, setReply] = createSignal<Msg>();
  const [editing, setEditing] = createSignal<Msg>();
  const [menu, setMenu] = createSignal<{ m: Msg; x: number; y: number }>();
  const [sel, setSel] = createSignal<string[]>([]);
  const [dlg, setDlg] = createSignal<Dialog>();
  const [atBottom, setAtBottom] = createSignal(true);
  const [fresh, setFresh] = createSignal(0);
  const [pinIx, setPinIx] = createSignal(0);
  const name = (id: string) => (id === p.me ? "Вы" : `Участник ${id}`);
  const shown = () => {
    const ids = new Set(p.msgs.map((m) => m.id));
    return [...p.msgs, ...pending().filter((x) => !x.real || !ids.has(x.real))];
  };
  createEffect(() => {
    const ids = new Set(p.msgs.map((m) => m.id));
    setPending((ps) => (ps.some((x) => x.real && ids.has(x.real)) ? ps.filter((x) => !x.real || !ids.has(x.real)) : ps));
  });
  let tmp = 0;
  const queue = async (m: Partial<Msg>, run: () => Promise<string>, undo?: () => void) => {
    const id = `tmp-${++tmp}`;
    setPending((ps) => [...ps, { id, author: p.me, mine: true, text: "", ts: Date.now(), status: 0, media: null, call: null, reply: null, edited: false, pinned: false, ...m }]);
    try {
      const real = await run();
      setPending((ps) => ps.map((x) => (x.id === id ? { ...x, real } : x)));
      setErr("");
      await p.reload();
    } catch (e) {
      setPending((ps) => ps.filter((x) => x.id !== id));
      setErr(errText(e));
      undo?.();
    }
  };
  const pinned = () => p.msgs.filter((m) => m.pinned);
  const pinShown = () => pinned()[pinIx() % Math.max(1, pinned().length)];
  const jump = (id: string) => {
    const el = list.querySelector(`[data-id="${id}"]`);
    if (!el) return;
    el.scrollIntoView({ block: "center", behavior: "smooth" });
    el.classList.add("flash");
    setTimeout(() => el.classList.remove("flash"), 1600);
  };
  const toggle = (id: string) => !id.startsWith("tmp-") && setSel((s) => (s.includes(id) ? s.filter((x) => x !== id) : [...s, id]));
  const picked = () => shown().filter((m) => sel().includes(m.id));
  const copy = (ms: Msg[]) => navigator.clipboard.writeText(ms.map(plain).join("\n"));
  const act = (run: Promise<unknown>) => run.then(p.reload, (e) => setErr(errText(e)));
  const startEdit = (m: Msg) => {
    setReply(undefined);
    setEditing(m);
    setText(m.text);
    queueMicrotask(() => (grow(), input.focus()));
  };
  const startReply = (m: Msg) => {
    if (editing()) setText("");
    setEditing(undefined);
    setReply(m);
    input.focus();
  };
  const cancelCompose = () => {
    if (editing()) setText("");
    setEditing(undefined);
    setReply(undefined);
    queueMicrotask(grow);
  };
  let pressTimer = 0;
  const press = (e: PointerEvent, m: Msg) => {
    if (e.pointerType !== "touch") return;
    const { clientX: x, clientY: y } = e;
    pressTimer = window.setTimeout(() => setMenu({ m, x, y }), 500);
  };
  const unpress = () => clearTimeout(pressTimer);
  const openMenu = (e: MouseEvent, m: Msg) => {
    e.preventDefault();
    if (!sel().length && !m.id.startsWith("tmp-")) setMenu({ m, x: e.clientX, y: e.clientY });
  };
  const clickMsg = (e: MouseEvent, m: Msg) => {
    if (sel().length || e.shiftKey) {
      e.preventDefault();
      toggle(m.id);
    }
  };
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
    for (const path of paths)
      await queue({ text: `📎 ${path.split(/[\\/]/).pop()}` }, () => invoke<string>("send_file", { path, original: original() }));
  };
  const attach = async () => {
    const paths = await open({ multiple: true });
    if (paths) sendFiles(paths);
  };
  onMount(async () => {
    // Media that loads later grows the list: keep the newest message in view.
    const bottom = () => atBottom() && (list.scrollTop = list.scrollHeight);
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
      if (view() || menu() || dlg()) return (setView(""), setMenu(undefined), setDlg(undefined));
      if (rec()) return stopRec(false);
      if (sel().length) return setSel([]);
      cancelCompose();
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
    const body = new Uint8Array(await blob.arrayBuffer());
    const headers = { kind, mime: r.mime, duration: String(Date.now() - r.started), wave: wave.join(",") };
    await queue({ text: kind === "voice" ? "🎤 Голосовое" : "📹 Видеосообщение" }, () => invoke<string>("send_rec", body, { headers }));
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
  // Stay at the bottom if already there (or on own send); otherwise count what came for the ↓ button.
  const near = () => list.scrollHeight - list.scrollTop - list.clientHeight < 80;
  const toBottom = (smooth = true) => list.scrollTo({ top: list.scrollHeight, behavior: smooth ? "smooth" : "instant" });
  let seen = -1;
  createEffect(() => {
    const all = shown();
    const added = seen < 0 ? 0 : Math.max(0, all.length - seen);
    const stick = seen < 0 || untrack(atBottom) || (added > 0 && all.at(-1)!.mine);
    const first = seen < 0;
    seen = all.length;
    queueMicrotask(() => (stick ? toBottom(!first) : setFresh((n) => n + added)));
  });
  const onScroll = () => {
    setAtBottom(near());
    if (near()) setFresh(0);
  };
  const grow = () => {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  };
  const send = () => {
    const t = text().trim();
    if (!t) return;
    const ed = editing(), r = reply();
    setText("");
    setEditing(undefined);
    setReply(undefined);
    queueMicrotask(grow);
    if (ed) return t !== ed.text && act(invoke("edit", { id: ed.id, text: t }));
    queue({ text: t, reply: r ? { id: r.id, author: r.author, text: plain(r) } : null },
      () => invoke<string>("send", { text: t, replyTo: r?.id ?? null }),
      () => (setText(t), setReply(r), queueMicrotask(grow)));
  };
  const canAll = (ids: string[]) => p.admin || ids.every((id) => p.msgs.find((m) => m.id === id)?.mine);
  return (
    <section class="chat">
      <Show when={sel().length} fallback={<header class="bar">
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
      </header>}>
        <header class="bar selbar">
          <button class="icon" onClick={() => setSel([])} title="Отмена (Esc)"><X size={20} /></button>
          <b class="grow">{sel().length} выбрано</b>
          <button onClick={() => (copy(picked()), setSel([]))}><Copy size={16} /> Копировать</button>
          <button onClick={() => setDlg({ kind: "forward", ids: picked().map((m) => m.id) })}><Forward size={16} /> Переслать</button>
          <button class="del" onClick={() => setDlg({ kind: "delete", ids: picked().map((m) => m.id) })}><Trash2 size={16} /> Удалить</button>
        </header>
      </Show>
      <Show when={pinShown()}>
        {(m) => (
          <div class="pinbar" onClick={() => (jump(m().id), setPinIx((i) => i + 1))} title="К сообщению">
            <Pin size={16} />
            <div class="grow">
              <b>Закреплённое{pinned().length > 1 ? ` · ${(pinIx() % pinned().length) + 1} из ${pinned().length}` : ""}</b>
              <div class="ellipsis small">{plain(m())}</div>
            </div>
            <button class="icon" title="Открепить" onClick={(e) => (e.stopPropagation(), act(invoke("pin", { id: m().id, pinned: false })))}><X size={16} /></button>
          </div>
        )}
      </Show>
      <div class="msgs-wrap">
        <div class="msgs" ref={list} onScroll={onScroll} classList={{ selecting: !!sel().length }}>
          <Show when={!shown().length}><p class="empty muted">Здесь пока тихо. Напишите первым!</p></Show>
          <For each={shown()}>
            {(m, i) => {
              const prev = () => shown()[i() - 1], next = () => shown()[i() + 1];
              return (
                <>
                  <Show when={!prev() || dayKey(prev()!.ts) !== dayKey(m.ts)}><div class="day"><span>{dayLabel(m.ts)}</span></div></Show>
                  {m.call ? (
                    <div class="call-line" classList={{ missed: m.call.missed }}>
                      {m.call.missed ? <PhoneMissed size={14} /> : m.call.video ? <Video size={14} /> : <Phone size={14} />}
                      {m.mine ? "" : `Участник ${m.author} · `}{callLabel(m.call)} · {time(m.ts)}
                    </div>
                  ) : (
                    <div class="msg" data-id={m.id}
                      classList={{ mine: m.mine, cont: same(prev(), m), tail: !same(m, next()), sel: sel().includes(m.id) }}
                      onClick={(e) => clickMsg(e, m)} onMouseDown={(e) => e.shiftKey && e.preventDefault()} onContextMenu={(e) => openMenu(e, m)}
                      onPointerDown={(e) => press(e, m)} onPointerUp={unpress} onPointerCancel={unpress} onPointerMove={unpress}>
                      <button class="pick" title="Выделить" onClick={(e) => (e.stopPropagation(), toggle(m.id))}>
                        <Show when={sel().includes(m.id)}><Check size={14} /></Show>
                      </button>
                      <Show when={!m.mine}>
                        <Show when={!same(m, next())} fallback={<div class="av-gap" />}><Avatar id={m.author} /></Show>
                      </Show>
                      <div class="bubble">
                        <Show when={!m.mine && !same(prev(), m)}>
                          <div class="author" style={{ color: `hsl(${hue(m.author)} 50% 50%)` }}>Участник {m.author}</div>
                        </Show>
                        <Show when={m.reply}>
                          {(r) => (
                            <div class="quote" onClick={(e) => (e.stopPropagation(), jump(r().id))}>
                              <Show when={r().author}><b>{name(r().author)}</b></Show>
                              <span>{r().text}</span>
                            </div>
                          )}
                        </Show>
                        <Show when={m.media}><MediaView id={m.id} m={m.media!} onView={setView} /></Show>
                        <Show when={m.text}><span class="text">{m.text}</span></Show>
                        <span class="meta">
                          <Show when={m.pinned}><Pin size={11} /></Show>
                          <Show when={m.edited}><span>изменено</span></Show>
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
                </>
              );
            }}
          </For>
        </div>
        <Show when={!atBottom()}>
          <button class="down" onClick={() => (toBottom(), setFresh(0))} title="Вниз">
            <ChevronDown size={22} />
            <Show when={fresh()}><b class="badge">{fresh()}</b></Show>
          </button>
        </Show>
      </div>
      <Show when={menu()}>
        {(mn) => (
          <div class="overlay" onClick={() => setMenu(undefined)} onContextMenu={(e) => (e.preventDefault(), setMenu(undefined))}>
            <div class="menu" style={{ left: `${Math.min(mn().x, innerWidth - 210)}px`, top: `${Math.min(mn().y, innerHeight - 310)}px` }}>
              <button onClick={() => startReply(mn().m)}><Reply size={16} /> Ответить</button>
              <Show when={plain(mn().m)}><button onClick={() => copy([mn().m])}><Copy size={16} /> Копировать</button></Show>
              <Show when={mn().m.mine}><button onClick={() => startEdit(mn().m)}><Pencil size={16} /> Редактировать</button></Show>
              <button onClick={() => act(invoke("pin", { id: mn().m.id, pinned: !mn().m.pinned }))}>
                {mn().m.pinned ? <PinOff size={16} /> : <Pin size={16} />} {mn().m.pinned ? "Открепить" : "Закрепить"}
              </button>
              <button onClick={() => setDlg({ kind: "forward", ids: [mn().m.id] })}><Forward size={16} /> Переслать</button>
              <button onClick={() => setSel([mn().m.id])}><CircleCheck size={16} /> Выделить</button>
              <button class="del" onClick={() => setDlg({ kind: "delete", ids: [mn().m.id] })}><Trash2 size={16} /> Удалить</button>
            </div>
          </div>
        )}
      </Show>
      <Show when={dlg()}>
        {(d) => (
          <div class="modal" onClick={() => setDlg(undefined)}>
            <div class="card dialog" onClick={(e) => e.stopPropagation()}>
              <Show when={d().kind === "delete"} fallback={
                <>
                  <h3>Переслать в чат</h3>
                  <button class="row chat-row" onClick={() => {
                    const ids = d().ids;
                    setDlg(undefined);
                    setSel([]);
                    act(invoke("forward", { ids }));
                  }}>
                    <div class="avatar group small-av"><Hash size={18} /></div><b>Общий чат</b>
                  </button>
                </>
              }>
                <h3>{d().ids.length > 1 ? `Удалить ${d().ids.length} сообщ.?` : "Удалить сообщение?"}</h3>
                <button onClick={() => { const ids = d().ids; setDlg(undefined); setSel([]); act(invoke("delete", { ids, all: false })); }}>
                  Удалить у меня
                </button>
                <Show when={canAll(d().ids)}>
                  <button class="del" onClick={() => { const ids = d().ids; setDlg(undefined); setSel([]); act(invoke("delete", { ids, all: true })); }}>
                    Удалить у всех
                  </button>
                </Show>
              </Show>
              <button onClick={() => setDlg(undefined)}>Отмена</button>
            </div>
          </div>
        )}
      </Show>
      <Show when={err() || p.callErr}><p class="error pad-x">{err() || p.callErr}</p></Show>
      <Show when={drag()}><div class="drop">Отпустите, чтобы отправить</div></Show>
      <Show when={view()}><div class="viewer" onClick={() => setView("")}><img src={view()} alt="" /></div></Show>
      <Show when={rec() && video()}><video ref={preview} class="circle live" autoplay muted playsinline /></Show>
      <Show when={editing() || reply()}>
        <div class="replybar">
          {editing() ? <Pencil size={18} /> : <Reply size={18} />}
          <div class="grow" onClick={() => jump((editing() ?? reply())!.id)}>
            <b>{editing() ? "Редактирование" : `Ответ: ${name(reply()!.author)}`}</b>
            <div class="ellipsis small">{plain((editing() ?? reply())!)}</div>
          </div>
          <button class="icon" onClick={cancelCompose} title="Отмена (Esc)"><X size={18} /></button>
        </div>
      </Show>
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
        <button class="icon send" disabled={!text().trim()} onClick={send} title={editing() ? "Сохранить" : "Отправить"}>
          {editing() ? <Check size={20} /> : <Send size={20} />}
        </button>
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
  const [gear, setGear] = createSignal(false);
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
              <span class="tile-name">Участник {t.id} <Signal net={t.net()} /></span>
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
        <button class="round" classList={{ off: gear() }} onClick={() => setGear(!gear())} title="Голос и видео"><Settings size={24} /></button>
        <button class="round hang" onClick={p.onEnd} title="Завершить"><PhoneOff size={24} /></button>
      </div>
      <Show when={gear()}>
        <div class="call-settings scroll pad"><VoiceSettings /></div>
      </Show>
    </div>
  );
}
