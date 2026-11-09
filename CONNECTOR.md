# Casualties Unknown Wiki

## Purpose
Read-only access to the unofficial fan database of the game
**Casualties Unknown** at
[casualtiesunknowndb.com](https://casualtiesunknowndb.com) — items,
liquids/drugs and their effects, status effects (moodles), recipes,
enemies & hazards, buildings, console commands, vitals, depth layers,
difficulty. Every lookup is fetched **live** from the site on demand and
returned **with the entry's image** (sprite icon) as an MCP image block,
not just text. No account, no API key, no credentials anywhere.

## Commands
MCP wrapper: `bin/cu-wiki-mcp` — spawns `server/cu_wiki_mcp.py`
(stdlib-only Python, newline JSON-RPC 2.0 over stdio) once per
invocation, shared by every `--name/--arguments` pair in order.

```sh
bin/cu-wiki-mcp list-tools
bin/cu-wiki-mcp call-tool --name cu_search --arguments '{"query":"stroke"}'
bin/cu-wiki-mcp call-tool --name cu_item --arguments '{"name":"sodium nitroprusside"}'
bin/cu-wiki-mcp call-tool --name cu_status --arguments '{"name":"stroke"}'
bin/cu-wiki-mcp call-tool --name cu_get --arguments '{"section":"liquids","slug":"procoagulant"}'
```

Allowed tools (hard-gated in the wrapper's `ALLOWED_TOOLS`; anything
else is refused):

- `cu_search(query, limit=10)` — search across all sections; returns
  section, display name, slug, page URL, and `image_url` when a sprite
  exists.
- `cu_get(section, slug)` — fetch one page live, return its extracted
  text plus its image (sprite for items/liquids/moodles, else the
  page's first `<img>`) when available.
- `cu_item(name)` — resolve an item by name/slug; returns the `/items`
  page text **combined with** the matching `/liquids` page text
  (drink/injection effects) when a liquid with the same slug exists,
  plus the item's sprite image block and its `image_url`.
- `cu_status(name)` — the `/moodles` page for a status effect, plus its
  icon image when one exists.

In the wrapper's printed output, image blocks are slimmed to
`{"type":"image","mimeType":"image/png","bytes":N}` (decoded byte
count); MCP clients calling the server directly receive the full
base64 image block.

## How it works
- **Live fetch only.** Page HTML is fetched on demand with a 20s
  timeout and a descriptive User-Agent; failures come back as tool
  errors naming the URL. Nothing is bulk-mirrored.
- **Search index:** built from `https://casualtiesunknowndb.com/sitemap.xml`
  (robots.txt allows all); display names are enriched once from the
  `/items`, `/moodles` and other section listing pages. Cached in
  `cache/index.json` for 24h.
- **Caches** (all under `cache/` next to the server): index 24h, page
  HTML 1h (`cache/pages/`), sprite PNGs 24h (`cache/img/`), composed
  presentation images 24h (`cache/img_canvas/`). Delete `cache/` to
  force a full refresh.
- **Images:** items/liquids/moodles use the canonical sprite
  `https://casualtiesunknowndb.com/icons/sprites/<slug>.png`, fetched
  and returned as a real MCP image content block alongside the text
  (the text also carries the `image_url`). Other sections fall back to
  the page's first `<img>`, else text only.
- **Presentation (zoomed-out canvas):** the source sprites are only
  4–32px pixel art. Returning them raw lets chat clients
  smooth-upscale them into a blurry mush, and returning them
  full-bleed makes tiny items fill the whole chat view so the shape
  can't be taken in. The server therefore returns every image as a
  **480x480 transparent canvas** with the sprite nearest-neighbor
  scaled to fit **within 288x288** (integer factor for small sprites,
  PIL `Image.NEAREST`, alpha preserved) and pasted centered — the
  sprite occupies at most 60% of the frame, whole, with margin
  around it, and the pixels stay crisp and faithful. The tool text
  notes both sizes, e.g. `Image: <url> (original 4x6, sprite shown
  192x288 on 480x480 canvas, nearest-neighbor)`. Composed PNGs are
  cached in `cache/img_canvas/`; if PIL is unavailable the original
  sprite is returned unchanged.
- Names resolve flexibly: exact slug, exact display name, or token
  match (so `"sodium nitroprusside"` finds slug `sodiumnitroprusside`).

## Operating rules
- Use this connector for any Casualties Unknown game-data question —
  don't answer item/effect questions from memory or general web search.
- When presenting an item or status effect, include its image (the
  image block / `image_url` the tool returned).
- Read-only: the server has no write tools and never posts to the site.
- Keep request volume human-scale: one lookup per question, rely on
  the caches instead of refetching.

## Limitations
- Unofficial fan data: values reflect the fan DB's extraction of the
  current game build and can lag or diverge after game updates.
- Display names for sections without listing-page names fall back to
  the raw slug.
- Extracted text is capped at 15,000 characters per page.
- A moodle/item whose sprite is missing upstream returns text only.

## Attribution
All data and images come from the unofficial fan database at
**casualtiesunknowndb.com**. This connector is **not affiliated with
the game developer** (Casualties Unknown, formerly Scav Prototype).
Content is fetched live per lookup and cached briefly as described
above; nothing is bulk-copied or redistributed.
