// Mycel mailbox: keeps encrypted envelopes for members who are offline (30 days).
// Cloudflare Worker with a D1 database bound as `DB`. Setup: mailbox/README.md.
//   POST /box/<topic>/<id>   body = envelope bytes (deduplicated by id);
//                            header x-push: 1 wakes the topic's phones (FCM), x-from: sender's token is skipped,
//                            x-to: device tags (comma-separated) of a 1:1 partner → only those phones (and untagged old ones)
//   GET  /box/<topic>?after=N  -> lines "<seq> <base64>", oldest first, at most 200
//   POST /push/<topic>       body = FCM token of a phone in that space, header x-device: its tag
// Pushes need the secret FCM_KEY = the Firebase service account JSON (optional).
const DAYS = 30;
const MAX_BODY = 1 << 20;
const PAGE = 200;
const HEX = /^[0-9a-f]{64}$/;

export default {
  async fetch(req, env, ctx) {
    const url = new URL(req.url);
    const [, kind, topic, id] = url.pathname.split("/");
    if (!HEX.test(topic ?? "") || (kind !== "box" && kind !== "push"))
      return new Response("Mycel mailbox works", { status: url.pathname === "/" ? 200 : 404 });
    await env.DB.batch([
      env.DB.prepare("CREATE TABLE IF NOT EXISTS box (seq INTEGER PRIMARY KEY AUTOINCREMENT, topic TEXT NOT NULL, id TEXT NOT NULL, ts INTEGER NOT NULL, body BLOB NOT NULL, UNIQUE (topic, id))"),
      env.DB.prepare("CREATE INDEX IF NOT EXISTS box_ts ON box (ts)"),
      env.DB.prepare("CREATE TABLE IF NOT EXISTS push (topic TEXT NOT NULL, token TEXT NOT NULL, ts INTEGER NOT NULL, device TEXT, PRIMARY KEY (topic, token))"),
    ]);
    const now = Date.now();
    if (kind === "push" && req.method === "POST" && !id) {
      const token = (await req.text()).trim();
      if (!token || token.length > 4096) return new Response("bad token", { status: 400 });
      const device = req.headers.get("x-device") ?? "";
      // Tables made before x-device existed: add the column once (fails harmlessly afterwards).
      await env.DB.prepare("ALTER TABLE push ADD COLUMN device TEXT").run().catch(() => {});
      await env.DB.prepare("INSERT OR REPLACE INTO push (topic, token, ts, device) VALUES (?, ?, ?, ?)").bind(topic, token, now, HEX.test(device) ? device : null).run();
      return new Response("ok");
    }
    if (kind === "box" && req.method === "POST" && HEX.test(id ?? "")) {
      const body = await req.arrayBuffer();
      if (body.byteLength > MAX_BODY) return new Response("too big", { status: 413 });
      const r = await env.DB.prepare("INSERT OR IGNORE INTO box (topic, id, ts, body) VALUES (?, ?, ?, ?)").bind(topic, id, now, body).run();
      if (r.meta.changes && req.headers.get("x-push") === "1" && env.FCM_KEY)
        ctx.waitUntil(wake(env, topic, req.headers.get("x-from") ?? "", req.headers.get("x-to") ?? "").catch((e) => console.log("push failed", e)));
      if (Math.random() < 0.02)
        await env.DB.batch([
          env.DB.prepare("DELETE FROM box WHERE ts < ?").bind(now - DAYS * 86400000),
          env.DB.prepare("DELETE FROM push WHERE ts < ?").bind(now - 60 * 86400000),
        ]);
      return new Response("ok");
    }
    if (kind === "box" && req.method === "GET" && !id) {
      const after = Number(url.searchParams.get("after")) || 0;
      const { results } = await env.DB.prepare("SELECT seq, body FROM box WHERE topic = ? AND seq > ? ORDER BY seq LIMIT ?").bind(topic, after, PAGE).all();
      const lines = results.map((r) => `${r.seq} ${b64(new Uint8Array(r.body))}\n`);
      return new Response(lines.join(""), { headers: { "content-type": "text/plain" } });
    }
    return new Response("bad request", { status: 400 });
  },
};

/** Sends a data-only FCM message ("something new", no content) to every phone of the topic but the sender,
 *  or only to the `to` device (1:1 chat); phones registered without a tag still get every push. */
async function wake(env, topic, from, to) {
  // x-to: comma-separated tags (every device of the partner's account, DESIGN §4.4).
  const tags = to.split(",").filter((t) => HEX.test(t)).slice(0, 16);
  const { results } = tags.length
    ? await env.DB.prepare(`SELECT token FROM push WHERE topic = ? AND token != ? AND (device IS NULL OR device IN (${tags.map(() => "?").join(",")}))`).bind(topic, from, ...tags).all()
    : await env.DB.prepare("SELECT token FROM push WHERE topic = ? AND token != ?").bind(topic, from).all();
  if (!results.length) return;
  const key = JSON.parse(env.FCM_KEY);
  const auth = await accessToken(key);
  await Promise.all(results.map(async ({ token }) => {
    const r = await fetch(`https://fcm.googleapis.com/v1/projects/${key.project_id}/messages:send`, {
      method: "POST",
      headers: { authorization: `Bearer ${auth}`, "content-type": "application/json" },
      body: JSON.stringify({ message: { token, data: { t: "msg" }, android: { priority: "high", ttl: "86400s" } } }),
    });
    // The app was removed or the token replaced: forget it.
    if (r.status === 404 || r.status === 400) await env.DB.prepare("DELETE FROM push WHERE token = ?").bind(token).run();
  }));
}

let cached = { token: "", exp: 0 };

/** OAuth token for FCM from the service account (JWT signed with its RS256 key), cached ~50 min. */
async function accessToken(key) {
  const now = Math.floor(Date.now() / 1000);
  if (cached.exp > now + 60) return cached.token;
  const enc = (o) => b64url(new TextEncoder().encode(JSON.stringify(o)));
  const unsigned = `${enc({ alg: "RS256", typ: "JWT" })}.${enc({
    iss: key.client_email,
    scope: "https://www.googleapis.com/auth/firebase.messaging",
    aud: "https://oauth2.googleapis.com/token",
    iat: now,
    exp: now + 3600,
  })}`;
  const der = Uint8Array.from(atob(key.private_key.replace(/-----[^-]+-----|\s/g, "")), (c) => c.charCodeAt(0));
  const pk = await crypto.subtle.importKey("pkcs8", der, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["sign"]);
  const sig = new Uint8Array(await crypto.subtle.sign("RSASSA-PKCS1-v1_5", pk, new TextEncoder().encode(unsigned)));
  const r = await fetch("https://oauth2.googleapis.com/token", {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: `grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=${unsigned}.${b64url(sig)}`,
  });
  const j = await r.json();
  if (!j.access_token) throw new Error(`oauth: ${JSON.stringify(j)}`);
  cached = { token: j.access_token, exp: now + (j.expires_in ?? 3600) };
  return cached.token;
}

function b64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

const b64url = (bytes) => b64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
