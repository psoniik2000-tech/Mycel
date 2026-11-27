// @mentions in group chats, like in Telegram: the picker over the composer and the links in messages.
import { createEffect, createMemo, createSignal, For, JSX, on, Show } from "solid-js";
import { usernameOf } from "./directory";
import { rich } from "./rich";

type Person = { id: string; me: boolean; name: string | null };
const NICK = /(?<![\p{L}\p{N}_])@([\p{L}\p{N}_]+)/gu;
const TYPING = /(?<![\p{L}\p{N}_])@([\p{L}\p{N}_]*)$/u;
// UI id -> @username on the main server, filled as answers come.
const [usernames, setUsernames] = createSignal<Record<string, string>>({});

// shortcut: a username belongs to one space's key, so most group members have none there and their
// nick is the name (renaming unlinks old mentions); publish it in the profile if that bites.
function nicks(people: Person[]) {
  const u = usernames();
  const base = people.map((m) => (m.name ?? "").replace(/[^\p{L}\p{N}_]+/gu, "_").replace(/^_+|_+$/g, "") || `id_${m.id}`);
  const low = base.map((b) => b.toLowerCase());
  return people.map((m, i) => {
    const taken = people.some((o, j) => j !== i && (low[j] === low[i] || u[o.id] === low[i]));
    const byName = taken ? `${base[i]}_${m.id.slice(0, 4)}` : base[i];
    return { m, byName, nick: u[m.id] ?? byName };
  });
}

/** Lowercased nick -> id: name nicks keep working after a username appears. */
function lookup(people: Person[]) {
  const all = nicks(people);
  return new Map([...all.map((x) => [x.byName.toLowerCase(), x.m.id] as const), ...all.map((x) => [x.nick.toLowerCase(), x.m.id] as const)]);
}

/** Ids of the others a text mentions. */
export function mentionIds(text: string, people: Person[]) {
  const map = lookup(people);
  const ids = [...text.matchAll(NICK)].map((x) => map.get(x[1].toLowerCase()));
  return [...new Set(ids)].filter((id): id is string => !!id && !people.find((m) => m.id === id)?.me);
}

/** Message text with its mentions as links to the member's profile. */
export function mentionText(text: string, people: Person[], open: (id: string) => void): JSX.Element {
  if (!text.includes("@")) return rich(text);
  const map = lookup(people);
  return text.split(NICK).map((s, i) => {
    if (!(i % 2)) return rich(s);
    const id = map.get(s.toLowerCase());
    if (!id) return `@${s}`;
    return <a class="mention" href="#" onClick={(e) => (e.preventDefault(), e.stopPropagation(), !people.find((m) => m.id === id)?.me && open(id))}>@{s}</a>;
  });
}

/** The member list shown while an `@…` is being typed. `people` is empty outside group chats. */
export function useMention(p: { people: () => Person[]; text: () => string; input: () => HTMLTextAreaElement; insert: (text: string, caret: number) => void; avatar: (id: string) => JSX.Element }) {
  const [caret, setCaret] = createSignal(0);
  const [closed, setClosed] = createSignal(false); // by Esc, until the next input
  const [cur, setCur] = createSignal(0);
  createEffect(() => p.people().forEach((m) =>
    usernameOf(m.id).then((u) => u && usernames()[m.id] !== u && setUsernames((s) => ({ ...s, [m.id]: u })))));
  const query = createMemo(() => p.text().slice(0, caret()).match(TYPING)?.[1]);
  const list = createMemo(() => {
    const q = query()?.toLowerCase();
    if (q === undefined || closed()) return [];
    return nicks(p.people()).filter((x) => !x.m.me && [x.nick, x.byName, ...(x.m.name ?? "").split(" ")].some((s) => s.toLowerCase().startsWith(q)));
  });
  createEffect(on(query, () => setCur(0)));
  const pick = (nick: string) => {
    const end = caret(), start = end - query()!.length - 1, t = p.text();
    setCaret(start + nick.length + 2);
    p.insert(`${t.slice(0, start)}@${nick} ${t.slice(end)}`, start + nick.length + 2);
  };
  return {
    /** The text or the caret moved. */
    track: (e?: Event) => (e?.type === "input" && setClosed(false), setCaret(p.input().selectionStart)),
    /** True: the key was for the list. */
    key: (e: KeyboardEvent) => {
      const n = list().length;
      if (!n || e.isComposing) return false;
      if (e.key === "ArrowDown" || e.key === "ArrowUp") setCur((cur() + (e.key === "ArrowDown" ? 1 : n - 1)) % n);
      else if ((e.key === "Enter" && !e.shiftKey) || e.key === "Tab") pick(list()[Math.min(cur(), n - 1)].nick);
      else if (e.key === "Escape") setClosed(true);
      else return false;
      e.preventDefault();
      e.stopPropagation(); // Esc must not also cancel the reply
      return true;
    },
    view: () => (
      <Show when={list().length}>
        <div class="mentions card" role="listbox" aria-label="Участники">
          <For each={list()}>
            {(x, i) => (
              // Keeps the focus (and the caret) in the field.
              <button class="row" role="option" aria-selected={i() === cur()} onPointerDown={(e) => e.preventDefault()} onClick={() => pick(x.nick)}>
                {p.avatar(x.m.id)}
                <b class="ellipsis">{x.m.name ?? `Участник ${x.m.id}`}</b>
                <span class="muted ellipsis">@{x.nick}</span>
              </button>
            )}
          </For>
        </div>
      </Show>
    ),
  };
}
