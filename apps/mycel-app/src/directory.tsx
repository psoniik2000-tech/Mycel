// Usernames on the main server and Telegram-style groups (DESIGN.md §4.6).
import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { AtSign, Check, LogIn, MessageCircle, UserPlus, Users, X } from "lucide-solid";
import { rich } from "./rich";
import { groupTitle } from "./simple";

/** The main server: an anchor that also keeps the username directory. `url#id` once pinned. */
export const DEFAULT_SERVER = "http://5.130.50.199:3340";
export const server = () => localStorage.getItem("server") || DEFAULT_SERVER;
const serverUrl = () => server().split("#")[0];
/** The main server's relay is used in every space too. */
export const withServer = (relays: string[]) => (relays.includes(serverUrl()) ? relays : [...relays, serverUrl()]);
/** ...and keeps envelopes for everyone offline, besides the user's mailbox (which wakes phones). */
export const withBox = (mailbox: string | null) => [...new Set([mailbox, serverUrl()].filter((m): m is string => !!m))].join(" ");
const errText = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));
const norm = (u: string) => u.trim().replace(/^@/, "").toLowerCase();
export const validUsername = (u: string) => /^[a-z0-9_]{5,32}$/.test(norm(u));
const userKey = (space: string) => `dir_user:${space}`;
// UI id ("" = me) -> @username (null: none), per run.
const names = new Map<string, Promise<string | null>>();

/** Registers (or renames to) `username`; the server's answer pins its id on first use. */
export async function register(space: string, username: string) {
  const pinned = await invoke<string>("dir_register", { server: server(), username: norm(username) });
  if (!server().includes("#") && pinned.startsWith(serverUrl() + "#")) localStorage.setItem("server", pinned);
  localStorage.setItem(userKey(space), norm(username));
  names.set("", Promise.resolve(norm(username)));
}

/** Sends our record again, if we have a username (it carries addresses and the search setting). */
export const refreshRegistration = async (space: string) => {
  const u = localStorage.getItem(userKey(space));
  if (u) await register(space, u);
};
/** At every start, quietly: addresses change. */
export const keepRegistered = (space: string) => void refreshRegistration(space).catch(() => {});

export const usernameOf = (id: string) => {
  if (!names.has(id)) names.set(id, invoke<string | null>("dir_name_of", { server: server(), id }).catch(() => null));
  return names.get(id)!;
};

/** " · @username" of a member or contact, once known. */
export function Username(p: { id: string; me?: boolean }) {
  const [u, setU] = createSignal<string | null>(null);
  createEffect(() => usernameOf(p.me ? "" : p.id).then(setU));
  return <Show when={u()}> · @{u()}</Show>;
}

/** First start: name and @username, a personal space made silently. `onOther`: the old ways. */
export function QuickStart(p: { space: string; onDone: () => void; onOther: () => void; color: number }) {
  const [name, setName] = createSignal("");
  const [user, setUser] = createSignal("");
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [made, setMade] = createSignal(false);
  const go = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    setErr("");
    try {
      if (!made()) {
        await invoke("create_space");
        await invoke("set_profile", { name: name().trim(), avatar: "", color: p.color });
        setMade(true);
      }
      await register(p.space, user());
      p.onDone();
    } catch (e) {
      setErr(errText(e));
    }
    setBusy(false);
  };
  return (
    <form class="card intro" onSubmit={go}>
      <h1>Добро пожаловать</h1>
      <p class="muted">Имя видят ваши собеседники, а по имени пользователя вас можно найти в поиске</p>
      <input placeholder="Как вас зовут?" maxLength={64} value={name()} disabled={made()} onInput={(e) => setName(e.currentTarget.value)} autofocus />
      <input placeholder="@имя_пользователя" maxLength={33} value={user()} onInput={(e) => setUser(e.currentTarget.value)} />
      <Show when={user() && !validUsername(user())}><p class="muted small">5–32 знака: латиница, цифры и _</p></Show>
      <Show when={err()}><p class="error">{err()}</p></Show>
      <button class="primary" disabled={busy() || !name().trim() || !validUsername(user())}>{busy() ? "Регистрируем…" : "Начать"}</button>
      <Show when={made() && err()}>
        <button type="button" onClick={p.onDone}>Продолжить без имени пользователя</button>
      </Show>
      <Show when={!made()}>
        <button type="button" class="link" onClick={p.onOther}>Другие способы</button>
      </Show>
    </form>
  );
}

/** Chat list: asks an account without @username to set one; the cross closes it for good. */
export function UsernameBanner(p: { onOpen: () => void }) {
  const [show, setShow] = createSignal(false);
  onMount(() => {
    if (localStorage.getItem("username_hint") === "0" || Object.keys(localStorage).some((k) => k.startsWith(userKey("")))) return;
    // An unreachable server says nothing about the name: stay quiet then.
    invoke<string | null>("dir_name_of", { server: server(), id: "" }).then((u) => setShow(!u), () => {});
  });
  return (
    <Show when={show()}>
      <div class="update-banner">
        <span class="grow" style={{ cursor: "pointer" }} role="button" tabIndex={0} onClick={p.onOpen} onKeyDown={(e) => e.key === "Enter" && p.onOpen()}>
          Задайте имя пользователя, чтобы вас находили
        </span>
        <button class="icon" title="Закрыть" onClick={() => (localStorage.setItem("username_hint", "0"), setShow(false))}><X size={18} /></button>
      </div>
    </Show>
  );
}

/** Chats search: people on the main server by @username, like Telegram's global search. */
export function DirSearch(p: { q: string; onOpen: (chat: string) => void }) {
  const [rows, setRows] = createSignal<[string, string][]>([]);
  const [err, setErr] = createSignal("");
  const [adding, setAdding] = createSignal("");
  createEffect(() => {
    const q = norm(p.q);
    setErr("");
    if (q.length < 2 || !/^[a-z0-9_]+$/.test(q)) return setRows([]);
    const t = setTimeout(() => invoke<[string, string][]>("dir_search", { server: server(), prefix: q }).then(setRows, (e) => (setRows([]), setErr(errText(e)))), 400);
    onCleanup(() => clearTimeout(t));
  });
  const add = async (u: string) => {
    setAdding(u);
    setErr("");
    try {
      p.onOpen(await invoke<string>("dir_add", { server: server(), username: u }));
    } catch (e) {
      setErr(errText(e));
    }
    setAdding("");
  };
  return (
    <Show when={rows().length || err()}>
      <p class="section muted">Глобальный поиск</p>
      <For each={rows()}>
        {([u, n]) => (
          <button class="row chat-row" disabled={!!adding()} onClick={() => add(u)}>
            <div class="avatar group"><AtSign size={20} /></div>
            <div class="grow">
              <div class="row-top"><b class="ellipsis">{n || u}</b></div>
              <div class="row-bottom"><span class="muted ellipsis">{adding() === u ? "Добавляем…" : `@${u}`}</span></div>
            </div>
          </button>
        )}
      </For>
      <Show when={err()}><p class="muted small pad">{err()}</p></Show>
    </Show>
  );
}

/** Settings → Профиль: our @username and the main server address. */
export function UsernameSettings(p: { space: string }) {
  const [cur, setCur] = createSignal<string | null>(localStorage.getItem(userKey(p.space)));
  const [user, setUser] = createSignal("");
  const [srv, setSrv] = createSignal(server());
  const [msg, setMsg] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  onMount(() => usernameOf("").then((u) => u && setCur(u)));
  const save = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      await register(p.space, user());
      setCur(norm(user()));
      setUser("");
      setMsg("Сохранено");
    } catch (e) {
      setMsg(errText(e));
    }
    setBusy(false);
  };
  return (
    <form class="profile" onSubmit={save}>
      <h3>Имя пользователя</h3>
      <p class="muted">{cur() ? `@${cur()} — по нему вас находят в поиске` : "Не задано: вас можно добавить только по ссылке"}</p>
      <input placeholder={cur() ? "Новое имя пользователя" : "@имя_пользователя"} maxLength={33} value={user()} onInput={(e) => (setUser(e.currentTarget.value), setMsg(""))} />
      <Show when={msg()}><p class="muted small">{msg()}</p></Show>
      <button class="primary" disabled={busy() || !validUsername(user())}>{cur() ? "Сменить" : "Сохранить"}</button>
      <h3>Сервер имён</h3>
      <p class="muted small">Адрес главного сервера, который хранит имена пользователей. Переписка через него не идёт.</p>
      <input value={srv()} onInput={(e) => setSrv(e.currentTarget.value)} />
      <button type="button" disabled={srv().trim() === server()} onClick={() => {
        srv().trim() && srv().trim() !== DEFAULT_SERVER ? localStorage.setItem("server", srv().trim()) : localStorage.removeItem("server");
        names.clear();
        setSrv(server());
      }}>Сохранить адрес</button>
    </form>
  );
}

type SpaceRow = { id: string; name: string };
const spacesChanged = () => window.dispatchEvent(new Event("mycel-spaces"));

/** Groups (other spaces) as rows in the chat list: a tap switches to one. */
export function SpaceRows(p: { current: string; personal: string; groupsOnly?: boolean; last: (id: string) => { text: string; time: string } | undefined; onEnter: (id: string) => void }) {
  const [list, setList] = createSignal<SpaceRow[]>([]);
  const load = () => invoke<{ spaces: SpaceRow[] }>("status").then((s) =>
    setList(s.spaces.filter((x) => x.id !== p.current && !(p.groupsOnly && x.name === p.personal))));
  onMount(() => {
    load();
    window.addEventListener("mycel-spaces", load);
  });
  onCleanup(() => window.removeEventListener("mycel-spaces", load));
  return (
    <For each={list()}>
      {(s) => (
        <button class="row chat-row" onClick={() => p.onEnter(s.id)}>
          <div class="avatar group">{s.name === p.personal ? <MessageCircle size={20} /> : <Users size={20} />}</div>
          <div class="grow">
            <div class="row-top"><b class="ellipsis">{groupTitle(s.name)}</b><small class="when">{p.last(s.id)?.time}</small></div>
            <div class="row-bottom"><span class="muted ellipsis">{rich(p.last(s.id)?.text ?? (s.name === p.personal ? "Контакты и переписка" : "Группа"))}</span></div>
          </div>
        </button>
      )}
    </For>
  );
}

/** «Новая группа»: pick contacts, name it; they get an invite in their chat with us. */
export function NewGroup(p: { people: { id: string; me: boolean; name: string | null }[]; onEnter: (id: string) => void }) {
  const [open, setOpen] = createSignal(false);
  const [name, setName] = createSignal("");
  const [picked, setPicked] = createSignal<string[]>([]);
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const toggle = (id: string) => setPicked(picked().includes(id) ? picked().filter((x) => x !== id) : [...picked(), id]);
  const create = async () => {
    setBusy(true);
    setErr("");
    try {
      const id = await invoke<string>("create_group", { name: name().trim(), members: picked() });
      setOpen(false);
      spacesChanged();
      p.onEnter(id);
    } catch (e) {
      setErr(errText(e));
    }
    setBusy(false);
  };
  return (
    <>
      <button class="row chat-row" onClick={() => (setOpen(true), setPicked([]), setName(""), setErr(""))}>
        <div class="avatar group"><UserPlus size={20} /></div>
        <div class="grow"><div class="row-top"><b>Новая группа</b></div></div>
      </button>
      <Show when={open()}>
        <div class="modal" onClick={(e) => e.target === e.currentTarget && !busy() && setOpen(false)}>
          <div class="card dialog">
            <h3>Новая группа</h3>
            <input placeholder="Название группы" maxLength={64} value={name()} onInput={(e) => setName(e.currentTarget.value)} autofocus />
            <p class="muted small">Кого пригласить: им придёт приглашение в ваш чат</p>
            <div class="scroll" style={{ "max-height": "40vh" }}>
              <For each={p.people.filter((m) => !m.me)} fallback={<p class="muted small">Сначала добавьте контакты</p>}>
                {(m) => (
                  <label class="check">
                    <input type="checkbox" checked={picked().includes(m.id)} onChange={() => toggle(m.id)} />
                    {m.name ?? "Без имени"}
                  </label>
                )}
              </For>
            </div>
            <Show when={err()}><p class="error">{err()}</p></Show>
            <div class="actions">
              <button class="primary" disabled={busy() || !name().trim()} onClick={create}>{busy() ? "Создаём…" : "Создать"}</button>
              <button disabled={busy()} onClick={() => setOpen(false)}>Отмена</button>
            </div>
          </div>
        </div>
      </Show>
    </>
  );
}

const JOIN_RE = /mycel:\/\/join\/[A-Z2-7]+/;
/** A group invite link inside a message text. */
export const joinLink = (text: string) => text.match(JOIN_RE)?.[0];

/** «Вступить» under a group invite: joins as a new space beside this one; it appears in the chat list. */
export function JoinGroup(p: { text: string; mine: boolean }) {
  const link = () => joinLink(p.text)!;
  const [state, setState] = createSignal(localStorage.getItem(`group:${link()}`) ? "done" : "");
  const join = async (e: MouseEvent) => {
    e.stopPropagation();
    setState("busy");
    try {
      await invoke("join_group", { link: link(), name: p.text.match(/«(.+?)»/)?.[1] ?? "" });
      localStorage.setItem(`group:${link()}`, "1");
      setState("done");
      spacesChanged();
    } catch (e) {
      setState(errText(e));
    }
  };
  return (
    <Show when={!p.mine}>
      <div class="actions">
        <button class="primary" disabled={state() === "busy" || state() === "done"} onClick={join}>
          {state() === "done" ? <><Check size={16} /> Вы в группе — она в списке чатов</> : state() === "busy" ? "Вступаем…" : <><LogIn size={16} /> Вступить</>}
        </button>
      </div>
      <Show when={state() && !["busy", "done"].includes(state())}><p class="error small">{state()}</p></Show>
    </Show>
  );
}

