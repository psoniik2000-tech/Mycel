// «Переслать в чат», like in Telegram: the chats of the open space, then groups (other spaces).
import { invoke } from "@tauri-apps/api/core";
import { Hash, Users } from "lucide-solid";
import { createSignal, For, JSX, onMount, Show } from "solid-js";
import { simple, SIMPLE_NAME } from "./simple";

type Space = { id: string; name: string };

/** `to`: "" = the space chat, else a person; `space`: set when it is not the open one. */
export function ForwardList(p: {
  current: string; people: { id: string; name: string }[]; avatar: (id: string) => JSX.Element;
  onPick: (to: string, name: string, space?: string) => void;
}) {
  const [spaces, setSpaces] = createSignal<Space[]>([]);
  // Personal chats live in their own space: a tap on it lists its people.
  const [inside, setInside] = createSignal<{ space: Space; people: [string, string][] }>();
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal("");
  onMount(() => invoke<{ spaces: Space[] }>("status").then((s) => setSpaces(s.spaces)));
  const others = () => spaces().filter((s) => s.id !== p.current);
  // Simple mode has no space chat in the personal space.
  const general = () => !(simple() && spaces().find((s) => s.id === p.current)?.name === SIMPLE_NAME);
  const open = async (s: Space) => {
    if (s.name !== SIMPLE_NAME) return p.onPick("", s.name, s.id);
    setBusy(true);
    setErr("");
    try {
      setInside({ space: s, people: await invoke<[string, string][]>("space_chats", { space: s.id }) });
    } catch (e) {
      setErr(String(e));
    }
    setBusy(false);
  };
  const row = (icon: JSX.Element, name: string, onClick: () => void) => (
    <button class="row chat-row" disabled={busy()} onClick={onClick}>{icon}<b class="ellipsis">{name}</b></button>
  );
  const group = (users?: boolean) => <div class="avatar group small-av">{users ? <Users size={18} /> : <Hash size={18} />}</div>;
  return (
    <div class="dialog-list">
      <Show when={inside()} fallback={<>
        <Show when={general()}>{row(group(), "Общий чат", () => p.onPick("", "Общий чат"))}</Show>
        <For each={p.people}>{(m) => row(p.avatar(m.id), m.name, () => p.onPick(m.id, m.name))}</For>
        <For each={others()}>{(s) => row(group(s.name === SIMPLE_NAME), s.name, () => open(s))}</For>
      </>}>
        {(i) => (
          <>
            <Show when={!simple()}>{row(group(), `${i().space.name}: общий чат`, () => p.onPick("", i().space.name, i().space.id))}</Show>
            <For each={i().people} fallback={<p class="muted small">Здесь пока нет контактов</p>}>
              {([id, name]) => row(<div class="avatar small-av group">{name[0]}</div>, name, () => p.onPick(id, name, i().space.id))}
            </For>
          </>
        )}
      </Show>
      <Show when={err()}><p class="error small">{err()}</p></Show>
    </div>
  );
}
