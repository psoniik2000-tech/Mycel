// Mic/camera capture in the WebView (MediaRecorder → Opus/WebM). Calls (phase 6) reuse `openStream`.

export const VOICE = { audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true } };
export const CIRCLE = { ...VOICE, video: { width: 480, height: 480, aspectRatio: 1, facingMode: "user" } };

export function openStream(video: boolean): Promise<MediaStream> {
  return navigator.mediaDevices.getUserMedia(video ? CIRCLE : VOICE);
}

export type Recording = { stream: MediaStream; mime: string; started: number; stop: () => Promise<Blob>; cancel: () => void };

/** Starts recording: voice ~24 kbit/s mono Opus, or a 480×480 video note. */
export async function record(video: boolean): Promise<Recording> {
  const stream = await openStream(video);
  const mime = video ? "video/webm;codecs=vp8,opus" : "audio/webm;codecs=opus";
  const rec = new MediaRecorder(stream, {
    mimeType: mime,
    audioBitsPerSecond: 24000,
    ...(video ? { videoBitsPerSecond: 800_000 } : {}),
  });
  const chunks: Blob[] = [];
  rec.ondataavailable = (e) => chunks.push(e.data);
  const end = () => stream.getTracks().forEach((t) => t.stop());
  rec.start(250);
  return {
    stream,
    mime,
    started: Date.now(),
    stop: () =>
      new Promise((done) => {
        rec.onstop = () => {
          end();
          done(new Blob(chunks, { type: mime }));
        };
        rec.stop();
      }),
    cancel: () => {
      rec.onstop = end;
      rec.stop();
    },
  };
}

/** 64 peak levels 0..255 of a recording (the voice message waveform). */
export async function waveform(blob: Blob): Promise<number[]> {
  const ctx = new AudioContext();
  try {
    const d = (await ctx.decodeAudioData(await blob.arrayBuffer())).getChannelData(0);
    const step = Math.max(1, Math.floor(d.length / 64));
    const peaks = Array.from({ length: 64 }, (_, i) => {
      let m = 0;
      for (let j = i * step; j < Math.min(d.length, (i + 1) * step); j++) m = Math.max(m, Math.abs(d[j]));
      return m;
    });
    const max = Math.max(...peaks, 1e-3);
    return peaks.map((p) => Math.round((p / max) * 255));
  } catch {
    return [];
  } finally {
    ctx.close();
  }
}
