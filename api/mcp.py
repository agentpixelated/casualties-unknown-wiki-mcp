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

Optional x402 monetisation (HTTP layer only; the stdio server and the
shared tool logic are untouched):

- DISABLED by default. Set X402_ENABLED=1 (plus X402_PAY_TO) to arm
  the paywall. While disabled, behaviour is byte-for-byte the public
  behaviour this endpoint has always had.
- When armed, the MCP split follows the official x402 pattern:
  initialize / ping / tools/list stay free, and so do the tools in
  the free set (default: cu_search). tools/call for a paid tool
  (default: cu_get, cu_item, cu_status) without an X-PAYMENT header
  is answered HTTP 402 with x402 payment requirements (scheme
  "exact", USDC). A batch is charged once, for the sum of its paid
  calls. Payment verification and settlement are delegated to a
  facilitator (default https://x402.org/facilitator — testnet);
  content is only served after the facilitator settles, and the
  settlement receipt is returned in the X-PAYMENT-RESPONSE header.
- Configuration is all environment variables (see X402_RELEASE.md):
  X402_ENABLED, X402_PAY_TO, X402_NETWORK (base-sepolia | base |
  solana-devnet | solana), X402_FACILITATOR_URL, X402_PAID_TOOLS,
  X402_PRICE_USD, X402_PRICE_<TOOL>, X402_RESOURCE_URL.
- Mainnet facilitator: when X402_FACILITATOR_URL points at the
  Coinbase CDP facilitator (https://api.cdp.coinbase.com/platform/
  v2/x402), every facilitator call carries a fresh CDP JWT
  (Authorization: Bearer) generated from CDP_API_KEY_ID +
  CDP_API_KEY_SECRET — but only when both are set. The JWT follows
  the official CDP API Authentication spec (generateJwt): header
  {alg, typ:"JWT", kid, nonce}, claims {sub, iss:"cdp",
  aud:["cdp_service"], nbf, exp:+120s, uri/uris bound to the exact
  method+host+path}. Both CDP secret formats are supported —
  Ed25519 (base64, 64-byte) signed EdDSA, and EC P-256 PEM signed
  ES256 — implemented in pure stdlib (no new dependencies). The
  unauthenticated x402.org testnet path is unchanged.
- If X402_ENABLED is set but the configuration is unusable (no
  pay-to address, unknown network, bad price), the paywall disarms
  itself and logs loudly instead of breaking the public endpoint.

Serverless notes: the filesystem is ephemeral, so the disk cache is
redirected to /tmp via CU_CACHE_DIR before the server module is
imported. The tool functions themselves are shared unchanged with the
stdio server — this file is only a transport adapter.
"""

import base64
import hashlib
import hmac as hmac_mod
import importlib.util
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.request
from decimal import Decimal, InvalidOperation
from http.server import BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlparse

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


# ------------------------------------------------------------------ x402

X402_VERSION = 1
X402_DEFAULT_FACILITATOR = "https://x402.org/facilitator"
_CDP_HOST = "api.cdp.coinbase.com"
CDP_FACILITATOR_URL = "https://api.cdp.coinbase.com/platform/v2/x402"
# network -> (USDC asset/contract/mint, EIP-712 domain name, version)
# Names read from the contracts themselves (name()): the Base Sepolia
# test token is named "USDC", mainnet USDC is "USD Coin" — the x402
# facilitator rejects requirements whose extra.name differs.
_NETWORK_ASSETS = {
    "base-sepolia": ("0x036CbD53842c5426634e7929541eC2318f3dCF7e",
                     "USDC", "2"),
    "base": ("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
             "USD Coin", "2"),
    "solana-devnet": ("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
                      None, None),
    "solana": ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
               None, None),
}
_EVM_NETWORKS = {"base-sepolia", "base"}
_USDC_DECIMALS = 6

_solana_fee_payer_cache = {}


def _usd_to_atomic(text):
    """'$0.005' / '0.005' -> 5000 (USDC has 6 decimals)."""
    value = Decimal(str(text).strip().lstrip("$"))
    atomic = int((value * (10 ** _USDC_DECIMALS)).to_integral_value())
    if atomic <= 0:
        raise InvalidOperation("price must be positive")
    return atomic


def _atomic_to_usd(atomic):
    text = f"{atomic / (10 ** _USDC_DECIMALS):f}"
    return "$" + text.rstrip("0").rstrip(".")


def _x402_config():
    """Parsed x402 config, or None when the paywall is disarmed.

    Disarmed = X402_ENABLED unset/off, OR enabled but unusable (the
    endpoint must never break for free users because of a bad
    monetisation setting; the misconfiguration is logged instead).
    """
    if os.environ.get("X402_ENABLED", "").strip().lower() \
            not in ("1", "true", "yes", "on"):
        return None
    pay_to = (os.environ.get("X402_PAY_TO") or "").strip()
    network = (os.environ.get("X402_NETWORK")
               or "base-sepolia").strip()
    if not pay_to:
        cu_wiki_mcp.log("x402: X402_ENABLED set but X402_PAY_TO is "
                        "empty — paywall DISARMED")
        return None
    if network not in _NETWORK_ASSETS:
        cu_wiki_mcp.log(f"x402: unknown X402_NETWORK {network!r} — "
                        "paywall DISARMED")
        return None
    paid = [t.strip() for t in
            (os.environ.get("X402_PAID_TOOLS")
             or "cu_get,cu_item,cu_status").split(",") if t.strip()]
    prices = {}
    for tool in paid:
        raw = (os.environ.get("X402_PRICE_" + tool.upper())
               or os.environ.get("X402_PRICE_USD") or "0.005")
        try:
            prices[tool] = _usd_to_atomic(raw)
        except (InvalidOperation, ValueError):
            cu_wiki_mcp.log(f"x402: bad price {raw!r} for {tool} — "
                            "paywall DISARMED")
            return None
    facilitator = (os.environ.get("X402_FACILITATOR_URL")
                   or X402_DEFAULT_FACILITATOR).rstrip("/")
    cdp_key_id = (os.environ.get("CDP_API_KEY_ID") or "").strip()
    cdp_key_secret = (os.environ.get("CDP_API_KEY_SECRET") or "").strip()
    is_cdp = urlparse(facilitator).hostname == _CDP_HOST
    if is_cdp and not (cdp_key_id and cdp_key_secret):
        cu_wiki_mcp.log("x402: facilitator is Coinbase CDP but "
                        "CDP_API_KEY_ID / CDP_API_KEY_SECRET are not "
                        "both set — facilitator calls will go out "
                        "UNAUTHENTICATED and CDP will reject them")
    return {
        "pay_to": pay_to,
        "network": network,
        "facilitator": facilitator,
        "prices": prices,
        "cdp_key_id": cdp_key_id,
        "cdp_key_secret": cdp_key_secret,
        "is_cdp": is_cdp,
    }


def _x402_public():
    """Paywall summary for the GET descriptor (payTo is public by
    design — it is printed in every 402 response)."""
    cfg = _x402_config()
    if cfg is None:
        return {"enabled": False}
    free = sorted(set(cu_wiki_mcp.TOOLS) - set(cfg["prices"]))
    return {
        "enabled": True,
        "x402Version": X402_VERSION,
        "network": cfg["network"],
        "asset": _NETWORK_ASSETS[cfg["network"]][0],
        "payTo": cfg["pay_to"],
        "paidTools": {t: _atomic_to_usd(p)
                      for t, p in sorted(cfg["prices"].items())},
        "freeTools": free,
        "freeMethods": ["initialize", "ping", "tools/list"],
    }


def _solana_fee_payer(cfg):
    """feePayer for Solana requirements, from the facilitator's
    /supported document (cached per facilitator+network)."""
    key = (cfg["facilitator"], cfg["network"])
    if key in _solana_fee_payer_cache:
        return _solana_fee_payer_cache[key]
    fee_payer = None
    try:
        req = urllib.request.Request(
            cfg["facilitator"] + "/supported",
            headers=_facilitator_headers(cfg, "GET", "supported"))
        with urllib.request.urlopen(req, timeout=15) as resp:
            doc = json.loads(resp.read().decode("utf-8"))
        for kind in doc.get("kinds", []):
            if kind.get("x402Version") == X402_VERSION \
                    and kind.get("network") == cfg["network"]:
                fee_payer = (kind.get("extra") or {}).get("feePayer")
                break
    except Exception as exc:  # requirements just omit feePayer
        cu_wiki_mcp.log(f"x402: /supported fetch failed: {exc!r}")
    _solana_fee_payer_cache[key] = fee_payer
    return fee_payer


def _payment_requirements(cfg, total_atomic, resource, tool_names):
    asset, dom_name, dom_version = _NETWORK_ASSETS[cfg["network"]]
    req = {
        "scheme": "exact",
        "network": cfg["network"],
        "maxAmountRequired": str(total_atomic),
        "resource": resource or "/mcp",
        "description": "Casualties Unknown Wiki MCP paid tool "
                       "call(s): " + ", ".join(tool_names),
        "mimeType": "application/json",
        "payTo": cfg["pay_to"],
        "maxTimeoutSeconds": 60,
        "asset": asset,
    }
    if cfg["network"] in _EVM_NETWORKS:
        req["extra"] = {"name": dom_name, "version": dom_version}
    else:
        fee_payer = _solana_fee_payer(cfg)
        if fee_payer:
            req["extra"] = {"feePayer": fee_payer}
    return req


def _402_body(requirements, error):
    return {"x402Version": X402_VERSION, "error": error,
            "accepts": [requirements]}


# ------------------------------------------------------------- CDP JWT
# Coinbase CDP facilitator auth, per the official CDP "API
# Authentication" docs (docs.cdp.coinbase.com/api-reference/v2/
# authentication): a Secret API Key (key id + key secret, created in
# the CDP Portal under API Keys -> Secret API Keys) is used to mint a
# short-lived JWT per request, sent as "Authorization: Bearer <jwt>".
#   header: {alg, typ: "JWT", kid: <key id>, nonce: <random hex>}
#   claims: {sub: <key id>, iss: "cdp", aud: ["cdp_service"],
#            nbf: now, exp: now + 120,
#            uri: "<METHOD> <host><path>",
#            uris: ["<METHOD> <host><path>"]}
# (Both uri and uris are emitted: the v2 docs' samples use "uri",
# the CDP SDK's generateJwt uses "uris"; the signature covers both,
# so the verifier can read whichever it expects.)
# Secret formats issued by the portal: Ed25519 = base64 of a 64-byte
# key (32-byte seed + 32-byte public key) -> alg EdDSA; ECDSA = PEM
# EC P-256 private key ("BEGIN EC PRIVATE KEY" / PKCS#8) -> ES256.
# Both signers are implemented in pure stdlib below — the endpoint
# deliberately has no third-party dependencies.

def _b64u(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


# --- Ed25519 (RFC 8032), pure Python

_ED_Q = 2 ** 255 - 19
_ED_L = 2 ** 252 + 27742317777372353535851937790883648493
_ED_D = (-121665 * pow(121666, _ED_Q - 2, _ED_Q)) % _ED_Q
_ED_B = (int("151122213495354007725011514095885315114"
             "54012693041857206046113283949847762202"),
         int("463168356949264781694283940034751631413"
             "07993866256225615783033603165251855960"))


def _ed_add(p, q):
    x1, y1 = p
    x2, y2 = q
    t = _ED_D * x1 * x2 * y1 * y2 % _ED_Q
    return ((x1 * y2 + x2 * y1) * pow(1 + t, _ED_Q - 2, _ED_Q) % _ED_Q,
            (y1 * y2 + x1 * x2) * pow(1 - t, _ED_Q - 2, _ED_Q) % _ED_Q)


def _ed_mul(p, e):
    result = (0, 1)
    while e:
        if e & 1:
            result = _ed_add(result, p)
        p = _ed_add(p, p)
        e >>= 1
    return result


def _ed_encode_point(p):
    x, y = p
    bits = [(y >> i) & 1 for i in range(255)] + [x & 1]
    return bytes(sum(bits[i * 8 + j] << j for j in range(8))
                 for i in range(32))


def _ed_sign(seed, msg):
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    pub = _ed_encode_point(_ed_mul(_ED_B, a))
    r = int.from_bytes(hashlib.sha512(h[32:] + msg).digest(),
                       "little") % _ED_L
    r_enc = _ed_encode_point(_ed_mul(_ED_B, r))
    k = int.from_bytes(hashlib.sha512(r_enc + pub + msg).digest(),
                       "little") % _ED_L
    s = (r + k * a) % _ED_L
    return r_enc + s.to_bytes(32, "little")


# --- ECDSA P-256 / ES256 (RFC 6979 deterministic), pure Python

_P256_P = 0xffffffff00000001000000000000000000000000ffffffffffffffffffffffff
_P256_N = 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551
_P256_G = (0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296,
           0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5)


def _ec_add(p, q):
    if p is None:
        return q
    if q is None:
        return p
    x1, y1 = p
    x2, y2 = q
    if x1 == x2 and (y1 + y2) % _P256_P == 0:
        return None
    if p == q:
        m = (3 * x1 * x1 - 3) * pow(2 * y1, _P256_P - 2, _P256_P)
    else:
        m = (y2 - y1) * pow(x2 - x1, _P256_P - 2, _P256_P)
    m %= _P256_P
    x3 = (m * m - x1 - x2) % _P256_P
    return x3, (m * (x1 - x3) - y1) % _P256_P


def _ec_mul(p, e):
    result = None
    while e:
        if e & 1:
            result = _ec_add(result, p)
        p = _ec_add(p, p)
        e >>= 1
    return result


def _es256_sign(priv, msg):
    digest = hashlib.sha256(msg).digest()
    z = int.from_bytes(digest, "big")
    bx = priv.to_bytes(32, "big")
    bh = (z - _P256_N if z >= _P256_N else z).to_bytes(32, "big")
    v = b"\x01" * 32
    k = b"\x00" * 32
    k = hmac_mod.new(k, v + b"\x00" + bx + bh, hashlib.sha256).digest()
    v = hmac_mod.new(k, v, hashlib.sha256).digest()
    k = hmac_mod.new(k, v + b"\x01" + bx + bh, hashlib.sha256).digest()
    v = hmac_mod.new(k, v, hashlib.sha256).digest()
    while True:
        v = hmac_mod.new(k, v, hashlib.sha256).digest()
        nonce = int.from_bytes(v, "big")
        if 1 <= nonce < _P256_N:
            r = _ec_mul(_P256_G, nonce)[0] % _P256_N
            if r:
                s = pow(nonce, -1, _P256_N) * (z + r * priv) % _P256_N
                if s:
                    s = min(s, _P256_N - s)  # low-s normal form
                    return r.to_bytes(32, "big") + s.to_bytes(32, "big")
        k = hmac_mod.new(k, v + b"\x00", hashlib.sha256).digest()
        v = hmac_mod.new(k, v, hashlib.sha256).digest()


# --- CDP secret parsing

def _der_item(data, i):
    """(tag, content, next_index) for one DER TLV at offset i."""
    tag = data[i]
    i += 1
    length = data[i]
    i += 1
    if length & 0x80:
        n = length & 0x7F
        length = int.from_bytes(data[i:i + n], "big")
        i += n
    return tag, data[i:i + length], i + length


def _pkcs8_inner_octet(der):
    """Private-key octet string inside a PKCS#8 SEQUENCE."""
    tag, seq, _ = _der_item(der, 0)
    if tag != 0x30:
        raise ValueError("not a SEQUENCE")
    _, _, i = _der_item(seq, 0)          # version INTEGER
    _, _, i = _der_item(seq, i)          # algorithm SEQUENCE
    tag, octet, _ = _der_item(seq, i)
    if tag != 0x04:
        raise ValueError("no private-key octet string")
    return octet


def _ec_scalar_from_der(der):
    """Private scalar from a SEC1 ECPrivateKey or a PKCS#8 blob."""
    tag, seq, _ = _der_item(der, 0)
    if tag != 0x30:
        raise ValueError("not a SEQUENCE")
    _, _, i = _der_item(seq, 0)          # version INTEGER
    tag, content, _ = _der_item(seq, i)
    if tag == 0x30:                      # PKCS#8: unwrap, recurse
        return _ec_scalar_from_der(_pkcs8_inner_octet(der))
    if tag != 0x04:
        raise ValueError("no EC private octet string")
    return int.from_bytes(content, "big")


def _ed_seed_from_der(der):
    inner = _pkcs8_inner_octet(der)      # CurvePrivateKey bytes
    tag, seed, _ = _der_item(inner, 0)
    if tag != 0x04 or len(seed) != 32:
        raise ValueError("no Ed25519 seed")
    return seed


_CDP_KEY_OID_ED25519 = b"\x06\x03\x2b\x65\x70"  # OID 1.3.101.112 TLV


def _cdp_parse_secret(secret):
    """CDP API key secret -> ("EdDSA", seed32) | ("ES256", scalar).

    Raises ValueError on anything unrecognised — callers treat that
    as 'no usable credential', never as a crash.
    """
    text = secret.strip().replace("\\n", "\n")
    if "-----BEGIN" in text:
        body = "".join(line.strip() for line in text.splitlines()
                       if line.strip() and "-----" not in line)
        der = base64.b64decode(body)
        if _CDP_KEY_OID_ED25519 in der:
            return "EdDSA", _ed_seed_from_der(der)
        return "ES256", _ec_scalar_from_der(der)
    raw = base64.b64decode("".join(text.split()), validate=True)
    if len(raw) == 64:                   # Ed25519 seed + public key
        return "EdDSA", raw[:32]
    if len(raw) == 32:                   # bare Ed25519 seed
        return "EdDSA", raw
    if raw[:1] == b"\x30":               # DER without PEM armour
        if _CDP_KEY_OID_ED25519 in raw:
            return "EdDSA", _ed_seed_from_der(raw)
        return "ES256", _ec_scalar_from_der(raw)
    raise ValueError("unrecognised CDP secret format")


_cdp_key_cache = {}


def _cdp_jwt(cfg, method, path):
    """Fresh CDP JWT for one facilitator request, or None.

    None (with a loud log line) when the credentials are missing or
    unparseable: the facilitator call then goes out unauthenticated
    and CDP's own auth error is what surfaces — the endpoint itself
    never crashes on a bad credential.
    """
    key_id = cfg.get("cdp_key_id") or ""
    secret = cfg.get("cdp_key_secret") or ""
    if not key_id or not secret:
        return None
    cache_key = hashlib.sha256(secret.encode()).hexdigest()
    if cache_key not in _cdp_key_cache:
        try:
            _cdp_key_cache[cache_key] = _cdp_parse_secret(secret)
        except Exception as exc:
            cu_wiki_mcp.log(f"x402: CDP_API_KEY_SECRET unparseable "
                            f"({exc!r}) — facilitator calls will be "
                            "unauthenticated")
            _cdp_key_cache[cache_key] = None
    parsed = _cdp_key_cache[cache_key]
    if parsed is None:
        return None
    alg, key_material = parsed
    host = urlparse(cfg["facilitator"]).hostname or _CDP_HOST
    uri = f"{method} {host}{path}"
    now = int(time.time())
    header = {"alg": alg, "typ": "JWT", "kid": key_id,
              "nonce": secrets.token_hex(16)}
    claims = {"sub": key_id, "iss": "cdp", "aud": ["cdp_service"],
              "nbf": now, "exp": now + 120,
              "uri": uri, "uris": [uri]}
    signing_input = (_b64u(json.dumps(header,
                                      separators=(",", ":")).encode())
                     + "." + _b64u(json.dumps(
                         claims, separators=(",", ":")).encode()))
    try:
        if alg == "EdDSA":
            signature = _ed_sign(key_material,
                                 signing_input.encode("ascii"))
        else:
            signature = _es256_sign(key_material,
                                    signing_input.encode("ascii"))
    except Exception as exc:
        cu_wiki_mcp.log(f"x402: CDP JWT signing failed: {exc!r}")
        return None
    return signing_input + "." + _b64u(signature)


def _facilitator_headers(cfg, method, endpoint):
    """Headers for one facilitator call, CDP JWT attached when the
    facilitator is Coinbase CDP and credentials are configured."""
    headers = {"Content-Type": "application/json",
               "User-Agent": "cu-wiki-mcp/1.0"}
    if cfg.get("is_cdp"):
        base_path = urlparse(cfg["facilitator"]).path.rstrip("/")
        token = _cdp_jwt(cfg, method, base_path + "/" + endpoint)
        if token:
            headers["Authorization"] = "Bearer " + token
    return headers


def _facilitator_call(cfg, endpoint, payment, requirements):
    """POST /verify or /settle; parsed JSON dict, or None."""
    body = json.dumps({
        "x402Version": X402_VERSION,
        "paymentPayload": payment,
        "paymentRequirements": requirements,
    }).encode("utf-8")
    # NOTE: an explicit User-Agent is required — the x402.org
    # facilitator's CDN rejects the default Python-urllib agent
    # (HTTP 403, Cloudflare error 1010), which would otherwise make
    # every verification look "unreachable". For the CDP facilitator
    # _facilitator_headers additionally attaches a per-call JWT
    # (a CDP JWT lives only 120 s; the single retry below reuses it
    # well inside that window).
    req = urllib.request.Request(
        cfg["facilitator"] + "/" + endpoint, data=body,
        headers=_facilitator_headers(cfg, "POST", endpoint),
        method="POST")
    for attempt in (1, 2):  # one retry on transient network failure
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            try:
                return json.loads(exc.read().decode("utf-8"))
            except Exception:
                # Non-JSON / empty error body. The CDP facilitator
                # answers bad credentials with a bare HTTP 401 and
                # an empty body — surface that as an auth-shaped
                # failure (402 "payment invalid: facilitator_http_
                # 401") instead of mislabelling it "unreachable".
                return {"isValid": False, "success": False,
                        "invalidReason":
                            f"facilitator_http_{exc.code}",
                        "errorReason":
                            f"facilitator_http_{exc.code}"}
        except Exception as exc:
            cu_wiki_mcp.log(f"x402: facilitator /{endpoint} attempt "
                            f"{attempt} failed: {exc!r}")
    return None


def _decode_payment_header(header):
    """X-PAYMENT header -> payment payload dict, or None."""
    try:
        decoded = base64.b64decode(header, validate=True)
        payment = json.loads(decoded.decode("utf-8"))
    except Exception:
        return None
    return payment if isinstance(payment, dict) else None


def _charges(cfg, messages):
    """(total_atomic, [paid tool names]) for a message list."""
    total, names = 0, []
    for msg in messages:
        if isinstance(msg, dict) and msg.get("method") == "tools/call":
            name = (msg.get("params") or {}).get("name")
            if name in cfg["prices"]:
                total += cfg["prices"][name]
                if name not in names:
                    names.append(name)
    return total, names


# -------------------------------------------------------------- transport

def _execute(messages, is_batch):
    """Run parsed messages through the shared JSON-RPC handler."""
    if is_batch:
        responses = [r for r in (cu_wiki_mcp.handle(m) for m in messages)
                     if r is not None]
        if not responses:
            return 202, None
        return 200, responses
    response = cu_wiki_mcp.handle(messages[0])
    if response is None:
        return 202, None  # notification: accepted, no body
    return 200, response


def process(raw, payment_header=None, resource=""):
    """One raw request body -> (http_status, response_dict_or_None,
    extra_headers)."""
    try:
        msg = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as exc:
        return 400, {"jsonrpc": "2.0", "id": None,
                     "error": {"code": -32700,
                               "message": f"parse error: {exc}"}}, {}
    if isinstance(msg, list):  # JSON-RPC batch
        messages, is_batch = msg, True
    elif isinstance(msg, dict):
        messages, is_batch = [msg], False
    else:
        return 400, {"jsonrpc": "2.0", "id": None,
                     "error": {"code": -32600,
                               "message": "expected a JSON-RPC object"}}, {}

    cfg = _x402_config()
    if cfg is not None:
        total, names = _charges(cfg, messages)
        if total > 0:
            requirements = _payment_requirements(cfg, total, resource,
                                                  names)
            if not payment_header:
                return 402, _402_body(
                    requirements, "X-PAYMENT header is required"), {}
            payment = _decode_payment_header(payment_header)
            if payment is None \
                    or payment.get("scheme") != "exact" \
                    or payment.get("network") != cfg["network"]:
                return 402, _402_body(
                    requirements, "invalid X-PAYMENT header"), {}
            verify = _facilitator_call(cfg, "verify", payment,
                                       requirements)
            if verify is None:
                return 402, _402_body(
                    requirements,
                    "payment verification unavailable "
                    "(facilitator unreachable)"), {}
            if not verify.get("isValid"):
                reason = verify.get("invalidReason") or "unknown"
                return 402, _402_body(
                    requirements, f"payment invalid: {reason}"), {}
            status, payload = _execute(messages, is_batch)
            settle = _facilitator_call(cfg, "settle", payment,
                                       requirements)
            if not settle or not settle.get("success"):
                detail = (settle or {}).get("errorReason") \
                    or "facilitator did not settle"
                return 402, _402_body(
                    requirements,
                    f"payment settlement failed: {detail}"), {}
            receipt = base64.b64encode(
                json.dumps(settle).encode("utf-8")).decode("ascii")
            return status, payload, {"X-PAYMENT-RESPONSE": receipt}

    status, payload = _execute(messages, is_batch)
    return status, payload, {}


class handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "cu-wiki-mcp/1.0"

    def log_message(self, *args):  # keep function logs clean
        pass

    def _send(self, status, payload, extra_headers=None):
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
                         "MCP-Protocol-Version, X-PAYMENT")
        self.send_header("Access-Control-Expose-Headers",
                         "X-PAYMENT-RESPONSE")
        for key, value in (extra_headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _resource_url(self):
        override = os.environ.get("X402_RESOURCE_URL")
        if override:
            return override
        host = self.headers.get("Host") or "localhost"
        proto = self.headers.get("X-Forwarded-Proto") or (
            "http" if host.startswith(("127.0.0.1", "localhost"))
            else "https")
        return f"{proto}://{host}/mcp"

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
        info = dict(INFO)
        info["x402"] = _x402_public()
        self._send(200, info)

    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        raw = self.rfile.read(length) if length > 0 else b""
        try:
            status, payload, extra = process(
                raw,
                payment_header=self.headers.get("X-PAYMENT"),
                resource=self._resource_url())
        except Exception as exc:  # never hang the transport
            cu_wiki_mcp.log(f"http handler failed: {exc!r}")
            status, payload, extra = 500, {
                "jsonrpc": "2.0", "id": None,
                "error": {"code": -32603, "message": f"internal: {exc}"}}, {}
        self._send(status, payload, extra)

    def do_OPTIONS(self):
        self._send(204, None)
