# Casualties Unknown Wiki MCP

An MCP server for the Casualties Unknown fan wiki database. It searches and
reads items, liquids, moodles/status effects, recipes, and more, and returns
each entry **with its sprite image**.

Casualties Unknown (formerly Scav Prototype) is a survival game with a deep
medical simulation — this server makes its wiki data queryable from any MCP
client.

## Use it remotely — no install

The server is deployed publicly on Vercel as a stateless remote MCP
(Streamable HTTP transport, JSON responses, no auth, no API key):

**Endpoint: `https://casualties-unknown-wiki-mcp.vercel.app/mcp`**

Point any MCP client that supports remote HTTP servers at it:

```json
{
  "mcpServers": {
    "casualties-unknown": {
      "url": "https://casualties-unknown-wiki-mcp.vercel.app/mcp"
    }
  }
}
```

Stdio-only clients can bridge through a proxy such as
[mcp-proxy](https://github.com/sparfenyuk/mcp-proxy) or
`npx mcp-remote https://casualties-unknown-wiki-mcp.vercel.app/mcp`.
The landing page at <https://casualties-unknown-wiki-mcp.vercel.app/>
summarizes the same details.

The hosted instance is this exact code: `api/mcp.py` serves the shared
JSON-RPC handler from `server/cu_wiki_mcp.py` over HTTP, with the disk
cache redirected to `/tmp` (`CU_CACHE_DIR`) since serverless storage is
ephemeral. This repository is git-connected to the Vercel project, so
pushes to `main` redeploy automatically.

## Tools

| Tool | What it does |
|---|---|
| `cu_search` | Search everything by name. Example: `{"query": "stroke"}` |
| `cu_item` | Item page plus its liquid effects in one result. Example: `{"name": "sodium nitroprusside"}` |
| `cu_status` | Moodle / status effect lookup. Example: `{"name": "stroke"}` |
| `cu_get` | Read any entry by section and slug. Example: `{"path": "liquids/procoagulant"}` |

## Data source

All data comes from the **unofficial fan database**
[casualtiesunknowndb.com](https://casualtiesunknowndb.com). Pages are fetched
live on demand and cached locally (index 24h, pages 1h, images 24h) — nothing
is bulk-mirrored.

This project is **not affiliated with the game developer**. Game data and
sprites belong to their respective owners; this is a fan tool.

## Images

The source sprites are tiny pixel art (4–32 px). Returned raw, chat clients
stretch them into a blur. The server therefore upscales each sprite with
**nearest-neighbor** scaling (pixel-faithful — no smoothing, no invented
detail) and centers it on a transparent 480×480 canvas, with the sprite
itself at most 288 px, so small items stay readable at a glance. The text
output notes the original and displayed sizes, plus the source image URL.

## Requirements

- Python 3 (standard library only for the server itself)
- Pillow — optional. Used for the sprite upscaling; without it the server
  gracefully falls back to returning the original sprite bytes.

## Running it locally (stdio)

The server also speaks newline-delimited JSON-RPC 2.0 (MCP) over stdio.
The wrapper script is the convenient entry point:

```sh
bin/cu-wiki-mcp list-tools
bin/cu-wiki-mcp call-tool --name cu_item --arguments '{"name": "procoagulant"}'
bin/cu-wiki-mcp call-tool --name cu_status --arguments '{"name": "stroke"}'
```

`connector.yaml` and `CONNECTOR.md` describe how the server is registered as
a connector (id `custom.casualties-unknown`), and `skill/SKILL.md` is the
agent skill that routes game questions to these tools.

## Layout

```
server/cu_wiki_mcp.py   the MCP server (shared JSON-RPC logic + stdio entry)
api/mcp.py              HTTP entry point for Vercel (Streamable HTTP)
index.html              public landing page for the hosted instance
requirements.txt        Pillow (Vercel installs it; optional locally)
vercel.json             /mcp rewrite + function config
bin/cu-wiki-mcp         CLI wrapper (list-tools / call-tool)
connector.yaml          connector registration
CONNECTOR.md            connector documentation
skill/SKILL.md          agent skill for this server
```
