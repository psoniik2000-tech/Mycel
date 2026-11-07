// Settings → "Голос и видео" (like Discord): devices, volumes, level meter, voice activation / push-to-talk.
import { createSignal, For, JSX, onCleanup, onMount, Show } from "solid-js";
import { loadVoice, Mic, saveVoice, speakers, Voice } from "./call";

const pct = (db: number) => `${db + 100}%`; // -100..0 dB → 0..100 %
const keyName = (code: string) => code.replace(/^Key|^Digit/, "");

export function VoiceSettings() {
  const [v, setV] = createSignal(loadVoice());
  const [devs, setDevs] = createSignal<MediaDeviceInfo[]>([]);
  const [level, setLevel] = createSignal(-100);
  const [testing, setTesting] = createSignal(false);
  const [waitKey, setWaitKey] = createSignal(false);
  const [err, setErr] = createSignal("");
  const ctx = new AudioContext({ sampleRate: 48000 });
  const mic = new Mic(ctx);
  let out = speakers(ctx, v());

  const open = () => mic.open(v()).then(() => setErr(""), () => setErr("Нет доступа к микрофону"));
  const list = () => navigator.mediaDevices.enumerateDevices().then(setDevs);
  const set = (patch: Partial<Voice>) => {
    const old = v();
    const nv = { ...old, ...patch };
    setV(nv);
    saveVoice(nv);
    mic.gain.gain.value = nv.inVol / 100;
    if (nv.mic !== old.mic || nv.ns !== old.ns || nv.aec !== old.aec || nv.agc !== old.agc) open();
    if (nv.out !== old.out || nv.outVol !== old.outVol) {
      out.disconnect();
      out = speakers(ctx, nv);
      if (testing()) mic.gain.connect(out);
    }
  };
  const test = () => {
    testing() ? mic.gain.disconnect(out) : mic.gain.connect(out);
    setTesting(!testing());
  };

  let raf = 0;
  const tick = () => {
    setLevel(mic.level());
    raf = requestAnimationFrame(tick);
  };
  const onKey = (e: KeyboardEvent) => {
    if (!waitKey()) return;
    e.preventDefault();
    setWaitKey(false);
    if (e.code !== "Escape") set({ pttKey: e.code });
  };
  onMount(async () => {
    await open();
    list(); // device names are visible only after the mic permission
    navigator.mediaDevices.addEventListener("devicechange", list);
    addEventListener("keydown", onKey);
    tick();
  });
  onCleanup(() => {
    cancelAnimationFrame(raf);
    navigator.mediaDevices.removeEventListener("devicechange", list);
    removeEventListener("keydown", onKey);
    mic.close();
    ctx.close();
  });

  const pick = (kind: MediaDeviceKind, label: string, key: "mic" | "out" | "cam") => (
    <label class="field">
      <span>{label}</span>
      <select value={v()[key]} onChange={(e) => set({ [key]: e.currentTarget.value })}>
        <option value="">По умолчанию</option>
        <For each={devs().filter((d) => d.kind === kind && d.deviceId !== "default" && d.deviceId !== "communications")}>
          {(d, i) => <option value={d.deviceId}>{d.label || `Устройство ${i() + 1}`}</option>}
        </For>
      </select>
    </label>
  );
  const slider = (label: string, key: "inVol" | "outVol"): JSX.Element => (
    <label class="field">
      <span>{label}: {v()[key]}%</span>
      <input type="range" min="0" max="200" value={v()[key]} onInput={(e) => set({ [key]: +e.currentTarget.value })} />
    </label>
  );
  const check = (label: string, key: "ns" | "aec" | "agc") => (
    <label class="check">
      <input type="checkbox" checked={v()[key]} onChange={(e) => set({ [key]: e.currentTarget.checked })} /> {label}
    </label>
  );
  const loud = () => level() >= v().threshold;

  return (
    <>
      <h3>Голос и видео</h3>
      <div class="voice-grid">
        {pick("audioinput", "Микрофон", "mic")}
        {pick("audiooutput", "Динамики", "out")}
        {slider("Громкость микрофона", "inVol")}
        {slider("Громкость звука", "outVol")}
      </div>
      <Show when={err()}><p class="error small">{err()}</p></Show>
      <div class="field">
        <span>Режим ввода</span>
        <div class="segmented">
          <button classList={{ active: v().mode === "vad" }} onClick={() => set({ mode: "vad" })}>Активация голосом</button>
          <button classList={{ active: v().mode === "ptt" }} onClick={() => set({ mode: "ptt" })}>Рация (push-to-talk)</button>
        </div>
      </div>
      <Show
        when={v().mode === "vad"}
        fallback={
          <div class="field">
            <span>Клавиша рации (работает, когда окно Mycel активно)</span>
            <button onClick={() => setWaitKey(true)}>{waitKey() ? "Нажмите клавишу… (Esc — отмена)" : keyName(v().pttKey)}</button>
          </div>
        }
      >
        <div class="field">
          <span>Порог: {v().threshold} дБ — голос передаётся, когда полоска правее отметки</span>
          <div class="meter" classList={{ open: loud() }}>
            <div class="meter-fill" style={{ width: pct(level()) }} />
            <div class="meter-mark" style={{ left: pct(v().threshold) }} />
            <input type="range" min="-100" max="0" value={v().threshold} onInput={(e) => set({ threshold: +e.currentTarget.value })} />
          </div>
        </div>
      </Show>
      <Show when={v().mode === "ptt"}>
        <div class="meter"><div class="meter-fill" style={{ width: pct(level()) }} /></div>
      </Show>
      <button onClick={test} classList={{ active: testing() }}>{testing() ? "Остановить проверку" : "Проверить микрофон"}</button>
      <p class="muted small">Во время проверки вы слышите себя. Лучше в наушниках.</p>
      {check("Шумоподавление", "ns")}
      {check("Эхоподавление", "aec")}
      {check("Автоматическая регулировка громкости", "agc")}
      <label class="field">
        <span>Качество звука: {v().kbps} кбит/с</span>
        <input type="range" min="16" max="128" step="8" value={v().kbps} onInput={(e) => set({ kbps: +e.currentTarget.value })} />
      </label>
      <p class="muted small">При плохой связи битрейт снижается сам и потом возвращается к выбранному.</p>
      {pick("videoinput", "Камера", "cam")}
    </>
  );
}

/** Signal bars for a participant; hover / tap shows ping, loss and bitrate. */
export function Signal(p: { net?: { rtt: number; loss: number; kbps: number; relay: boolean } }) {
  const [shown, setShown] = createSignal(false);
  const bars = () => {
    const n = p.net;
    if (!n) return 0;
    return n.rtt < 120 && n.loss < 0.02 ? 3 : n.rtt < 300 && n.loss < 0.08 ? 2 : 1;
  };
  return (
    <span class={`signal q${bars()}`} onClick={() => setShown(!shown())} onMouseLeave={() => setShown(false)}>
      <i /><i /><i />
      <Show when={p.net}>
        {(n) => (
          <span class="signal-tip" classList={{ shown: shown() }}>
            Пинг: {n().rtt} мс<br />
            Потери: {(n().loss * 100).toFixed(1)}%<br />
            Битрейт: {n().kbps} кбит/с
            <Show when={n().relay}><br />Через сервер-посредник</Show>
          </span>
        )}
      </Show>
    </span>
  );
}
