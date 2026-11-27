// Emoji drawn as Twemoji SVGs shipped with the app (/emoji, see vite.config.ts), like in Telegram.
import { createSignal, Show } from "solid-js";
import { EMOJI_RE, emojiFile, isArt } from "./emoji";

export function Emoji(p: { e: string }) {
  const [missing, setMissing] = createSignal(false); // not in the set: the system font draws it
  return (
    <Show when={!missing()} fallback={p.e}>
      <img class="emoji" src={`/emoji/${emojiFile(p.e)}.svg`} alt={p.e} draggable={false} onError={() => setMissing(true)} />
    </Show>
  );
}

/** Plain text with its emoji replaced by pictures (alt keeps copy-paste intact). */
export const rich = (text: string) => text.split(EMOJI_RE).map((s, i) => (i % 2 && isArt(s) ? <Emoji e={s} /> : s));
