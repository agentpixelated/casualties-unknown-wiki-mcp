# x402 monetisation — release package (NOT deployed)

Status: implemented + verified on staging/testnet only. **Production
is untouched** and still serves everything for free; the paywall
defaults to OFF even after this code is deployed.

## What changed

Only the HTTP transport layer (`api/mcp.py`) plus docs:

- New x402 middleware in `api/mcp.py` (stdlib only, no new
  dependencies; `server/cu_wiki_mcp.py` and the stdio server are
  byte-identical to production).
- Split (official x402 MCP pattern — `tools/list` free, `tools/call`
  paid):
  - Free forever: `initialize`, `ping`, `tools/list`, `cu_search`.
  - Paid: `cu_get`, `cu_item`, `cu_status` — default **$0.005/call**
    each. A JSON-RPC batch is charged once for the sum of its paid
    calls.
- Unpaid paid call → HTTP 402, x402 v1 body
  `{x402Version, error, accepts:[{scheme:"exact", network,
  maxAmountRequired, resource, description, mimeType, payTo,
  maxTimeoutSeconds, asset, extra}]}`.
- Paid call → facilitator `/verify`; content executes only after
  `/settle` succeeds; receipt is returned base64-encoded in the
  `X-PAYMENT-RESPONSE` response header. Any verify/settle failure →
  402 with the reason, content never leaks.
- GET descriptor gains an `x402` object (enabled, network, payTo,
  per-tool prices, free tools) for client discovery.
- Safety: `X402_ENABLED` set but unusable (missing `X402_PAY_TO`,
  unknown network, bad price) → paywall disarms itself with a loud
  log line; the public endpoint keeps serving free. The endpoint
  cannot be broken by a monetisation misconfiguration.

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `X402_ENABLED` | off | `1`/`true`/`yes`/`on` arms the paywall |
| `X402_PAY_TO` | — (required when enabled) | Wallet receiving USDC |
| `X402_NETWORK` | `base-sepolia` | `base-sepolia` \| `base` \| `solana-devnet` \| `solana` |
| `X402_FACILITATOR_URL` | `https://x402.org/facilitator` | Verify/settle service |
| `X402_PAID_TOOLS` | `cu_get,cu_item,cu_status` | Comma-separated paid tools |
| `X402_PRICE_USD` | `0.005` | Default price per paid call |
| `X402_PRICE_<TOOL>` | — | Per-tool override, e.g. `X402_PRICE_CU_ITEM=0.01` |
| `X402_RESOURCE_URL` | derived from Host | Canonical URL printed in 402 requirements |
| `CDP_API_KEY_ID` | — | Coinbase CDP Secret API key id (see CDP section) |
| `CDP_API_KEY_SECRET` | — | Coinbase CDP Secret API key secret (Ed25519 base64 or EC PEM) |

The two CDP variables are read **only** when `X402_FACILITATOR_URL`
points at `api.cdp.coinbase.com`; the x402.org testnet path ignores
them completely.

USDC contracts baked in: Base Sepolia
`0x036CbD53842c5426634e7929541eC2318f3dCF7e`, Base mainnet
`0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`, Solana devnet mint
`4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU`, Solana mainnet mint
`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` (Solana requirements
take `feePayer` from the facilitator's `/supported`).

## Verification evidence (staging, 2026-11-11)

Full suite: **20/20 checks passed** (harness + raw evidence in the
release package: `tests/run_tests.py`, `evidence/`).

- Paywall OFF: tools/list, descriptor (`x402.enabled=false`),
  `cu_search`, `cu_status` — identical behaviour to production.
- Paywall ON + mock facilitator: free split holds; unpaid → 402 with
  exact requirements ($0.005 = `5000` atomic, correct contract,
  `extra.name="USDC"` on Sepolia); paid → 200 + content +
  `X-PAYMENT-RESPONSE` success receipt; batch of 2 paid calls →
  single 402 for `10000`; garbage header → 402.
- Paywall ON + **real x402.org facilitator (Base Sepolia)**: unpaid →
  402; a genuinely signed EIP-3009 payload from the throwaway TEST
  payer passed `/verify` (`{"isValid": true}`); the paid call then
  **settled on-chain** — tx
  `0x933b726b035a3f132cb4c0767f12875ae47caafaa0a47306bca1a0606b99af18`
  (Base Sepolia, $0.005 USDC, TEST payer → TEST payTo) — and only
  then returned content with the settlement receipt; a tampered
  signature was rejected (`invalid_exact_evm_signature`).
- Misconfiguration (`X402_ENABLED=1`, no pay-to): endpoint unbroken,
  everything served free.

Test wallets were throwaway keys generated from fixed junk bytes,
used on testnet only. **No mainnet address exists in this package.**

Gotchas found by the tests (already fixed in the code):

1. The x402.org facilitator's CDN rejects Python-urllib's default
   User-Agent (HTTP 403, Cloudflare 1010) — facilitator calls send
   `User-Agent: cu-wiki-mcp/1.0`, on Vercel too.
2. The Base **Sepolia** USDC token's EIP-712 domain name is `"USDC"`,
   while **mainnet** USDC is `"USD Coin"` — the facilitator rejects
   requirements whose `extra.name` differs from the contract's
   `name()`. Both are baked in per network.
3. Payment signatures must be `0x`-prefixed hex (client-side note).

## Coinbase CDP facilitator (mainnet) — added 2026-11-11

Owner decision: **go-live on Base mainnet with the CDP
facilitator.** Implemented in staging, verified as far as possible
without real credentials (see evidence below). Production still
untouched.

### Credential format (per official CDP docs)

Source: CDP "API Authentication",
https://docs.cdp.coinbase.com/api-reference/v2/authentication
(updated 2026-11-10), and the CDP x402 "Monetize Service" skill doc,
https://docs.cdp.coinbase.com/agentic-wallet/cli/skills/monetize-service
("Using the CDP facilitator … requires `CDP_API_KEY_ID` and
`CDP_API_KEY_SECRET` environment variables from the CDP Portal").

- Create at https://portal.cdp.coinbase.com → **API Keys → Secret
  API Keys → Create API key**. The secret is shown **once**.
- `CDP_API_KEY_ID` — the key's id/name (UUID-style identifier).
- `CDP_API_KEY_SECRET` — the private key, in one of the two formats
  the portal issues:
  - **Ed25519** (portal default/recommended for CDP APIs):
    base64 encoding of a 64-byte key (32-byte seed + 32-byte
    public key) → JWT alg **EdDSA**.
  - **ECDSA**: PEM EC P-256 private key
    (`-----BEGIN EC PRIVATE KEY-----`, or PKCS#8
    `-----BEGIN PRIVATE KEY-----`; newlines must be preserved —
    a literal `\n`-escaped one-line value is also accepted by our
    parser) → JWT alg **ES256**.
- Authentication is a **fresh JWT per facilitator call**, sent as
  `Authorization: Bearer <jwt>`, minted `generateJwt`-style:
  header `{alg, typ:"JWT", kid:<key id>, nonce:<random hex>}`;
  claims `{sub:<key id>, iss:"cdp", aud:["cdp_service"], nbf,
  exp: now+120s, uri:"<METHOD> api.cdp.coinbase.com<path>",
  uris:[same]}` — bound to the exact method + host + path of the
  call it authorises.
- Facilitator base URL: `https://api.cdp.coinbase.com/platform/v2/x402`
  with `POST /verify`, `POST /settle`, `GET /supported`.
- **Facilitator fee:** the official CDP docs pages above do **not**
  state a fee for the x402 facilitator — treat any number as
  UNVERIFIED. (Secondary sources conflict: one implementation guide
  reports Coinbase subsidising settlement gas / free as of Apr
  2026; another reports 1,000 free settlements/month then $0.001
  each. Confirm in the CDP Portal/billing when the key is created.)

### Implementation (staging `api/mcp.py`)

- CDP auth activates only when the facilitator host is
  `api.cdp.coinbase.com` **and** both CDP env vars are set. A CDP
  facilitator URL without credentials logs a loud warning and calls
  go out unauthenticated (CDP rejects them; see error shape).
- Both signers are **pure stdlib** (no new dependencies, matching
  the endpoint's design): RFC 8032 Ed25519 and RFC 6979
  deterministic ECDSA P-256, plus a minimal DER parser for
  SEC1/PKCS#8 PEM and base64 secrets.
- Error shape, probed live against the real CDP facilitator: bad
  or missing credentials → **bare HTTP 401 with an empty body**
  on `/verify`, `/settle` and `/supported`. The client maps a
  non-JSON facilitator error to `facilitator_http_<status>`, so a
  paid call with bad CDP credentials surfaces to the MCP client as
  HTTP 402 `payment invalid: facilitator_http_401` — an auth
  failure, never a crash and never mislabelled "unreachable", and
  content is never served.

### CDP verification evidence (2026-11-11, `evidence-cdp/`)

- Signer cross-checks vs independent implementations (PyNaCl +
  `cryptography`): **12/12 PASS** — Ed25519 sign/verify both
  directions, ES256 sign verified for SEC1 and PKCS#8 keys, all
  four secret formats parsed, JWT header/claims shape, JWT
  signature verifies, garbage secret → no JWT, no crash.
- Dummy-credential end-to-end (well-formed junk Ed25519 key, and
  an unparseable garbage secret): **5/5 PASS** — unpaid → 402;
  paid → 402 `payment invalid: facilitator_http_401`; free
  `cu_search` → 200 throughout.
- Live probe of the real CDP facilitator: no-auth and junk-JWT
  requests both → HTTP 401 (empty body), as documented above.
- Regression: the pre-existing harness (x402.org testnet path,
  mock facilitator, disarm safety) still passes **20/20**,
  including a fresh real Base Sepolia settlement.

## Production rollout (when go-live is approved)

### Final production env (Vercel) — go-live configuration

| Variable | Production value |
|---|---|
| `X402_ENABLED` | `true` (set **last**, see steps) |
| `X402_NETWORK` | `base` |
| `X402_PRICE_USD` | `0.005` |
| `X402_FACILITATOR_URL` | `https://api.cdp.coinbase.com/platform/v2/x402` |
| `X402_PAY_TO` | ⏳ **waiting on owner** — EVM (0x…) address, see below |
| `CDP_API_KEY_ID` | ⏳ **waiting on vault** — CDP Secret API key id |
| `CDP_API_KEY_SECRET` | ⏳ **waiting on vault** — CDP Secret API key secret |

Optional/unchanged: `X402_PAID_TOOLS` (default
`cu_get,cu_item,cu_status`), `X402_PRICE_<TOOL>` overrides,
`X402_RESOURCE_URL` (derived from Host).

**About the owner's wallets (Jupiter + Phantom):** `X402_PAY_TO`
on Base must be an **EVM address (0x…)**. Phantom is multi-chain
and shows a separate Ethereum/Base address — that is the address
to use. Jupiter is Solana-only and cannot receive Base USDC; it
would only become relevant for a future Solana-network round
(x402 v2, a separate verification round — not this go-live).

### Steps

1. Push the updated tree to GitHub (`invoke_push.py` in the publish
   staging dir pushes `api/mcp.py` + `README.md`; add this file to
   its `FILES` list if it should be published too).
2. Redeploy the Vercel project from the updated tree (the project is
   NOT git-attached — pushes alone do not redeploy).
3. Set the env vars above **except** `X402_ENABLED`, redeploy, and
   verify while still disarmed: GET descriptor must show
   `x402.enabled=false`, all tools still free (200).
4. Set `X402_ENABLED=true`, redeploy, then post-deploy checks:
   - `GET /` descriptor → `x402.enabled=true`, `network="base"`,
     `payTo` = owner's Phantom EVM address.
   - Unpaid `tools/call cu_status` → **HTTP 402** with Base
     requirements (`asset` = mainnet USDC
     `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`,
     `maxAmountRequired="5000"`, `extra.name="USD Coin"`).
   - `tools/list` → 200; unpaid `cu_search` → **200** (still free).
   - First real paid call from an x402 client → 200 + content +
     `X-PAYMENT-RESPONSE` receipt, settlement visible on Base.
     (If CDP credentials were wrong, paid calls would instead
     return 402 `payment invalid: facilitator_http_401` — that is
     the tell.)
5. Rollback = unset `X402_ENABLED` (or set it to `0`) + redeploy.
   No code rollback needed.

## Open decisions for go-live (owner)

1. ⏳ **Mainnet wallet address** for `X402_PAY_TO` — the owner uses
   Jupiter + Phantom; for Base this must be **Phantom's Ethereum/
   Base (0x…) address**. Awaiting that exact address. (Solana via
   Jupiter remains a possible later round: requirements support
   exists, but Solana on public facilitators is x402 v2 — a second
   verification round, not a switch flip.)
2. ✅ **Mainnet facilitator — DECIDED: Coinbase CDP** (implemented
   + dummy-verified, section above). Remaining input: the real
   CDP Secret API key (id + secret) delivered via the Secure
   Vault, created at https://portal.cdp.coinbase.com.
3. **Final price** — implemented default is $0.005/call for each of
   `cu_get` / `cu_item` / `cu_status`, `cu_search` free. Any change
   is a pure env edit (`X402_PRICE_USD` / per-tool overrides).

## Follow-up debt (not part of this go-live)

The Rust rewrite of the CU MCP is finished but **not deployed**
(owner's host choice pending); production remains this Python
tree, which is what this release targets. When the Rust version
is deployed, the x402 layer (env contract + verify/settle flow +
CDP JWT) must be ported to it — this document is the spec.
