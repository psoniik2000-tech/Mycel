import { createEffect, createMemo, createSignal, For, Match, onCleanup, onMount, Show, Switch, untrack } from "solid-js";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ask, message, open } from "@tauri-apps/plugin-dialog";
import { getCurrent, onOpenUrl } from "@tauri-apps/plugin-deep-link";
import { isPermissionGranted, requestPermission } from "@tauri-apps/plugin-notification";
import {
  ArrowLeft, Bell, BellOff, Camera, Check, CheckCheck, ChevronDown, ChevronUp, CircleCheck, Clock, Copy, Forward, Hash, Link, Lock as LockIcon, LogOut, MessageCircle, Mic, MicOff, Paperclip, Pencil, Phone, PhoneMissed, PhoneOff, Pin, PinOff, Plus, Reply, Search, Send,
  Settings, ShieldAlert, Smile, ShieldCheck, Trash2, UserPlus, Users, Video, VideoOff, X,
} from "lucide-solid";
import logo from "../app-icon.svg";
import { label, Media, MediaView } from "./media";
import { EMOJI, recentEmoji, useEmoji } from "./emoji";
import { record, Recording, waveform } from "./capture";
import { CallMedia, Peer, ring } from "./call";
import { Signal, VoiceSettings } from "./voice";
import { mode, ModeChoice, ModeSettings, setMode, simple, SIMPLE_NAME } from "./simple";

// status (own messages, §7.4): 0 ⏱ sending, 1 ✓ some member has it, 2 ✓✓ every member has it.
type Msg = { id: string; author: string; mine: boolean; text: string; ts: number; status: number; media: Media | null; call: CallView | null;
  reply: { id: string; author: string; text: string } | null; edited: boolean; pinned: boolean;
  reactions: [string, string[]][] };
type CallView = { id: string; video: boolean; author: string; chat: string; mine: boolean; ts: number; duration_ms: number | null; missed: boolean; active: boolean };
// chat: "" = the space chat (group call), else the 1:1 partner's id.
type CallSignal = { id: string; action: "offer" | "accept" | "reject" | "end"; video: boolean; author: string; chat: string; ts: number };
type Active = { id: string; video: boolean; started: number; peers: string[]; chat: string };
type Member = { id: string; me: boolean; admin: boolean; name: string | null; avatar: string; color: number | null; link: "direct" | "relay" | null; seen: number | null };
type ChatRow = { id: string; last: Msg | null; unread: number; muted: boolean };
type Space = { id: string; name: string; remembered: boolean };
type Status = { spaces: Space[]; last: string | null; current: string | null; dir: string; in_space: boolean };
type Tab = "chats" | "calls" | "people" | "settings";
// What the user came to do, picked before the password (unknown: Onboard offers both).
type How = "create" | "join";

const time = (ts: number) => new Date(ts).toLocaleTimeString("ru", { hour: "2-digit", minute: "2-digit" });
const hue = (id: string) => parseInt(id.slice(0, 4), 16) % 360;
// Avatar colors a member can pick (profile `color` is an index here).
const PALETTE = [210, 0, 25, 45, 130, 175, 265, 320];
// Members with their profiles, for names and avatars everywhere.
const [people, setPeople] = createSignal<Member[]>([]);
const memberOf = (id: string) => people().find((m) => m.id === id);
const nameOf = (id: string) => memberOf(id)?.name ?? `Участник ${id}`;
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
const n0 = () => localStorage.getItem("n0") !== "0";
// This PC is an anchor: its relay URL (Settings → Якорь).
const anchor = () => localStorage.getItem("anchor") || null;
// Backup public relays (n0's test servers): used besides the main n0 ones; editable in Settings → Связь.
const DEFAULT_RELAYS = ["https://euc1-1.staging-relay.n0.iroh.link", "https://use1-1.staging-relay.n0.iroh.link"];
const relays = (): string[] => {
  try {
    const v = JSON.parse(localStorage.getItem("relays") ?? "null");
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
  invoke("unlock", { id, name: "", password: null, remember: true, n0: n0(), anchor: anchor(), relays: relays(), mailbox: mailbox() });
const autoAnchor = () => !android && localStorage.getItem("anchor_auto") !== "0";
/** Turns this PC's anchor on (relay at the external IP, port 3340) or off; reopens the space. Returns the URL. */
async function setAnchor(id: string, on: boolean): Promise<string> {
  // shortcut: fixed port 3340; a port field when someone needs another one.
  const u = on ? `http://${await invoke<string>("public_ip")}:3340` : "";
  await invoke("unlock", { id, name: "", password: null, remember: true, n0: n0(), anchor: u || null, relays: relays(), mailbox: mailbox() });
  u ? localStorage.setItem("anchor", u) : localStorage.removeItem("anchor");
  return u;
}
/** The router lets us open the port: become an anchor without asking (Settings → Якорь turns it off). */
async function tryAutoAnchor(id: string) {
  if (!autoAnchor() || anchor() || !(await invoke<boolean>("upnp_ok").catch(() => false))) return;
  try {
    await setAnchor(id, true);
    const [, mapped] = await invoke<[boolean, boolean]>("anchor_check");
    if (!mapped) await setAnchor(id, false);
  } catch {
    // stays a plain member
  }
}
// The open space.
let spaceId = "";
// Unsent text per space and chat, kept on this device (storage may be unavailable).
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

/** The connection to a member in words, for the 1:1 chat header. */
const linkText = (m?: Member) => {
  if (!m || m.me) return "личный чат";
  if (m.link) return m.link === "direct" ? "на связи напрямую" : "на связи через сервер-ретранслятор";
  if (!m.seen) return "не в сети, сообщения дойдут через почтовый ящик";
  const min = Math.round((Date.now() - m.seen) / 60_000);
  return `был(а) в сети ${min < 1 ? "только что" : min < 60 ? `${min} мин назад` : new Date(m.seen).toLocaleString("ru", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })}`;
};

function Avatar(p: { id: string; onClick?: () => void; dot?: boolean }) {
  const m = () => memberOf(p.id);
  return (
    <div class="avatar" classList={{ link: !!p.onClick }} style={{ background: `hsl(${colorOf(p.id)} 45% 55%)` }}
      onClick={(e) => p.onClick && (e.stopPropagation(), p.onClick())}>
      {initials(p.id)}
      <Show when={p.dot && !m()?.me}><i class={`link-dot ${m()?.link ?? "off"}`} title={linkText(m())} /></Show>
    </div>
  );
}

// Name and avatar letter/color: asked on first entry, changed in Settings.
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
    if (inSpace) tryAutoAnchor(spaceId);
  };
  // A remembered space opens at once; otherwise (or if the saved password no longer fits) the password screen.
  const enter = async (sp?: Space, h?: How) => {
    setSpace(sp);
    setHow(h);
    if (sp?.remembered) {
      try {
        return await opened(await invoke<boolean>("unlock", { id: sp.id, name: "", password: null, remember: true, n0: n0(), anchor: anchor(), relays: relays(), mailbox: mailbox() }));
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
        ? "Это приглашение в другой круг контактов. Принять? Нынешние чаты закроются, вернуться к ним можно через режим пространств в Настройках"
        : "Вступить в другое пространство по этой ссылке? Нынешнее закроется, вернуться в него можно из списка пространств";
      if (!(await ask(q, { title: "Приглашение", okLabel: "Принять", cancelLabel: "Отмена" }))) return;
      await leave();
    }
    setInvite(url);
    if (stage() === "onboard") setHow("join");
    else enter(undefined, "join");
  };
  onMount(async () => {
    await start();
    await onOpenUrl(invited);
    await invited(await getCurrent());
  });
  return (
    <Show when={status()}>
      {(s) => (
        <Switch>
          <Match when={stage() === "spaces" && !s().spaces.length && !mode()}>
            <ModeChoice onSimple={() => enter()} onSpaces={() => setStage("spaces")} />
          </Match>
          <Match when={stage() === "spaces"}>
            <Spaces spaces={s().spaces} onOpen={enter} onNew={(h) => enter(undefined, h)} onRemoved={refresh} />
          </Match>
          <Match when={stage() === "lock"}>
            <Lock space={space()} how={how()} count={s().spaces.length} onDone={opened}
              onBack={() => (s().spaces.length || setMode(null), setStage("spaces"))}
              onRemoved={async () => (await refresh(), setStage("spaces"))} />
          </Match>
          <Match when={stage() === "onboard"}>
            <Onboard how={how()} setHow={setHow} link={invite()} setLink={setInvite} onDone={() => setStage("main")} />
          </Match>
          <Match when={stage() === "main" && s().spaces.find((x) => x.id === s().current)}>
            {(sp) => <Main dir={s().dir} theme={theme()} setTheme={setTheme} space={sp()} onLeave={leave} refresh={refresh} />}
          </Match>
        </Switch>
      )}
    </Show>
  );
}

function Lock(p: { space?: Space; how?: How; count: number; onDone: (inSpace: boolean) => void; onBack: () => void; onRemoved: () => void }) {
  const [forgot, setForgot] = createSignal(false);
  const defaultName = simple() ? SIMPLE_NAME : p.count ? `Пространство ${p.count + 1}` : "Моё пространство";
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
      p.onDone(await invoke<boolean>("unlock", { id: p.space?.id ?? null, name: name().trim() || defaultName, password: pw(), remember: remember(), n0: n0(), anchor: anchor(), relays: relays(), mailbox: mailbox() }));
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
  return (
    <div class="center">
      <div class="card intro">
        <img src={logo} class="logo" alt="" />
        <h1>{p.spaces.length ? "Пространства" : "Mycel"}</h1>
        <Show when={!p.spaces.length}>
          <p class="muted">Создайте своё пространство или вступите в существующее по ссылке-приглашению</p>
        </Show>
        <For each={p.spaces}>
          {(s) => (
            <div class="actions">
              <button class="grow" onClick={() => p.onOpen(s)}><Hash size={18} /> <span class="ellipsis">{s.name}</span></button>
              <button class="icon" title="Удалить с этого устройства" onClick={() => setDel(s)}><Trash2 size={18} /></button>
            </div>
          )}
        </For>
        <Show when={p.spaces.length}><div class="or"><span>или</span></div></Show>
        <button class="primary" onClick={() => p.onNew("create")}><Plus size={18} /> {p.spaces.length ? "Создать новое" : "Создать пространство"}</button>
        <button onClick={() => p.onNew("join")}><Link size={18} /> Вступить по ссылке</button>
      </div>
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
        <h3>Удалить «{p.space.name}» с этого устройства?</h3>
        <Show when={p.forgot}>
          <p class="muted small">
            Пароль восстановить нельзя: им зашифрована вся переписка на этом устройстве. Можно удалить это пространство отсюда и
            создать новое или вступить заново по приглашению.
          </p>
        </Show>
        <p class="muted small">
          Вся переписка и файлы этого пространства на этом устройстве будут стёрты. У других участников всё останется. Вернуться можно
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
  const [url, setUrl] = createSignal(anchor() ?? "");
  const [state, setState] = createSignal<[boolean, boolean]>();
  const [msg, setMsg] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [auto, setAuto] = createSignal(autoAnchor());
  const check = async () => {
    setState(undefined);
    setState(await invoke<[boolean, boolean]>("anchor_check"));
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
      <label class="check">
        <input type="checkbox" checked={auto()} onChange={(e) => {
          localStorage.setItem("anchor_auto", e.currentTarget.checked ? "1" : "0");
          setAuto(e.currentTarget.checked);
        }} />
        Включать автоматически, если роутер позволяет
      </label>
      <p class="muted small">Mycel сам откроет порт на роутере (UPnP), если роутер это разрешает и у вас «белый» IP.</p>
      <Show when={url()}>
        <div class="anchor-status">
          <div>Адрес: <code>{url()}</code></div>
          <Show when={state()} fallback={<div class="muted">Проверяю доступность…</div>}>
            {(st) => (
              <>
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
  const targets = () => [...list(), ...(anchor() && !list().includes(anchor()!) ? [anchor()!] : [])];
  const check = async () => {
    setChecking(true);
    setChecked({});
    setChecked(await checkServers(targets(), false).catch(() => ({})));
    setChecking(false);
  };
  const [list, setList] = createSignal(relays());
  const [url, setUrl] = createSignal("");
  const [msg, setMsg] = createSignal("");
  const [now, setNow] = createSignal<string | null>();
  const refreshNow = () => invoke<string | null>("home_relay").then(setNow, () => setNow(null));
  refreshNow();
  const save = async (next: string[]) => {
    const prev = list();
    localStorage.setItem("relays", JSON.stringify(next));
    setList(next);
    try {
      await reopen(p.space.id);
      setMsg("");
      setNow(undefined);
      refreshNow();
    } catch (e) {
      localStorage.setItem("relays", JSON.stringify(prev));
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
          <li><b>Якорь друга.</b> Если у кого-то из участников включён якорь, его адрес виден у него в «Настройки» → «Якорь» (вида <code>http://1.2.3.4:3340</code>). Попросите прислать и добавьте сюда.</li>
          <li><b>Ещё один свой ПК.</b> Установите на нём Mycel, войдите в пространство и нажмите «Настройки» → «Якорь» → «Сделать этот ПК якорем». Отдельная программа не нужна, адрес появится там же.</li>
          <li><b>VPS или сервер без экрана.</b> Нужна программа <code>mycel-anchor</code> (Linux или Windows): где её взять и как запустить — в разделе «Якорь» файла README проекта.</li>
          <li><b>Публичные iroh-relay.</b> Подойдёт любой сервер iroh-relay, например <code>https://euc1-1.staging-relay.n0.iroh.link</code>.</li>
        </ul>
        <p>Удалить сервер можно корзиной справа. Изменения применяются сразу, без перезапуска.</p>
      </details>
      <Show when={anchor() && !list().includes(anchor()!)}>
        <div class="relay-row">
          <span class="muted small">Якорь:</span>
          <code class="grow ellipsis">{anchor()}</code>
          <CheckMark c={checked()[anchor()!]} />
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
        <p>Пустое поле выключает ящик: тогда сообщения доходят, только когда отправитель или якорь в сети.</p>
      </details>
      <p class="muted small">Mycel {ver()}</p>
    </>
  );
}

function AppSettings() {
  const [on, setOn] = createSignal(false);
  invoke<boolean>("autostart", { on: null }).then(setOn, () => {});
  return (
    <>
      <h3>Приложение</h3>
      <label class="check">
        <input type="checkbox" checked={on()} onChange={(e) => invoke<boolean>("autostart", { on: e.currentTarget.checked }).then(setOn, (err) => alert(errText(err)))} />
        Запускать вместе с Windows
      </label>
      <p class="muted">Закрытие окна сворачивает Mycel в трей (значок у часов): сообщения и звонки продолжают приходить, уведомления показываются. Выйти полностью: правой кнопкой по значку → «Выйти». Когда Mycel полностью закрыт, новые сообщения придут при следующем запуске — от отправителя или от якоря.</p>
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
  );
}

function Main(p: { dir: string; theme: string; setTheme: (t: string) => void; space: Space; onLeave: () => void; refresh: () => void }) {
  const [del, setDel] = createSignal(false);
  const [tab, setTab] = createSignal<Tab>("chats");
  const [chatOpen, setChatOpen] = createSignal(false);
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
  // Simple mode has no space chat ("").
  const loadChats = () => invoke<ChatRow[]>("chats").then((r) => setRows(simple() ? r.filter((x) => x.id) : r));
  createEffect(() => (simple(), untrack(loadChats)));
  // Unread counts follow our Read receipts: reload them once the receipt is stored.
  const markRead = () => invoke("mark_read", { chat: chat() }).catch(() => {}).then(loadChats);
  const toggleMute = (on: boolean) =>
    invoke("set_muted", { chat: chat(), on }).then(loadChats, (e) => setCallErr("Не удалось изменить уведомления: " + errText(e)));
  createEffect(() => chatVisible() && msgs() && untrack(markRead));
  const unread = () => rows().reduce((n, r) => n + r.unread, 0);

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
  const acceptCall = async () => {
    const c = incoming()!;
    setIncoming(undefined);
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
    pollPeers();
    // A join comes as an MLS commit, not a message: poll members with peers.
    const timer = setInterval(() => (pollPeers(), loadMembers()), 3000);
    // Listen before the first load, so nothing that arrives in between is lost.
    const onMsg = (c: string) => (c === chat() ? reload() : loadChats());
    const un = await listen<string>("message", (e) => onMsg(e.payload));
    const unChanged = await listen<string>("changed", (e) => (onMsg(e.payload), loadMembers()));
    const unCall = await listen<CallSignal>("call", (e) => onCall(e.payload));
    const unStatus = await listen<[string, number][]>("status", (e) => {
      const st = new Map(e.payload);
      setMsgs((all) => all.map((m) => (st.has(m.id) && st.get(m.id) !== m.status ? { ...m, status: st.get(m.id)! } : m)));
    });
    await reload();
    const onVis = () => {
      invoke("set_visible", { v: !document.hidden });
      if (!document.hidden) invoke("sync_now").catch(() => {});
    };
    document.addEventListener("visibilitychange", onVis);
    // Android back closes the chat: opening it pushes a history entry.
    const onBack = () => setChatOpen(false);
    addEventListener("popstate", onBack);
    if (!(await isPermissionGranted())) requestPermission();
    const onWide = () => setWide(wideQuery.matches);
    const onFocus = () => setFocused(true);
    const onBlur = () => setFocused(false);
    wideQuery.addEventListener("change", onWide);
    addEventListener("focus", onFocus);
    addEventListener("blur", onBlur);
    onCleanup(() => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVis);
      removeEventListener("popstate", onBack);
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

  const sortedRows = () => [...rows()].sort((a, b) => (b.last?.ts ?? 0) - (a.last?.ts ?? 0));
  const titles = () => ({ chats: "Чаты", calls: "Звонки", people: simple() ? "Контакты" : "Люди", settings: "Настройки" });
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
        {navItem("people", titles().people, Users)}
        {navItem("settings", "Настройки", Settings)}
      </nav>

      <section class="panel">
        <header class="bar">
          <h2>{titles()[tab()]}</h2>
          <NetDot n={peers()} />
        </header>
        <Switch>
          <Match when={tab() === "chats"}>
            <div class="scroll">
              <For each={sortedRows()}>
                {(r) => {
                  const l = () => r.last;
                  const draft = () => getDraft(draftKey(r.id));
                  return (
                    <button class="row chat-row" classList={{ active: wide() && chat() === r.id }} onClick={() => openChat(r.id)}>
                      {r.id ? <Avatar id={r.id} dot /> : <div class="avatar group"><Hash size={20} /></div>}
                      <div class="grow">
                        <div class="row-top"><b class="ellipsis">{r.id ? nameOf(r.id) : "Общий чат"}</b><small class="muted">{l() ? time(l()!.ts) : ""}</small></div>
                        <div class="row-bottom">
                          <span class="muted ellipsis">{draft() ? `Черновик: ${draft()}` : l() ? (l()!.mine ? "Вы: " : !r.id && !l()!.call ? `${nameOf(l()!.author)}: ` : "") + (l()!.call ? callLabel(l()!.call!) : l()!.media ? label(l()!.media!) : l()!.text) : "Сообщений пока нет"}</span>
                          <Show when={r.unread}><b class="badge">{r.unread}</b></Show>
                        </div>
                      </div>
                    </button>
                  );
                }}
              </For>
              <p class="muted small pad">{simple() ? "Новый чат: «Контакты» → «Написать». Добавить человека — там же, «Добавить контакт»" : "Личный чат: «Люди» → «Написать» или нажмите на аватар в общем чате"}</p>
            </div>
          </Match>
          <Match when={tab() === "calls"}>
            <Calls calls={callList()} onCall={startCall} inCall={!!active()} />
          </Match>
          <Match when={tab() === "people"}>
            <People members={members()} simple={simple()} onWrite={openChat} onCall={(id, v) => startCall(v, id)} inCall={!!active()} />
          </Match>
          <Match when={tab() === "settings"}>
            <div class="scroll pad">
              <h3>Профиль</h3>
              <ProfileForm submit="Сохранить" />
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
                <input type="checkbox" checked={n0()} onChange={(e) => {
                  localStorage.setItem("n0", e.currentTarget.checked ? "1" : "0");
                  reopen(p.space.id).catch((e) => alert(errText(e)));
                }} />
                Публичные серверы связи n0 (Европа, США, Азия)
              </label>
              <p class="muted">Помогают устройствам из разных сетей найти друг друга. Сами сообщения после этого идут напрямую, а через сервер — только если напрямую не получилось. Можно выключить, если настроен свой якорь.</p>
              <RelaySettings space={p.space} />
              <MailboxSettings space={p.space} />
              <Show when={!android}>
                <AnchorSettings space={p.space} />
                <AppSettings />
              </Show>
              <LoginSettings space={p.space} refresh={p.refresh} />
              <ModeSettings />
              <h3>{simple() ? "Данные" : `Пространство «${p.space.name}»`}</h3>
              <p class="muted">Зашифрованная база хранится здесь:</p>
              <code class="path">{p.dir}</code>
              <div class="actions">
                <Show when={!simple()}><button onClick={p.onLeave}><LogOut size={18} /> Сменить пространство</button></Show>
                <button class="danger" onClick={() => setDel(true)}><Trash2 size={18} /> Удалить с этого устройства</button>
              </div>
              <Show when={del()}>
                <RemoveSpace space={p.space} onClose={() => setDel(false)} onDone={p.onLeave} />
              </Show>
            </div>
          </Match>
        </Switch>
      </section>

      <Show when={!(simple() && !chat()) && { c: chat() }} keyed
        fallback={<section class="chat"><div class="msgs"><p class="empty muted">Выберите чат или контакт</p></div></section>}>
        {(k) => (
          <Chat chat={k.c} msgs={msgs()} members={members().length} me={members().find((m) => m.me)?.id ?? ""} admin={members().some((m) => m.me && m.admin)}
            peers={peers()} back={() => history.back()} reload={reload} inCall={!!active()} onCall={(v) => startCall(v)} callErr={callErr()}
            openChat={openChat} muted={!!rows().find((r) => r.id === k.c)?.muted} onMute={toggleMute} />
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
        <Incoming c={incoming()!} onAccept={acceptCall} onReject={rejectCall} />
      </Show>
    </div>
  );
}

function People(p: { members: Member[]; simple: boolean; onWrite: (id: string) => void; onCall: (id: string, video: boolean) => void; inCall: boolean }) {
  const [inv, setInv] = createSignal<{ link: string; svg: string }>();
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal("");
  const [copied, setCopied] = createSignal(false);
  const amAdmin = () => p.members.some((m) => m.me && m.admin);
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
          <UserPlus size={18} /> {busy() ? "Готовим ссылку…" : p.simple ? "Добавить контакт" : "Пригласить"}
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
      <p class="section muted">{p.simple ? `Контакты · ${p.members.length - 1}` : `Участники · ${p.members.length}`}</p>
      <For each={p.members}>
        {(m) => (
          <div class="row person">
            <Avatar id={m.id} dot onClick={m.me ? undefined : () => p.onWrite(m.id)} />
            <div class="grow">
              <b>{nameOf(m.id)}{m.me ? " (вы)" : ""}</b>
              <div class="muted small mono">{m.id}<Show when={m.admin && !p.simple}> · админ</Show>
                <Show when={status(m) === "ok"}> · проверен</Show></div>
              <Show when={status(m) === "changed"}><div class="error">Ключ сменился (новое устройство?) — проверьте код безопасности заново</div></Show>
            </div>
            <Show when={!m.me}>
              <button class="icon" onClick={(e) => (openSafe(m), e.currentTarget.closest(".scroll")?.scrollTo({ top: 0, behavior: "smooth" }))} title="Код безопасности">
                {status(m) === "changed" ? <ShieldAlert size={20} /> : <ShieldCheck size={20} />}
              </button>
              <button class="icon" onClick={() => p.onWrite(m.id)} title="Написать"><MessageCircle size={20} /></button>
              <button class="icon" disabled={p.inCall} onClick={() => p.onCall(m.id, false)} title="Позвонить"><Phone size={20} /></button>
              <button class="icon" disabled={p.inCall} onClick={() => p.onCall(m.id, true)} title="Видеозвонок"><Video size={20} /></button>
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
  !!a && !!b && !a.call && !b.call && a.mine === b.mine && (a.mine || a.author === b.author)
  && dayKey(a.ts) === dayKey(b.ts) && Math.abs(b.ts - a.ts) < 600_000;
const plain = (m: Msg) => m.text || (m.media ? label(m.media) : "");
type Dialog = { kind: "delete" | "forward"; ids: string[] };
const baseName = (path: string) => path.split(/[\\/]/).pop() ?? path;
const isPhoto = (path: string) => /\.(jpe?g|png|webp)$/i.test(path);

function Chat(p: {
  chat: string; msgs: Msg[]; members: number; me: string; admin: boolean; peers: number; back: () => void; reload: () => Promise<void>;
  inCall: boolean; onCall: (video: boolean) => void; callErr: string; openChat: (id: string) => void;
  muted: boolean; onMute: (on: boolean) => void;
}) {
  const key = draftKey(p.chat);
  const [text, setText] = createSignal(getDraft(key));
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
  const name = (id: string) => (id === p.me ? "Вы" : nameOf(id));
  const shown = () => {
    const ids = new Set(p.msgs.map((m) => m.id));
    return [...p.msgs, ...pending().filter((x) => !x.real || !ids.has(x.real))];
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
    setPending((ps) => [...ps, { id, author: p.me, mine: true, text: "", ts: Date.now(), status: 0, media: null, call: null, reply: null, edited: false, pinned: false, reactions: [], ...m }]);
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
    const el = list.querySelector(`[data-id="${id}"]`);
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
  // Files waiting in the "send" window (like Telegram's): photos compressed unless unticked.
  const [toSend, setToSend] = createSignal<string[]>([]);
  const [compress, setCompress] = createSignal(localStorage.getItem("compress") !== "0");
  const [drag, setDrag] = createSignal(false);
  const [video, setVideo] = createSignal(false);
  const [rec, setRec] = createSignal<Recording>();
  const [elapsed, setElapsed] = createSignal(0);
  const [slide, setSlide] = createSignal(0); // finger moved left while holding, px (≤ 0)
  const [lift, setLift] = createSignal(0); // finger moved up while holding, px (≥ 0)
  const [locked, setLocked] = createSignal(false); // recording locked: hands-free, send/cancel by buttons
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
  // Android pickers give content:// URIs, not paths: read the files here and stash them for send_file.
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
    setToSend(paths);
  };
  const confirmSend = async () => {
    const paths = toSend(), original = !compress();
    setToSend([]);
    for (const path of paths)
      await queue({ text: `📎 ${baseName(path)}` }, () => invoke<string>("send_file", { chat: p.chat, path, original }));
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
    // A restored draft gets its height once the chat is visible (hidden on a phone until opened).
    let sized = false;
    const ro = new ResizeObserver(() => {
      if (!sized && list.clientHeight) (sized = true), grow();
      bottom();
    });
    ro.observe(list);
    onCleanup(() => ro.disconnect());
    const un = await getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "drop") {
        setDrag(false);
        sendFiles(e.payload.paths);
      } else setDrag(e.payload.type !== "leave");
    });
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (toSend().length) return setToSend([]);
      if (view() || menu() || dlg()) return (setView(""), setMenu(undefined), setDlg(undefined));
      if (rec()) return stopRec(false);
      if (sel().length) return setSel([]);
      if (query() !== undefined) return setQuery(undefined);
      cancelCompose();
    };
    addEventListener("keydown", esc);
    onCleanup(() => {
      un();
      removeEventListener("keydown", esc);
    });
  });

  // 🎤/📹: a tap switches the mode, holding records; release sends, slide sideways (or Esc) cancels.
  // Hold: slide left past 120px cancels; slide up past 80px locks (release does nothing then; the big button sends).
  let holdTimer = 0, tick = 0, startX = 0, startY = 0, holding = false, raf = 0, halo!: HTMLElement;
  let ctx: AudioContext | undefined;
  const vib = (ms: number) => navigator.vibrate?.(ms);
  // The halo behind the mic scales 1.0–1.4 with the microphone's loudness (smoothed).
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
      if (halo) halo.style.transform = `scale(${1 + 0.4 * lvl})`;
      raf = requestAnimationFrame(frame);
    };
    frame();
  };
  const stopRec = async (send: boolean) => {
    const r = rec();
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
      if (!send) vib(40); // a deliberate cancel buzzes; a too-short press is silently dropped
      return r.cancel();
    }
    vib(15);
    const blob = await r.stop();
    const kind = video() ? "videonote" : "voice";
    const wave = kind === "voice" ? await waveform(blob) : [];
    const body = new Uint8Array(await blob.arrayBuffer());
    const headers = { chat: p.chat, kind, mime: r.mime, duration: String(Date.now() - r.started), wave: wave.join(",") };
    await queue({ text: kind === "voice" ? "🎤 Голосовое" : "📹 Видеосообщение" }, () => invoke<string>("send_rec", body, { headers }));
  };
  const down = (e: PointerEvent) => {
    if (locked()) {
      // Locked: the big button is Send.
      void stopRec(true);
      return;
    }
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    startX = e.clientX;
    startY = e.clientY;
    holding = true;
    holdTimer = window.setTimeout(async () => {
      try {
        const r = await record(video());
        if (!holding) return r.cancel();
        setRec(r);
        setElapsed(0);
        vib(15);
        if (video()) preview.srcObject = r.stream;
        meter(r.stream);
        tick = window.setInterval(() => {
          setElapsed(Date.now() - r.started);
          if (video() && Date.now() - r.started >= 60_000) stopRec(true);
        }, 100);
      } catch (e) {
        setErr("Нет доступа к микрофону или камере: " + errText(e));
      }
    }, 300);
  };
  const up = () => {
    if (!holding) return; // already cancelled or locked while sliding
    holding = false;
    clearTimeout(holdTimer);
    if (rec()) stopRec(true);
    else setVideo(!video());
  };
  const abort = () => {
    holding = false;
    clearTimeout(holdTimer);
    if (rec() && !locked()) stopRec(false);
  };
  const move = (e: PointerEvent) => {
    if (!holding || !rec()) return;
    const dx = Math.min(0, e.clientX - startX), dy = Math.max(0, startY - e.clientY);
    if (dy > 80) {
      holding = false; // from now on the release is ignored
      setLocked(true);
      setSlide(0);
      setLift(0);
      vib(15);
    } else if (dx < -120) {
      holding = false;
      stopRec(false);
    } else {
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
    queue({ text: t, reply: r ? { id: r.id, author: r.author, text: plain(r) } : null },
      () => invoke<string>("send", { chat: p.chat, text: t, replyTo: r?.id ?? null }),
      // The chat may be closed by now: keep the unsent text as its draft unless a newer one exists.
      () => (getDraft(key) || setDraft(key, t), setText(t), setReply(r), queueMicrotask(grow)));
  };
  // While editing, the field holds the old message, not the draft.
  createEffect(() => {
    const t = text();
    if (!editing()) setDraft(key, t);
  });
  const canAll = (ids: string[]) => p.admin || ids.every((id) => p.msgs.find((m) => m.id === id)?.mine);
  return (
    <section class="chat">
      <Show when={sel().length} fallback={<header class="bar">
        <button class="icon back" onClick={p.back} title="Назад"><ArrowLeft size={20} /></button>
        {p.chat ? <Avatar id={p.chat} dot /> : <div class="avatar group small-av"><Hash size={18} /></div>}
        <div class="grow ellipsis">
          <b>{p.chat ? nameOf(p.chat) : "Общий чат"}</b>
          <div class="muted small">{p.chat ? linkText(memberOf(p.chat)) : `${p.members} участн.`}</div>
        </div>
        <NetDot n={p.peers} />
        <button class="icon" onClick={() => setQuery(query() === undefined ? "" : undefined)} title="Поиск в чате"><Search size={20} /></button>
        <button class="icon" aria-pressed={p.muted} onClick={() => p.onMute(!p.muted)}
          title={p.muted ? "Без звука: включить уведомления" : "Отключить уведомления"}>
          {p.muted ? <BellOff size={20} /> : <Bell size={20} />}
        </button>
        <button class="icon" disabled={p.inCall || p.members < 2} onClick={() => p.onCall(false)}
          title={p.chat ? "Звонок" : "Групповой звонок"}><Phone size={20} /></button>
        <button class="icon" disabled={p.inCall || p.members < 2} onClick={() => p.onCall(true)}
          title={p.chat ? "Видеозвонок" : "Групповой видеозвонок"}><Video size={20} /></button>
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
                      {m.mine || p.chat ? "" : `${nameOf(m.author)} · `}{callLabel(m.call)} · {time(m.ts)}
                    </div>
                  ) : (
                    <div class="msg" data-id={m.id}
                      classList={{ mine: m.mine, cont: same(prev(), m), tail: !same(m, next()), sel: sel().includes(m.id), found: hit() === m.id }}
                      onClick={(e) => clickMsg(e, m)} onMouseDown={(e) => e.shiftKey && e.preventDefault()} onContextMenu={(e) => openMenu(e, m)}
                      onPointerDown={(e) => press(e, m)} onPointerUp={unpress} onPointerCancel={unpress} onPointerMove={unpress}>
                      <button class="pick" title="Выделить" onClick={(e) => (e.stopPropagation(), toggle(m.id))}>
                        <Show when={sel().includes(m.id)}><Check size={14} /></Show>
                      </button>
                      <Show when={!m.mine}>
                        <Show when={!same(m, next())} fallback={<div class="av-gap" />}>
                          <Avatar id={m.author} onClick={p.chat ? undefined : () => p.openChat(m.author)} />
                        </Show>
                      </Show>
                      <div class="bubble">
                        <Show when={!m.mine && !p.chat && !same(prev(), m)}>
                          <div class="author link" style={{ color: `hsl(${colorOf(m.author)} 50% 50%)` }} title="Написать лично"
                            onClick={(e) => (e.stopPropagation(), p.openChat(m.author))}>{nameOf(m.author)}</div>
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
                        <Show when={m.text}>
                          <span class="text">{re() ? m.text.split(re()!).map((s, i) => (i % 2 ? <mark>{s}</mark> : s)) : m.text}</span>
                        </Show>
                        <Show when={m.reactions.length}>
                          <div class="reacts">
                            <For each={m.reactions}>
                              {([e, who]) => (
                                <button classList={{ on: who.includes(p.me) }} title={who.map(name).join(", ")}
                                  onClick={(ev) => (ev.stopPropagation(), act(invoke("react", { id: m.id, emoji: e })))}>
                                  {e} {who.length}
                                </button>
                              )}
                            </For>
                          </div>
                        </Show>
                        <span class="meta">
                          <Show when={m.pinned}><Pin size={11} /></Show>
                          <Show when={m.edited}><span>изменено</span></Show>
                          {time(m.ts)}
                          <Show when={m.mine}>
                            <Switch>
                              <Match when={m.status === 0}><span title="Отправляется"><Clock size={14} /></span></Match>
                              <Match when={m.status === 1}><span title="Доставлено"><Check size={14} /></span></Match>
                              <Match when={m.status === 2}><span title="Прочитано"><CheckCheck size={14} /></span></Match>
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
            <div class="menu" style={{ left: `${Math.min(mn().x, innerWidth - 210)}px`, top: `${Math.min(mn().y, innerHeight - 350)}px` }}>
              <div class="emojis">
                <For each={["👍", "❤️", "😂", "😮", "😢", "🔥"]}>
                  {(e) => <button onClick={() => act(invoke("react", { id: mn().m.id, emoji: e }))}>{e}</button>}
                </For>
              </div>
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
                  <div class="dialog-list">
                    <For each={["", ...people().filter((m) => !m.me).map((m) => m.id)]}>
                      {(to) => (
                        <button class="row chat-row" onClick={() => {
                          const ids = d().ids;
                          setDlg(undefined);
                          setSel([]);
                          act(invoke("forward", { ids, to }));
                        }}>
                          {to ? <Avatar id={to} /> : <div class="avatar group small-av"><Hash size={18} /></div>}
                          <b>{to ? nameOf(to) : "Общий чат"}</b>
                        </button>
                      )}
                    </For>
                  </div>
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
            <button class="primary" onClick={confirmSend}><Send size={16} /> Отправить</button>
            <button onClick={() => setToSend([])}>Отмена</button>
          </div>
        </div>
      </Show>
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
      <input ref={picker} type="file" multiple hidden onChange={pick} />
      <div class="composer">
        <button class="icon" onClick={attach} title="Прикрепить файл (или перетащите в окно)"><Paperclip size={20} /></button>
        <Show when={rec()}>
          <div class="recording grow" classList={{ cancel: slide() < -60 }}>
            <i class="dot" /> {Math.floor(elapsed() / 60000)}:{String(Math.floor(elapsed() / 1000) % 60).padStart(2, "0")},{Math.floor(elapsed() / 100) % 10}
            <Show when={locked()} fallback={
              <span class="muted small grow" style={{ opacity: 1 - Math.min(1, -slide() / 120) }}>‹ Влево — отмена</span>
            }>
              <span class="grow" />
              <button class="link" onClick={() => stopRec(false)}>Отмена</button>
            </Show>
          </div>
        </Show>
        <Show when={rec() && !locked()}>
          <div class="lock-tip" style={{ transform: `translate(${slide()}px, ${-lift()}px)`, opacity: 0.3 + 0.7 * (lift() / 80) }}><LockIcon size={18} /></div>
        </Show>
        <textarea ref={input} rows={1} placeholder={video() ? "Сообщение · удерживайте 📹 для видеосообщения" : "Сообщение"} value={text()} style={{ display: rec() ? "none" : undefined }}
          onInput={(e) => { setText(e.currentTarget.value); grow(); }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
              e.preventDefault();
              send();
            }
          }} />
        <Show when={!rec()}>
          <button class="icon" classList={{ on: emo() !== undefined }} title="Смайлики"
            onClick={() => setEmo(emo() !== undefined ? undefined : recentEmoji().length ? -1 : 0)}><Smile size={20} /></button>
        </Show>
        <Show when={emo() !== undefined}>
          <div class="overlay" onClick={() => setEmo(undefined)} />
          <div class="emoji-panel card">
            <div class="emoji-tabs">
              <Show when={recentEmoji().length}>
                <button classList={{ active: emo() === -1 }} onClick={() => setEmo(-1)} title="Недавние"><Clock size={18} /></button>
              </Show>
              <For each={EMOJI}>{([icon], i) => <button classList={{ active: emo() === i() }} onClick={() => setEmo(i())}>{icon}</button>}</For>
            </div>
            <div class="emoji-grid">
              <For each={emo() === -1 ? recentEmoji() : EMOJI[emo()!][1].split(" ")}>
                {(e) => <button onClick={() => (insertEmoji(e), input.focus())}>{e}</button>}
              </For>
            </div>
          </div>
        </Show>
        <Show when={text().trim() || editing()} fallback={
          <button class="icon mic" classList={{ live: !!rec() }}
            style={rec() ? { transform: `translateX(${slide()}px) scale(2)` } : undefined}
            onPointerDown={down} onPointerUp={up} onPointerMove={move} onPointerCancel={abort}
            title={video() ? "Удерживайте — видеосообщение; нажмите — голосовое" : "Удерживайте — голосовое; нажмите — видеосообщение"}>
            <i class="halo" ref={halo} />
            {locked() ? <Send size={20} /> : video() ? <Video size={20} /> : <Mic size={20} />}
          </button>
        }>
          <button class="icon send" disabled={!text().trim()} onClick={send} title={editing() ? "Сохранить" : "Отправить"}>
            {editing() ? <Check size={20} /> : <Send size={20} />}
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
                <b>{c.chat ? nameOf(c.chat) : c.mine ? "Общий чат" : `${nameOf(c.author)} · общий чат`}</b>
                <div class="muted small" classList={{ error: c.missed && !c.mine }}>{c.mine ? "↗ " : "↙ "}{callLabel(c)}</div>
              </div>
              <small class="muted">{new Date(c.ts).toLocaleString("ru", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })}</small>
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
      <p class="muted">{p.c.video ? "Входящий видеозвонок" : "Входящий звонок"}{p.c.chat ? "" : " · общий чат"}</p>
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
  const [camErr, setCamErr] = createSignal("");
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
    } catch (e) {
      setCamErr("Камера не включилась: " + errText(e));
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
              <span class="tile-name">{nameOf(t.id)} <Signal net={t.net()} /></span>
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
      <Show when={camErr()}><p class="error">{camErr()}</p></Show>
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
