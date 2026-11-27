// Albums: media sent together (one `album` id) share a bubble and a Telegram-like grid.
import { For } from "solid-js";
import { Media, MediaView } from "./media";

type Item = { id: string; author: string; media: Media | null; call: unknown };

/** One bubble per album: an author's media with one album id, up to 10, until that author sends
 * something else (others' messages may slip in between). `list` keeps the first of each. */
export function albums<T extends Item>(msgs: T[]) {
  const list: T[] = [], items = new Map<string, T[]>(), open = new Map<string, T[]>(); // open: by author
  for (const m of msgs) {
    if (!m.call) {
      const id = m.media?.album, group = open.get(m.author);
      if (id && group && group[0].media!.album === id && group.length < 10) {
        group.push(m);
        continue;
      }
      open.delete(m.author);
      if (id) {
        const g = [m];
        open.set(m.author, g);
        items.set(m.id, g);
      }
    }
    list.push(m);
  }
  return { list, items };
}

// Tiles per row for 1…10 items.
const ROWS = [[], [1], [2], [1, 2], [1, 3], [2, 3], [3, 3], [2, 2, 3], [2, 3, 3], [3, 3, 3], [3, 3, 4]];
/** How many tiles share the row of tile `i` among `n`. */
const inRow = (n: number, i: number) => ROWS[n].find((r) => (i -= r) < 0)!;

export function Album(p: { items: Item[]; onView: (id: string) => void }) {
  return (
    <div class="album">
      <For each={p.items}>
        {(m, i) => {
          const row = () => inRow(p.items.length, i());
          return (
            <div style={{ "grid-column": `span ${12 / row()}`, "aspect-ratio": row() === 1 ? "1.6" : "1" }}>
              <MediaView tile id={m.id} m={m.media!} onView={() => p.onView(m.id)} />
            </div>
          );
        }}
      </For>
    </div>
  );
}
