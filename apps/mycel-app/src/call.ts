// Call media in the WebView (§9): getUserMedia (echo/noise suppression) → WebCodecs Opus/VP8 → Rust
// (encrypts, sends over `mycel/call/1`); incoming frames → decoders → speakers / a canvas per peer.
// Chromium already ships AEC, noise suppression and both codecs, so Rust needs no audio/video stack.
import { Channel, invoke } from "@tauri-apps/api/core";

const AUDIO = 0, VIDEO_KEY = 1, VIDEO_DELTA = 2;
/// Playout delay of the jitter buffer, seconds.
const JITTER = 0.06;
const KEY_EVERY = 48; // frames (~2 s): late joiners and dropped frames recover by the next key frame

// MediaStreamTrackProcessor is Chromium-only (WebView2), not in TS's DOM lib.
const Processor = (globalThis as any).MediaStreamTrackProcessor;
const frames = <T>(track: MediaStreamTrack): ReadableStreamDefaultReader<T> => new Processor({ track }).readable.getReader();

const send = (kind: number, chunk: EncodedAudioChunk | EncodedVideoChunk) => {
  const b = new Uint8Array(chunk.byteLength);
  chunk.copyTo(b);
  invoke("call_send", b, { headers: { kind: String(kind) } }).catch(() => {});
};

export type Peer = { id: string; canvas: HTMLCanvasElement; videoAt: number };

type Rx = Peer & { adec: AudioDecoder; next: number; lastA: bigint; vdec?: VideoDecoder; key: boolean };

export class CallMedia {
  local = new MediaStream();
  mic = true;
  cam = false;
  private ctx = new AudioContext({ sampleRate: 48000 });
  private rx = new Map<string, Rx>();
  private stopped = false;
  private bitrate = 600_000;
  private venc?: VideoEncoder;
  private vcfg?: VideoEncoderConfig;
  private forceKey = false;
  private timer = 0;

  constructor(private onPeers: (p: Peer[]) => void) {}

  async start(video: boolean) {
    const ch = new Channel<ArrayBuffer>();
    ch.onmessage = (b) => this.receive(b);
    await invoke("call_listen", { ch });
    const s = await navigator.mediaDevices.getUserMedia({
      audio: { channelCount: 1, sampleRate: 48000, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
    });
    this.local.addTrack(s.getAudioTracks()[0]);
    this.pumpAudio(s.getAudioTracks()[0]);
    if (video) await this.setCam(true).catch(() => {});
    this.timer = window.setInterval(() => this.adapt(), 2000);
  }

  stop() {
    this.stopped = true;
    clearInterval(this.timer);
    this.local.getTracks().forEach((t) => t.stop());
    for (const p of this.rx.values()) {
      p.adec.state !== "closed" && p.adec.close();
      p.vdec?.state !== "closed" && p.vdec?.close();
    }
    this.ctx.close();
  }

  setMic(on: boolean) {
    this.mic = on;
    this.local.getAudioTracks().forEach((t) => (t.enabled = on));
  }

  /** Camera on/off; the camera is opened on first use (an audio call can turn it on). */
  async setCam(on: boolean) {
    this.cam = on;
    let t = this.local.getVideoTracks()[0];
    if (on && !t) {
      const s = await navigator.mediaDevices.getUserMedia({ video: { width: 640, height: 480, frameRate: 24 } });
      t = s.getVideoTracks()[0];
      this.local.addTrack(t);
      this.pumpVideo(t);
    }
    if (t) t.enabled = on;
    this.forceKey = on;
  }

  private async pumpAudio(track: MediaStreamTrack) {
    const enc = new AudioEncoder({ output: (c) => send(AUDIO, c), error: () => {} });
    enc.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 1, bitrate: 32000 });
    const r = frames<AudioData>(track);
    while (!this.stopped) {
      const { value, done } = await r.read();
      if (done) break;
      if (this.mic && enc.state === "configured") enc.encode(value);
      value.close();
    }
    enc.state !== "closed" && enc.close();
  }

  private async pumpVideo(track: MediaStreamTrack) {
    const st = track.getSettings();
    this.vcfg = { codec: "vp8", width: st.width ?? 640, height: st.height ?? 480, bitrate: this.bitrate, framerate: 24, latencyMode: "realtime" };
    const enc = (this.venc = new VideoEncoder({
      output: (c) => send(c.type === "key" ? VIDEO_KEY : VIDEO_DELTA, c),
      error: () => {},
    }));
    enc.configure(this.vcfg);
    const r = frames<VideoFrame>(track);
    let n = 0;
    while (!this.stopped) {
      const { value, done } = await r.read();
      if (done) break;
      // Busy encoder (slow PC): skip a frame rather than build up delay.
      if (this.cam && enc.state === "configured" && enc.encodeQueueSize < 2) {
        enc.encode(value, { keyFrame: this.forceKey || n++ % KEY_EVERY === 0 });
        this.forceKey = false;
      }
      value.close();
    }
    enc.state !== "closed" && enc.close();
  }

  /** Simple bitrate adaptation by packet loss. */
  private async adapt() {
    if (!this.venc || !this.vcfg || this.venc.state !== "configured") return;
    const loss = await invoke<number>("call_loss").catch(() => 0);
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
    p = { id, canvas, videoAt: 0, adec, next: 0, lastA: -1n, key: false };
    this.rx.set(id, p);
    this.onPeers([...this.rx.values()]);
    return p;
  }

  private receive(buf: ArrayBuffer) {
    if (this.stopped) return;
    const v = new DataView(buf);
    const kind = v.getUint8(0);
    const id = [...new Uint8Array(buf, 1, 4)].map((b) => b.toString(16).padStart(2, "0")).join("");
    const seq = v.getBigUint64(5);
    const data = new Uint8Array(buf, 13);
    const p = this.peer(id);
    const timestamp = Math.round(performance.now() * 1000);
    if (kind === AUDIO) {
      if (seq <= p.lastA) return; // late or duplicate: its slot has been played already
      p.lastA = seq;
      if (p.adec.state === "configured") p.adec.decode(new EncodedAudioChunk({ type: "key", timestamp, data }));
      return;
    }
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

  /** Minimal jitter buffer: play each 20 ms frame back to back, JITTER behind real time. */
  private play(p: Rx, d: AudioData) {
    const buf = this.ctx.createBuffer(1, d.numberOfFrames, d.sampleRate);
    d.copyTo(buf.getChannelData(0), { planeIndex: 0, format: "f32-planar" });
    d.close();
    const now = this.ctx.currentTime;
    // Underrun (gap) or drift past 4× the target: resync.
    if (p.next < now || p.next > now + 4 * JITTER) p.next = now + JITTER;
    const src = this.ctx.createBufferSource();
    src.buffer = buf;
    src.connect(this.ctx.destination);
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
