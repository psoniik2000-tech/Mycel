// «Был(а) …» and Settings → Конфиденциальность, in Telegram's words.
import { createSignal, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { refreshRegistration } from "./directory";

const DAY = 86_400_000;

/** Status of someone who is not online. `hidden`: the time is not shared, only roughly how long ago. */
export function lastSeen(seen: number | null, hidden: boolean, now = Date.now()) {
  if (hidden) {
    const age = seen ? now - seen : 0;
    return age < 3 * DAY ? "был(а) недавно" : age < 7 * DAY ? "был(а) на этой неделе" : "был(а) давно";
  }
  if (!seen) return "не в сети";
  const min = Math.floor((now - seen) / 60_000);
  if (min < 1) return "был(а) только что";
  if (min < 60) return `был(а) ${min} мин назад`;
  // Calendar days between, rounded over DST shifts.
  const days = Math.round((new Date(now).setHours(0, 0, 0, 0) - new Date(seen).setHours(0, 0, 0, 0)) / DAY);
  const at = new Date(seen).toLocaleTimeString("ru", { hour: "2-digit", minute: "2-digit" });
  return days <= 0 ? `был(а) сегодня в ${at}` : days === 1 ? `был(а) вчера в ${at}` : days < 7 ? "был(а) на этой неделе" : "был(а) давно";
}

type Who = "everyone" | "contacts" | "nobody";
type Privacy = { last_seen: Who; calls: Who; read_receipts: boolean; findable: boolean };

export function PrivacySettings(p: { space: string }) {
  const [cur, setCur] = createSignal<Privacy>();
  const [err, setErr] = createSignal("");
  invoke<Privacy>("privacy").then(setCur, (e) => setErr(String(e)));
  const save = async (patch: Partial<Privacy>) => {
    const next = { ...cur()!, ...patch };
    setErr("");
    try {
      await invoke("set_privacy", { privacy: next });
      setCur(next);
      // The directory entry carries this flag.
      if ("findable" in patch) await refreshRegistration(p.space);
    } catch (e) {
      setErr(String(e));
    }
  };
  const who = (title: string, key: "last_seen" | "calls", contacts: string, hint: string) => (
    <>
      <h3>{title}</h3>
      <div class="segmented">
        <For each={[["everyone", "Все"], ["contacts", contacts], ["nobody", "Никто"]] as [Who, string][]}>
          {([v, label]) => <button classList={{ active: cur()![key] === v }} aria-pressed={cur()![key] === v} onClick={() => save({ [key]: v })}>{label}</button>}
        </For>
      </div>
      <p class="muted small">{hint}</p>
    </>
  );
  return (
    <>
      <Show when={cur()}>
        {who("Время захода", "last_seen", "Мои контакты", "Кому видно, когда вы в сети. От кого скрыто — и вы не видите их время: вместо него «был(а) недавно». Контакты — кого вы добавили и кому писали.")}
        {who("Кто может звонить", "calls", "Контакты", "Остальные звонки отклоняются без сигнала.")}
        <h3>Отчёты о прочтении</h3>
        <label class="check">
          <input type="checkbox" checked={cur()!.read_receipts} onChange={(e) => save({ read_receipts: e.currentTarget.checked })} />
          Отправлять и видеть отметки о прочтении
        </label>
        <p class="muted small">Выключено — в личных чатах собеседник не узнает, что вы прочли, и вы не увидите этого о нём. В общих чатах отметки остаются.</p>
        <h3>Кто может найти по @username</h3>
        <div class="segmented">
          <For each={[[true, "Все"], [false, "Никто"]] as [boolean, string][]}>
            {([v, label]) => <button classList={{ active: cur()!.findable === v }} aria-pressed={cur()!.findable === v} onClick={() => save({ findable: v })}>{label}</button>}
          </For>
        </div>
        <p class="muted small">«Никто» — вас нет в поиске по имени. Добавить вас тогда можно по ссылке-приглашению. Собеседники со старой версией могут ещё находить вас.</p>
      </Show>
      <Show when={err()}><p class="error">{err()}</p></Show>
    </>
  );
}
