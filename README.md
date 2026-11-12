# Casualties Unknown Wiki MCP

An MCP server for the Casualties Unknown fan wiki database. It searches and
reads items, liquids, moodles/status effects, recipes, and more, and returns
each entry **with its sprite image**.

Casualties Unknown (formerly Scav Prototype) is a survival game with a deep
medical simulation — this server makes its wiki data queryable from any MCP
client.

## Use it remotely — no install

The server runs as a Python function on Vercel as a stateless remote MCP
(Streamable HTTP transport, JSON responses, no auth, no API key):

**Endpoint: `https://casualties-unknown-wiki-mcp.vercel.app/mcp`**

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
The landing page at
<https://casualties-unknown-wiki-mcp.vercel.app/> summarizes the
same details.

**x402 paywall: integrated, OFF pending CDP key.** Both servers
implement the x402 v1 paywall — `cu_get`, `cu_item` and `cu_status` at
$0.005/call in USDC on Base, `initialize` / `ping` / `tools/list` /
`cu_search` free — but it is disabled in the live deployments
(`X402_ENABLED=false`; on Vercel it is an env var, on k3s it is in
`k8s/deployment.yaml`) until the CDP facilitator Secret API key is
created and mounted. The Python paywall disarms itself to free serving
if enabled without a usable configuration; the Rust paywall fails
closed (paid tools answer 402, never served free). See
`X402_RELEASE.md` for the full spec and go-live steps.

**Alternate endpoint (also live):**
`https://8080-agqxcobsjbzbmcbys3qqt35mxi.instabox.tech/mcp` — the
same tools served by the Rust binary (`src/main.rs`) on a k3s cluster
(hostNetwork on node1, public port published via InstaBox). It is the
fallback if Vercel is down. The k3s instance is built by
`.github/workflows/build-push.yml` into
`ghcr.io/agentpixelated/casualties-unknown-wiki-mcp:1.0.0` and deployed
by `k8s/deployment.yaml` (1 replica, `--http 8080`, disk cache on a
1Gi local-path PVC at `/app/cache`).

Note: the Vercel project is **not** git-attached (a team-scope grant is
still missing), so pushes to `main` do **not** redeploy it — Vercel
deploys are made inline from this tree. The k3s image rebuilds on push
to `main` via GitHub Actions.

> Status 2026-11-12: the Vercel redeploy of this tree (Python + x402
> code) is pending — the Vercel connection lost access to the
> `agentpixie` team scope (403 on every API call) and must be
> re-granted before a new production deployment can be made. The live
> Vercel endpoint meanwhile still serves the 9 Nov build (same 4 tools,
> no x402 code yet); the k3s endpoint above already serves the Rust
> build with the x402 paywall integrated (OFF).

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

- Python 3 (standard library only for the server itself) — for the
  live Vercel server (`server/`, `api/mcp.py`).
- Rust (stable) + Cargo — for the alternate k3s server (`src/`). No runtime
  dependencies beyond system CA roots; the k3s image is debian-slim.
- Pillow — optional. Used for the sprite upscaling; without it the server
  gracefully falls back to returning the original sprite bytes.

## Running it locally (stdio)

The server also speaks newline-delimited JSON-RPC 2.0 (MCP) over stdio.
The Rust binary is the local stdio server (drop-in for the Python
one — same tools, same output):

```sh
cargo run --release -- list-tools   # or: ./target/release/cu-wiki-mcp
```

The Python wrapper below still works against `server/cu_wiki_mcp.py`:

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
src/main.rs              the Rust MCP server (live on k3s; stdio + --http)
src/x402.rs              x402 v1 paywall (integrated, OFF pending CDP key)
Cargo.toml / Cargo.lock  Rust manifest (binary: cu-wiki-mcp 1.0.0)
Dockerfile               multi-stage build -> debian-slim runtime image
.github/workflows/build-push.yml   GHCR image build on push to main
k8s/deployment.yaml      k3s manifests (namespace + PVC + Deployment)
server/cu_wiki_mcp.py   the Python MCP server (live on Vercel)
api/mcp.py              HTTP entry point for Vercel (Streamable HTTP) + x402 paywall
X402_RELEASE.md         x402 spec, verification evidence, go-live steps
pyproject.toml          Vercel Python entrypoint declaration + Pillow dep
index.html              public landing page for the hosted instance
requirements.txt        Pillow (Vercel installs it; optional locally)
vercel.json             /mcp rewrite + function config
bin/cu-wiki-mcp         CLI wrapper (list-tools / call-tool)
connector.yaml          connector registration
CONNECTOR.md            connector documentation
skill/SKILL.md          agent skill for this server
```
