// Call media in the WebView (§9): getUserMedia (echo/noise suppression) → WebCodecs Opus/VP8 → Rust
// (encrypts, sends over `mycel/call/1`); incoming frames → decoders → speakers / a canvas per peer.
// Chromium already ships AEC, noise suppression and both codecs, so Rust needs no audio/video stack.
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import { Accessor, createSignal, Setter } from "solid-js";

const AUDIO = 0, VIDEO_KEY = 1, VIDEO_DELTA = 2;
const KEY_EVERY = 48; // frames (~2 s): late joiners and dropped frames recover by the next key frame
const FRAME_US = 20_000; // one Opus frame
const HANGOVER = 400; // ms the voice gate stays open after the level drops

// MediaStreamTrackProcessor is Chromium-only (WebView2), not in TS's DOM lib.
const Processor = (globalThis as any).MediaStreamTrackProcessor;
const frames = <T>(track: MediaStreamTrack): ReadableStreamDefaultReader<T> => new Processor({ track }).readable.getReader();

const send = (kind: number, data: Uint8Array) => invoke("call_send", data, { headers: { kind: String(kind) } }).catch(() => {});
const bytes = (c: EncodedAudioChunk | EncodedVideoChunk) => {
  const b = new Uint8Array(c.byteLength);
  c.copyTo(b);
  return b;
};

/** "Voice & video" settings (Settings tab); saved locally, applied to a running call at once. */
export type Voice = {
  mic: string; out: string; cam: string; // deviceId, "" = system default
  inVol: number; outVol: number; // %, 0..200
  mode: "vad" | "ptt";
  threshold: number; // dB, -100..0: voice activation opens above it
  pttKey: string; // KeyboardEvent.code
  ns: boolean; aec: boolean; agc: boolean;
  kbps: number; // Opus bitrate, 16..128
};
const DEFAULT: Voice = { mic: "", out: "", cam: "", inVol: 100, outVol: 100, mode: "vad", threshold: -55, pttKey: "KeyV", ns: true, aec: true, agc: true, kbps: 64 };
export function loadVoice(): Voice {
  try {
    return { ...DEFAULT, ...JSON.parse(localStorage.getItem("voice") ?? "{}") };
  } catch {
    return { ...DEFAULT };
  }
}
let current: CallMedia | undefined;
export function saveVoice(v: Voice) {
  localStorage.setItem("voice", JSON.stringify(v));
  current?.apply(v);
}

/** Speakers with the chosen device and volume. */
export function speakers(ctx: AudioContext, v: Voice): GainNode {
  const g = ctx.createGain();
  g.gain.value = v.outVol / 100;
  g.connect(ctx.destination);
  (ctx as any).setSinkId?.(v.out).catch(() => {});
  return g;
}

/** Microphone → input gain → level meter. Reopened on device / processing changes. */
export class Mic {
  gain: GainNode;
  private an: AnalyserNode;
  private buf = new Float32Array(1024);
  private raw?: MediaStream;
  private src?: MediaStreamAudioSourceNode;
  constructor(private ctx: AudioContext) {
    this.gain = ctx.createGain();
    this.an = ctx.createAnalyser();
    this.an.fftSize = 1024;
    this.gain.connect(this.an);
  }
  async open(v: Voice) {
    const s = await navigator.mediaDevices.getUserMedia({
      audio: { deviceId: v.mic || undefined, channelCount: 1, sampleRate: 48000, echoCancellation: v.aec, noiseSuppression: v.ns, autoGainControl: v.agc },
    });
    this.close();
    this.raw = s;
    this.src = this.ctx.createMediaStreamSource(s);
    this.src.connect(this.gain);
    this.gain.gain.value = v.inVol / 100;
  }
  /** Current level, dB (-100..0). */
  level(): number {
    this.an.getFloatTimeDomainData(this.buf);
    let sum = 0;
    for (const x of this.buf) sum += x * x;
    return Math.max(-100, 10 * Math.log10(sum / this.buf.length + 1e-12));
  }
  close() {
    this.src?.disconnect();
    this.raw?.getTracks().forEach((t) => t.stop());
  }
}

/** Connection quality to a participant (the signal icon). */
export type Net = { rtt: number; loss: number; kbps: number; relay: boolean };
export type Peer = { id: string; canvas: HTMLCanvasElement; videoAt: number; talkAt: number; net: Accessor<Net | undefined> };

/** A getUserMedia / encoder failure in words; no camera at all is not an error, the call goes on by voice. */
export const camError = (e: unknown) => {
  const n = (e as DOMException)?.name;
  if (n === "NotFoundError" || n === "OverconstrainedError" || n === "NotReadableError") return "Камера не найдена — звонок продолжается голосом";
  if (n === "NotAllowedError") return "Нет доступа к камере — разрешите его в настройках";
  return "Камера не включилась: " + ((e as Error)?.message ?? String(e));
};

type Rx = Peer & {
  setNet: Setter<Net | undefined>;
  adec: AudioDecoder;
  vdec?: VideoDecoder;
  key: boolean;
  lastN: number; // last audio frame number received
  next: number; // jitter buffer: playout time of the next frame
  lastTs: number; // timestamp of the last played frame, µs
  lastBuf?: AudioBuffer; // for loss concealment
  jitter: number; // RFC 3550 interarrival jitter, ms
  transit: number;
  rxBytes: number;
};

export class CallMedia {
  local = new MediaStream();
  mic = true;
  cam = false;
  facing: "user" | "environment" = "user";
  camErr = ""; // why the camera asked for at start did not turn on
  private ctx = new AudioContext({ sampleRate: 48000 });
  private v = loadVoice();
  private input = new Mic(this.ctx);
  private out = speakers(this.ctx, this.v);
  private rx = new Map<string, Rx>();
  private stopped = false;
  private bitrate = 600_000;
  private venc?: VideoEncoder;
  private vcfg?: VideoEncoderConfig;
  private forceKey = false;
  private aenc?: AudioEncoder;
  private kbps = this.v.kbps; // current Opus bitrate: the setting, lowered on loss
  private red = false; // heavy loss: each packet also carries the previous frame
  private ptt = false;
  private unlistenPtt?: UnlistenFn;
  private openUntil = 0;
  private timer = 0;
  private keys = (e: KeyboardEvent) => {
    if (e.code === this.v.pttKey) this.ptt = e.type === "keydown";
  };

  constructor(private onPeers: (p: Peer[]) => void) {}

  async start(video: boolean) {
    current = this;
    const ch = new Channel<ArrayBuffer>();
    ch.onmessage = (b) => this.receive(b);
    await invoke("call_listen", { ch });
    await this.input.open(this.v);
    const dest = this.ctx.createMediaStreamDestination();
    dest.channelCount = 1;
    this.input.gain.connect(dest);
    const t = dest.stream.getAudioTracks()[0];
    this.local.addTrack(t);
    this.pumpAudio(t);
    // Push-to-talk: the window's keys, and on desktop the same key as a global shortcut (Rust
    // `ptt_key`), so it works while another window has the focus.
    addEventListener("keydown", this.keys);
    addEventListener("keyup", this.keys);
    if (this.v.mode === "ptt") {
      this.unlistenPtt = await listen<boolean>("ptt", (e) => (this.ptt = e.payload)).catch(() => undefined);
      await invoke("ptt_key", { key: this.v.pttKey }).catch(() => {});
    }
    if (video) await this.setCam(true).catch((e) => (this.camErr = camError(e)));
    this.timer = window.setInterval(() => this.poll(), 2000);
  }

  stop() {
    this.stopped = true;
    if (current === this) current = undefined;
    clearInterval(this.timer);
    removeEventListener("keydown", this.keys);
    removeEventListener("keyup", this.keys);
    if (this.unlistenPtt) {
      this.unlistenPtt();
      this.unlistenPtt = undefined;
      invoke("ptt_key", { key: null }).catch(() => {});
    }
    this.input.close();
    this.local.getTracks().forEach((t) => t.stop());
    for (const p of this.rx.values()) {
      p.adec.state !== "closed" && p.adec.close();
      p.vdec?.state !== "closed" && p.vdec?.close();
    }
    this.ctx.close();
  }

  /** New settings mid-call: the mic / camera are reopened only if their options changed. */
  async apply(v: Voice) {
    const old = this.v;
    this.v = v;
    this.input.gain.gain.value = v.inVol / 100;
    this.out.gain.value = v.outVol / 100;
    if (v.out !== old.out) (this.ctx as any).setSinkId?.(v.out).catch(() => {});
    if (v.kbps !== old.kbps) this.setKbps(v.kbps);
    if (v.mic !== old.mic || v.ns !== old.ns || v.aec !== old.aec || v.agc !== old.agc) await this.input.open(v).catch(() => {});
    const cam = this.local.getVideoTracks()[0];
    if (v.cam !== old.cam && cam) {
      cam.stop(); // ends its pumpVideo loop
      this.local.removeTrack(cam);
      if (this.cam) await this.setCam(true).catch(() => {});
    }
  }

  setMic(on: boolean) {
    this.mic = on;
    this.local.getAudioTracks().forEach((t) => (t.enabled = on));
  }

  /** Camera on/off; the camera is opened on first use (an audio call can turn it on). */
  async setCam(on: boolean) {
    let t = this.local.getVideoTracks()[0];
    if (on && !t) {
      // A chosen device wins; otherwise the front / back camera (phones).
      const pick = this.v.cam ? { deviceId: this.v.cam } : { facingMode: this.facing };
      const s = await navigator.mediaDevices.getUserMedia({ video: { ...pick, width: 640, height: 480, frameRate: 24 } });
      t = s.getVideoTracks()[0];
      this.local.addTrack(t);
      this.pumpVideo(t);
    }
    this.cam = on;
    if (t) t.enabled = on;
    this.forceKey = on;
  }

  /** Front ⇄ back camera: the old track is stopped (its pump ends) and the other one opened. */
  async flipCam() {
    const old = this.local.getVideoTracks()[0];
    this.facing = this.facing === "user" ? "environment" : "user";
    this.v = { ...this.v, cam: "" }; // this call only: by facing, not the chosen device
    old?.stop();
    if (old) this.local.removeTrack(old);
    try {
      await this.setCam(true);
    } catch (e) {
      this.facing = this.facing === "user" ? "environment" : "user";
      await this.setCam(true).catch(() => {});
      throw e;
    }
  }

  /** A participant hung up: drop its tile and decoders. */
  drop(id: string) {
    const p = this.rx.get(id);
    if (!p) return;
    p.adec.state !== "closed" && p.adec.close();
    p.vdec?.state !== "closed" && p.vdec?.close();
    this.rx.delete(id);
    this.onPeers([...this.rx.values()]);
  }

  /** Voice gate: push-to-talk key held, or the level above the threshold (+ hangover). */
  private talking(): boolean {
    if (this.v.mode === "ptt") return this.ptt;
    const now = performance.now();
    if (this.input.level() >= this.v.threshold) this.openUntil = now + HANGOVER;
    return now < this.openUntil;
  }

  private setKbps(k: number) {
    this.kbps = k;
    if (this.aenc?.state === "configured") this.aenc.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 1, bitrate: k * 1000 });
  }

  // Audio payload: frame number u32 | length u16 | frame [| previous frame, while `red`].
  private async pumpAudio(track: MediaStreamTrack) {
    let n = 0;
    let prev = new Uint8Array(0);
    const enc = (this.aenc = new AudioEncoder({
      output: (c) => {
        const cur = bytes(c);
        const extra = this.red ? prev : new Uint8Array(0);
        const b = new Uint8Array(6 + cur.length + extra.length);
        const dv = new DataView(b.buffer);
        dv.setUint32(0, n++);
        dv.setUint16(4, cur.length);
        b.set(cur, 6);
        b.set(extra, 6 + cur.length);
        prev = cur;
        send(AUDIO, b);
      },
      error: () => {},
    }));
    enc.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 1, bitrate: this.kbps * 1000 });
    const r = frames<AudioData>(track);
    while (!this.stopped) {
      const { value, done } = await r.read();
      if (done) break;
      if (this.mic && enc.state === "configured" && this.talking()) enc.encode(value);
      value.close();
    }
    enc.state !== "closed" && enc.close();
  }

  // Phones: frames come rotated by metadata (portrait) and the hardware VP8 encoder may refuse the
  // stream, so the encoder is sized by the actual frames and falls back to software on an error.
  private async pumpVideo(track: MediaStreamTrack) {
    let soft = false;
    let fails = 0;
    let enc: VideoEncoder | undefined;
    const open = (w: number, h: number) => {
      enc?.state !== "closed" && enc?.close();
      this.vcfg = { codec: "vp8", width: w, height: h, bitrate: this.bitrate, framerate: 24, latencyMode: "realtime", hardwareAcceleration: soft ? "prefer-software" : "no-preference" };
      enc = this.venc = new VideoEncoder({
        output: (c) => send(c.type === "key" ? VIDEO_KEY : VIDEO_DELTA, bytes(c)),
        error: (e) => {
          console.warn("video encoder", e);
          soft = true; // the next frame reopens it in software
          fails++;
        },
      });
      enc.configure(this.vcfg);
      this.forceKey = true;
    };
    const r = frames<VideoFrame>(track);
    let rot: OffscreenCanvasRenderingContext2D | undefined;
    let n = 0;
    while (!this.stopped) {
      const { value, done } = await r.read();
      if (done) break;
      let f = value;
      // Rotated by metadata: draw upright (drawImage applies the rotation), the receiver gets plain pixels.
      if ((value as any).rotation) {
        if (rot?.canvas.width !== value.displayWidth || rot.canvas.height !== value.displayHeight)
          rot = new OffscreenCanvas(value.displayWidth, value.displayHeight).getContext("2d")!;
        rot.drawImage(value, 0, 0, value.displayWidth, value.displayHeight);
        f = new VideoFrame(rot.canvas, { timestamp: value.timestamp });
        value.close();
      }
      if (fails > 2) {
        f.close();
        continue; // no working encoder here: the call goes on without our video
      }
      if (!enc || enc.state === "closed" || this.vcfg?.width !== f.displayWidth || this.vcfg?.height !== f.displayHeight || (soft && this.vcfg.hardwareAcceleration !== "prefer-software"))
        open(f.displayWidth, f.displayHeight);
      // Busy encoder (slow PC): skip a frame rather than build up delay.
      if (this.cam && enc!.state === "configured" && enc!.encodeQueueSize < 2) {
        enc!.encode(f, { keyFrame: this.forceKey || n++ % KEY_EVERY === 0 });
        this.forceKey = false;
      }
      f.close();
    }
    enc?.state !== "closed" && enc?.close();
  }

  /** Every 2 s: link stats per participant, then bitrate adaptation by the worst loss. */
  private async poll() {
    const links = await invoke<{ id: string; rtt: number; loss: number; relay: boolean }[]>("call_stats").catch(() => []);
    if (this.stopped) return;
    let loss = 0;
    for (const l of links) {
      const p = this.peer(l.id); // a silent participant still gets a tile
      p.setNet({ rtt: l.rtt, loss: l.loss, relay: l.relay, kbps: Math.round((p.rxBytes * 8) / 2000) });
      p.rxBytes = 0;
      loss = Math.max(loss, l.loss);
    }
    // Audio: redundancy on heavy loss; bitrate down on loss, back up to the setting when clean.
    this.red = loss > 0.03 || (this.red && loss > 0.005);
    const k = loss > 0.1 ? Math.max(16, this.kbps * 0.75) : loss < 0.02 ? Math.min(this.v.kbps, this.kbps * 1.2) : this.kbps;
    if (Math.abs(k - this.kbps) >= 2) this.setKbps(Math.round(k));
    // Video.
    if (!this.venc || !this.vcfg || this.venc.state !== "configured") return;
    const b = loss > 0.05 ? Math.max(150_000, this.bitrate * 0.7) : loss < 0.01 ? Math.min(1_200_000, this.bitrate * 1.1) : this.bitrate;
    if (Math.abs(b - this.bitrate) < 20_000) return;
    this.bitrate = Math.round(b);
    this.venc.configure((this.vcfg = { ...this.vcfg, bitrate: this.bitrate }));
    this.forceKey = true;
  }

  private peer(id: string): Rx {
    let p = this.rx.get(id);
    if (p) return p;
    const canvas = document.createElement("canvas");
    const adec = new AudioDecoder({ output: (d) => this.play(p!, d), error: () => {} });
    adec.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 1 });
    const [net, setNet] = createSignal<Net>();
    p = { id, canvas, videoAt: 0, talkAt: 0, net, setNet, adec, key: false, lastN: -1, next: 0, lastTs: -1, jitter: 0, transit: NaN, rxBytes: 0 };
    this.rx.set(id, p);
    this.onPeers([...this.rx.values()]);
    return p;
  }

  private receive(buf: ArrayBuffer) {
    if (this.stopped) return;
    const v = new DataView(buf);
    const kind = v.getUint8(0);
    const id = [...new Uint8Array(buf, 1, 4)].map((b) => b.toString(16).padStart(2, "0")).join("");
    const data = new Uint8Array(buf, 13);
    const p = this.peer(id);
    p.rxBytes += buf.byteLength;
    if (kind === AUDIO) return this.receiveAudio(p, data);
    const timestamp = Math.round(performance.now() * 1000);
    if (kind === VIDEO_KEY) p.key = true;
    if (!p.key) return;
    if (!p.vdec || p.vdec.state === "closed") {
      const ctx = p.canvas.getContext("2d")!;
      p.vdec = new VideoDecoder({
        output: (f) => {
          if (p.canvas.width !== f.displayWidth) p.canvas.width = f.displayWidth;
          if (p.canvas.height !== f.displayHeight) p.canvas.height = f.displayHeight;
          ctx.drawImage(f, 0, 0);
          f.close();
          p.videoAt = Date.now();
        },
        // Broken stream: start over from the next key frame.
        error: () => (p.key = false),
      });
      p.vdec.configure({ codec: "vp8" });
    }
    p.vdec.decode(new EncodedVideoChunk({ type: kind === VIDEO_KEY ? "key" : "delta", timestamp, data }));
  }

  private receiveAudio(p: Rx, data: Uint8Array) {
    if (data.length < 6) return;
    const dv = new DataView(data.buffer, data.byteOffset, data.byteLength);
    const n = dv.getUint32(0);
    const len = dv.getUint16(4);
    if (n <= p.lastN) return; // late or duplicate: its slot has been played already
    p.talkAt = Date.now(); // the sender's voice gate sends audio only while they speak
    // Jitter from frame numbers (the sender's clock) vs arrival time.
    const transit = performance.now() - (n * FRAME_US) / 1000;
    const dd = Math.abs(transit - p.transit);
    // Consecutive frames only; a jump of ≥0.5 s is a voice-gate pause, not jitter.
    if (n === p.lastN + 1 && dd < 500) p.jitter += (dd - p.jitter) / 16;
    p.transit = transit;
    const decode = (k: number, d: Uint8Array) =>
      p.adec.state === "configured" && p.adec.decode(new EncodedAudioChunk({ type: "key", timestamp: k * FRAME_US, data: d }));
    // Exactly one frame lost and this packet carries a copy of it: recover it.
    if (n === p.lastN + 2 && data.length > 6 + len) decode(n - 1, data.subarray(6 + len));
    p.lastN = n;
    decode(n, data.subarray(6, 6 + len));
  }

  /** Jitter buffer: frames play back to back, an adaptive delay behind real time; short gaps are concealed. */
  private play(p: Rx, d: AudioData) {
    const buf = this.ctx.createBuffer(1, d.numberOfFrames, d.sampleRate);
    d.copyTo(buf.getChannelData(0), { planeIndex: 0, format: "f32-planar" });
    const ts = d.timestamp;
    d.close();
    const now = this.ctx.currentTime;
    const target = Math.min(0.3, Math.max(0.04, 0.02 + (3 * p.jitter) / 1000));
    const gap = p.lastTs < 0 ? 0 : Math.round((ts - p.lastTs) / FRAME_US) - 1;
    p.lastTs = ts;
    // Underrun or drift past 3× the target: resync.
    if (p.next < now || p.next > now + 3 * target) p.next = now + target;
    // Up to 3 lost frames: replay the last one, fading (simple PLC). Longer gaps are pauses (voice gate).
    else if (p.lastBuf && gap > 0 && gap <= 3) for (let i = 1; i <= gap; i++) this.schedule(p, p.lastBuf, 0.6 / i);
    this.schedule(p, buf, 1);
    p.lastBuf = buf;
  }

  private schedule(p: Rx, buf: AudioBuffer, gain: number) {
    const src = this.ctx.createBufferSource();
    src.buffer = buf;
    if (gain === 1) src.connect(this.out);
    else {
      const g = this.ctx.createGain();
      g.gain.value = gain;
      src.connect(g).connect(this.out);
    }
    src.start(p.next);
    p.next += buf.duration;
  }
}

/** Ring tones by Web Audio: "in" (incoming, two short beeps) or "out" (ringback). Returns stop(). */
export function ring(kind: "in" | "out"): () => void {
  const ctx = new AudioContext();
  const beep = (at: number, len: number, f: number) => {
    const o = ctx.createOscillator(), g = ctx.createGain();
    o.frequency.value = f;
    g.gain.setValueAtTime(0.15, at);
    g.gain.setValueAtTime(0, at + len);
    o.connect(g).connect(ctx.destination);
    o.start(at);
    o.stop(at + len);
  };
  const once = () => {
    const t = ctx.currentTime;
    if (kind === "in") {
      beep(t, 0.25, 880);
      beep(t + 0.35, 0.25, 880);
    } else beep(t, 1, 425);
  };
  once();
  const timer = setInterval(once, kind === "in" ? 2000 : 4000);
  return () => {
    clearInterval(timer);
    ctx.close();
  };
}
