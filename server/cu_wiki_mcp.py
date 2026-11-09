#!/usr/bin/env python3
"""Casualties Unknown wiki MCP server (stdlib only, JSON-RPC over stdio).

Read-only live reader for the unofficial fan database at
https://casualtiesunknowndb.com (items, liquids, moodles/status effects,
recipes, enemies, buildings, commands, vitals, ...). Pages are fetched
LIVE on demand — nothing is bulk-mirrored. Only caches:

  - the sitemap-derived search index (+ display names parsed from the
    section listing pages), 24h, in cache/index.json next to this file's
    parent connector dir;
  - individual page HTML, 1h, in cache/pages/;
  - sprite/icon PNGs, 24h, in cache/img/;
  - composed presentation images, 24h, in cache/img_canvas/ (source
    sprites are 4–32px pixel art; a full-bleed upscale fills the whole
    chat view, so each sprite is integer-scaled with PIL NEAREST to
    fit within 288x288 and pasted centered on a transparent 480x480
    canvas — zoomed out, whole, with margin — before being returned).

Protocol: newline-delimited JSON-RPC 2.0 on stdin/stdout.
Supported methods: initialize, notifications/initialized (ignored),
tools/list, tools/call, ping. All logging goes to stderr so stdout stays
clean protocol traffic. No credentials exist or are stored anywhere.

The cache directory defaults to cache/ in the connector dir, and can be
overridden with the CU_CACHE_DIR environment variable (the Vercel HTTP
entry point in api/mcp.py sets it to /tmp, the only writable location
in a serverless sandbox). The JSON-RPC handler (handle()) is shared by
the stdio entry point here and the HTTP entry point in api/mcp.py.
"""

import base64
import io
import json
import math
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from html.parser import HTMLParser
from pathlib import Path

BASE = "https://casualtiesunknowndb.com"
UA = ("cu-wiki-mcp/1.0 (read-only live reader for "
      "casualtiesunknowndb.com; on-demand fetch, no bulk mirroring)")
TIMEOUT = 20

SERVER_DIR = Path(__file__).resolve().parent
_cache_env = os.environ.get("CU_CACHE_DIR")
CACHE_DIR = (Path(_cache_env).expanduser() if _cache_env
             else SERVER_DIR.parent / "cache")
INDEX_FILE = CACHE_DIR / "index.json"
PAGES_DIR = CACHE_DIR / "pages"
IMG_DIR = CACHE_DIR / "img"
IMG_CANVAS_DIR = CACHE_DIR / "img_canvas"
INDEX_TTL = 24 * 3600
PAGE_TTL = 3600
IMG_TTL = 24 * 3600
# Image presentation: every returned image is a CANVAS_SIZE square with
# the sprite NEAREST-scaled to fit within SPRITE_MAX on a side and
# pasted centered — small items appear zoomed out, whole, with margin,
# instead of filling the whole chat view.
CANVAS_SIZE = 480
SPRITE_MAX = 288

# Sections whose entries have a sprite at /icons/sprites/<slug>.png.
SPRITE_SECTIONS = {"items", "liquids", "moodles"}
# Listing pages parsed (once, with the index) for display names.
LISTING_SECTIONS = ["items", "moodles", "liquids", "recipes", "enemies",
                    "buildings", "commands", "depth", "difficulty",
                    "vitals", "hazards", "firearms", "anatomy", "skills"]
MAX_TEXT = 15000


def log(*args):
    print(*args, file=sys.stderr, flush=True)


class ToolError(Exception):
    pass


# ---------------------------------------------------------------- fetch

def fetch_bytes(url, ttl=None, cache_path=None):
    """Fetch url (optionally disk-cached); ToolError naming the URL."""
    if cache_path is not None and ttl:
        try:
            if cache_path.is_file() and \
                    time.time() - cache_path.stat().st_mtime < ttl:
                return cache_path.read_bytes()
        except OSError:
            pass
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            data = resp.read()
    except (urllib.error.URLError, urllib.error.HTTPError, OSError) as exc:
        raise ToolError(f"fetch failed for {url}: {exc}")
    if cache_path is not None:
        try:
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            cache_path.write_bytes(data)
        except OSError as exc:
            log(f"cache write failed for {url}: {exc}")
    return data


def fetch_page_html(section, slug):
    cache_path = PAGES_DIR / f"{section}__{slug}.html"
    data = fetch_bytes(f"{BASE}/{section}/{slug}", ttl=PAGE_TTL,
                       cache_path=cache_path)
    return data.decode("utf-8", errors="replace")


def sprite_url(slug):
    return f"{BASE}/icons/sprites/{slug}.png"


def fetch_image(section, slug, first_img=None):
    """Return (png_bytes, image_url) or (None, url_or_None).

    Items/liquids/moodles: canonical sprite first. Other sections: the
    page's first <img>. A failed image fetch never fails the tool.
    """
    candidates = []
    if section in SPRITE_SECTIONS:
        candidates.append(sprite_url(slug))
    if first_img:
        abs_img = urllib.parse.urljoin(BASE + "/", first_img)
        if abs_img not in candidates:
            candidates.append(abs_img)
    for url in candidates:
        m = re.search(r"/([^/]+)\.png($|\?)", url)
        key = m.group(1) if m else re.sub(r"[^a-z0-9]+", "_", url.lower())
        cache_path = IMG_DIR / f"{section}__{key}.png"
        try:
            data = fetch_bytes(url, ttl=IMG_TTL, cache_path=cache_path)
        except ToolError:
            continue
        if data[:8] == b"\x89PNG\r\n\x1a\n" or data[:3] == b"\xff\xd8\xff" \
                or data[:4] == b"GIF8" or data[8:12] == b"WEBP":
            return data, url
    return None, (candidates[0] if candidates else None)


def canvas_image(data, cache_path):
    """Compose the image zoomed-out on a transparent square canvas.

    Returns (png_bytes, (orig_w, orig_h), (shown_w, shown_h)). The DB's
    sprites are 4–32px; a full-bleed upscale fills the whole chat view
    so the shape can't be seen at a glance. Instead the sprite is
    scaled with Image.NEAREST to fit within SPRITE_MAX x SPRITE_MAX
    (integer factor for small sprites; NEAREST downscale if already
    larger) and pasted centered on a fully transparent
    CANVAS_SIZE x CANVAS_SIZE canvas — the returned block is always
    that square canvas with the sprite occupying at most 60% of the
    frame. Composed PNGs are cached at cache_path for IMG_TTL. If PIL
    is unavailable or the bytes don't decode, returns
    (data, None, None) — the original image is served unchanged,
    never an error.
    """
    try:
        from PIL import Image
    except ImportError:
        return data, None, None
    try:
        with Image.open(io.BytesIO(data)) as im:
            im.load()
            w, h = im.size
            longest = max(w, h)
            if longest > SPRITE_MAX:
                scale = SPRITE_MAX / longest
                shown = (max(1, round(w * scale)),
                         max(1, round(h * scale)))
            else:
                factor = max(1, SPRITE_MAX // longest)
                shown = (w * factor, h * factor)
            try:
                if cache_path.is_file() and \
                        time.time() - cache_path.stat().st_mtime < IMG_TTL:
                    return cache_path.read_bytes(), (w, h), shown
            except OSError:
                pass
            sprite = im.convert("RGBA").resize(shown, Image.NEAREST)
            canvas = Image.new("RGBA", (CANVAS_SIZE, CANVAS_SIZE),
                               (0, 0, 0, 0))
            offset = ((CANVAS_SIZE - shown[0]) // 2,
                      (CANVAS_SIZE - shown[1]) // 2)
            canvas.paste(sprite, offset, sprite)
            buf = io.BytesIO()
            canvas.save(buf, format="PNG")
            out = buf.getvalue()
    except Exception as exc:
        log(f"canvas compose failed, serving original: {exc!r}")
        return data, None, None
    try:
        cache_path.parent.mkdir(parents=True, exist_ok=True)
        cache_path.write_bytes(out)
    except OSError as exc:
        log(f"img_canvas cache write failed: {exc}")
    return out, (w, h), shown


# ------------------------------------------------------------- html in

class PageParser(HTMLParser):
    """Extract readable text + first <img> + title from a DB page.

    Text is restricted to <main> when the page has one; script/style/
    nav/header/footer/aside content is skipped.
    """

    SKIP = {"script", "style", "noscript", "nav", "header", "footer",
            "aside", "svg", "button", "form"}
    BLOCK = {"p", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
             "section", "article", "main", "table", "ul", "ol", "pre",
             "blockquote", "td", "th"}

    def __init__(self, require_main=False):
        super().__init__(convert_charrefs=True)
        self.require_main = require_main
        self.in_main = 0
        self.skip_depth = 0
        self.parts = []
        self.first_img = None
        self.title = ""

    def _active(self):
        return not self.require_main or self.in_main > 0

    def handle_starttag(self, tag, attrs):
        d = dict(attrs)
        if tag == "main":
            self.in_main += 1
        if tag in self.SKIP:
            self.skip_depth += 1
        if tag == "img" and self.first_img is None and d.get("src"):
            self.first_img = d["src"]
        if tag == "title":
            self.skip_depth += 1  # title handled via handle_data flag below
            self._in_title = True
        if self._active() and not self.skip_depth and tag in self.BLOCK:
            self.parts.append("\n")

    def handle_endtag(self, tag):
        if tag == "main" and self.in_main:
            self.in_main -= 1
        if tag in self.SKIP and self.skip_depth:
            self.skip_depth -= 1
        if tag == "title":
            if self.skip_depth:
                self.skip_depth -= 1
            self._in_title = False
        if self._active() and not self.skip_depth and tag in self.BLOCK:
            self.parts.append("\n")

    _in_title = False

    def handle_data(self, data):
        if getattr(self, "_in_title", False):
            self.title += data
            return
        if self.skip_depth or not self._active():
            return
        self.parts.append(data)

    def text(self):
        raw = "".join(self.parts)
        lines = [re.sub(r"\s+", " ", ln).strip() for ln in raw.splitlines()]
        out, blank = [], False
        for ln in lines:
            if ln:
                out.append(ln)
                blank = False
            elif not blank:
                out.append("")
                blank = True
        text = "\n".join(out).strip()
        if len(text) > MAX_TEXT:
            text = text[:MAX_TEXT] + "\n[... truncated ...]"
        return text


def parse_page(html):
    parser = PageParser(require_main=("<main" in html))
    parser.feed(html)
    return parser


class ListingParser(HTMLParser):
    """Map slug -> display name from a section listing page's anchors."""

    def __init__(self, section):
        super().__init__(convert_charrefs=True)
        self.prefix = f"/{section}/"
        self.cur = None
        self.names = {}

    def handle_starttag(self, tag, attrs):
        d = dict(attrs)
        if tag == "a" and d.get("href", "").startswith(self.prefix):
            slug = d["href"][len(self.prefix):].strip("/")
            if slug and "/" not in slug:
                self.cur = slug
        if tag == "img" and self.cur and self.cur not in self.names \
                and d.get("alt"):
            self.names[self.cur] = d["alt"]

    def handle_data(self, data):
        if self.cur and data.strip() and self.cur not in self.names:
            self.names[self.cur] = data.strip()

    def handle_endtag(self, tag):
        if tag == "a":
            self.cur = None


# ---------------------------------------------------------------- index

def build_index():
    entries = {}
    sitemap = fetch_bytes(f"{BASE}/sitemap.xml").decode("utf-8",
                                                        errors="replace")
    for url in re.findall(r"<loc>(.*?)</loc>", sitemap):
        path = urllib.parse.urlparse(url).path.strip("/")
        parts = path.split("/")
        if len(parts) != 2:
            continue
        section, slug = parts
        entries[(section, slug)] = {"section": section, "slug": slug,
                                    "name": slug}
    for section in LISTING_SECTIONS:
        if not any(s == section for s, _ in entries):
            continue
        try:
            html = fetch_bytes(f"{BASE}/{section}").decode(
                "utf-8", errors="replace")
        except ToolError as exc:
            log(f"index: listing fetch skipped: {exc}")
            continue
        parser = ListingParser(section)
        parser.feed(html)
        for slug, name in parser.names.items():
            if (section, slug) in entries:
                entries[(section, slug)]["name"] = name
    index = {"fetched_at": time.time(),
             "entries": sorted(entries.values(),
                               key=lambda e: (e["section"], e["slug"]))}
    try:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        INDEX_FILE.write_text(json.dumps(index), encoding="utf-8")
    except OSError as exc:
        log(f"index cache write failed: {exc}")
    return index


def load_index():
    try:
        if INDEX_FILE.is_file() and \
                time.time() - INDEX_FILE.stat().st_mtime < INDEX_TTL:
            return json.loads(INDEX_FILE.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        pass
    return build_index()


_INDEX = None


def get_index():
    global _INDEX
    if _INDEX is None:
        _INDEX = load_index()
    return _INDEX


def slugify(name):
    return re.sub(r"[^a-z0-9]+", "", (name or "").lower())


def page_url(section, slug):
    return f"{BASE}/{section}/{slug}"


def resolve(section, name):
    """Resolve a user-given name to an index entry in a section."""
    entries = [e for e in get_index()["entries"] if e["section"] == section]
    want = slugify(name)
    if not want:
        raise ToolError("name must not be empty")
    for e in entries:
        if e["slug"] == want:
            return e
    low = (name or "").strip().lower()
    for e in entries:
        if e["name"].lower() == low:
            return e
    tokens = [t for t in re.split(r"[^a-z0-9]+", low) if t]
    best = None
    for e in entries:
        ename = e["name"].lower()
        if tokens and all(t in ename or t in e["slug"] for t in tokens):
            if best is None or len(ename) < len(best["name"]):
                best = e
    if best:
        return best
    for e in entries:
        if want in e["slug"] or e["slug"] in want:
            return e
    raise ToolError(f"no {section} entry matching '{name}' on {BASE} "
                    f"(see {BASE}/{section})")


# ------------------------------------------------------- result shape

def make_result(header, body, image=None, image_url=None, image_sizes=None):
    """Tool fn return: dict consumed by handle() to build content."""
    lines = list(header)
    if image_url:
        if image_sizes:
            (ow, oh), (sw, sh) = image_sizes
            lines.append(f"Image: {image_url} (original {ow}x{oh}, "
                         f"sprite shown {sw}x{sh} on "
                         f"{CANVAS_SIZE}x{CANVAS_SIZE} canvas, "
                         f"nearest-neighbor)")
        else:
            lines.append(f"Image: {image_url}")
    text = "\n".join(lines) + "\n\n" + body
    return {"_text": text, "_image": image, "_image_url": image_url}


def get_entry_result(section, slug, name=None):
    html = fetch_page_html(section, slug)
    parser = parse_page(html)
    title = name or parser.title.split("—")[0].strip() or slug
    image, image_url = fetch_image(section, slug, parser.first_img)
    image_sizes = None
    if image is not None:
        image, orig, shown = canvas_image(
            image, IMG_CANVAS_DIR / f"{section}__{slug}.png")
        if orig is not None:
            image_sizes = (orig, shown)
    header = [f"{title} [{section}]", f"URL: {page_url(section, slug)}"]
    return make_result(header, parser.text(), image, image_url,
                       image_sizes)


# ---------------------------------------------------------------- tools

def tool_search(args):
    query = (args.get("query") or "").strip()
    if not query:
        raise ToolError("query must not be empty")
    try:
        limit = int(args.get("limit", 10))
    except (TypeError, ValueError):
        limit = 10
    limit = max(1, min(limit, 50))
    low = query.lower()
    want = slugify(query)
    tokens = [t for t in re.split(r"[^a-z0-9]+", low) if t]
    scored = []
    for e in get_index()["entries"]:
        ename, eslug = e["name"].lower(), e["slug"]
        score = 0
        if low in ename or want == eslug:
            score = 100
        elif want and want in eslug:
            score = 80
        elif tokens and all(t in ename or t in eslug for t in tokens):
            score = 60
        elif tokens and any(t in ename or t in eslug for t in tokens):
            score = 30
        if score:
            scored.append((-score, e["section"], eslug, e))
    scored.sort(key=lambda t: (t[0], t[1], t[2]))
    results = []
    for _s, _sec, _slug, e in scored[:limit]:
        r = {"section": e["section"], "name": e["name"],
             "slug": e["slug"], "url": page_url(e["section"], e["slug"])}
        if e["section"] in SPRITE_SECTIONS:
            img, url = fetch_image(e["section"], e["slug"])
            if img is not None:
                r["image_url"] = url
        results.append(r)
    payload = {"query": query, "count": len(results), "results": results}
    return {"_text": json.dumps(payload, indent=2), "_image": None,
            "_image_url": None}


def tool_get(args):
    section = slugify(args.get("section") or "")
    slug = slugify(args.get("slug") or "")
    if not section or not slug:
        raise ToolError("section and slug are required")
    known = {e["section"] for e in get_index()["entries"]}
    if section not in known:
        raise ToolError(f"unknown section '{section}' on {BASE}; "
                        f"known sections: {', '.join(sorted(known))}")
    entry = resolve(section, args.get("slug") or "")
    return get_entry_result(section, entry["slug"], entry["name"])


def tool_item(args):
    name = args.get("name") or ""
    entry = resolve("items", name)
    slug = entry["slug"]
    result = get_entry_result("items", slug, entry["name"])
    liquid_slugs = {e["slug"] for e in get_index()["entries"]
                    if e["section"] == "liquids"}
    if slug in liquid_slugs:
        html = fetch_page_html("liquids", slug)
        parser = parse_page(html)
        result["_text"] += (
            f"\n\n===== Liquid effects ({page_url('liquids', slug)}) "
            f"=====\n\n" + parser.text())
    return result


def tool_status(args):
    name = args.get("name") or ""
    entry = resolve("moodles", name)
    return get_entry_result("moodles", entry["slug"], entry["name"])


TOOLS = {
    "cu_search": {
        "description": ("Search the Casualties Unknown fan DB across all "
                        "sections (items, liquids, moodles/status effects, "
                        "recipes, enemies, buildings, commands, vitals...). "
                        "Returns matches with section, name, slug, page URL "
                        "and sprite image_url when one exists."),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "default": 10},
            },
            "required": ["query"],
        },
        "fn": tool_search,
    },
    "cu_get": {
        "description": ("Fetch one DB page live by section and slug (e.g. "
                        "section='liquids', slug='procoagulant') and return "
                        "its extracted text, plus the entry's image when "
                        "the page/sprite provides one."),
        "inputSchema": {
            "type": "object",
            "properties": {
                "section": {"type": "string"},
                "slug": {"type": "string"},
            },
            "required": ["section", "slug"],
        },
        "fn": tool_get,
    },
    "cu_item": {
        "description": ("Look up an item by name or slug. Returns the item "
                        "page text COMBINED with its liquid-effects page "
                        "text when a matching liquid exists, plus the "
                        "item's sprite image."),
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        },
        "fn": tool_item,
    },
    "cu_status": {
        "description": ("Look up a status effect / moodle by name or slug "
                        "(e.g. 'stroke'). Returns the moodles page text "
                        "plus its icon image when one exists."),
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        },
        "fn": tool_status,
    },
}


# ------------------------------------------------------------ protocol

def handle(msg):
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        return {
            "jsonrpc": "2.0", "id": msg_id,
            "result": {
                "protocolVersion": msg.get("params", {}).get(
                    "protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "cu-wiki-mcp", "version": "1.0.0"},
            },
        }
    if method == "ping":
        return {"jsonrpc": "2.0", "id": msg_id, "result": {}}
    if method == "tools/list":
        return {
            "jsonrpc": "2.0", "id": msg_id,
            "result": {
                "tools": [
                    {"name": name,
                     "description": spec["description"],
                     "inputSchema": spec["inputSchema"]}
                    for name, spec in TOOLS.items()
                ]
            },
        }
    if method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        spec = TOOLS.get(name)
        if spec is None:
            return {"jsonrpc": "2.0", "id": msg_id,
                    "error": {"code": -32602,
                              "message": f"unknown tool: {name}"}}
        try:
            result = spec["fn"](params.get("arguments", {}) or {})
        except ToolError as exc:
            return {"jsonrpc": "2.0", "id": msg_id,
                    "result": {"content": [{"type": "text",
                                            "text": f"Error: {exc}"}],
                               "isError": True}}
        except Exception as exc:  # keep the protocol alive on surprises
            log(f"tool {name} failed: {exc!r}")
            return {"jsonrpc": "2.0", "id": msg_id,
                    "result": {"content": [{"type": "text",
                                            "text": f"Error: {exc}"}],
                               "isError": True}}
        content = [{"type": "text", "text": result["_text"]}]
        if result.get("_image"):
            content.append({
                "type": "image",
                "mimeType": "image/png",
                "data": base64.b64encode(result["_image"]).decode("ascii"),
            })
        return {"jsonrpc": "2.0", "id": msg_id,
                "result": {"content": content}}
    if method and method.startswith("notifications/"):
        return None  # notifications get no response
    return {"jsonrpc": "2.0", "id": msg_id,
            "error": {"code": -32601,
                      "message": f"unknown method: {method}"}}


def main():
    log(f"cu-wiki-mcp: serving {BASE} (cache: {CACHE_DIR})")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as exc:
            sys.stdout.write(json.dumps(
                {"jsonrpc": "2.0", "id": None,
                 "error": {"code": -32700, "message": f"parse error: {exc}"}}
            ) + "\n")
            sys.stdout.flush()
            continue
        response = handle(msg)
        if response is not None:
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()


if __name__ == "__main__":
    main()
