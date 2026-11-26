// Media bubbles (§8.2): photo, video, voice, video note, file; downloads with progress (§8.3).
import { createSignal, For, Match, onMount, Show, Switch } from "solid-js";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import { decode } from "blurhash";
import { Download, ExternalLink, FileText, Film, Pause, Play, Save } from "lucide-solid";

export type Media = {
  kind: "file" | "photo" | "video" | "voice" | "videonote";
  name: string | null;
  mime: string;
  size: number;
  blurhash: string | null;
  wave: number[] | null;
  duration_ms: number | null;
  dims: [number, number] | null;
  path: string | null;
};

const [progress, setProgress] = createSignal<Record<string, number>>({});
listen<{ id: string; done: number }>("progress", (e) => setProgress((p) => ({ ...p, [e.payload.id]: e.payload.done })));

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

export function MediaView(p: { id: string; m: Media; onView: (src: string) => void }) {
  const [path, setPath] = createSignal(p.m.path);
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal("");
  const src = () => (path() ? convertFileSrc(path()!) : "");
  const fetch = async () => {
    if (busy()) return;
    setBusy(true);
    setErr("");
    try {
      setPath(await invoke<string>("fetch_media", { id: p.id }));
    } catch (e) {
      setErr("Не удалось скачать: никого с файлом нет на связи");
      console.warn(e);
    }
    setBusy(false);
  };
  // §8.3: photos and voice always, video and files up to 20 MB.
  onMount(() => {
    const auto = ["photo", "voice", "videonote"].includes(p.m.kind) || p.m.size <= 20 << 20;
    if (!path() && auto) fetch();
  });
  const pct = () => Math.min(100, Math.round(((progress()[p.id] ?? 0) / Math.max(1, p.m.size)) * 100));
  const waiting = () => (
    <div class="dl">
      <Show when={busy()} fallback={<button class="icon" onClick={fetch} title="Скачать"><Download size={18} /></button>}>
        <div class="ring" style={{ "--p": pct() }} />
      </Show>
      <span class="muted small">{busy() ? `${pct()}% из ${size(p.m.size)}` : size(p.m.size)}</span>
    </div>
  );
  const [w, h] = p.m.dims ?? [4, 3];
  return (
    <div class="media">
      <Switch>
        <Match when={p.m.kind === "photo"}>
          <div class="photo" style={{ "aspect-ratio": `${w} / ${h}` }}>
            <Show when={path()} fallback={
              <>
                <Show when={p.m.blurhash}><img class="blur" src={blurUrl(p.m.blurhash!, w, h)} alt="" /></Show>
                <div class="over">{waiting()}</div>
              </>
            }>
              <img src={src()} alt="Фото" onClick={() => p.onView(src())} />
            </Show>
          </div>
        </Match>
        <Match when={p.m.kind === "video"}>
          <Show when={path()} fallback={<div class="file"><Film size={28} /><div class="grow"><div class="ellipsis">{p.m.name ?? "Видео"}</div>{waiting()}</div></div>}>
            <video class="video" src={src()} controls preload="metadata" />
          </Show>
        </Match>
        <Match when={p.m.kind === "voice"}>
          <Voice src={src()} wave={p.m.wave ?? []} ms={p.m.duration_ms ?? 0} ready={!!path()} />
        </Match>
        <Match when={p.m.kind === "videonote"}>
          <Show when={path()} fallback={<div class="circle empty">{waiting()}</div>}>
            <Circle src={src()} />
          </Show>
        </Match>
        <Match when={p.m.kind === "file"}>
          <div class="file">
            <FileText size={28} />
            <div class="grow">
              <div class="ellipsis" title={p.m.name ?? ""}>{p.m.name ?? "Файл"}</div>
              <Show when={path()} fallback={waiting()}>
                <div class="actions">
                  <span class="muted small">{size(p.m.size)}</span>
                  <button class="link" onClick={() => invoke("open_media", { id: p.id })}><ExternalLink size={14} /> Открыть</button>
                  <button class="link" onClick={async () => {
                    const to = await save({ defaultPath: p.m.name ?? undefined });
                    if (to) invoke("save_media", { id: p.id, to });
                  }}><Save size={14} /> Сохранить как</button>
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

function Voice(p: { src: string; wave: number[]; ms: number; ready: boolean }) {
  let audio!: HTMLAudioElement;
  const [playing, setPlaying] = createSignal(false);
  const [pos, setPos] = createSignal(0);
  const [rate, setRate] = createSignal(1);
  const bars = () => (p.wave.length ? p.wave : Array(64).fill(40));
  const toggle = () => (audio.paused ? audio.play() : audio.pause());
  const seek = (e: MouseEvent) => {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (p.ms) audio.currentTime = (((e.clientX - r.left) / r.width) * p.ms) / 1000;
  };
  return (
    <div class="voice">
      <button class="icon play" disabled={!p.ready} onClick={toggle}>{playing() ? <Pause size={18} /> : <Play size={18} />}</button>
      <div class="wave" onClick={seek}>
        <For each={bars()}>
          {(v, i) => <i style={{ height: `${10 + (v / 255) * 90}%` }} classList={{ done: (i() + 0.5) / bars().length <= pos() / Math.max(1, p.ms) }} />}
        </For>
      </div>
      <span class="muted small">{clock(playing() || pos() ? pos() : p.ms)}</span>
      <button class="link rate" onClick={() => { const r = rate() === 1 ? 1.5 : rate() === 1.5 ? 2 : 1; setRate(r); audio.playbackRate = r; }}>{rate()}×</button>
      {/* Keeps the position on pause: playback continues from where it stopped. */}
      <audio ref={audio} src={p.src} preload="auto"
        onPlay={() => { audio.playbackRate = rate(); setPlaying(true); }}
        onPause={() => setPlaying(false)}
        onEnded={() => { setPlaying(false); setPos(0); }}
        onTimeUpdate={() => setPos(audio.currentTime * 1000)} />
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
