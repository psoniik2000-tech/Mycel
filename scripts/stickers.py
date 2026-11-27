#!/usr/bin/env python3
"""Rebuilds the sticker set: Microsoft Fluent Emoji 3D (MIT) as WebP in apps/mycel-app/public/stickers.

    git clone --depth 1 --filter=blob:none --no-checkout https://github.com/microsoft/fluentui-emoji
    python scripts/stickers.py <that clone>

The clone is only the file list; the files come over HTTPS. Needs git and ffmpeg (with libwebp). Also writes apps/mycel-app/src/sticker-sets.ts.
"""
import json, pathlib, subprocess, sys, time, urllib.parse, urllib.request

# (tab title, asset folder names); the first one is the tab's icon.
SETS = [
    ("Эмоции", """Grinning face with big eyes|Beaming face with smiling eyes|Face with tears of joy|Rolling on the floor laughing
        Grinning face with sweat|Smiling face with smiling eyes|Smiling face with halo|Smiling face with hearts|Smiling face with heart-eyes
        Star-struck|Face blowing a kiss|Winking face|Face savoring food|Winking face with tongue|Zany face|Money-mouth face
        Hugging face|Face with hand over mouth|Shushing face|Thinking face|Face with raised eyebrow|Neutral face
        Smirking face|Face with rolling eyes|Grimacing face|Relieved face|Sleeping face|Face with medical mask|Face with thermometer
        Nauseated face|Hot face|Cold face|Woozy face|Exploding head|Cowboy hat face|Partying face|Smiling face with sunglasses
        Nerd face|Pleading face|Face holding back tears|Crying face|Loudly crying face|Face screaming in fear|Pouting face
        Face with steam from nose|Face with symbols on mouth|Smiling face with horns|Skull|Pile of poo|Clown face|Ghost|Alien|Robot
        Melting face|Saluting face"""),
    ("Жесты и сердца", """Thumbs up|Thumbs down|Waving hand|Ok hand|Victory hand|Crossed fingers|Love-you gesture|Call me hand
        Raised fist|Oncoming fist|Clapping hands|Raising hands|Handshake|Folded hands|Flexed biceps|Heart hands
        Red heart|Orange heart|Yellow heart|Green heart|Blue heart|Purple heart|Broken heart|Heart on fire|Sparkling heart
        Two hearts|Heart with arrow|Kiss mark|Hundred points|Fire|Eyes"""),
    ("Животные", """Dog face|Cat face|Mouse face|Hamster|Rabbit face|Fox|Bear|Panda|Koala|Tiger face|Lion|Cow face|Pig face|Frog
        Monkey face|See-no-evil monkey|Hear-no-evil monkey|Speak-no-evil monkey|Chicken|Penguin|Baby chick|Duck|Owl|Wolf|Unicorn
        Honeybee|Butterfly|Lady beetle|Turtle|Octopus|Spouting whale|Dolphin|Shark|Sloth|Hedgehog|Crocodile|T-Rex"""),
    ("Еда", """Pizza|Hamburger|French fries|Hot dog|Taco|Sushi|Steaming bowl|Doughnut|Cookie|Birthday cake|Shortcake|Cupcake
        Chocolate bar|Ice cream|Popcorn|Strawberry|Watermelon|Banana|Avocado|Hot beverage|Bubble tea|Beer mug|Clinking beer mugs
        Clinking glasses|Bottle with popping cork"""),
    ("Разное", """Party popper|Confetti ball|Balloon|Wrapped gift|Christmas tree|Fireworks|Trophy|Crown|Gem stone|Money bag
        Rocket|Airplane|Automobile|Soccer ball|Video game|Musical notes|Guitar|Light bulb|Alarm clock|Bouquet|Rose|Sunflower
        Four leaf clover|Rainbow|Sun with face|Crescent moon|Star|Sparkles|High voltage|Snowflake|Collision|Zzz"""),
]

repo = pathlib.Path(sys.argv[1])
app = pathlib.Path(__file__).resolve().parent.parent / "apps" / "mycel-app"
out = app / "public" / "stickers" / "f3d"


def git(*args):
    return subprocess.run(["git", "-C", str(repo), "-c", "core.quotepath=off", *args], check=True, capture_output=True, encoding="utf-8").stdout


paths = set(git("ls-tree", "-r", "--name-only", "HEAD").splitlines())
folders = {p.split("/")[1].lower(): p.split("/")[1] for p in paths if p.startswith("assets/")}
chosen, missing = [], []
for title, names in SETS:
    items = []
    for name in (n.strip() for n in names.replace("\n", "|").split("|")):
        f = folders.get(name.lower())
        pngs = f and [p for p in paths if p.startswith(f"assets/{f}/") and "/3D/" in p and ("/Default/" in p or p.count("/") == 3)]
        if pngs:
            items.append((f"assets/{f}/metadata.json", pngs[0]))
        else:
            missing.append(name)
    chosen.append((title, items))
if missing:
    sys.exit(f"no such assets: {missing}")

commit = git("rev-parse", "HEAD").strip()


def get(path):
    # Over HTTPS: a blobless clone's lazy fetch is unreliable for a few hundred files.
    url = f"https://raw.githubusercontent.com/microsoft/fluentui-emoji/{commit}/{urllib.parse.quote(path)}"
    for attempt in range(4):
        try:
            with urllib.request.urlopen(url, timeout=60) as r:
                return r.read()
        except OSError:
            if attempt == 3:
                raise
            time.sleep(2 ** attempt)


out.mkdir(parents=True, exist_ok=True)
for old in out.glob("*.webp"):
    old.unlink()
ts = []
for title, items in chosen:
    row = []
    for meta, png in items:
        glyph = json.loads(get(meta))["glyph"]
        # The id is the emoji's code points without FE0F: stable, and short enough for the wire (§ STICKER in mycel-proto).
        name = "-".join(f"{ord(c):x}" for c in glyph if c != "\ufe0f")
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "png_pipe", "-i", "pipe:0", "-c:v", "libwebp", "-quality", "82",
                        "-compression_level", "6", str(out / f"{name}.webp")], input=get(png), check=True)
        row.append(glyph)
    ts.append(f'  ["{title}", "{" ".join(row)}"],')
(out.parent / "LICENSE.txt").write_bytes(get("LICENSE"))
(app / "src" / "sticker-sets.ts").write_text(
    "// Generated by scripts/stickers.py: sticker tabs, each a title and the emoji of its stickers.\n"
    "export const STICKERS: [string, string][] = [\n" + "\n".join(ts) + "\n];\n", encoding="utf-8", newline="\n")
files = list(out.glob("*.webp"))
print(f"{len(files)} stickers, {sum(f.stat().st_size for f in files) / 1e6:.2f} MB")
