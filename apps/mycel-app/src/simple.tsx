// "Simple messenger" mode (DESIGN.md §4.2): a hidden space plus pairwise contacts, shown as a contact list. A UI setting of this device.
import { createSignal, Show } from "solid-js";
import { Hash, MessageCircle } from "lucide-solid";
import logo from "../app-icon.svg";

type Mode = "simple" | "spaces";
const read = (): Mode | null => {
  try {
    const v = localStorage.getItem("mode");
    return v === "simple" || v === "spaces" ? v : null;
  } catch {
    return null;
  }
};
// null: not chosen yet (first start shows the choice). Spaces mode is only ever an explicit choice.
const [mode, setModeSig] = createSignal<Mode | null>(read());
export { mode };
export const simple = () => mode() !== "spaces";
export const setMode = (m: Mode | null) => {
  try {
    m ? localStorage.setItem("mode", m) : localStorage.removeItem("mode");
  } catch {
    // the choice just lasts until restart
  }
  setModeSig(m);
};
/** Name of the space simple mode creates (seen only in spaces mode). */
export const SIMPLE_NAME = "Личные чаты";
/** A space's name as a group's: outside spaces mode the default names lose the word «пространство». */
export const groupTitle = (name: string) =>
  simple() ? name.replace(/^Моё пространство$/, "Моя группа").replace(/^Пространство (\d+)$/, "Группа $1") : name;

export function ModeChoice(p: { onSimple: () => void; onSpaces: () => void; onRestore: () => void }) {
  const [other, setOther] = createSignal(false);
  return (
    <div class="center">
      <div class="card intro">
        <img src={logo} class="logo" alt="" />
        <h1>Mycel</h1>
        <p class="muted">Мессенджер со сквозным шифрованием: регистрация по имени пользователя, писать можно любому по @имени</p>
        <button class="primary" onClick={() => (setMode("simple"), p.onSimple())}><MessageCircle size={18} /> Начать</button>
        <Show when={other()} fallback={<button class="link" onClick={() => setOther(true)}>Другие способы</button>}>
          <button onClick={() => (setMode("spaces"), p.onSpaces())}><Hash size={18} /> Пространства и свой сервер</button>
          <p class="muted small">Сообщества с общим чатом, администратором и приглашениями, свой якорь вместо главного сервера</p>
          <button class="link" onClick={p.onRestore}>Восстановить из копии</button>
        </Show>
      </div>
    </div>
  );
}

export function ModeSettings() {
  return (
    <>
      <h3>Режим пространств</h3>
      <label class="check">
        <input type="checkbox" checked={!simple()} onChange={(e) => setMode(e.currentTarget.checked ? "spaces" : "simple")} />
        Пространства: управление сообществами, свой сервер (якорь)
      </label>
      <p class="muted">Выключено — обычный мессенджер: чаты, контакты и группы. Переписка и данные от режима не меняются.</p>
    </>
  );
}
