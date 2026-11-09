# Casualties Unknown Wiki — game data with images

Answer Casualties Unknown game-data questions from the unofficial fan
DB via its MCP server. Full connector doc:
`~/workspace/custom-connectors/custom.casualties-unknown/CONNECTOR.md`.

## When to use
Any question about Casualties Unknown (formerly Scav Prototype) game
data: items and what they do, liquids/drugs and their drink/injection
effects, status effects / moodles (e.g. stroke, hypertensive crisis),
recipes, enemies & hazards, buildings, console commands, vitals and
survival thresholds, depth layers, difficulty. If the user names a drug,
item, or condition from the game, this skill applies — check the game
data before answering, never from real-world knowledge or memory.

## Commands
```sh
CU=~/workspace/custom-connectors/custom.casualties-unknown/bin/cu-wiki-mcp
$CU list-tools
$CU call-tool --name cu_search --arguments '{"query":"stroke"}'
$CU call-tool --name cu_item --arguments '{"name":"sodium nitroprusside"}'
$CU call-tool --name cu_status --arguments '{"name":"stroke"}'
$CU call-tool --name cu_get --arguments '{"section":"liquids","slug":"procoagulant"}'
```
Multiple `--name/--arguments` pairs in one call run in one session.

- `cu_search` — find entries across sections (returns name, slug, URL,
  sprite `image_url`).
- `cu_item` — item page + its liquid-effects page combined, + sprite.
  **Default for any item/drug question.**
- `cu_status` — a moodle/status effect page, + its icon.
- `cu_get` — any other single page by section + slug.

## Rules
- Item/effect answers MUST come from this MCP — the game's numbers
  routinely differ from real-world pharmacology and from intuition
  (e.g. in-game stroke is treated with procoagulant/Braingrow, not
  with the blood-pressure drug that caused it).
- When presenting an item or status effect, **include its image**:
  pass through the image block / link the `image_url` the tool
  returned. Data without the image is an incomplete answer here.
  Sprites come back **zoomed out on a 480x480 transparent canvas** on
  purpose — the source sprites are 4–32px pixel art, so the server
  nearest-neighbor scales them to fit within 288x288 and centers them
  with margin (at most 60% of the frame), instead of a full-bleed
  upscale that fills the whole chat view or a raw sprite clients
  smooth into a blurry smear. The composed image is pixel-faithful,
  don't "fix" or regenerate it.
- Quote the DB's per-mL / per-dose effect numbers exactly as returned;
  don't round, convert, or substitute real-world dosages.
- Data is live-fetched from casualtiesunknowndb.com (unofficial fan
  DB, not the developer) and cached briefly (index 24h, pages 1h) —
  if an answer looks stale after a game update, clear
  `~/workspace/custom-connectors/custom.casualties-unknown/cache/` and
  retry once.
