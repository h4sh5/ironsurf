//! Archive **browsing** server (task: archive server mode).
//!
//! Archive mode (💾) writes visits to disk; this module lets the user *read*
//! them back inside the browser. `ironsurf://archive` asks the shell to bring
//! the server up — tabs are sandboxed and the archive tree belongs to the
//! shell, so only the shell may bind a socket and read the folder. The shell
//! listens on **localhost** (`127.0.0.1` on a **random free port** by default,
//! both changeable from the server's own control page) and serves the archive
//! folder (`$HOME/Downloads/ironsurf-tabs` by default) read-only:
//!
//! ```text
//! GET  /                      → the archive front page: the visit timeline, each visit
//!                               folder pre-traversed to the sites saved under it
//! GET  /ironsurf/serve        → the control page (status + folder/host/port form)
//! GET  /ironsurf/serve/apply  → apply form values, rebind, 302 back with a note
//! GET  /ironsurf/serve/stop   → stop serving (the control page stays up)
//! GET  /…                     → a file, or a directory listing under the root
//! ```
//!
//! Two threads per server: a **control** thread (bound once, on
//! `127.0.0.1:<random>`, so the control page keeps one address the user can
//! bookmark even after the served address moves) and a **serve** thread (whose
//! listener is replaced whenever the folder, host or port changes). Only the
//! control thread mutates the shared state; a *generation* counter retires the
//! previous serve thread when the listener is rebound. Every change is reported
//! to the shell's event loop through a bounded channel, which mirrors it into
//! the shell settings and persists `ui.json` (best effort). Nothing here
//! touches Slint, so the whole module runs — and is tested — headless.
//!
//! Serving is deliberately boring HTTP/1.1 on `std::net` alone: `Connection:
//! close`, no keep-alive, no chunked bodies, a 30 s socket timeout, headers
//! capped at 64 KiB. **Traversal is refused**: a target path is joined segment
//! by segment with separators (also `%2f`, `%5c`) and dot-segments
//! neutralized, then canonicalized and required to live under the root — the
//! browser can only read inside the folder it archived itself.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use std::os::fd::AsRawFd;

use ironsurf_common::{debug, error};
use serde::{Deserialize, Serialize};

use crate::archive::default_archive_root;

/// The host the control socket always binds: loopback only. An archive of
/// someone's browsing is not a public offering.
pub const CONTROL_HOST: &str = "127.0.0.1";
/// Default host for the served address.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Port `0` means "pick a free one" — the default, i.e. a random port.
pub const RANDOM_PORT: u16 = 0;
/// The control page's path, answered on the served socket *and* on the
/// control socket, so both addresses show the same page.
pub const CONTROL_PATH: &str = "/ironsurf/serve";
/// Socket timeout for every accepted connection: a stalled client may not hold
/// the single-threaded server hostage.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest request header block we will read.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Largest POST body we will read (the control form is a few hundred bytes).
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Largest file the serving thread reads into memory (archived video is
/// bigger than a browser session deserves).
const MAX_SERVE_BYTES: u64 = 512 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// What to serve and from where. `port == 0` = a random free port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// Absolute archive folder to serve.
    pub root: PathBuf,
    /// Host to bind (`127.0.0.1` unless the user typed something else).
    pub host: String,
    /// Port to bind (`0` = random).
    pub port: u16,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            root: default_archive_root(),
            host: DEFAULT_HOST.to_string(),
            port: RANDOM_PORT,
        }
    }
}

/// The spec the shell should serve, from its persisted settings: a saved
/// folder/host wins, otherwise the fallback root and `127.0.0.1`; a saved port
/// wins, otherwise a random free one. Blank strings mean "not set" — the
/// control page's form submits empty fields when the user clears them.
pub fn spec_from(
    root: Option<&str>,
    fallback_root: PathBuf,
    host: Option<String>,
    port: Option<u16>,
) -> Spec {
    let root = match root.map(str::trim).filter(|r| !r.is_empty()) {
        Some(text) => normalize_root(text).unwrap_or_else(|_| {
            // A folder that cannot be used (typo, permissions) must not sink
            // the server: the configured archive root is always fair game.
            let fallback = fallback_root.clone();
            let _ = std::fs::create_dir_all(&fallback);
            fallback
        }),
        None => fallback_root,
    };
    let host = match host.map(|h| h.trim().to_string()).filter(|h| !h.is_empty()) {
        Some(text) => normalize_host(&text).unwrap_or_else(|_| DEFAULT_HOST.to_string()),
        None => DEFAULT_HOST.to_string(),
    };
    Spec { root, host, port: port.unwrap_or(RANDOM_PORT) }
}

/// `http://host:port`, the form used on the wire and in listings.
pub fn address_of(host: &str, port: u16) -> String {
    format!("http://{host}:{port}")
}

/// Normalize a host the user typed: trim, lowercase, `localhost` →
/// `127.0.0.1` (loopback either way, with no DNS in the picture). A URL, a
/// path or a `host:port` string is rejected — the bind itself is the final
/// judge.
/// The address a form field may ask the archive server to bind: **loopback,
/// nothing else**. The served folder is unauthenticated — there is no login,
/// no cookie, no token in front of a plain `GET /…` — so a wildcard or a
/// routable address would export the user's archive (and after a folder
/// override, anything on the machine) to whoever can reach the port. The form
/// still shows the host field, and a rejected one falls back to `127.0.0.1`.
pub fn normalize_host(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(DEFAULT_HOST.to_string());
    }
    if trimmed == "localhost" || trimmed == "127.0.0.1" {
        return Ok("127.0.0.1".to_string());
    }
    if trimmed == "::1" {
        return Ok("::1".to_string());
    }
    // The whole 127/8 is loopback on every platform we ship on.
    if trimmed.starts_with("127.")
        && trimmed.split('.').all(|label| {
            !label.is_empty() && label.len() <= 3 && label.parse::<u8>().is_ok()
        })
    {
        return Ok(trimmed);
    }
    Err(format!(
        "only the loopback address may serve the archive (it is unauthenticated): {raw}"
    ))
}

/// Folders the archive server will never export, however it is asked: the
/// system tree and the machine root are not archives, and an unauthenticated
/// server pointed at them leaks the machine rather than the session. Checked
/// before anything is created, so a hostile `root` cannot plant directories.
fn forbidden_root(raw: &str) -> bool {
    const DENY: [&str; 15] = [
        "/", "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/dev", "/proc", "/sys",
        "/boot", "/run", "/root", "/srv", "/var",
    ];
    let trimmed = raw.trim_end_matches('/');
    let candidate = if trimmed.is_empty() { "/" } else { trimmed };
    DENY.iter().any(|denied| {
        candidate == *denied || candidate.starts_with(&format!("{denied}/"))
    })
}

/// Parse a port field: empty / `0` / `random` → [`RANDOM_PORT`], else 1–65535.
pub fn parse_port(raw: &str) -> Result<u16, String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() || trimmed == "0" || trimmed == "random" || trimmed == "any" {
        return Ok(RANDOM_PORT);
    }
    trimmed
        .parse::<u16>()
        .map_err(|_| format!("not a port: {raw}"))
        .and_then(|p| {
            if p == 0 {
                Err(format!("not a port: {raw}"))
            } else {
                Ok(p)
            }
        })
}

/// Normalize a folder field: trim, expand a leading `~`, insist on an absolute
/// path, create the tree (an archive folder that does not exist yet is a
/// feature, not an error), and return the canonical absolute path so symlinks
/// and trailing separators do not persist in the server configuration.
pub fn normalize_root(raw: &str) -> Result<PathBuf, String> {
    let expanded = expand_tilde(raw.trim());
    if expanded.is_empty() {
        return Err("folder must not be empty".to_string());
    }
    let path = PathBuf::from(&expanded);
    if !path.is_absolute() {
        return Err(format!("folder must be an absolute path: {expanded}"));
    }
    if forbidden_root(&expanded) {
        return Err(format!("{expanded} is a system folder, not an archive folder"));
    }
    std::fs::create_dir_all(&path).map_err(|e| format!("cannot create {expanded}: {e}"))?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize {expanded}: {e}"))?;
    Ok(canonical)
}

/// `~` and `~/…` expanded against `$HOME` (left alone when `HOME` is unset).
pub fn expand_tilde(raw: &str) -> String {
    if raw == "~" {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).display().to_string())
            .unwrap_or_else(|| raw.to_owned());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            path.push(rest);
            return path.display().to_string();
        }
    }
    raw.to_owned()
}

// ---------------------------------------------------------------------------
// Small HTTP helpers (pure)
// ---------------------------------------------------------------------------

/// Percent-decode UTF-8 `%XX` escapes. A `+` is **not** a space here (archive
/// folder names carry ISO8601 timestamps and offsets, and a URL path never
/// spells a space that way); malformed escapes pass through verbatim so text
/// never gains meaning it did not have.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    // The escape is validated on **bytes**, never by slicing `s` at offsets the
    // input chose: `%` + two ASCII hex bytes decodes, anything else — including
    // a `%` in front of a multibyte character, whose bytes are not ASCII hex —
    // is copied verbatim. `&s[i + 1..i + 3]` used to slice off a char boundary
    // and panic, and because the request path is decoded here and again per
    // segment in [`resolve_inside`], one listed file name stopped the server.
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `%XX` digit as 0–15; `None` for anything that is not ASCII hex.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode a path for a listing link: `/` survives, everything that is
/// not path-ish is escaped, so names holding `?`, `#`, `%` or spaces stay
/// clickable.
pub fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'/' => out.push('/'),
            _ if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode a note for a redirect query (nothing survives but alnum).
pub fn percent_encode_note(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.as_bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode a *form* value, where `+` means a space.
pub fn percent_decode_form(s: &str) -> String {
    let decoded = percent_decode(s);
    decoded.replace('+', " ")
}

/// Parse `application/x-www-form-urlencoded` (a body or a query string) into
/// decoded key → value pairs; the last value wins for a repeated key.
pub fn parse_form(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (percent_decode_form(k), percent_decode_form(v)),
            None => (percent_decode(pair), String::new()),
        };
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => out.push((key, value)),
        }
    }
    out
}

/// `form.get("root")`: `None` = the field was absent, `Some("")` = present but
/// blank (the control form treats both as "keep what is configured").
pub fn form_get<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
    form.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Escape for HTML PCDATA/attributes (folder, host, port and note all come
/// from a request — never parse user text as markup).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Content type by file extension (an archive holds whatever the sites sent;
/// unknown types are `application/octet-stream` so nothing guesses markup out
/// of a binary file).
/// The media type of a file **with its first bytes in hand**: the extension
/// first, then a cheap sniff of what is actually inside for the shapes an
/// archived page leaves behind without an extension (an archived
/// `…/index.html` served under a query-less name, a document saved as
/// `example.com`, a page served from a URL ending in `?q=…`).
pub fn content_type_of_with_sniff(path: &Path, head: &[u8]) -> &'static str {
    let by_ext = content_type_of(path);
    if by_ext != "application/octet-stream" || head.is_empty() {
        return by_ext;
    }
    let trimmed = match std::str::from_utf8(head) {
        Ok(text) => text.trim_start(),
        // Not UTF-8 at all: leave it binary.
        Err(_) => return by_ext,
    };
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        "text/html; charset=utf-8"
    } else if lower.starts_with("<?xml") {
        "text/xml; charset=utf-8"
    } else {
        by_ext
    }
}

/// The media type of a file by **name only** (see
/// [`content_type_of_with_sniff`] for the sniffing variant).
pub fn content_type_of(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" | "xhtml" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" | "md" | "csv" | "log" => "text/plain; charset=utf-8",
        "xml" | "rss" | "atom" | "svg" => "text/xml; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" | "oga" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// What a request needs answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// `Location` for a 302.
    pub location: Option<String>,
    /// `true` for HEAD: headers as if GET, no body on the wire.
    pub no_body: bool,
}

impl Response {
    fn html(status: u16, body: String, no_body: bool) -> Self {
        Response {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
            location: None,
            no_body,
        }
    }
    fn redirect(location: &str) -> Self {
        Response {
            status: 302,
            content_type: "text/html; charset=utf-8",
            body: format!(
                "<!doctype html><meta charset=utf-8><title>Redirecting</title><a href=\"{}\">{}</a>",
                escape_html(location),
                escape_html(location)
            )
            .into_bytes(),
            location: Some(location.to_string()),
            no_body: false,
        }
    }
    fn error(status: u16, why: &str) -> Self {
        Response::html(status, render_error_page(status, why), false)
    }
    /// The full HTTP/1.1 response bytes (`Connection: close`, since the client
    /// must not sit on a keep-alive we do not implement).
    pub fn to_http(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            204 => "No Content",
            302 => "Found",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        if let Some(location) = &self.location {
            head.push_str(&format!("Location: {location}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        if !self.no_body {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// Split a request head into `(method, decoded path, raw query)`. `None` = not
/// a request line we can serve (bad version, unsupported method, non-origin
/// target).
pub fn parse_request_head(head: &str) -> Option<(String, String, String)> {
    let line = head.lines().next()?.trim_end_matches('\r');
    let mut words = line.split_whitespace();
    let method = words.next()?.to_ascii_uppercase();
    let target = words.next()?;
    let version = words.next().unwrap_or("HTTP/1.1");
    if !matches!(method.as_str(), "GET" | "HEAD" | "POST") {
        return None;
    }
    if !version.starts_with("HTTP/1.") && !version.starts_with("HTTP/2") {
        return None;
    }
    if words.next().is_some() || !target.starts_with('/') {
        return None; // only origin-form targets
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    // The path is decoded percent-only; the query is handed over raw and
    // decoded by whoever parses it as a form.
    Some((method, percent_decode(&path), query))
}

// ---------------------------------------------------------------------------
// Path resolution (traversal-proof)
// ---------------------------------------------------------------------------

/// Resolve a request path **inside** `root`: each segment is percent-decoded,
/// stripped of separators (so `%2f`/`%5c` cannot forge one) and trimmed;
/// dot-segments resolve the URL way (a `..` above the root pops the root, i.e.
/// stays at it) and the result must still be under the root after
/// canonicalization — so a symlink out of the folder is refused too.
/// `Err(403)` = traversal attempt, `Err(404)` = nothing there.
pub fn resolve_inside(root: &Path, url_path: &str) -> Result<PathBuf, u16> {
    let mut target = root.to_path_buf();
    for raw in url_path.split('/') {
        let decoded = percent_decode(raw);
        let cleaned: String = decoded
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '/' | '\\'))
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() || cleaned == "." {
            continue;
        }
        if cleaned == ".." {
            // Never pop above the root: a hostile `..` can only ever mean
            // "the root itself", so no path can ever leave the folder.
            if target != root {
                target.pop();
            }
            continue;
        }
        target.push(cleaned);
    }
    if !target.starts_with(root) {
        return Err(403);
    }
    let root_canon = root.canonicalize().map_err(|_| 404u16)?;
    let target_canon = target.canonicalize().map_err(|_| 404u16)?;
    if !target_canon.starts_with(&root_canon) {
        return Err(403);
    }
    Ok(target_canon)
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// The shared dark stylesheet (same family as the internal pages).
const STYLE: &str = "body { margin: 0; background: #14161a; color: #e6e6e6;
    font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 52em; margin: 0 auto; padding: 2.5em 1.5em; }
  h1 { color: #8fce8f; margin: 0 0 0.2em 0; font-size: 1.4em; }
  h2 { color: #8fce8f; font-size: 1.05em; margin: 1.8em 0 0.5em 0; }
  .sub { color: #9aa0a6; margin: 0 0 1.6em 0; }
  a { color: #8fce8f; text-decoration: none; }
  code, kbd { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
  table { border-collapse: collapse; width: 100%; }
  td { padding: 0.18em 0.6em 0.18em 0; vertical-align: top; }
  .size { color: #6b7178; white-space: nowrap; }
  .visit td { font-weight: bold; }
  .child td { padding-left: 1.8em; }
  .child .size { font-weight: normal; }
  .dim { color: #6b7178; font-style: italic; }
  input[type=text] { width: 100%; box-sizing: border-box; background: #1f2227;
    color: #e6e6e6; border: 1px solid #2c3037; border-radius: 3px; padding: 0.35em 0.5em;
    font-family: monospace; }
  button { background: #23272e; color: #e6e6e6; border: 1px solid #3a4048;
    border-radius: 3px; padding: 0.35em 0.9em; font-family: monospace; cursor: pointer; }
  .note { background: #1f2227; border-left: 3px solid #8fce8f; padding: 0.5em 0.8em;
    margin: 0 0 1.4em 0; }
  .bad { border-left-color: #e57373; }
  .foot { margin-top: 2.5em; color: #6b7178; font-size: 0.9em; }";

/// The prologue + `<head>` for a given title.
fn page_open(title: &str) -> String {
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n\
         <style>\n{STYLE}\n  </style>\n</head>\n<body>\n<div class=\"wrap\">\n",
        escape_html(title)
    )
}

const PAGE_CLOSE: &str = "</div>\n</body>\n</html>\n";

/// A plain error page (400/403/404/405/500) in the house style.
pub fn render_error_page(status: u16, why: &str) -> String {
    let title = match status {
        403 => "Not yours to read",
        404 => "Not in the archive",
        405 => "Method not allowed",
        400 => "Bad request",
        _ => "Server error",
    };
    format!(
        "{}  <h1>{title}</h1>\n  <p><code>{status}</code> &mdash; {}</p>\n  \
         <p><a href=\"{CONTROL_PATH}\">Archive server control page</a></p>\n{PAGE_CLOSE}",
        page_open(title),
        escape_html(why)
    )
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < 1024 * KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * KB as f64))
    }
}

/// How many visit folders the front page pre-traverses at all.
const MAX_TIMELINE_VISITS: usize = 250;
/// How many sites the front page lists under one visit folder.
const MAX_TIMELINE_SITES: usize = 100;

/// The sites saved under one visit folder — its subdirectories (one per
/// archived `host[:port]`) first, then stray files, each capped so the front
/// page stays bounded however the archive grew. `None` when the folder cannot
/// be read: a broken visit never takes the front page down with it.
fn timeline_children(dir: &Path) -> Option<(Vec<String>, Vec<(String, u64)>, bool)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut sites: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut truncated = false;
    for entry in entries.flatten() {
        // `file_type` does not follow symlinks: a link that *points at* a
        // directory is never treated as a site to name or traverse into.
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false);
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_dir {
            if sites.len() < MAX_TIMELINE_SITES {
                sites.push(name);
            } else {
                truncated = true;
            }
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if files.len() < MAX_TIMELINE_SITES {
                files.push((name, size));
            } else {
                truncated = true;
            }
        }
    }
    sites.sort();
    files.sort();
    Some((sites, files, truncated))
}

/// A directory listing: parent link, then subdirectories (trailing slash, so
/// relative links work) and files with their sizes — alphabetically, with the
/// per-visit ISO8601 folders sorting into a timeline for free.
///
/// The **root** listing is the archive front page, so it also *pre-traverses*
/// each visit folder one level: every site saved under a timestamp is linked
/// directly (`&lt;visit&gt;/&lt;host[:port]&gt;/`), instead of the user having
/// to open every timestamp first to find a saved page. The reach is capped
/// ([`MAX_TIMELINE_VISITS`], [`MAX_TIMELINE_SITES`]) and symlinked folders are
/// listed but never traversed.
pub fn render_listing(dir: &Path, url_path: &str) -> Result<String, u16> {
    let entries = std::fs::read_dir(dir).map_err(|_| 404u16)?;
    let front_page = url_path.is_empty() || url_path == "/";
    let mut rows: Vec<(String, bool, u64, bool)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false);
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if is_dir {
            rows.push((format!("{name}/"), true, 0, true));
        } else {
            rows.push((name, false, size, false));
        }
    }
    rows.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });

    let shown = if url_path.is_empty() { "/" } else { url_path };
    let mut out = page_open("Archive listing");
    out.push_str(&format!(
        "  <h1>Archive folder</h1>\n  <p class=\"sub\">{}<br><code>{}</code></p>\n",
        escape_html(&dir.display().to_string()),
        escape_html(shown)
    ));
    if shown != "/" {
        out.push_str("  <table>\n    <tr><td><a href=\"../\">../</a></td><td></td></tr>\n");
    } else {
        out.push_str("  <table>\n");
    }
    let mut expanded = 0usize;
    let mut capped_note_shown = false;
    for (name, is_dir, size, traversable) in &rows {
        let href = percent_encode_path(name);
        let size_text = if *is_dir {
            String::new()
        } else {
            human_size(*size)
        };
        let row_class = if *is_dir && front_page { " class=\"visit\"" } else { "" };
        out.push_str(&format!(
            "    <tr{row_class}><td><a href=\"{href}\">{}</a></td><td class=\"size\">{size_text}</td></tr>\n",
            escape_html(name)
        ));
        if !(*is_dir && front_page && *traversable) {
            continue;
        }
        // The front page reaches one level down: the sites of this visit,
        // linked straight to `<visit>/<site>/` so no click per timestamp is
        // needed to get at a saved page. A folder it cannot read simply
        // shows nothing under it — the timestamp link still stands.
        if expanded >= MAX_TIMELINE_VISITS {
            if !capped_note_shown {
                capped_note_shown = true;
                out.push_str("    <tr class=\"child\"><td class=\"dim\">\u{2026} more visit folders than the front page traverses \u{2014} open them one at a time</td><td></td></tr>\n");
            }
            continue;
        }
        expanded += 1;
        let child_href_base = name.clone(); // "<visit>/"
        match timeline_children(&dir.join(name.trim_end_matches('/'))) {
            None => out.push_str("    <tr class=\"child\"><td class=\"dim\">(cannot be read)</td><td></td></tr>\n"),
            Some((sites, files, truncated)) => {
                if sites.is_empty() && files.is_empty() {
                    out.push_str("    <tr class=\"child\"><td class=\"dim\">(no sites saved here)</td><td></td></tr>\n");
                }
                for site in &sites {
                    let child = format!("{child_href_base}{site}/");
                    out.push_str(&format!(
                        "    <tr class=\"child\"><td><a href=\"{}\">{}/</a></td><td class=\"size\"></td></tr>\n",
                        percent_encode_path(&child),
                        escape_html(site)
                    ));
                }
                for (file, fsize) in &files {
                    let child = format!("{child_href_base}{file}");
                    out.push_str(&format!(
                        "    <tr class=\"child\"><td><a href=\"{}\">{}</a></td><td class=\"size\">{}</td></tr>\n",
                        percent_encode_path(&child),
                        escape_html(file),
                        human_size(*fsize)
                    ));
                }
                if truncated {
                    out.push_str("    <tr class=\"child\"><td class=\"dim\">\u{2026} more saved here \u{2014} open the visit folder to see all of it</td><td></td></tr>\n");
                }
            }
        }
    }
    out.push_str("  </table>\n");
    if rows.is_empty() {
        out.push_str("  <p class=\"foot\">This folder is empty. Turn archive mode (\u{1F4BE}) on and visit something, then reload.</p>\n");
    }
    out.push_str(
        "  <p class=\"foot\">Read-only view of the browser's own archive \u{2014} paths that try to \
         climb out of the folder are refused. <a href=\"",
    );
    out.push_str(CONTROL_PATH);
    out.push_str("\">Control page</a></p>\n");
    out.push_str(PAGE_CLOSE);
    Ok(out)
}

/// The state the control page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageState {
    /// The spec in force (folder, bind host, requested port).
    pub spec: Spec,
    /// `http://host:port` while serving (`None` = stopped).
    pub serving: Option<String>,
    /// The control page's own stable address (`None` = render without the
    /// mutation form, which is how the served origin is answered).
    pub control: Option<String>,
    /// The session secret the control page's form carries and the control
    /// socket checks. Empty = this render may not authorize anything.
    pub token: String,
}

/// `true` when a note reads as a failure (it gets the red bar).
fn looks_like_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("cannot")
        || lower.contains("failed")
        || lower.contains("refused")
        || lower.contains("must be")
        || lower.contains("in use")
        || lower.contains("not a ")
}

/// The control page: status, the folder/host/port form, stop, and the
/// documentation of what is served and how it got there. `note` is the decoded
/// message of a redirect (`?note=…`) and is escaped.
pub fn render_control_page(state: &PageState, note: Option<&str>) -> String {
    let serving = match &state.serving {
        Some(addr) => format!(
            "<strong>Serving</strong> <code>{}</code> \u{2014} <a href=\"{addr}/\">open the archive</a>",
            escape_html(addr)
        ),
        None => "<strong>Stopped</strong> \u{2014} nothing is being served; apply a folder below to start again.".to_string(),
    };
    let control = match &state.control {
        Some(addr) => format!(
            "<code>{}</code> \u{2014} this page's own address, which never moves even when the served address does",
            escape_html(addr)
        ),
        None => "&mdash;".to_string(),
    };
    let note_html = match note {
        Some(text) if !text.trim().is_empty() => format!(
            "  <div class=\"note{}\">{}</div>\n",
            if looks_like_error(text) { " bad" } else { "" },
            escape_html(text)
        ),
        _ => String::new(),
    };
    let root = state.spec.root.display().to_string();
    // The field shows the port **in use**: a request for "random" is answered
    // with the number the OS picked, so the page never shows an empty box for
    // a server that is demonstrably listening (and submitting it blank keeps
    // that port instead of rolling a new one).
    let port_text = port_of_address(state.serving.as_deref())
        .map(|p| p.to_string())
        .unwrap_or_else(|| {
            if state.spec.port == RANDOM_PORT {
                String::new()
            } else {
                state.spec.port.to_string()
            }
        });
    let default_root = default_archive_root().display().to_string();

    let mut out = page_open("Archive server");
    out.push_str(
        "  <h1>Archive server</h1>\n  <p class=\"sub\">Serves the browser's archive folder over \
         <strong>localhost</strong> so saved visits can be browsed in a tab. Read-only: nothing \
         here writes, and a path that tries to climb out of the folder is refused.</p>\n",
    );
    out.push_str(&note_html);
    out.push_str("  <h2>Status</h2>\n  <table>\n");
    out.push_str(&format!("    <tr><td>Served from</td><td>{serving}</td></tr>\n"));
    if let Some(address) = &state.control {
        out.push_str(&format!("    <tr><td>This page</td><td>{address}</td></tr>\n"));
    }
    out.push_str(&format!(
        "    <tr><td>Folder in force</td><td><code>{}</code></td></tr>\n  </table>\n\n",
        escape_html(&root)
    ));
    // The mutation form belongs to the control page alone, and it carries the
    // session token: without it the control socket refuses the change, so an
    // `<img>`, a link or a redirect from any other page cannot re-point the
    // server. On the served origin this whole block is omitted.
    if !state.token.is_empty() {
    out.push_str(&format!(
        "  <h2>Choose what to serve, and from where</h2>\n  \
         <form method=\"get\" action=\"{CONTROL_PATH}/apply\">\n    \
         {token_field}\n    <table>\n      \
         <tr><td style=\"width:8em\">Folder</td><td><input type=\"text\" name=\"root\" \
         value=\"{root}\" placeholder=\"{default_root}\"></td></tr>\n      \
         <tr><td>Host</td><td><input type=\"text\" name=\"host\" value=\"{host}\" \
         placeholder=\"127.0.0.1\"></td></tr>\n      \
         <tr><td>Port</td><td><input type=\"text\" name=\"port\" value=\"{port}\" \
         placeholder=\"random free port\"></td></tr>\n    </table>\n    \
         <p><button type=\"submit\">Apply</button> &nbsp;\n      <a href=\"{CONTROL_PATH}\">Refresh</a> &nbsp;\n      \
         <button type=\"submit\" formaction=\"{CONTROL_PATH}/stop\">Stop serving</button></p>\n  </form>\n\n",
        root = escape_html(&root),
        default_root = escape_html(&default_root),
        host = escape_html(&state.spec.host),
        port = escape_html(&port_text),
        // AUDIT: the token is the control socket's own credential. A document
        // loaded from the served folder lives on the served origin, so the
        // field is emitted only on the control socket's own page — elsewhere
        // the form is shown but inert (apply/stop answer 403 without it).
        token_field = match &state.control {
            Some(_) => format!(
                "<input type=\"hidden\" name=\"token\" value=\"{}\">",
                escape_html(&state.token)
            ),
            None => String::from(
                "<!-- the folder, host and port are changed from ironsurf://archive -->"
            ),
        },
    ));
    } else {
        out.push_str(
            "  <h2>Choose what to serve, and from where</h2>\n  \
             <p class=\"sub\">Read-only view: the folder, host and port are set from the \
             control page, reached by typing <code>ironsurf://archive</code> in the browser.</p>\n",
        );
    }
    out.push_str(&format!(
        "  <h2>How it works</h2>\n  <ul>\n    \
         <li>Type <code>ironsurf://archive</code> in the URL bar to reach this page from the \
         browser; <a href=\"ironsurf://archive?open=1\"><code>ironsurf://archive?open=1</code></a> \
         opens the archive <em>root</em> in a <strong>new tab</strong> instead.</li>\n    \
         <li>The folder defaults to <code>$HOME/Downloads/ironsurf-tabs</code> \u{2014} exactly what \
         the \u{1F4BE} archive mode writes there, as \
         <code>&lt;folder&gt;/&lt;ISO8601 visit timestamp&gt;/&lt;host[:port]&gt;/index.html</code>, \
         so the top-level listing reads as a timeline of visits &mdash; and it &lt;em&gt;pre-traverses&lt;/em&gt; \
         each timestamp, listing every site saved under it with a direct link, so no visit folder \
         ever has to be opened just to find a saved page.</li>\n    \
         <li>The default address is <code>127.0.0.1</code> on a <strong>random free port</strong>; \
         any port you name is used instead. Only loopback is served \u{2014} the archive never goes \
         out to the network.</li>\n    \
         <li>An accepted choice persists in the browser's UI settings, so the next \
         <code>ironsurf://archive</code> starts with it. Applying a new address re-binds the \
         listener; this control page keeps its own.</li>\n  </ul>\n  \
         <p class=\"foot\">Served by the shell process (<code>ironsurf-shell</code>), which owns \
         the archive folder; the tab only asked for the address.</p>\n"
    ));
    out.push_str(PAGE_CLOSE);
    out
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Answer one request. `method` is upper-case, `path` percent-decoded,
/// `query`/`body` raw. Serving is read-only; the only mutations are the two
/// control endpoints, and those live on the control socket (see
/// [`ServeHandle`]), which is why the serve socket defers them.
pub fn route_request(method: &str, path: &str, query: &str, state: &PageState) -> Response {
    if !matches!(method, "GET" | "HEAD" | "POST") {
        return Response::error(405, "only GET, HEAD and POST are served");
    }
    let no_body = method == "HEAD";
    if path == CONTROL_PATH {
        let form = parse_form(query);
        let note = form_get(&form, "note");
        // AUDIT: a document loaded from the served folder is a document on the
        // served origin, and one same-origin fetch of this page used to reveal
        // the "random" control port. The status is fine to show; the control
        // address and the mutation form are not.
        let mut view = state.clone();
        view.control = None;
        return Response::html(200, render_control_page(&view, note), no_body);
    }
    if path == format!("{CONTROL_PATH}/apply") || path == format!("{CONTROL_PATH}/stop") {
        // AUDIT: these paths used to defer to the control socket with the whole
        // query, so a page on the served origin needed neither a token nor even
        // knowledge of the control port — a top-level navigation follows the
        // 302 for it. Deferral is now for the control page's own form only,
        // which carries the session token; anything else is refused here.
        let form = parse_form(query);
        if state.token.is_empty()
            || form_get(&form, "token") != Some(state.token.as_str())
        {
            return Response::error(
                403,
                "changing the archive server needs the form from the control page \
                 (type ironsurf://archive in the browser)",
            );
        }
        // Changing the server is the control socket's job (its single thread
        // owns the state); point the browser at it.
        // The query rides along: dropping `?root=…&port=…` here would have the
        // control socket re-apply what it is already serving.
        let mut resp = match &state.control {
            Some(addr) => {
                let target = if query.is_empty() {
                    format!("{addr}{path}")
                } else {
                    format!("{addr}{path}?{query}")
                };
                Response::redirect(&target)
            }
            None => Response::error(500, "the control socket is gone"),
        };
        resp.no_body = no_body;
        return resp;
    }
    if no_body && method != "HEAD" {
        return Response::error(405, "only GET and HEAD read files");
    }
    if method == "POST" {
        return Response::error(405, "the archive is read-only over HTTP");
    }
    match resolve_inside(&state.spec.root, path) {
        Ok(target) => serve_file(target, path, no_body),
        Err(status) => Response::error(status, &explain_status(status, path)),
    }
}

fn explain_status(status: u16, path: &str) -> String {
    match status {
        403 => format!("refusing to serve {path}: it is outside the archive folder"),
        _ => format!("{path} is not in the archive folder"),
    }
}

fn serve_file(target: PathBuf, path: &str, no_body: bool) -> Response {
    if target.is_dir() {
        let index = target.join("index.html");
        if index.is_file() {
            return serve_bytes(&index, path, no_body);
        }
        match render_listing(&target, path) {
            Ok(html) => Response::html(200, html, no_body),
            Err(status) => Response::error(status, &explain_status(status, path)),
        }
    } else {
        serve_bytes(&target, path, no_body)
    }
}

fn serve_bytes(file: &Path, path: &str, no_body: bool) -> Response {
    // One file in memory per connection (one connection at a time): archived
    // media can be huge, so a file past the limit is named instead of read.
    if std::fs::metadata(file).map(|m| m.len() > MAX_SERVE_BYTES).unwrap_or(false) {
        return Response::error(
            413,
            &format!(
                "{path} is larger than the {} MiB this server reads in one piece",
                MAX_SERVE_BYTES / (1024 * 1024)
            ),
        );
    }
    match std::fs::read(file) {
        Ok(body) => Response {
            status: 200,
            content_type: content_type_of_with_sniff(file, &body[..body.len().min(512)]),
            body,
            location: None,
            no_body,
        },
        Err(err) => {
            let status = if err.kind() == std::io::ErrorKind::NotFound {
                404
            } else {
                500
            };
            Response::error(status, &format!("cannot read {path}: {err}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Threads and the live handle
// ---------------------------------------------------------------------------

/// What the shell's event loop is told about the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServeEvent {
    /// The folder/host/port changed (the control page, or a request carrying
    /// an override): the shell mirrors it into its settings and persists them.
    Spec { spec: Spec },
    /// The served address changed (`None` = the server stopped serving).
    Address { address: Option<String> },
    /// A bind failed (port busy, host refused, …); whatever was serving
    /// keeps serving and the control page says why nothing changed.
    Failed { error: String },
    /// The serve thread exited (its listener was retired).
    Gone,
    /// The control socket is closing (the shell asked for a shutdown).
    Closed,
}

impl ServeEvent {
    /// The served address this event reports, if it is about the address at
    /// all (`Some(None)` = serving stopped, `None` = not an address event).
    pub fn address(&self) -> Option<Option<String>> {
        match self {
            ServeEvent::Address { address } => Some(address.clone()),
            ServeEvent::Gone => Some(None),
            _ => None,
        }
    }
}

/// Commands accepted on the internal control pipe (never a public API).
#[derive(Debug, PartialEq, Eq)]
pub enum ServeCmd {
    /// Serve this folder/address instead (re-binds when the address moved).
    Set(Spec),
    /// Stop the serve listener, keeping the control page alive.
    Stop,
    /// Stop serving and end both threads.
    Shutdown,
}

/// JSON form of a [`Spec`] for the control pipe: a folder with spaces,
/// brackets or a `?` in its name must survive the trip verbatim.
#[derive(Serialize, Deserialize)]
struct SpecWire {
    root: String,
    host: String,
    port: u16,
}

/// The line form of a command ([`ServeHandle`] writes it to the control
/// socket, the control thread answers it).
pub fn encode_command(cmd: &ServeCmd) -> String {
    match cmd {
        ServeCmd::Stop => "Stop\n".to_string(),
        ServeCmd::Shutdown => "Shutdown\n".to_string(),
        ServeCmd::Set(spec) => format!(
            "Set {}\n",
            serde_json::to_string(&SpecWire {
                root: spec.root.display().to_string(),
                host: spec.host.clone(),
                port: spec.port,
            })
            .unwrap_or_default()
        ),
    }
}

/// Parse a control-pipe line back into a command (`None` = garbage).
pub fn parse_command(line: &str) -> Option<ServeCmd> {
    let line = line.trim();
    if line == "Stop" {
        return Some(ServeCmd::Stop);
    }
    if line == "Shutdown" {
        return Some(ServeCmd::Shutdown);
    }
    let wire: SpecWire = serde_json::from_str(line.strip_prefix("Set ")?.trim()).ok()?;
    Some(ServeCmd::Set(Spec {
        root: PathBuf::from(wire.root),
        host: wire.host,
        port: wire.port,
    }))
}

/// Mutable server state, guarded by one mutex. Only the control thread mutates
/// it; the serve thread reads it per request and checks its **generation** to
/// notice that its listener was retired.
#[derive(Default)]
struct Inner {
    spec: Spec,
    /// A secret for the life of the process, handed to the control page's form
    /// and required on every mutation. Nothing else distinguishes the user's
    /// own form from a request another site arranged.
    token: String,
    /// `http://host:port` while a serve thread is listening (`None` = stopped).
    address: Option<String>,
    /// Bumped on every re-bind/stop; a serve thread whose generation is older
    /// retires itself and drops its listener, so exactly one port is live.
    generation: u64,
    /// `true` once the shell asked for a shutdown (the control loop ends).
    shutting_down: bool,
}

/// The pieces the control thread hands out to the serve threads.
struct Hub {
    inner: Mutex<Inner>,
    /// The control page's own address (constant for the life of the server).
    control: String,
    /// The session token (mirrors [`Inner::token`], read without the lock).
    token: String,
    /// The self-pipe the serve threads `poll()` beside their listener, so a
    /// re-bind/stop wakes a blocked `accept()` immediately: `wake()` opens a
    /// client, writes one byte, and closes it — the read end reports the
    /// connection (and its EOF) without the serve thread ever reading it.
    wake: SocketAddr,
}

impl Hub {
    /// The page state a request is answered from.
    fn page(&self) -> PageState {
        let inner = self.inner.lock().expect("archive server state poisoned");
        PageState {
            spec: inner.spec.clone(),
            serving: inner.address.clone(),
            control: Some(self.control.clone()),
            token: self.token.clone(),
        }
    }

    /// The token a mutation must present.
    fn token(&self) -> &str {
        &self.token
    }

    fn generation(&self) -> u64 {
        self.inner.lock().expect("archive server state poisoned").generation
    }

    /// Nudge every blocked `accept()` so it re-reads the generation.
    fn wake(&self) {
        if let Ok(mut stream) = TcpStream::connect(self.wake) {
            let _ = stream.write_all(b"w");
        }
    }
}

/// A live archive server: the control socket (one address, for the life of the
/// shell session) plus the serve listener the control thread manages.
///
/// Dropping the handle (or [`ServeHandle::shutdown`]) stops both threads;
/// [`ServeHandle::stop`] only stops serving and leaves the control page up.
pub struct ServeHandle {
    /// The control page's address (`http://127.0.0.1:<random>`).
    pub control_url: String,
    /// The control socket's address (tests, and the shell's status line).
    pub control_addr: SocketAddr,
    /// The served address right after `start` (`None` = the bind failed).
    pub address: Option<String>,
}

impl ServeHandle {
    /// Bring the server up: bind the control socket and the served address,
    /// start the control thread and the serve thread. A failed serve bind is
    /// **not** fatal — the control page comes up anyway so the address can be
    /// fixed from the browser, and the reason rides out as
    /// [`ServeEvent::Failed`].
    pub fn start(spec: Spec, events: Sender<ServeEvent>) -> Result<ServeHandle, String> {
        let control_listener = TcpListener::bind((CONTROL_HOST, 0)).map_err(|e| {
            format!("cannot bind the archive control socket on {CONTROL_HOST}: {e}")
        })?;
        let control_addr = control_listener
            .local_addr()
            .map_err(|e| format!("control socket address: {e}"))?;
        let control_url = address_of(CONTROL_HOST, control_addr.port());
        let wake_listener = TcpListener::bind((CONTROL_HOST, 0))
            .map_err(|e| format!("cannot bind the archive wake socket: {e}"))?;
        let wake_addr = wake_listener
            .local_addr()
            .map_err(|e| format!("wake socket address: {e}"))?;
        let wake_tx = TcpStream::connect(wake_addr)
            .map_err(|e| format!("cannot open the archive wake pipe: {e}"))?;

        // A server that cannot bind is not a server: fail the call with the
        // reason (folder? address busy?) so the asker can say so, rather than
        // handing back a handle that serves nothing.
        let serve = match bind_serve(&spec) {
            Ok(pair) => Some(pair),
            Err(err) => {
                send_event(events.clone(), ServeEvent::Failed { error: err.clone() });
                return Err(err);
            }
        };
        let address = serve.as_ref().map(|pair| pair.1.clone());
        // Deliberately *no* connection is opened to the control socket here:
        // the control thread answers connections in turn, and a connection
        // parked in the backlog before it starts would sit in front of the
        // page's first request. Every call below connects fresh and closes.
        let hub = Arc::new(Hub {
            inner: Mutex::new(Inner {
                spec: spec.clone(),
                address: address.clone(),
                generation: 1,
                shutting_down: false,
                token: session_token(),
            }),
            control: control_url.clone(),
            token: session_token(),
            wake: wake_addr,
        });
        drop(wake_tx);
        if let Some(addr) = address.clone() {
            send_event(events.clone(), ServeEvent::Address { address: Some(addr) });
        }

        let ctl_hub = hub.clone();
        let ctl_events = events.clone();
        thread::Builder::new()
            .name("ironsurf-archive-ctl".to_string())
            .spawn(move || {
                control_loop(control_listener, wake_listener, hub, events);
            })
            .map_err(|e| format!("cannot start the archive control thread: {e}"))?;

        if let Some((listener, addr)) = serve {
            spawn_serve_thread(ctl_hub.clone(), listener, addr, 1, ctl_events.clone());
        }

        Ok(ServeHandle {
            control_url,
            control_addr,
            address,
        })
    }

    /// Is the control socket answering? (A dropped handle or a dead thread
    /// shows up as a refused connection.)
    pub fn alive(&self) -> bool {
        self.ping().is_some()
    }

    /// One control-socket round trip, returning the first bytes of the reply.
    pub fn ping(&self) -> Option<Vec<u8>> {
        let mut stream = TcpStream::connect(self.control_addr).ok()?;
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        stream
            .write_all(b"GET /ironsurf/ping HTTP/1.1\r\n\r\n")
            .ok()?;
        let mut reply = vec![0u8; 64];
        let n = stream.read(&mut reply).unwrap_or(0);
        if n == 0 {
            return None;
        }
        reply.truncate(n);
        Some(reply)
    }

    fn command(&self, cmd: ServeCmd) -> std::io::Result<()> {
        let line = encode_command(&cmd);
        let mut stream = TcpStream::connect(self.control_addr)?;
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
        // The command thread answers on its own time; the reply is the event
        // stream, not this socket.
        Ok(())
    }

    /// Serve this folder/address instead (re-binds when the address moved).
    pub fn set_spec(&self, spec: Spec) -> std::io::Result<()> {
        self.command(ServeCmd::Set(spec))
    }

    /// Stop serving; the control page stays up and can start again.
    pub fn stop(&self) -> std::io::Result<()> {
        self.command(ServeCmd::Stop)
    }

    /// Stop serving and shut both threads down (the shell is exiting).
    pub fn shutdown(&self) {
        let _ = self.command(ServeCmd::Shutdown);
    }
}

impl Drop for ServeHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind the served address; port `0` = the kernel picks a free one (the
/// "random port" default).
fn bind_serve(spec: &Spec) -> Result<(TcpListener, String), String> {
    let addrs = (spec.host.as_str(), spec.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", spec.host))?;
    let addr = addrs
        .into_iter()
        .next()
        .ok_or_else(|| format!("no address for {}", spec.host))?;
    let listener = TcpListener::bind(addr)
        .map_err(|e| format!("cannot bind {}:{}: {e}", spec.host, spec.port))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("bound address: {e}"))?;
    Ok((listener, address_of(spec.host.as_str(), bound.port())))
}

/// Never blocks the sender: a stalled event loop drops the event (the shell's
/// own copy of the spec is then one event behind and the next request
/// re-syncs it).
fn send_event(events: Sender<ServeEvent>, event: ServeEvent) {
    // The shell's queue is unbounded: a server thread must never block behind
    // an event loop that is busy painting.
    let _ = events.send(event);
}

/// `poll()` one fd for readability (`-1` = none, never ready).
pub fn fd_readable(fd: i32, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if fd < 0 {
        // Nothing to watch: sleep out the timeout so the loop stays idle.
        let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        unsafe { libc::poll(std::ptr::null_mut(), 0, millis) };
        return false;
    }
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
    ready > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
}

/// `poll()` two fds (the listener and the self-pipe) for readability.
fn any_readable(first: i32, second: i32, timeout: Duration) -> bool {
    let mut pfds = [
        libc::pollfd { fd: first, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: second, events: libc::POLLIN, revents: 0 },
    ];
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let ready = unsafe { libc::poll(pfds.as_mut_ptr(), 2, millis) };
    if ready <= 0 {
        return false;
    }
    pfds.iter()
        .filter(|p| p.fd >= 0)
        .any(|p| (p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0)
}

/// Serve one listener until its generation is retired (see [`serve_loop`]).
fn spawn_serve_thread(
    hub: Arc<Hub>,
    listener: TcpListener,
    address: String,
    generation: u64,
    events: Sender<ServeEvent>,
) {
    let for_thread = events.clone();
    let spawn = thread::Builder::new()
        .name("ironsurf-archive-serve".to_string())
        .spawn(move || serve_loop(hub, listener, generation, for_thread));
    if let Err(err) = spawn {
        error!("archive server: cannot start the serve thread for {address}: {err}");
        send_event(
            events,
            ServeEvent::Failed { error: format!("cannot start the serve thread: {err}") },
        );
    }
}

/// The serve thread's loop (see [`spawn_serve_thread`]).
fn serve_loop(
    hub: Arc<Hub>,
    listener: TcpListener,
    generation: u64,
    events: Sender<ServeEvent>,
) {
    let listener_fd = listener.as_raw_fd();
    // Our end of the self-pipe: never read (the wake-up *is* the connection
    // and the subsequent EOF), never closed until the thread exits.
    let wake_end = TcpStream::connect(hub.wake).ok();
    let wake_fd = wake_end.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
    loop {
        if hub.generation() != generation {
            break;
        }
        if !any_readable(listener_fd, wake_fd, Duration::from_secs(1)) {
            continue;
        }
        if wake_fd >= 0 && fd_readable(wake_fd, Duration::ZERO) {
            // The nudge woke us: re-check the generation, keep watching.
            continue;
        }
        match listener.accept() {
            Ok((stream, _peer)) => {
                let page = hub.page();
                handle_connection(stream, &page);
            }
            Err(ref err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => break, // the listener was closed by the control thread
        }
    }
    drop(listener);
    drop(wake_end);
    // If this was the current generation, nothing is serving any more.
    let mut inner = hub.inner.lock().expect("archive server state poisoned");
    if inner.generation == generation {
        inner.address = None;
    }
    drop(inner);
    send_event(events, ServeEvent::Gone);
}

/// Read one request from a connection and answer it. A short, broken or
/// over-long request is simply dropped (no panic, no partial reply).
fn handle_connection(mut stream: TcpStream, page: &PageState) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let Some((method, path, query)) = read_request_head(&mut stream) else { return };
    let response = route_request(&method, &path, &query, page);
    let bytes = response.to_http();
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

/// Is this mutation one the user meant? It must carry the session token the
/// control page embedded — which nothing else can know, and which an `<img>`,
/// a link or a redirect from another site cannot add — and no browser header
/// may claim it came from somewhere else (`Origin`/`Referer` are checked when
/// present, since a cross-site top-level navigation does send them).
fn mutation_authorized(form: &[(String, String)], hub: &Arc<Hub>, head: &str) -> Result<(), String> {
    let token = hub.token();
    if token.is_empty() || form_get(form, "token") != Some(token) {
        return Err(
            "changing the archive server needs the form from the control page; a request \
             without its session token is refused"
                .to_string(),
        );
    }
    for name in ["origin", "referer"] {
        if let Some(value) = header_value(head, name) {
            let authority = hub.control.rsplit_once("://").map(|(_, rest)| rest).unwrap_or("");
            let sent = value
                .trim()
                .rsplit_once("://")
                .map(|(_, rest)| rest.trim_end_matches('/'))
                .unwrap_or(value.trim().trim_end_matches('/'));
            if !sent.is_empty() && sent != authority {
                return Err(format!(
                    "a request from {sent} may not change the archive server"
                ));
            }
        }
    }
    Ok(())
}

/// One header value from a request head, case-insensitively.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_owned())
    })
}

/// The form body of a `POST` to the control endpoints: whatever already
/// arrived after the header block, plus the rest of the declared
/// `Content-Length` (capped at [`MAX_BODY_BYTES`]; an undeclared length means
/// a chunked or endless client, which the control page has no use for).
fn read_form_body(stream: &mut TcpStream, head: &str) -> String {
    let (declared, already) = match head.find("\r\n\r\n") {
        Some(at) => {
            let declared = head[..at]
                .lines()
                .rev()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name.trim().eq_ignore_ascii_case("content-length"))
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            (declared.min(MAX_BODY_BYTES), head[at + 4..].to_string())
        }
        None => (0, String::new()),
    };
    let mut body = already.chars().take(declared).collect::<String>();
    while body.len() < declared {
        let mut chunk = [0u8; 4096];
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        body.push_str(&String::from_utf8_lossy(&chunk[..n.min(declared - body.len())]));
    }
    body
}

/// Read the request head (bounded) and parse it. `None` = nothing servable.
fn read_request_head(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n")
            || buf.windows(2).any(|w| w == b"\n\n")
            || buf.len() > MAX_HEADER_BYTES
        {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    parse_request_head(&head)
}

/// The control thread: answers the control page, its two endpoints, the
/// internal command pipe and a `ping`. One connection at a time, and the
/// thread lives as long as the shell asked for the server.
fn control_loop(
    listener: TcpListener,
    wake_listener: TcpListener,
    hub: Arc<Hub>,
    events: Sender<ServeEvent>,
) {
    // The self-pipe's listener stays open for the thread's life (the serve
    // threads connect to it); nothing is ever accepted from it.
    let _wake = wake_listener;
    while !shutdown_requested(&hub) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let outcome = handle_control_connection(stream, &hub, &events);
                if outcome == ControlOutcome::Shutdown {
                    break;
                }
            }
            Err(ref err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(err) => {
                debug!("archive control socket stopped: {err}");
                break;
            }
        }
    }
    send_event(events, ServeEvent::Closed);
}

/// A per-process secret for the control form: 16 bytes from the OS RNG, or a
/// mix of clock, pid and a randomly-seeded hasher where no RNG device is open.
fn session_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok()
    {
        return bytes.iter().map(|b| format!("{b:02x}")).collect();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    hasher.write_u64(std::process::id() as u64);
    format!("{:016x}{:016x}", hasher.finish(), nanos as u64)
}

/// Has the shell asked for a shutdown (via the control pipe)?
fn shutdown_requested(hub: &Arc<Hub>) -> bool {
    hub.inner
        .lock()
        .expect("archive server state poisoned")
        .shutting_down
}

#[derive(PartialEq, Eq)]
enum ControlOutcome {
    Handled,
    Shutdown,
}

/// Answer one control-socket exchange.
fn handle_control_connection(
    mut stream: TcpStream,
    hub: &Arc<Hub>,
    events: &Sender<ServeEvent>,
) -> ControlOutcome {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return ControlOutcome::Handled,
    };
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    let first_line = text
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_string();

    // Internal command pipe: anything but an HTTP request line.
    if !first_line.starts_with("GET ") && !first_line.starts_with("HEAD ") && !first_line.starts_with("POST ") {
        return match parse_command(&first_line) {
            Some(cmd) => run_command(cmd, hub, events),
            None => {
                debug!("archive server: ignoring unknown control line {first_line:?}");
                ControlOutcome::Handled
            }
        };
    }

    let Some((method, path, query)) = parse_request_head(&text) else {
        let response = Response::error(400, "not a request this socket serves");
        let _ = stream.write_all(&response.to_http());
        let _ = stream.flush();
        return ControlOutcome::Handled;
    };

    if path == "/ironsurf/ping" {
        let response = Response {
            status: 200,
            content_type: "text/plain; charset=utf-8",
            body: b"ironsurf archive control\n".to_vec(),
            location: None,
            no_body: false,
        };
        let _ = stream.write_all(&response.to_http());
        let _ = stream.flush();
        return ControlOutcome::Handled;
    }

    if path == CONTROL_PATH {
        let note = form_get(&parse_form(&query), "note").map(str::to_owned);
        let page = hub.page();
        let response = Response::html(200, render_control_page(&page, note.as_deref()), method == "HEAD");
        let _ = stream.write_all(&response.to_http());
        let _ = stream.flush();
        return ControlOutcome::Handled;
    }

    if path == format!("{CONTROL_PATH}/apply") {
        // The page submits its form as a GET; a POST (curl, or another page)
        // is answered too, over the body it declared — capped, so neither a
        // huge form nor a stalled writer can hold the control thread.
        let form = if method == "POST" {
            parse_form(&read_form_body(&mut stream, &text))
        } else {
            parse_form(&query)
        };
        if let Err(why) = mutation_authorized(&form, hub, &text) {
            let response = Response::error(403, &why);
            let _ = stream.write_all(&response.to_http());
            let _ = stream.flush();
            return ControlOutcome::Handled;
        }
        let note = match apply_form(&form, hub, events) {
            Ok(spec) => format!(
                "Now serving {} from {}",
                address_of(&spec.host, spec.port),
                spec.root.display()
            ),
            Err(err) => err,
        };
        let response = Response::redirect(&format!(
            "{CONTROL_PATH}?note={}",
            percent_encode_note(&note)
        ));
        let _ = stream.write_all(&response.to_http());
        let _ = stream.flush();
        return ControlOutcome::Handled;
    }

    if path == format!("{CONTROL_PATH}/stop") {
        let form = if method == "POST" {
            parse_form(&read_form_body(&mut stream, &text))
        } else {
            parse_form(&query)
        };
        if let Err(why) = mutation_authorized(&form, hub, &text) {
            let response = Response::error(403, &why);
            let _ = stream.write_all(&response.to_http());
            let _ = stream.flush();
            return ControlOutcome::Handled;
        }
        stop_serving(hub, events);
        let response = Response::redirect(&format!(
            "{CONTROL_PATH}?note={}",
            percent_encode_note(
                "Stopped serving \u{2014} the control page stays up; apply a folder to start again."
            )
        ));
        let _ = stream.write_all(&response.to_http());
        let _ = stream.flush();
        return ControlOutcome::Handled;
    }

    let response = Response::error(
        404,
        &format!("{path} is not part of the control page — the archive itself is served on the served address"),
    );
    let _ = stream.write_all(&response.to_http());
    let _ = stream.flush();
    ControlOutcome::Handled
}

/// Run one internal command (the control thread is the only mutator).
fn run_command(cmd: ServeCmd, hub: &Arc<Hub>, events: &Sender<ServeEvent>) -> ControlOutcome {
    match cmd {
        ServeCmd::Set(spec) => {
            if let Err(err) = apply_spec(spec, hub, events) {
                send_event(events.clone(), ServeEvent::Failed { error: err });
            }
            ControlOutcome::Handled
        }
        ServeCmd::Stop => {
            stop_serving(hub, events);
            ControlOutcome::Handled
        }
        ServeCmd::Shutdown => {
            stop_serving(hub, events);
            hub.inner
                .lock()
                .expect("archive server state poisoned")
                .shutting_down = true;
            ControlOutcome::Shutdown
        }
    }
}

/// Re-bind with `spec`: the new listener is bound **before** the old one is
/// retired, so a busy port leaves the running server untouched (and the error
/// is what the control page shows).
/// Apply a new spec. The `Spec` event that goes back to the shell reports the
/// address **actually bound** — a request for "port 0" (random) is answered
/// with the port the OS picked, so what gets persisted is what is live.
fn apply_spec(spec: Spec, hub: &Arc<Hub>, events: &Sender<ServeEvent>) -> Result<Spec, String> {
    // **The same address, a different folder** is the common apply (pointing
    // the server at another archive run), and it needs no new socket: the
    // serving thread reads the folder per request. Rebinding the port we are
    // still holding would fail with `EADDRINUSE` — the retired listener is
    // released on its own thread — so re-pointing is also the only way this
    // could ever have worked.
    let (current, serving) = {
        let inner = hub.inner.lock().expect("archive server state poisoned");
        (inner.spec.clone(), inner.address.clone())
    };
    let serving_port = port_of_address(serving.as_deref());
    let wanted_port = if spec.port == RANDOM_PORT { serving_port } else { Some(spec.port) };
    if serving.is_some()
        && spec.host == current.host
        && wanted_port.is_some()
        && wanted_port == serving_port
    {
        let spec = Spec { port: wanted_port.unwrap_or(spec.port), ..spec };
        hub.inner.lock().expect("archive server state poisoned").spec = spec.clone();
        hub.wake();
        send_event(events.clone(), ServeEvent::Spec { spec: spec.clone() });
        return Ok(spec);
    }

    let (listener, address) = bind_serve(&spec)?;
    let spec = Spec {
        port: address
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(spec.port),
        ..spec
    };
    {
        let mut inner = hub.inner.lock().expect("archive server state poisoned");
        inner.spec = spec.clone();
        inner.generation += 1;
        let generation = inner.generation;
        inner.address = Some(address.clone());
        drop(inner);
        hub.wake();
        send_event(events.clone(), ServeEvent::Spec { spec: spec.clone() });
        send_event(events.clone(), ServeEvent::Address { address: Some(address) });
        spawn_serve_thread(
            hub.clone(),
            listener,
            address_of(&spec.host, spec.port),
            generation,
            events.clone(),
        );
    }
    Ok(spec)
}


/// Retire the serve listener; the control page stays up.
fn stop_serving(hub: &Arc<Hub>, events: &Sender<ServeEvent>) {
    {
        let mut inner = hub.inner.lock().expect("archive server state poisoned");
        inner.generation += 1;
        inner.address = None;
    }
    hub.wake();
    send_event(events.clone(), ServeEvent::Address { address: None });
}

/// Normalize the control form into a spec (blank fields keep the value in
/// force) and apply it. The `Err` text is what the control page shows.
fn apply_form(
    form: &[(String, String)],
    hub: &Arc<Hub>,
    events: &Sender<ServeEvent>,
) -> Result<Spec, String> {
    let mut spec = hub.page().spec;
    if let Some(text) = form_get(form, "root") {
        if !text.trim().is_empty() {
            spec.root = normalize_root(text)?;
        }
    }
    if let Some(text) = form_get(form, "host") {
        if !text.trim().is_empty() {
            spec.host = normalize_host(text)?;
        }
    }
    // A typed port wins; a blank or absent field means "leave the port alone"
    // — including the one the OS picked when the server started on a random
    // port, so pressing Apply never moves the address the user was given.
    match form_get(form, "port") {
        Some(text) if !text.trim().is_empty() => spec.port = parse_port(text)?,
        _ => {
            if let Some(live) = port_of_address(hub.page().serving.as_deref()) {
                spec.port = live;
            }
        }
    }
    // What comes back is what got bound (a request for a random port is
    // answered with the port chosen), so the note says the truth.
    apply_spec(spec, hub, events)
}

/// The port inside an `http://host:port` address.
fn port_of_address(address: Option<&str>) -> Option<u16> {
    address?.rsplit(':').next().and_then(|p| p.parse().ok())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ironsurf-serve-{}-{}-{tag}",
            std::process::id(),
            {
                static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            }
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox");
        dir
    }

    fn page(root: PathBuf) -> PageState {
        PageState {
            spec: Spec {
                root,
                host: "127.0.0.1".into(),
                port: 8123,
            },
            serving: Some("http://127.0.0.1:8123".into()),
            control: Some("http://127.0.0.1:9001".into()),
            token: "test-token".into(),
        }
    }

    /// Archived pages reach the folder with every naming scheme, so the media
    /// type is sniffed from the bytes when the name says nothing
    /// (`…/example.com` from a query-less document, a `?q=…` file name).
    #[test]
    fn an_extensionless_archived_page_is_sniffed_by_what_is_inside() {
        let root = sandbox("sniff");
        std::fs::write(
            root.join("example.com"),
            b"<!DOCTYPE html><html><title>archived</title></html>",
        )
        .unwrap();
        std::fs::write(root.join("feed?q=1"), b"<?xml version=\"1.0\"?><rss/>").unwrap();
        std::fs::write(root.join("blob"), [0u8, 1, 2, 3, 255, 254]).unwrap();
        std::fs::write(root.join("plain"), b"  \n a page that is not markup").unwrap();

        let state = page(root);
        let html = route_request("GET", "/example.com", "", &state);
        assert_eq!(html.content_type, "text/html; charset=utf-8");
        let xml = route_request("GET", "/feed%3Fq=1", "", &state);
        assert_eq!(xml.content_type, "text/xml; charset=utf-8");
        let binary = route_request("GET", "/blob", "", &state);
        assert_eq!(binary.content_type, "application/octet-stream");
        let text = route_request("GET", "/plain", "", &state);
        assert_eq!(text.content_type, "application/octet-stream", "no sniffing a guess");
    }

    /// A file past the read-in-one-piece limit is named, not swallowed: the
    /// listing stays usable instead of the shell trading memory for a video.
    #[test]
    fn a_file_past_the_memory_limit_is_named_not_read() {
        let root = sandbox("huge");
        let big = root.join("clip.mp4");
        let file = std::fs::File::create(&big).unwrap();
        // Sparse: the size is what matters, not the bytes on disk.
        file.set_len(MAX_SERVE_BYTES + 1).unwrap();

        let state = page(root);
        let resp = route_request("GET", "/clip.mp4", "", &state);
        assert_eq!(resp.status, 413);
        let body = String::from_utf8_lossy(&resp.body).into_owned();
        assert!(body.contains("clip.mp4"), "{body}");
        assert!(body.contains("512"), "the limit is stated: {body}");

        // One byte under, the same file is served.
        let small = state.spec.root.join("small.mp4");
        let small_file = std::fs::File::create(&small).unwrap();
        small_file.set_len(MAX_SERVE_BYTES - 1024).unwrap();
        // (Served, not refused: a 200 with the header block proves it got read.)
        let resp = route_request("GET", "/small.mp4", "", &state);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "video/mp4");
    }

    #[test]
    fn the_root_lists_the_archive_and_links_to_the_control_page() {
        let root = sandbox("root");
        std::fs::create_dir_all(root.join("visit")).unwrap();
        let resp = route_request("GET", "/", "", &page(root.clone()));
        assert_eq!(resp.status, 200);
        let html = String::from_utf8_lossy(&resp.body).into_owned();
        assert!(html.contains("Archive folder"), "{html}");
        assert!(html.contains("visit/"), "{html}");
        assert!(html.contains(CONTROL_PATH), "the listing links to the control page");
        // HEAD on the listing ships the headers, no body.
        let head = route_request("HEAD", "/", "", &page(root));
        assert_eq!(head.status, 200);
        assert!(head.no_body);
    }

    /// The front page is the archive's timeline: it pre-traverses each visit
    /// folder so its saved sites are linked directly under the timestamp,
    /// without the user opening one timestamp after another.
    #[test]
    fn the_front_page_lists_the_sites_under_every_visit() {
        let root = sandbox("timeline");
        let (early, late) = ("2026-01-01T09:00:00+00:00", "2026-01-02T10:00:00+00:00");
        std::fs::create_dir_all(root.join(format!("{early}/example.com"))).unwrap();
        std::fs::create_dir_all(root.join(format!("{late}/dev.example"))).unwrap();
        std::fs::write(
            root.join(format!("{early}/example.com/index.html")),
            b"<html>archived</html>",
        )
        .unwrap();
        std::fs::write(root.join(format!("{late}/loose.txt")), b"loose").unwrap();
        std::fs::create_dir_all(root.join("2026-01-03T11:00:00+00:00")).unwrap();

        let resp = route_request("GET", "/", "", &page(root));
        assert_eq!(resp.status, 200);
        let html = String::from_utf8_lossy(&resp.body).into_owned();
        // Each site is linked straight through its visit folder — the href is
        // what serves the archived `index.html` without a click per timestamp.
        assert!(
            html.contains("href=\"2026-01-01T09%3A00%3A00%2B00%3A00/example.com/\""),
            "{html}"
        );
        assert!(html.contains(">example.com/</a>"), "displayed with a trailing slash");
        assert!(
            html.contains("href=\"2026-01-02T10%3A00%3A00%2B00%3A00/dev.example/\""),
            "{html}"
        );
        assert!(html.contains("href=\"2026-01-02T10%3A00%3A00%2B00%3A00/loose.txt\""), "stray files too");
        // The visits stay a timeline: earlier visit (and its sites) first.
        assert!(
            html.find("2026-01-01T09%3A00%3A00%2B00%3A00").unwrap()
                < html.find("2026-01-02T10%3A00%3A00%2B00%3A00").unwrap()
        );
        assert!(
            html.find("2026-01-02T10%3A00%3A00%2B00%3A00").unwrap()
                < html.find("2026-01-03T11%3A00%3A00%2B00%3A00").unwrap()
        );
        assert!(html.contains("no sites saved here"), "an empty visit says so");
    }

    /// Pre-traversal reads the tree from the shell side, so it goes one level
    /// per real folder and never through a symlink — a link standing where a
    /// visit folder is expected cannot put an outside tree on the front page.
    #[test]
    fn the_front_page_traversal_never_follows_a_symlink() {
        // The name lives *inside* the linked folder: it could reach the page
        // only by the front page traversing the link.
        let outer = sandbox("timeline-outside");
        std::fs::create_dir_all(outer.join("secret-site")).unwrap();
        let root = sandbox("timeline-link");
        std::fs::create_dir_all(root.join("2026-01-01T09:00:00+00:00/example.com")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outer, root.join("2026-01-04T12:00:00+00:00")).unwrap();

        let html = String::from_utf8_lossy(&route_request("GET", "/", "", &page(root)).body)
            .into_owned();
        assert!(html.contains("example.com/"), "the real visit is expanded");
        assert!(!html.contains("secret-site"), "the linked tree is never traversed: {html}");
    }

    #[test]
    fn the_control_page_states_the_address_and_holds_the_form() {
        let html = render_control_page(&page(sandbox("ctl")), None);
        assert!(html.contains("Archive server"));
        assert!(html.contains("http://127.0.0.1:8123"));
        assert!(html.contains("random free port"));
        assert!(html.contains("ironsurf://archive?open=1"));
        assert!(html.contains("$HOME/Downloads/ironsurf-tabs"));
        for field in ["name=\"root\"", "name=\"host\"", "name=\"port\""] {
            assert!(html.contains(field), "{field} missing");
        }
        assert!(html.contains("Stop serving"));
    }

    #[test]
    fn user_text_never_becomes_markup() {
        let hostile = "</title><script>alert(1)</script>";
        let state = PageState {
            spec: Spec {
                root: PathBuf::from(hostile),
                ..Spec::default()
            },
            ..page(sandbox("xss"))
        };
        let html = render_control_page(&state, Some(hostile));
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn head_answers_with_headers_and_no_body() {
        let head = route_request("HEAD", CONTROL_PATH, "", &page(sandbox("head")));
        assert_eq!(head.status, 200);
        assert!(head.no_body);
        assert!(!head.body.is_empty(), "Content-Length still describes a GET");
        let on_wire = head.to_http();
        let text = String::from_utf8_lossy(&on_wire).into_owned();
        let (headers, body) = text.split_once("\r\n\r\n").expect("head/body split");
        assert!(body.is_empty(), "HEAD must not ship a body");
        assert!(headers.contains("Content-Length:"));
    }

    /// Mutating the server is the control socket's job (its single thread owns
    /// the state), so the serve socket defers &mdash; and the deferral has to
    /// carry the form's query, or the control socket would re-apply what it is
    /// already serving and the user's change would vanish mid-redirect.
    #[test]
    fn apply_and_stop_defer_to_the_control_socket_with_their_query() {
        let state = page(sandbox("defer"));
        let applied = route_request(
            "GET",
            &format!("{CONTROL_PATH}/apply"),
            "root=/tmp&token=test-token",
            &state,
        );
        assert_eq!(applied.status, 302);
        assert_eq!(
            applied.location.as_deref(),
            Some("http://127.0.0.1:9001/ironsurf/serve/apply?root=/tmp&token=test-token"),
            "the form survives the hop"
        );

        let stopped = route_request(
            "GET",
            &format!("{CONTROL_PATH}/stop"),
            "token=test-token",
            &state,
        );
        assert_eq!(stopped.status, 302);
        assert_eq!(
            stopped.location.as_deref(),
            Some("http://127.0.0.1:9001/ironsurf/serve/stop?token=test-token")
        );

        // AUDIT: without the form's token the served origin gets no hop to the
        // control socket at all — a page loaded from the archive cannot even
        // start a change it has no business making.
        for path in ["apply", "stop"] {
            let hostile = route_request("GET", &format!("{CONTROL_PATH}/{path}"), "root=%2F", &state);
            assert_eq!(
                hostile.status, 403,
                "an untokened {path} must be refused on the served origin"
            );
        }

        // A file request is answered here, never deferred.
        assert_ne!(
            route_request("GET", "/index.html", "", &state).status,
            302,
            "files are served, not redirected"
        );
    }

    #[test]
    fn files_are_served_with_a_guessed_type() {
        let root = sandbox("files");
        std::fs::create_dir_all(root.join("2026-01-01T00:00:00+00:00/example.com")).unwrap();
        std::fs::write(
            root.join("2026-01-01T00:00:00+00:00/example.com/index.html"),
            b"<html>archived</html>",
        )
        .unwrap();
        std::fs::write(root.join("style.css"), b"body{color:red}").unwrap();

        let state = page(root.clone());
        let resp = route_request("GET", "/style.css", "", &state);
        assert_eq!(resp.status, 200, "root {root:?} body {:?}", String::from_utf8_lossy(&resp.body));
        assert_eq!(resp.content_type, "text/css; charset=utf-8");
        assert_eq!(resp.body, b"body{color:red}");

        // A directory whose `index.html` exists is that page, not a listing.
        let resp = route_request("GET", "/2026-01-01T00:00:00+00:00/example.com/", "", &state);
        assert_eq!(resp.status, 200, "root {} exists {} body {}", root.display(), root.join("2026-01-01T00:00:00+00:00/example.com/index.html").exists(), String::from_utf8_lossy(&resp.body));
        assert_eq!(resp.body, b"<html>archived</html>");
    }

    #[test]
    fn a_directory_without_an_index_is_listed() {
        let root = sandbox("listing");
        std::fs::create_dir_all(root.join("visit-b")).unwrap();
        std::fs::create_dir_all(root.join("visit-a")).unwrap();
        std::fs::write(root.join("note.txt"), b"hi").unwrap();
        let resp = route_request("GET", "/", "", &page(root.clone()));
        assert_eq!(resp.status, 200);
        let html = String::from_utf8_lossy(&resp.body).into_owned();
        assert!(html.contains("Archive folder"), "{html}");
        // Directories first, alphabetical, with a trailing slash; files sized.
        let dirs = html.find("visit-a/").expect("visit-a");
        assert!(dirs < html.find("visit-b/").expect("visit-b"));
        assert!(html.find("visit-b/").unwrap() < html.find("note.txt").unwrap(), "files after dirs");
        assert!(html.contains("2 B"), "the size column");
    }

    #[test]
    fn traversal_is_refused_however_it_is_written() {
        // A nested folder: `..` has somewhere harmless to land, so a leak can
        // only mean the resolution actually left the archive folder.
        let outer = sandbox("traverse-outer");
        let root = outer.join("archive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(outer.join("secret.txt"), b"secret contents").unwrap();
        let state = page(root);
        for path in [
            "/../secret.txt",
            "/..%2fsecret.txt",
            "/%2e%2e/secret.txt",
            "/..%252fsecret.txt",
            "/..%5csecret.txt",
            "/%2e%2e%2e%2e/secret.txt",
            "/nested/../../secret.txt",
            "/%2e%2e/%2e%2e/etc/passwd",
        ] {
            let resp = route_request("GET", path, "", &state);
            // Either nothing is found (the traversal was neutralized inside
            // the folder) or it is refused outright — never the file itself.
            assert!(
                matches!(resp.status, 403 | 404),
                "{path} → {} {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            );
            assert!(!resp.body.windows(7).any(|w| w == b"secret"), "{path} leaked the file");
        }
    }

    #[test]
    fn a_symlink_out_of_the_folder_is_refused() {
        // Canonicalization is part of the resolution, so a link that points
        // above the root cannot be used as a door.
        let root = sandbox("symlink-root");
        let outside = sandbox("symlink-outside");
        std::fs::write(outside.join("secret.txt"), b"top secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
        let resp = route_request("GET", "/link.txt", "", &page(root));
        assert!(matches!(resp.status, 403 | 404), "status {}", resp.status);
        assert!(!resp.body.windows(10).any(|w| w == b"top secret"));
    }

    #[test]
    fn missing_files_and_strange_methods_are_pages_not_errors() {
        let state = page(sandbox("miss"));
        let resp = route_request("GET", "/nope.html", "", &state);
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("not in the archive"));
        assert_eq!(route_request("PUT", "/x", "", &state).status, 405);
        assert_eq!(route_request("POST", "/x", "", &state).status, 405);
    }

    #[test]
    fn request_lines_are_parsed_and_junk_is_not() {
        let (method, path, query) = parse_request_head("GET /a%20b/c?x=1 HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("request");
        assert_eq!((method.as_str(), path.as_str(), query.as_str()), ("GET", "/a b/c", "x=1"));
        assert!(parse_request_head("CONNECT evil:443 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_request_head("GET absolute.example/ HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_request_head("Not a request\r\n\r\n").is_none());
    }

    #[test]
    fn hosts_and_ports_are_normalized() {
        assert_eq!(normalize_host("").unwrap(), "127.0.0.1");
        assert_eq!(normalize_host("localhost").unwrap(), "127.0.0.1");
        assert_eq!(normalize_host(" Localhost ").unwrap(), "127.0.0.1");
        // AUDIT: the served folder is unauthenticated, so only loopback may bind.
        assert!(normalize_host("0.0.0.0").is_err());
        assert!(normalize_host("10.0.0.7").is_err());
        assert!(normalize_host("corp.example.com").is_err());
        assert_eq!(normalize_host("127.0.0.53").unwrap(), "127.0.0.53");
        assert!(normalize_host("http://evil.example/").is_err());
        assert!(normalize_host("127.0.0.1:8080").is_err());
        assert!(normalize_host("..").is_err());

        assert_eq!(parse_port("").unwrap(), RANDOM_PORT);
        assert_eq!(parse_port("0").unwrap(), RANDOM_PORT);
        assert_eq!(parse_port("random").unwrap(), RANDOM_PORT);
        assert_eq!(parse_port("8088").unwrap(), 8088);
        assert!(parse_port("99999").is_err());
        assert!(parse_port("port").is_err());
    }

    #[test]
    fn folders_are_normalized_created_and_kept_absolute() {
        let dir = sandbox("normalize");
        let as_text = dir.display().to_string();
        assert_eq!(normalize_root(&format!("{as_text}/")).unwrap(), dir.canonicalize().unwrap());
        assert!(normalize_root("relative/folder").is_err());
        assert!(normalize_root("   ").is_err());
        // A folder that does not exist yet is created, not rejected.
        let fresh = dir.join("fresh/nested");
        assert_eq!(
            normalize_root(&fresh.display().to_string()).unwrap(),
            fresh.canonicalize().unwrap()
        );
    }

    #[test]
    fn form_parsing_keeps_the_last_value_and_decodes() {
        let form = parse_form("root=%2Ftmp%2Fmy%20archive&host=&port=8088&port=9090");
        assert_eq!(form_get(&form, "root"), Some("/tmp/my archive"));
        assert_eq!(form_get(&form, "host"), Some(""));
        assert_eq!(form_get(&form, "port"), Some("9090"));
        assert_eq!(form_get(&form, "absent"), None);
    }

    #[test]
    fn commands_survive_their_pipe_round_trip() {
        let root = sandbox("cmd");
        let spec = Spec { root: root.join("weird ?[name]"), host: "127.0.0.1".into(), port: 7777 };
        let line = encode_command(&ServeCmd::Set(spec.clone()));
        assert_eq!(parse_command(&line), Some(ServeCmd::Set(spec)));
        assert_eq!(parse_command("Stop"), Some(ServeCmd::Stop));
        assert_eq!(parse_command("Shutdown"), Some(ServeCmd::Shutdown));
        assert_eq!(parse_command("nonsense"), None);
    }

    // -- live server ---------------------------------------------------------

    /// One request over a fresh connection, returning the whole reply.
    /// The session token the control page embeds in its form, read the way a
    /// browser reads it: fetch the control page, take the hidden field.
    fn control_token(addr: SocketAddr) -> String {
        let reply = talk(addr, "GET /ironsurf/serve HTTP/1.1\r\n\r\n");
        reply
            .split("name=\"token\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default()
            .to_string()
    }

    fn talk(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).expect("write");
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A retired address stops answering once its thread notices (it polls on
    /// a one-second heartbeat), so "refuses connections" is an eventual fact.
    fn wait_until_unreachable(addr: SocketAddr) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect(addr).is_err() {
                return;
            }
            assert!(Instant::now() < deadline, "{addr} was never retired");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    struct Live {
        handle: ServeHandle,
        events: mpsc::Receiver<ServeEvent>,
        #[allow(dead_code)]
        root: PathBuf,
    }

    fn live(spec: Spec) -> Live {
        let (tx, rx) = mpsc::channel();
        let handle = ServeHandle::start(spec.clone(), tx).expect("start");
        Live { handle, events: rx, root: spec.root.clone() }
    }

    /// The next folder/host/port change, ignoring the retirement chatter a
    /// re-bind leaves behind.
    fn next_spec(rx: &mpsc::Receiver<ServeEvent>) -> Spec {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ServeEvent::Spec { spec }) => return spec,
                Ok(ServeEvent::Gone) | Ok(ServeEvent::Address { .. }) => {}
                Ok(other) => panic!("unexpected event {other:?}"),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("events closed"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            assert!(Instant::now() < deadline, "no Spec event");
        }
    }

    /// The first `Address` event (`None` = nothing being served).
    fn next_address(rx: &mpsc::Receiver<ServeEvent>) -> Option<String> {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("an address event")
            .address()
            .flatten()
    }

    /// Wait until the serve socket settles on `want` (`None` = stopped
    /// serving). A replaced listener reports its own retirement as `Gone`, and
    /// the address that goes with it can arrive in any order, so this looks for
    /// the address the caller asked for rather than "whatever came next".
    fn address_after(rx: &mpsc::Receiver<ServeEvent>, want: Option<u16>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ServeEvent::Address { address: Some(address) }) => {
                    let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
                    if Some(port) == want {
                        return;
                    }
                }
                Ok(ServeEvent::Address { address: None }) if want.is_none() => return,
                Ok(ServeEvent::Gone) => {}
                Ok(other) => panic!("unexpected event {other:?}"),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("events closed"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            assert!(Instant::now() < deadline, "never settled on {want:?}");
        }
    }

    #[test]
    fn a_live_server_browses_the_folder_and_never_escalates() {
        let root = sandbox("live");
        std::fs::create_dir_all(root.join("visit/127.0.0.1:1234")).unwrap();
        std::fs::write(
            root.join("visit/127.0.0.1:1234/index.html"),
            b"<html><title>archived visit</title></html>",
        )
        .unwrap();
        std::fs::write(root.join("visit/notes.md"), b"# notes").unwrap();

        let server = live(Spec { root: root.clone(), ..Spec::default() });
        let address = next_address(&server.events).expect("an address");
        assert!(address.starts_with("http://127.0.0.1:"), "{address}");
        // The default port is random, never a fixed well-known one.
        assert!(!address.ends_with(":80") && !address.ends_with(":8080"));
        let serve_port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
        let serve_addr = SocketAddr::from(([127, 0, 0, 1], serve_port));

        // An archived file, with a guessed type.
        let reply = talk(serve_addr, "GET /visit/notes.md HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.contains("text/plain"), "{reply}");
        assert!(reply.ends_with("# notes"), "{reply}");

        // A directory holding an `index.html` is that page, not a listing.
        let reply = talk(serve_addr, "GET /visit/127.0.0.1:1234/ HTTP/1.1\r\n\r\n");
        assert!(reply.contains("archived visit"), "{reply}");

        // The root lists the archive and links to the control page, which
        // documents the server and names its own address.
        let reply = talk(serve_addr, "GET / HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.contains("visit/"), "the listing chains down: {reply}");
        let reply = talk(serve_addr, "GET /ironsurf/serve HTTP/1.1\r\n\r\n");
        assert!(reply.contains("Archive server"), "{reply}");
        assert!(reply.contains(&address), "the page names its own address: {reply}");
        assert!(reply.contains("random free port"), "{reply}");

        // Traversal over the wire: a page, never the file above the folder.
        let reply = talk(serve_addr, "GET /%2e%2e%2e%2e/etc/passwd HTTP/1.1\r\n\r\n");
        assert!(
            reply.starts_with("HTTP/1.1 403") || reply.starts_with("HTTP/1.1 404"),
            "{reply}"
        );
        assert!(!reply.contains("root:"));

        // HEAD ships the headers and no body.
        let reply = talk(serve_addr, "HEAD /visit/notes.md HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        let (headers, body) = reply.split_once("\r\n\r\n").expect("head/body split");
        assert!(headers.contains("Content-Length:"));
        assert!(body.is_empty(), "HEAD must not ship a body");

        server.handle.shutdown();
    }

    #[test]
    fn the_control_socket_applies_a_new_folder_and_address() {
        let first = sandbox("ctl-first");
        let second = sandbox("ctl-second");
        std::fs::write(second.join("marker.txt"), b"in the second folder").unwrap();
        let target = sandbox("ctl-target");
        std::fs::write(target.join("marker.txt"), b"in the target folder").unwrap();

        let server = live(Spec { root: first, ..Spec::default() });
        let address = next_address(&server.events).expect("first address");
        let serve_port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
        let control_addr = server.handle.control_addr;

        // What the control page's form submits, pointed at another folder and
        // an explicit port.
        let query = format!(
            "root={}&host=127.0.0.1&port={}",
            percent_encode_note(&target.display().to_string()),
            serve_port + 1
        );
        let token = control_token(control_addr);
        assert!(!token.is_empty(), "the control page carries a session token");
        let reply = talk(
            control_addr,
            &format!("GET /ironsurf/serve/apply?{query}&token={token} HTTP/1.1\r\n\r\n"),
        );
        assert!(reply.starts_with("HTTP/1.1 302"), "REPLY<{reply}>");
        assert!(reply.contains("Now%20serving"), "the note reports success: REPLY<{reply}>");

        // The Spec event carries the change to the shell, which persists it.
        let saw_spec = next_spec(&server.events);
        assert_eq!(saw_spec.root, target.canonicalize().unwrap());
        assert_eq!(saw_spec.port, serve_port + 1);
        address_after(&server.events, Some(serve_port + 1));

        // The new folder is what is served now.
        let reply = talk(
            SocketAddr::from(([127, 0, 0, 1], serve_port + 1)),
            "GET /marker.txt HTTP/1.1\r\n\r\n",
        );
        assert!(reply.contains("in the target folder"), "{reply}");

        // The retired address stops answering.
        wait_until_unreachable(SocketAddr::from(([127, 0, 0, 1], serve_port)));

        // Stop serving: the served address goes away, the control page lives.
        server.handle.stop().expect("stop");
        address_after(&server.events, None);
        assert!(server.handle.alive(), "the control page survives a stop");
        let reply = talk(control_addr, "GET /ironsurf/serve HTTP/1.1\r\n\r\n");
        assert!(reply.contains("Stopped"), "{reply}");

        // Applying a folder after a stop starts serving again. The address is
        // fresh, because a retired listener can hold its port for a moment.
        let restart_port = serve_port + 7;
        let reply = talk(
            control_addr,
            &format!(
                "GET /ironsurf/serve/apply?root={}&port={restart_port}&token={token} HTTP/1.1\r\n\r\n",
                percent_encode_note(&second.display().to_string())
            ),
        );
        assert!(reply.starts_with("HTTP/1.1 302"), "REPLY<{reply}>");
        let again = next_spec(&server.events);
        assert_eq!(again.root, second.canonicalize().unwrap());
        assert_eq!(again.port, restart_port);
        let port: u16 = restart_port;
        address_after(&server.events, Some(port));
        let reply = talk(
            SocketAddr::from(([127, 0, 0, 1], port)),
            "GET /marker.txt HTTP/1.1\r\n\r\n",
        );
        assert!(reply.contains("in the second folder"), "{reply}");

        server.handle.shutdown();
    }

    /// Pressing Apply with the port field left blank (what the page shows when
    /// the server started on a random port) must not roll a new port: the
    /// address the user has been given stays working.
    #[test]
    fn a_blank_port_keeps_the_address_the_user_was_given() {
        let from = sandbox("keep-from");
        let to = sandbox("keep-to");
        std::fs::write(to.join("moved.txt"), b"new folder, same address").unwrap();
        let server = live(Spec { root: from, ..Spec::default() });
        let address = next_address(&server.events).expect("address");
        let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();

        let token = control_token(server.handle.control_addr);
        let reply = talk(
            server.handle.control_addr,
            &format!(
                "GET {CONTROL_PATH}/apply?root={}&token={token} HTTP/1.1\r\n\r\n",
                percent_encode_note(&to.display().to_string())
            ),
        );
        assert!(reply.starts_with("HTTP/1.1 302"), "{reply}");
        let spec = next_spec(&server.events);
        assert_eq!(spec.root, to, "the folder moved");
        assert_eq!(spec.port, port, "the port stayed put");
        let served = talk(
            SocketAddr::from(([127, 0, 0, 1], port)),
            "GET /moved.txt HTTP/1.1\r\n\r\n",
        );
        assert!(served.contains("new folder, same address"), "{served}");
        server.handle.shutdown();
    }

    #[test]
    fn the_control_page_form_can_also_be_posted() {
        // The page submits a GET; a POST with a declared body is answered the
        // same way (curl, or a page that prefers POST).
        let get_folder = sandbox("post-get");
        let post_folder = sandbox("post-target");
        std::fs::write(post_folder.join("posted.txt"), b"posted body works").unwrap();
        let server = live(Spec { root: get_folder, ..Spec::default() });
        let address = next_address(&server.events).expect("address");
        let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
        let control = server.handle.control_addr;

        // A fresh port: a retired listener can still hold the old one for a
        // moment, and the form's port field is free to say so.
        let new_port = port + 3;
        let token = control_token(control);
        let body = format!(
            "root={}&port={new_port}&token={token}",
            percent_encode_note(&post_folder.display().to_string())
        );
        let request = format!(
            "POST {CONTROL_PATH}/apply HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        // AUDIT: a POST is only a form, not an identity. Without the session
        // token the control socket must refuse, or any page could arrange one.
        let hostile_body = format!("root={}", percent_encode_note(&post_folder.display().to_string()));
        let hostile = format!(
            "POST {CONTROL_PATH}/apply HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: {}\r\n\r\n{hostile_body}",
            hostile_body.len()
        );
        let hostile_reply = talk(control, &hostile);
        assert!(
            hostile_reply.starts_with("HTTP/1.1 403"),
            "an untokened POST must not move the server: {hostile_reply}"
        );
        let reply = talk(control, &request);
        assert!(reply.starts_with("HTTP/1.1 302"), "REPLY<{reply}>");
        let spec = next_spec(&server.events);
        assert_eq!(spec.port, new_port, "the posted body carried the port");
        address_after(&server.events, Some(new_port));
        let served = talk(
            SocketAddr::from(([127, 0, 0, 1], new_port)),
            "GET /posted.txt HTTP/1.1\r\n\r\n",
        );
        assert!(served.contains("posted body works"), "{served}");
        server.handle.shutdown();
    }

    /// Asking for a second server on an address that is taken fails with the
    /// reason — a handle that would serve nothing is never handed out.
    #[test]
    fn a_server_that_cannot_bind_is_refused_with_a_reason() {
        let root = sandbox("busy-start");
        let taken = live(Spec { root: root.clone(), ..Spec::default() });
        let address = next_address(&taken.events).expect("address");
        let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();

        let (tx, _rx) = mpsc::channel();
        let started = ServeHandle::start(Spec { root: root.clone(), port, ..Spec::default() }, tx);
        let err = match started {
            Ok(handle) => {
                handle.shutdown();
                panic!("a server on a taken port must not start")
            }
            Err(err) => err,
        };
        assert!(err.contains("bind") || err.contains("address"), "{err}");
        assert!(err.contains(&port.to_string()), "the reason names the port: {err}");
        // The first server is untouched.
        assert_eq!(
            talk(SocketAddr::from(([127, 0, 0, 1], port)), "GET / HTTP/1.1\r\n\r\n")
                .lines()
                .next()
                .unwrap_or_default(),
            "HTTP/1.1 200 OK"
        );
        taken.handle.shutdown();
    }

    #[test]
    fn a_busy_address_is_reported_and_the_running_server_survives() {
        let root = sandbox("busy");
        let server = live(Spec { root: root.clone(), ..Spec::default() });
        let address = next_address(&server.events).expect("address");
        let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
        // Squat on another port so the re-bind has something to collide with.
        let squatter = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = squatter.local_addr().unwrap().port();

        let token = control_token(server.handle.control_addr);
        let reply = talk(
            server.handle.control_addr,
            &format!(
                "GET /ironsurf/serve/apply?root={}&port={taken}&token={token} HTTP/1.1\r\n\r\n",
                percent_encode_note(&root.display().to_string())
            ),
        );
        assert!(reply.starts_with("HTTP/1.1 302"), "REPLY<{reply}>");
        let note = reply
            .split("note=")
            .nth(1)
            .and_then(|rest| rest.split_once("\r\n").map(|(n, _)| n.to_string()))
            .expect("a note on the redirect");
        let note = percent_decode(&note);
        assert!(note.contains("cannot bind"), "{note}");

        // The running server is untouched.
        let still = talk(
            SocketAddr::from(([127, 0, 0, 1], port)),
            "GET /ironsurf/serve HTTP/1.1\r\n\r\n",
        );
        assert!(still.contains("Archive server"), "{still}");
        assert!(server.handle.alive());

        server.handle.shutdown();
    }

    #[test]
    fn shutdown_reports_closed_and_releases_the_port() {
        let root = sandbox("shutdown");
        let server = live(Spec { root, ..Spec::default() });
        let address = next_address(&server.events).expect("address");
        let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
        server.handle.shutdown();
        let closed = {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut closed = false;
            while Instant::now() < deadline {
                match server.events.recv_timeout(Duration::from_millis(100)) {
                    Ok(ServeEvent::Closed) => {
                        closed = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
            closed
        };
        assert!(closed, "the control thread reports Closed");
        wait_until_unreachable(SocketAddr::from(([127, 0, 0, 1], port)));
    }

    // ---------------------------------------------------------------------
    // AUDIT proofs — each asserts the CURRENT (insecure) behaviour, so a
    // passing test is the finding. See docs/SECURITY_AUDIT.md.
    // ---------------------------------------------------------------------

    /// AUDIT F4: the served folder's own origin can *start* the server change.
    /// `/ironsurf/serve/apply` on the serve socket is answered with a 302 to
    /// the control socket that keeps the whole query, so a document loaded
    /// from the served folder needs no token and never has to know the
    /// control port: a top-level navigation follows the redirect for it.
    #[test]
    fn an_archive_page_can_start_the_server_change_without_a_token() {
        let state = PageState {
            spec: Spec::default(),
            serving: Some("http://127.0.0.1:8123".into()),
            control: Some("http://127.0.0.1:9001".into()),
            token: String::new(),
        };
        let resp = route_request(
            "GET",
            "/ironsurf/serve/apply",
            "root=%2F&host=0.0.0.0&port=4321",
            &state,
        );
        // The serve socket must refuse the mutation outright, not hand it on.
        assert_ne!(
            resp.status, 302,
            "the served origin forwarded a server change to the control socket \
             at {:?} — a page on the archive origin needs no token",
            resp.location
        );
    }

    /// AUDIT F5: the control page served *on the serve socket* prints the
    /// control socket's address, and the listing's own footer links to it
    /// (`<a href="/ironsurf/serve">`). A document already on the served
    /// origin therefore reads the "random" control port with one same-origin
    /// fetch, and can then re-point the server at any folder.
    #[test]
    fn the_served_origin_discloses_the_control_address() {
        let root = sandbox("disclose");
        let state = PageState {
            spec: Spec { root: root.clone(), host: "127.0.0.1".into(), port: 8123 },
            serving: Some("http://127.0.0.1:8123".into()),
            control: Some("http://127.0.0.1:9001".into()),
            token: "test-token".into(),
        };
        let control_page = route_request("GET", "/ironsurf/serve", "", &state);
        assert_eq!(control_page.status, 200);
        let body = String::from_utf8_lossy(&control_page.body).into_owned();
        assert!(
            !body.contains("http://127.0.0.1:9001"),
            "the control socket's address was disclosed to the served origin \
             (one same-origin fetch reveals the port)"
        );
        // …and the listing points the browser at that page.
        let listing = render_listing(&root, "/").expect("listing");
        assert!(listing.contains("Control page"), "the listing links to it");
    }

    /// AUDIT F6: the request path is percent-decoded twice (once in
    /// `parse_request_head`, once per segment in `resolve_inside`). A name the
    /// listing links to verbatim (`%aé`) therefore reaches
    /// `percent_decode("%a\u{e9}")`, which slices a `String` off a char
    /// boundary and panics — killing the serve thread, so the archive stops
    /// serving for the rest of the session.
    #[test]
    fn a_twice_decoded_name_kills_the_path_resolver() {
        let root = sandbox("twice");
        std::fs::write(root.join("%a\u{e9}"), b"planted").expect("plant");
        let listing = render_listing(&root, "/").expect("listing");
        // The href the listing emits is exactly what a browser puts on the
        // wire, and it survives url-crate normalisation unchanged.
        let href = listing
            .find("href=\"")
            .map(|at| &listing[at + 6..])
            .and_then(|rest| rest.split('"').next())
            .expect("a link in the listing")
            .to_string();
        assert_eq!(href, "%25a%C3%A9");
        // First decode (`parse_request_head`), then per segment below.
        let once = percent_decode(&href);
        assert_eq!(once, "%a\u{e9}");
        let r = std::panic::catch_unwind(|| resolve_inside(&root, &format!("/{once}")));
        assert!(
            r.is_ok(),
            "the percent-decode inside resolve_inside panicked on a name the \
             listing itself links to"
        );
    }
}
