// Full-screen photos and videos of a chat: swipe or arrows between them, pinch / wheel / double
// tap to zoom a photo, swipe down or Esc to close.
import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { ArrowLeft, ChevronLeft, ChevronRight, ExternalLink, Save } from "lucide-solid";
import { Media, saveAs, useMedia } from "./media";

export type Shot = { id: string; author: string; ts: number; text: string; media: Media };

const FIT = { s: 1, x: 0, y: 0 };
type Point = { x: number; y: number };

export function Viewer(p: { shots: Shot[]; at: string; onAt: (id: string) => void; onClose: () => void; name: (author: string) => string }) {
  const ix = () => p.shots.findIndex((s) => s.id === p.at);
  const cur = () => p.shots[ix()];
  // The current one and its neighbours, by id: a reloaded message must not restart its video.
  const near = () => p.shots.slice(Math.max(0, ix() - 1), ix() + 2).map((s) => s.id);
  // An album has one caption, on one of its messages.
  const caption = () => {
    const c = cur();
    if (!c || c.text) return c?.text;
    const a = c.media.album;
    return a ? p.shots.find((s) => s.author === c.author && s.media.album === a && s.text)?.text : "";
  };
  const [zoom, setZoom] = createSignal(FIT);
  const [drag, setDrag] = createSignal({ x: 0, y: 0 }); // a swipe in progress, at zoom 1
  const [moving, setMoving] = createSignal(false);
  const [err, setErr] = createSignal("");
  let stage!: HTMLDivElement;

  // Closing goes through history: Android's back button closes the viewer, not the chat under it.
  let popped = false, closing = false;
  const swallow = (e: Event) => e.stopImmediatePropagation();
  const onPop = (e: Event) => (swallow(e), (popped = true), p.onClose());
  const close = () => {
    if (!closing) history.back();
    closing = true;
  };

  const go = (d: number) => {
    const s = p.shots[ix() + d];
    if (s) setZoom(FIT), setErr(""), p.onAt(s.id);
  };
  const clamp = (s: number, x: number, y: number) => {
    const w = (stage.clientWidth * (s - 1)) / 2, h = (stage.clientHeight * (s - 1)) / 2;
    return { s, x: Math.max(-w, Math.min(w, x)), y: Math.max(-h, Math.min(h, y)) };
  };
  // Zooms by `k`, keeping the screen point `at` in place; `pan` moves along.
  const zoomAt = (k: number, at: Point, pan: Point = { x: 0, y: 0 }) => {
    if (cur()?.media.kind !== "photo") return;
    const r = stage.getBoundingClientRect(), z = zoom();
    const s = Math.max(1, Math.min(5, z.s * k)), f = s / z.s;
    const px = at.x - r.left - r.width / 2, py = at.y - r.top - r.height / 2;
    setZoom(clamp(s, px - (px - z.x) * f + pan.x, py - (py - z.y) * f + pan.y));
  };

  const points = new Map<number, Point>();
  let start: Point = { x: 0, y: 0 }, axis: "x" | "y" | undefined, moved = false, pinched = false, backdrop = false, tapped = 0;
  const down = (e: PointerEvent) => {
    const t = e.target as Element;
    if (e.button > 0 || t.closest("button")) return;
    // The bottom of a video is its own controls (seeking).
    if (t.tagName === "VIDEO" && e.clientY > t.getBoundingClientRect().bottom - 70) return;
    points.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (points.size === 1) {
      start = { x: e.clientX, y: e.clientY };
      axis = undefined;
      moved = pinched = false;
      backdrop = t.classList.contains("slide");
    } else moved = pinched = true;
    setMoving(true);
  };
  const move = (e: PointerEvent) => {
    const last = points.get(e.pointerId);
    if (!last) return;
    // The button was released outside the window before the pointer got captured.
    if (e.pointerType === "mouse" && !e.buttons) return up(e);
    const now = { x: e.clientX, y: e.clientY };
    if (points.size > 1) {
      const o = [...points].find(([id]) => id !== e.pointerId)![1];
      setDrag({ x: 0, y: 0 });
      const k = Math.hypot(now.x - o.x, now.y - o.y) / Math.max(1, Math.hypot(last.x - o.x, last.y - o.y));
      zoomAt(k, { x: (last.x + o.x) / 2, y: (last.y + o.y) / 2 }, { x: (now.x - last.x) / 2, y: (now.y - last.y) / 2 });
      points.set(e.pointerId, now);
      return;
    }
    const dx = now.x - start.x, dy = now.y - start.y;
    if (!moved && Math.hypot(dx, dy) < 10) return;
    // Captured only once it is a drag: a plain click must still reach a video.
    if (!stage.hasPointerCapture(e.pointerId)) stage.setPointerCapture(e.pointerId);
    moved = true;
    const z = zoom();
    if (z.s > 1) setZoom(clamp(z.s, z.x + now.x - last.x, z.y + now.y - last.y));
    else if (!pinched) {
      axis ??= Math.abs(dx) > Math.abs(dy) ? "x" : "y";
      setDrag(axis === "x" ? { x: dx, y: 0 } : { x: 0, y: Math.max(0, dy) });
    }
    points.set(e.pointerId, now);
  };
  const up = (e: PointerEvent) => {
    if (!points.delete(e.pointerId) || points.size) return;
    setMoving(false);
    const d = drag();
    setDrag({ x: 0, y: 0 });
    if (e.type === "pointercancel") return;
    if (d.y > 100) return close();
    if (Math.abs(d.x) > 60) return go(d.x < 0 ? 1 : -1);
    if (moved) return;
    if (backdrop && e.pointerType === "mouse") return close();
    // Counted by hand, as in the chat: not every WebView sends dblclick for taps.
    const double = e.timeStamp - tapped < 300;
    tapped = double ? 0 : e.timeStamp;
    if (double) zoom().s > 1 ? setZoom(FIT) : zoomAt(2, { x: e.clientX, y: e.clientY });
  };

  const key = (e: KeyboardEvent) => {
    if (e.key === "Escape") close();
    else if (e.key === "ArrowLeft") go(-1);
    else if (e.key === "ArrowRight") go(1);
    else return;
    e.preventDefault();
  };
  onMount(() => {
    (document.activeElement as HTMLElement | null)?.blur(); // Enter must not reach the message field
    history.pushState(null, "");
    addEventListener("popstate", onPop, { capture: true, once: true });
    addEventListener("keydown", key);
  });
  onCleanup(() => {
    removeEventListener("keydown", key);
    if (popped) return;
    // Removed some other way (the chat changed): still take our history entry back, silently.
    removeEventListener("popstate", onPop, true);
    addEventListener("popstate", swallow, { capture: true, once: true });
    if (!closing) history.back();
  });
  // The message was deleted meanwhile.
  createEffect(() => ix() < 0 && close());

  return (
    <div class="viewer" role="dialog" aria-label="Просмотр" style={{ background: `rgb(0 0 0 / ${Math.max(0.3, 1 - drag().y / 400) * 94}%)` }}>
      <div class="stage" ref={stage} onPointerDown={down} onPointerMove={move} onPointerUp={up} onPointerCancel={up}
        on:wheel={(e) => (e.preventDefault(), zoomAt(Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.002)), { x: e.clientX, y: e.clientY }))}>
        <For each={near()}>
          {(id) => {
            const i = () => p.shots.findIndex((s) => s.id === id);
            const shot = p.shots[i()];
            return (
              <div class="slide" classList={{ moving: moving(), current: id === p.at }}
                style={{ transform: `translate(calc(${(i() - ix()) * 100}% + ${drag().x}px), ${drag().y}px)` }}>
                <Slide shot={shot} current={id === p.at} zoom={id === p.at ? zoom() : FIT} moving={moving()} />
              </div>
            );
          }}
        </For>
      </div>
      <Show when={cur()}>
        {(c) => (
          <>
            <header>
              <button class="icon" onClick={close} title="Закрыть (Esc)"><ArrowLeft size={22} /></button>
              <div class="grow">
                <b class="ellipsis">{p.name(c().author)}</b>
                <small>{new Date(c().ts).toLocaleString("ru", { day: "numeric", month: "long", hour: "2-digit", minute: "2-digit" })}</small>
              </div>
              <span>{ix() + 1} из {p.shots.length}</span>
              <button class="icon" title="Открыть" onClick={() => (setErr(""), invoke("open_media", { id: c().id }).catch((e) => setErr(`Не удалось открыть: ${e}`)))}><ExternalLink size={22} /></button>
              <button class="icon" title="Сохранить"
                onClick={() => (setErr(""), saveAs(c().id, c().media).catch((e) => setErr(`Не удалось сохранить: ${e}`)))}><Save size={22} /></button>
            </header>
            <Show when={ix() > 0}><button class="icon nav-btn prev" onClick={() => go(-1)} title="Раньше (←)"><ChevronLeft size={28} /></button></Show>
            <Show when={ix() < p.shots.length - 1}><button class="icon nav-btn next" onClick={() => go(1)} title="Позже (→)"><ChevronRight size={28} /></button></Show>
            <Show when={err() || caption()}><footer>{err() || caption()}</footer></Show>
          </>
        )}
      </Show>
    </div>
  );
}

function Slide(p: { shot: Shot; current: boolean; zoom: typeof FIT; moving: boolean }) {
  const m = p.shot.media;
  const { path, src, err, waiting } = useMedia(p.shot.id, m, m.kind === "photo" || m.size <= 20 << 20);
  let video: HTMLVideoElement | undefined;
  createEffect(() => {
    if (!path()) return;
    if (p.current) video?.play().catch(() => {});
    else video?.pause();
  });
  return (
    <>
      <Show when={path()} fallback={waiting()}>
        <Show when={m.kind === "photo"} fallback={<video ref={video} src={src()} controls playsinline preload="metadata" />}>
          <img src={src()} alt="Фото" draggable={false} classList={{ moving: p.moving }}
            style={{ transform: `translate(${p.zoom.x}px, ${p.zoom.y}px) scale(${p.zoom.s})` }} />
        </Show>
      </Show>
      <Show when={err()}><p class="error">{err()}</p></Show>
    </>
  );
}
