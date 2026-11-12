//! x402 paywall for the HTTP transport — Rust port of the paywall in
//! `api/mcp.py` (staging tree), per `X402_RELEASE.md`.
//!
//! Model (identical to the Python release):
//! - Free forever: `initialize`, `ping`, `tools/list`, `cu_search`.
//! - Paid (default $0.005/call): `cu_get`, `cu_item`, `cu_status`.
//!   A JSON-RPC batch is charged once for the sum of its paid calls.
//! - Unpaid paid call -> HTTP 402 with the x402 v1 payment-required
//!   body `{x402Version, error, accepts:[{scheme:"exact", network,
//!   maxAmountRequired, resource, description, mimeType, payTo,
//!   maxTimeoutSeconds, asset, extra}]}`.
//! - Paid call -> facilitator `/verify`; content executes only after
//!   `/settle` succeeds; the settlement receipt is returned
//!   base64-encoded in the `X-PAYMENT-RESPONSE` response header. Any
//!   verify/settle failure -> 402 with the reason, content never
//!   leaks.
//! - Config is env-only: `X402_ENABLED`, `X402_PAY_TO`,
//!   `X402_NETWORK`, `X402_FACILITATOR_URL`, `X402_PAID_TOOLS`,
//!   `X402_PRICE_USD`, `X402_PRICE_<TOOL>`, `X402_RESOURCE_URL`,
//!   `CDP_API_KEY_ID`, `CDP_API_KEY_SECRET`.
//!
//! ONE DELIBERATE DEVIATION from the Python release: when
//! `X402_ENABLED` is set but the configuration is unusable (no
//! pay-to address, unknown network, bad price), the Python endpoint
//! DISARMS the paywall and serves paid tools free. This port FAILS
//! CLOSED instead: paid tools answer 402 with a configuration error
//! and are never served free, while free tools keep working. The
//! misconfiguration is logged loudly to stderr.
//!
//! The stdio transport is untouched by this module — the paywall is
//! HTTP-only, exactly like the Python release.

use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::log;

pub const X402_VERSION: u64 = 1;
const DEFAULT_FACILITATOR: &str = "https://x402.org/facilitator";
const CDP_HOST: &str = "api.cdp.coinbase.com";
const USDC_DECIMALS: u32 = 6;
pub const DEFAULT_PAID_TOOLS: [&str; 3] = ["cu_get", "cu_item", "cu_status"];
const ALL_TOOLS: [&str; 4] = ["cu_get", "cu_item", "cu_search", "cu_status"];

// network -> (USDC asset/contract/mint, EIP-712 domain name, version)
// Names read from the contracts themselves: Base Sepolia USDC is
// "USDC", mainnet USDC is "USD Coin" — the facilitator rejects
// requirements whose extra.name differs from the contract's name().
fn network_asset(network: &str) -> Option<(&'static str, Option<&'static str>, Option<&'static str>)> {
    match network {
        "base-sepolia" => Some((
            "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
            Some("USDC"),
            Some("2"),
        )),
        "base" => Some((
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            Some("USD Coin"),
            Some("2"),
        )),
        "solana-devnet" => Some((
            "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
            None,
            None,
        )),
        "solana" => Some((
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            None,
            None,
        )),
        _ => None,
    }
}

fn is_evm(network: &str) -> bool {
    network == "base-sepolia" || network == "base"
}

fn env_truthy(name: &str) -> bool {
    matches!(
        env::var(name).unwrap_or_default().trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_trimmed(name: &str) -> String {
    env::var(name).unwrap_or_default().trim().to_string()
}

/// Parsed x402 configuration.
#[derive(Clone)]
pub struct X402Config {
    pub pay_to: String,
    pub network: String,
    pub facilitator: String,
    /// tool name -> price in USDC atomic units (6 decimals).
    pub prices: Vec<(String, u64)>,
    pub cdp_key_id: String,
    pub cdp_key_secret: String,
    pub is_cdp: bool,
}

impl X402Config {
    pub fn price_of(&self, tool: &str) -> Option<u64> {
        self.prices.iter().find(|(t, _)| t == tool).map(|(_, p)| *p)
    }
}

/// Paywall state, re-read from the environment on every call (the
/// Python reference re-parses per request too).
pub enum X402State {
    /// Paywall off, or enabled-but-unusable is NOT this variant.
    Off,
    Armed(X402Config),
    /// Enabled but unusable — fail closed (see module docs).
    Misconfigured(String),
}

pub fn x402_state() -> X402State {
    if !env_truthy("X402_ENABLED") {
        return X402State::Off;
    }
    let pay_to = env_trimmed("X402_PAY_TO");
    if pay_to.is_empty() {
        let msg = "X402_ENABLED set but X402_PAY_TO is empty".to_string();
        log(&format!("x402: {msg} — paywall FAILING CLOSED (paid tools unavailable)"));
        return X402State::Misconfigured(msg);
    }
    let network = match env::var("X402_NETWORK") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => "base-sepolia".to_string(),
    };
    if network_asset(&network).is_none() {
        let msg = format!("unknown X402_NETWORK {network:?}");
        log(&format!("x402: {msg} — paywall FAILING CLOSED (paid tools unavailable)"));
        return X402State::Misconfigured(msg);
    }
    let paid_raw = env::var("X402_PAID_TOOLS")
        .unwrap_or_else(|_| "cu_get,cu_item,cu_status".to_string());
    let paid: Vec<String> = paid_raw
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let mut prices: Vec<(String, u64)> = Vec::new();
    for tool in &paid {
        let raw = env::var(format!("X402_PRICE_{}", tool.to_uppercase()))
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| env::var("X402_PRICE_USD").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "0.005".to_string());
        match usd_to_atomic(&raw) {
            Ok(p) => prices.push((tool.clone(), p)),
            Err(e) => {
                let msg = format!("bad price {raw:?} for {tool}: {e}");
                log(&format!("x402: {msg} — paywall FAILING CLOSED (paid tools unavailable)"));
                return X402State::Misconfigured(msg);
            }
        }
    }
    let facilitator = env::var("X402_FACILITATOR_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_FACILITATOR.to_string());
    let cdp_key_id = env_trimmed("CDP_API_KEY_ID");
    let cdp_key_secret = env_trimmed("CDP_API_KEY_SECRET");
    let is_cdp = host_of(&facilitator) == Some(CDP_HOST.to_string());
    if is_cdp && (cdp_key_id.is_empty() || cdp_key_secret.is_empty()) {
        log("x402: facilitator is Coinbase CDP but CDP_API_KEY_ID / \
             CDP_API_KEY_SECRET are not both set — facilitator calls \
             will go out UNAUTHENTICATED and CDP will reject them");
    }
    X402State::Armed(X402Config {
        pay_to,
        network,
        facilitator,
        prices,
        cdp_key_id,
        cdp_key_secret,
        is_cdp,
    })
}

/// "$0.005" / "0.005" -> 5000 (USDC has 6 decimals). Mirrors the
/// Python Decimal conversion, including round-half-even on a 7th+
/// fractional digit and the positive-price check.
pub fn usd_to_atomic(text: &str) -> Result<u64, String> {
    let t = text.trim().strip_prefix('$').unwrap_or(text.trim()).trim();
    if t.is_empty() {
        return Err("empty price".to_string());
    }
    let (int_part, frac_part) = match t.split_once('.') {
        Some((i, f)) => (i, f),
        None => (t, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return Err(format!("not a decimal price: {text:?}"));
    }
    let int_val: u128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| "price too large".to_string())?
    };
    let mut atomic: u128 = int_val * 10u128.pow(USDC_DECIMALS);
    if frac_part.len() <= 6 {
        let mut frac = frac_part.to_string();
        while frac.len() < 6 {
            frac.push('0');
        }
        let frac_val: u128 = if frac.is_empty() {
            0
        } else {
            frac.parse().map_err(|_| "price too large".to_string())?
        };
        atomic += frac_val;
    } else {
        // >6 fractional digits: round-half-even on the 7th digit on.
        let kept = &frac_part[..6];
        let rest = &frac_part[6..];
        let kept_val: u128 = kept.parse().map_err(|_| "price too large".to_string())?;
        atomic += kept_val;
        let cmp = compare_to_half(rest);
        match cmp {
            std::cmp::Ordering::Greater => atomic += 1,
            std::cmp::Ordering::Equal => {
                if atomic % 2 == 1 {
                    atomic += 1;
                }
            }
            std::cmp::Ordering::Less => {}
        }
    }
    if atomic == 0 {
        return Err("price must be positive".to_string());
    }
    u64::try_from(atomic).map_err(|_| "price too large".to_string())
}

/// Compare a digit string (the part after the 6th fractional digit)
/// against exactly one half (0.5): the string "5" followed by zeros.
fn compare_to_half(rest: &str) -> std::cmp::Ordering {
    let mut half = String::from("5");
    while half.len() < rest.len() {
        half.push('0');
    }
    let rest_n = rest.trim_end_matches('0');
    let half_n = half.trim_end_matches('0');
    if rest_n.len() != half_n.len() {
        return rest_n.len().cmp(&half_n.len());
    }
    rest_n.cmp(half_n)
}

/// 5000 -> "$0.005". Mirrors Python's _atomic_to_usd.
pub fn atomic_to_usd(atomic: u64) -> String {
    let text = format!("{}.{:06}", atomic / 1_000_000, atomic % 1_000_000);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("${text}")
}

/// Paywall summary for the GET descriptor (payTo is public by
/// design — it is printed in every 402 response).
pub fn public_descriptor() -> Value {
    match x402_state() {
        X402State::Off => json!({"enabled": false}),
        X402State::Misconfigured(msg) => {
            json!({"enabled": false, "error": format!("x402 misconfiguration: {msg}")})
        }
        X402State::Armed(cfg) => {
            let (asset, _, _) = network_asset(&cfg.network).unwrap();
            let mut paid_tools = Map::new();
            let mut prices_sorted = cfg.prices.clone();
            prices_sorted.sort_by(|a, b| a.0.cmp(&b.0));
            for (tool, price) in &prices_sorted {
                paid_tools.insert(tool.clone(), json!(atomic_to_usd(*price)));
            }
            let free: Vec<&str> = ALL_TOOLS
                .iter()
                .filter(|t| cfg.price_of(t).is_none())
                .copied()
                .collect();
            json!({
                "enabled": true,
                "x402Version": X402_VERSION,
                "network": cfg.network,
                "asset": asset,
                "payTo": cfg.pay_to,
                "paidTools": paid_tools,
                "freeTools": free,
                "freeMethods": ["initialize", "ping", "tools/list"],
            })
        }
    }
}

/// feePayer for Solana requirements, from the facilitator's
/// /supported document (cached per facilitator+network).
fn solana_fee_payer(cfg: &X402Config) -> Option<String> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), Option<String>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (cfg.facilitator.clone(), cfg.network.clone());
    if let Ok(guard) = cache.lock() {
        if let Some(v) = guard.get(&key) {
            return v.clone();
        }
    }
    let mut fee_payer = None;
    match facilitator_get(cfg, "supported") {
        Ok(Some(doc)) => {
            if let Some(kinds) = doc.get("kinds").and_then(|k| k.as_array()) {
                for kind in kinds {
                    if kind.get("x402Version").and_then(|v| v.as_u64())
                        == Some(X402_VERSION)
                        && kind.get("network").and_then(|v| v.as_str())
                            == Some(cfg.network.as_str())
                    {
                        fee_payer = kind
                            .get("extra")
                            .and_then(|e| e.get("feePayer"))
                            .and_then(|f| f.as_str())
                            .map(|s| s.to_string());
                        break;
                    }
                }
            }
        }
        Ok(None) => log("x402: /supported fetch failed: no response"),
        Err(e) => log(&format!("x402: /supported fetch failed: {e}")),
    }
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, fee_payer.clone());
    }
    fee_payer
}

pub fn payment_requirements(
    cfg: &X402Config,
    total_atomic: u64,
    resource: &str,
    tool_names: &[String],
) -> Value {
    let (asset, dom_name, dom_version) = network_asset(&cfg.network).unwrap();
    let mut req = Map::new();
    req.insert("scheme".into(), json!("exact"));
    req.insert("network".into(), json!(cfg.network));
    req.insert("maxAmountRequired".into(), json!(total_atomic.to_string()));
    req.insert("resource".into(), json!(if resource.is_empty() { "/mcp" } else { resource }));
    req.insert(
        "description".into(),
        json!(format!(
            "Casualties Unknown Wiki MCP paid tool call(s): {}",
            tool_names.join(", ")
        )),
    );
    req.insert("mimeType".into(), json!("application/json"));
    req.insert("payTo".into(), json!(cfg.pay_to));
    req.insert("maxTimeoutSeconds".into(), json!(60));
    req.insert("asset".into(), json!(asset));
    if is_evm(&cfg.network) {
        req.insert(
            "extra".into(),
            json!({"name": dom_name, "version": dom_version}),
        );
    } else if let Some(fee_payer) = solana_fee_payer(cfg) {
        req.insert("extra".into(), json!({"feePayer": fee_payer}));
    }
    Value::Object(req)
}

pub fn body_402(requirements: &Value, error: &str) -> Value {
    json!({"x402Version": X402_VERSION, "error": error, "accepts": [requirements]})
}

/// (total_atomic, [paid tool names]) for a message list. A batch is
/// charged once for the sum of its paid calls.
pub fn charges(cfg: &X402Config, messages: &[Value]) -> (u64, Vec<String>) {
    let mut total = 0u64;
    let mut names: Vec<String> = Vec::new();
    for msg in messages {
        if msg.get("method").and_then(|m| m.as_str()) == Some("tools/call") {
            if let Some(name) = msg
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            {
                if let Some(price) = cfg.price_of(name) {
                    total += price;
                    if !names.contains(&name.to_string()) {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    (total, names)
}

/// True when any message is a paid-tool call under the DEFAULT paid
/// set — used only for the fail-closed misconfigured state, where
/// no prices exist to charge against.
pub fn charges_default_paid(messages: &[Value]) -> bool {
    messages.iter().any(|msg| {
        msg.get("method").and_then(|m| m.as_str()) == Some("tools/call")
            && msg
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .map(|n| DEFAULT_PAID_TOOLS.contains(&n))
                .unwrap_or(false)
    })
}

/// X-PAYMENT header -> payment payload dict, or None.
pub fn decode_payment_header(header: &str) -> Option<Value> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header.trim())
        .ok()?;
    let payment: Value = serde_json::from_slice(&decoded).ok()?;
    if payment.is_object() {
        Some(payment)
    } else {
        None
    }
}

// ------------------------------------------------------- facilitator

fn facilitator_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| crate::build_http_client("cu-wiki-mcp/1.0", 30))
}

fn facilitator_headers(cfg: &X402Config, method: &str, endpoint: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), "cu-wiki-mcp/1.0".to_string());
    if cfg.is_cdp {
        let base_path = path_of(&cfg.facilitator);
        let base_path = base_path.trim_end_matches('/');
        if let Some(token) = cdp_jwt(cfg, method, &format!("{base_path}/{endpoint}")) {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
    }
    headers
}

/// GET <facilitator>/<endpoint>; parsed JSON dict, or Ok(None) on a
/// transport failure (caller logs), Err(message) on a bad body.
fn facilitator_get(cfg: &X402Config, endpoint: &str) -> Result<Option<Value>, String> {
    let url = format!("{}/{}", cfg.facilitator, endpoint);
    let mut req = facilitator_client().get(&url);
    for (k, v) in facilitator_headers(cfg, "GET", endpoint) {
        req = req.header(k, v);
    }
    let resp = req.send().map_err(|e| format!("{e}"))?;
    let text = resp.text().map_err(|e| format!("{e}"))?;
    serde_json::from_str(&text).map(Some).map_err(|e| format!("bad JSON: {e}"))
}

/// POST /verify or /settle; parsed JSON dict, or None when the
/// facilitator is unreachable (both attempts failed at transport).
/// Mirrors the Python helper, including the one retry on transient
/// network failure and the non-JSON-error -> facilitator_http_<code>
/// mapping (the CDP facilitator answers bad credentials with a bare
/// HTTP 401 and an empty body).
pub fn facilitator_call(
    cfg: &X402Config,
    endpoint: &str,
    payment: &Value,
    requirements: &Value,
) -> Option<Value> {
    let url = format!("{}/{}", cfg.facilitator, endpoint);
    let body = json!({
        "x402Version": X402_VERSION,
        "paymentPayload": payment,
        "paymentRequirements": requirements,
    });
    let body_text = serde_json::to_string(&body).unwrap_or_default();
    let headers = facilitator_headers(cfg, "POST", endpoint);
    for attempt in 1..=2 {
        let mut req = facilitator_client()
            .post(&url)
            .body(body_text.clone());
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        match req.send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().unwrap_or_default();
                if resp_status_is_error(status) {
                    if let Ok(doc) = serde_json::from_str::<Value>(&text) {
                        return Some(doc);
                    }
                    return Some(json!({
                        "isValid": false, "success": false,
                        "invalidReason": format!("facilitator_http_{status}"),
                        "errorReason": format!("facilitator_http_{status}"),
                    }));
                }
                match serde_json::from_str::<Value>(&text) {
                    Ok(doc) => return Some(doc),
                    Err(e) => {
                        log(&format!(
                            "x402: facilitator /{endpoint} attempt {attempt} \
                             returned unparsable JSON: {e}"
                        ));
                        if attempt == 2 {
                            return None;
                        }
                    }
                }
            }
            Err(e) => {
                log(&format!(
                    "x402: facilitator /{endpoint} attempt {attempt} failed: {e}"
                ));
                if attempt == 2 {
                    return None;
                }
            }
        }
    }
    None
}

fn resp_status_is_error(status: u16) -> bool {
    status >= 400
}

// ------------------------------------------------------------- CDP JWT
// Coinbase CDP facilitator auth, per the official CDP "API
// Authentication" docs: a Secret API Key (key id + key secret) mints
// a short-lived JWT per request, sent as "Authorization: Bearer".
//   header: {alg, typ: "JWT", kid: <key id>, nonce: <random hex>}
//   claims: {sub: <key id>, iss: "cdp", aud: ["cdp_service"],
//            nbf: now, exp: now + 120,
//            uri: "<METHOD> <host><path>",
//            uris: ["<METHOD> <host><path>"]}
// Secret formats issued by the portal: Ed25519 = base64 of a 64-byte
// key (32-byte seed + 32-byte public key) -> alg EdDSA; ECDSA = PEM
// EC P-256 private key -> ES256. Ported from the Python staging
// implementation (which is pure stdlib there; here the two signers
// come from ed25519-dalek / p256 instead of hand-rolled curves).

fn b64u(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

enum CdpKey {
    Ed25519([u8; 32]),
    Es256([u8; 32]),
}

/// One DER TLV at offset i: (tag, content, next_index).
fn der_item(data: &[u8], i: usize) -> Result<(u8, &[u8], usize), String> {
    if i + 2 > data.len() {
        return Err("DER truncated".to_string());
    }
    let tag = data[i];
    let mut j = i + 1;
    let mut length = data[j] as usize;
    j += 1;
    if length & 0x80 != 0 {
        let n = length & 0x7F;
        if n == 0 || n > 4 || j + n > data.len() {
            return Err("bad DER length".to_string());
        }
        length = 0;
        for k in 0..n {
            length = (length << 8) | data[j + k] as usize;
        }
        j += n;
    }
    if j + length > data.len() {
        return Err("DER content overruns buffer".to_string());
    }
    Ok((tag, &data[j..j + length], j + length))
}

/// Private-key octet string inside a PKCS#8 SEQUENCE.
fn pkcs8_inner_octet(der: &[u8]) -> Result<Vec<u8>, String> {
    let (tag, seq, _) = der_item(der, 0)?;
    if tag != 0x30 {
        return Err("not a SEQUENCE".to_string());
    }
    let (_, _, mut i) = der_item(seq, 0)?; // version INTEGER
    let (_, _, i2) = der_item(seq, i)?; // algorithm SEQUENCE
    i = i2;
    let (tag, octet, _) = der_item(seq, i)?;
    if tag != 0x04 {
        return Err("no private-key octet string".to_string());
    }
    Ok(octet.to_vec())
}

/// Private scalar from a SEC1 ECPrivateKey or a PKCS#8 blob.
fn ec_scalar_from_der(der: &[u8]) -> Result<[u8; 32], String> {
    let (tag, seq, _) = der_item(der, 0)?;
    if tag != 0x30 {
        return Err("not a SEQUENCE".to_string());
    }
    let (_, _, i) = der_item(seq, 0)?; // version INTEGER
    let (tag, content, _) = der_item(seq, i)?;
    if tag == 0x30 {
        // PKCS#8: unwrap, recurse.
        return ec_scalar_from_der(&pkcs8_inner_octet(der)?);
    }
    if tag != 0x04 {
        return Err("no EC private octet string".to_string());
    }
    if content.len() != 32 {
        return Err(format!("EC scalar is {} bytes, expected 32", content.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(content);
    Ok(out)
}

fn ed_seed_from_der(der: &[u8]) -> Result<[u8; 32], String> {
    let inner = pkcs8_inner_octet(der)?; // CurvePrivateKey bytes
    let (tag, seed, _) = der_item(&inner, 0)?;
    if tag != 0x04 || seed.len() != 32 {
        return Err("no Ed25519 seed".to_string());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(seed);
    Ok(out)
}

const ED25519_OID_TLV: &[u8] = b"\x06\x03\x2b\x65\x70"; // OID 1.3.101.112

/// CDP API key secret -> parsed key. Raises (Err) on anything
/// unrecognised — callers treat that as 'no usable credential',
/// never as a crash.
fn cdp_parse_secret(secret: &str) -> Result<CdpKey, String> {
    let text = secret.trim().replace("\\n", "\n");
    if text.contains("-----BEGIN") {
        let body: String = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.contains("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .map_err(|e| format!("bad PEM base64: {e}"))?;
        if contains_slice(&der, ED25519_OID_TLV) {
            return Ok(CdpKey::Ed25519(ed_seed_from_der(&der)?));
        }
        return Ok(CdpKey::Es256(ec_scalar_from_der(&der)?));
    }
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .map_err(|e| format!("bad secret base64: {e}"))?;
    if raw.len() == 64 {
        // Ed25519 seed + public key.
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&raw[..32]);
        return Ok(CdpKey::Ed25519(seed));
    }
    if raw.len() == 32 {
        // Bare Ed25519 seed.
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&raw);
        return Ok(CdpKey::Ed25519(seed));
    }
    if raw.first() == Some(&0x30) {
        // DER without PEM armour.
        if contains_slice(&raw, ED25519_OID_TLV) {
            return Ok(CdpKey::Ed25519(ed_seed_from_der(&raw)?));
        }
        return Ok(CdpKey::Es256(ec_scalar_from_der(&raw)?));
    }
    Err("unrecognised CDP secret format".to_string())
}

fn contains_slice(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

fn random_hex_bytes(n: usize) -> Vec<u8> {
    // /dev/urandom, read_exact — never fs::read (urandom has no EOF).
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = vec![0u8; n];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    let mut x = now as u64 ^ (std::process::id() as u64) << 32;
    for _ in 0..n {
        // xorshift64* — fallback only, never the primary path.
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.push((x.wrapping_mul(0x2545F4914F6CDD1D) >> 56) as u8);
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fresh CDP JWT for one facilitator request, or None.
///
/// None (with a loud log line) when the credentials are missing or
/// unparseable: the facilitator call then goes out unauthenticated
/// and CDP's own auth error is what surfaces — the endpoint itself
/// never crashes on a bad credential.
pub fn cdp_jwt(cfg: &X402Config, method: &str, path: &str) -> Option<String> {
    let key_id = cfg.cdp_key_id.trim();
    let secret = cfg.cdp_key_secret.trim();
    if key_id.is_empty() || secret.is_empty() {
        return None;
    }
    let parsed = match cdp_parse_secret(secret) {
        Ok(p) => p,
        Err(e) => {
            log(&format!(
                "x402: CDP_API_KEY_SECRET unparseable ({e}) — \
                 facilitator calls will be unauthenticated"
            ));
            return None;
        }
    };
    let host = host_of(&cfg.facilitator).unwrap_or_else(|| CDP_HOST.to_string());
    let uri = format!("{method} {host}{path}");
    let now = now_secs();
    let nonce = random_hex_bytes(16)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let header = json!({"alg": match parsed {
        CdpKey::Ed25519(_) => "EdDSA",
        CdpKey::Es256(_) => "ES256",
    }, "typ": "JWT", "kid": key_id, "nonce": nonce});
    let claims = json!({
        "sub": key_id, "iss": "cdp", "aud": ["cdp_service"],
        "nbf": now, "exp": now + 120,
        "uri": uri, "uris": [uri],
    });
    let signing_input = format!(
        "{}.{}",
        b64u(serde_json::to_string(&header).unwrap_or_default().as_bytes()),
        b64u(serde_json::to_string(&claims).unwrap_or_default().as_bytes()),
    );
    let signature: Vec<u8> = match parsed {
        CdpKey::Ed25519(seed) => {
            use ed25519_dalek::Signer;
            let key = ed25519_dalek::SigningKey::from_bytes(&seed);
            key.sign(signing_input.as_bytes()).to_bytes().to_vec()
        }
        CdpKey::Es256(scalar) => {
            use p256::ecdsa::signature::Signer as _;
            let key = match p256::ecdsa::SigningKey::from_bytes(&scalar.into()) {
                Ok(k) => k,
                Err(e) => {
                    log(&format!("x402: CDP JWT signing failed: {e}"));
                    return None;
                }
            };
            let sig: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
            sig.to_bytes().to_vec()
        }
    };
    Some(format!("{signing_input}.{}", b64u(&signature)))
}

// ------------------------------------------------------------ url bits

/// Host (without port) of an http(s) URL, or None.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let hostport = rest.split('/').next().unwrap_or("");
    let host = hostport.split('@').last().unwrap_or(hostport);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.split(':').next().unwrap_or(host),
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Path of an http(s) URL ("" when none).
pub fn path_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    match rest.find('/') {
        Some(i) => rest[i..].to_string(),
        None => String::new(),
    }
}

/// Canonical resource URL for the 402 requirements, mirroring the
/// Python handler: X402_RESOURCE_URL override, else Host header with
/// X-Forwarded-Proto (http for loopback hosts, https otherwise).
pub fn resource_url(host: Option<&str>, forwarded_proto: Option<&str>) -> String {
    if let Ok(v) = env::var("X402_RESOURCE_URL") {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    let host = host.unwrap_or("localhost");
    let proto = match forwarded_proto {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => {
            if host.starts_with("127.0.0.1") || host.starts_with("localhost") {
                "http".to_string()
            } else {
                "https".to_string()
            }
        }
    };
    format!("{proto}://{host}/mcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures generated with openssl 3.0 (see task report for the
    // commands); the EC key's public point is in ec_pub.pem there.
    const EC_SEC1_PEM: &str = "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEINkQCBeFwCWVULoCrL4jIL10p79uAW7KeKBGEFv031I/oAoGCCqGSM49\nAwEHoUQDQgAEhY67E5PXP0ezXP7F8siCdeCCmYNaAP8/e1FxZ4jqwcbJZWhJEoT5\nTBIQLUL/sFj3N0/hohc4Um6KSY6GGnraYg==\n-----END EC PRIVATE KEY-----";
    const EC_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg2RAIF4XAJZVQugKs\nviMgvXSnv24Bbsp4oEYQW/TfUj+hRANCAASFjrsTk9c/R7Nc/sXyyIJ14IKZg1oA\n/z97UXFniOrBxsllaEkShPlMEhAtQv+wWPc3T+GiFzhSbopJjoYaetpi\n-----END PRIVATE KEY-----";
    const ED_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIGFqD7z+C31qKex0EmpCgwrj/pHcuNk6Tyl2LyPD0jTa\n-----END PRIVATE KEY-----";
    // Ed25519 seed inside ED_PKCS8_PEM (bytes 61:6a:0f:bc:...).
    const ED_SEED_HEX: &str = "616a0fbcfe0b7d6a29ec74126a42830ae3fe91dcb8d93a4f29762f23c3d234da";

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn test_cfg(secret: &str) -> X402Config {
        X402Config {
            pay_to: "0x0000000000000000000000000000000000000000".to_string(),
            network: "base".to_string(),
            facilitator: "https://api.cdp.coinbase.com/platform/v2/x402".to_string(),
            prices: vec![("cu_status".to_string(), 5000)],
            cdp_key_id: "test-key-id".to_string(),
            cdp_key_secret: secret.to_string(),
            is_cdp: true,
        }
    }

    fn b64u_decode(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap()
    }

    #[test]
    fn usd_to_atomic_cases() {
        assert_eq!(usd_to_atomic("0.005").unwrap(), 5000);
        assert_eq!(usd_to_atomic("$0.005").unwrap(), 5000);
        assert_eq!(usd_to_atomic("0.01").unwrap(), 10000);
        assert_eq!(usd_to_atomic("1").unwrap(), 1_000_000);
        assert_eq!(usd_to_atomic("0.000001").unwrap(), 1);
        // 7th fractional digit: round-half-even (1.5 -> 2, 2.5 -> 2).
        assert_eq!(usd_to_atomic("0.0000015").unwrap(), 2);
        assert_eq!(usd_to_atomic("0.0000025").unwrap(), 2);
        assert_eq!(usd_to_atomic("0.0000026").unwrap(), 3);
        assert!(usd_to_atomic("0.0000001").is_err());
        assert!(usd_to_atomic("0").is_err());
        assert!(usd_to_atomic("abc").is_err());
        assert!(usd_to_atomic("").is_err());
    }

    #[test]
    fn atomic_to_usd_cases() {
        assert_eq!(atomic_to_usd(5000), "$0.005");
        assert_eq!(atomic_to_usd(10000), "$0.01");
        assert_eq!(atomic_to_usd(1_000_000), "$1");
        assert_eq!(atomic_to_usd(1), "$0.000001");
    }

    #[test]
    fn cdp_parse_secret_formats() {
        // base64 64-byte Ed25519 (seed + pubkey) — release dummy key.
        let b64_64 = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        match cdp_parse_secret(&b64_64).unwrap() {
            CdpKey::Ed25519(seed) => assert_eq!(seed, [7u8; 32]),
            _ => panic!("expected Ed25519"),
        }
        // base64 bare 32-byte seed.
        let b64_32 = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        match cdp_parse_secret(&b64_32).unwrap() {
            CdpKey::Ed25519(seed) => assert_eq!(seed, [9u8; 32]),
            _ => panic!("expected Ed25519"),
        }
        // SEC1 + PKCS#8 EC PEMs decode to the same scalar.
        let s1 = match cdp_parse_secret(EC_SEC1_PEM).unwrap() {
            CdpKey::Es256(s) => s,
            _ => panic!("expected ES256"),
        };
        let s2 = match cdp_parse_secret(EC_PKCS8_PEM).unwrap() {
            CdpKey::Es256(s) => s,
            _ => panic!("expected ES256"),
        };
        assert_eq!(s1, s2);
        // Ed25519 PKCS#8 PEM decodes to the known seed.
        match cdp_parse_secret(ED_PKCS8_PEM).unwrap() {
            CdpKey::Ed25519(seed) => assert_eq!(seed.as_slice(), hex_bytes(ED_SEED_HEX)),
            _ => panic!("expected Ed25519"),
        }
        // Literal \n-escaped one-line PEM is accepted.
        let escaped = EC_SEC1_PEM.replace('\n', "\\n");
        assert!(matches!(
            cdp_parse_secret(&escaped).unwrap(),
            CdpKey::Es256(_)
        ));
        // Garbage is rejected, never a crash.
        assert!(cdp_parse_secret("not-a-real-secret").is_err());
        assert!(cdp_parse_secret("").is_err());
    }

    fn check_jwt_shape(jwt: &str, alg: &str, method: &str, path: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT must have 3 parts: {jwt}");
        let header: Value = serde_json::from_slice(&b64u_decode(parts[0])).unwrap();
        let claims: Value = serde_json::from_slice(&b64u_decode(parts[1])).unwrap();
        assert_eq!(header["alg"], alg);
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "test-key-id");
        assert_eq!(header["nonce"].as_str().unwrap().len(), 32);
        assert_eq!(claims["sub"], "test-key-id");
        assert_eq!(claims["iss"], "cdp");
        assert_eq!(claims["aud"], json!(["cdp_service"]));
        let nbf = claims["nbf"].as_u64().unwrap();
        let exp = claims["exp"].as_u64().unwrap();
        assert_eq!(exp - nbf, 120);
        let uri = format!("{method} api.cdp.coinbase.com{path}");
        assert_eq!(claims["uri"], uri);
        assert_eq!(claims["uris"], json!([uri]));
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        (
            signing_input.into_bytes(),
            b64u_decode(parts[2]),
            header.to_string().into_bytes(),
        )
    }

    #[test]
    fn cdp_jwt_ed25519_roundtrip() {
        let cfg = test_cfg(ED_PKCS8_PEM);
        let jwt = cdp_jwt(&cfg, "POST", "/platform/v2/x402/verify").unwrap();
        eprintln!("TEST_JWT_ED25519 {jwt}");
        let (signing_input, sig, _) =
            check_jwt_shape(&jwt, "EdDSA", "POST", "/platform/v2/x402/verify");
        // Independent verify path (verifying key from the known seed).
        use ed25519_dalek::Verifier;
        let seed: [u8; 32] = hex_bytes(ED_SEED_HEX).try_into().unwrap();
        let vk = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let signature = ed25519_dalek::Signature::from_bytes(&sig.try_into().unwrap());
        vk.verify(&signing_input, &signature).unwrap();
    }

    #[test]
    fn cdp_jwt_es256_roundtrip() {
        let cfg = test_cfg(EC_SEC1_PEM);
        let jwt = cdp_jwt(&cfg, "POST", "/platform/v2/x402/settle").unwrap();
        eprintln!("TEST_JWT_ES256 {jwt}");
        let (signing_input, sig, _) =
            check_jwt_shape(&jwt, "ES256", "POST", "/platform/v2/x402/settle");
        // Independent verify path (verifying key from the parsed scalar).
        use p256::ecdsa::signature::Verifier as _;
        let scalar = match cdp_parse_secret(EC_SEC1_PEM).unwrap() {
            CdpKey::Es256(s) => s,
            _ => panic!("expected ES256"),
        };
        let sk = p256::ecdsa::SigningKey::from_bytes(&scalar.into()).unwrap();
        let vk = p256::ecdsa::VerifyingKey::from(&sk);
        let signature = p256::ecdsa::Signature::from_slice(&sig).unwrap();
        vk.verify(&signing_input, &signature).unwrap();
    }

    #[test]
    fn cdp_jwt_no_credentials() {
        let mut cfg = test_cfg("");
        cfg.cdp_key_id = String::new();
        assert!(cdp_jwt(&cfg, "POST", "/platform/v2/x402/verify").is_none());
        let mut cfg = test_cfg("not-a-real-secret");
        cfg.cdp_key_id = "x".to_string();
        assert!(cdp_jwt(&cfg, "POST", "/platform/v2/x402/verify").is_none());
    }

    #[test]
    fn decode_payment_header_roundtrip() {
        let payload = json!({"x402Version": 1, "scheme": "exact",
                             "network": "base-sepolia", "payload": {}});
        let header = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&payload).unwrap());
        assert_eq!(decode_payment_header(&header).unwrap(), payload);
        assert!(decode_payment_header("not-base64!!").is_none());
        assert!(decode_payment_header("aGVsbG8=").is_none()); // valid b64, not JSON
    }

    #[test]
    fn charges_batch_sums_paid() {
        let cfg = X402Config {
            pay_to: "0x0".to_string(),
            network: "base-sepolia".to_string(),
            facilitator: "https://x402.org/facilitator".to_string(),
            prices: vec![
                ("cu_status".to_string(), 5000),
                ("cu_item".to_string(), 5000),
                ("cu_get".to_string(), 5000),
            ],
            cdp_key_id: String::new(),
            cdp_key_secret: String::new(),
            is_cdp: false,
        };
        let msgs = vec![
            json!({"method": "tools/call", "params": {"name": "cu_status"}}),
            json!({"method": "tools/call", "params": {"name": "cu_item"}}),
            json!({"method": "tools/call", "params": {"name": "cu_search"}}),
            json!({"method": "tools/list"}),
        ];
        let (total, names) = charges(&cfg, &msgs);
        assert_eq!(total, 10000);
        assert_eq!(names, vec!["cu_status".to_string(), "cu_item".to_string()]);
        assert!(charges_default_paid(&msgs));
        assert!(!charges_default_paid(&[msgs[2].clone(), msgs[3].clone()]));
    }

    #[test]
    fn host_and_path_of() {
        assert_eq!(
            host_of("https://api.cdp.coinbase.com/platform/v2/x402"),
            Some("api.cdp.coinbase.com".to_string())
        );
        assert_eq!(host_of("http://127.0.0.1:8501"), Some("127.0.0.1".to_string()));
        assert_eq!(host_of("not-a-url"), None);
        assert_eq!(
            path_of("https://api.cdp.coinbase.com/platform/v2/x402"),
            "/platform/v2/x402"
        );
        assert_eq!(path_of("https://x402.org/facilitator"), "/facilitator");
    }
}
