// Media bubbles (§8.2): photo, video, voice, video note, file; downloads with progress (§8.3).
import { createSignal, For, Match, onMount, Show, Switch } from "solid-js";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import { decode } from "blurhash";
import { Download, ExternalLink, FileText, Pause, Play, Save } from "lucide-solid";

export type Media = {
  kind: "file" | "photo" | "video" | "voice" | "videonote";
  name: string | null;
  mime: string;
  size: number;
  blurhash: string | null;
  wave: number[] | null;
  duration_ms: number | null;
  dims: [number, number] | null;
  /** Shared by the media of one album. */
  album: string | null;
  path: string | null;
};

const [progress, setProgress] = createSignal<Record<string, number>>({});
listen<{ id: string; done: number }>("progress", (e) => setProgress((p) => ({ ...p, [e.payload.id]: e.payload.done })));

// One download per message at a time: its bubble and the viewer may both ask.
const running: Record<string, Promise<string> | undefined> = {};
// Paths got since the history was loaded.
const got: Record<string, string | undefined> = {};
const download = (id: string) =>
  (running[id] ??= invoke<string>("fetch_media", { id }).then((p) => (got[id] = p)).finally(() => delete running[id]));

const android = /Android/.test(navigator.userAgent);

/** "Save": on Android into Pictures/Movies/Downloads (MediaStore); on desktop through the save dialog. */
export async function saveAs(id: string, m: Media) {
  if (android) {
    const where = await invoke<string>("save_download", { id });
    alert(`Сохранено в «${where}»`);
    return;
  }
  // Photos from a phone gallery may come without a name; Android needs one with an extension.
  const to = await save({ defaultPath: m.name ?? `mycel.${m.mime.split(/[/;]/)[1] ?? "bin"}` });
  if (to) await invoke("save_media", { id, to });
}

export const size = (n: number) =>
  n < 1024 ? `${n} Б` : n < 1 << 20 ? `${(n / 1024).toFixed(0)} КБ` : n < 1 << 30 ? `${(n / (1 << 20)).toFixed(1)} МБ` : `${(n / (1 << 30)).toFixed(2)} ГБ`;
const clock = (ms: number) => `${Math.floor(ms / 60000)}:${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}`;
export const label = (m: Media) =>
  ({ file: `📎 ${m.name ?? "Файл"}`, photo: "🖼 Фото", video: "🎬 Видео", voice: "🎤 Голосовое", videonote: "📹 Видеосообщение" })[m.kind];

function blurUrl(hash: string, w: number, h: number) {
  const cw = 32, ch = Math.max(1, Math.round((32 * h) / w));
  const c = document.createElement("canvas");
  c.width = cw;
  c.height = ch;
  const ctx = c.getContext("2d")!;
  const img = ctx.createImageData(cw, ch);
  img.data.set(decode(hash, cw, ch));
  ctx.putImageData(img, 0, 0);
  return c.toDataURL();
}

/** A thin spinning ring; the arc grows with the download. */
const Ring = (p: { pct: number }) => (
  <svg class="ring" viewBox="0 0 36 36"><circle cx="18" cy="18" r="15" pathLength="100" stroke-dasharray={`${Math.max(8, p.pct)} 100`} /></svg>
);

/** The file of a media message: downloads it (at once when `auto`) and shows the progress. */
export function useMedia(id: string, m: Media, auto: boolean) {
  const [path, setPath] = createSignal(m.path ?? got[id] ?? null);
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal("");
  const src = () => (path() ? convertFileSrc(path()!) : "");
  const fetch = async () => {
    if (busy()) return;
    setBusy(true);
    setErr("");
    try {
      setPath(await download(id));
    } catch (e) {
      setErr("Не удалось скачать: никого с файлом нет на связи");
      console.warn(e);
    }
    setBusy(false);
  };
  onMount(() => {
    if (!path() && auto) fetch();
  });
  const pct = () => Math.min(100, Math.round(((progress()[id] ?? 0) / Math.max(1, m.size)) * 100));
  // Telegram-style: a spinning ring while downloading, else a download button.
  const dlIcon = () => (
    <Show when={busy()} fallback={<button class="icon" onClick={(e) => (e.stopPropagation(), fetch())} title="Скачать"><Download size={18} /></button>}>
      <Ring pct={pct()} />
    </Show>
  );
  const status = () => <span class="muted small">{busy() ? `${pct()}% из ${size(m.size)}` : size(m.size)}</span>;
  const waiting = () => <div class="dl">{dlIcon()}{status()}</div>;
  return { path, src, err, waiting, dlIcon, status, busy, pct, fetch };
}

/** `tile`: a cell of an album grid, which sets the size. `onView` opens the full-screen viewer. */
export function MediaView(p: { id: string; m: Media; onView: () => void; tile?: boolean }) {
  // §8.3: photos and voice always, video and files up to 20 MB.
  const { path, src, err, waiting, dlIcon, status, busy, pct, fetch } = useMedia(p.id, p.m, ["photo", "voice", "videonote"].includes(p.m.kind) || p.m.size <= 20 << 20);
  const [w, h] = p.m.dims ?? [4, 3];
  // Not here yet: the blurred preview (or a grey box) of the right shape, the ring in the middle.
  const placeholder = () => (
    <div class="photo" style={p.tile ? {} : { "aspect-ratio": `${w} / ${h}` }}>
      <Show when={p.m.blurhash}><img class="blur" src={blurUrl(p.m.blurhash!, w, h)} alt="" /></Show>
      <div class="over">{waiting()}</div>
    </div>
  );
  return (
    <div class="media">
      <Switch>
        <Match when={p.m.kind === "photo"}>
          <Show when={path()} fallback={placeholder()}>
            <div class="photo" style={p.tile ? {} : { "aspect-ratio": `${w} / ${h}` }}>
              <img src={src()} alt="Фото" onClick={p.onView} />
            </div>
          </Show>
        </Match>
        <Match when={p.m.kind === "video"}>
          <Show when={path()} fallback={placeholder()}>
            {/* #t: Android's WebView shows no first frame without it. */}
            <button class="video" onClick={p.onView} title="Смотреть видео">
              <video src={`${src()}#t=0.1`} preload="metadata" muted playsinline />
              <span class="playbtn"><Play size={22} /></span>
            </button>
          </Show>
        </Match>
        <Match when={p.m.kind === "voice"}>
          <Voice src={src()} wave={p.m.wave ?? []} ms={p.m.duration_ms ?? 0} ready={!!path()} busy={busy()} pct={pct()} fetch={fetch} />
        </Match>
        <Match when={p.m.kind === "videonote"}>
          <Show when={path()} fallback={<div class="circle empty">{waiting()}</div>}>
            <Circle src={src()} />
          </Show>
        </Match>
        <Match when={p.m.kind === "file"}>
          <div class="file">
            <Show when={path()} fallback={dlIcon()}><FileText size={28} /></Show>
            <div class="grow">
              <div class="ellipsis" title={p.m.name ?? ""}>{p.m.name ?? "Файл"}</div>
              <Show when={path()} fallback={status()}>
                <div class="actions">
                  <span class="muted small">{size(p.m.size)}</span>
                  <button class="link" onClick={() => invoke("open_media", { id: p.id })}><ExternalLink size={14} /> Открыть</button>
                  <button class="link" onClick={() => saveAs(p.id, p.m)}><Save size={14} /> {android ? "Сохранить" : "Сохранить как"}</button>
                </div>
              </Show>
            </div>
          </div>
        </Match>
      </Switch>
      <Show when={err()}><p class="error small">{err()}</p></Show>
    </div>
  );
}

function Voice(p: { src: string; wave: number[]; ms: number; ready: boolean; busy: boolean; pct: number; fetch: () => void }) {
  let audio!: HTMLAudioElement;
  const [playing, setPlaying] = createSignal(false);
  const [pos, setPos] = createSignal(0);
  const [rate, setRate] = createSignal(1);
  const [dur, setDur] = createSignal(0);
  // The recorded length; a WebM without it in its header reports Infinity (or junk while loading).
  const len = () => p.ms || dur();
  const bars = () => (p.wave.length ? p.wave : Array(64).fill(40));
  const toggle = () => (audio.paused ? audio.play() : audio.pause());
  const seek = (e: MouseEvent) => {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (len()) { audio.currentTime = (((e.clientX - r.left) / r.width) * len()) / 1000; setPos(audio.currentTime * 1000); }
  };
  return (
    <div class="voice">
      <button class="icon play" onClick={() => (p.ready ? toggle() : p.fetch())} title={p.ready ? undefined : "Скачать"}>
        {!p.ready ? (p.busy ? <Ring pct={p.pct} /> : <Download size={18} />) : playing() ? <Pause size={18} /> : <Play size={18} />}
      </button>
      <div class="wave" onClick={seek}>
        <For each={bars()}>
          {(v, i) => <i style={{ height: `${10 + (v / 255) * 90}%` }} classList={{ done: (i() + 0.5) / bars().length <= pos() / Math.max(1, len()) }} />}
        </For>
      </div>
      <span class="muted small">{clock(playing() || pos() ? Math.min(pos(), len() || pos()) : len())}</span>
      <button class="link rate" onClick={() => { const r = rate() === 1 ? 1.5 : rate() === 1.5 ? 2 : 1; setRate(r); audio.playbackRate = r; }}>{rate()}×</button>
      {/* Keeps the position on pause: playback continues from where it stopped. */}
      <audio ref={audio} src={p.src} preload="auto"
        onPlay={() => { audio.playbackRate = rate(); setPlaying(true); }}
        onPause={() => setPlaying(false)}
        onEnded={() => {
          setPlaying(false);
          setPos(0);
          // As in Telegram: the next voice message in the chat plays on.
          const all = [...document.querySelectorAll<HTMLAudioElement>(".voice audio")];
          all[all.indexOf(audio) + 1]?.play().catch(() => {});
        }}
        onDurationChange={() => isFinite(audio.duration) && setDur(audio.duration * 1000)}
        onTimeUpdate={() => !audio.paused && setPos(audio.currentTime * 1000)} />
    </div>
  );
}

function Circle(p: { src: string }) {
  let v!: HTMLVideoElement;
  // Autoplays muted in a loop (§8.2); a click plays it with sound from the start.
  const click = () => {
    if (v.muted) {
      v.muted = false;
      v.loop = false;
      v.currentTime = 0;
      v.play();
    } else v.paused ? v.play() : v.pause();
  };
  return <video ref={v} class="circle" src={p.src} autoplay muted loop playsinline onClick={click} onEnded={() => { v.muted = true; v.loop = true; v.play(); }} />;
}
