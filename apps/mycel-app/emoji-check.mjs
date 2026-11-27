// Self-check: node emoji-check.mjs — splitting, big-emoji detection, and a picture for every picker emoji.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { EMOJI, EMOJI_RE, emojiFile, emojiOnly, isArt } from "./src/emoji.ts";

const has = (e) => existsSync(new URL(`node_modules/@twemoji/svg/${emojiFile(e)}.svg`, import.meta.url));
for (const [icon, list] of EMOJI)
  for (const e of [icon, ...list.split(" ")]) {
    assert.deepEqual(e.split(EMOJI_RE), ["", e, ""], `split ${e}`);
    assert.ok(isArt(e) && has(e), `no picture for ${e} (${emojiFile(e)})`);
  }
for (const e of ["🇷🇺", "👍🏽", "👨‍👩‍👧", "1️⃣", "🏴󠁧󠁢󠁥󠁮󠁧󠁿"]) assert.ok(emojiOnly(e) === 1 && has(e), e);
assert.equal(emojiOnly("😀 🔥\n❤️"), 3);
for (const t of ["", "привет 😀", "😀😀😀😀", "12", "©", "™"]) assert.equal(emojiOnly(t), 0, t);
console.log("emoji ok");
