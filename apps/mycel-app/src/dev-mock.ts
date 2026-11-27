// Browser-only fake backend for UI work (`npm run dev` without Tauri). Never runs in the app or the build.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";

if (import.meta.env.DEV && !(window as any).__TAURI_INTERNALS__) {
  mockWindows("main");
  (window as any).__TAURI_INTERNALS__.convertFileSrc = (p: string) => p;
  localStorage.setItem("mode", localStorage.getItem("mode") ?? "simple");
  const now = Date.now(), min = 60_000, day = 86_400_000;
  const ME = "aa11", ANNA = "b2c4", OLEG = "3f90", MAMA = "c7d1", WORK = "e512";
  const photo = "data:image/svg+xml," + encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300"><defs><linearGradient id="g" x2="1" y2="1"><stop stop-color="#5b8bd9"/><stop offset="1" stop-color="#e8a33d"/></linearGradient></defs><rect width="400" height="300" fill="url(#g)"/><circle cx="300" cy="80" r="40" fill="#fff8"/><path d="M0 300 L120 160 L220 260 L290 200 L400 300Z" fill="#2a3b2a"/></svg>`);
  const wave = Array.from({ length: 48 }, (_, i) => Math.round(60 + 160 * Math.abs(Math.sin(i * 0.7))));
  const members = [
    { id: ME, me: true, admin: true, name: "Евгений", avatar: "", color: 0, link: null, seen: null },
    { id: ANNA, me: false, admin: false, name: "Анна Смирнова", avatar: "", color: 6, link: "direct", seen: now },
    { id: OLEG, me: false, admin: false, name: "Олег", avatar: "", color: 4, link: null, seen: now - 25 * min },
    { id: MAMA, me: false, admin: false, name: "Мама", avatar: "", color: 7, link: "relay", seen: now },
    { id: WORK, me: false, admin: false, name: "Константин Константинопольский", avatar: "", color: 2, link: null, seen: now - 3 * day },
  ];
  let n = 0;
  const m = (author: string, text: string, ts: number, extra: any = {}) => ({
    id: `m${++n}`, author, mine: author === ME, text, ts, status: author === ME ? 2 : 0, media: null, call: null, reply: null,
    edited: false, pinned: false, reactions: [], ...extra,
  });
  const media = (kind: string, more: any) => ({ kind, name: null, mime: "", size: 120_000, blurhash: null, wave: null, duration_ms: null, dims: null, path: null, ...more });
  const hist: Record<string, any[]> = {
    [ANNA]: [
      m(ANNA, "Привет! Как съездил?", now - day - 60 * min),
      m(ME, "Отлично, вот фото с горы", now - day - 55 * min),
      m(ME, "", now - day - 54 * min, { media: media("photo", { dims: [4, 3], path: photo }) }),
      m(ANNA, "Вау, красота 😍", now - day - 50 * min, { reactions: [["❤️", [ME]]] }),
      m(ANNA, "", now - 40 * min, { media: media("voice", { wave, duration_ms: 14_000, path: "data:audio/wav;base64," }) }),
      m(ME, "Да, давай в субботу. Я возьму палатку и горелку, с тебя карта и что-нибудь поесть на два дня", now - 30 * min, { reply: { id: "m4", author: ANNA, text: "Вау, красота 😍" } }),
      m(ANNA, "Договорились!", now - 12 * min),
      m(ANNA, "Во сколько выезжаем?", now - 11 * min),
      m(ME, "В 8 у метро", now - 5 * min, { status: 1, edited: true }),
    ],
    [OLEG]: [m(OLEG, "Скинь, пожалуйста, документы по проекту", now - 2 * 60 * min), m(ME, "Держи", now - 100 * min, { media: media("file", { name: "Отчёт за квартал (финальная версия).pdf", size: 2_400_000, path: "x" }) })],
    [MAMA]: [m(MAMA, "Ты поел?", now - 3 * min), m(MAMA, "Позвони, как будет время", now - 2 * min)],
    [WORK]: [m(WORK, "Созвон перенесли на четверг, в 15:00 по Москве, ссылка будет в календаре", now - 2 * day)],
  };
  const unread: Record<string, number> = { [MAMA]: 2, [ANNA]: 1 };
  const chats = () => Object.keys(hist).map((id) => ({ id, last: hist[id].at(-1), unread: unread[id] ?? 0, muted: id === WORK }));
  const calls = [{ id: "c1", video: false, author: MAMA, chat: MAMA, mine: false, ts: now - day, duration_ms: 320_000, missed: false, active: false },
    { id: "c2", video: true, author: OLEG, chat: OLEG, mine: false, ts: now - 2 * day, duration_ms: null, missed: true, active: false }];
  const space = { id: "s1", name: "Личные чаты", remembered: true };
  const handlers: Record<string, (a: any) => unknown> = {
    status: () => ({ spaces: [space], last: "s1", current: "s1", dir: "C:\\Users\\me\\AppData\\Roaming\\mycel\\s1", in_space: true }),
    members: () => members,
    chats,
    history: (a) => hist[a.chat] ?? [],
    calls_list: () => calls,
    peers: () => 2,
    mark_read: (a) => void (unread[a.chat] = 0),
    send: (a) => (hist[a.chat].push(m(ME, a.text, Date.now(), a.replyTo ? { reply: { id: a.replyTo, author: ANNA, text: "…" } } : {})), "x"),
    react: () => null,
    safety_code: () => ({ link: "mycel://safety/123", svg: "" }),
    upnp_ok: () => false,
    autostart: () => false,
    home_relay: () => null,
    "plugin:deep-link|get_current": () => null,
    "plugin:notification|is_permission_granted": () => true,
    "plugin:app|version": () => "0.1.8-dev",
  };
  mockIPC((cmd, args) => (cmd.startsWith("plugin:event|") ? Math.random() : (handlers[cmd]?.(args) ?? null)));
}
