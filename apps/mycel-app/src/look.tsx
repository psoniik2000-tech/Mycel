// Settings → «Оформление»: chat wallpaper (one per light/dark theme) and message text size.
// Both live in localStorage like the theme and reach the page as CSS variables.
import { createSignal, For, onCleanup } from "solid-js";
import { CheckCheck, ImagePlus } from "lucide-solid";

type Side = "light" | "dark";

const tile = (size: number, body: string) =>
  `url("data:image/svg+xml,${encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}">${body}</svg>`)}")`;
const dots = (c: string) => tile(22, `<g fill="${c}"><circle cx="5" cy="5" r="1.6"/><circle cx="16" cy="16" r="1.6"/></g>`);
const hyphae = (c: string) => tile(120,
  `<g fill="none" stroke="${c}" stroke-width="1.4" stroke-linecap="round"><path d="M8 20q22 4 30 24t30 22q16 2 22 18M38 44q2 18-14 28M68 66q4-20 24-28t18-28M92 38l20 12M24 72l-4 24 14 16M60 8q-6 14 8 22"/></g>` +
  `<g fill="${c}"><circle cx="8" cy="20" r="2.5"/><circle cx="90" cy="84" r="2.5"/><circle cx="110" cy="10" r="2.5"/><circle cx="68" cy="30" r="2.5"/><circle cx="34" cy="112" r="2.5"/></g>`);

// [id, name, light, dark]: CSS `background` values.
const WALLS: [string, string, string, string][] = [
  ["", "Стандартные", "var(--chat-bg)", "var(--chat-bg)"],
  ["sage", "Шалфей", "#d5e0cd", "#1a2a1f"],
  ["sky", "Небо", "#cfdfea", "#142739"],
  ["dawn", "Рассвет", "linear-gradient(160deg, #fbe3c0, #f2c4bd 55%, #cdbfe0)", "linear-gradient(160deg, #3d2b17, #3b1c29 55%, #211e45)"],
  ["mint", "Мята", "linear-gradient(160deg, #d9edbf, #a9dcc9 50%, #b4d3ea)", "linear-gradient(160deg, #1d3826, #10363a 50%, #172844)"],
  ["lilac", "Сирень", "linear-gradient(200deg, #e3d4f2, #c8d3f5 50%, #f3d3e4)", "linear-gradient(200deg, #33234e, #1a2750 50%, #3c1b2e)"],
  ["dots", "Точки", `${dots("#8a6a3040")}, #efe6d4`, `${dots("#ffffff2e")}, #15181f`],
  ["mycel", "Грибница", `${hyphae("#2f5d3a38")}, linear-gradient(160deg, #e6ecc9, #c5dfc4)`, `${hyphae("#a8e0b833")}, linear-gradient(160deg, #14241d, #1f1a2e)`],
];

const css = (side: Side, v: string) => {
  // A photo of the user's own is stored as a data: URL, dimmed under the dark theme.
  if (v.startsWith("data:")) return `${side === "dark" ? "linear-gradient(#0008, #0008), " : ""}url("${v}") center / cover`;
  return (WALLS.find((w) => w[0] === v) ?? WALLS[0])[side === "dark" ? 3 : 2];
};

const [walls, setWalls] = createSignal({ light: localStorage.getItem("wall_light") ?? "", dark: localStorage.getItem("wall_dark") ?? "" });
const setWall = (side: Side, v: string) => {
  localStorage.setItem(`wall_${side}`, v); // first: a photo may not fit the quota
  document.documentElement.style.setProperty(`--wall-${side}`, css(side, v));
  setWalls({ ...walls(), [side]: v });
};

const clamp = (n: number) => Math.min(24, Math.max(12, n || 16));
export const [msgSize, setSize] = createSignal(clamp(Number(localStorage.getItem("msg_size"))));
const setMsgSize = (n: number) => {
  localStorage.setItem("msg_size", String(n));
  document.documentElement.style.setProperty("--msg-size", `${n}px`);
  setSize(n);
};

document.documentElement.style.setProperty("--msg-size", `${msgSize()}px`);
for (const s of ["light", "dark"] as const) document.documentElement.style.setProperty(`--wall-${s}`, css(s, walls()[s]));

/** The picked file scaled down to a JPEG data: URL small enough for localStorage. */
const shrink = async (f: File) => {
  const img = await createImageBitmap(f);
  const k = Math.min(1, 1600 / Math.max(img.width, img.height));
  const c = document.createElement("canvas");
  c.width = Math.round(img.width * k);
  c.height = Math.round(img.height * k);
  c.getContext("2d")!.drawImage(img, 0, 0, c.width, c.height);
  img.close();
  return c.toDataURL("image/jpeg", 0.8);
};

export function Appearance(p: { theme: string }) {
  const media = matchMedia("(prefers-color-scheme: dark)");
  const [sysDark, setSysDark] = createSignal(media.matches);
  const watch = () => setSysDark(media.matches);
  media.addEventListener("change", watch);
  onCleanup(() => media.removeEventListener("change", watch));
  // The wallpaper picked here is the one of the theme on screen.
  const side = (): Side => (p.theme === "dark" || (p.theme === "system" && sysDark()) ? "dark" : "light");
  const photo = () => walls()[side()].startsWith("data:");
  const [err, setErr] = createSignal("");
  const pick = async (input: HTMLInputElement) => {
    const f = input.files?.[0], s = side();
    input.value = ""; // the same file can be picked again
    if (!f) return;
    setErr("");
    try {
      setWall(s, await shrink(f));
    } catch {
      setErr("Не удалось поставить это фото: формат не читается или не хватает места");
    }
  };
  let file!: HTMLInputElement;
  return (
    <>
      <h3>Размер текста сообщений</h3>
      <div class="look-preview" aria-hidden="true">
        <div class="msg tail"><div class="bubble"><span class="text">Как тебе новые обои?</span><span class="meta">12:30</span></div></div>
        <div class="msg mine tail"><div class="bubble"><span class="text">Отлично, и читать удобно 👍</span><span class="meta">12:31 <CheckCheck size={14} /></span></div></div>
      </div>
      <div class="size-row">
        <input type="range" min="12" max="24" step="1" value={msgSize()} aria-label="Размер текста сообщений"
          onInput={(e) => setMsgSize(Number(e.currentTarget.value))} />
        <output>{msgSize()}</output>
      </div>
      <h3>Обои чата · {side() === "dark" ? "тёмная тема" : "светлая тема"}</h3>
      <div class="walls">
        <For each={WALLS}>
          {([id, name]) => (
            <button title={name} aria-label={name} aria-pressed={walls()[side()] === id} style={{ background: css(side(), id) }}
              onClick={() => (setErr(""), setWall(side(), id))} />
          )}
        </For>
        <button class="own" aria-pressed={photo()} style={photo() ? { background: css(side(), walls()[side()]) } : {}} onClick={() => file.click()}>
          <ImagePlus size={20} /> Своё фото
        </button>
      </div>
      <input ref={file} type="file" accept="image/*" hidden onChange={(e) => pick(e.currentTarget)} />
      {err() && <p class="error">{err()}</p>}
    </>
  );
}
