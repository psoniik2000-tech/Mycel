// Stickers: pictures shipped with the app (public/stickers, built by scripts/stickers.py). A message
// carries the id `<set>/<name>`; its text is the sticker's emoji, shown when the picture is missing.
import { createSignal, For, JSX, Show } from "solid-js";
import { emojiFile } from "./emoji";
import { rich } from "./rich";
import { STICKERS } from "./sticker-sets";

const idOf = (e: string) => `f3d/${emojiFile(e.replace(/\uFE0F/g, ""))}`;
const url = (id: string) => `/stickers/${id}.webp`;

export function Sticker(p: { id: string; emoji: string }) {
  const [missing, setMissing] = createSignal(false);
  return (
    <span class="sticker-art">
      <Show when={!missing()} fallback={<span>{rich(p.emoji)}</span>}>
        <img class="sticker-img" src={url(p.id)} alt={p.emoji} draggable={false} onError={() => setMissing(true)} />
      </Show>
    </span>
  );
}

// Which half of the picker and which set are open, kept while the app runs.
const [stickers, setStickers] = createSignal(false);
const [tab, setTab] = createSignal(0);

/** The composer's picker: the emoji panel (`children`) or the stickers, with the switch below. */
export function Picker(p: { send: (id: string, emoji: string) => void; children: JSX.Element }) {
  return (
    <>
      <Show when={stickers()} fallback={p.children}>
        <div class="emoji-tabs">
          <For each={STICKERS}>{([title, list], i) => (
            <button classList={{ active: tab() === i() }} title={title} onClick={() => setTab(i())}>
              <img class="emoji" src={url(idOf(list.split(" ")[0]))} alt={title} />
            </button>
          )}</For>
        </div>
        <div class="emoji-grid sticker-grid">
          <For each={STICKERS[tab()][1].split(" ")}>{(e) => (
            <button title={e} onClick={() => p.send(idOf(e), e)}><img src={url(idOf(e))} alt={e} loading="lazy" draggable={false} /></button>
          )}</For>
        </div>
      </Show>
      <div class="emoji-tabs picker-modes">
        <button classList={{ active: !stickers() }} onClick={() => setStickers(false)}>Эмодзи</button>
        <button classList={{ active: stickers() }} onClick={() => setStickers(true)}>Стикеры</button>
      </div>
    </>
  );
}
