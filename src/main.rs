//! Casualties Unknown wiki MCP server — Rust port of server/cu_wiki_mcp.py.
//!
//! Read-only live reader for the unofficial fan database at
//! https://casualtiesunknowndb.com. Pages are fetched LIVE on demand;
//! only caches: sitemap-derived search index (24h), page HTML (1h),
//! sprite PNGs (24h), composed presentation images (24h) — same layout
//! and TTLs as the Python original. Sprites are integer-scaled with
//! NEAREST to fit within 288x288 and pasted centered on a transparent
//! 480x480 canvas, exactly like the original.
//!
//! Protocol: newline-delimited JSON-RPC 2.0 on stdin/stdout (stdio
//! mode, default), or a stateless Streamable-HTTP-style endpoint with
//! `--http <port>` (mirrors api/mcp.py). Logging goes to stderr.

use base64::Engine as _;
use image::{DynamicImage, GenericImageView, ImageEncoder, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

mod x402;

const BASE: &str = "https://casualtiesunknowndb.com";
const UA: &str = "cu-wiki-mcp/1.0 (read-only live reader for \
                casualtiesunknowndb.com; on-demand fetch, no bulk mirroring)";
const TIMEOUT: u64 = 20;
const INDEX_TTL: u64 = 24 * 3600;
const PAGE_TTL: u64 = 3600;
const IMG_TTL: u64 = 24 * 3600;
const CANVAS_SIZE: u32 = 480;
const SPRITE_MAX: u32 = 288;
const MAX_TEXT: usize = 15000;

fn sprite_sections() -> HashSet<&'static str> {
    ["items", "liquids", "moodles"].into_iter().collect()
}

const LISTING_SECTIONS: [&str; 14] = [
    "items", "moodles", "liquids", "recipes", "enemies", "buildings",
    "commands", "depth", "difficulty", "vitals", "hazards", "firearms",
    "anatomy", "skills",
];

type ToolErr = String;

fn log(msg: &str) {
    eprintln!("{msg}");
}

/// Shared reqwest builder: system-native roots (rustls-tls-native-
/// roots, like the existing build), explicit proxy env vars, and
/// NO_PROXY honoured so loopback hosts (e.g. a local mock
/// facilitator) bypass the egress proxy.
pub(crate) fn build_http_client(user_agent: &str, timeout_secs: u64) -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(user_agent);
    let proxy_url = env::var("HTTPS_PROXY")
        .or_else(|_| env::var("https_proxy"))
        .or_else(|_| env::var("ALL_PROXY"))
        .or_else(|_| env::var("all_proxy"))
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(p) = proxy_url {
        if let Ok(proxy) = reqwest::Proxy::all(&p) {
            builder = builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_env()));
        }
    }
    builder.build().expect("HTTP client")
}

// ---------------------------------------------------------------- cache

fn cache_dir() -> PathBuf {
    match env::var("CU_CACHE_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            // mirror the Python default: cache/ in the connector root,
            // here the project root (binary lives in target/release/).
            let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
            let root = exe
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            root.join("cache")
        }
    }
}

fn index_file() -> PathBuf { cache_dir().join("index.json") }
fn pages_dir() -> PathBuf { cache_dir().join("pages") }
fn img_dir() -> PathBuf { cache_dir().join("img") }
fn img_canvas_dir() -> PathBuf { cache_dir().join("img_canvas") }

fn fresh(path: &Path, ttl: u64) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(mtime) = meta.modified() {
            if let Ok(elapsed) = SystemTime::now().duration_since(mtime) {
                return elapsed.as_secs() < ttl;
            }
        }
    }
    false
}

// ---------------------------------------------------------------- fetch

fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // Honor the standard proxy env vars (HTTPS_PROXY / ALL_PROXY)
        // explicitly, like urllib and curl; some sandboxes route all
        // egress through a proxy and direct TLS fails there. The value
        // is read at runtime only — never logged or stored. (ureq was
        // tried first and stalled against this sandbox's egress proxy
        // on the sitemap fetch, so the port uses reqwest.)
        build_http_client(UA, TIMEOUT)
    })
}

fn fetch_bytes(
    url: &str,
    ttl: Option<u64>,
    cache_path: Option<&Path>,
) -> Result<Vec<u8>, ToolErr> {
    if let (Some(ttl), Some(path)) = (ttl, cache_path) {
        if fresh(path, ttl) {
            if let Ok(data) = std::fs::read(path) {
                return Ok(data);
            }
        }
    }
    // error_for_status mirrors urllib: 4xx/5xx are fetch failures.
    let resp = client()
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("fetch failed for {url}: {e}"))?;
    let buf = resp
        .bytes()
        .map_err(|e| format!("fetch failed for {url}: {e}"))?
        .to_vec();
    if let Some(path) = cache_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(path, &buf) {
            log(&format!("cache write failed for {url}: {e}"));
        }
    }
    Ok(buf)
}

fn fetch_page_html(section: &str, slug: &str) -> Result<String, ToolErr> {
    let path = pages_dir().join(format!("{section}__{slug}.html"));
    let data = fetch_bytes(&format!("{BASE}/{section}/{slug}"),
                           Some(PAGE_TTL), Some(&path))?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn sprite_url(slug: &str) -> String {
    format!("{BASE}/icons/sprites/{slug}.png")
}

/// Minimal URL join mirroring urllib.parse.urljoin(BASE + "/", rel)
/// for the reference shapes this site produces.
fn urljoin(rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    if let Some(stripped) = rel.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    if rel.starts_with('/') {
        return format!("{BASE}{rel}");
    }
    format!("{BASE}/{rel}")
}

fn png_key(url: &str) -> String {
    // Python: re.search(r"/([^/]+)\.png($|\?)", url)
    let bytes = url.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &url[i..i + 4] == ".png" {
            let after_ok = i + 4 == url.len() || url[i + 4..].starts_with('?');
            if after_ok {
                if let Some(slash) = url[..i].rfind('/') {
                    let stem = &url[slash + 1..i];
                    if !stem.is_empty() {
                        return stem.to_string();
                    }
                }
            }
        }
        i += 1;
    }
    // fallback: re.sub(r"[^a-z0-9]+", "_", url.lower())
    let mut out = String::new();
    let mut last_us = false;
    for c in url.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn fetch_image(
    section: &str,
    slug: &str,
    first_img: Option<&str>,
) -> (Option<Vec<u8>>, Option<String>) {
    let mut candidates: Vec<String> = Vec::new();
    if sprite_sections().contains(section) {
        candidates.push(sprite_url(slug));
    }
    if let Some(fi) = first_img {
        let abs = urljoin(fi);
        if !candidates.contains(&abs) {
            candidates.push(abs);
        }
    }
    for url in &candidates {
        let key = png_key(url);
        let path = img_dir().join(format!("{section}__{key}.png"));
        let data = match fetch_bytes(url, Some(IMG_TTL), Some(&path)) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let is_img = data.starts_with(b"\x89PNG\r\n\x1a\n")
            || data.starts_with(b"\xff\xd8\xff")
            || data.starts_with(b"GIF8")
            || (data.len() >= 12 && &data[8..12] == b"WEBP");
        if is_img {
            return (Some(data), Some(url.clone()));
        }
    }
    (None, candidates.into_iter().next())
}

// ------------------------------------------------------------- canvas

fn py_round(x: f64) -> u32 {
    x.round_ties_even() as u32
}

/// Compose the image zoomed-out on a transparent square canvas.
/// Returns (png_bytes, Option<(orig, shown)>) — mirrors canvas_image().
fn canvas_image(data: &[u8], cache_path: &Path) -> (Vec<u8>, Option<((u32, u32), (u32, u32))>) {
    let img = match image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
    {
        Ok(r) => match r.decode() {
            Ok(im) => im,
            Err(e) => {
                log(&format!("canvas compose failed, serving original: {e:?}"));
                return (data.to_vec(), None);
            }
        },
        Err(e) => {
            log(&format!("canvas compose failed, serving original: {e:?}"));
            return (data.to_vec(), None);
        }
    };
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    let shown = if longest > SPRITE_MAX {
        let scale = SPRITE_MAX as f64 / longest as f64;
        (
            (py_round(w as f64 * scale)).max(1),
            (py_round(h as f64 * scale)).max(1),
        )
    } else {
        let factor = (SPRITE_MAX / longest).max(1);
        (w * factor, h * factor)
    };
    if fresh(cache_path, IMG_TTL) {
        if let Ok(cached) = std::fs::read(cache_path) {
            return (cached, Some(((w, h), shown)));
        }
    }
    let rgba = img.to_rgba8();
    let sprite = image::imageops::resize(&rgba, shown.0, shown.1,
                                         image::imageops::FilterType::Nearest);
    let mut canvas = RgbaImage::new(CANVAS_SIZE, CANVAS_SIZE);
    let off = (
        (CANVAS_SIZE - shown.0) / 2,
        (CANVAS_SIZE - shown.1) / 2,
    );
    // PIL paste(sprite, offset, sprite): every channel blended with the
    // sprite's own alpha as mask. Onto a fully transparent canvas that
    // is out = src * (src_a/255) per channel.
    for y in 0..shown.1 {
        for x in 0..shown.0 {
            let s = sprite.get_pixel(x, y);
            let m = s.0[3] as f32 / 255.0;
            let mut px = [0u8; 4];
            for i in 0..4 {
                px[i] = (s.0[i] as f32 * m).round() as u8;
            }
            canvas.put_pixel(off.0 + x, off.1 + y, image::Rgba(px));
        }
    }
    let mut buf = Vec::new();
    let enc = image::codecs::png::PngEncoder::new_with_quality(
        &mut buf,
        image::codecs::png::CompressionType::Best,
        image::codecs::png::FilterType::Adaptive,
    );
    if enc
        .write_image(&canvas, CANVAS_SIZE, CANVAS_SIZE,
                     image::ExtendedColorType::Rgba8)
        .is_err()
    {
        return (data.to_vec(), None);
    }
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(cache_path, &buf) {
        log(&format!("img_canvas cache write failed: {e}"));
    }
    (buf, Some(((w, h), shown)))
}

// ------------------------------------------------------------- html in

fn skip_tags() -> HashSet<&'static str> {
    ["script", "style", "noscript", "nav", "header", "footer", "aside",
     "svg", "button", "form"].into_iter().collect()
}

fn block_tags() -> HashSet<&'static str> {
    ["p", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
     "section", "article", "main", "table", "ul", "ol", "pre",
     "blockquote", "td", "th"].into_iter().collect()
}

struct PageData {
    text: String,
    first_img: Option<String>,
    title: String,
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

struct PageWalker {
    require_main: bool,
    in_main: i32,
    skip_depth: i32,
    in_title: bool,
    parts: Vec<String>,
    first_img: Option<String>,
    title: String,
}

impl PageWalker {
    fn active(&self) -> bool {
        !self.require_main || self.in_main > 0
    }

    fn walk(&mut self, node: ego_tree::NodeRef<scraper::Node>) {
        use scraper::Node;
        match node.value() {
            Node::Element(el) => {
                let tag: &str = &el.name.local;
                // handle_starttag
                if tag == "main" {
                    self.in_main += 1;
                }
                if skip_tags().contains(tag) {
                    self.skip_depth += 1;
                }
                if tag == "img" && self.first_img.is_none() {
                    if let Some(src) = el.attr("src") {
                        if !src.is_empty() {
                            self.first_img = Some(src.to_string());
                        }
                    }
                }
                if tag == "title" {
                    self.skip_depth += 1;
                    self.in_title = true;
                }
                if self.active() && self.skip_depth == 0 && block_tags().contains(tag) {
                    self.parts.push("\n".to_string());
                }
                for child in node.children() {
                    self.walk(child);
                }
                // handle_endtag
                if tag == "main" && self.in_main > 0 {
                    self.in_main -= 1;
                }
                if skip_tags().contains(tag) && self.skip_depth > 0 {
                    self.skip_depth -= 1;
                }
                if tag == "title" {
                    if self.skip_depth > 0 {
                        self.skip_depth -= 1;
                    }
                    self.in_title = false;
                }
                if self.active() && self.skip_depth == 0 && block_tags().contains(tag) {
                    self.parts.push("\n".to_string());
                }
            }
            Node::Text(t) => {
                let data: &str = t;
                if self.in_title {
                    self.title.push_str(data);
                    return;
                }
                if self.skip_depth > 0 || !self.active() {
                    return;
                }
                self.parts.push(data.to_string());
            }
            _ => {
                for child in node.children() {
                    self.walk(child);
                }
            }
        }
    }

    fn text(&self) -> String {
        let raw = self.parts.concat();
        let mut out: Vec<String> = Vec::new();
        let mut blank = false;
        for ln in raw.split('\n') {
            let ln = collapse_ws(ln).trim().to_string();
            if !ln.is_empty() {
                out.push(ln);
                blank = false;
            } else if !blank {
                out.push(String::new());
                blank = true;
            }
        }
        let mut text = out.join("\n").trim().to_string();
        if text.chars().count() > MAX_TEXT {
            text = text.chars().take(MAX_TEXT).collect::<String>()
                + "\n[... truncated ...]";
        }
        text
    }
}

fn parse_page(html: &str) -> PageData {
    let doc = scraper::Html::parse_document(html);
    let mut walker = PageWalker {
        require_main: html.contains("<main"),
        in_main: 0,
        skip_depth: 0,
        in_title: false,
        parts: Vec::new(),
        first_img: None,
        title: String::new(),
    };
    walker.walk(doc.tree.root());
    PageData {
        text: walker.text(),
        first_img: walker.first_img,
        title: walker.title,
    }
}

/// Map slug -> display name from a section listing page's anchors.
fn parse_listing(html: &str, section: &str) -> Vec<(String, String)> {
    let doc = scraper::Html::parse_document(html);
    let prefix = format!("/{section}/");
    let mut names: Vec<(String, String)> = Vec::new();

    fn has(names: &[(String, String)], slug: &str) -> bool {
        names.iter().any(|(s, _)| s == slug)
    }

    fn anchor_events(
        node: ego_tree::NodeRef<scraper::Node>,
        slug: &str,
        names: &mut Vec<(String, String)>,
    ) {
        use scraper::Node;
        for child in node.children() {
            match child.value() {
                Node::Element(el) => {
                    if &*el.name.local == "img" && !has(names, slug) {
                        if let Some(alt) = el.attr("alt") {
                            if !alt.is_empty() {
                                names.push((slug.to_string(), alt.to_string()));
                            }
                        }
                    }
                    anchor_events(child, slug, names);
                }
                Node::Text(t) => {
                    let data: &str = t;
                    if !data.trim().is_empty() && !has(names, slug) {
                        names.push((slug.to_string(), data.trim().to_string()));
                    }
                }
                _ => anchor_events(child, slug, names),
            }
        }
    }

    fn walk(node: ego_tree::NodeRef<scraper::Node>, prefix: &str,
            names: &mut Vec<(String, String)>) {
        use scraper::Node;
        if let Node::Element(el) = node.value() {
            if &*el.name.local == "a" {
                if let Some(href) = el.attr("href") {
                    if href.starts_with(prefix) {
                        let slug = href[prefix.len()..].trim_matches('/');
                        if !slug.is_empty() && !slug.contains('/') {
                            anchor_events(node, slug, names);
                        }
                    }
                }
            }
        }
        for child in node.children() {
            walk(child, prefix, names);
        }
    }

    walk(doc.tree.root(), &prefix, &mut names);
    names
}

// ---------------------------------------------------------------- index

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    section: String,
    slug: String,
    name: String,
}

#[derive(Serialize, Deserialize)]
struct Index {
    fetched_at: f64,
    entries: Vec<Entry>,
}

fn build_index() -> Result<Index, ToolErr> {
    let mut entries: Vec<Entry> = Vec::new();
    let sitemap = fetch_bytes(&format!("{BASE}/sitemap.xml"), None, None)?;
    let sitemap = String::from_utf8_lossy(&sitemap).into_owned();
    let mut rest = sitemap.as_str();
    while let Some(start) = rest.find("<loc>") {
        let after = &rest[start + 5..];
        match after.find("</loc>") {
            Some(end) => {
                let url = &after[..end];
                // urllib.parse.urlparse(url).path
                let path = match url.find("://") {
                    Some(i) => {
                        let h = &url[i + 3..];
                        match h.find('/') {
                            Some(j) => &h[j..],
                            None => "",
                        }
                    }
                    None => url,
                };
                let path = path.trim_matches('/');
                let parts: Vec<&str> = path.split('/').collect();
                if parts.len() == 2 {
                    entries.push(Entry {
                        section: parts[0].to_string(),
                        slug: parts[1].to_string(),
                        name: parts[1].to_string(),
                    });
                }
                rest = &after[end + 6..];
            }
            None => break,
        }
    }
    for section in LISTING_SECTIONS {
        if !entries.iter().any(|e| e.section == section) {
            continue;
        }
        let html = match fetch_bytes(&format!("{BASE}/{section}"), None, None) {
            Ok(d) => String::from_utf8_lossy(&d).into_owned(),
            Err(e) => {
                log(&format!("index: listing fetch skipped: {e}"));
                continue;
            }
        };
        for (slug, name) in parse_listing(&html, section) {
            if let Some(e) = entries
                .iter_mut()
                .find(|e| e.section == section && e.slug == slug)
            {
                e.name = name;
            }
        }
    }
    entries.sort_by(|a, b| (&a.section, &a.slug).cmp(&(&b.section, &b.slug)));
    let index = Index {
        fetched_at: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        entries,
    };
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(text) = serde_json::to_string(&index) {
        if let Err(e) = std::fs::write(index_file(), text) {
            log(&format!("index cache write failed: {e}"));
        }
    }
    Ok(index)
}

fn load_index() -> Result<Index, ToolErr> {
    let path = index_file();
    if fresh(&path, INDEX_TTL) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(idx) = serde_json::from_str::<Index>(&text) {
                return Ok(idx);
            }
        }
    }
    build_index()
}

fn get_index() -> Result<&'static Index, ToolErr> {
    static INDEX: OnceLock<Index> = OnceLock::new();
    if let Some(idx) = INDEX.get() {
        return Ok(idx);
    }
    let idx = load_index()?;
    let _ = INDEX.set(idx);
    Ok(INDEX.get().unwrap())
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

fn page_url(section: &str, slug: &str) -> String {
    format!("{BASE}/{section}/{slug}")
}

fn tokenize(low: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in low.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn resolve(section: &str, name: &str) -> Result<Entry, ToolErr> {
    let entries: Vec<&Entry> = get_index()?
        .entries
        .iter()
        .filter(|e| e.section == section)
        .collect();
    let want = slugify(name);
    if want.is_empty() {
        return Err("name must not be empty".to_string());
    }
    for e in &entries {
        if e.slug == want {
            return Ok((*e).clone());
        }
    }
    let low = name.trim().to_lowercase();
    for e in &entries {
        if e.name.to_lowercase() == low {
            return Ok((*e).clone());
        }
    }
    let tokens = tokenize(&low);
    let mut best: Option<&Entry> = None;
    for e in &entries {
        let ename = e.name.to_lowercase();
        if !tokens.is_empty()
            && tokens.iter().all(|t| ename.contains(t.as_str()) || e.slug.contains(t.as_str()))
        {
            let blen = best.map(|b| b.name.to_lowercase().chars().count());
            if blen.is_none() || ename.chars().count() < blen.unwrap() {
                best = Some(e);
            }
        }
    }
    if let Some(b) = best {
        return Ok(b.clone());
    }
    for e in &entries {
        if e.slug.contains(&want) || want.contains(&e.slug) {
            return Ok((*e).clone());
        }
    }
    Err(format!(
        "no {section} entry matching '{name}' on {BASE} (see {BASE}/{section})"
    ))
}

// ------------------------------------------------------- result shape

struct ToolResult {
    text: String,
    image: Option<Vec<u8>>,
}

fn get_entry_result(section: &str, slug: &str, name: Option<&str>) -> Result<ToolResult, ToolErr> {
    let html = fetch_page_html(section, slug)?;
    let page = parse_page(&html);
    let title = match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => {
            let t = page.title.split('\u{2014}').next().unwrap_or("").trim().to_string();
            if t.is_empty() { slug.to_string() } else { t }
        }
    };
    let (image, image_url) = fetch_image(section, slug, page.first_img.as_deref());
    let mut image_sizes = None;
    let mut image = image;
    if let Some(img) = image.take() {
        let (composed, sizes) = canvas_image(
            &img,
            &img_canvas_dir().join(format!("{section}__{slug}.png")),
        );
        image = Some(composed);
        image_sizes = sizes;
    }
    let mut lines = vec![
        format!("{title} [{section}]"),
        format!("URL: {}", page_url(section, slug)),
    ];
    if let Some(url) = &image_url {
        if let Some(((ow, oh), (sw, sh))) = image_sizes {
            lines.push(format!(
                "Image: {url} (original {ow}x{oh}, sprite shown {sw}x{sh} \
                 on {CANVAS_SIZE}x{CANVAS_SIZE} canvas, nearest-neighbor)"
            ));
        } else {
            lines.push(format!("Image: {url}"));
        }
    }
    let text = lines.join("\n") + "\n\n" + &page.text;
    Ok(ToolResult { text, image })
}

// ---------------------------------------------------------------- tools

fn tool_search(args: &Value) -> Result<ToolResult, ToolErr> {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if query.is_empty() {
        return Err("query must not be empty".to_string());
    }
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10);
    let limit = limit.clamp(1, 50) as usize;
    let low = query.to_lowercase();
    let want = slugify(&query);
    let tokens = tokenize(&low);
    let mut scored: Vec<(i64, String, String, Entry)> = Vec::new();
    for e in &get_index()?.entries {
        let ename = e.name.to_lowercase();
        let eslug = &e.slug;
        let mut score = 0;
        if ename.contains(&low) || want == *eslug {
            score = 100;
        } else if !want.is_empty() && eslug.contains(&want) {
            score = 80;
        } else if !tokens.is_empty()
            && tokens.iter().all(|t| ename.contains(t.as_str()) || eslug.contains(t.as_str()))
        {
            score = 60;
        } else if !tokens.is_empty()
            && tokens.iter().any(|t| ename.contains(t.as_str()) || eslug.contains(t.as_str()))
        {
            score = 30;
        }
        if score > 0 {
            scored.push((-score, e.section.clone(), eslug.clone(), e.clone()));
        }
    }
    scored.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
    let mut results = Vec::new();
    for (_, _, _, e) in scored.iter().take(limit) {
        let mut r = Map::new();
        r.insert("section".into(), json!(e.section));
        r.insert("name".into(), json!(e.name));
        r.insert("slug".into(), json!(e.slug));
        r.insert("url".into(), json!(page_url(&e.section, &e.slug)));
        if sprite_sections().contains(e.section.as_str()) {
            let (img, url) = fetch_image(&e.section, &e.slug, None);
            if img.is_some() {
                r.insert("image_url".into(), json!(url));
            }
        }
        results.push(Value::Object(r));
    }
    let mut payload = Map::new();
    payload.insert("query".into(), json!(query));
    payload.insert("count".into(), json!(results.len()));
    payload.insert("results".into(), Value::Array(results));
    let text = serde_json::to_string_pretty(&Value::Object(payload)).unwrap();
    Ok(ToolResult { text, image: None })
}

fn tool_get(args: &Value) -> Result<ToolResult, ToolErr> {
    let section = slugify(args.get("section").and_then(|v| v.as_str()).unwrap_or(""));
    let raw_slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
    let slug = slugify(raw_slug);
    if section.is_empty() || slug.is_empty() {
        return Err("section and slug are required".to_string());
    }
    let mut known: Vec<&str> = get_index()?
        .entries
        .iter()
        .map(|e| e.section.as_str())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    known.sort();
    if !known.contains(&section.as_str()) {
        return Err(format!(
            "unknown section '{section}' on {BASE}; known sections: {}",
            known.join(", ")
        ));
    }
    let entry = resolve(&section, raw_slug)?;
    get_entry_result(&section, &entry.slug.clone(), Some(&entry.name.clone()))
}

fn tool_item(args: &Value) -> Result<ToolResult, ToolErr> {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let entry = resolve("items", name)?;
    let (slug, ename) = (entry.slug.clone(), entry.name.clone());
    let mut result = get_entry_result("items", &slug, Some(&ename))?;
    let has_liquid = get_index()?
        .entries
        .iter()
        .any(|e| e.section == "liquids" && e.slug == slug);
    if has_liquid {
        let html = fetch_page_html("liquids", &slug)?;
        let page = parse_page(&html);
        result.text += &format!(
            "\n\n===== Liquid effects ({}) =====\n\n{}",
            page_url("liquids", &slug),
            page.text
        );
    }
    Ok(result)
}

fn tool_status(args: &Value) -> Result<ToolResult, ToolErr> {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let entry = resolve("moodles", name)?;
    let (slug, ename) = (entry.slug.clone(), entry.name.clone());
    get_entry_result("moodles", &slug, Some(&ename))
}

// ------------------------------------------------------------ protocol

fn tools_list() -> Value {
    json!([
        {
            "name": "cu_search",
            "description": "Search the Casualties Unknown fan DB across all sections (items, liquids, moodles/status effects, recipes, enemies, buildings, commands, vitals...). Returns matches with section, name, slug, page URL and sprite image_url when one exists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 10}
                },
                "required": ["query"]
            }
        },
        {
            "name": "cu_get",
            "description": "Fetch one DB page live by section and slug (e.g. section='liquids', slug='procoagulant') and return its extracted text, plus the entry's image when the page/sprite provides one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "section": {"type": "string"},
                    "slug": {"type": "string"}
                },
                "required": ["section", "slug"]
            }
        },
        {
            "name": "cu_item",
            "description": "Look up an item by name or slug. Returns the item page text COMBINED with its liquid-effects page text when a matching liquid exists, plus the item's sprite image.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "cu_status",
            "description": "Look up a status effect / moodle by name or slug (e.g. 'stroke'). Returns the moodles page text plus its icon image when one exists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        }
    ])
}

fn call_tool(name: &str, args: &Value) -> Result<ToolResult, ToolErr> {
    match name {
        "cu_search" => tool_search(args),
        "cu_get" => tool_get(args),
        "cu_item" => tool_item(args),
        "cu_status" => tool_status(args),
        _ => unreachable!(),
    }
}

/// Handle one JSON-RPC message. Returns None for notifications,
/// mirroring handle() in the Python server.
fn handle(msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let pv = msg
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": pv,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "cu-wiki-mcp", "version": "1.0.0"}
                }
            }))
        }
        "ping" => Some(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tools_list()}
        })),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !["cu_search", "cu_get", "cu_item", "cu_status"].contains(&name) {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": format!("unknown tool: {name}")}
                }));
            }
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(name, &args) {
                Ok(result) => {
                    let mut content = vec![json!({"type": "text", "text": result.text})];
                    if let Some(img) = &result.image {
                        content.push(json!({
                            "type": "image",
                            "mimeType": "image/png",
                            "data": base64::engine::general_purpose::STANDARD.encode(img)
                        }));
                    }
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": content}
                    }))
                }
                Err(e) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": format!("Error: {e}")}],
                        "isError": true
                    }
                })),
            }
        }
        m if m.starts_with("notifications/") => None,
        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("unknown method: {method}")}
        })),
    }
}

fn run_stdio() {
    log(&format!("cu-wiki-mcp: serving {BASE} (cache: {})", cache_dir().display()));
    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock()).lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    while let Some(Ok(line)) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                });
                let _ = writeln!(out, "{resp}");
                let _ = out.flush();
                continue;
            }
        };
        if let Some(resp) = handle(&msg) {
            let _ = writeln!(out, "{resp}");
            let _ = out.flush();
        }
    }
}

// ------------------------------------------------------------ http mode

fn http_info() -> Value {
    json!({
        "name": "cu-wiki-mcp",
        "version": "1.0.0",
        "transport": "streamable-http (stateless, JSON responses)",
        "tools": ["cu_get", "cu_item", "cu_search", "cu_status"],
        "data_source": BASE,
        "usage": "POST a JSON-RPC 2.0 message: initialize, tools/list, tools/call, ping.",
        "x402": x402::public_descriptor()
    })
}

struct HttpOutcome {
    status: u16,
    payload: Option<Value>,
    /// Base64-encoded settlement receipt for X-PAYMENT-RESPONSE.
    payment_response: Option<String>,
}

fn outcome(status: u16, payload: Option<Value>) -> HttpOutcome {
    HttpOutcome { status, payload, payment_response: None }
}

/// One raw request body -> outcome. Mirrors process() in api/mcp.py,
/// including the x402 gate: free calls pass through; paid calls
/// need a valid X-PAYMENT, verified + settled by the facilitator,
/// before content is served.
fn http_process(raw: &[u8], headers: &HashMap<String, String>) -> HttpOutcome {
    let text = match std::str::from_utf8(raw) {
        Ok(t) => t,
        Err(e) => {
            return outcome(400, Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            })));
        }
    };
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return outcome(400, Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            })));
        }
    };
    let (messages, is_batch) = match &msg {
        Value::Array(items) => (items.clone(), true),
        Value::Object(_) => (vec![msg.clone()], false),
        _ => {
            return outcome(400, Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32600, "message": "expected a JSON-RPC object"}
            })));
        }
    };

    match x402::x402_state() {
        x402::X402State::Misconfigured(err) => {
            // Fail closed: paid tools are unavailable, never free.
            if x402::charges_default_paid(&messages) {
                return outcome(402, Some(json!({
                    "x402Version": x402::X402_VERSION,
                    "error": format!("x402 misconfiguration: {err} — paid tools unavailable"),
                    "accepts": [],
                })));
            }
        }
        x402::X402State::Armed(cfg) => {
            let (total, names) = x402::charges(&cfg, &messages);
            if total > 0 {
                let resource = x402::resource_url(
                    headers.get("host").map(|s| s.as_str()),
                    headers.get("x-forwarded-proto").map(|s| s.as_str()),
                );
                let requirements =
                    x402::payment_requirements(&cfg, total, &resource, &names);
                let payment_header = headers.get("x-payment");
                let Some(payment_header) = payment_header.filter(|h| !h.trim().is_empty()) else {
                    return outcome(402, Some(x402::body_402(
                        &requirements, "X-PAYMENT header is required")));
                };
                let payment = x402::decode_payment_header(payment_header);
                let valid_shape = payment.as_ref().map(|p| {
                    p.get("scheme").and_then(|s| s.as_str()) == Some("exact")
                        && p.get("network").and_then(|s| s.as_str())
                            == Some(cfg.network.as_str())
                }).unwrap_or(false);
                let Some(payment) = payment.filter(|_| valid_shape) else {
                    return outcome(402, Some(x402::body_402(
                        &requirements, "invalid X-PAYMENT header")));
                };
                let verify = x402::facilitator_call(&cfg, "verify", &payment, &requirements);
                let Some(verify) = verify else {
                    return outcome(402, Some(x402::body_402(
                        &requirements,
                        "payment verification unavailable (facilitator unreachable)")));
                };
                if verify.get("isValid").and_then(|v| v.as_bool()) != Some(true) {
                    let reason = verify
                        .get("invalidReason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    return outcome(402, Some(x402::body_402(
                        &requirements, &format!("payment invalid: {reason}"))));
                }
                let exec = execute(&messages, is_batch);
                let settle = x402::facilitator_call(&cfg, "settle", &payment, &requirements);
                let settled = settle
                    .as_ref()
                    .and_then(|s| s.get("success"))
                    .and_then(|v| v.as_bool())
                    == Some(true);
                if !settled {
                    let detail = settle
                        .as_ref()
                        .and_then(|s| s.get("errorReason"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("facilitator did not settle");
                    return outcome(402, Some(x402::body_402(
                        &requirements,
                        &format!("payment settlement failed: {detail}"))));
                }
                let receipt = base64::engine::general_purpose::STANDARD.encode(
                    serde_json::to_string(&settle.unwrap()).unwrap_or_default(),
                );
                return HttpOutcome {
                    status: exec.0,
                    payload: exec.1,
                    payment_response: Some(receipt),
                };
            }
        }
        x402::X402State::Off => {}
    }

    let (status, payload) = execute(&messages, is_batch);
    outcome(status, payload)
}

/// Run parsed messages through the JSON-RPC handler.
fn execute(messages: &[Value], is_batch: bool) -> (u16, Option<Value>) {
    if is_batch {
        let responses: Vec<Value> = messages.iter().filter_map(handle).collect();
        if responses.is_empty() {
            return (202, None);
        }
        return (200, Some(Value::Array(responses)));
    }
    match handle(&messages[0]) {
        None => (202, None),
        Some(r) => (200, Some(r)),
    }
}

fn http_send(stream: &mut std::net::TcpStream, status: u16, body: &[u8],
             content_type: &str, extra_headers: &[(&str, &str)]) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        402 => "Payment Required",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, Accept, Mcp-Session-Id, MCP-Protocol-Version, X-PAYMENT\r\n\
         Access-Control-Expose-Headers: X-PAYMENT-RESPONSE\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    if !body.is_empty() {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

fn http_handle_conn(mut stream: std::net::TcpStream) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    // read headers (keys lowercased; values kept verbatim)
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length: usize = 0;
    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                let t = line.trim();
                if t.is_empty() {
                    break;
                }
                if let Some((k, v)) = t.split_once(':') {
                    let key = k.trim().to_lowercase();
                    let val = v.trim().to_string();
                    if key == "content-length" {
                        content_length = val.parse().unwrap_or(0);
                    }
                    headers.insert(key, val);
                }
            }
            Err(_) => return,
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path_full = parts.next().unwrap_or("/");
    let path = path_full.split('?').next().unwrap_or("/");
    match method {
        "POST" => {
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
            }
            let out = http_process(&body, &headers);
            let bytes = out
                .payload
                .map(|p| serde_json::to_vec(&p).unwrap_or_default())
                .unwrap_or_default();
            let extra: Vec<(&str, &str)> = match &out.payment_response {
                Some(r) => vec![("X-PAYMENT-RESPONSE", r.as_str())],
                None => vec![],
            };
            http_send(&mut stream, out.status, &bytes, "application/json", &extra);
        }
        "GET" => {
            if path == "/" || path == "/index.html" {
                let root = env::current_exe()
                    .ok()
                    .and_then(|e| {
                        e.parent()
                            .and_then(|p| p.parent())
                            .and_then(|p| p.parent())
                            .map(|p| p.to_path_buf())
                    })
                    .unwrap_or_else(|| PathBuf::from("."));
                let html = std::fs::read(root.join("index.html")).unwrap_or_else(|_| {
                    b"<!DOCTYPE html><title>Casualties Unknown Wiki MCP</title><p>POST JSON-RPC to /mcp.</p>".to_vec()
                });
                http_send(&mut stream, 200, &html, "text/html; charset=utf-8", &[]);
            } else {
                let info = serde_json::to_vec(&http_info()).unwrap_or_default();
                http_send(&mut stream, 200, &info, "application/json", &[]);
            }
        }
        "OPTIONS" => http_send(&mut stream, 204, &[], "application/json", &[]),
        _ => http_send(&mut stream, 200, &[], "application/json", &[]),
    }
}

fn run_http(port: u16) {
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))
        .unwrap_or_else(|e| panic!("bind {port}: {e}"));
    log(&format!("cu-wiki-mcp: HTTP on :{port} (cache: {})", cache_dir().display()));
    for conn in listener.incoming() {
        if let Ok(stream) = conn {
            std::thread::spawn(move || http_handle_conn(stream));
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--http") {
        let port: u16 = args
            .get(i + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);
        run_http(port);
    } else {
        run_stdio();
    }
}
