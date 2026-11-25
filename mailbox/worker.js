// Mycel mailbox: keeps encrypted envelopes for members who are offline (30 days).
// Cloudflare Worker with a D1 database bound as `DB`. Setup: mailbox/README.md.
//   POST /box/<topic>/<id>   body = envelope bytes (deduplicated by id)
//   GET  /box/<topic>?after=N  -> lines "<seq> <base64>", oldest first, at most 200
const DAYS = 30;
const MAX_BODY = 1 << 20;
const PAGE = 200;

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    const m = url.pathname.match(/^\/box\/([0-9a-f]{64})(?:\/([0-9a-f]{64}))?$/);
    if (!m) return new Response("Mycel mailbox works", { status: url.pathname === "/" ? 200 : 404 });
    await env.DB.batch([
      env.DB.prepare("CREATE TABLE IF NOT EXISTS box (seq INTEGER PRIMARY KEY AUTOINCREMENT, topic TEXT NOT NULL, id TEXT NOT NULL, ts INTEGER NOT NULL, body BLOB NOT NULL, UNIQUE (topic, id))"),
      env.DB.prepare("CREATE INDEX IF NOT EXISTS box_ts ON box (ts)"),
    ]);
    const [, topic, id] = m;
    if (req.method === "POST" && id) {
      const body = await req.arrayBuffer();
      if (body.byteLength > MAX_BODY) return new Response("too big", { status: 413 });
      const now = Date.now();
      await env.DB.prepare("INSERT OR IGNORE INTO box (topic, id, ts, body) VALUES (?, ?, ?, ?)").bind(topic, id, now, body).run();
      if (Math.random() < 0.02) await env.DB.prepare("DELETE FROM box WHERE ts < ?").bind(now - DAYS * 86400000).run();
      return new Response("ok");
    }
    if (req.method === "GET" && !id) {
      const after = Number(url.searchParams.get("after")) || 0;
      const { results } = await env.DB.prepare("SELECT seq, body FROM box WHERE topic = ? AND seq > ? ORDER BY seq LIMIT ?").bind(topic, after, PAGE).all();
      const lines = results.map((r) => `${r.seq} ${b64(new Uint8Array(r.body))}\n`);
      return new Response(lines.join(""), { headers: { "content-type": "text/plain" } });
    }
    return new Response("bad request", { status: 400 });
  },
};

function b64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}
