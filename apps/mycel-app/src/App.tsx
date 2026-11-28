import { createEffect, createMemo, createRenderEffect, createSignal, For, getOwner, Index, Match, on, onCleanup, onMount, runWithOwner, Show, Switch, untrack } from "solid-js";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ask, message, open, save } from "@tauri-apps/plugin-dialog";
import { getCurrent, onOpenUrl } from "@tauri-apps/plugin-deep-link";
import { isPermissionGranted, requestPermission } from "@tauri-apps/plugin-notification";
import {
  ArrowLeft, Bell, BellOff, EllipsisVertical, Camera, Check, CheckCheck, ChevronDown, ChevronUp, CircleCheck, Clock, Contact, Copy, Forward, Hash, Link, Lock as LockIcon, LogOut, MessageCircle, Mic, MicOff, Paperclip, Pencil, Phone, PhoneMissed, PhoneOff, Pin, PinOff, Plus, Reply, Search, Send,
  Settings, ShieldAlert, Smile, ShieldCheck, Trash2, UserMinus, UserPlus, ScreenShare, ScreenShareOff, Users, Video, VideoOff, X, ChevronRight, Database, Layers, Monitor, Palette, Wifi, Archive, ArchiveRestore, Smartphone, Timer, SwitchCamera,
  EyeOff,
} from "lucide-solid";
import logo from "../app-icon.svg";
import { label, Media, MediaView } from "./media";
import { Album, albums } from "./album";
import { Shot, useBack, Viewer } from "./viewer";
import { EMOJI, emojiOnly, recentEmoji, useEmoji } from "./emoji";
import { Emoji, rich } from "./rich";
import { Picker, Sticker } from "./stickers";
import { record, Recording, waveform } from "./capture";
import { CallMedia, camError, Peer, ring } from "./call";
import { Signal, VoiceSettings } from "./voice";
import { groupTitle, mode, ModeChoice, ModeSettings, setMode, simple, SIMPLE_NAME } from "./simple";
import { Appearance, msgSize } from "./look";
import { Accounts, DirSearch, JoinGroup, joinLink, keepRegistered, NewGroup, QuickStart, SpaceRows, Username, UsernameBanner, UsernameSettings, withBox, withServer } from "./directory";
import { lastSeen, PrivacySettings } from "./privacy";
import { mentionIds, mentionText, useMention } from "./mention";
import { ForwardList } from "./forward";

// status (own messages, §7.4): 0 ⏱ sending, 1 ✓ some member has it, 2 ✓✓ every member has it.
type Msg = { id: string; author: string; mine: boolean; text: string; sticker?: string | null; ts: number; status: number; media: Media | null; call: CallView | null;
  reply: { id: string; author: string; text: string } | null; edited: boolean; pinned: boolean;
  reactions: [string, string[]][];
  // ttl: seconds after ts it disappears (0 = never); fwd: «Переслано от …»; service: a line like «X включил(а) автоудаление».
  ttl: number; fwd: string | null; service: boolean };
type CallView = { id: string; video: boolean; author: string; chat: string; mine: boolean; ts: number; duration_ms: number | null; missed: boolean; active: boolean };
// chat: "" = the space chat (group call), else the 1:1 partner's id.
type CallSignal = { id: string; action: "offer" | "accept" | "reject" | "end"; video: boolean; author: string; device: string; chat: string; ts: number };
type Active = { id: string; video: boolean; started: number; peers: string[]; chat: string };
type Member = { id: string; me: boolean; admin: boolean; name: string | null; avatar: string; color: number | null; link: "direct" | "relay" | null; online: boolean; seen: number | null; hidden: boolean; removed: boolean };
type ChatRow = { id: string; last: Msg | null; unread: number; muted: boolean; mention: boolean; pin: number | null; archived: boolean };
type Space = { id: string; name: string; remembered: boolean };
type Status = { spaces: Space[]; last: string | null; current: string | null; dir: string; in_space: boolean };
type Tab = "chats" | "people" | "groups" | "calls" | "settings";
// What the user came to do, picked before the password (unknown: Onboard offers both).
type How = "create" | "join";

const time = (ts: number) => new Date(ts).toLocaleTimeString("ru", { hour: "2-digit", minute: "2-digit" });
/** Chat list time, as in Telegram: today the time, this week the weekday, older the date. */
const when = (ts: number) => {
  const d = new Date(ts), now = new Date();
  if (d.toDateString() === now.toDateString()) return time(ts);
  if (now.getTime() - ts < 6 * 86_400_000) return d.toLocaleDateString("ru", { weekday: "short" }).replace(/^./, (c) => c.toUpperCase());
  return d.toLocaleDateString("ru", { day: "2-digit", month: "2-digit", year: d.getFullYear() === now.getFullYear() ? undefined : "2-digit" });
};
const hue = (id: string) => parseInt(id.slice(0, 4), 16) % 360;
// Avatar colors a member can pick (profile `color` is an index here).
const PALETTE = [210, 0, 25, 45, 130, 175, 265, 320];
// Members with their profiles, for names and avatars everywhere.
const [people, setPeople] = createSignal<Member[]>([]);
const memberOf = (id: string) => people().find((m) => m.id === id);
const nameOf = (id: string) => memberOf(id)?.name ?? (simple() ? "Участник" : `Участник ${id}`);
// The open space: the personal one has no common chat, in any other it is the group's chat.
const [openName, setOpenName] = createSignal("");
const personal = () => openName() === SIMPLE_NAME;
const groupName = () => groupTitle(openName());
// Last message of each group left during this run, for its row in the chat list.
// shortcut: only the open space's database is readable and message text stays out of plain storage,
// so after a restart a group row has no preview until visited; needs the backend to read closed spaces.
const lastMsg = new Map<string, { text: string; ts: number }>();
const colorOf = (id: string) => {
  const c = memberOf(id)?.color;
  return c == null ? hue(id) : PALETTE[c % PALETTE.length];
};
const initials = (id: string) => {
  const m = memberOf(id);
  return m?.avatar || (m?.name ? m.name.trim().split(/\s+/).slice(0, 2).map((w) => [...w][0]).join("").toUpperCase() : id.slice(0, 2).toUpperCase());
};
/** Any thrown value as readable text: string, Error, or a plugin's `{ message }` object. */
const errText = (e: unknown): string =>
  typeof e === "string" ? e : e instanceof Error ? e.message : typeof (e as any)?.message === "string" ? (e as any).message : JSON.stringify(e);
const android = /Android/.test(navigator.userAgent);
// n0, relays and the anchor are per space: the key carries the space id (a new space, not yet
// named, uses the bare key).
const sk = (k: string, space?: string | null) => (space ? `${k}:${space}` : k);
// Per-space value, else the old shared one (settings made before 0.1.15 keep working).
const ls = (k: string, space?: string | null) => localStorage.getItem(sk(k, space)) ?? localStorage.getItem(k);
const n0 = (space?: string | null) => ls("n0", space) !== "0";
// This PC is an anchor: its relay URL (Settings → Связь → Якорь). Spaces mode only.
const anchor = (space?: string | null) => (simple() ? null : ls("anchor", space) || null);
// Up to 0.1.17 the anchor switched itself on (UPnP) and held port 3340. Turned off once, unless the
// user had unticked that automation ("0"): then it is their own, and so is spaces mode.
{
  const keys = Object.keys(localStorage).filter((k) => k === "anchor" || k.startsWith("anchor:"));
  if (localStorage.getItem("anchor_auto") !== "0") {
    keys.forEach((k) => localStorage.removeItem(k));
    localStorage.setItem("anchor_auto", "0");
  } else if (!mode() && keys.length) setMode("spaces");
  // 0.2.3: the main server is its own program on port 3340 now; an anchor kept from back then took
  // that port before it (and every time it restarts). Off once more; Settings → Связь turns it on.
  if (!localStorage.getItem("anchor_off_023")) {
    keys.forEach((k) => localStorage.removeItem(k));
    localStorage.setItem("anchor_off_023", "1");
  }
}
// Backup public relays (n0's test servers): used besides the main n0 ones; editable in Settings → Связь.
const DEFAULT_RELAYS = ["https://euc1-1.staging-relay.n0.iroh.link", "https://use1-1.staging-relay.n0.iroh.link"];
const relays = (space?: string | null): string[] => {
  try {
    const v = JSON.parse(ls("relays", space) ?? "null");
    return Array.isArray(v) ? v : DEFAULT_RELAYS;
  } catch {
    return DEFAULT_RELAYS;
  }
};
// Mailbox (Cloudflare Worker, mailbox/README.md) that keeps messages for members who are offline.
// shortcut: one shared default baked into the build; per space if spaces need different ones.
const DEFAULT_MAILBOX = "https://mycel-box.psoniik2000.workers.dev";
const mailbox = () => (localStorage.getItem("mailbox") ?? DEFAULT_MAILBOX) || null;
/** Applies changed connection settings to the open space at once (it reopens). */
const reopen = (id: string) =>
  invoke("unlock", { id, name: "", password: null, remember: true, n0: n0(id), anchor: anchor(id), relays: withServer(relays(id)), mailbox: withBox(mailbox()) });
/** Turns this PC's anchor on (relay at the external IP, port 3340) or off; reopens the space. Returns the URL. */
async function setAnchor(id: string, on: boolean): Promise<string> {
  // shortcut: fixed port 3340; a port field when someone needs another one.
  if (on) await invoke("anchor_port_free", { port: 3340 });
  const u = on ? `http://${await invoke<string>("public_ip")}:3340` : "";
  await invoke("unlock", { id, name: "", password: null, remember: true, n0: n0(id), anchor: u || null, relays: withServer(relays(id)), mailbox: withBox(mailbox()) });
  u ? localStorage.setItem(sk("anchor", id), u) : localStorage.removeItem(sk("anchor", id));
  return u;
}
// The open space.
let spaceId = "";
// Unsent text per space and chat, kept on this device (storage may be unavailable).
// Who is typing, per chat: an entry lives 5 s after the last signal, or until their message.
type TypingSignal = { chat: string; author: string; voice: boolean; done: boolean };
type Typer = { author: string; voice: boolean; at: number };
const [typers, setTypers] = createSignal<Record<string, Typer[]>>({});
const dropTyper = (chat: string, keep: (t: Typer) => boolean) =>
  setTypers((all) => ({ ...all, [chat]: (all[chat] ?? []).filter(keep) }));
const onTyping = (s: TypingSignal) => {
  const at = Date.now();
  dropTyper(s.chat, (t) => t.author !== s.author);
  if (s.done) return;
  setTypers((all) => ({ ...all, [s.chat]: [...(all[s.chat] ?? []), { author: s.author, voice: s.voice, at }] }));
  setTimeout(() => dropTyper(s.chat, (t) => t.at !== at), 5000);
};
const typingText = (chat: string) => {
  const t = typers()[chat] ?? [];
  if (!t.length) return "";
  const verb = (v: boolean, many = false) => (v ? (many ? "записывают голосовое…" : "записывает голосовое…") : many ? "печатают…" : "печатает…");
  if (chat) return verb(t[0].voice);
  return `${t.map((x) => nameOf(x.author)).join(", ")} ${verb(t.every((x) => x.voice), t.length > 1)}`;
};

const [draftsChanged, touchDrafts] = createSignal<void>(undefined, { equals: false });
const draftKey = (chat: string) => `draft:${spaceId}:${chat}`;
const getDraft = (key: string) => {
  draftsChanged();
  try {
    return localStorage.getItem(key) ?? "";
  } catch {
    return "";
  }
};
const setDraft = (key: string, text: string) => {
  try {
    text.trim() ? localStorage.setItem(key, text) : localStorage.removeItem(key);
  } catch {
    // not kept
  }
  touchDrafts();
};
/** Drafts are plain text outside the encrypted database: they go with their space. */
const dropDrafts = (space: string) => {
  try {
    Object.keys(localStorage).filter((k) => k.startsWith(`draft:${space}:`)).forEach((k) => localStorage.removeItem(k));
  } catch {
    // nothing stored
  }
};
const clock = (ms: number) => `${Math.floor(ms / 60000)}:${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}`;
const callLabel = (c: CallView) =>
  c.missed ? (c.mine ? "Без ответа" : "Пропущенный звонок")
  : c.active ? "Идёт звонок"
  : `${c.video ? "Видеозвонок" : "Звонок"}, ${clock(c.duration_ms ?? 0)}`;

/** The connection to a member in words (tooltip, ⋮ menu); one who hides their time shows only that. */
const linkText = (m?: Member) => {
  if (!m || m.me) return "личный чат";
  if (m.hidden) return presence(m);
  if (m.link) return m.link === "direct" ? "на связи напрямую" : "на связи через сервер-ретранслятор";
  // Online without a link here: through another device of theirs.
  return m.online ? "в сети" : "не в сети, сообщения дойдут через почтовый ящик";
};

/** Header status in Telegram's words; the link details are in linkText (tooltip, ⋮ menu). */
const presence = (m?: Member) => (!m || m.me ? "личный чат" : m.online ? "в сети" : lastSeen(m.seen, m.hidden));

function Avatar(p: { id: string; onClick?: () => void; dot?: boolean }) {
  const m = () => memberOf(p.id);
  return (
    <div class="avatar" classList={{ link: !!p.onClick }} style={{ background: `hsl(${colorOf(p.id)} 45% 55%)` }}
      onClick={(e) => p.onClick && (e.stopPropagation(), p.onClick())}>
      {initials(p.id)}
      <Show when={p.dot && !m()?.me && m()?.online}><i class={`link-dot ${m()?.link ?? "direct"}`} title={linkText(m())} /></Show>
    </div>
  );
}

// Name and avatar letter/color: asked on first entry, changed in Settings.
/** Settings → Devices (§4.4): this account's devices; the first one links and unlinks the others. */
function Devices() {
  const [list, setList] = createSignal<{ id: string; me: boolean; root: boolean }[]>([]);
  const [inv, setInv] = createSignal<{ link: string; svg: string }>();
  const [err, setErr] = createSignal("");
  const load = () => invoke<{ id: string; me: boolean; root: boolean }[]>("devices").then(setList, (e) => setErr(errText(e)));
  load();
  const amRoot = () => list().some((d) => d.me && d.root);
  const link = async () => {
    setErr("");
    try {
      setInv(await invoke<{ link: string; svg: string }>("device_invite"));
    } catch (e) {
      setErr(errText(e));
    }
  };
  const unlink = async (id: string) => {
    if (!(await ask("Отвязать устройство? Оно перестанет получать ваши чаты", { title: "Устройства", okLabel: "Отвязать", cancelLabel: "Отмена" }))) return;
    await invoke("unlink_device", { id }).catch((e) => setErr(errText(e)));
    load();
  };
  return (
    <>
      <h3>Устройства</h3>
      <For each={list()}>
        {(d) => (
          <div class="row">
            <span>{d.me ? "Это устройство" : `Устройство ${d.id.slice(0, 8)}`}{d.root ? " · основное" : ""}</span>
            <Show when={amRoot() && !d.me}>
              <button onClick={() => unlink(d.id)}><Trash2 size={16} /> Отвязать</button>
            </Show>
          </div>
        )}
      </For>
      <Show when={amRoot()}>
        <button onClick={link}><Plus size={18} /> Добавить устройство</button>
      </Show>
      <Show when={inv()}>
        <div class="card invite">
          <div class="qr" innerHTML={inv()!.svg} />
          <p class="muted small">На новом устройстве установите Mycel, нажмите «Войти по QR или ссылке» и отсканируйте этот QR или вставьте ссылку. Ссылка одноразовая</p>
          <code class="path">{inv()!.link}</code>
          <button onClick={() => navigator.clipboard.writeText(inv()!.link)}><Copy size={18} /> Копировать</button>
        </div>
      </Show>
      <Show when={err()}><p class="error">{err()}</p></Show>
    </>
  );
}

function ProfileForm(p: { submit: string; onDone?: () => void }) {
  const me = () => people().find((m) => m.me);
  const [name, setName] = createSignal(me()?.name ?? "");
  const [avatar, setAvatar] = createSignal(me()?.avatar ?? "");
  const [color, setColor] = createSignal(me()?.color ?? Math.floor(Math.random() * PALETTE.length));
  const [err, setErr] = createSignal("");
  const [saved, setSaved] = createSignal(false);
  const save = async (e: Event) => {
    e.preventDefault();
    try {
      await invoke("set_profile", { name: name().trim(), avatar: avatar().trim(), color: color() });
      setPeople(await invoke<Member[]>("members"));
      setErr("");
      setSaved(true);
      setTimeout(() => setSaved(false), 1500);
      p.onDone?.();
    } catch (e) {
      setErr(errText(e));
    }
  };
  const letter = () => avatar().trim() || name().trim().split(/\s+/).slice(0, 2).map((w) => [...w][0] ?? "").join("").toUpperCase() || "?";
  return (
    <form class="profile" onSubmit={save}>
      <div class="profile-top">
        <div class="avatar big" style={{ background: `hsl(${PALETTE[color()]} 45% 55%)` }}>{letter()}</div>
        <input placeholder="Ваше имя" maxLength={64} value={name()} onInput={(e) => setName(e.currentTarget.value)} autofocus />
      </div>
      <div class="swatches">
        <For each={PALETTE}>
          {(h, i) => <button type="button" class="swatch" classList={{ active: color() === i() }} style={{ background: `hsl(${h} 45% 55%)` }}
            onClick={() => setColor(i())} title="Цвет аватара" />}
        </For>
        <input class="letter" placeholder="Буква" maxLength={2} value={avatar()} onInput={(e) => setAvatar(e.currentTarget.value)}
          title="Буква или эмодзи на аватаре (пусто — инициалы)" />
      </div>
      <Show when={err()}><p class="error">{err()}</p></Show>
      <button class="primary" disabled={!name().trim()}>{saved() ? "Сохранено" : p.submit}</button>
    </form>
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
  // lock: the password of `space` (none = a new space); spaces: the list (empty on first start: just create / join).
  const [stage, setStage] = createSignal<"spaces" | "lock" | "onboard" | "main">();
  const [space, setSpace] = createSignal<Space>();
  const [how, setHow] = createSignal<How>();
  const [invite, setInvite] = createSignal("");
  const [restoring, setRestoring] = createSignal(false);
  const [theme, setTheme] = createSignal(localStorage.getItem("theme") ?? "system");
  createEffect(() => {
    document.documentElement.dataset.theme = theme();
    localStorage.setItem("theme", theme());
  });
  const refresh = async () => {
    const s = await invoke<Status>("status");
    setStatus(s);
    spaceId = s.current ?? "";
    return s;
  };
  const opened = async (inSpace: boolean) => {
    await refresh();
    setStage(inSpace ? "main" : "onboard");
    if (inSpace) keepRegistered(spaceId);
  };
  // A remembered space opens at once; otherwise (or if the saved password no longer fits) the password screen.
  const enter = async (sp?: Space, h?: How) => {
    setSpace(sp);
    setHow(h);
    if (sp?.remembered) {
      try {
        return await opened(await invoke<boolean>("unlock", { id: sp.id, name: "", password: null, remember: true, n0: n0(sp.id), anchor: anchor(sp.id), relays: withServer(relays(sp.id)), mailbox: withBox(mailbox()) }));
      } catch {
        // falls through to the password screen
      }
    }
    setStage("lock");
  };
  const leave = async () => {
    await invoke("close_space");
    setPeople([]);
    await refresh();
    setStage("spaces");
  };
  const start = async () => {
    const s = await refresh();
    if (s.current) return setStage(s.in_space ? "main" : "onboard");
    const last = s.spaces.find((x) => x.id === s.last);
    if (last) return enter(last);
    setStage("spaces");
  };
  // A mycel://join/… link only fills in the join field: joining stays a button press.
  const invited = async (urls: string[] | null) => {
    const url = urls?.find((u) => u.startsWith("mycel://join/") || u.startsWith("mycel://contact/"));
    // Android may hand the launch link over again later: a used one is skipped.
    if (!url || url === localStorage.getItem("joined")) return;
    // A contact link (§4.2) adds a chat to the open space, nothing closes.
    if (stage() === "main" && url.startsWith("mycel://contact/")) {
      if (!(await ask("Добавить этот контакт?", { title: "Контакт", okLabel: "Добавить", cancelLabel: "Отмена" }))) return;
      localStorage.setItem("joined", url);
      return invoke("join", { link: url }).catch((e) => message(errText(e), { title: "Контакт", kind: "error" }));
    }
    if (stage() === "main") {
      const q = simple()
        ? "Принять приглашение? Откроется новая группа, вернуться к нынешним чатам можно из списка чатов"
        : "Вступить в другое пространство по этой ссылке? Нынешнее закроется, вернуться в него можно из списка пространств";
      if (!(await ask(q, { title: "Приглашение", okLabel: "Принять", cancelLabel: "Отмена" }))) return;
      await leave();
    }
    setInvite(url);
    if (stage() === "onboard") setHow("join");
    else enter(undefined, "join");
  };
  onMount(async () => {
    checkUpdate().then(setRelease, () => {});
    await start();
    await onOpenUrl(invited);
    await invited(await getCurrent());
  });
  return (
    <Show when={status()}>
      {(s) => (
        <Switch>
          <Match when={stage() === "spaces" && !s().spaces.length && !mode()}>
            <ModeChoice onSimple={() => enter()} onSpaces={() => setStage("spaces")} onRestore={() => setRestoring(true)} />
            <Show when={restoring()}><Restore onClose={() => setRestoring(false)} /></Show>
          </Match>
          <Match when={stage() === "spaces"}>
            <Spaces spaces={s().spaces} onOpen={enter} onNew={(h) => enter(undefined, h)} onRemoved={refresh} />
          </Match>
          <Match when={stage() === "lock"}>
            <Lock space={space()} how={how()} link={invite()} count={s().spaces.length} onDone={opened}
              onBack={() => (s().spaces.length || setMode(null), setStage("spaces"))}
              onRemoved={async () => (await refresh(), setStage("spaces"))} />
          </Match>
          <Match when={stage() === "onboard"}>
            <Onboard how={how()} setHow={setHow} link={invite()} setLink={setInvite} onDone={() => setStage("main")} />
          </Match>
          <Match when={stage() === "main" && s().spaces.find((x) => x.id === s().current)}>
            {(sp) => /* Another space is another node: Main starts over (chats, messages, listeners). */ <Show when={sp().id} keyed>{(_) => <Main dir={s().dir} theme={theme()} setTheme={setTheme} space={sp()} onLeave={leave} refresh={refresh}
              onEnter={async (id) => { const sp = (await refresh()).spaces.find((x) => x.id === id); if (sp) enter(sp); }}
              onAdd={async () => { await invoke("close_space"); setPeople([]); await refresh(); enter(); }} />}</Show>}
          </Match>
        </Switch>
      )}
    </Show>
  );
}

function Lock(p: { space?: Space; how?: How; link: string; count: number; onDone: (inSpace: boolean) => void; onBack: () => void; onRemoved: () => void }) {
  const [forgot, setForgot] = createSignal(false);
  // A new space joined by anything but a contact link is a group, not the personal one.
  const group = p.how === "join" && !p.link.startsWith("mycel://contact/");
  const defaultName = simple() && !group ? SIMPLE_NAME : p.count ? `Пространство ${p.count + 1}` : "Моё пространство";
  const [name, setName] = createSignal(defaultName);
  const [pw, setPw] = createSignal("");
  const [pw2, setPw2] = createSignal("");
  const [remember, setRemember] = createSignal(true);
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const exists = () => !!p.space;
  const joining = () => !exists() && p.how === "join";
  const submit = async (e: Event) => {
    e.preventDefault();
    if (!exists() && !joining() && !name().trim()) return setErr("Введите название");
    if (!exists() && pw() !== pw2()) return setErr("Пароли не совпадают");
    if (!pw()) return setErr("Введите пароль");
    setBusy(true);
    setErr("");
    try {
      p.onDone(await invoke<boolean>("unlock", { id: p.space?.id ?? null, name: name().trim() || defaultName, password: pw(), remember: remember(), n0: n0(p.space?.id), anchor: anchor(p.space?.id), relays: withServer(relays(p.space?.id)), mailbox: withBox(mailbox()) }));
    } catch (e) {
      setErr(exists() ? "Неверный пароль" : errText(e));
    }
    setBusy(false);
  };
  return (
    <div class="center">
      <form class="card intro" onSubmit={submit}>
        <img src={logo} class="logo" alt="" />
        <h1>{simple() ? "Mycel" : p.space?.name ?? (joining() ? "Вступить по ссылке" : "Новое пространство")}</h1>
        <p class="muted">
          {exists() ? "Введите пароль, чтобы открыть переписку"
            : `Придумайте пароль: им шифруются все данные на этом устройстве${joining() ? ". Ссылку-приглашение вставите на следующем шаге" : ""}`}
        </p>
        <Show when={!exists() && !joining() && !simple()}>
          <input placeholder="Название (видно только вам)" maxLength={64} value={name()} onInput={(e) => setName(e.currentTarget.value)} />
        </Show>
        <input type="password" placeholder="Пароль" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} autofocus />
        <Show when={!exists()}>
          <input type="password" placeholder="Повторите пароль" value={pw2()} onInput={(e) => setPw2(e.currentTarget.value)} />
        </Show>
        <label class="check">
          <input type="checkbox" checked={remember()} onChange={(e) => setRemember(e.currentTarget.checked)} />
          Запомнить на этом устройстве
        </label>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <button class="primary" disabled={busy()}>{busy() ? "Открываем…" : exists() ? "Открыть" : "Продолжить"}</button>
        <Show when={!(simple() && exists())}>
          <button type="button" onClick={p.onBack}><ArrowLeft size={18} /> {p.count && !simple() ? "Все пространства" : "Назад"}</button>
        </Show>
        <Show when={exists()}>
          <button type="button" class="link" onClick={() => setForgot(true)}>Забыли пароль?</button>
        </Show>
      </form>
      <Show when={forgot() && p.space}>
        {(s) => <RemoveSpace space={s()} forgot onClose={() => setForgot(false)} onDone={p.onRemoved} />}
      </Show>
    </div>
  );
}

function Spaces(p: { spaces: Space[]; onOpen: (s: Space) => void; onNew: (how: How) => void; onRemoved: () => void }) {
  const [del, setDel] = createSignal<Space>();
  const [restore, setRestore] = createSignal(false);
  return (
    <div class="center">
      <div class="card intro">
        <img src={logo} class="logo" alt="" />
        <h1>{p.spaces.length ? (simple() ? "Чаты и группы" : "Пространства") : "Mycel"}</h1>
        <Show when={!p.spaces.length}>
          <p class="muted">{simple() ? "Начните переписку или вступите в группу по ссылке-приглашению" : "Создайте своё пространство или вступите в существующее по ссылке-приглашению"}</p>
        </Show>
        <For each={p.spaces}>
          {(s) => (
            <div class="actions">
              <button class="grow" onClick={() => p.onOpen(s)}><Hash size={18} /> <span class="ellipsis">{groupTitle(s.name)}</span></button>
              <button class="icon" title="Удалить с этого устройства" onClick={() => setDel(s)}><Trash2 size={18} /></button>
            </div>
          )}
        </For>
        <Show when={p.spaces.length}><div class="or"><span>или</span></div></Show>
        <Show when={!simple() || !p.spaces.some((s) => s.name === SIMPLE_NAME)}>
          <button class="primary" onClick={() => p.onNew("create")}><Plus size={18} /> {simple() ? "Начать" : p.spaces.length ? "Создать новое" : "Создать пространство"}</button>
        </Show>
        <button onClick={() => p.onNew("join")}><Link size={18} /> Вступить по ссылке</button>
        <button class="link" onClick={() => setRestore(true)}>Восстановить из копии</button>
      </div>
      <Show when={restore()}><Restore onClose={() => setRestore(false)} /></Show>
      <Show when={del()}>
        {(s) => <RemoveSpace space={s()} onClose={() => setDel()} onDone={() => (setDel(), p.onRemoved())} />}
      </Show>
    </div>
  );
}

function RemoveSpace(p: { space: Space; forgot?: boolean; onClose: () => void; onDone: () => void }) {
  const [err, setErr] = createSignal("");
  return (
    <div class="modal" onClick={(e) => e.target === e.currentTarget && p.onClose()}>
      <div class="card dialog">
        <h3>Удалить «{groupTitle(p.space.name)}» с этого устройства?</h3>
        <Show when={p.forgot}>
          <p class="muted small">
            Пароль восстановить нельзя: им зашифрована вся переписка на этом устройстве. Можно удалить {simple() ? "эти чаты" : "это пространство"} отсюда и
            начать заново или вступить по приглашению.
          </p>
        </Show>
        <p class="muted small">
          Вся переписка и файлы {simple() ? "этих чатов" : "этого пространства"} на этом устройстве будут стёрты. У других участников всё останется. Вернуться можно
          только по новому приглашению.
        </p>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <div class="actions">
          <button class="danger" onClick={() => invoke("remove_space", { id: p.space.id }).then(() => (dropDrafts(p.space.id), p.onDone()), (e) => setErr(errText(e)))}>
            <Trash2 size={18} /> Удалить
          </button>
          <button onClick={p.onClose}>Отмена</button>
        </div>
      </div>
    </div>
  );
}

// Settings → Вход: password at start, password change.
function AnchorSettings(p: { space: Space }) {
  const [url, setUrl] = createSignal(anchor(p.space.id) ?? "");
  const [state, setState] = createSignal<[boolean, boolean, boolean]>();
  const [msg, setMsg] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const check = async () => {
    setState(undefined);
    setMsg("");
    const st = await invoke<[boolean, boolean, boolean]>("anchor_check");
    setState(st);
    // The relay is not listening: say if someone else holds its port.
    if (!st[2]) await invoke("anchor_port_free", { port: 3340 }).catch((e) => setMsg(errText(e)));
  };
  const apply = async (on: boolean) => {
    setBusy(true);
    setMsg(on ? "Запускаю…" : "");
    try {
      setUrl(await setAnchor(p.space.id, on));
      setMsg("");
      if (on) await check();
    } catch (e) {
      setMsg(errText(e));
    }
    setBusy(false);
  };
  if (url()) check().catch(() => {});
  return (
    <>
      <h3>Якорь</h3>
      <p class="muted">Якорь — это ваш ПК в роли «почтового отделения» для пространства. Он:</p>
      <ul class="muted">
        <li>хранит сообщения и файлы для тех, кто сейчас не в сети, и отдаёт их, когда человек появится — даже если отправитель уже вышел;</li>
        <li>помогает соединиться устройствам из разных сетей (телефон в мобильном интернете ↔ ПК дома) без чужих серверов.</li>
      </ul>
      <p class="muted">Переписка остаётся зашифрованной: якорь не может её прочитать. Работает, пока ПК включён и Mycel запущен (можно свернуть в трей).</p>
      <p class="muted small">Mycel сам откроет порт на роутере (UPnP), если роутер это разрешает и у вас «белый» IP.</p>
      <Show when={url()}>
        <div class="anchor-status">
          <div>Адрес: <code>{url()}</code></div>
          <Show when={state()} fallback={<div class="muted">Проверяю доступность…</div>}>
            {(st) => (
              <>
                <div>Сервер якоря: {st()[2] ? "работает" : "не запущен, перезапускаю…"}</div>
                <div>Порт на роутере: {st()[1] ? "открыт автоматически" : "не открыт автоматически"}</div>
                <div>Доступ из интернета: {st()[0] ? "есть ✓" : "не подтверждён"}</div>
                <Show when={!st()[0]}>
                  <p class="muted small">
                    {st()[1]
                      ? "Порт открыт, но проверка изнутри сети не прошла. Часто это нормально: многие роутеры не умеют «заходить сами на себя». Проверьте с телефона через мобильный интернет. Если не работает — у провайдера, возможно, нет «белого» IP (CGNAT)."
                      : "Роутер не дал открыть порт. Включите UPnP в настройках роутера (обычно адрес 192.168.0.1 или 192.168.1.1, раздел «UPnP» или «NAT») либо пробросьте TCP-порт 3340 на этот ПК вручную."}
                  </p>
                </Show>
              </>
            )}
          </Show>
        </div>
      </Show>
      <div class="actions">
        <Show when={url()} fallback={<button disabled={busy()} onClick={() => apply(true)}>Сделать этот ПК якорем</button>}>
          <button disabled={busy()} onClick={() => apply(false)}>Выключить якорь</button>
          <button disabled={busy() || !state()} onClick={() => check().catch(() => {})}>Проверить ещё раз</button>
        </Show>
      </div>
      <Show when={msg()}>
        <p class="muted">{msg()}</p>
      </Show>
    </>
  );
}

// Server check (Settings): url → round trip in ms, or the reason it did not answer.
type Check = Record<string, { ms: number | null; err: string }>;
const checkServers = async (urls: string[], mailbox: boolean): Promise<Check> =>
  Object.fromEntries((await invoke<[string, number | null, string][]>("check_servers", { urls, mailbox })).map(([u, ms, err]) => [u, { ms, err }]));
function CheckMark(p: { c?: { ms: number | null; err: string } }) {
  return (
    <Show when={p.c}>
      {(c) => (c().ms !== null
        ? <span class="ok small">✓ {c().ms} мс</span>
        : <span class="error small" title={c().err}>✗ не отвечает</span>)}
    </Show>
  );
}

function RelaySettings(p: { space: Space }) {
  const [checked, setChecked] = createSignal<Check>({});
  const [checking, setChecking] = createSignal(false);
  // The anchor is a relay too: check it with the backups.
  const targets = () => [...list(), ...(anchor(p.space.id) && !list().includes(anchor(p.space.id)!) ? [anchor(p.space.id)!] : [])];
  const check = async () => {
    setChecking(true);
    setChecked({});
    setChecked(await checkServers(targets(), false).catch(() => ({})));
    setChecking(false);
  };
  const [list, setList] = createSignal(relays(p.space.id));
  const [url, setUrl] = createSignal("");
  const [msg, setMsg] = createSignal("");
  const [now, setNow] = createSignal<string | null>();
  const refreshNow = () => invoke<string | null>("home_relay").then(setNow, () => setNow(null));
  refreshNow();
  const save = async (next: string[]) => {
    const prev = list();
    localStorage.setItem(sk("relays", p.space.id), JSON.stringify(next));
    setList(next);
    try {
      await reopen(p.space.id);
      setMsg("");
      setNow(undefined);
      refreshNow();
    } catch (e) {
      localStorage.setItem(sk("relays", p.space.id), JSON.stringify(prev));
      setList(prev);
      setMsg(errText(e));
    }
  };
  const add = (e: Event) => {
    e.preventDefault();
    let u = url().trim().replace(/\/+$/, "");
    if (!u) return;
    if (!/^https?:\/\//.test(u)) u = `https://${u}`;
    if (list().includes(u)) return setMsg("Такой сервер уже есть");
    setUrl("");
    save([...list(), u]);
  };
  return (
    <>
      <h3>Резервные серверы</h3>
      <p class="muted">Запасные серверы связи (iroh-relay) на случай, если основные недоступны: например, их блокирует провайдер. Mycel сам выбирает самый быстрый из доступных. Серверы видят только зашифрованный трафик, прочитать переписку не могут. Адреса из списка попадают в приглашения, так что новые участники тоже смогут ими пользоваться.</p>
      <div class="anchor-status">
        <div>Сейчас связь идёт через: <Show when={now() !== undefined} fallback={<span class="muted">проверяю…</span>}>
          <code>{now() ?? "только напрямую (сервер недоступен)"}</code>
        </Show></div>
      </div>
      <For each={list()} fallback={<p class="muted small">Список пуст.</p>}>
        {(r) => (
          <div class="relay-row">
            <code class="grow ellipsis">{r}</code>
            <CheckMark c={checked()[r]} />
            <button class="icon" title="Удалить" onClick={() => save(list().filter((x) => x !== r))}><Trash2 size={16} /></button>
          </div>
        )}
      </For>
      <form class="relay-row" onSubmit={add}>
        <input class="grow" placeholder="https://relay.example.com или http://1.2.3.4:3340" value={url()} onInput={(e) => setUrl(e.currentTarget.value)} />
        <button type="submit">Добавить</button>
      </form>
      <Show when={msg()}><p class="error">{msg()}</p></Show>
      <details class="muted">
        <summary>Где взять адрес сервера</summary>
        <ul>
          <Show when={!simple()}>
            <li><b>Якорь друга.</b> Если у кого-то из участников включён якорь, его адрес виден у него в «Настройки» → «Якорь» (вида <code>http://1.2.3.4:3340</code>). Попросите прислать и добавьте сюда.</li>
            <li><b>Ещё один свой ПК.</b> Установите на нём Mycel, войдите в пространство и нажмите «Настройки» → «Якорь» → «Сделать этот ПК якорем». Отдельная программа не нужна, адрес появится там же.</li>
            <li><b>VPS или сервер без экрана.</b> Нужна программа <code>mycel-anchor</code> (Linux или Windows): где её взять и как запустить — в разделе «Якорь» файла README проекта.</li>
          </Show>
          <li><b>Публичные iroh-relay.</b> Подойдёт любой сервер iroh-relay, например <code>https://euc1-1.staging-relay.n0.iroh.link</code>.</li>
        </ul>
        <p>Удалить сервер можно корзиной справа. Изменения применяются сразу, без перезапуска.</p>
      </details>
      <Show when={anchor(p.space.id) && !list().includes(anchor(p.space.id)!)}>
        <div class="relay-row">
          <span class="muted small">Якорь:</span>
          <code class="grow ellipsis">{anchor(p.space.id)}</code>
          <CheckMark c={checked()[anchor(p.space.id)!]} />
        </div>
      </Show>
      <div class="actions">
        <button class="primary" disabled={checking() || !targets().length} onClick={check}>{checking() ? "Проверяю…" : "Проверить серверы"}</button>
        <button onClick={() => save(DEFAULT_RELAYS)}>Вернуть список по умолчанию</button>
      </div>
    </>
  );
}

function MailboxSettings(p: { space: Space }) {
  const [url, setUrl] = createSignal(mailbox() ?? "");
  const [ver, setVer] = createSignal("");
  getVersion().then(setVer, () => {});
  const [msg, setMsg] = createSignal("");
  const [checked, setChecked] = createSignal<Check>({});
  const [checking, setChecking] = createSignal(false);
  const check = async () => {
    setChecking(true);
    setChecked({});
    setChecked(await checkServers([mailbox()!], true).catch(() => ({})));
    setChecking(false);
  };
  const save = async (e: Event) => {
    e.preventDefault();
    let u = url().trim().replace(/\/+$/, "");
    if (u && !/^https?:\/\//.test(u)) u = `https://${u}`;
    const prev = localStorage.getItem("mailbox");
    localStorage.setItem("mailbox", u);
    setUrl(u);
    try {
      await reopen(p.space.id);
      setMsg(u ? "Сохранено. Сообщения для тех, кто не в сети, теперь ждут их в ящике." : "Ящик выключен.");
    } catch (err) {
      prev === null ? localStorage.removeItem("mailbox") : localStorage.setItem("mailbox", prev);
      setMsg(errText(err));
    }
  };
  return (
    <>
      <h3>Почтовый ящик</h3>
      <p class="muted">Хранит сообщения для тех, кто сейчас не в сети, до 30 дней. Получатель заберёт их, когда откроет Mycel, даже если вас самих уже нет онлайн. Ящик видит только зашифрованные данные. Адрес должен быть одинаковым у всех участников.</p>
      <form class="relay-row" onSubmit={save}>
        <input class="grow" placeholder="https://mycel-box.имя.workers.dev" value={url()} onInput={(e) => setUrl(e.currentTarget.value)} />
        <button type="submit">Сохранить</button>
      </form>
      <Show when={msg()}><p class="muted small">{msg()}</p></Show>
      <Show when={mailbox()}>
        <div class="actions">
          <button disabled={checking()} onClick={check}>{checking() ? "Проверяю…" : "Проверить ящик"}</button>
          <CheckMark c={checked()[mailbox()!]} />
        </div>
      </Show>
      <details class="muted">
        <summary>Как завести свой ящик (бесплатно, 5 минут)</summary>
        <p>Нужен бесплатный аккаунт Cloudflare. Пошаговая инструкция и код лежат в папке <code>mailbox</code> проекта (файл README). Коротко: создать базу D1, создать Worker, вставить код, привязать базу под именем <code>DB</code>, скопировать адрес вида <code>https://….workers.dev</code> сюда.</p>
        <p>Пустое поле выключает ящик: тогда сообщения доходят, только когда {simple() ? "отправитель" : "отправитель или якорь"} в сети.</p>
      </details>
      <p class="muted small">Mycel {ver()}</p>
      <p class="muted small">Эмодзи: Twemoji © Twitter и участники jdecked/twemoji, лицензия CC-BY 4.0</p>
      <p class="muted small">Стикеры: Fluent Emoji © Microsoft Corporation, лицензия MIT</p>
    </>
  );
}

// In-app update (update.rs): the newest release, when it is newer than this app.
type Release = { version: string; url: string };
const [release, setRelease] = createSignal<Release | null>(null);
const [progress, setProgress] = createSignal<number | null>(null); // % while the installer downloads
const checkUpdate = () => invoke<Release | null>("update_check");
const installUpdate = async (r: Release) => {
  setProgress(0);
  const off = await listen<{ done: number; total: number }>("update-progress", (e) =>
    setProgress(e.payload.total ? Math.round((e.payload.done * 100) / e.payload.total) : 0));
  try {
    await invoke("update_install", { url: r.url, version: r.version });
  } catch (e) {
    alert(errText(e));
  } finally {
    off();
    setProgress(null);
  }
};

function AppSettings() {
  const [on, setOn] = createSignal(false);
  const [msg, setMsg] = createSignal("");
  invoke<boolean>("autostart", { on: null }).then(setOn, () => {});
  const check = () => {
    setMsg("Проверяю…");
    checkUpdate().then((r) => {
      setRelease(r);
      setMsg(r ? "" : "Установлена последняя версия");
    }, (e) => setMsg(errText(e)));
  };
  return (
    <>
      <h3>Приложение</h3>
      <Show when={!android}>
        <label class="check">
          <input type="checkbox" checked={on()} onChange={(e) => invoke<boolean>("autostart", { on: e.currentTarget.checked }).then(setOn, (err) => alert(errText(err)))} />
          Запускать вместе с Windows
        </label>
        <p class="muted">Закрытие окна сворачивает Mycel в трей (значок у часов): сообщения и звонки продолжают приходить, уведомления показываются. Выйти полностью: правой кнопкой по значку → «Выйти». Когда Mycel полностью закрыт, новые сообщения придут при следующем запуске — {simple() ? "от отправителя или с сервера" : "от отправителя или от якоря"}.</p>
      </Show>
      <Show when={release()} fallback={<button onClick={check}>Проверить обновления</button>}>
        {(r) => (
          <button class="primary" disabled={progress() !== null} onClick={() => installUpdate(r())}>
            {progress() === null ? `Обновить до ${r().version}` : `Загрузка… ${progress()}%`}
          </button>
        )}
      </Show>
      <Show when={msg()}><p class="muted">{msg()}</p></Show>
    </>
  );
}

function LoginSettings(p: { space: Space; refresh: () => void }) {
  const [old, setOld] = createSignal("");
  const [pw, setPw] = createSignal("");
  const [pw2, setPw2] = createSignal("");
  const [msg, setMsg] = createSignal("");
  const change = async (e: Event) => {
    e.preventDefault();
    if (!pw()) return setMsg("Введите новый пароль");
    if (pw() !== pw2()) return setMsg("Пароли не совпадают");
    try {
      await invoke("change_password", { old: old(), new: pw() });
      setOld("");
      setPw("");
      setPw2("");
      setMsg("Пароль изменён");
    } catch (e) {
      setMsg(errText(e));
    }
  };
  return (
    <>
      <h3>Вход</h3>
      <label class="check">
        <input type="checkbox" checked={!p.space.remembered}
          onChange={(e) => invoke("set_remember", { on: !e.currentTarget.checked }).then(p.refresh, (e) => setMsg(errText(e)))} />
        Требовать пароль при запуске
      </label>
      <form class="section" onSubmit={change}>
        <input type="password" placeholder="Текущий пароль" value={old()} onInput={(e) => setOld(e.currentTarget.value)} />
        <input type="password" placeholder="Новый пароль" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} />
        <input type="password" placeholder="Повторите новый пароль" value={pw2()} onInput={(e) => setPw2(e.currentTarget.value)} />
        <button>Сменить пароль</button>
        <Show when={msg()}><p class="muted small">{msg()}</p></Show>
      </form>
    </>
  );
}

// Settings → Резервная копия: a sealed `.mycelbak` of every space, restore, a chat as readable HTML.
function BackupSettings(p: { chats: string[] }) {
  const [pw, setPw] = createSignal("");
  const [pw2, setPw2] = createSignal("");
  const [media, setMedia] = createSignal(true);
  const [chat, setChat] = createSignal("");
  const [msg, setMsg] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [restoring, setRestoring] = createSignal(false);
  // Recovery phrase: kept by the OS vault, shown once when made; a backup sealed by it opens with the words.
  const [hasPhrase, setHasPhrase] = createSignal(false);
  const [byPhrase, setByPhrase] = createSignal(true);
  const [shown, setShown] = createSignal("");
  onMount(() => invoke<boolean>("has_phrase").then(setHasPhrase, () => {}));
  const newPhrase = async () => {
    if (hasPhrase() && !(await ask("Старые копии, сделанные по фразе, откроются только старой фразой. Создать новую?", { title: "Фраза восстановления", okLabel: "Создать", cancelLabel: "Отмена" }))) return;
    invoke<string>("new_phrase").then(setShown, (e) => setMsg(errText(e)));
  };
  const written = async () => {
    try {
      await invoke("set_phrase", { phrase: shown() });
      setHasPhrase(true);
      setByPhrase(true);
      setShown("");
    } catch (e) {
      setMsg(errText(e));
    }
  };
  const phrase = () => hasPhrase() && byPhrase();
  // Android: no save dialog paths, the file goes to Downloads.
  const target = async (name: string, ext: string) =>
    android ? null : save({ defaultPath: name, filters: [{ name: ext, extensions: [ext] }] });
  const make = async (e: Event) => {
    e.preventDefault();
    if (!phrase() && !pw()) return setMsg("Придумайте пароль для копии");
    if (!phrase() && pw() !== pw2()) return setMsg("Пароли не совпадают");
    const to = await target("mycel.mycelbak", "mycelbak");
    if (!to && !android) return;
    setBusy(true);
    setMsg("Создаём копию…");
    try {
      const [path, left] = await invoke<[string, string[]]>("backup", { to, password: pw(), media: media(), phrase: phrase() });
      setPw("");
      setPw2("");
      setMsg(`Сохранено: ${path}` + (left.length ? `. Без пароля на этом устройстве не вошли: ${left.join(", ")}` : ""));
    } catch (e) {
      setMsg(errText(e));
    }
    setBusy(false);
  };
  const exportChat = async () => {
    if (!(await ask("В файле переписка будет НЕ зашифрована: её прочитает любой, у кого окажется файл. Продолжить?", { title: "Экспорт переписки", okLabel: "Экспортировать", cancelLabel: "Отмена" }))) return;
    const to = await target("chat.html", "html");
    if (!to && !android) return;
    invoke<string>("export_chat", { chat: chat(), to }).then((path) => setMsg(`Сохранено: ${path}`), (e) => setMsg(errText(e)));
  };
  return (
    <>
      <h3>Резервная копия</h3>
      <p class="muted">Один файл с историей и чатами для нового устройства или после переустановки. Ключи шифрования в копию не входят: после восстановления это новое устройство, в {simple() ? "группы" : "пространства"} и к контактам нужно войти заново по приглашению — история останется.</p>
      <form class="section" onSubmit={make}>
        <Show when={hasPhrase()}>
          <label class="check">
            <input type="checkbox" checked={byPhrase()} onChange={(e) => setByPhrase(e.currentTarget.checked)} />
            Зашифровать фразой восстановления (вместо пароля)
          </label>
        </Show>
        <Show when={!phrase()}>
          <input type="password" placeholder="Пароль копии" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} />
          <input type="password" placeholder="Повторите пароль" value={pw2()} onInput={(e) => setPw2(e.currentTarget.value)} />
        </Show>
        <label class="check">
          <input type="checkbox" checked={media()} onChange={(e) => setMedia(e.currentTarget.checked)} />
          С фото, видео и файлами (скачанными на это устройство)
        </label>
        <div class="actions">
          <button disabled={busy()}>Создать копию</button>
          <button type="button" onClick={() => setRestoring(true)}>Восстановить из копии</button>
        </div>
      </form>
      <h3>Фраза восстановления</h3>
      <p class="muted">24 слова вместо пароля копии: файл копии и эти слова восстанавливают историю на новом устройстве.</p>
      <div class="actions">
        <button onClick={newPhrase}>{hasPhrase() ? "Создать новую фразу" : "Создать фразу восстановления"}</button>
      </div>
      <Show when={shown()}>
        <div class="modal">
          <div class="card dialog">
            <h3>Фраза восстановления</h3>
            <p class="error">Запишите слова по порядку на бумаге. Больше они не покажутся. Кто знает фразу и получит файл копии, прочтёт всю переписку.</p>
            <ol style={{ columns: 2, margin: "8px 0", "padding-left": "28px" }}>
              <For each={shown().split(" ")}>{(w) => <li>{w}</li>}</For>
            </ol>
            <div class="actions">
              <button class="primary" onClick={written}>Я записал(а)</button>
              <button onClick={() => setShown("")}>Отмена</button>
            </div>
          </div>
        </div>
      </Show>
      <div class="actions">
        <select value={chat()} onChange={(e) => setChat(e.currentTarget.value)}>
          <For each={p.chats}>{(c) => <option value={c}>{c ? nameOf(c) : groupName()}</option>}</For>
        </select>
        <button onClick={exportChat}>Экспорт переписки</button>
      </div>
      <Show when={msg()}><p class="muted small">{msg()}</p></Show>
      <Show when={restoring()}><Restore onClose={() => setRestoring(false)} /></Show>
    </>
  );
}

// Restore from a `.mycelbak`: new spaces with the backup's password, then the app starts over in them.
function Restore(p: { onClose: () => void }) {
  const [path, setPath] = createSignal("");
  const [pw, setPw] = createSignal("");
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  // By recovery phrase: 24 fields with the BIP-39 list as autocomplete; the checksum is checked by Rust.
  const [byPhrase, setByPhrase] = createSignal(false);
  const [words, setWords] = createSignal<string[]>(Array(24).fill(""));
  const [list, setList] = createSignal<Set<string>>(new Set());
  const usePhrase = (on: boolean) => {
    setByPhrase(on);
    if (on && !list().size) invoke<string[]>("phrase_words").then((l) => setList(new Set(l)), () => {});
  };
  // A pasted phrase fills the fields from this one on.
  const setWord = (i: number, v: string) => {
    const parts = v.toLowerCase().trim().split(/\s+/);
    const w = [...words()];
    if (parts.length > 1) parts.slice(0, 24 - i).forEach((p, k) => (w[i + k] = p));
    else w[i] = v.toLowerCase().trim();
    setWords(w);
  };
  // Red only once no word starts so; the button waits for every word to be in the list.
  const bad = (w: string) => !!w && list().size > 0 && ![...list()].some((x) => x.startsWith(w));
  const phraseOk = () => words().every((w) => list().has(w));
  // Android pickers give content:// URIs: the file is read here and stashed (as for attachments).
  let picker!: HTMLInputElement;
  const pick = async () => {
    if (!android) return setPath(((await open({ filters: [{ name: "Mycel", extensions: ["mycelbak"] }] })) as string | null) ?? "");
    picker.click();
  };
  const stash = async () => {
    const f = picker.files?.[0];
    if (f) setPath(await invoke<string>("stash", new Uint8Array(await f.arrayBuffer()), { headers: { name: encodeURIComponent(f.name) } }));
  };
  const run = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    setErr("");
    try {
      await invoke("restore_backup", { path: path(), password: byPhrase() ? words().join(" ") : pw(), phrase: byPhrase() });
      await invoke("close_space");
      location.reload();
    } catch (e) {
      setErr(errText(e));
      setBusy(false);
    }
  };
  return (
    <div class="modal" onClick={(e) => e.target === e.currentTarget && p.onClose()}>
      <form class="card dialog" onSubmit={run}>
        <h3>Восстановить из копии</h3>
        <input ref={picker} type="file" hidden onChange={stash} />
        <button type="button" onClick={pick}>{path() ? baseName(path()) : "Выбрать файл .mycelbak"}</button>
        <label class="check">
          <input type="checkbox" checked={byPhrase()} onChange={(e) => usePhrase(e.currentTarget.checked)} />
          По фразе из 24 слов
        </label>
        <Show when={byPhrase()} fallback={<input type="password" placeholder="Пароль копии" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} />}>
          <datalist id="bip39">
            <For each={[...list()]}>{(w) => <option value={w} />}</For>
          </datalist>
          <div style={{ display: "grid", "grid-template-columns": "1fr 1fr 1fr", gap: "4px" }}>
            <Index each={words()}>
              {(w, i) => (
                <input list="bip39" autocapitalize="off" autocomplete="off" spellcheck={false} placeholder={`${i + 1}`} value={w()}
                  style={{ "min-width": 0, "border-color": bad(w()) ? "var(--danger)" : undefined }}
                  onInput={(e) => setWord(i, e.currentTarget.value)} />
              )}
            </Index>
          </div>
        </Show>
        <p class="muted small">{simple() ? "Чаты" : "Пространства"} из копии добавятся на это устройство, их пароль — пароль копии. Затем войдите в них заново по приглашению: история уже будет на месте.</p>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <div class="actions">
          <button class="primary" disabled={busy() || !path() || (byPhrase() ? !phraseOk() : !pw())}>{busy() ? "Восстанавливаем…" : "Восстановить"}</button>
          <button type="button" onClick={p.onClose}>Отмена</button>
        </div>
      </form>
    </div>
  );
}

// Android camera scan of one QR code; throws "cancelled" on back.
async function scanQr(): Promise<string> {
  const bs = await import("@tauri-apps/plugin-barcode-scanner");
  // The plugin's scan() fails unless the camera permission was asked for first.
  if ((await bs.checkPermissions()) !== "granted" && (await bs.requestPermissions()) !== "granted")
    throw "Нет доступа к камере: разрешите его Mycel в настройках телефона";
  // Android back cancels the scan (otherwise it closes the app): give it a history entry to pop.
  history.pushState(null, "");
  let back = false;
  const onBack = () => ((back = true), bs.cancel());
  addEventListener("popstate", onBack, { once: true });
  localStorage.setItem("scanning", "1");
  try {
    return (await bs.scan({ formats: [bs.Format.QRCode] })).content;
  } finally {
    localStorage.removeItem("scanning");
    removeEventListener("popstate", onBack);
    if (!back) history.back();
  }
}

function Onboard(p: { how?: How; setHow: (h: How) => void; link: string; setLink: (v: string) => void; onDone: () => void }) {
  const [name, setName] = createSignal("");
  // Set while the camera scanner is open: if it is still set on the next start, the scanner crashed the app.
  const crashed = localStorage.getItem("scanning") === "1";
  localStorage.removeItem("scanning");
  const [err, setErr] = createSignal(crashed ? "Сканер QR не сработал на этом телефоне. Скопируйте ссылку-приглашение и вставьте её в поле выше" : "");
  const [busy, setBusy] = createSignal("");
  const [stage, setStage] = createSignal("");
  const [other, setOther] = createSignal(false);
  const run = async (what: string, cmd: string, args?: Record<string, unknown>) => {
    setBusy(what);
    setErr("");
    // Joining is one call: the hint just follows the clock (the core gives up after 40 s).
    setStage("Ищу пригласившего…");
    const slow = setTimeout(() => setStage("Всё ещё ищу… Mycel у пригласившего должен быть открыт и в сети"), 12000);
    try {
      await invoke(cmd, args);
      if (what === "join") localStorage.setItem("joined", p.link.trim()), p.setLink("");
      // Main asks again if this fails.
      await invoke("set_profile", { name: name().trim(), avatar: "", color: Math.floor(Math.random() * PALETTE.length) }).catch(() => {});
      p.onDone();
    } catch (e) {
      setErr(errText(e));
    }
    clearTimeout(slow);
    setBusy("");
  };
  return (
    <Show when={p.how || other()} fallback={
      <div class="center">
        <QuickStart space={spaceId} color={Math.floor(Math.random() * PALETTE.length)} onDone={p.onDone} onOther={() => setOther(true)} scan={android ? scanQr : undefined} />
      </div>
    }>
    <div class="center">
      <div class="card intro">
        <img src={logo} class="logo" alt="" />
        <h1>{p.how === "join" ? (simple() ? "Принять приглашение" : "Вступить по ссылке") : p.how === "create" && !simple() ? "Новое пространство" : "Добро пожаловать"}</h1>
        <p class="muted">
          {simple() ? (p.how === "join" ? "Представьтесь и вставьте ссылку, которую вам прислали" : "Начните сами и добавляйте контакты своей ссылкой — или вставьте ссылку, которую вам прислали")
            : p.how === "join" ? "Представьтесь и вставьте ссылку-приглашение" : p.how === "create" ? "Остался один шаг" : "Создайте своё пространство или вступите в существующее по ссылке-приглашению"}
        </p>
        <input placeholder="Как вас зовут?" maxLength={64} value={name()} onInput={(e) => setName(e.currentTarget.value)} autofocus />
        <Show when={!name().trim()}><p class="muted small">Имя увидят другие участники</p></Show>
        <Show when={p.how !== "join"}>
          <button class="primary" disabled={!!busy() || !name().trim()} onClick={() => run("create", "create_space")}>
            <Plus size={18} /> {busy() === "create" ? "Создаём…" : simple() ? "Начать" : "Создать пространство"}
          </button>
        </Show>
        <Show when={!p.how}><div class="or"><span>или</span></div></Show>
        <Show when={p.how !== "create"}>
          <textarea rows={3} placeholder="mycel://join/…" value={p.link} onInput={(e) => p.setLink(e.currentTarget.value)} />
          <Show when={android}>
            <button disabled={!!busy()} onClick={async () => {
              try {
                p.setLink(await scanQr());
              } catch (e) {
                if (errText(e) !== "cancelled") setErr(errText(e));
              }
            }}><Camera size={18} /> Сканировать QR</button>
          </Show>
          <button classList={{ primary: p.how === "join" }} disabled={!!busy() || !p.link.trim() || !name().trim()} onClick={() => run("join", "join", { link: p.link })}>
            <Link size={18} /> {busy() === "join" ? (simple() ? "Подключаемся…" : "Вступаем…") : simple() ? "Принять приглашение" : "Вступить по ссылке"}
          </button>
          <Show when={busy() === "join"}><p class="muted small">{stage()}</p></Show>
        </Show>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <Show when={p.how}>
          <button type="button" class="link" disabled={!!busy()} onClick={() => (setErr(""), p.setHow(p.how === "join" ? "create" : "join"))}>
            {p.how === "join" ? (simple() ? "Начать самому" : "Лучше создать своё пространство") : "Есть ссылка-приглашение?"}
          </button>
        </Show>
      </div>
    </div>
    </Show>
  );
}

const [active, setActive] = createSignal<Active>();
const [tiles, setTiles] = createSignal<Peer[]>([]);
let media: CallMedia | undefined;

function Main(p: { dir: string; theme: string; setTheme: (t: string) => void; space: Space; onLeave: () => void; refresh: () => void; onEnter: (id: string) => void; onAdd: () => void }) {
  const [del, setDel] = createSignal(false);
  const [tab, setTab] = createSignal<Tab>("chats");
  const [chatOpen, setChatOpen] = createSignal(false);
  const [find, setFind] = createSignal<string>(); // chat list search, undefined = closed
  const [arch, setArch] = createSignal(false); // the chats tab shows the archive
  const openArch = () => (history.pushState(null, ""), setArch(true));
  const [sub, setSub] = createSignal(""); // open settings page, "" = the list
  const openSub = (s: string) => (history.pushState(null, ""), setSub(s));
  // The open chat: "" = the space chat, else the 1:1 partner's id.
  const [chat, setChat] = createSignal("");
  const [msgs, setMsgs] = createSignal<Msg[]>([]);
  const members = people;
  const [peers, setPeers] = createSignal(0);
  const [rows, setRows] = createSignal<ChatRow[]>([]);
  const [callList, setCallList] = createSignal<CallView[]>([]);
  const [focused, setFocused] = createSignal(document.hasFocus());
  const wideQuery = matchMedia("(min-width: 900px)");
  const [wide, setWide] = createSignal(wideQuery.matches);
  const chatVisible = () => (wide() || chatOpen()) && focused();
  createRenderEffect(() => setOpenName(p.space.name));
  const owner = getOwner();
  // The personal space has no common chat ("").
  const loadChats = () => invoke<ChatRow[]>("chats").then((r) => setRows(personal() ? r.filter((x) => x.id) : r));
  // The anchor runs in spaces mode only: switching the mode applies that at once.
  createEffect(on(simple, () => reopen(p.space.id).then(loadChats, (e) => alert(errText(e))), { defer: true }));
  // Unread counts follow our Read receipts: reload them once the receipt is stored.
  const markRead = () => invoke("mark_read", { chat: chat() }).catch(() => {}).then(loadChats);
  const toggleMute = (on: boolean) =>
    invoke("set_muted", { chat: chat(), on }).then(loadChats, (e) => setCallErr("Не удалось изменить уведомления: " + errText(e)));
  createEffect(() => chatVisible() && msgs() && untrack(markRead));
  const sum = (rs: ChatRow[]) => rs.reduce((n, r) => n + r.unread, 0);
  const archived = () => rows().filter((r) => r.archived);
  const unread = () => sum(rows().filter((r) => !r.archived));
  const setFlag = (cmd: "set_muted" | "set_pinned" | "set_archived", id: string, on: boolean) =>
    invoke(cmd, { chat: id, on }).then(loadChats, (e) => alert("Не удалось изменить чат: " + errText(e)));
  // Chat row gestures, like on messages: right click or holding opens the menu, sliding left archives.
  const [rowMenu, setRowMenu] = createSignal<{ id: string; x: number; y: number }>();
  const [rowSwipe, setRowSwipe] = createSignal<{ id: string; dx: number }>();
  let rowTimer = 0, rowHeld = false, rowTouch: { x: number; y: number } | undefined;
  const rowPress = (e: PointerEvent, id: string) => {
    rowHeld = false;
    if (e.pointerType !== "touch") return;
    const { clientX: x, clientY: y } = e;
    rowTouch = { x, y };
    rowTimer = window.setTimeout(() => ((rowHeld = true), (rowTouch = undefined), setRowMenu({ id, x, y })), 500);
  };
  const rowMove = (e: PointerEvent, id: string) => {
    if (!rowTouch) return;
    const dx = e.clientX - rowTouch.x;
    if (!rowSwipe() && Math.hypot(dx, e.clientY - rowTouch.y) < 10) return;
    clearTimeout(rowTimer);
    rowHeld = true; // not a tap any more
    setRowSwipe({ id, dx: Math.min(0, Math.max(dx, -100)) });
  };
  // A right click, or the WebView's own long press arriving before our timer.
  const rowContext = (e: MouseEvent, id: string) => {
    e.preventDefault();
    clearTimeout(rowTimer);
    rowHeld ||= !!rowTouch;
    rowTouch = undefined;
    setRowMenu({ id, x: e.clientX, y: e.clientY });
  };
  // pointercancel means the list took the gesture to scroll.
  const rowRelease = (e: PointerEvent, r: ChatRow) => {
    clearTimeout(rowTimer);
    if (e.type === "pointerup" && (rowSwipe()?.dx ?? 0) <= -70) setFlag("set_archived", r.id, !r.archived);
    rowTouch = undefined;
    setRowSwipe(undefined);
  };

  const loadMembers = () => invoke<Member[]>("members").then(setPeople);
  const loadCalls = () => invoke<CallView[]>("calls_list").then(setCallList);
  // Latest call wins; unchanged messages keep their objects, so their bubbles (and media) don't re-render.
  let seq = 0;
  const reload = async () => {
    const s = ++seq;
    const c = chat();
    const fresh = await invoke<Msg[]>("history", { chat: c });
    if (s !== seq || c !== chat()) return;
    const old = new Map(msgs().map((m) => [m.id, m]));
    setMsgs(fresh.map((m) => {
      const o = old.get(m.id);
      return o && JSON.stringify(o) === JSON.stringify(m) ? o : m;
    }));
    loadChats();
  };
  const [profile, setProfile] = createSignal<string>();
  // «Удалить чат» stays on this device; «Удалить контакт» also hides the person in «Контакты» (no kick in the protocol).
  const deleteChat = async (id: string, contact: boolean) => {
    const text = contact
      ? `Удалить ${nameOf(id)} из контактов? Переписка удалится с этого устройства${simple() ? "" : ", из пространства человек не исключается"}`
      : `Удалить чат с ${nameOf(id)}? Переписка удалится только на этом устройстве`;
    if (!(await ask(text, { title: contact ? "Удалить контакт" : "Удалить чат", kind: "warning", okLabel: "Удалить", cancelLabel: "Отмена" }))) return;
    try {
      await invoke("delete_chat", { chat: id, contact });
    } catch (e) {
      return message("Не удалось удалить: " + errText(e), { kind: "error" });
    }
    setProfile();
    if (chat() === id) {
      setMsgs([]);
      setChat("");
      if (chatOpen()) history.back();
    }
    loadChats();
    loadMembers();
  };
  const openChat = async (id: string) => {
    if (id && memberOf(id)?.me) return;
    setCallErr("");
    try {
      if (id) await invoke("open_dm", { peer: id });
    } catch (e) {
      return setCallErr("Не удалось открыть чат: " + errText(e));
    }
    setTab("chats");
    if (chat() !== id) {
      setMsgs([]);
      setChat(id);
      await reload();
    }
    if (!chatOpen()) history.pushState(null, "");
    setChatOpen(true);
  };

  // Calls (§9): `incoming` rings full screen; `active` (module level: a call outlives a space
  // switch, its node stays open in the background) is the call screen (started = 0 while ringing out).
  const [incoming, setIncoming] = createSignal<CallSignal>();
  const [callErr, setCallErr] = createSignal("");
  const endCall = async () => {
    media?.stop();
    media = undefined;
    setTiles([]);
    setActive(undefined);
    await invoke("call_end").catch(() => {});
    reload();
  };
  // The call goes on in another space: the app keeps this space's node open until it ends.
  const afterCall = (go: () => void) => go();
  const enterSpace = (id: string) => afterCall(() => {
    const r = rows().find((x) => !x.id);
    if (r?.last) lastMsg.set(p.space.id, { text: preview(r), ts: r.last.ts });
    p.onEnter(id);
  });
  const spaceLast = (id: string) => {
    const l = lastMsg.get(id);
    return l && { text: l.text, time: when(l.ts) };
  };
  const openMedia = async (video: boolean) => {
    media = new CallMedia(setTiles);
    await media.start(video);
  };
  const startCall = async (video: boolean, to = chat()) => {
    if (active()) return;
    setCallErr("");
    try {
      if (to) await invoke("open_dm", { peer: to });
      await openMedia(video);
      setActive({ id: await invoke<string>("call_start", { chat: to, video }), video, started: 0, peers: [], chat: to });
      reload();
      loadCalls();
      // Nobody answers in 45 s: hang up (the feed shows "no answer").
      const id = active()!.id;
      setTimeout(() => active()?.id === id && !active()!.started && endCall(), 45_000);
    } catch (e) {
      setCallErr("Не удалось начать звонок: " + errText(e));
      endCall();
    }
  };
  // Answers the ring, or joins a call still going on from its line in the chat.
  const acceptCall = async (c: Pick<CallSignal, "id" | "video" | "chat"> = incoming()!) => {
    if (incoming()?.id === c.id) setIncoming(undefined);
    if (active()) await endCall();
    setCallErr("");
    try {
      await openMedia(c.video);
      const peers = await invoke<string[]>("call_accept", { id: c.id });
      setActive({ id: c.id, video: c.video, started: Date.now(), peers, chat: c.chat });
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
  // Retried sends can deliver a call's End before its Offer: never ring an ended call.
  const ended = new Set<string>();
  const onCall = (c: CallSignal) => {
    const a = active();
    if (c.action === "end") ended.add(c.id + c.author);
    // A stale ring (its End got lost) must not block a new call.
    if (c.action === "offer" && !ended.has(c.id + c.author) && Date.now() - c.ts < 60_000 && !(incoming() && Date.now() - incoming()!.ts < 60_000)) setIncoming(c);
    if ((c.action === "end" || c.action === "reject") && incoming()?.id === c.id && incoming()!.author === c.author) setIncoming(undefined);
    if (a?.id === c.id) {
      if (c.action === "accept") setActive({ ...a, started: a.started || Date.now(), peers: [...new Set([...a.peers, c.author])] });
      if (c.action === "end" || c.action === "reject") {
        const peers = a.peers.filter((p) => p !== c.author);
        setActive({ ...a, peers });
        media?.drop(c.device);
        // Everyone left (or the only other person declined): hang up.
        if ((a.started && !peers.length) || (c.action === "reject" && (a.chat || members().length <= 2))) endCall();
      }
    }
    if (c.chat === chat()) reload();
    else loadChats();
    loadCalls();
  };
  const pollPeers = () => invoke<number>("peers").then(setPeers);
  onMount(async () => {
    loadMembers();
    loadCalls();
    loadChats();
    pollPeers();
    // A join comes as an MLS commit, not a message: poll members with peers.
    const timer = setInterval(() => (pollPeers(), loadMembers()), 3000);
    // Listen before the first load, so nothing that arrives in between is lost.
    const onMsg = (c: string) => (c === chat() ? reload() : loadChats());
    const un = await listen<string>("message", (e) => onMsg(e.payload));
    const unChanged = await listen<string>("changed", (e) => (onMsg(e.payload), loadMembers()));
    const unCall = await listen<CallSignal>("call", (e) => onCall(e.payload));
    const unTyping = await listen<TypingSignal>("typing", (e) => onTyping(e.payload));
    const unStatus = await listen<[string, number][]>("status", (e) => {
      const st = new Map(e.payload);
      setMsgs((all) => all.map((m) => (st.has(m.id) && st.get(m.id) !== m.status ? { ...m, status: st.get(m.id)! } : m)));
      loadChats(); // the row's tick
    });
    await reload();
    const onVis = () => {
      invoke("set_visible", { v: !document.hidden });
      if (!document.hidden) invoke("sync_now").catch(() => {});
    };
    document.addEventListener("visibilitychange", onVis);
    // Android back closes the chat: opening it pushes a history entry.
    // On a wide screen the chat stays beside the list, so Back leaves the archive.
    const onBack = () => (chatOpen() && !(wide() && arch()) ? setChatOpen(false) : (setSub(""), setArch(false)));
    addEventListener("popstate", onBack);
    if (!(await isPermissionGranted())) requestPermission();
    const onWide = () => setWide(wideQuery.matches);
    const onFocus = () => setFocused(true);
    const onBlur = () => setFocused(false);
    wideQuery.addEventListener("change", onWide);
    addEventListener("focus", onFocus);
    addEventListener("blur", onBlur);
    // After the awaits the owner is gone: without it the cleanup would never run.
    runWithOwner(owner, () => onCleanup(() => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVis);
      removeEventListener("popstate", onBack);
      un();
      unChanged();
      unCall();
      unTyping();
      unStatus();
      wideQuery.removeEventListener("change", onWide);
      removeEventListener("focus", onFocus);
      removeEventListener("blur", onBlur);
    }));
  });

  const preview = (r: ChatRow) => {
    const l = r.last;
    if (l?.service) return serviceText(l);
    return l ? (l.mine ? "Вы: " : !r.id && !l.call ? `${nameOf(l.author)}: ` : "") + (l.call ? callLabel(l.call) : l.media ? label(l.media) : l.sticker ? `${l.text} Стикер` : l.text) : "Сообщений пока нет";
  };
  const sortedRows = () => {
    const q = find()?.trim().toLowerCase();
    // A search looks everywhere, the archive included.
    return rows().filter((r) => (q ? `${chatName(r.id)} ${preview(r)}`.toLowerCase().includes(q) : r.archived === arch()))
      .sort((a, b) => (a.pin ?? Infinity) - (b.pin ?? Infinity) || (b.last?.ts ?? 0) - (a.last?.ts ?? 0));
  };
  const chatName = (id: string) => (id ? nameOf(id) : groupName());
  const titles = { chats: "Чаты", people: "Контакты", groups: "Группы", calls: "Звонки", settings: "Настройки" };
  const navItem = (t: Tab, label: string, Icon: typeof Users) => (
    <button classList={{ nav: true, active: tab() === t }} onClick={() => { setTab(t); setChatOpen(false); setSub(""); setArch(false); setFind(undefined); }} title={label}>
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
        {navItem("people", titles.people, Contact)}
        {navItem("groups", titles.groups, Users)}
        {navItem("calls", "Звонки", Phone)}
        {navItem("settings", "Настройки", Settings)}
      </nav>

      <section class="panel">
        <Show when={release()}>
          {(r) => (
            <div class="update-banner">
              <span class="grow">Доступна версия {r().version}</span>
              <button disabled={progress() !== null} onClick={() => installUpdate(r())}>
                {progress() === null ? "Обновить" : `${progress()}%`}
              </button>
              <button class="icon" onClick={() => setRelease(null)} title="Позже"><X size={18} /></button>
            </div>
          )}
        </Show>
        <header class="bar">
          <Show when={(tab() === "settings" && sub()) || (tab() === "chats" && arch())}>
            <button class="icon" onClick={() => history.back()} title="Назад"><ArrowLeft size={20} /></button>
          </Show>
          <Show when={find() !== undefined} fallback={<>
            <h2 class="ellipsis">{tab() === "settings" && sub() ? sub() : tab() === "chats" && arch() ? "Архив" : titles[tab()]}</h2>
            <NetDot n={peers()} />
            <Show when={tab() === "chats"}><button class="icon" onClick={() => setFind("")} title="Поиск"><Search size={20} /></button></Show>
          </>}>
            <input class="grow find" placeholder="Поиск" value={find()} ref={(el) => queueMicrotask(() => el.focus())}
              onInput={(e) => setFind(e.currentTarget.value)} onKeyDown={(e) => e.key === "Escape" && setFind(undefined)} />
            <button class="icon" onClick={() => setFind(undefined)} title="Закрыть поиск"><X size={20} /></button>
          </Show>
        </header>
        <Switch>
          <Match when={tab() === "chats"}>
            <div class="scroll">
              <Show when={!arch() && find() === undefined}>
                <UsernameBanner onOpen={() => (setTab("settings"), openSub("Профиль"))} />
              </Show>
              <Show when={!arch() && !find()?.trim() && archived().length}>
                <button class="row chat-row" onClick={openArch}>
                  <div class="avatar archive"><Archive size={24} /></div>
                  <div class="grow">
                    <div class="row-top"><b class="ellipsis">Архив</b></div>
                    <div class="row-bottom">
                      <span class="muted ellipsis">{archived().map((r) => chatName(r.id)).join(", ")}</span>
                      <Show when={sum(archived())}><b class="badge off">{sum(archived())}</b></Show>
                    </div>
                  </div>
                </button>
              </Show>
              <Show when={!arch() && !find()?.trim()}>
                <SpaceRows current={p.space.id} personal={SIMPLE_NAME} last={spaceLast} onEnter={enterSpace} />
              </Show>
              <For each={sortedRows()}>
                {(r) => {
                  const l = () => r.last;
                  const draft = () => getDraft(draftKey(r.id));
                  const name = () => chatName(r.id);
                  const dx = () => (rowSwipe()?.id === r.id ? rowSwipe()!.dx : 0);
                  return (
                    // A held or slid touch must not end in a click on the row or on the menu's backdrop.
                    <button class="row chat-row" classList={{ active: wide() && chat() === r.id }} onClick={() => !rowHeld && openChat(r.id)}
                      onContextMenu={(e) => rowContext(e, r.id)}
                      onPointerDown={(e) => rowPress(e, r.id)} onPointerMove={(e) => rowMove(e, r.id)} onPointerUp={(e) => rowRelease(e, r)} onPointerCancel={(e) => rowRelease(e, r)}
                      onTouchEnd={(e) => rowHeld && e.preventDefault()}
                      style={dx() ? { transform: `translateX(${dx()}px)`, transition: "none" } : {}}>
                      <Show when={dx()}>
                        <span class="swipe-archive" style={{ opacity: Math.min(1, -dx() / 70) }}>{r.archived ? <ArchiveRestore size={22} /> : <Archive size={22} />}</span>
                      </Show>
                      {r.id ? <Avatar id={r.id} dot /> : <div class="avatar group"><Users size={20} /></div>}
                      <div class="grow">
                        <div class="row-top">
                          <b class="ellipsis">{name()}</b>
                          <Show when={r.muted}><BellOff size={14} class="muted" /></Show>
                          <small class="when">
                            <Show when={l()?.mine && !l()!.call}>{l()!.status === 2 ? <CheckCheck size={16} /> : l()!.status === 1 ? <Check size={16} /> : <Clock size={14} />}</Show>
                            {l() ? when(l()!.ts) : ""}
                          </small>
                        </div>
                        <div class="row-bottom">
                          <span class="muted ellipsis">{draft() ? <><span class="draft">Черновик:</span> {rich(draft()!)}</> : rich(preview(r))}</span>
                          <Show when={r.pin !== null}><Pin size={15} class="muted pin-mark" aria-label="Закреплён" /></Show>
                          <Show when={r.mention}><b class="badge" title="Вас упомянули">@</b></Show>
                          <Show when={r.unread}><b class="badge" classList={{ off: r.muted }}>{r.unread}</b></Show>
                        </div>
                      </div>
                    </button>
                  );
                }}
              </For>
              <Show when={!rows().length}><p class="muted small pad">Новый чат: «Контакты» → нажмите на человека → «Написать». Добавить человека — там же</p></Show>
              <Show when={find()?.trim() && !sortedRows().length}><p class="muted small pad">Ничего не найдено</p></Show>
              <Show when={find()?.trim()}>
                <DirSearch q={find()!} onOpen={(id) => (setFind(undefined), loadChats(), openChat(id))} />
              </Show>
              <Show when={arch() && !find()?.trim() && !archived().length}><p class="muted small pad">Архив пуст</p></Show>
            </div>
            <Show when={!arch()}><button class="fab" onClick={() => setTab("people")} title="Новый чат"><Pencil size={22} /></button></Show>
            <Show when={rows().find((r) => r.id === rowMenu()?.id)}>
              {(r) => {
                const full = () => rows().filter((x) => x.pin !== null).length >= 5;
                return (
                  // Android may send its own long-press contextmenu after ours has opened the menu.
                  <div class="overlay" onClick={() => setRowMenu(undefined)} onContextMenu={(e) => (e.preventDefault(), rowHeld || setRowMenu(undefined))}>
                    <div class="menu" style={{ left: `${Math.min(rowMenu()!.x, innerWidth - 240)}px`, top: `${Math.min(rowMenu()!.y, innerHeight - 200)}px` }}>
                      <Show when={!r().archived}>
                        <button disabled={r().pin === null && full()} onClick={() => setFlag("set_pinned", r().id, r().pin === null)}>
                          {r().pin === null ? <Pin size={16} /> : <PinOff size={16} />} {r().pin !== null ? "Открепить" : full() ? "Закреплено уже 5" : "Закрепить"}
                        </button>
                      </Show>
                      <button onClick={() => setFlag("set_archived", r().id, !r().archived)}>
                        {r().archived ? <ArchiveRestore size={16} /> : <Archive size={16} />} {r().archived ? "Из архива" : "В архив"}
                      </button>
                      <button onClick={() => setFlag("set_muted", r().id, !r().muted)}>
                        {r().muted ? <Bell size={16} /> : <BellOff size={16} />} {r().muted ? "Со звуком" : "Без звука"}
                      </button>
                      <Show when={r().unread}>
                        <button onClick={() => invoke("mark_read", { chat: r().id }).then(loadChats, (e) => alert("Не удалось отметить: " + errText(e)))}>
                          <CheckCheck size={16} /> Отметить прочитанным
                        </button>
                      </Show>
                    </div>
                  </div>
                );
              }}
            </Show>
          </Match>
          <Match when={tab() === "calls"}>
            <Calls calls={callList()} onCall={startCall} inCall={!!active()} />
          </Match>
          <Match when={tab() === "people"}>
            <People members={members()} simple={simple() && personal()} onWrite={openChat} onProfile={setProfile} onCall={(id, v) => startCall(v, id)} inCall={!!active()} />
          </Match>
          <Match when={tab() === "groups"}>
            <div class="scroll">
              <NewGroup people={members()} onEnter={enterSpace} />
              {/* The open space, if it is a group, then the others. */}
              <Show when={!personal()}>
                <button class="row chat-row" onClick={() => openChat("")}>
                  <div class="avatar group"><Users size={20} /></div>
                  <div class="grow">
                    <div class="row-top"><b class="ellipsis">{groupName()}</b></div>
                    <Show when={rows().find((r) => !r.id)}>{(r) => <div class="row-bottom"><span class="muted ellipsis">{rich(preview(r()))}</span></div>}</Show>
                  </div>
                </button>
              </Show>
              <SpaceRows current={p.space.id} personal={SIMPLE_NAME} groupsOnly last={spaceLast} onEnter={enterSpace} />
            </div>
          </Match>
          <Match when={tab() === "settings"}>
            <Show when={sub()} fallback={
              <div class="scroll">
                <button class="row me-card" onClick={() => openSub("Профиль")}>
                  <Show when={members().find((m) => m.me)}>{(me) => <Avatar id={me().id} />}</Show>
                  <div class="grow">
                    <b class="ellipsis">{members().find((m) => m.me)?.name ?? "Без имени"}</b>
                    <div class="muted small">Имя и аватар<Username id="" me /></div>
                  </div>
                </button>
                <Show when={simple()}>
                  <Accounts current={p.space.id} personal={SIMPLE_NAME} myName={members().find((m) => m.me)?.name} onEnter={enterSpace} onAdd={() => afterCall(p.onAdd)} />
                </Show>
                <div class="list">
                  <For each={[
                    ["Оформление", Palette, "Тема, обои, размер текста"],
                    ["Голос и видео", Mic, "Микрофон, камера, громкость"],
                    ["Связь", Wifi, "Серверы, почтовый ящик" + (android || simple() ? "" : ", якорь")],
                    ["Приложение", Monitor, android ? "Обновления" : "Автозапуск, трей, обновления"],
                    ["Устройства", Smartphone, "Телефон и ПК как один аккаунт"],
                    ["Конфиденциальность", EyeOff, "Время захода, звонки, прочтение, поиск"],
                    ["Вход", LockIcon, "Пароль при запуске"],
                    ["Резервная копия", Archive, "Копия, восстановление, экспорт чата"],
                    ["Другие способы", Layers, "Режим пространств, свой сервер"],
                    [simple() ? "Данные" : "Пространство", Database, "Хранение, удаление"],
                  ] as [string, typeof Mic, string][]}>
                    {([t, Icon, hint]) => (
                      <button class="row item" onClick={() => openSub(t)}>
                        <Icon size={22} />
                        <div class="grow"><div>{t}</div><div class="muted small ellipsis">{hint}</div></div>
                        <ChevronRight size={18} class="muted" />
                      </button>
                    )}
                  </For>
                </div>
              </div>
            }>
              <div class="scroll pad">
                <Switch>
                  <Match when={sub() === "Профиль"}>
                    <ProfileForm submit="Сохранить" />
                    <UsernameSettings space={p.space.id} />
                  </Match>
                  <Match when={sub() === "Оформление"}>
                    <h3>Тема</h3>
                    <div class="segmented">
                      <For each={[["system", "Как в системе"], ["light", "Светлая"], ["dark", "Тёмная"]]}>
                        {([v, label]) => (
                          <button classList={{ active: p.theme === v }} onClick={() => p.setTheme(v)}>{label}</button>
                        )}
                      </For>
                    </div>
                    <Appearance theme={p.theme} />
                  </Match>
                  <Match when={sub() === "Голос и видео"}>
                    <VoiceSettings />
                  </Match>
                  <Match when={sub() === "Связь"}>
                    <label class="check">
                      <input type="checkbox" checked={n0(p.space.id)} onChange={(e) => {
                        localStorage.setItem(sk("n0", p.space.id), e.currentTarget.checked ? "1" : "0");
                        reopen(p.space.id).catch((e) => alert(errText(e)));
                      }} />
                      Публичные серверы связи n0 (Европа, США, Азия)
                    </label>
                    <p class="muted">Помогают устройствам из разных сетей найти друг друга. Сами сообщения после этого идут напрямую, а через сервер — только если напрямую не получилось. Когда {simple() ? "" : "в пространстве есть якорь или "}добавлен резервный сервер, связь идёт только через них, а n0 нужен лишь чтобы находить адреса.</p>
                    <RelaySettings space={p.space} />
                    <MailboxSettings space={p.space} />
                    <Show when={!android && !simple()}>
                      <AnchorSettings space={p.space} />
                    </Show>
                  </Match>
                  <Match when={sub() === "Приложение"}>
                    <AppSettings />
                  </Match>
                  <Match when={sub() === "Конфиденциальность"}>
                    <PrivacySettings space={p.space.id} />
                  </Match>
                  <Match when={sub() === "Вход"}>
                    <LoginSettings space={p.space} refresh={p.refresh} />
                  </Match>
                  <Match when={sub() === "Устройства"}>
                    <Devices />
                  </Match>
                  <Match when={sub() === "Резервная копия"}>
                    <BackupSettings chats={rows().map((r) => r.id)} />
                  </Match>
                  <Match when={sub() === "Другие способы"}>
                    <ModeSettings />
                  </Match>
                  <Match when={true}>
                    <h3>{simple() ? "Данные" : `Пространство «${p.space.name}»`}</h3>
                    <p class="muted">Зашифрованная база хранится здесь:</p>
                    <code class="path">{p.dir}</code>
                    <div class="actions">
                      <Show when={!simple()}><button onClick={() => afterCall(p.onLeave)}><LogOut size={18} /> Сменить пространство</button></Show>
                      <button class="danger" onClick={() => setDel(true)}><Trash2 size={18} /> Удалить с этого устройства</button>
                    </div>
                    <Show when={del()}>
                      <RemoveSpace space={p.space} onClose={() => setDel(false)} onDone={p.onLeave} />
                    </Show>
                  </Match>
                </Switch>
              </div>
            </Show>
          </Match>
        </Switch>
      </section>

      <Show when={!(personal() && !chat()) && { c: chat() }} keyed
        fallback={<section class="chat"><div class="msgs"><p class="empty muted">Выберите чат или контакт</p></div></section>}>
        {(k) => (
          <Chat chat={k.c} msgs={msgs()} members={members().length} me={members().find((m) => m.me)?.id ?? ""} admin={members().some((m) => m.me && m.admin)}
            peers={peers()} back={() => history.back()} reload={reload} inCall={!!active()} onCall={(v) => startCall(v)} callErr={callErr()}
            openChat={openChat} onJoin={acceptCall} muted={!!rows().find((r) => r.id === k.c)?.muted} onMute={toggleMute}
            onProfile={() => setProfile(k.c)} onMention={setProfile} onDelete={() => deleteChat(k.c, false)} />
        )}
      </Show>
      <Show when={profile()} keyed>
        {(id) => (
          <Profile id={id} inCall={!!active()} onClose={() => setProfile()} onDelete={(c) => deleteChat(id, c)}
            onWrite={() => (setProfile(), openChat(id))} onCall={(v) => (setProfile(), startCall(v, id))} />
        )}
      </Show>
      <Show when={people().some((m) => m.me && !m.name)}>
        <div class="modal">
          <div class="card dialog">
            <h3>Как вас зовут?</h3>
            <p class="muted small">Имя и аватар увидят другие участники. Поменять можно в Настройках.</p>
            <ProfileForm submit="Готово" />
          </div>
        </div>
      </Show>
      <Show when={active()}>
        <CallScreen a={active()!} tiles={tiles()} media={media!} onEnd={endCall} />
      </Show>
      {/* After the call screen: a new call rings on top of an active one. */}
      <Show when={incoming()}>
        <Incoming c={incoming()!} onAccept={() => acceptCall()} onReject={rejectCall} />
      </Show>
    </div>
  );
}

/** A person's profile, like Telegram's: opened from the chat header and from «Контакты». */
function Profile(p: { id: string; inCall: boolean; onClose: () => void; onWrite: () => void; onCall: (video: boolean) => void; onDelete: (contact: boolean) => void }) {
  const m = () => memberOf(p.id);
  const [code, setCode] = createSignal<{ link: string; svg: string }>();
  const [err, setErr] = createSignal("");
  const showCode = () => (code() ? setCode() : invoke<{ link: string; svg: string }>("safety_code", { id: p.id }).then(setCode, (e) => setErr(errText(e))));
  return (
    <div class="modal" onClick={p.onClose}>
      <div class="card dialog profile" onClick={(e) => e.stopPropagation()}>
        <Avatar id={p.id} dot />
        <h3>{nameOf(p.id)}</h3>
        <div class="muted small" classList={{ online: !!m()?.online }}>{presence(m())}<Username id={p.id} /></div>
        <button onClick={p.onWrite}><MessageCircle size={18} /> Написать</button>
        <button disabled={p.inCall} onClick={() => p.onCall(false)}><Phone size={18} /> Позвонить</button>
        <button disabled={p.inCall} onClick={() => p.onCall(true)}><Video size={18} /> Видеозвонок</button>
        <button onClick={showCode}><ShieldCheck size={18} /> Код безопасности</button>
        <Show when={code()}>
          {(c) => (
            <>
              <div class="qr" innerHTML={c().svg} />
              <code class="path">{c().link}</code>
              <p class="muted small">Сверьте код с собеседником лично; отметить проверенным можно в «Контактах»</p>
            </>
          )}
        </Show>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <button class="del" onClick={() => p.onDelete(false)}><Trash2 size={18} /> Удалить чат</button>
        <button class="del" onClick={() => p.onDelete(true)}><UserMinus size={18} /> Удалить контакт</button>
      </div>
    </div>
  );
}

function People(p: { members: Member[]; simple: boolean; onWrite: (id: string) => void; onProfile: (id: string) => void; onCall: (id: string, video: boolean) => void; inCall: boolean }) {
  const [inv, setInv] = createSignal<{ link: string; svg: string }>();
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal("");
  const [copied, setCopied] = createSignal(false);
  const amAdmin = () => p.members.some((m) => m.me && m.admin);
  const shown = () => p.members.filter((m) => !m.removed);
  // Verified members on this device: name (or short id) -> safety code; "!" = the key changed since.
  const vKey = `verified:${spaceId}`;
  const readVer = (): Record<string, string> => {
    try {
      return JSON.parse(localStorage.getItem(vKey) ?? "{}");
    } catch {
      return {};
    }
  };
  const [ver, setVer] = createSignal(readVer());
  const saveVer = (v: Record<string, string>) => (localStorage.setItem(vKey, JSON.stringify(v)), setVer(v));
  const vName = (m: Member) => m.name ?? m.id;
  const [codes, setCodes] = createSignal<Record<string, string>>({});
  const codeOf = async (id: string) => {
    const c = codes()[id] ?? (await invoke<{ link: string; svg: string }>("safety_code", { id })).link;
    setCodes({ ...codes(), [id]: c });
    return c;
  };
  // A verified name whose device now gives another code (new device or key): drop the mark, warn.
  createEffect(() => {
    for (const m of p.members) {
      const v = untrack(ver)[vName(m)];
      if (!m.me && v && v !== "!")
        codeOf(m.id).then((c) => c !== v && saveVer({ ...readVer(), [vName(m)]: "!" })).catch(() => {});
    }
  });
  const status = (m: Member) => {
    const v = ver()[vName(m)];
    return v === "!" ? "changed" : v && v === codes()[m.id] ? "ok" : "";
  };
  const [safe, setSafe] = createSignal<{ m: Member; link: string; svg: string; err?: string }>();
  const openSafe = async (m: Member) => {
    if (safe()?.m.id === m.id) return setSafe();
    try {
      const s = await invoke<{ link: string; svg: string }>("safety_code", { id: m.id });
      setCodes({ ...codes(), [m.id]: s.link });
      setSafe({ m, ...s });
    } catch (e) {
      setErr(errText(e));
    }
  };
  const markSafe = (on: boolean) => {
    const s = safe()!, v = readVer();
    if (on) v[vName(s.m)] = s.link;
    else delete v[vName(s.m)];
    saveVer(v);
  };
  const scanSafe = async () => {
    try {
      const got = (await scanQr()).trim();
      if (got === safe()!.link) markSafe(true), setSafe({ ...safe()!, err: undefined });
      else setSafe({ ...safe()!, err: "Коды не совпадают: возможна подмена собеседника" });
    } catch (e) {
      if (errText(e) !== "cancelled") setSafe({ ...safe()!, err: errText(e) });
    }
  };
  const invite = async () => {
    setBusy(true);
    setErr("");
    try {
      setInv(await invoke<{ link: string; svg: string }>(p.simple ? "contact_invite" : "invite"));
    } catch (e) {
      setErr(errText(e));
    }
    setBusy(false);
  };
  const [got, setGot] = createSignal("");
  const accept = async () => {
    setBusy(true);
    setErr("");
    try {
      await invoke("join", { link: got() });
      setGot("");
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
        <button class="primary" disabled={!(p.simple || amAdmin()) || busy()} onClick={invite}>
          <UserPlus size={18} /> {busy() ? "Готовим ссылку…" : p.simple ? "Добавить контакт" : simple() ? "Пригласить в группу" : "Пригласить"}
        </button>
        <Show when={!p.simple && !amAdmin()}>
          <p class="muted small">Приглашать может только администратор</p>
        </Show>
        <Show when={p.simple && !inv()}>
          <p class="muted small">Покажите QR или отправьте ссылку человеку — когда он её примет, вы появитесь друг у друга в контактах</p>
          <div class="row">
            <input class="grow" placeholder="Или вставьте ссылку, которую прислали вам" value={got()} onInput={(e) => setGot(e.currentTarget.value)} />
            <button disabled={busy() || !got().trim()} onClick={accept}><Link size={18} /> Принять</button>
          </div>
        </Show>
        <Show when={err()}><p class="error">{err()}</p></Show>
        <Show when={inv()}>
          <div class="card invite">
            <div class="qr" innerHTML={inv()!.svg} />
            <p class="muted small">{p.simple ? "Ссылка одноразовая: для одного человека. Других ваших контактов он не увидит" : "Ссылка одноразовая: по ней сможет вступить один человек"}</p>
            <code class="path">{inv()!.link}</code>
            <button onClick={copy}>{copied() ? <Check size={18} /> : <Copy size={18} />} {copied() ? "Скопировано" : "Копировать"}</button>
          </div>
        </Show>
        <Show when={safe()}>
          {(s) => (
            <div class="card invite">
              <b>Код безопасности · {nameOf(s().m.id)}</b>
              <div class="qr" innerHTML={s().svg} />
              <code class="path">{s().link}</code>
              <p class="muted small">Сверьте код с собеседником лично или отсканируйте его QR: совпадает — значит, на связи именно его устройство</p>
              <Show when={android}><button onClick={scanSafe}><Camera size={18} /> Сканировать QR собеседника</button></Show>
              <Show when={status(s().m) === "ok"} fallback={<button class="primary" onClick={() => markSafe(true)}><ShieldCheck size={18} /> Отметить проверенным</button>}>
                <button onClick={() => markSafe(false)}>Снять отметку «Проверен»</button>
              </Show>
              <Show when={s().err}><p class="error">{s().err}</p></Show>
            </div>
          )}
        </Show>
      </div>
      <p class="section muted">{p.simple ? `Контакты · ${shown().length - 1}` : `Участники · ${shown().length}`}</p>
      <For each={shown()}>
        {(m) => (
          <div class="row person" classList={{ link: !m.me }} onClick={() => !m.me && p.onProfile(m.id)}>
            <Avatar id={m.id} dot onClick={m.me ? undefined : () => p.onProfile(m.id)} />
            <div class="grow">
              <b class="ellipsis">{nameOf(m.id)}{m.me ? " (вы)" : ""}</b>
              <div class="muted small" classList={{ online: m.online }} title={simple() ? undefined : m.id}>{m.me ? (simple() ? "в сети" : m.id) : presence(m)}<Show when={simple()}><Username id={m.id} me={m.me} /></Show><Show when={m.admin && !simple()}> · админ</Show>
                <Show when={status(m) === "ok"}> · проверен</Show></div>
              <Show when={status(m) === "changed"}><div class="error">Ключ сменился (новое устройство?) — проверьте код безопасности заново</div></Show>
            </div>
            <Show when={!m.me}>
              <button class="icon" onClick={(e) => (e.stopPropagation(), openSafe(m), e.currentTarget.closest(".scroll")?.scrollTo({ top: 0, behavior: "smooth" }))} title="Код безопасности">
                {status(m) === "changed" ? <ShieldAlert size={20} /> : <ShieldCheck size={20} />}
              </button>
              <button class="icon" disabled={p.inCall} onClick={(e) => (e.stopPropagation(), p.onCall(m.id, false))} title="Позвонить"><Phone size={20} /></button>
            </Show>
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
  !!a && !!b && !a.call && !b.call && !a.service && !b.service && a.mine === b.mine && (a.mine || a.author === b.author)
  && dayKey(a.ts) === dayKey(b.ts) && Math.abs(b.ts - a.ts) < 600_000;
// The node writes «включил(а) …»; own lines read «Вы включили …».
const serviceText = (m: Msg) => (m.mine ? `Вы ${m.text.replace("ил(а)", "или")}` : `${nameOf(m.author)} ${m.text}`);
const TTLS: [number, string][] = [[0, "Выкл"], [86_400, "1 день"], [604_800, "1 неделя"], [2_592_000, "1 месяц"]];
const plain = (m: Msg) => m.text || (m.media ? label(m.media) : "");
type Dialog = { kind: "delete" | "forward"; ids: string[] };
const baseName = (path: string) => path.split(/[\\/]/).pop() ?? path;
const isPhoto = (path: string) => /\.(jpe?g|png|webp)$/i.test(path);
const isVideo = (path: string) => /\.(mp4|m4v|webm|mov|mkv)$/i.test(path);

function Chat(p: {
  chat: string; msgs: Msg[]; members: number; me: string; admin: boolean; peers: number; back: () => void; reload: () => Promise<void>;
  inCall: boolean; onCall: (video: boolean) => void; callErr: string; openChat: (id: string) => void; onJoin: (c: CallView) => void;
  muted: boolean; onMute: (on: boolean) => void; onProfile: () => void; onMention: (id: string) => void; onDelete: () => void;
}) {
  const key = draftKey(p.chat);
  const [text, setText] = createSignal(getDraft(key));
  // Sent but not in the history yet (⏱); `real` is the event id once the node has it.
  const [pending, setPending] = createSignal<(Msg & { real?: string })[]>([]);
  const [reply, setReply] = createSignal<Msg>();
  const [editing, setEditing] = createSignal<Msg>();
  const [menu, setMenu] = createSignal<{ m: Msg; x: number; y: number }>();
  const [more, setMore] = createSignal(false); // the chat header's ⋮ menu
  const [ttl, setTtl] = createSignal(0), [ttlPick, setTtlPick] = createSignal(false); // «Автоудаление»
  // Disappearing messages go from the feed the moment they expire.
  const [clock, setClock] = createSignal(Date.now());
  const clockT = setInterval(() => p.msgs.some((m) => m.ttl) && setClock(Date.now()), 1000);
  onCleanup(() => clearInterval(clockT));
  const [sel, setSel] = createSignal<string[]>([]);
  const [dlg, setDlg] = createSignal<Dialog>();
  const [atBottom, setAtBottom] = createSignal(true);
  const [fresh, setFresh] = createSignal(0);
  const [pinIx, setPinIx] = createSignal(0);
  const name = (id: string) => (id === p.me ? "Вы" : nameOf(id));
  const shown = () => {
    const ids = new Set(p.msgs.map((m) => m.id));
    const live = p.msgs.filter((m) => !m.ttl || m.ts + m.ttl * 1000 > clock());
    return [...live, ...pending().filter((x) => !x.real || !ids.has(x.real))];
  };
  // Bubbles: an album is one, shown at its first message.
  const grouped = createMemo(() => albums(shown()));
  const itemsOf = (m: Msg) => grouped().items.get(m.id) ?? [m];
  // An album speaks (reply, copy, edit) through the message carrying its caption.
  const headOf = (m: Msg) => itemsOf(m).find((x) => x.text) ?? m;
  // Reactions of all an album's messages; a tap goes to the one already holding mine.
  const reactionsOf = (m: Msg) => {
    const all = new Map<string, { who: string[]; id: string }>();
    for (const x of itemsOf(m))
      for (const [e, who] of x.reactions) {
        const r = all.get(e) ?? { who: [], id: x.id };
        all.set(e, { who: [...new Set([...r.who, ...who])], id: who.includes(p.me) ? x.id : r.id });
      }
    return [...all];
  };
  createEffect(() => {
    const ids = new Set(p.msgs.map((m) => m.id));
    setPending((ps) => (ps.some((x) => x.real && ids.has(x.real)) ? ps.filter((x) => !x.real || !ids.has(x.real)) : ps));
  });
  let tmp = 0;
  // One send at a time: parallel invokes would reach the node (and get their order) in any order.
  let chain: Promise<unknown> = Promise.resolve();
  const queue = async (m: Partial<Msg>, run: () => Promise<string>, undo?: () => void) => {
    const id = `tmp-${++tmp}`;
    setPending((ps) => [...ps, { id, author: p.me, mine: true, text: "", ts: Date.now(), status: 0, media: null, call: null, reply: null, edited: false, pinned: false, reactions: [], ttl: 0, fwd: null, service: false, ...m }]);
    try {
      const job = chain.then(run);
      chain = job.catch(() => {});
      const real = await job;
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
    const el = list.querySelector(`[data-id~="${id}"]`);
    if (!el) return;
    el.scrollIntoView({ block: "center", behavior: "smooth" });
    el.classList.add("flash");
    setTimeout(() => el.classList.remove("flash"), 1600);
  };
  // Search in the loaded history; query undefined = closed. Starts at the newest match.
  const [query, setQuery] = createSignal<string>();
  const [cur, setCur] = createSignal("");
  const re = createMemo(() => {
    const q = query()?.trim();
    // The capturing group makes split() keep the matches (odd items) for highlighting.
    return q ? new RegExp(`(${q.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")})`, "iu") : undefined;
  });
  const hits = createMemo(() => {
    const r = re();
    return r ? p.msgs.filter((m) => !m.call && r.test(m.text)).map((m) => m.id) : [];
  });
  const hit = () => (hits().includes(cur()) ? cur() : hits().at(-1));
  const goHit = (d: number) => {
    const h = hits();
    if (!h.length) return;
    const id = h[(h.indexOf(hit()!) + d + h.length) % h.length];
    setCur(id);
    jump(id);
  };
  const toggle = (m: Msg) => {
    if (m.id.startsWith("tmp-")) return;
    const ids = itemsOf(m).map((x) => x.id);
    setSel((s) => (s.includes(m.id) ? s.filter((x) => !ids.includes(x)) : [...s, ...ids]));
  };
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
    if (editing()) setText(getDraft(key));
    setEditing(undefined);
    setReply(m);
    input.focus();
  };
  const cancelCompose = () => {
    if (editing()) setText(getDraft(key));
    setEditing(undefined);
    setReply(undefined);
    queueMicrotask(grow);
  };
  // Touch: holding opens the menu, sliding right replies.
  let pressTimer = 0, held = false, touch: { x: number; y: number } | undefined;
  const [swipe, setSwipe] = createSignal<{ id: string; dx: number }>();
  const press = (e: PointerEvent, m: Msg) => {
    if (e.pointerType !== "touch" || sel().length || m.id.startsWith("tmp-")) return;
    const { clientX: x, clientY: y } = e;
    touch = { x, y };
    held = false;
    pressTimer = window.setTimeout(() => ((held = true), (touch = undefined), setMenu({ m, x, y })), 500);
  };
  const swipeMove = (e: PointerEvent, m: Msg) => {
    if (!touch) return;
    const dx = e.clientX - touch.x;
    // A resting finger jitters: only a real move cancels the hold.
    if (!swipe() && Math.hypot(dx, e.clientY - touch.y) < 10) return;
    clearTimeout(pressTimer);
    setSwipe({ id: m.id, dx: Math.max(0, Math.min(dx, 80)) });
  };
  // pointercancel means the list took the gesture to scroll: no reply then.
  const unpress = (e: PointerEvent, m: Msg) => {
    clearTimeout(pressTimer);
    if (e.type === "pointerup" && (swipe()?.dx ?? 0) >= 60) startReply(headOf(m));
    touch = undefined;
    setSwipe(undefined);
  };
  const openMenu = (e: MouseEvent, m: Msg) => {
    e.preventDefault();
    if (!sel().length && !m.id.startsWith("tmp-")) setMenu({ m, x: e.clientX, y: e.clientY });
  };
  // Double click / double tap = ❤️. Counted by hand: not every WebView sends dblclick for taps.
  let tap = { id: "", at: 0 };
  const clickMsg = (e: MouseEvent, m: Msg) => {
    if (sel().length || e.shiftKey) {
      e.preventDefault();
      return toggle(m);
    }
    if (m.id.startsWith("tmp-") || (e.target as Element).closest("button, a, img:not(.sticker-img), video, .wave")) return;
    const double = tap.id === m.id && e.timeStamp - tap.at < 350;
    tap = { id: double ? "" : m.id, at: e.timeStamp };
    if (double) act(invoke("react", { id: headOf(m).id, emoji: "❤️" }));
  };
  const [err, setErr] = createSignal("");
  // The message open in the full-screen viewer, and everything it can leaf through.
  const [view, setView] = createSignal("");
  const openView = (id: string) => !sel().length && setView(id);
  const shots = createMemo(() => p.msgs.filter((m) => m.media?.kind === "photo" || m.media?.kind === "video") as Shot[]);
  // Files waiting in the "send" window (like Telegram's): photos compressed unless unticked.
  const [toSend, setToSend] = createSignal<string[]>([]);
  const [caption, setCaption] = createSignal("");
  const [compress, setCompress] = createSignal(localStorage.getItem("compress") !== "0");
  const [drag, setDrag] = createSignal(false);
  const [video, setVideo] = createSignal(false);
  const [rec, setRec] = createSignal<Recording>();
  const [elapsed, setElapsed] = createSignal(0);
  const [slide, setSlide] = createSignal(0); // finger moved left while holding, px (≤ 0)
  const [lift, setLift] = createSignal(0); // finger moved up while holding, px (≥ 0)
  const [locked, setLocked] = createSignal(false); // recording locked: hands-free, send/cancel by buttons
  const [hint, setHint] = createSignal(""); // shown after a press too short to record
  const [emo, setEmo] = createSignal<number>(); // open emoji panel: tab index, -1 = recent
  const insertEmoji = (e: string) => {
    const a = input.selectionStart, b = input.selectionEnd, t = text();
    setText(t.slice(0, a) + e + t.slice(b));
    input.value = text();
    input.setSelectionRange(a + e.length, a + e.length);
    useEmoji(e);
    grow();
  };
  let preview!: HTMLVideoElement;
  // Android pickers give content:// URIs, not paths: read the files here and stash them for send_files.
  let picker!: HTMLInputElement;
  const pick = async () => {
    const files = [...(picker.files ?? [])];
    picker.value = "";
    const paths = await Promise.all(files.map(async (f) =>
      invoke<string>("stash", new Uint8Array(await f.arrayBuffer()), { headers: { name: encodeURIComponent(f.name) } })));
    sendFiles(paths);
  };

  const sendFiles = async (paths: string[]) => {
    if (!paths.length) return;
    await invoke("allow_preview", { paths }).catch(() => {});
    setCaption("");
    setToSend(paths);
  };
  const confirmSend = () => {
    const paths = toSend(), original = !compress(), text = caption();
    if (!paths.length) return;
    setToSend([]);
    queue({ text: paths.length > 1 ? `📎 Файлов: ${paths.length}` : `📎 ${baseName(paths[0])}` },
      () => invoke<string>("send_files", { chat: p.chat, paths, original, caption: text }),
      // Part of them may have gone out before the failure.
      () => void p.reload());
  };
  const attach = async () => {
    if (android) return picker.click();
    const paths = await open({ multiple: true });
    if (paths) sendFiles(paths);
  };
  onMount(async () => {
    // Media that loads later grows the list: keep the newest message in view.
    const bottom = () => atBottom() && (list.scrollTop = list.scrollHeight);
    list.addEventListener("load", bottom, true);
    list.addEventListener("loadedmetadata", bottom, true);
    // The reply bar or a growing input shrinks the list: keep the last message in view.
    // A restored draft (or a new text size) gets its height once the chat is visible (hidden on a phone until opened).
    let sized = 0;
    const fit = () => {
      if (list.clientHeight && sized !== msgSize()) (sized = msgSize()), grow();
    };
    const ro = new ResizeObserver(() => (fit(), bottom()));
    ro.observe(list);
    onCleanup(() => ro.disconnect());
    createEffect(() => (msgSize(), queueMicrotask(() => (fit(), bottom()))));
    const un = await getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "drop") {
        setDrag(false);
        sendFiles(e.payload.paths);
      } else setDrag(e.payload.type !== "leave");
    });
    // Ctrl+V of a screenshot or copied files: the same as attaching them.
    const paste = async (e: ClipboardEvent) => {
      const files = [...(e.clipboardData?.files ?? [])];
      if (!files.length) return;
      e.preventDefault();
      const paths = await Promise.all(files.map(async (f) =>
        invoke<string>("stash", new Uint8Array(await f.arrayBuffer()), { headers: { name: encodeURIComponent(f.name || "image.png") } })));
      sendFiles(paths);
    };
    document.addEventListener("paste", paste);
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || view()) return; // the viewer closes itself
      if (toSend().length) return setToSend([]);
      if (menu() || dlg()) return (setMenu(undefined), setDlg(undefined));
      if (rec()) return stopRec(false);
      if (sel().length) return setSel([]);
      if (query() !== undefined) return setQuery(undefined);
      cancelCompose();
    };
    addEventListener("keydown", esc);
    onCleanup(() => {
      un();
      document.removeEventListener("paste", paste);
      removeEventListener("keydown", esc);
    });
  });

  // 🎤/📹: a tap switches the mode, holding records; release sends, slide sideways (or Esc) cancels.
  // Hold: slide left past 120px cancels; slide up past 80px locks (release does nothing then; the big button sends).
  let holdTimer = 0, holdId = 0, hintTimer = 0, tick = 0, startX = 0, startY = 0, holding = false, red = false, raf = 0, halo!: HTMLElement;
  let ctx: AudioContext | undefined;
  // Telegram-like haptics: short ticks on start, entering the cancel zone, lock, send and cancel; never on every move.
  const vib = (ms: number) => android && void invoke("vibrate", { ms }).catch(() => {});
  // The halo behind the mic scales 1.0–1.25 with the microphone's loudness (smoothed).
  const meter = (stream: MediaStream) => {
    ctx = new AudioContext();
    const an = ctx.createAnalyser();
    an.fftSize = 512;
    ctx.createMediaStreamSource(stream).connect(an);
    const buf = new Uint8Array(an.fftSize);
    let lvl = 0;
    const frame = () => {
      an.getByteTimeDomainData(buf);
      let sum = 0;
      for (const v of buf) sum += (v - 128) ** 2;
      lvl += (Math.min(1, Math.sqrt(sum / buf.length) / 128 * 4) - lvl) * 0.25;
      if (halo) halo.style.transform = `scale(${1 + 0.25 * lvl})`;
      raf = requestAnimationFrame(frame);
    };
    frame();
  };
  const stopRec = async (send: boolean) => {
    const r = rec();
    holding = false; // a release after the recording ended by itself does nothing
    clearTimeout(holdTimer);
    clearInterval(tick);
    cancelAnimationFrame(raf);
    void ctx?.close();
    ctx = undefined;
    setRec(undefined);
    setSlide(0);
    setLift(0);
    setLocked(false);
    if (!r) return;
    if (!send || Date.now() - r.started < 500) {
      if (!send) vib(50); // a deliberate cancel ticks; a too-short press is dropped
      return r.cancel();
    }
    const blob = await r.stop();
    vib(30); // after the mic closes: Android mutes haptics while audio is being recorded
    const kind = video() ? "videonote" : "voice";
    const wave = kind === "voice" ? await waveform(blob) : [];
    const body = new Uint8Array(await blob.arrayBuffer());
    const headers = { chat: p.chat, kind, mime: r.mime, duration: String(Date.now() - r.started), wave: wave.join(",") };
    await queue({ text: kind === "voice" ? "🎤 Голосовое" : "📹 Видеосообщение" }, () => invoke<string>("send_rec", body, { headers }));
  };
  const down = (e: PointerEvent) => {
    e.preventDefault(); // no focus, text selection or closing keyboard: the press only records
    if (!e.isPrimary || e.button !== 0) return;
    // Locked: the big button is Send.
    if (locked()) return void stopRec(true);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    startX = e.clientX;
    startY = e.clientY;
    holding = true;
    setHint("");
    const id = ++holdId; // the mic may still be opening for an earlier press
    clearTimeout(holdTimer);
    holdTimer = window.setTimeout(async () => {
      holdTimer = 0;
      vib(30); // before the mic opens: Android mutes haptics while audio is being recorded
      try {
        const r = await record(video());
        if (!holding || id !== holdId) return r.cancel();
        setRec(r);
        setElapsed(0);
        red = false;
        if (video()) preview.srcObject = r.stream;
        meter(r.stream);
        tick = window.setInterval(() => {
          setElapsed(Date.now() - r.started);
          if (!video()) invoke("typing", { chat: p.chat, voice: true }).catch(() => {});
          if (video() && Date.now() - r.started >= 60_000) stopRec(true);
        }, 100);
      } catch (e) {
        setErr("Нет доступа к микрофону или камере: " + errText(e));
      }
    }, 200);
  };
  const up = () => {
    if (!holding) return; // already cancelled or locked while sliding
    // Released before recording was due: a tap, switches voice ↔ video.
    if (holdTimer) setVideo(!video());
    const r = rec();
    if (!r || Date.now() - r.started < 500) {
      setHint(`Удерживайте для записи ${video() ? "видео" : "голосового"}`);
      clearTimeout(hintTimer);
      hintTimer = window.setTimeout(() => setHint(""), 2000);
    }
    void stopRec(true); // sends; no recording or a too-short one is dropped
  };
  // The system took the pointer away (also fires after a normal release, when `holding` is already false).
  const abort = () => {
    if (holding) void stopRec(false);
  };
  onCleanup(() => (clearTimeout(hintTimer), void stopRec(false))); // leaving the chat frees the mic
  const move = (e: PointerEvent) => {
    if (!holding || !rec()) return;
    const dx = Math.min(0, e.clientX - startX), dy = Math.max(0, startY - e.clientY);
    if (dy > 80) {
      holding = false; // from now on the release is ignored
      setLocked(true);
      setSlide(0);
      setLift(0);
      vib(30);
    } else if (dx < -120) {
      holding = false;
      stopRec(false);
    } else {
      // One tick when the finger enters the cancel zone (the bar turns red), none while inside.
      const r = dx < -60;
      if (r !== red && (red = r)) vib(20);
      setSlide(dx);
      setLift(Math.min(dy, 80));
    }
  };
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
    seen = all.length;
    // Instant: a smooth scroll still running when the next message lands would read as "scrolled up".
    queueMicrotask(() => (stick ? toBottom(false) : setFresh((n) => n + added)));
  });
  const onScroll = () => {
    // Date pills all stick to the same spot: hide each one the next has reached.
    const days = list.querySelectorAll<HTMLElement>(".day");
    days.forEach((d, i) =>
      (d.style.visibility = i + 1 < days.length && days[i + 1].offsetTop - d.offsetTop < d.offsetHeight ? "hidden" : ""));
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
    setText(ed ? getDraft(key) : "");
    setEditing(undefined);
    setReply(undefined);
    queueMicrotask(grow);
    if (ed) return t !== ed.text && act(invoke("edit", { id: ed.id, text: t }));
    const mentions = p.chat ? [] : mentionIds(t, people());
    queue({ text: t, reply: r ? { id: r.id, author: r.author, text: plain(r) } : null },
      () => invoke<string>("send", { chat: p.chat, text: t, replyTo: r?.id ?? null, mentions }),
      // The chat may be closed by now: keep the unsent text as its draft unless a newer one exists.
      () => (getDraft(key) || setDraft(key, t), setText(t), setReply(r), queueMicrotask(grow)));
  };
  // One tap sends; the typed text stays in the field.
  const sendSticker = (sticker: string, emoji: string) => {
    const r = reply();
    setEmo(undefined);
    setReply(undefined);
    queue({ text: emoji, sticker, reply: r ? { id: r.id, author: r.author, text: plain(r) } : null },
      () => invoke<string>("send", { chat: p.chat, text: emoji, replyTo: r?.id ?? null, sticker }),
      () => reply() || setReply(r));
  };
  // While editing, the field holds the old message, not the draft.
  createEffect(() => {
    const t = text();
    if (!editing()) setDraft(key, t);
  });
  const mention = useMention({
    people: () => (p.chat ? [] : people()), text, input: () => input, avatar: (id) => <Avatar id={id} />,
    insert: (t, caret) => (setText(t), (input.value = t), input.focus(), input.setSelectionRange(caret, caret), grow()),
  });
  const canAll = (ids: string[]) => p.admin || ids.every((id) => p.msgs.find((m) => m.id === id)?.mine);
  return (
    <section class="chat">
      <Show when={sel().length} fallback={<header class="bar">
        <button class="icon back" onClick={p.back} title="Назад"><ArrowLeft size={20} /></button>
        {p.chat ? <Avatar id={p.chat} dot onClick={p.onProfile} /> : <div class="avatar group small-av"><Users size={18} /></div>}
        <div class="grow ellipsis" classList={{ link: !!p.chat }} onClick={() => p.chat && p.onProfile()}>
          <b>{p.chat ? nameOf(p.chat) : groupName()}</b>
          <div class="muted small ellipsis" classList={{ typing: !!typingText(p.chat), online: !!memberOf(p.chat)?.online }} title={p.chat ? linkText(memberOf(p.chat)) : ""}>
            {typingText(p.chat) || (p.chat ? presence(memberOf(p.chat)) : `${p.members} участн.`)}
          </div>
        </div>
        <button class="icon" disabled={p.inCall || p.members < 2} onClick={() => p.onCall(false)}
          title={p.chat ? "Звонок" : "Групповой звонок"}><Phone size={20} /></button>
        <button class="icon" onClick={() => (setTtlPick(false), setMore(true), invoke<number>("timer", { chat: p.chat }).then(setTtl, () => {}))}
          title="Ещё"><EllipsisVertical size={20} /></button>
        <Show when={more()}>
          <div class="overlay" onClick={() => setMore(false)}>
            <div class="menu more">
              <Show when={!ttlPick()} fallback={
                <For each={TTLS}>
                  {([secs, l]) => <button onClick={() => secs !== ttl() && act(invoke("set_timer", { chat: p.chat, secs }))}>
                    <Timer size={18} /> <span class="grow">{l}</span>{secs === ttl() && <Check size={16} />}</button>}
                </For>
              }>
              <button disabled={p.inCall || p.members < 2} onClick={() => p.onCall(true)}><Video size={18} /> {p.chat ? "Видеозвонок" : "Групповой видеозвонок"}</button>
              <button onClick={() => setQuery(query() === undefined ? "" : undefined)}><Search size={18} /> Поиск</button>
              <button onClick={() => p.onMute(!p.muted)}>{p.muted ? <><Bell size={18} /> Включить уведомления</> : <><BellOff size={18} /> Без звука</>}</button>
              <button onClick={(e) => (e.stopPropagation(), setTtlPick(true))}>
                <Timer size={18} /> <span class="grow">Автоудаление</span><span class="muted">{TTLS.find(([s]) => s === ttl())?.[1] ?? `${ttl()} с`}</span></button>
              <Show when={p.chat}>
                <button onClick={p.onProfile}><Users size={18} /> Профиль</button>
                <button class="del" onClick={p.onDelete}><Trash2 size={18} /> Удалить чат</button>
              </Show>
              <div class="muted small more-net"><NetDot n={p.peers} /><Show when={p.chat}><div>{linkText(memberOf(p.chat))}</div></Show></div>
              </Show>
            </div>
          </div>
        </Show>
      </header>}>
        <header class="bar selbar">
          <button class="icon" onClick={() => setSel([])} title="Отмена (Esc)"><X size={20} /></button>
          <b class="grow">{sel().length} выбрано</b>
          <button onClick={() => (copy(picked()), setSel([]))}><Copy size={16} /> Копировать</button>
          <button onClick={() => setDlg({ kind: "forward", ids: picked().map((m) => m.id) })}><Forward size={16} /> Переслать</button>
          <button class="del" onClick={() => setDlg({ kind: "delete", ids: picked().map((m) => m.id) })}><Trash2 size={16} /> Удалить</button>
        </header>
      </Show>
      <Show when={query() !== undefined}>
        <div class="searchbar">
          <input ref={(el) => queueMicrotask(() => el.focus())} placeholder="Поиск в чате" value={query()}
            onInput={(e) => { setQuery(e.currentTarget.value); setCur(""); goHit(0); }}
            onKeyDown={(e) => e.key === "Enter" && !e.isComposing && goHit(e.shiftKey ? 1 : -1)} />
          <Show when={re()}>
            <small class="muted">{hits().length ? `${hits().indexOf(hit()!) + 1} из ${hits().length}` : "Не найдено"}</small>
          </Show>
          <button class="icon" disabled={!hits().length} onClick={() => goHit(-1)} title="Раньше (Enter)"><ChevronUp size={20} /></button>
          <button class="icon" disabled={!hits().length} onClick={() => goHit(1)} title="Позже (Shift+Enter)"><ChevronDown size={20} /></button>
          <button class="icon" onClick={() => setQuery(undefined)} title="Закрыть поиск (Esc)"><X size={20} /></button>
        </div>
      </Show>
      <Show when={pinShown()}>
        {(m) => (
          <div class="pinbar" onClick={() => (jump(m().id), setPinIx((i) => i + 1))} title="К сообщению">
            <Pin size={16} />
            <div class="grow">
              <b>Закреплённое{pinned().length > 1 ? ` · ${(pinIx() % pinned().length) + 1} из ${pinned().length}` : ""}</b>
              <div class="ellipsis small">{rich(plain(m()))}</div>
            </div>
            <button class="icon" title="Открепить" onClick={(e) => (e.stopPropagation(), act(invoke("pin", { id: m().id, pinned: false })))}><X size={16} /></button>
          </div>
        )}
      </Show>
      <div class="msgs-wrap">
        <div class="msgs" ref={list} onScroll={onScroll} classList={{ selecting: !!sel().length }}>
          <Show when={!shown().length}><p class="empty muted">Здесь пока тихо. Напишите первым!</p></Show>
          <For each={grouped().list}>
            {(m, i) => {
              const prev = () => grouped().list[i() - 1], next = () => grouped().list[i() + 1];
              const items = () => itemsOf(m), head = () => headOf(m);
              const status = () => Math.min(...items().map((x) => x.status));
              // 1–3 emoji and nothing else: large, without a bubble.
              const big = () => (items().length === 1 && !m.media && !m.reply && !m.fwd ? emojiOnly(head().text ?? "") : 0);
              return (
                <>
                  <Show when={!prev() || dayKey(prev()!.ts) !== dayKey(m.ts)}><div class="day"><span>{dayLabel(m.ts)}</span></div></Show>
                  {m.service ? <div class="call-line"><Timer size={14} />{serviceText(m)}</div> : m.call ? (
                    <div class="call-line" classList={{ missed: m.call.missed }}>
                      {m.call.missed ? <PhoneMissed size={14} /> : m.call.video ? <Video size={14} /> : <Phone size={14} />}
                      {m.mine || p.chat ? "" : `${nameOf(m.author)} · `}{callLabel(m.call)} · {time(m.ts)}
                      <Show when={m.call.active && !p.inCall}><button class="link" onClick={() => p.onJoin(m.call!)}>Присоединиться</button></Show>
                    </div>
                  ) : (
                    <div class="msg" data-id={items().map((x) => x.id).join(" ")}
                      classList={{ mine: m.mine, cont: same(prev(), m), tail: !same(m, next()), sel: sel().includes(m.id), found: items().some((x) => x.id === hit()),
                        // Photo/video alone: edge to edge with the time over it; a video note has no bubble at all.
                        bare: items().length === 1 && !!m.media && (m.media.kind === "photo" || m.media.kind === "video") && !m.text && !m.reply && !m.fwd && !m.reactions.length,
                        note: m.media?.kind === "videonote" && !m.text && !m.reply, big: !!big() || !!m.sticker, sticker: !!m.sticker }}
                      onClick={(e) => clickMsg(e, m)} onMouseDown={(e) => (e.shiftKey || e.detail > 1) && e.preventDefault()} onContextMenu={(e) => openMenu(e, m)}
                      onPointerDown={(e) => press(e, m)} onPointerMove={(e) => swipeMove(e, m)} onPointerUp={(e) => unpress(e, m)} onPointerCancel={(e) => unpress(e, m)}
                      // Lifting the finger after a hold must not click the menu's backdrop and close it.
                      onTouchEnd={(e) => held && e.preventDefault()}
                      style={swipe()?.id === m.id ? { transform: `translateX(${swipe()!.dx}px)`, transition: "none" } : {}}>
                      <Show when={swipe()?.id === m.id}>
                        <span class="swipe-reply" style={{ opacity: Math.min(1, swipe()!.dx / 60) }}><Reply size={18} /></span>
                      </Show>
                      <button class="pick" title="Выделить" onClick={(e) => (e.stopPropagation(), toggle(m))}>
                        <Show when={sel().includes(m.id)}><Check size={14} /></Show>
                      </button>
                      <Show when={!m.mine && !p.chat}>
                        <Show when={!same(m, next())} fallback={<div class="av-gap" />}>
                          <Avatar id={m.author} onClick={() => p.openChat(m.author)} />
                        </Show>
                      </Show>
                      <div class="bubble">
                        <Show when={!m.mine && !p.chat && !same(prev(), m)}>
                          <div class="author link" style={{ color: `hsl(${colorOf(m.author)} 50% 50%)` }} title="Написать лично"
                            onClick={(e) => (e.stopPropagation(), p.openChat(m.author))}>{nameOf(m.author)}</div>
                        </Show>
                        <Show when={m.fwd}><div class="fwd">Переслано от <b>{m.fwd}</b></div></Show>
                        <Show when={m.reply}>
                          {(r) => (
                            <div class="quote" onClick={(e) => (e.stopPropagation(), jump(r().id))}>
                              <Show when={r().author}><b>{name(r().author)}</b></Show>
                              <span>{rich(r().text)}</span>
                            </div>
                          )}
                        </Show>
                        <Show when={items().length > 1} fallback={
                          <Show when={m.media}><MediaView id={m.id} m={m.media!} onView={() => openView(m.id)} /></Show>
                        }>
                          <Album items={items()} onView={openView} />
                        </Show>
                        <Show when={head().sticker}>{(id) => <Sticker id={id()} emoji={head().text} />}</Show>
                        <Show when={!head().sticker && head().text}>
                          <span class="text" style={big() ? { "font-size": `${112 - 16 * big()}px` } : undefined}>
                            {re() ? head().text.split(re()!).map((s, i) => (i % 2 ? <mark>{rich(s)}</mark> : rich(s))) : p.chat ? rich(head().text) : mentionText(head().text, people(), p.onMention)}
                          </span>
                        </Show>
                        <Show when={joinLink(head().text ?? "")}><JoinGroup text={head().text} mine={m.mine} /></Show>
                        <Show when={reactionsOf(m).length}>
                          <div class="reacts">
                            <For each={reactionsOf(m)}>
                              {([e, { who, id }]) => (
                                <button classList={{ on: who.includes(p.me) }} title={who.map(name).join(", ")}
                                  onClick={(ev) => (ev.stopPropagation(), act(invoke("react", { id, emoji: e })))}>
                                  {rich(e)} {who.length}
                                </button>
                              )}
                            </For>
                          </div>
                        </Show>
                        <span class="meta">
                          <Show when={m.ttl}><span title="Автоудаление"><Timer size={11} /></span></Show>
                          <Show when={items().some((x) => x.pinned)}><Pin size={11} /></Show>
                          <Show when={items().some((x) => x.edited)}><span>изменено</span></Show>
                          {time(m.ts)}
                          <Show when={m.mine}>
                            <Switch>
                              <Match when={status() === 0}><span title="Отправляется"><Clock size={14} /></span></Match>
                              <Match when={status() === 1}><span title="Отправлено"><Check size={14} /></span></Match>
                              <Match when={status() === 2}><span title="Прочитано"><CheckCheck size={14} /></span></Match>
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
        {(mn) => {
          const head = () => headOf(mn().m), ids = () => itemsOf(mn().m).map((x) => x.id);
          const pinnedOne = () => itemsOf(mn().m).find((x) => x.pinned);
          return (
          <div class="overlay" onClick={() => setMenu(undefined)} onContextMenu={(e) => (e.preventDefault(), setMenu(undefined))}>
            <div class="menu" style={{ left: `${Math.min(mn().x, innerWidth - 210)}px`, top: `${Math.min(mn().y, innerHeight - 350)}px` }}>
              <div class="emojis">
                <For each={["👍", "❤️", "😂", "😮", "😢", "🔥"]}>
                  {(e) => <button onClick={() => act(invoke("react", { id: head().id, emoji: e }))}><Emoji e={e} /></button>}
                </For>
              </div>
              <button onClick={() => startReply(head())}><Reply size={16} /> Ответить</button>
              <Show when={plain(head())}><button onClick={() => copy([head()])}><Copy size={16} /> Копировать</button></Show>
              <Show when={mn().m.mine && !head().sticker}><button onClick={() => startEdit(head())}><Pencil size={16} /> Редактировать</button></Show>
              <button onClick={() => act(invoke("pin", { id: (pinnedOne() ?? head()).id, pinned: !pinnedOne() }))}>
                {pinnedOne() ? <PinOff size={16} /> : <Pin size={16} />} {pinnedOne() ? "Открепить" : "Закрепить"}
              </button>
              <button onClick={() => setDlg({ kind: "forward", ids: ids() })}><Forward size={16} /> Переслать</button>
              <button onClick={() => setSel(ids())}><CircleCheck size={16} /> Выделить</button>
              <button class="del" onClick={() => setDlg({ kind: "delete", ids: ids() })}><Trash2 size={16} /> Удалить</button>
            </div>
          </div>
          );
        }}
      </Show>
      <Show when={dlg()}>
        {(d) => (
          <div class="modal" onClick={() => setDlg(undefined)}>
            <div class="card dialog" onClick={(e) => e.stopPropagation()}>
              <Show when={d().kind === "delete"} fallback={
                <>
                  <h3>Переслать в чат</h3>
                  <ForwardList current={spaceId} avatar={(id) => <Avatar id={id} />}
                    people={people().filter((m) => !m.me && !m.removed).map((m) => ({ id: m.id, name: nameOf(m.id) }))}
                    onPick={(to, name, space) => {
                      const ids = d().ids;
                      setDlg(undefined);
                      setSel([]);
                      if (!space) return act(invoke("forward", { ids, to }));
                      // Another space is opened for this: it takes a few seconds.
                      setHint(`Пересылаю в «${name}»…`);
                      act(invoke("forward_to_space", { ids, space, to }).then(() => setHint(`Переслано в «${name}»`))
                        .finally(() => (clearTimeout(hintTimer), (hintTimer = window.setTimeout(() => setHint(""), 2000)))));
                    }} />
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
      <Show when={toSend().length}>
        <div class="modal" onClick={() => setToSend([])}>
          <div class="card dialog" onClick={(e) => e.stopPropagation()}>
            <h3>{toSend().length > 1 ? `Отправить файлы: ${toSend().length}` : "Отправить файл"}</h3>
            <div class="send-previews">
              <For each={toSend()}>
                {(f) => isPhoto(f)
                  ? <img src={convertFileSrc(f)} alt="" />
                  : isVideo(f)
                  ? <video src={`${convertFileSrc(f)}#t=0.1`} preload="metadata" muted playsinline />
                  : <div class="file-chip"><Paperclip size={16} /> <span class="ellipsis">{baseName(f)}</span></div>}
              </For>
            </div>
            <Show when={toSend().some(isPhoto)}>
              <label class="check">
                <input type="checkbox" checked={compress()} onChange={(e) => {
                  setCompress(e.currentTarget.checked);
                  localStorage.setItem("compress", e.currentTarget.checked ? "1" : "0");
                }} />
                Сжать фото
              </label>
              <p class="muted small">{compress()
                ? "Отправится быстрее и займёт меньше места. Место съёмки и данные камеры будут удалены."
                : "Фото уйдёт файлом в исходном качестве, вместе с метаданными (место и время съёмки, камера)."}</p>
            </Show>
            <input placeholder="Подпись" aria-label="Подпись" value={caption()} onInput={(e) => setCaption(e.currentTarget.value)}
              onKeyDown={(e) => e.key === "Enter" && !e.isComposing && confirmSend()} />
            <button class="primary" onClick={confirmSend}><Send size={16} /> Отправить</button>
            <button onClick={() => setToSend([])}>Отмена</button>
          </div>
        </div>
      </Show>
      <Show when={drag()}><div class="drop">Отпустите, чтобы отправить</div></Show>
      <Show when={view()}><Viewer shots={shots()} at={view()} onAt={setView} onClose={() => setView("")} name={name} /></Show>
      <Show when={rec() && video()}><video ref={preview} class="circle live" autoplay muted playsinline /></Show>
      <Show when={editing() || reply()}>
        <div class="replybar">
          {editing() ? <Pencil size={18} /> : <Reply size={18} />}
          <div class="grow" onClick={() => jump((editing() ?? reply())!.id)}>
            <b>{editing() ? "Редактирование" : `Ответ: ${name(reply()!.author)}`}</b>
            <div class="ellipsis small">{rich(plain((editing() ?? reply())!))}</div>
          </div>
          <button class="icon" onClick={cancelCompose} title="Отмена (Esc)"><X size={18} /></button>
        </div>
      </Show>
      <input ref={picker} type="file" multiple hidden onChange={pick} />
      <div class="composer">
        <button class="icon" onClick={attach} title="Прикрепить файл (или перетащите в окно)"><Paperclip size={22} /></button>
        <Show when={rec() && !locked()}>
          <div class="lock-tip" style={{ transform: `translate(${slide()}px, ${-lift()}px)` }}><LockIcon size={18} /><ChevronUp size={16} /></div>
        </Show>
        <Show when={hint()}><div class="rec-hint" role="status">{hint()}</div></Show>
        {/* While recording the field stays in place (hidden): the bar keeps its height. */}
        <div class="field-wrap" classList={{ rec: !!rec() }}>
          <Show when={rec()}>
            <div class="recording" classList={{ cancel: slide() < -60 }}>
              <i class="dot" /> {Math.floor(elapsed() / 60000)}:{String(Math.floor(elapsed() / 1000) % 60).padStart(2, "0")},{Math.floor(elapsed() / 100) % 10}
              <Show when={locked()} fallback={
                <span class="muted small grow" style={{ opacity: 1 - Math.min(1, -slide() / 120) }}>‹ Влево — отмена</span>
              }>
                <span class="grow" />
                <button class="link" onClick={() => stopRec(false)}>Отмена</button>
              </Show>
            </div>
          </Show>
          <button class="icon" classList={{ on: emo() !== undefined }} title="Смайлики"
            onClick={() => setEmo(emo() !== undefined ? undefined : recentEmoji().length ? -1 : 0)}><Smile size={22} /></button>
        <textarea ref={input} rows={1} placeholder={video() ? "Сообщение · удерживайте 📹 для видеосообщения" : "Сообщение"} value={text()}
          onInput={(e) => {
            setText(e.currentTarget.value);
            mention.track(e);
            grow();
            // The core sends at most once per 3 s.
            if (text().trim() && !editing()) invoke("typing", { chat: p.chat, voice: false }).catch(() => {});
          }}
          onClick={mention.track} onKeyUp={mention.track}
          onKeyDown={(e) => {
            if (mention.key(e)) return;
            if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
              e.preventDefault();
              send();
            }
          }} />
        </div>
        {mention.view()}
        <Show when={emo() !== undefined}>
          <div class="overlay" onClick={() => setEmo(undefined)} />
          <div class="emoji-panel card">
            <Picker send={sendSticker}>
              <div class="emoji-tabs">
                <Show when={recentEmoji().length}>
                  <button classList={{ active: emo() === -1 }} onClick={() => setEmo(-1)} title="Недавние"><Clock size={18} /></button>
                </Show>
                <For each={EMOJI}>{([icon], i) => <button classList={{ active: emo() === i() }} onClick={() => setEmo(i())}><Emoji e={icon} /></button>}</For>
              </div>
              <div class="emoji-grid">
                <For each={emo() === -1 ? recentEmoji() : EMOJI[emo()!][1].split(" ")}>
                  {(e) => <button onClick={() => (insertEmoji(e), input.focus())}><Emoji e={e} /></button>}
                </For>
              </div>
            </Picker>
          </div>
        </Show>
        <Show when={!rec() && (text().trim() || editing())} fallback={
          <button class="icon mic" classList={{ live: !!rec() }}
            style={rec() ? { transform: `translateX(${slide()}px)` } : undefined}
            onPointerDown={down} onPointerUp={up} onPointerMove={move} onPointerCancel={abort} onLostPointerCapture={(e) => e.target === e.currentTarget && abort()}
            onContextMenu={(e) => e.preventDefault()}
            title={video() ? "Удерживайте — видеосообщение; нажмите — голосовое" : "Удерживайте — голосовое; нажмите — видеосообщение"}>
            <i class="halo" ref={halo} />
            {locked() ? <Send size={rec() ? 28 : 22} /> : video() ? <Video size={rec() ? 28 : 22} /> : <Mic size={rec() ? 28 : 22} />}
          </button>
        }>
          <button class="icon send" disabled={!text().trim()} onClick={send} title={editing() ? "Сохранить" : "Отправить"}>
            {editing() ? <Check size={22} /> : <Send size={22} />}
          </button>
        </Show>
      </div>
    </section>
  );
}

function Calls(p: { calls: CallView[]; onCall: (video: boolean, to: string) => void; inCall: boolean }) {
  return (
    <div class="scroll">
      <Show when={!p.calls.length}><p class="empty muted">Звонков пока не было</p></Show>
      <For each={p.calls}>
        {(c) => {
          // Who the call was with: the partner of a 1:1 chat, else the offerer (group call).
          const who = () => c.chat || c.author;
          return (
            <div class="row">
              {c.chat || !c.mine ? <Avatar id={who()} /> : <div class="avatar group"><Hash size={20} /></div>}
              <div class="grow">
                <b class="ellipsis">{c.chat ? nameOf(c.chat) : c.mine ? groupName() : `${nameOf(c.author)} · ${groupName()}`}</b>
                <div class="muted small ellipsis" classList={{ error: c.missed && !c.mine }}>
                  {c.mine ? "↗ " : "↙ "}{callLabel(c)} · {when(c.ts) === time(c.ts) ? time(c.ts) : `${when(c.ts)}, ${time(c.ts)}`}
                </div>
              </div>
              <button class="icon" disabled={p.inCall} onClick={() => p.onCall(c.video, c.chat)} title="Позвонить снова">
                {c.video ? <Video size={20} /> : <Phone size={20} />}
              </button>
            </div>
          );
        }}
      </For>
    </div>
  );
}

function Incoming(p: { c: CallSignal; onAccept: () => void; onReject: () => void }) {
  onCleanup(ring("in"));
  return (
    <div class="call-screen incoming">
      <Avatar id={p.c.author} />
      <h2>{nameOf(p.c.author)}</h2>
      <p class="muted">{p.c.video ? "Входящий видеозвонок" : "Входящий звонок"}{p.c.chat ? "" : ` · ${groupName()}`}</p>
      <div class="call-buttons">
        <button class="round hang" onClick={p.onReject} title="Отклонить"><PhoneOff size={26} /></button>
        <button class="round accept" onClick={p.onAccept} title="Принять">{p.c.video ? <Video size={26} /> : <Phone size={26} />}</button>
      </div>
    </div>
  );
}

/** Mounted while the call is full screen: Back (and the arrow) folds it into the bar. */
function CallTop(p: { onMini: () => void; label: string }) {
  const close = useBack(() => p.onMini());
  return (
    <div class="call-top">
      <button class="icon" onClick={close} title="Свернуть"><ChevronDown size={26} /></button>
      <span class="muted">{p.label}</span>
    </div>
  );
}

function CallScreen(p: { a: Active; tiles: Peer[]; media: CallMedia; onEnd: () => void }) {
  const [mini, setMini] = createSignal(false);
  const [now, setNow] = createSignal(Date.now());
  const [mic, setMic] = createSignal(p.media.mic);
  const [cam, setCam] = createSignal(p.media.cam);
  const [gear, setGear] = createSignal(false);
  const [camErr, setCamErr] = createSignal(p.media.camErr);
  const [front, setFront] = createSignal(true);
  const [screen, setScreen] = createSignal(p.media.screen);
  const [adding, setAdding] = createSignal(false);
  const [invited, setInvited] = createSignal<string[]>([]);
  const [addErr, setAddErr] = createSignal("");
  p.media.onScreenEnd = () => setScreen(false);
  const toggleScreen = async () => {
    setCamErr("");
    try {
      await p.media.setScreen(!screen());
      setScreen(!screen());
    } catch (e) {
      // Closing the picker is not an error.
      if ((e as Error)?.name !== "NotAllowedError") setCamErr(`Демонстрация экрана: ${e}`);
    }
  };
  const invite = async (id: string) => {
    setAddErr("");
    try {
      await invoke("call_invite", { chat: id });
      setInvited([...invited(), id]);
    } catch (e) {
      setAddErr(String(e));
    }
  };
  const t = setInterval(() => setNow(Date.now()), 250);
  onCleanup(() => clearInterval(t));
  // Ringback until someone answers.
  createEffect(() => {
    if (!p.a.started) onCleanup(ring("out"));
  });
  const toggleMic = () => (p.media.setMic(!mic()), setMic(!mic()));
  const dur = () => (p.a.started ? clock(now() - p.a.started) : "Вызов…");
  const toggleCam = async () => {
    setCamErr("");
    try {
      await p.media.setCam(!cam());
      setCam(!cam());
    } catch (e) {
      setCamErr(camError(e));
    }
  };
  const flip = async () => {
    setCamErr("");
    await p.media.flipCam().catch((e) => setCamErr(camError(e)));
    setFront(p.media.facing === "user");
  };
  return (
    <Show when={!mini()} fallback={
      <div class="callbar">
        <button class="icon" onClick={toggleMic} title={mic() ? "Выключить микрофон" : "Включить микрофон"}>{mic() ? <Mic size={20} /> : <MicOff size={20} />}</button>
        <button class="callbar-info" onClick={() => (setGear(false), setMini(false))} title="Развернуть звонок">
          <b class="ellipsis">{p.a.chat ? nameOf(p.a.chat) : `${groupName()} · ${p.tiles.length + 1}`}</b>
          <span>{dur()}</span>
        </button>
        <button class="icon hang" onClick={p.onEnd} title="Завершить"><PhoneOff size={20} /></button>
      </div>
    }>
    <div class="call-screen">
      <CallTop onMini={() => setMini(true)} label={dur()} />
      <div class="call-grid">
        <For each={p.tiles}>
          {(t) => (
            <div class="tile" classList={{ talking: now() - t.talkAt < 400 }}>
              {/* Video shows while frames come; otherwise the avatar. */}
              <div style={{ display: now() - t.videoAt < 1500 ? "contents" : "none" }}>{t.canvas}</div>
              <Show when={now() - t.videoAt >= 1500}><Avatar id={t.id} /></Show>
              <span class="tile-name">{nameOf(t.id)} <Signal net={t.net()} /></span>
            </div>
          )}
        </For>
        <Show when={!p.tiles.length}>
          <div class="tile"><span class="muted">{p.a.started ? "Соединяемся…" : "Ждём ответа"}</span></div>
        </Show>
      </div>
      <Show when={cam() || screen()}>
        <video class="self" classList={{ back: !front() }} autoplay muted playsinline ref={(v) => (v.srcObject = p.media.local)} />
      </Show>
      <Show when={camErr()}><p class="error call-err">{camErr()}</p></Show>
      <div class="call-buttons">
        <button class="round" classList={{ off: !mic() }} onClick={toggleMic}
          title={mic() ? "Выключить микрофон" : "Включить микрофон"}>{mic() ? <Mic size={24} /> : <MicOff size={24} />}</button>
        <button class="round" classList={{ off: !cam() }} onClick={toggleCam}
          title={cam() ? "Выключить камеру" : "Включить камеру"}>{cam() ? <Video size={24} /> : <VideoOff size={24} />}</button>
        <Show when={android && cam()}><button class="round" onClick={flip} title="Сменить камеру"><SwitchCamera size={24} /></button></Show>
        <Show when={!android}>
          <button class="round" classList={{ off: screen() }} onClick={toggleScreen}
            title={screen() ? "Остановить демонстрацию" : "Демонстрация экрана или окна"}>{screen() ? <ScreenShareOff size={24} /> : <ScreenShare size={24} />}</button>
        </Show>
        <button class="round" onClick={() => (setAddErr(""), setAdding(true))} title="Добавить в звонок"><UserPlus size={24} /></button>
        <button class="round" classList={{ off: gear() }} onClick={() => setGear(!gear())} title="Голос и видео"><Settings size={24} /></button>
        <button class="round hang" onClick={p.onEnd} title="Завершить"><PhoneOff size={24} /></button>
      </div>
      <Show when={gear()}>
        <div class="call-settings scroll pad"><VoiceSettings /></div>
      </Show>
      <Show when={adding()}>
        <div class="modal" onClick={() => setAdding(false)}>
          <div class="card dialog" onClick={(e) => e.stopPropagation()}>
            <h3>Добавить в звонок</h3>
            <div class="dialog-list">
              <For each={people().filter((m) => !m.me && !m.removed)} fallback={<p class="muted">Некого добавить</p>}>
                {(m) => (
                  <button class="row chat-row" disabled={invited().includes(m.id)} onClick={() => invite(m.id)}>
                    <Avatar id={m.id} /><b class="ellipsis">{nameOf(m.id)}</b>
                    <Show when={invited().includes(m.id)}><span class="muted">Звоним…</span></Show>
                  </button>
                )}
              </For>
            </div>
            <Show when={addErr()}><p class="error">{addErr()}</p></Show>
            <button onClick={() => setAdding(false)}>Готово</button>
          </div>
        </div>
      </Show>
    </div>
    </Show>
  );
}
