#!/usr/bin/env python3
"""HTTP entry point for the Casualties Unknown wiki MCP (Vercel).

Exposes the same JSON-RPC tool logic as the stdio server
(server/cu_wiki_mcp.py) as a public, stateless, Streamable-HTTP-style
MCP endpoint:

    POST /mcp        (rewritten to /api/mcp by vercel.json; /api/mcp
                      also works directly)

- Requests are single JSON-RPC 2.0 messages (a batch array is also
  accepted). Responses are plain application/json — no SSE stream,
  no session, no auth: the endpoint is read-only and public.
- Notifications (messages without an id) are answered 202 with an
  empty body, per the Streamable HTTP transport.
- GET returns a small JSON descriptor of the endpoint (it does not
  open an SSE stream).

Serverless notes: the filesystem is ephemeral, so the disk cache is
redirected to /tmp via CU_CACHE_DIR before the server module is
imported. The tool functions themselves are shared unchanged with the
stdio server — this file is only a transport adapter.
"""

import importlib.util
import json
import os
import sys
from http.server import BaseHTTPRequestHandler
from pathlib import Path

os.environ.setdefault("CU_CACHE_DIR", "/tmp/cu-wiki-cache")

_ROOT = Path(__file__).resolve().parent.parent


def _load_server_module():
    """Load server/cu_wiki_mcp.py by file path (no sys.path games, so
    the import works identically locally and in the Vercel bundle)."""
    path = _ROOT / "server" / "cu_wiki_mcp.py"
    spec = importlib.util.spec_from_file_location("cu_wiki_mcp", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


cu_wiki_mcp = _load_server_module()

INFO = {
    "name": "cu-wiki-mcp",
    "version": cu_wiki_mcp.__dict__.get("__version__", "1.0.0"),
    "transport": "streamable-http (stateless, JSON responses)",
    "tools": sorted(cu_wiki_mcp.TOOLS),
    "data_source": cu_wiki_mcp.BASE,
    "usage": "POST a JSON-RPC 2.0 message: initialize, tools/list, "
             "tools/call, ping.",
}


def process(raw):
    """One raw request body -> (http_status, response_dict_or_None)."""
    try:
        msg = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as exc:
        return 400, {"jsonrpc": "2.0", "id": None,
                     "error": {"code": -32700,
                               "message": f"parse error: {exc}"}}
    if isinstance(msg, list):  # JSON-RPC batch
        responses = [r for r in (cu_wiki_mcp.handle(m) for m in msg)
                     if r is not None]
        if not responses:
            return 202, None
        return 200, responses
    if not isinstance(msg, dict):
        return 400, {"jsonrpc": "2.0", "id": None,
                     "error": {"code": -32600,
                               "message": "expected a JSON-RPC object"}}
    response = cu_wiki_mcp.handle(msg)
    if response is None:
        return 202, None  # notification: accepted, no body
    return 200, response


class handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "cu-wiki-mcp/1.0"

    def log_message(self, *args):  # keep function logs clean
        pass

    def _send(self, status, payload):
        body = b"" if payload is None else \
            json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods",
                         "POST, GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers",
                         "Content-Type, Accept, Mcp-Session-Id, "
                         "MCP-Protocol-Version")
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path in ("/", "/index.html"):
            # Entrypoint routing may send / here instead of to the
            # static file; serve the landing page either way.
            try:
                html = (_ROOT / "index.html").read_bytes()
            except OSError:
                html = (b"<!DOCTYPE html><title>Casualties Unknown Wiki "
                        b"MCP</title><p>POST JSON-RPC to /mcp.</p>")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(html)))
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(html)
            return
        self._send(200, INFO)

    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        raw = self.rfile.read(length) if length > 0 else b""
        try:
            status, payload = process(raw)
        except Exception as exc:  # never hang the transport
            cu_wiki_mcp.log(f"http handler failed: {exc!r}")
            status, payload = 500, {
                "jsonrpc": "2.0", "id": None,
                "error": {"code": -32603, "message": f"internal: {exc}"}}
        self._send(status, payload)

    def do_OPTIONS(self):
        self._send(204, None)
