//! Container-scoped network layer (`ARCHITECTURE.md` §Network layer).
//!
//! Blitz edition (task 7b). Two policies:
//!
//! * [`NetPolicy::Passthrough`] (default): plain `reqwest` (rustls) fetches
//!   served by our own [`BlitzNetProvider`] worker (Blitz has no network stack
//!   of its own once `blitz_net::Provider` is replaced); cookies are **not**
//!   injected for subresources, and isolation comes from the
//!   one-container-per-process model.
//! * [`NetPolicy::Managed`] (`--managed-net`): every http(s) load goes through
//!   our own `reqwest` (rustls) client so we control `Cookie`-header injection
//!   from the container's [`ironsurf_storage::CookieJar`], capture `Set-Cookie`
//!   responses into the jar, answer from [`ironsurf_storage::NetCache`] (with
//!   ETag/`If-None-Match` revalidation — except entries the server declared
//!   explicitly fresh, which are served without touching the wire at all),
//!   and enforce https-only: `http:`
//!   **main-document** navigations are upgraded to `https:`; if the upgrade
//!   fetch fails the load is blocked with an error page. `http:` sub-resources
//!   and `about:`/`file:`/`data:` fixtures keep working unmodified (the
//!   documented "allow http" escape hatch).
//!
//! Main-document navigations always go through [`fetch_document`] in the tab
//! process (never via the provider), so **the container cookie jar always
//! applies to top-level requests, regardless of provider mode**: cookies a
//! site sets on a navigation are captured and the matching ones are sent
//! back on every later navigation and on every redirect hop. What
//! `Passthrough` skips is only the https-upgrade and the [`NetCache`]; what
//! it additionally skips for *subresources* is cookie handling entirely (the
//! documented escape hatch).
//!
//! Everything policy-related is a pure function over plain types so it can be
//! unit-tested without a live network (see `tests/e2e.rs`).
//!
//! NOTE: `blitz_traits::net::NetHandler` has exactly ONE method (`bytes`);
//! status/headers never reach the document, and the trait has no error path —
//! a failed fetch drops the `Box<dyn NetHandler>` (same failure mode as
//! upstream `blitz-net`), see [`deliver`].

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::time::Duration;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use blitz_traits::net::{Bytes, HeaderMap, NetHandler, NetProvider, NetWaker, Request};
use ironsurf_storage::{fresh_until, CachedEntry, CookieJar, NetCache, StoreError};
use url::Url;

// Every request/response decision is logged to stderr at `debug`; only things
// that silently broke a load (transport failures, dropped subresources) reach
// `warn`, so a normal page stays quiet at the default level.
use ironsurf_common::{debug, trace, warn};
use ironsurf_common::NetEvent;

// ---------------------------------------------------------------------------
// DevTools network reporting (`FromTab::NetworkEvent`)

/// Sink for network events: called exactly once per *completed or failed*
/// request (redirect hops count as their own events; a response served
/// entirely from [`NetCache`] reports one event carrying the cached entry's
/// status/size — duration 0 for a direct fresh serve, the measured
/// revalidation time for a 304 round trip). `None` keeps the fetcher quiet —
/// every call site that does not care about the DevTools network panel
/// passes `None`.
pub type NetReport<'a> = Option<&'a (dyn Fn(NetEvent) + Send + Sync)>;

/// A completed request: status + size + measured wall clock, no error.
fn net_ok(method: &str, url: &Url, status: u16, response_bytes: u64, duration_ms: u64) -> NetEvent {
    NetEvent {
        method: method.to_owned(),
        url: url.as_str().to_owned(),
        status: Some(status),
        response_bytes: Some(response_bytes),
        duration_ms: Some(duration_ms),
        error: None,
        // The reporting boundary tags the initiator (document/script/xhr/…):
        // it is the layer that knows *why* the request exists.
        initiator: None,
    }
}

/// A failed request: `status == None`, `error == Some(reason)`.
fn net_failed(method: &str, url: &Url, duration_ms: u64, error: String) -> NetEvent {
    NetEvent {
        method: method.to_owned(),
        url: url.as_str().to_owned(),
        status: None,
        response_bytes: None,
        duration_ms: Some(duration_ms),
        error: Some(error),
        initiator: None,
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// A loggable rendering of `url`: `data:` URLs are reduced to their length so
/// a 2 MB inline asset cannot spray itself across stderr.
pub fn url_for_log(url: &Url) -> String {
    if url.scheme() == "data" {
        format!("data: URL ({} bytes)", url.as_str().len())
    } else {
        url.as_str().to_owned()
    }
}

/// Current wall-clock seconds since the Unix epoch (same clock the storage
/// layer uses for cookie expiry).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Which network policy this tab applies to http(s) loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetPolicy {
    /// Plain reqwest fetches; no cookie injection for subresources.
    Passthrough,
    /// Intercept http(s) loads: container cookies, cache, https-only policy.
    Managed,
}

impl Default for NetPolicy {
    fn default() -> Self {
        NetPolicy::Passthrough
    }
}

/// What the managed policy decides to do with an intercepted request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Fetch unmodified (non-http schemes, and — for the https-only escape
    /// hatch — non-main-frame `http:` loads).
    Pass,
    /// Fetch `Url` ourselves (the request is already `https:`).
    Fetch(Url),
    /// The request is an `http:` main-frame navigation: attempt `Url` (the
    /// `https:` upgrade) and block with an error page if that fails.
    Upgrade(Url),
}

/// True for `http:`/`https:` (the schemes the managed layer understands).
pub fn is_http_scheme(url: &Url) -> bool {
    url.scheme() == "http" || url.scheme() == "https"
}

/// Upgrade an `http:` URL to `https:` (default port folded away).
pub fn upgrade_to_https(url: &Url) -> Option<Url> {
    if url.scheme() != "http" {
        return None;
    }
    let mut upgraded = url.clone();
    upgraded.set_scheme("https").map_err(|_| {}).ok()?;
    // `set_scheme` keeps an explicit `:80`; the canonical https port is 443,
    // so drop only the insecure default (non-default ports are preserved).
    if upgraded.port() == Some(80) {
        let _ = upgraded.set_port(None);
    }
    Some(upgraded)
}

/// Classify one **main-document** navigation. Unlike subresources (where
/// `Passthrough` keeps the documented no-cookie escape hatch), the top-level
/// fetch always rides on [`managed_http_fetch`] — cookies are always sent
/// and captured for navigations. `Passthrough` only skips the https-upgrade
/// (and the cache, via the `cacheable` argument at the call site); `Managed`
/// upgrades `http:` and blocks with an error page when the upgrade fails.
pub fn plan_document(policy: NetPolicy, request_url: &Url) -> Plan {
    if !is_http_scheme(request_url) {
        return Plan::Pass; // local schemes never reach the network layer
    }
    match request_url.scheme() {
        "https" => Plan::Fetch(request_url.clone()),
        _ => match upgrade_to_https(request_url) {
            Some(https) if policy == NetPolicy::Managed => Plan::Upgrade(https),
            // Passthrough (or an unparseable upgrade): fetch the URL as
            // asked for — still with cookies from the container jar.
            _ => Plan::Fetch(request_url.clone()),
        },
    }
}



/// Revalidation outcome for a cached entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revalidate {
    /// Server answered 304 and the validators still match → serve cached.
    ServeCached,
    /// Server answered something else (or validators mismatched) → use the new
    /// body/headers and update the cache.
    UseFresh {
        status: u16,
        headers: Vec<(String, String)>,
    },
}

/// Pure 304/revalidation decision: 304 + (matching `ETag` or matching
/// `Last-Modified`) serves the cached entry; everything else is fresh.
pub fn revalidate(
    cached: &CachedEntry,
    response_status: u16,
    response_etag: Option<&str>,
    response_last_modified: Option<&str>,
) -> Revalidate {
    if response_status == 304 {
        let etag_ok = match (&cached.etag, response_etag) {
            (Some(c), Some(r)) => c == r,
            _ => false,
        };
        let lm_ok = match (&cached.last_modified, response_last_modified) {
            (Some(c), Some(r)) => c == r,
            _ => false,
        };
        if etag_ok || lm_ok {
            return Revalidate::ServeCached;
        }
    }
    Revalidate::UseFresh {
        status: response_status,
        headers: Vec::new(),
    }
}

/// Extract the validators from a header list (case-insensitive names).
pub fn validators(headers: &[(String, String)]) -> (Option<String>, Option<String>) {
    let mut etag = None;
    let mut last_modified = None;
    for (name, value) in headers {
        match name.to_ascii_lowercase().as_str() {
            "etag" => etag = Some(value.clone()),
            "last-modified" => last_modified = Some(value.clone()),
            _ => {}
        }
    }
    (etag, last_modified)
}

/// Response headers that may be forwarded as-is (hop-by-hop headers and
/// body-transform headers — reqwest already decoded them — never).
// Kept for the shell/interception paths; exercised by the unit tests.
#[allow(dead_code)]
pub fn forwarded_response_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    const DROP: &[&str] = &[
        "content-encoding",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "proxy-connection",
        "te",
        "trailer",
        "set-cookie", // captured into the jar, never forwarded
    ];
    headers
        .iter()
        .filter(|(name, _)| !DROP.contains(&name.to_ascii_lowercase().as_str()))
        .cloned()
        .collect()
}

/// Capture every `Set-Cookie` header of a response into the jar.
pub fn capture_set_cookies<'a, I: IntoIterator<Item = &'a str>>(
    jar: &mut CookieJar,
    request_url: &Url,
    set_cookie_headers: I,
    now_unix: i64,
) {
    for value in set_cookie_headers {
        jar.set_from_set_cookie(request_url, value, now_unix);
    }
}

/// Minimal HTML error page used to block failed https upgrades.
pub fn blocked_error_page(request_url: &Url, https_url: &Url, reason: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Blocked: insecure navigation</title></head>
  <body style="font-family: monospace; margin: 2em">
    <h1>⛬ Navigation blocked</h1>
    <p>IronSurf refuses to load <code>{request}</code>: this tab enforces
    HTTPS for navigations and the upgrade to <code>{https}</code> failed.</p>
    <p><small>{reason}</small></p>
  </body>
</html>
"#,
        request = request_url.as_str(),
        https = https_url.as_str(),
        reason = reason,
    )
}

// ---------------------------------------------------------------------------
// Local schemes (`file:` / `data:` / `about:`)

/// User-Agent sent on every outbound request (we own the whole stack now).
/// This is the **default**: a tab launched with `--user-agent`, or fed a
/// `ToTab::SetUserAgent`, sends that value instead (see [`SharedUserAgent`]).
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; IronSurf/0.1)";

/// The tab's **effective User-Agent** in a shared mutable cell: the runtime
/// owns it (the `--user-agent` flag seeds it, `ToTab::SetUserAgent` replaces
/// it), the fetchers read it at request time, and the JS bindings report it
/// through `navigator.userAgent`. One cell per tab — a change takes effect on
/// every request issued after it without touching any request builder.
pub type SharedUserAgent = Arc<Mutex<String>>;

/// Resolve a `ToTab::SetUserAgent` value (and the `--user-agent` flag) to the
/// effective UA: `Some(v)` is used trimmed, a value that is empty after
/// trimming resets to the built-in default, and `None` is the default.
pub fn resolve_user_agent(value: Option<&str>) -> String {
    match value {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                USER_AGENT.to_owned()
            } else {
                trimmed.to_owned()
            }
        }
        None => USER_AGENT.to_owned(),
    }
}

/// A fresh effective-UA cell seeded from a launch override (`None`/blank ⇒
/// the built-in default).
#[must_use]
pub fn new_user_agent(initial: Option<&str>) -> SharedUserAgent {
    Arc::new(Mutex::new(resolve_user_agent(initial)))
}

/// Read the current value of a UA cell (poison-tolerant: the string is the
/// whole payload, a poisoned lock still holds the honest last value).
#[must_use]
pub fn read_user_agent(user_agent: &SharedUserAgent) -> String {
    user_agent
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

// ---------------------------------------------------------------------------
// Do Not Track (task: private browsing)

/// This tab process's **Do Not Track** switch (born off). The tab process
/// IS the container (ARCHITECTURE §Design overview), so — exactly like the
/// process proxy cell — there is one per process and no ambiguity: the
/// `--private` spawn flag seeds it, `ToTab::SetDoNotTrack` replaces it, and
/// every request started after the change (documents, subresources, form
/// POSTs, redirect hops, XHR/`fetch`) carries the answer: while set, a
/// `DNT: 1` header rides along; while clear, no DNT header is sent at all.
static DO_NOT_TRACK: AtomicBool = AtomicBool::new(false);

/// Flip the process-wide DNT switch (the header itself is applied per
/// request at build time, so the next request is already affected).
pub fn set_do_not_track(enabled: bool) {
    DO_NOT_TRACK.store(enabled, Ordering::Relaxed);
}

/// Whether outbound requests must carry `DNT: 1` right now.
#[must_use]
pub fn do_not_track() -> bool {
    DO_NOT_TRACK.load(Ordering::Relaxed)
}

/// Add the `DNT: 1` header to a request being built, when the switch is on
/// (a no-op otherwise — a clear switch sends no DNT header at all).
fn with_dnt(req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
    if do_not_track() {
        req.header("DNT", "1")
    } else {
        req
    }
}

// ---------------------------------------------------------------------------
// Per-container proxy (task: proxies)

/// The proxy every outbound request of this tab rides, in a process-wide
/// cell. The tab process IS the container (ARCHITECTURE §Design overview),
/// so — exactly like [`shared_worker`] — there is one per process and no
/// ambiguity about which tab it belongs to: the `--proxy` spawn flag seeds
/// it, `ToTab::SetProxy` replaces it, and every later request (documents,
/// subresources, form POSTs, redirect hops, XHR/`fetch`, workers, media,
/// downloads) rides it. `None` = direct connections. Writing the cell
/// rebuilds the shared [`FetchWorker`]'s clients — reqwest 0.13 binds a
/// proxy to the *client*, not the request, and a client is an `Arc` clone,
/// so the swap lands on every holder of the shared worker instantly.
pub type SharedProxy = Arc<Mutex<Option<ProxyRoute>>>;

/// One proxied route: reqwest's proxy **plus the URL it was built from**.
/// reqwest 0.13 gives `Proxy` no url getter, so the cell remembers the
/// address itself — failure messages ([`attribute_send_error`]) name the
/// proxy that broke, not just the site whose socket was never opened.
#[derive(Clone, Debug)]
pub struct ProxyRoute {
    /// The trimmed URL the route was resolved from.
    pub url: String,
    /// The reqwest proxy this route swaps the shared clients onto.
    pub proxy: reqwest::Proxy,
}

static PROCESS_PROXY: OnceLock<SharedProxy> = OnceLock::new();

/// This tab process's proxy cell (born empty: direct, the default).
#[must_use]
pub fn process_proxy() -> SharedProxy {
    PROCESS_PROXY
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Resolve a `ToTab::SetProxy` value (and the `--proxy` flag) into reqwest's
/// proxy shape: `None` or a value blank after trimming is `None` (direct);
/// anything else becomes an **all-scheme** proxy (`socks5://`, `socks5h://`,
/// `http://`, `https://` — the set [`ironsurf_common::validate_proxy_url`]
/// guards everywhere else too — an entry a hand-fed `--proxy` never saw).
/// Err is a URL that gate rejects (unknown scheme, missing host, whitespace
/// inside, too long) or one reqwest itself cannot parse: callers log it and
/// stay direct.
pub fn resolve_proxy(value: Option<&str>) -> Result<Option<reqwest::Proxy>, String> {
    resolve_proxy_route(value).map(|route| route.map(|r| r.proxy))
}

/// The sibling of `resolve_proxy` that also keeps the trimmed URL: the
/// route cell needs the address for failure messages, reqwest's `Proxy`
/// keeps no copy of it.
fn resolve_proxy_route(value: Option<&str>) -> Result<Option<ProxyRoute>, String> {
    let Some(raw) = value else { return Ok(None) };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    // The contract's own gate first: reqwest 0.13 parses *any* URL into a
    // Proxy (empty hosts included) and only ignores schemes it does not
    // speak much later — validate_proxy_url is what turns socks4, gopher,
    // `socks5://` or a typo into a loud Err instead of a silently ignored
    // (i.e. direct) route. The trimmed URL is what reqwest then parses.
    let trimmed = ironsurf_common::validate_proxy_url(raw)
        .map_err(|e| format!("invalid proxy {raw:?}: {e}"))?;
    let proxy = reqwest::Proxy::all(&trimmed)
        .map_err(|e| format!("invalid proxy {trimmed:?}: {e}"))?;
    Ok(Some(ProxyRoute { url: trimmed, proxy }))
}

/// Seed or replace the process proxy from a raw value: the cell records it
/// and the shared fetch worker rebuilds its clients, so every fetch started
/// after this call rides the new route. The error text is worth logging (a
/// rejected proxy leaves the previous route in place).
pub fn set_process_proxy(value: Option<&str>) -> Result<(), String> {
    let route = resolve_proxy_route(value)?;
    *process_proxy()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = route.clone();
    shared_worker().set_proxy(route.map(|r| r.proxy));
    Ok(())
}

/// The proxy the next request rides (poison-tolerant; `None` = direct).
pub fn current_proxy() -> Option<reqwest::Proxy> {
    current_proxy_route().map(|r| r.proxy)
}

/// The URL of the proxy this tab routes through (`None` = direct). While
/// this is `Some`, every TCP connection a request opens goes **to the
/// proxy**, which is exactly what [`attribute_send_error`] leans on.
pub fn current_proxy_url() -> Option<String> {
    current_proxy_route().map(|r| r.url)
}

fn current_proxy_route() -> Option<ProxyRoute> {
    process_proxy()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The prefix every proxy-attributed transport failure carries. It marks
/// the message for [`NetError::error_page`] (which renders
/// [`proxy_error_page`] instead of only restating a line) and keeps the
/// status-line text self-explanatory on its own.
pub const PROXY_FAIL_MARK: &str = "proxy failure: ";

/// The whole error chain, lowercased — reqwest buries the OS error several
/// sources deep (`Connection refused`, `timed out`, tunnel refusals), and
/// attribution reads all of it.
fn error_chain_lower(e: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = e.to_string().to_lowercase();
    let mut source = e.source();
    while let Some(err) = source {
        chain.push(' ');
        chain.push_str(&err.to_string().to_lowercase());
        source = err.source();
    }
    chain
}

/// Describe a reqwest send failure as a **proxy failure** when this tab
/// routes through one.
///
/// With a proxy set, the only socket a request opens is *to the proxy*:
/// the origin is resolved there (socks5h), connected there, and CONNECTed
/// through there. So while [`current_proxy_url`] is `Some`, connect
/// failures and timeouts are the proxy's failure — but reqwest cannot say
/// so itself (its message names the site's URL), and a dead proxy would
/// otherwise read exactly like a dead website. Returns `None` when the
/// route is direct or the failure is not one a proxy causes (it happened
/// after a response started, or is a builder-level error). The text names
/// the proxy address and says the site was never contacted.
pub fn attribute_send_error(e: &reqwest::Error) -> Option<String> {
    let proxy = current_proxy_url()?;
    let chain = error_chain_lower(e);
    let what = if e.is_timeout()
        || chain.contains("timed out")
        || chain.contains("timeout")
    {
        "timed out — the proxy did not answer in time. Check that it is \
         still up and reachable."
    } else if !e.is_connect() {
        return None;
    } else if chain.contains("connection refused") {
        "refused the connection — nothing is listening at the proxy's \
         address and port. Check that the proxy process is running."
    } else if chain.contains("proxy") || chain.contains("tunnel") || chain.contains("407") {
        "rejected the request — it is reachable but will not forward it \
         (authentication, TLS, or access rules)."
    } else if chain.contains("failed to lookup")
        || chain.contains("dns")
        || chain.contains("name or service")
    {
        "cannot be reached — its hostname does not resolve. Check the \
         address."
    } else {
        "could not be reached. Check its address and port."
    };
    Some(format!(
        "{PROXY_FAIL_MARK}{proxy} {what} The site itself was never contacted; \
         Settings (☰) → Proxy can switch this container back to Direct."
    ))
}

/// Minimal HTML error page for navigations where the **proxy** is the
/// blamed cause: the container routes through it, it failed, and the site
/// was never reached.
/// Rendered by [`NetError::error_page`] for any transport message carrying
/// [`PROXY_FAIL_MARK`].
pub fn proxy_error_page(request_url: &Url, message: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Proxy failure</title></head>
  <body style="font-family: monospace; margin: 2em">
    <h1>⚠ Proxy failure</h1>
    <p><code>{url}</code> was not loaded: this container routes through a
    proxy, and the proxy failed. The site itself was never contacted.</p>
    <p><small>{detail}</small></p>
    <p><small>Pick Direct or another definition in Settings (☰) → Proxy —
    the choice applies to this container only.</small></p>
  </body>
</html>
"#,
        url = request_url.as_str(),
        detail = message.strip_prefix(PROXY_FAIL_MARK).unwrap_or(message),
    )
}

/// Read a `file:` URL from disk (same path semantics as blitz-net: `url.path()`).
pub fn read_file_url(url: &Url) -> Result<Vec<u8>, String> {
    if url.scheme() != "file" {
        return Err(format!("not a file: URL: {url}"));
    }
    std::fs::read(url.path()).map_err(|e| format!("read {}: {e}", url.as_str()))
}

/// The browser's internal scheme. `ironsurf://…` URLs are **always** answered
/// locally by the tab — they never reach the network, are never upgraded,
/// and never carry cookies. `ironsurf://welcome` is the default start page
/// (`ironsurf_common::WELCOME_URL`).
pub const INTERNAL_SCHEME: &str = "ironsurf";

/// Which built-in page an `ironsurf://…` URL selects. `url` puts the first
/// authority segment of a non-special scheme into `host()` and the rest into
/// `path()`, so `ironsurf://welcome`, `ironsurf:///welcome` and
/// `ironsurf://ironsurf/welcome` all select `welcome`. Lowercased (schemes and
/// hosts are case-insensitive).
pub fn internal_page_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path().trim_matches('/').to_ascii_lowercase();
    if host.is_empty() || host == INTERNAL_SCHEME {
        path
    } else {
        host
    }
}

/// Escape text for HTML PCDATA/attributes (internal pages interpolate the
/// requested URL, which is attacker-influenced text — same rule as the
/// `data:` source view: never parse user text as markup).
pub fn escape_html_text(s: &str) -> String {
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

/// What the shell answered the last `ironsurf://archive` request with (see
/// [`archive_server_status`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveServerStatus {
    /// The server is listening.
    pub ok: bool,
    /// Its `http://host:port` root (empty when `ok` is false).
    pub address: String,
    /// Why it is not listening (`ok == false`).
    pub error: Option<String>,
}

/// The archive server answer this process last heard, shared with the page
/// renderer. `ironsurf://archive` is answered locally like every internal page,
/// but its content depends on the **shell** (tabs cannot bind a socket nor read
/// the archive tree), so the runtime asks over IPC and the reply is remembered
/// here for the page to render. `None` = no answer yet.
pub fn archive_server_status() -> Option<ArchiveServerStatus> {
    ARCHIVE_STATUS
        .get_or_init(Mutex::default)
        .lock()
        .expect("archive status mutex poisoned")
        .clone()
}

/// Record the shell's answer (the runtime calls this when
/// [`ironsurf_common::ToTab::ArchiveServerInfo`] lands).
pub fn set_archive_server_status(status: Option<ArchiveServerStatus>) {
    *ARCHIVE_STATUS
        .get_or_init(Mutex::default)
        .lock()
        .expect("archive status mutex poisoned") = status;
}

static ARCHIVE_STATUS: OnceLock<Mutex<Option<ArchiveServerStatus>>> = OnceLock::new();

/// Built-in bytes for the internal `ironsurf:` scheme: `ironsurf://welcome`
/// (the start page) and `ironsurf://archive` (the archive browsing server's
/// summary); any other page name renders an honest "no such internal page"
/// instead of a fetch error, so typos land on a real page.
pub fn internal_page_bytes(url: &Url) -> Option<Vec<u8>> {
    if url.scheme() != INTERNAL_SCHEME {
        return None;
    }
    let page = internal_page_name(url);
    let shown = escape_html_text(url.as_str());
    let html = if page == "welcome" || page.is_empty() {
        WELCOME_PAGE.replace("{url}", &shown)
    } else if page == ARCHIVE_PAGE {
        ARCHIVE_PAGE_HTML.replace("{status}", &archive_summary_html())
    } else if page == HISTORY_PAGE {
        HISTORY_PAGE_HTML
            .replace("{status}", &history_status_html())
            .replace("{rows}", &history_rows_html())
    } else {
        INTERNAL_NOT_FOUND
            .replace("{page}", &escape_html_text(&page))
            .replace("{url}", &shown)
    };
    Some(html.into_bytes())
}

/// The internal page that drives the shell's **archive browsing server**.
pub const ARCHIVE_PAGE: &str = "archive";

/// The internal page that lists the **shared browsing history** — the visit log
/// the ☰ settings panel used to hold. The rows belong to the shell (the log is
/// at the data root, outside every container's sandbox), so the page renders
/// the shell's last answer to [`ironsurf_common::FromTab::HistoryListRequest`]
/// and asks again on every load; `?clear=1` (its own row) empties the log first.
pub const HISTORY_PAGE: &str = "history";

/// That page as an address: the string the settings drawer's **History** button
/// opens and this module's own re-render returns to after `?clear=1`. Named
/// once, in the IPC contract (`ironsurf_common::HISTORY_URL`), so the shell and
/// the tab can never disagree about what the page is called.
pub use ironsurf_common::HISTORY_URL;

/// Where the shell's archive server keeps its own page (folder, host and port,
/// editable). Duplicated from `ironsurf-shell`'s `archive_server::CONTROL_PATH`
/// on purpose: the tab never links to the shell's code, only to its address.
pub const ARCHIVE_CONTROL_PATH: &str = "/ironsurf/serve";

/// Whether a URL is the `ironsurf://archive` page (any spelling the internal
/// scheme accepts: `ironsurf://archive`, `ironsurf:///archive/`, …).
pub fn is_archive_page(url: &Url) -> bool {
    url.scheme() == INTERNAL_SCHEME && internal_page_name(url) == ARCHIVE_PAGE
}

/// Whether a URL is the `ironsurf://history` page (any spelling the internal
/// scheme accepts: `ironsurf://history`, `ironsurf:///history/`, …).
pub fn is_history_page(url: &Url) -> bool {
    url.scheme() == INTERNAL_SCHEME && internal_page_name(url) == HISTORY_PAGE
}

/// The visit list the shell answered with (see [`history_listing`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryListing {
    /// The newest visits, newest first (the shell caps the rows and reports the
    /// whole log's size in `total`).
    pub entries: Vec<ironsurf_common::HistoryVisit>,
    /// Every visit in the shared log, rows shown or not.
    pub total: usize,
}

/// What the shell answered the last `ironsurf://history` request with. `None`
/// = no answer yet, which the page renders honestly as "asking the browser".
pub fn history_listing() -> Option<HistoryListing> {
    HISTORY_LISTING
        .get_or_init(Mutex::default)
        .lock()
        .expect("history listing mutex poisoned")
        .clone()
}

/// Record the shell's answer (the runtime calls this when
/// [`ironsurf_common::ToTab::HistoryList`] lands) so the page can be rendered.
pub fn set_history_listing(listing: Option<HistoryListing>) {
    *HISTORY_LISTING
        .get_or_init(Mutex::default)
        .lock()
        .expect("history listing mutex poisoned") = listing;
}

static HISTORY_LISTING: OnceLock<Mutex<Option<HistoryListing>>> = OnceLock::new();

/// One line of the history page's status: the whole log is counted, the rows
/// shown are admitted when the list was cut.
fn history_status_html() -> String {
    match history_listing() {
        None => String::from(
            "<p class=\"wait\">Asking the browser for the visit log&hellip; reload this page \n             if nothing appears.</p>",
        ),
        Some(listing) => {
            let n = listing.total;
            let shown = listing.entries.len();
            if n == 0 {
                String::from("<p class=\"wait\">No browsing history yet — visits land here as tabs navigate.</p>")
            } else if shown < n {
                format!(
                    "<p>{n} visits shared by every container &mdash; the newest {shown} listed. \
                     <a href=\"{HISTORY_URL}?clear=1\">Clear history</a> (all {n}, in every container).</p>"
                )
            } else {
                format!(
                    "<p>{n} visit{} shared by every container. \
                     <a href=\"{HISTORY_URL}?clear=1\">Clear history</a>.</p>",
                    if n == 1 { "" } else { "s" }
                )
            }
        }
    }
}

/// The visit list itself, newest first. Every byte of it is shell text — titles
/// and URLs are data, never markup — so all of it is escaped.
fn history_rows_html() -> String {
    let Some(listing) = history_listing() else {
        return String::from("<li class=\"wait\">One moment&hellip;</li>");
    };
    if listing.entries.is_empty() {
        return String::from(
            "<li class=\"none\">Nothing yet. Every page a tab loads is written to one log \
             shared by all containers.</li>",
        );
    }
    listing
        .entries
        .iter()
        .map(|visit| {
            let href = escape_html_text(&visit.url);
            let title = visit
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or(&visit.url);
            format!(
                "<li><a href=\"{href}\">{title}</a> <span class=\"when\">{when}</span>\
                 <br><code>{href}</code></li>",
                title = escape_html_text(title),
                when = format_unix_utc(visit.at),
            )
        })
        .collect::<Vec<_>>()
        .join("\n    ")
}

/// Whole seconds since the Unix epoch as `YYYY-MM-DD HH:MM` in UTC — the tab
/// has no calendar crate and a history list does not need a timezone database
/// (a day-boundary in the local zone is not worth pulling `chrono` into the
/// tab process for).
pub fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hour, minute) = (secs_of_day / 3600, secs_of_day % 3600 / 60);
    // Days-from-civil, inverted (Howard Hinnant's algorithm): the epoch is day 0.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11] (March = 0)
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}")
}

/// The summary block on `ironsurf://archive`, from the shell's last answer.
/// Every piece of shell text is escaped: the address and the error string are
/// data, never markup.
fn archive_summary_html() -> String {
    match archive_server_status() {
        None => String::from(
            "<p class=\"wait\">Asking the browser to start the server&hellip; reload this page \n             if nothing appears.</p>",
        ),
        Some(status) if status.ok => {
            let address = escape_html_text(&status.address);
            format!(
                "<p class=\"ok\">Serving at <a href=\"{address}\"><code>{address}</code></a>.</p>\n             <p><a href=\"{address}/{control}\"><code>{address}/{control}</code></a> &mdash; the server's own \n             page, where the folder, host and port are editable.</p>",
                control = "ironsurf/serve",
            )
        }
        Some(status) => {
            let why = status
                .error
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or("the browser did not say why");
            format!(
                "<p class=\"bad\">The archive server is not running.<br>{} \n                 <a href=\"ironsurf://archive\">Ask again</a>.</p>",
                escape_html_text(why)
            )
        }
    }
}

/// The built-in start page (`ironsurf://welcome`). Self-contained by design:
/// inline styles only, no subresources, no scripts — it must render on a
/// machine with no network, which is exactly when it matters most.
const WELCOME_PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Welcome to IronSurf</title>
<style>
  body { margin: 0; background: #14161a; color: #e6e6e6;
         font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 46em; margin: 0 auto; padding: 3em 1.5em; }
  h1 { color: #8fce8f; margin: 0 0 0.2em 0; }
  .sub { color: #9aa0a6; margin: 0 0 2em 0; }
  h2 { color: #8fce8f; font-size: 1.1em; margin: 2em 0 0.5em 0; }
  code, kbd { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
  ul { padding-left: 1.2em; }
  a { color: #8fce8f; }
  .foot { margin-top: 3em; color: #6b7178; font-size: 0.9em; }
</style>
</head>
<body>
<div class="wrap">
  <h1>IronSurf</h1>
  <p class="sub">Container-scoped private browsing &mdash; Blitz + Boa engine host,
     every tab a sandboxed OS process.</p>

  <h2>Getting around</h2>
  <ul>
    <li><kbd>Ctrl-L</kbd> &mdash; focus the URL bar; type a host or a search</li>
    <li><kbd>Ctrl-T</kbd> &mdash; new tab &middot; <kbd>Ctrl-W</kbd> &mdash; close tab</li>
    <li><kbd>Ctrl-Tab</kbd> &mdash; switch tab &middot; <kbd>Ctrl-R</kbd> &mdash; reload</li>
    <li><kbd>Alt-1</kbd> &hellip; <kbd>Alt-9</kbd> &mdash; jump to the 1st&hellip;9th tab
     (the last tab from the ninth on); the number on each tab is its key right now</li>
    <li>middle-click a link &mdash; open it in a new tab of this container</li>
  </ul>

  <h2>Containers</h2>
  <p>Each tab lives in a container with its own cookie jar, cache and profile
     directory on disk. Tabs in different containers share nothing &mdash; not
     memory, not cookies, not cache. Open the container manager from the tab
     bar to create, color and reassign.</p>

  <h2>User agent</h2>
  <p>Each container can send its own <code>User-Agent</code>. The <kbd>UA</kbd>
     button in the top bar opens a menu for the active tab's container:
     <em>Default (IronSurf)</em> plus the built-in presets (Firefox, Chrome,
     Bingbot, Googlebot, Android Chrome), the current choice highlighted, and a
     custom string field for anything else. The choice applies to every later
     request in that container &mdash; documents, form posts, redirects,
     subresources, media, and <code>navigator.userAgent</code> &mdash; with no
     reload needed, and it persists with the container.</p>

  <h2>Archive server</h2>
  <p>Archive mode (the <kbd>&#128190;</kbd> button) saves visits to
     <code>$HOME/Downloads/ironsurf-tabs</code>. <a
     href="ironsurf://archive"><code>ironsurf://archive</code></a> starts a local
     HTTP server over that folder &mdash; on <code>127.0.0.1</code> and a random
     free port by default, both editable on the server's own page, along with the
     folder &mdash; and answers with its address, so the archive can be browsed as
     ordinary pages (<code>ironsurf://archive?open=1</code> opens the listing in a
     new tab). It only ever reads, and only inside the chosen folder.</p>

  <p class="foot">You are looking at <code>{url}</code> &mdash; a page built into
     the tab process. It is the default start page.</p>
</div>
</body>
</html>
"#;

/// The `ironsurf://archive` page: what the archive browsing server is, where it
/// is answering, and how to point it at another folder. Like every internal
/// page it is self-contained (inline styles, no scripts, no subresources) and
/// it is served by the **shell**, not this process — a sandboxed tab cannot
/// bind a socket or read the archive tree.
const ARCHIVE_PAGE_HTML: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>ironsurf: archive server</title>
<style>
  body { margin: 0; background: #14161a; color: #e6e6e6;
         font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 46em; margin: 0 auto; padding: 3em 1.5em; }
  h1 { color: #8fce8f; margin: 0 0 0.2em 0; }
  h2 { color: #8fce8f; font-size: 1.1em; margin: 2em 0 0.5em 0; }
  .sub { color: #9aa0a6; margin: 0 0 2em 0; }
  a { color: #8fce8f; }
  code { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
  ul { padding-left: 1.2em; }
  .ok { color: #8fce8f; }
  .bad { color: #e57373; }
  .wait { color: #9aa0a6; }
  .foot { margin-top: 3em; color: #6b7178; font-size: 0.9em; }
</style>
</head>
<body>
<div class="wrap">
  <h1>Archive server</h1>
  <p class="sub">A local HTTP server over the folder of archived visits, so the
     archive can be browsed with an ordinary page instead of a file manager.</p>

  <h2>State</h2>
  {status}

  <h2>Folder, host and port</h2>
  <p>The server's own page &mdash; <code>&lt;address&gt;/ironsurf/serve</code>
  &mdash; holds the form: an absolute <em>folder</em> (default
     <code>$HOME/Downloads/ironsurf-tabs</code>), a <em>host</em> (default
     <code>127.0.0.1</code>, so nothing outside this machine can connect) and a
     <em>port</em> (default a random free one). Applying re-binds the server and
     the choice is remembered across restarts; nothing is ever written to the
     archive over HTTP &mdash; the server only reads, and it refuses any path
     that tries to climb out of the folder.</p>

  <h2>Links</h2>
  <ul>
    <li><code>ironsurf://archive</code> &mdash; this page: start the server and
        read its address</li>
    <li><code>ironsurf://archive?open=1</code> &mdash; start it and open the
        archive listing in a <strong>new tab</strong>, leaving this page where
        it is</li>
  </ul>

  <p class="foot">Archived visits are the files archive mode (the &#128190; button)
     writes: one folder per visit, timestamped, holding the page as it was
     fetched. This page is built into the browser; the bytes it describes are
     served by the browser process, not fetched from anywhere.</p>
</div>
</body>
</html>
"#;

/// The `ironsurf://history` page: the shared visit log, newest first. The log
/// belongs to the **shell** (one `<data-root>/history.jsonl` for every
/// container), so this page renders the answer to the request the tab made on
/// the way in and asks again on every load — the same favour
/// `ironsurf://archive` asks of its server. Self-contained like every internal
/// page: inline styles, no scripts, no subresources.
const HISTORY_PAGE_HTML: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>ironsurf: history</title>
<style>
  body { margin: 0; background: #14161a; color: #e6e6e6;
         font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 46em; margin: 0 auto; padding: 3em 1.5em; }
  h1 { color: #8fce8f; margin: 0 0 0.2em 0; }
  h2 { color: #8fce8f; font-size: 1.1em; margin: 2em 0 0.5em 0; }
  .sub { color: #9aa0a6; margin: 0 0 2em 0; }
  a { color: #8fce8f; }
  code { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
  ul { padding-left: 1.2em; }
  li { margin: 0 0 0.9em 0; }
  .when { color: #9aa0a6; }
  li code { font-size: 0.85em; color: #8a9199; background: none;
            padding: 0; word-break: break-all; }
  .wait { color: #9aa0a6; }
  .none { color: #9aa0a6; list-style: none; margin-left: -1.2em; }
  .foot { margin-top: 3em; color: #6b7178; font-size: 0.9em; }
</style>
</head>
<body>
<div class="wrap">
  <h1>History</h1>
  <p class="sub">Every page this browser has visited, newest first &mdash; one log
     shared by all containers, kept at the data root and never inside a
     container&#39;s sandbox.</p>

  <h2>State</h2>
  {status}

  <h2>Visits</h2>
  <ul>
    {rows}
  </ul>

  <p class="foot">The address bar&#39;s completions read the same log. Reloading
     this page reads it again; <code>ironsurf://history?clear=1</code> empties it
     for every container. This page is built into the browser.</p>
</div>
</body>
</html>
"#;

/// Shown for an unknown `ironsurf://<page>` so a typo is a page, not an error.
const INTERNAL_NOT_FOUND: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>ironsurf: no such page</title>
<style>
  body { margin: 0; background: #14161a; color: #e6e6e6;
         font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 40em; margin: 0 auto; padding: 3em 1.5em; }
  h1 { color: #e57373; margin: 0 0 0.2em 0; }
  a { color: #8fce8f; }
  code { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
</style>
</head>
<body>
<div class="wrap">
  <h1>No such internal page</h1>
  <p><code>ironsurf://{page}</code> is not built into the browser.
     The request never left this process &mdash; nothing failed on a network.</p>
  <p>Known page: <a href="ironsurf://welcome"><code>ironsurf://welcome</code></a></p>
  <p><code>{url}</code></p>
</div>
</body>
</html>
"#;

/// Built-in bytes for `about:` URLs we can answer locally (`about:blank`,
/// `about:ironsurf…`); `None` for anything else.
pub fn builtin_page_bytes(url: &Url) -> Option<Vec<u8>> {
    if url.scheme() != "about" {
        return None;
    }
    let what = url.path().to_ascii_lowercase();
    if what == "blank" {
        Some(
            br#"<!doctype html>
<html><head><meta charset="utf-8"><title>about:blank</title></head>
<body></body></html>
"#
            .to_vec(),
        )
    } else if what.starts_with("ironsurf") {
        Some(
            format!(
                r#"<!doctype html>
<html><head><meta charset="utf-8"><title>about:ironsurf</title></head>
<body style="font-family: monospace; margin: 2em">
  <h1>IronSurf</h1>
  <p>Container-scoped private browser &mdash; Blitz + Boa engine host.</p>
  <p><code>{url}</code></p>
</body></html>
"#,
                url = url.as_str()
            )
            .into_bytes(),
        )
    } else {
        None
    }
}

/// A `data:` URL (RFC 2397) parsed into media type + decoded bytes.
///
/// `data:[<mediatype>][;base64],<payload>` where `mediatype` is
/// `type/subtype[;charset=…]`. Callers need the media type, not just the
/// bytes: `data:text/javascript;base64,…` is a *program*, and handing it to the
/// HTML parser (or executing it) would both be wrong — see
/// [`DataUrl::renders_as_source`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataUrl {
    /// Lowercased media type without parameters (`text/javascript`).
    /// Missing/empty type means `text/plain`, and an unknown type is never
    /// guessed from the payload (no sniffing).
    pub mime: String,
    /// `charset` parameter, lowercased (`utf-8`, `iso-8859-1`, …).
    pub charset: Option<String>,
    /// Whether the payload used the `;base64` transfer encoding.
    pub base64: bool,
    /// Decoded payload bytes.
    pub bytes: Vec<u8>,
}

impl DataUrl {
    /// Media types we hand to the HTML parser (`image/svg+xml` is included
    /// because Blitz rasterises/inline-renders it like a markup document).
    pub fn is_html(&self) -> bool {
        matches!(
            self.mime.as_str(),
            "text/html"
                | "application/xhtml+xml"
                | "image/svg+xml"
                | "text/xml"
                | "application/xml"
        )
    }

    /// JavaScript media types (same set the `<script type>` matcher accepts,
    /// see `js::is_classic_script_type`), plus the `module` suffix type.
    pub fn is_javascript(&self) -> bool {
        matches!(
            self.mime.as_str(),
            "text/javascript"
                | "application/javascript"
                | "application/x-javascript"
                | "text/ecmascript"
                | "application/ecmascript"
                | "text-javascript"
        )
    }

    /// True for documents we display as **inert source text** instead of
    /// parsing: scripts, stylesheets, JSON, and any `text/*` that is not
    /// markup. Binary types (`image/png`, `application/octet-stream`, …) keep
    /// the previous behaviour — the subresource path is where they belong.
    pub fn renders_as_source(&self) -> bool {
        if self.is_html() {
            return false;
        }
        self.is_javascript()
            || self.mime.starts_with("text/")
            || (self.mime.starts_with("application/")
                && matches!(
                    self.mime.as_str(),
                    "application/json"
                        | "application/manifest+json"
                        | "application/ld+json"
                        | "application/x-web-app-manifest+json"
                        | "application/css+json"
                ))
    }

    /// Payload as text. We honour `charset` only as far as recognising UTF-8:
    /// every other label decodes lossily, like our other document reads.
    pub fn text(&self) -> String {
        match self.charset.as_deref() {
            // fast path — no copy for the overwhelmingly common case
            Some("utf-8") | None => match std::str::from_utf8(&self.bytes) {
                Ok(s) => s.to_owned(),
                Err(_) => String::from_utf8_lossy(&self.bytes).into_owned(),
            },
            _ => String::from_utf8_lossy(&self.bytes).into_owned(),
        }
    }
}

/// Parse a `data:` URL into its media type and decoded bytes; `None` for other
/// schemes, for payloads without the `,` separator, and for invalid base64.
pub fn parse_data_url(url: &Url) -> Option<DataUrl> {
    if url.scheme() != "data" {
        return None;
    }
    let rest = url.as_str().strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime_raw = parts.next().unwrap_or_default().trim();
    let params: Vec<&str> = parts.map(str::trim).collect();
    let base64 = params.iter().any(|p| p.eq_ignore_ascii_case("base64"));
    let charset = params.iter().find_map(|p| {
        p.to_ascii_lowercase()
            .strip_prefix("charset=")
            .map(|c| c.trim_matches('"').trim().to_owned())
    });
    let bytes = if base64 {
        decode_base64(payload)?
    } else {
        percent_decode(payload.as_bytes())
    };
    let mime = if mime_raw.is_empty() {
        "text/plain".to_owned()
    } else {
        mime_raw.to_ascii_lowercase()
    };
    Some(DataUrl {
        mime,
        charset,
        base64,
        bytes,
    })
}

/// Decode a `data:` URL (RFC 2397): `data:[<mediatype>][;base64],<payload>`.
/// Use [`parse_data_url`] when the media type matters (it always does for
/// top-level documents).
pub fn decode_data_url(url: &Url) -> Option<Vec<u8>> {
    parse_data_url(url).map(|d| d.bytes)
}

/// Minimal percent decoding (byte oriented; UTF-8 is reassembled downstream).
pub(crate) fn percent_decode(input: &[u8]) -> Vec<u8> {
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(hi), Some(lo)) = (digit(input[i + 1]), digit(input[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// Standard-alphabet base64 encoder (RFC 4648, `=` padded). Only tests build
/// `data:` URLs, so it is test-only code kept beside [`decode_base64`] — the
/// pair cannot drift.
#[cfg(test)]
pub fn encode_base64(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                    abcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let chars = [
            ALPHA[(n >> 18) as usize & 63],
            ALPHA[(n >> 12) as usize & 63],
            ALPHA[(n >> 6) as usize & 63],
            ALPHA[n as usize & 63],
        ];
        let used = chunk.len() + 1; // 1..=3 real bytes ⇒ 2..=4 characters
        for &c in &chars[..used] {
            out.push(c as char);
        }
        for _ in used..4 {
            out.push('=');
        }
    }
    out
}

/// Strict base64 decoder (RFC 4648 standard + URL-safe alphabets, optional
/// `=` padding) — enough for `data:` URLs, no extra dependency needed.
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u64> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u64),
            b'a'..=b'z' => Some(((c - b'a') as u64) + 26),
            b'0'..=b'9' => Some(((c - b'0') as u64) + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    let mut out: Vec<u8> = Vec::new();
    for &b in input.as_bytes() {
        if matches!(b, b'\n' | b'\r' | b' ' | b'\t') {
            continue;
        }
        if b == b'=' {
            break;
        }
        let v = val(b)?;
        acc = acc << 6 | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
            acc &= (1u64 << nbits) - 1; // drop the bits already emitted
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Worker-thread fetcher (blocking reqwest/rustls on spawned threads)

/// Owns the container's blocking `reqwest` (rustls) clients and runs GETs on
/// freshly-spawned worker threads, feeding `Box<dyn NetHandler>` callbacks.
/// The clients live behind mutexes so [`FetchWorker::set_proxy`] can swap
/// the container's route at runtime: reqwest binds proxies to clients, and
/// handing out an `Arc`-cheap clone per fetch is how the swap reaches every
/// fetcher without rebuilding anything else.
pub struct FetchWorker {
    client: Mutex<reqwest::blocking::Client>,
    /// Same transport, redirects left to the caller (the managed cookie path).
    redirect_manual: Mutex<reqwest::blocking::Client>,
    /// The launch timeout, kept so a proxy swap rebuilds identical clients.
    timeout: std::time::Duration,
}

impl FetchWorker {
    /// 30 s-timeout rustls client (keeps tabs responsive on hung servers).
    pub fn new() -> Arc<Self> {
        Self::with_timeout(std::time::Duration::from_secs(30))
    }

    pub fn with_timeout(timeout: std::time::Duration) -> Arc<Self> {
        Arc::new(Self::built(timeout))
    }

    fn built(timeout: std::time::Duration) -> Self {
        let (client, redirect_manual) = Self::clients(timeout, None);
        FetchWorker {
            client: Mutex::new(client),
            redirect_manual: Mutex::new(redirect_manual),
            timeout,
        }
    }

    /// Build the pair of clients for one route: `proxy = None` is direct,
    /// `Some(p)` sends both through `p` (the same route serves passthrough
    /// and managed traffic — a container's traffic does not split). The
    /// auto client follows redirects; the manual one hands every hop back to
    /// the managed cookie path, which replays per-hop `Cookie` headers and
    /// captures per-hop `Set-Cookie`s — reqwest's automatic policy would
    /// blindly replay the first hop's jar header and show only the final
    /// hop's responses.
    fn clients(
        timeout: std::time::Duration,
        proxy: Option<&reqwest::Proxy>,
    ) -> (reqwest::blocking::Client, reqwest::blocking::Client) {
        let build = |auto: bool| {
            let mut builder = reqwest::blocking::Client::builder()
                .use_rustls_tls()
                .redirect(if auto {
                    reqwest::redirect::Policy::limited(10)
                } else {
                    reqwest::redirect::Policy::none()
                })
                .timeout(timeout)
                .pool_max_idle_per_host(16)
                .connect_timeout(std::time::Duration::from_secs(10))
                .tcp_nodelay(true);
            if auto {
                // HTTP/2 keep-alive + adaptive flow control (multiplexed tabs).
                builder = builder
                    .http2_keep_alive_interval(std::time::Duration::from_secs(30))
                    .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
                    .http2_adaptive_window(true);
            }
            if let Some(proxy) = proxy {
                builder = builder.proxy(proxy.clone());
            }
            builder.build().expect("reqwest client")
        };
        (build(true), build(false))
    }

    /// Swap the container's proxy: both clients are rebuilt on the spot, so
    /// every `client()`/`managed_client()` taken afterwards rides the new
    /// route (requests already in flight finish on the old one, exactly
    /// like a tab that had the page half-loaded when the switch flipped).
    pub fn set_proxy(&self, proxy: Option<reqwest::Proxy>) {
        let (auto, manual) = Self::clients(self.timeout, proxy.as_ref());
        *self.client.lock().unwrap_or_else(|p| p.into_inner()) = auto;
        *self.redirect_manual.lock().unwrap_or_else(|p| p.into_inner()) = manual;
    }

    pub fn client(&self) -> reqwest::blocking::Client {
        self.client.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The client managed cookie fetches must use (3xx responses come back
    /// to us instead of being followed blindly).
    pub fn managed_client(&self) -> reqwest::blocking::Client {
        self.redirect_manual.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

}

impl Default for FetchWorker {
    fn default() -> Self {
        Self::built(std::time::Duration::from_secs(30))
    }
}

// ---------------------------------------------------------------------------
// Subresource fetch pool (CAPPED, EAGAIN-tolerant)

/// Subresource fetch threads ONE provider runs at a time. A page carries
/// hundreds of `<img>`/`<link>` loads and Blitz hands every one of them to
/// `NetProvider::fetch` in a single parse pass, so the historical "one
/// `std::thread::spawn` per subresource" design made thread count a function
/// of page size: an image-heavy page under a thread limit took `EAGAIN` in
/// `thread::spawn` — which panics — and the whole tab died (the tube-"scan"
/// page crash: 24 000 threads and a dead process). Eight concurrent fetchers
/// saturate the shared reqwest pool as well as an unbounded fan-out ever did,
/// and put a hard ceiling on the tab's thread count.
pub const MAX_SUBRESOURCE_FETCHERS: usize = 8;

/// One settled subresource load: fetch on the wire, then deliver (or drop)
/// the `Box<dyn NetHandler>` — the closure owns the handler and must settle
/// it exactly once, because `NetHandler` has no error path.
type FetchJob = Box<dyn FnOnce() + Send + 'static>;

/// A capped, self-healing work queue for [`FetchJob`]s: the queue a page's
/// subresources run on.
///
/// Three properties, all load-bearing:
///
/// * **Bounded** — at most `cap` threads, however many jobs the page submits
///   (a 5 000-image page gets `cap` fetch threads, not 5 000).
/// * **Never panics on thread creation** — spawns go through
///   `thread::Builder::spawn`, which reports `EAGAIN` as an error instead of
///   panicking like `thread::spawn` does. A failed spawn is not fatal and not
///   final: the job stays queued and every worker that goes looking for more
///   work retries the spawn, so the pool heals itself once the limit passes.
///   If the pool has NO thread at all (creation failed from the start), the
///   submitter runs a queued job itself — slow (that is Blitz's parse thread),
///   but alive and never stalled.
/// * **Nothing is dropped** — every submitted job eventually settles its
///   handler; jobs are never discarded to save resources, and a panicking job
///   is contained so it cannot take the worker (and every later fetch) down.
pub struct FetchQueue {
    /// Worker threads this queue may run (never exceeded).
    cap: usize,
    /// Jobs waiting for a worker; workers sleep on `ready` when it is empty.
    pending: Mutex<VecDeque<FetchJob>>,
    ready: Condvar,
    /// Live worker threads. Workers never exit (they park on `ready`), so the
    /// count never drifts and the cap can be enforced without exit races.
    threads: AtomicUsize,
    /// Serialises spawn attempts so two concurrent submits can never
    /// co-operate their way past `cap`.
    spawning: Mutex<()>,
    /// Test seam: while positive, that many spawn attempts fail with `EAGAIN`
    /// (`WouldBlock`) — the sandbox thread limit, deterministically.
    fail_spawns: AtomicUsize,
}

impl FetchQueue {
    /// A pool of at most `cap` fetch threads (always at least one).
    fn new(cap: usize) -> Arc<Self> {
        Arc::new(FetchQueue {
            cap: cap.max(1),
            pending: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            threads: AtomicUsize::new(0),
            spawning: Mutex::new(()),
            fail_spawns: AtomicUsize::new(0),
        })
    }

    /// Threads this queue is willing to run (observational).
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Live worker threads (observational).
    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::SeqCst)
    }

    /// Jobs not yet picked up by a worker (observational).
    pub fn queued(&self) -> usize {
        self.pending
            .lock()
            .map(|jobs| jobs.len())
            .unwrap_or_default()
    }

    /// Make the next `n` worker-spawn attempts fail with `EAGAIN` — the OS
    /// thread limit under test (see the pool tests).
    fn fail_next_spawns(&self, n: usize) {
        self.fail_spawns.store(n, Ordering::SeqCst);
    }

    /// Queue a job and make sure somebody runs it. Returns immediately except
    /// in the degraded case where the pool has no thread to hand the work to.
    pub fn submit(self: &Arc<Self>, job: FetchJob) {
        self.pending
            .lock()
            .expect("fetch queue mutex poisoned")
            .push_back(job);
        self.ready.notify_one();
        self.top_up();
        if self.threads() == 0 {
            // No workers exist and none could be created: run one queued job
            // on the calling thread rather than strand it. Thread creation
            // failures are self-healing afterwards — every later submit and
            // every finished job retries the spawn.
            warn!("no subresource fetch thread could be created; running one fetch inline");
            self.run_next();
        }
    }

    /// Grow the pool towards `min(cap, jobs waiting)`, stopping at the first
    /// failed spawn (`EAGAIN`) — retried by the next submit or the next worker
    /// to finish a job.
    fn top_up(self: &Arc<Self>) {
        // Held across the spawn attempts: only one thread may be growing the
        // pool at a time, or the cap becomes advisory.
        let _gate = self
            .spawning
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while self.threads() < self.cap {
            // Grow only while there is more backlog than workers: a queue
            // holding one job needs one thread, not `cap`.
            if self.threads() >= self.queued() {
                break;
            }
            match self.spawn_worker() {
                Ok(()) => (),
                Err(err) => {
                    trace!("subresource fetch worker unavailable ({err}); queueing the rest");
                    break;
                }
            }
        }
    }

    fn spawn_worker(self: &Arc<Self>) -> io::Result<()> {
        // Test seam (`top_up` is the only caller, and it holds the `spawning`
        // gate, so the load/subtract pair cannot race).
        if self.fail_spawns.load(Ordering::SeqCst) > 0 {
            self.fail_spawns.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.threads.fetch_add(1, Ordering::SeqCst);
        let queue = Arc::clone(self);
        match std::thread::Builder::new()
            .name("ironsurf-fetch".to_string())
            .spawn(move || queue.worker())
        {
            // Detached on purpose: the worker parks on the queue instead of
            // exiting, so it costs nothing between loads and needs no join.
            Ok(_worker) => Ok(()),
            Err(err) => {
                self.threads.fetch_sub(1, Ordering::SeqCst);
                Err(err)
            }
        }
    }

    /// Run the next queued job on the calling thread (a no-op when idle).
    fn run_next(&self) {
        let job = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        if let Some(job) = job {
            run_guarded(job);
        }
    }

    /// The worker loop: sleep while idle, run jobs as they arrive, and try to
    /// bring up more of the pool whenever a backlog built up on the wire.
    fn worker(self: Arc<Self>) {
        loop {
            let job = {
                let mut pending = self
                    .pending
                    .lock()
                    .expect("fetch queue mutex poisoned");
                while pending.is_empty() {
                    pending = self.ready.wait(pending).expect("fetch queue condvar poisoned");
                }
                pending.pop_front()
            };
            if let Some(job) = job {
                run_guarded(job);
                // While we were on the wire the queue may have grown, and the
                // resource limit that blocked the last spawn may have passed.
                if self.queued() > 0 {
                    self.top_up();
                }
            }
        }
    }
}

/// Settle one job, containing its panic: a broken fetch must cost that one
/// resource, not the worker thread and everything still queued behind it.
fn run_guarded(job: FetchJob) {
    if let Err(err) = catch_unwind(AssertUnwindSafe(job)) {
        let detail = err
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| err.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unspecified panic".to_owned());
        warn!("subresource fetch job panicked ({detail}); the fetch worker survives");
    }
}


/// Everything an intercepted load yields after cookies/validators were fed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchOutcome {
    /// Post-redirect URL the response actually came from.
    pub final_url: Url,
    pub status: u16,
    /// Header list (original casing, `Set-Cookie`s still included).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// True when the body came from [`NetCache`] (304 revalidation or a
    /// direct serve of an explicitly fresh entry).
    pub from_cache: bool,
}

fn collect_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                v.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// Delivery: the ONE success path of `blitz_traits::net::NetHandler`.
/// Passing `None` on failure models the trait's missing error path exactly
/// like upstream `blitz-net` does (the handler is dropped, the resource
/// silently never loads).
pub fn deliver(handler: Option<Box<dyn NetHandler>>, resolved_url: String, bytes: Vec<u8>) {
    match handler {
        Some(handler) => handler.bytes(resolved_url, Bytes::from(bytes)),
        None => drop(handler),
    }
}

/// A main-document `POST` (an HTML form submission): the encoded body and
/// the enctype we are sending it as. `None` everywhere below means GET.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainDocPost {
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Build the request line: GET, or POST with the form body (and its
/// `Content-Type`) riding along.
///
/// The clients this builds on are the container's current route (they are
/// swapped wholesale by [`FetchWorker::set_proxy`] when the proxy changes),
/// so both the passthrough path and the managed hop loop — documents, POSTs
/// and redirects — ride it without per-request plumbing.
fn build_request(
    client: &reqwest::blocking::Client,
    url: &Url,
    post: Option<&MainDocPost>,
) -> reqwest::blocking::RequestBuilder {
    let req = match post {
        Some(body) => {
            let req = client.post(url.as_str()).body(body.body.clone());
            match &body.content_type {
                Some(content_type) => req.header(reqwest::header::CONTENT_TYPE, content_type.as_str()),
                None => req,
            }
        }
        None => client.get(url.as_str()),
    };
    // Private browsing (task): every request this process builds carries
    // DNT: 1 while the switch is on (XHR builds its own, handled beside).
    with_dnt(req)
}

/// Plain (`Passthrough`) HTTP fetch (GET, or a form `POST` with `post`):
/// our User-Agent plus whatever the Blitz `Request` carried; no cookies, no
/// cache — and a POST never consults the cache either.
#[allow(dead_code)] // quiet convenience wrapper (the live paths report; tests use it)
pub fn plain_http_fetch(
    client: &reqwest::blocking::Client,
    url: &Url,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
) -> Result<FetchOutcome, String> {
    plain_http_fetch_report(client, url, extra, post, USER_AGENT, None)
}

/// [`plain_http_fetch`] with the DevTools reporter attached: one event for
/// the finished request (reqwest's automatic redirects mean only the final
/// response is visible here) or one failure event naming the failure.
pub fn plain_http_fetch_report(
    client: &reqwest::blocking::Client,
    url: &Url,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
    user_agent: &str,
    report: NetReport,
) -> Result<FetchOutcome, String> {
    let verb = if post.is_some() { "POST" } else { "GET" };
    let started = std::time::Instant::now();
    let mut req = build_request(client, url, post).header("User-Agent", user_agent);
    if let Some(headers) = extra {
        if !headers.is_empty() {
            req = req.headers(headers.clone());
        }
    }
    let response = req.send().map_err(|e| {
        warn!("passthrough {verb} {url} failed: {e}");
        // While the tab rides a proxy, a connect/timeout failure is the
        // proxy's — the socket that refused or stalled was the proxy's,
        // and reqwest's own text would blame the site (attribution reads
        // the route cell, not the client). Otherwise: the plain wording.
        let message = attribute_send_error(&e).unwrap_or_else(|| format!("fetch {url}: {e}"));
        if let Some(report) = report {
            report(net_failed(verb, url, elapsed_ms(started), message.clone()));
        }
        message
    })?;
    let status = response.status().as_u16();
    let final_url = response.url().clone();
    let headers = collect_headers(response.headers());
    let body = response.bytes().map(|b| b.to_vec()).map_err(|e| {
        let message = format!("body {url}: {e}");
        if let Some(report) = report {
            report(net_failed(verb, &final_url, elapsed_ms(started), message.clone()));
        }
        message
    })?;
    debug!("{verb} {url} -> {status} ({} B, passthrough)", body.len());
    if let Some(report) = report {
        report(net_ok(verb, &final_url, status, body.len() as u64, elapsed_ms(started)));
    }
    Ok(FetchOutcome {
        final_url,
        status,
        headers,
        body,
        from_cache: false,
    })
}

/// The answer an `XMLHttpRequest.send` gets back: everything the page reads
/// off the finished request (`headers` with lowercased names, `body` as
/// lossy UTF-8 — the shape the glue hands back as a JSON envelope).
pub struct XhrReply {
    pub status: u16,
    pub status_text: String,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    /// Raw response bytes (the XHR glue lossy-decodes them to UTF-8 for
    /// `responseText`; `fetch()` keeps them byte-exact).
    pub body: Vec<u8>,
}

/// One `XMLHttpRequest` exchange (task: infosec.exchange posts): ANY method,
/// the page's own request headers, an optional body — on the shared worker's
/// auto-redirect client. The container jar rides along exactly like a
/// browser's: the jar's `Cookie` header is sent for the URL, `Set-Cookie`
/// responses are captured into the jar (the page's own `Cookie` header is
/// ignored — the jar owns that header), and one [`NetEvent`] per finished or
/// failed request feeds the DevTools network panel. The cache is
/// deliberately NOT consulted: an API body replayed stale from the document
/// cache would lie to the app (browsers may use the HTTP cache here, but the
/// response the page gets is always the truth). https/http only; any other
/// scheme is an error the page sees as a network failure.
pub fn xhr_fetch_report<J>(
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    jar: &mut J,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<XhrReply, String>
where
    J: JarAccess + ?Sized,
{
    if !is_http_scheme(url) {
        return Err(format!("xhr: unsupported scheme for {url}"));
    }
    let verb = method.to_ascii_uppercase();
    let user_agent = read_user_agent(user_agent);
    let started = std::time::Instant::now();
    let parsed_method = reqwest::Method::from_bytes(verb.as_bytes())
        .map_err(|_| format!("xhr: bad method {method:?}"))?;
    let mut req = with_dnt(
        shared_worker()
            .client()
            .request(parsed_method, url.as_str())
            .header("User-Agent", user_agent),
    );
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("cookie") || name.eq_ignore_ascii_case("user-agent") {
            continue; // the jar owns Cookie, the UA cell owns User-Agent
        }
        req = req.header(name.as_str(), value.as_str());
    }
    if let Some(cookie) = jar.cookie_header_at(url, now) {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    if let Some(body) = body {
        req = req.body(body.to_vec());
    }
    let response = req.send().map_err(|e| {
        // Proxy attribution: see the passthrough fetcher — a page's XHR/
        // fetch() against a dead proxy fails with the proxy named, not a
        // message that reads like the API host is down.
        let message = attribute_send_error(&e).unwrap_or_else(|| format!("fetch {url}: {e}"));
        warn!("xhr {verb} {url} failed: {e}");
        if let Some(report) = report {
            report(net_failed(&verb, url, elapsed_ms(started), message.clone()));
        }
        message
    })?;
    let status = response.status().as_u16();
    // reqwest exposes no reason phrase (HTTP/2 has none either); the IANA
    // canonical reason is what browsers synthesise for HTTP/2 responses.
    let status_text = response
        .status()
        .canonical_reason()
        .unwrap_or("")
        .to_owned();
    let final_url = response.url().clone();
    let headers = collect_headers(response.headers());
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let bytes = response.bytes().map_err(|e| {
        let message = format!("body {final_url}: {e}");
        if let Some(report) = report {
            report(net_failed(&verb, &final_url, elapsed_ms(started), message.clone()));
        }
        message
    })?;
    if !set_cookies.is_empty() {
        jar.capture_set_cookies_at(&final_url, set_cookies, now);
    }
    debug!("xhr {verb} {final_url} -> {status} ({} B)", bytes.len());
    if let Some(report) = report {
        report(net_ok(
            &verb,
            &final_url,
            status,
            bytes.len() as u64,
            elapsed_ms(started),
        ));
    }
    Ok(XhrReply {
        status,
        status_text,
        final_url: final_url.to_string(),
        headers,
        body: bytes.to_vec(),
    })
}

/// Cookie-jar work a managed fetch needs, in three shapes: the jar itself
/// (a caller that already owns one — tests, the media worker's private jar),
/// a [`MutexGuard`] the caller holds, and a [`Mutex`] **shared** with other
/// fetch threads.
///
/// The point of the split is *lock scope*: with a `Mutex` implementation the
/// mutex is taken for the instant of one map operation and released before
/// the request is built, so no fetch ever holds container state across
/// `send()`/`bytes()`. With `Passthrough`-free managed fetching that used to
/// mean every image/CSS/font in a tab downloaded strictly one-by-one.
pub trait JarAccess {
    /// The `Cookie` header the jar assembles for `url` at `now` (none when
    /// nothing matches).
    fn cookie_header_at(&mut self, url: &Url, now: i64) -> Option<String>;
    /// Bind every `Set-Cookie` header of a response to `url` at `now`.
    fn capture_set_cookies_at(
        &mut self,
        url: &Url,
        set_cookies: Vec<String>,
        now: i64,
    );
}

impl JarAccess for CookieJar {
    fn cookie_header_at(&mut self, url: &Url, now: i64) -> Option<String> {
        self.cookie_header(url, now)
    }
    fn capture_set_cookies_at(&mut self, url: &Url, set_cookies: Vec<String>, now: i64) {
        capture_set_cookies(self, url, set_cookies.iter().map(String::as_str), now);
    }
}

impl JarAccess for Mutex<CookieJar> {
    fn cookie_header_at(&mut self, url: &Url, now: i64) -> Option<String> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).cookie_header(url, now)
    }
    fn capture_set_cookies_at(&mut self, url: &Url, set_cookies: Vec<String>, now: i64) {
        let mut jar = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        capture_set_cookies(&mut jar, url, set_cookies.iter().map(String::as_str), now);
    }
}

impl<'jar> JarAccess for &'jar Mutex<CookieJar> {
    fn cookie_header_at(&mut self, url: &Url, now: i64) -> Option<String> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).cookie_header(url, now)
    }
    fn capture_set_cookies_at(&mut self, url: &Url, set_cookies: Vec<String>, now: i64) {
        let mut jar = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        capture_set_cookies(&mut jar, url, set_cookies.iter().map(String::as_str), now);
    }
}

impl JarAccess for MutexGuard<'_, CookieJar> {
    fn cookie_header_at(&mut self, url: &Url, now: i64) -> Option<String> {
        self.cookie_header(url, now)
    }
    fn capture_set_cookies_at(&mut self, url: &Url, set_cookies: Vec<String>, now: i64) {
        capture_set_cookies(self, url, set_cookies.iter().map(String::as_str), now);
    }
}

/// The two cache reads/writes a managed fetch performs, in the same three
/// shapes as [`JarAccess`]. `CachedEntry` is cloned out of the cache, so a
/// shared cache never hands a reference (or its lock) to a fetch thread
/// across the network exchange.
pub trait CacheAccess {
    /// Clone of the cached response for `url` (misses are `None`).
    fn lookup_at(&self, url: &str) -> Option<CachedEntry>;
    /// Insert/replace the response for `url` (LRU bounds apply).
    fn store_at(
        &self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: Vec<u8>,
        now: i64,
    );
}

impl CacheAccess for NetCache {
    fn lookup_at(&self, url: &str) -> Option<CachedEntry> {
        self.lookup(url)
    }
    fn store_at(
        &self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: Vec<u8>,
        now: i64,
    ) {
        self.store(url, status, headers, body, now)
    }
}

impl CacheAccess for Mutex<NetCache> {
    fn lookup_at(&self, url: &str) -> Option<CachedEntry> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).lookup(url)
    }
    fn store_at(
        &self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: Vec<u8>,
        now: i64,
    ) {
        self.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .store(url, status, headers, body, now)
    }
}

impl<'cache> CacheAccess for &'cache Mutex<NetCache> {
    fn lookup_at(&self, url: &str) -> Option<CachedEntry> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).lookup(url)
    }
    fn store_at(
        &self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: Vec<u8>,
        now: i64,
    ) {
        self.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .store(url, status, headers, body, now)
    }
}

impl CacheAccess for MutexGuard<'_, NetCache> {
    fn lookup_at(&self, url: &str) -> Option<CachedEntry> {
        self.lookup(url)
    }
    fn store_at(
        &self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: Vec<u8>,
        now: i64,
    ) {
        self.store(url, status, headers, body, now)
    }
}

/// Maximum redirects a single managed fetch follows before giving up.
const MAX_MANAGED_REDIRECTS: u32 = 10;

/// Managed HTTP fetch (GET, or a form `POST` with `post`): the full cookie
/// workhorse. Redirects are followed **hop by hop** (the caller must hand us
/// a `Policy::none()` client — [`FetchWorker::managed_client`]) so that
/// every hop gets its own `Cookie` header from the container jar and every
/// hop's `Set-Cookie`s are captured against **that hop's** URL — reqwest's
/// automatic redirect would replay hop 1's cookies to hop 2 verbatim and
/// only ever see the final hop's `Set-Cookie`s, silently breaking the
/// per-origin rules. Method conversion follows the browser convention: 303
/// always retries as GET; 301/302 turn a POST into a GET; 307/308 keep the
/// method and body.
///
/// `If-None-Match`/`If-Modified-Since` validators go on the first hop and
/// [`NetCache`] entries are stored under the original `url` — but only when
/// `cacheable` (a `POST` is never cached, look up or store). `extra` (from
/// the Blitz `Request`) never overrides our `Cookie` header.
#[allow(dead_code)] // quiet convenience wrapper (the live paths report; tests use it)
pub fn managed_http_fetch(
    client: &reqwest::blocking::Client,
    url: &Url,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
    cacheable: bool,
) -> Result<FetchOutcome, String> {
    managed_http_fetch_report(
        client, url, jar, cache, now, extra, post, cacheable, USER_AGENT, None,
    )
}

/// [`managed_http_fetch`] with the DevTools reporter attached. Exactly one
/// event per completed or failed request: every followed redirect hop gets
/// its own event (that hop's method, URL, 3xx status, declared body size),
/// the terminating response gets the final event, and a send/body failure
/// gets a single failure event (`status: None`). An explicitly fresh
/// [`NetCache`] hit emits exactly one event for the original URL (the cached
/// entry's status/size, duration 0) and never touches the network.
pub fn managed_http_fetch_report<J, C>(
    client: &reqwest::blocking::Client,
    url: &Url,
    jar: &mut J,
    cache: &C,
    now: i64,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
    cacheable: bool,
    user_agent: &str,
    report: NetReport,
) -> Result<FetchOutcome, String>
where
    J: JarAccess + ?Sized,
    C: CacheAccess + ?Sized,
{
    let is_post = post.is_some();
    let cacheable = cacheable && !is_post;
    let started = std::time::Instant::now();
    let cached = if cacheable {
        let cached = cache.lookup_at(url.as_str());
        if cached.is_some() {
            debug!("cache entry for {url}: checking explicit freshness");
        }
        cached
    } else {
        None
    };

    // Direct serve: an entry the server declared *explicitly* fresh
    // (Cache-Control max-age/s-maxage/Expires, and none of no-store/no-cache/
    // must-revalidate/proxy-revalidate — see `fresh_until`) costs no network
    // round trip at all: RFC 9111 says exactly that. Everything else keeps
    // the conditional-revalidation path below unchanged.
    //
    // Cookie note: this path must NOT re-capture Set-Cookie. The entry's
    // Set-Cookies were already captured into the jar when the response was
    // first received and stored; replaying them from the cached headers on
    // every serve would resurrect stale (possibly superseded or expired)
    // session cookies over and over.
    if let Some(entry) = cached.as_ref() {
        if entry.status < 400
            && fresh_until(&entry.headers, entry.cached_at).is_some_and(|until| until > now)
        {
            debug!(
                "GET {url} served fresh from cache ({} B, cached at {}, no revalidation)",
                entry.body.len(),
                entry.cached_at
            );
            // DevTools sees the served document as one completed request for
            // the original URL, mirroring the revalidated-hit rule (the
            // cached entry's status/size); there is no wire time, so the
            // duration is 0.
            if let Some(report) = report {
                report(net_ok("GET", url, entry.status, entry.body.len() as u64, 0));
            }
            let entry = cached.expect("fresh hit implies a cached entry");
            return Ok(FetchOutcome {
                final_url: url.clone(),
                status: entry.status,
                headers: entry.headers,
                body: entry.body,
                from_cache: true,
            });
        }
    }

    let mut current = url.clone();
    let mut method = post; // POST -> GET conversions happen on redirect
    let mut first_hop = true;
    let mut hops_left = MAX_MANAGED_REDIRECTS;
    let (verb, status, final_url, headers, body) = loop {
        let mut req = build_request(client, &current, method).header("User-Agent", user_agent);
        if let Some(extra_headers) = extra {
            for (name, value) in extra_headers {
                if name.as_str().eq_ignore_ascii_case("cookie") {
                    continue; // the jar owns the Cookie header
                }
                req = req.header(name.clone(), value.clone());
            }
        }
        // The jar answers for this hop's origin, at this hop's URL — a
        // redirect into another host must never inherit cookies blindly.
        if let Some(cookie) = jar.cookie_header_at(&current, now) {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        if first_hop {
            if let Some(entry) = cached.as_ref() {
                let (etag, last_modified) = validators(&entry.headers);
                if let Some(etag) = etag {
                    req = req.header(reqwest::header::IF_NONE_MATCH, etag);
                } else if let Some(lm) = last_modified {
                    req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
                }
            }
        }
        let verb = if method.is_some() { "POST" } else { "GET" };
        let hop_start = std::time::Instant::now();
        let response = req.send().map_err(|e| {
            warn!("{verb} {current} failed: {e}");
            // Proxy attribution on every hop (redirect chains included):
            // see the passthrough fetcher.
            let message = attribute_send_error(&e).unwrap_or_else(|| format!("fetch {current}: {e}"));
            if let Some(report) = report {
                report(net_failed(
                    verb,
                    &current,
                    elapsed_ms(hop_start),
                    message.clone(),
                ));
            }
            message
        })?;
        let status = response.status().as_u16();

        // Every hop's Set-Cookies bind to the hop that sent them.
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        jar.capture_set_cookies_at(&current, set_cookies.clone(), now);
        if !set_cookies.is_empty() {
            debug!(
                "captured {} Set-Cookie header(s) for {}",
                set_cookies.len(),
                current
            );
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|loc| !loc.is_empty())
                .and_then(|loc| current.join(loc).ok());
            match location {
                Some(target) if hops_left > 0 => {
                    debug!("{verb} {current} -> {status} redirect to {target} ({hops_left} hops left)");
                    // DevTools sees the hop as its own completed request; the
                    // hop body is never read, so the declared size is the
                    // honest answer (0 when the server declared none).
                    if let Some(report) = report {
                        let declared = response
                            .headers()
                            .get(reqwest::header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .unwrap_or(0);
                        report(net_ok(
                            verb,
                            &current,
                            status,
                            declared,
                            elapsed_ms(hop_start),
                        ));
                    }
                    // 303 always retries with GET; 301/302 convert a POST.
                    // 307/308 keep the method and body.
                    if status == 303 || (matches!(status, 301 | 302) && method.is_some()) {
                        method = None;
                    }
                    current = target;
                    first_hop = false;
                    hops_left -= 1;
                    continue; // response dropped here: hop body unread
                }
                Some(target) => {
                    warn!("redirect loop budget exhausted at {current} (to {target}); serving the {status}");
                }
                None => {} // no usable Location: the 3xx is the answer
            }
        }

        let headers = collect_headers(response.headers());
        let body = response.bytes().map(|b| b.to_vec()).map_err(|e| {
            let message = format!("body {current}: {e}");
            if let Some(report) = report {
                report(net_failed(
                    verb,
                    &current,
                    elapsed_ms(hop_start),
                    message.clone(),
                ));
            }
            message
        })?;
        break (verb, status, current.clone(), headers, body);
    };

    let (etag, last_modified) = validators(&headers);
    let empty = || CachedEntry {
        status: 0,
        headers: Vec::new(),
        body: Vec::new(),
        etag: None,
        last_modified: None,
        cached_at: 0,
    };
    match revalidate(
        &cached.clone().unwrap_or_else(empty),
        status,
        etag.as_deref(),
        last_modified.as_deref(),
    ) {
        Revalidate::ServeCached => {
            let entry = cached.expect("ServeCached implies a cached entry existed");
            debug!(
                "GET {url} -> {status} served from cache ({} B, cached at {})",
                entry.body.len(),
                entry.cached_at
            );
            // The 304 exchange reported its own event; the document the
            // caller actually gets is the cached one, so the DevTools entry
            // carries the cache entry's status/size, not the 304.
            if let Some(report) = report {
                report(net_ok(
                    verb,
                    &final_url,
                    entry.status,
                    entry.body.len() as u64,
                    elapsed_ms(started),
                ));
            }
            Ok(FetchOutcome {
                final_url,
                status: entry.status,
                headers: entry.headers,
                body: entry.body,
                from_cache: true,
            })
        }
        Revalidate::UseFresh { .. } => {
            if cacheable && status < 400 {
                debug!("GET {url} -> {status} ({} B, stored in cache)", body.len());
                cache.store_at(url.as_str(), status, &headers, body.clone(), now);
            } else if is_post {
                debug!("POST {url} -> {status} ({} B, never cached)", body.len());
            } else {
                debug!("managed {url} -> {status} ({} B, not cacheable)", body.len());
            }
            if let Some(report) = report {
                report(net_ok(
                    verb,
                    &final_url,
                    status,
                    body.len() as u64,
                    elapsed_ms(started),
                ));
            }
            Ok(FetchOutcome {
                final_url,
                status,
                headers,
                body,
                from_cache: false,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// BlitzNetProvider — `blitz_traits::net::NetProvider` for DocumentConfig

/// Sub-resource network provider handed to
/// `DocumentConfig { net_provider: Some(Arc<BlitzNetProvider>) }`.
///
/// `NetProvider`/`NetHandler` are `Send + Sync + 'static` (called from Blitz
/// worker contexts), so the mutable container state ([`CookieJar`],
/// [`NetCache`]) lives behind `Mutex` — never `RefCell`.
pub struct BlitzNetProvider {
    policy: NetPolicy,
    worker: Arc<FetchWorker>,
    /// The CAPPED pool this tab's subresource loads run on (see
    /// [`FetchQueue`]): one page's thousands of `<img>`/`<link>` fetches never
    /// become thousands of threads.
    subresources: Arc<FetchQueue>,
    jar: Arc<Mutex<CookieJar>>,
    cache: Arc<Mutex<NetCache>>,
    waker: Arc<Mutex<Option<Arc<dyn NetWaker>>>>,
    /// The tab's effective User-Agent cell (shared with the runtime and the
    /// JS bindings): read per request, so a `ToTab::SetUserAgent` applies to
    /// every subresource fetched after it.
    user_agent: SharedUserAgent,
    /// Completed subresource requests (one [`NetEvent`] each), queued by the
    /// fetch threads and drained by the tab loop into
    /// `FromTab::NetworkEvent` messages. The NetWaker already wakes the loop
    /// when a fetch settles, so nothing extra is needed to make the drain
    /// timely.
    net_events: Arc<Mutex<Vec<NetEvent>>>,
    /// DevTools **console** entries raised by the fetch threads — today the
    /// wrong-`Content-Type` seam for `.css` responses (see
    /// [`crate::logging::css_content_type_warning`]); drained by the tab loop
    /// exactly like `net_events` (same wake, same tick).
    console_events: crate::logging::ConsoleQueue,
    /// Archive-mode sink (the shell's 💾 toggle): a delivered subresource
    /// body is queued here (`store` is a no-op while the mode is off) and
    /// shipped as `FromTab::ArchivedFetch` by the tab loop.
    archive: Arc<crate::archive::ArchiveState>,
    /// The **private-address rule** (see [`crate::private_addr`], AUDIT
    /// finding 5): refuses an http(s) subresource of a page on a public origin
    /// that points at the browser's own network. Owns the tab's console queue,
    /// so a refusal prints a DevTools warning.
    private: crate::private_addr::PrivateAddressGuard,
    /// The URL of the document the provider is loading for, written by the tab
    /// on every navigation. Blitz hands a `NetProvider` no document handle, so
    /// the origin the private-address rule compares against has to be kept
    /// here — the top-level page's, which an `<iframe>` inherits (the
    /// conservative reading; see the module docs).
    document: Arc<Mutex<Option<Url>>>,
}

impl BlitzNetProvider {
    /// Jar + cache come from the container's profile dir
    /// (`ContainerStore::cookie_jar` / `::cache`).
    pub fn new(policy: NetPolicy, jar: CookieJar, cache: NetCache) -> Self {
        let console_events = crate::logging::console_queue();
        let private = crate::private_addr::PrivateAddressGuard::new(console_events.clone());
        BlitzNetProvider {
            policy,
            // One process-wide FetchWorker: documents, scripts, subresources
            // and media share a single reqwest pool (keep-alive, TLS, DNS).
            worker: shared_worker(),
            // One capped fetch pool per tab (per process in practice): thread
            // count is a constant, not a function of how many images the page
            // happens to carry.
            subresources: FetchQueue::new(MAX_SUBRESOURCE_FETCHERS),
            jar: Arc::new(Mutex::new(jar)),
            cache: Arc::new(Mutex::new(cache)),
            waker: Arc::new(Mutex::new(None)),
            user_agent: new_user_agent(None),
            net_events: Arc::new(Mutex::new(Vec::new())),
            console_events,
            archive: crate::archive::ArchiveState::shared(false),
            private,
            document: Arc::new(Mutex::new(None)),
        }
    }

    /// The queue of finished subresource requests: the tab loop drains this
    /// every poll tick and emits one `FromTab::NetworkEvent` per entry.
    pub fn net_events(&self) -> Arc<Mutex<Vec<NetEvent>>> {
        Arc::clone(&self.net_events)
    }

    /// The DevTools console queue (fetch-thread side): drained every poll
    /// tick into `FromTab::ConsoleLog` entries (source `"css"` today).
    pub fn console_events(&self) -> crate::logging::ConsoleQueue {
        Arc::clone(&self.console_events)
    }

    /// The tab's handle on the **private-address rule** (also shared with the
    /// JS host, so `fetch`/XHR and subresources report through one queue and
    /// one de-duplication set).
    pub fn private_guard(&self) -> crate::private_addr::PrivateAddressGuard {
        self.private.clone()
    }

    /// The cell the tab writes the **current document URL** into: it is what
    /// the private-address rule treats as the requesting origin for
    /// subresources. Same `Arc`, so a write is seen by every fetch thread.
    pub fn document_cell(&self) -> Arc<Mutex<Option<Url>>> {
        Arc::clone(&self.document)
    }

    /// The document subresources are attributed to (see [`Self::document_cell`]).
    fn document_url(&self) -> Option<Url> {
        self.document.lock().expect("document cell mutex").clone()
    }

    /// A locally served subresource answered as an instant 200.
    fn report_local(&self, method: &str, url: &Url, status: u16, response_bytes: u64) {
        let mut event = net_ok(method, url, status, response_bytes, 0);
        event.initiator = Some("resource".to_owned());
        self.net_events
            .lock()
            .expect("net events mutex")
            .push(event);
    }

    /// A locally served subresource that failed: `status: None`, reason in
    /// `error`.
    fn report_local_failed(&self, method: &str, url: &Url, error: String) {
        let mut event = net_failed(method, url, 0, error);
        event.initiator = Some("resource".to_owned());
        self.net_events
            .lock()
            .expect("net events mutex")
            .push(event);
    }

    /// Ready-to-share (`Arc<dyn NetProvider>`) form for `DocumentConfig`.
    #[allow(dead_code)] // convenience constructor (used by tests/embedders)
    pub fn shared(policy: NetPolicy, jar: CookieJar, cache: NetCache) -> Arc<Self> {
        Arc::new(Self::new(policy, jar, cache))
    }

    /// Builder: poke the tab loop when a fetch settles — Blitz has no error
    /// delivery, so the waker is the only way failed sub-fetches wake us.
    #[must_use]
    pub fn with_waker(self, waker: Arc<dyn NetWaker>) -> Self {
        *self.waker.lock().expect("waker mutex") = Some(waker);
        self
    }

    /// Builder: share one [`FetchWorker`] (and its client) with the runtime.
    #[must_use]
    #[allow(dead_code)] // worker sharing is wired when fetch_document moves in-process
    pub fn with_worker(mut self, worker: Arc<FetchWorker>) -> Self {
        self.worker = worker;
        self
    }

    /// Builder: run this provider's subresources on a pool of `cap` threads
    /// instead of the default [`MAX_SUBRESOURCE_FETCHERS`] (also the seam the
    /// pool tests use to prove the cap holds and that `EAGAIN` on thread
    /// creation degrades rather than kills).
    #[must_use]
    pub fn with_subresource_pool(mut self, cap: usize) -> Self {
        self.subresources = FetchQueue::new(cap);
        self
    }

    /// Builder: share the tab's effective User-Agent cell (the `--user-agent`
    /// flag / `ToTab::SetUserAgent` value) with subresource fetches.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: SharedUserAgent) -> Self {
        self.user_agent = user_agent;
        self
    }

    /// Builder: share the tab's archive-mode sink with subresource fetches —
    /// every delivered 2xx/3xx body is queued for `FromTab::ArchivedFetch`
    /// while the mode is on.
    #[must_use]
    pub fn with_archive(mut self, archive: Arc<crate::archive::ArchiveState>) -> Self {
        self.archive = archive;
        self
    }

    pub fn policy(&self) -> NetPolicy {
        self.policy
    }

    /// Flush the container cookie jar to disk (tab `Destroy` path).
    pub fn save_cookies(&self) -> Result<(), StoreError> {
        self.jar.lock().expect("jar mutex poisoned").save()
    }

    /// The tab's handle on the **same** jar the provider uses for
    /// subresources — one jar per container per process, so a `Set-Cookie`
    /// from a main-document navigation is immediately visible to
    /// subresources (and vice versa), and persistence cannot overwrite one
    /// handle's cookies with the other's.
    pub fn jar(&self) -> Arc<Mutex<CookieJar>> {
        Arc::clone(&self.jar)
    }

    /// The tab's handle on the **same** [`NetCache`] the provider serves
    /// subresources from — one cache per tab, so a document (or an external
    /// `<script src>`) and the images/CSS/fonts of the same page share hits,
    /// validators and LRU bounds instead of keeping two private copies.
    pub fn cache_handle(&self) -> Arc<Mutex<NetCache>> {
        Arc::clone(&self.cache)
    }

    #[allow(dead_code)] // waker fires via the NetWaker installed by with_waker
    fn wake(&self, doc_id: usize) {
        let guard = self.waker.lock().expect("waker mutex poisoned");
        if let Some(waker) = guard.as_ref() {
            waker.wake(doc_id);
        }
    }

    fn spawn_http(&self, doc_id: usize, url: Url, request: Request, handler: Box<dyn NetHandler>) {
        let worker = Arc::clone(&self.worker);
        let managed = self.policy == NetPolicy::Managed;
        let jar = Arc::clone(&self.jar);
        let cache = Arc::clone(&self.cache);
        let waker = self.waker.lock().expect("waker mutex poisoned").clone();
        let events = Arc::clone(&self.net_events);
        let console = Arc::clone(&self.console_events);
        let user_agent = Arc::clone(&self.user_agent);
        let archive = Arc::clone(&self.archive);
        debug!(
            "subresource GET {url} (doc {doc_id}, {})",
            if managed { "managed" } else { "passthrough" }
        );
        let job: FetchJob = Box::new(move || {
            let signal = request.signal.clone();
            // A load Blitz has already cancelled (the document it belonged to
            // is gone) must not sit in the pool queue taking a slot from live
            // loads: settle it and get out of the way.
            if signal.as_ref().is_some_and(|s| s.aborted()) {
                trace!("subresource {url} was aborted before it fetched");
                if let Some(waker) = waker {
                    waker.wake(doc_id);
                }
                deliver(None, String::new(), Vec::new());
                return;
            }
            // Every settled request lands in the tab loop's DevTools queue
            // (redirect hops report individually — same rule as documents).
            // These are Blitz's subresource loads (images, CSS, fonts, …),
            // so every event is tagged with the `"resource"` initiator.
            let report = |mut event: NetEvent| {
                event.initiator = Some("resource".to_owned());
                events.lock().expect("net events mutex").push(event);
            };
            let outcome = if managed {
                // No lock rides along with the fetch: the jar and the cache
                // are shared `Mutex`es and the fetch takes each one only for
                // the instant of its own map operation (see [`JarAccess`],
                // [`CacheAccess`]), so every subresource of a page really
                // does download in parallel.
                let mut jar: &Mutex<CookieJar> = &jar;
                let cache: &Mutex<NetCache> = &cache;
                managed_http_fetch_report(
                    &worker.managed_client(),
                    &url,
                    &mut jar,
                    &cache,
                    now_unix(),
                    None,
                    None,
                    true,
                    &read_user_agent(&user_agent),
                    Some(&report),
                )
            } else {
                plain_http_fetch_report(
                    &worker.client(),
                    &url,
                    Some(&request.headers),
                    None,
                    &read_user_agent(&user_agent),
                    Some(&report),
                )
            };
            // Same as upstream blitz-net: wake the doc thread even on error,
            // and drop the handler when the fetch failed (no error path).
            if let Some(waker) = waker {
                waker.wake(doc_id);
            }
            let aborted = signal.as_ref().is_some_and(|s| s.aborted());
            match outcome {
                Ok(o) if !aborted && o.status < 400 => {
                    debug!(
                        "subresource {} -> {} delivered ({} B)",
                        url,
                        o.status,
                        o.body.len()
                    );
                    // DevTools console seam: a `.css` response that is not
                    // `text/css` is the one CSS problem this side of blitz
                    // can see (stylesheet parse errors are invisible —
                    // src/logging.rs documents the gap).
                    if let Some(entry) =
                        crate::logging::css_content_type_warning(&o.final_url, o.status, &o.headers)
                    {
                        console.lock().expect("console events mutex").push(entry);
                    }
                    // Archive mode: the delivered body is shipped to the shell
                    // (no-op while off; never for local schemes).
                    archive.store(&o.final_url, false, &o.body);
                    deliver(Some(handler), o.final_url.to_string(), o.body)
                }
                Ok(o) if aborted => {
                    trace!("subresource {url} aborted (status {})", o.status);
                    deliver(None, String::new(), Vec::new())
                }
                Ok(o) => {
                    warn!("subresource {url} -> HTTP {} (not delivered)", o.status);
                    deliver(None, String::new(), Vec::new())
                }
                Err(message) => {
                    warn!("subresource {url} failed: {message}");
                    deliver(None, String::new(), Vec::new())
                }
            }
        });
        // Queued, not spawned: however many subresources this page declares,
        // at most `MAX_SUBRESOURCE_FETCHERS` of them are ever on the wire, and
        // a thread-limit `EAGAIN` degrades instead of panicking the tab.
        self.subresources.submit(job);
    }
}

impl NetProvider for BlitzNetProvider {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.clone();
        // AUDIT F5: a page on the web may not read the browser's own network.
        // Refused before the request is built — nothing goes on the wire — and
        // announced once on the DevTools console (see `crate::private_addr`).
        if let Err(reason) = self.private.check(self.document_url().as_ref(), &url, "subresource") {
            self.report_local_failed("GET", &url, reason);
            deliver(None, String::new(), Vec::new());
            return;
        }
        match url.scheme() {
            // Local schemes complete inline — the doc thread will resolve again.
            "data" => match decode_data_url(&url) {
                Some(bytes) => {
                    debug!(
                        "subresource data: URL decoded ({} B) for doc {doc_id}",
                        bytes.len()
                    );
                    deliver(Some(handler), url.to_string(), bytes)
                }
                None => {
                    warn!("subresource {} could not be decoded", url_for_log(&url));
                    deliver(None, String::new(), Vec::new())
                }
            },
            // Local schemes report instant 200s (or the read failure) so the
            // DevTools panel shows the subresource; `data:` URLs stay silent
            // — an inline asset is no request, and pages carry many.
            "file" => match read_file_url(&url) {
                Ok(bytes) => {
                    debug!("subresource {url} read from disk ({} B)", bytes.len());
                    self.report_local("GET", &url, 200, bytes.len() as u64);
                    deliver(Some(handler), url.to_string(), bytes)
                }
                Err(err) => {
                    warn!("subresource {url} unreadable: {err}");
                    self.report_local_failed("GET", &url, err);
                    deliver(None, String::new(), Vec::new())
                }
            },
            "about" => match builtin_page_bytes(&url) {
                Some(bytes) => {
                    debug!(
                        "subresource {url} served from built-ins ({} B)",
                        bytes.len()
                    );
                    self.report_local("GET", &url, 200, bytes.len() as u64);
                    deliver(Some(handler), url.to_string(), bytes)
                }
                None => {
                    debug!("subresource {url}: no built-in page (dropped)");
                    self.report_local_failed(
                        "GET",
                        &url,
                        "no built-in page for this about: URL".to_owned(),
                    );
                    deliver(None, String::new(), Vec::new())
                }
            },
            INTERNAL_SCHEME => match internal_page_bytes(&url) {
                Some(bytes) => {
                    debug!(
                        "subresource {url} served from internal pages ({} B)",
                        bytes.len()
                    );
                    self.report_local("GET", &url, 200, bytes.len() as u64);
                    deliver(Some(handler), url.to_string(), bytes)
                }
                None => {
                    debug!("subresource {url}: no internal page (dropped)");
                    self.report_local_failed(
                        "GET",
                        &url,
                        "no internal page for this ironsurf: URL".to_owned(),
                    );
                    deliver(None, String::new(), Vec::new())
                }
            },
            "http" | "https" => self.spawn_http(doc_id, url, request, handler),
            // javascript:/blob:/…: unsupported (dropped handler).
            other => {
                debug!(
                    "subresource scheme {other} unsupported ({}), dropped",
                    url_for_log(&url)
                );
                deliver(None, String::new(), Vec::new())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main-document fetch (always ours, both policies; task 7c drives this)

/// A `Refresh` directive — the non-standard HTTP `Refresh:` response header
/// and the semantically identical `<meta http-equiv="refresh">` element share
/// this shape (task: Anubis `metarefresh` gates). `url` is already resolved
/// against the base that produced it (the response's final URL, or the
/// document URL for a meta tag), so the caller only has to arm a timer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshSpec {
    /// Seconds to wait before navigating (`0` = immediately).
    pub delay: Duration,
    /// Where to navigate. A directive with no `url=` reloads the same URL.
    pub url: Url,
}

/// Parse a `Refresh` value (`"5"`, `"5; url=/next"`, `"0, url=\"/x\""`): the
/// delay in seconds plus the optional target text, verbatim. Both the standard
/// `;` separator and the legacy comma form are accepted, `url=` is
/// case-insensitive, an optional pair of single/double quotes around the URL
/// is stripped, and anything after the first separator that is not `url=` is
/// taken as the target itself (older generators wrote `"5; /next"`). `None`
/// only for a value that does not start with a number.
pub fn parse_refresh(value: &str) -> Option<(u64, Option<String>)> {
    let value = value.trim();
    let digits = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    if digits == 0 {
        return None;
    }
    let delay: u64 = value[..digits].parse().ok()?;
    let rest = value[digits..].trim_start();
    // The separator is optional: `"5"` alone means "reload this URL".
    let rest = rest
        .strip_prefix(';')
        .or_else(|| rest.strip_prefix(','))
        .map(str::trim_start)
        .unwrap_or(rest);
    if rest.is_empty() {
        return Some((delay, None));
    }
    let target = if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("url=") {
        rest[4..].trim()
    } else {
        rest.trim()
    };
    let target = target
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| {
            target
                .strip_prefix('\'')
                .and_then(|t| t.strip_suffix('\''))
        })
        .unwrap_or(target)
        .trim();
    if target.is_empty() {
        return Some((delay, None));
    }
    Some((delay, Some(target.to_owned())))
}

/// The [`RefreshSpec`] a response's headers ask for, resolved against the URL
/// the response came from (`None` = no `Refresh:` header, or one we cannot
/// parse).
pub fn refresh_from_headers(headers: &[(String, String)], base: &Url) -> Option<RefreshSpec> {
    let raw = headers
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case("refresh"))
        .map(|(_, value)| value.as_str())?;
    let (seconds, target) = parse_refresh(raw)?;
    let url = match target {
        Some(target) => base.join(&target).unwrap_or_else(|_| base.clone()),
        None => base.clone(),
    };
    Some(RefreshSpec {
        delay: Duration::from_secs(seconds),
        url,
    })
}

/// A fetched main document: post-redirect URL + status + raw bytes
/// (`String::from_utf8_lossy` at parse time) + the `Refresh:` directive the
/// response carried, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocFetch {
    pub url: Url,
    pub status: u16,
    pub body: Vec<u8>,
    /// `Refresh:` header (task: Anubis `metarefresh` gates), resolved against
    /// [`Self::url`]. The runtime arms a real timer for it; a cached entry
    /// replays the directive stored with its headers like any browser cache.
    /// The header the fetch layer never sees is `content-disposition`.
    pub refresh: Option<RefreshSpec>,
    /// `Content-Type:` of the response (task: file downloads) — the media type
    /// decides whether this top-level fetch is a page or a file. Local schemes
    /// answer `None` (a `file:` document keeps its current behaviour).
    pub content_type: Option<String>,
    /// `Content-Disposition:` of the response (task: file downloads). `Some`
    /// with an `attachment` disposition makes the fetch a download whatever its
    /// media type; the `filename=` value inside is only a suggestion.
    pub content_disposition: Option<String>,
}

/// [`DocFetch`] from a completed exchange, lifting the `Refresh:` header out of
/// the response headers (used by every http(s) navigation shape).
fn doc_fetch_from(outcome: FetchOutcome) -> DocFetch {
    let refresh = refresh_from_headers(&outcome.headers, &outcome.final_url);
    DocFetch {
        url: outcome.final_url,
        status: outcome.status,
        body: outcome.body,
        refresh,
        content_type: header_value(&outcome.headers, "content-type").map(str::to_owned),
        content_disposition: header_value(&outcome.headers, "content-disposition")
            .map(str::to_owned),
    }
}

/// First value of a named response header, name matched case-insensitively
/// (task: file downloads — `Content-Type:` / `Content-Disposition:`).
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

impl DocFetch {
    /// Body as UTF-8 text (lossy, like every browser's fallback decoding).
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Why a navigation-level fetch could not produce a [`DocFetch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// Managed policy: the `http:` → `https:` upgrade failed, so the
    /// insecure navigation is blocked ([`blocked_error_page`] to render).
    Blocked {
        request: Url,
        https: Url,
        reason: String,
    },
    /// The (already https, or passthrough) fetch failed.
    Transport { url: Url, message: String },
    /// I/O error reading a `file:` URL.
    Io { url: Url, message: String },
    /// Scheme the fetcher never fetches (`javascript:` — the tab runs these as
    /// programs; everything else unsupported).
    Unsupported { url: Url },
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Blocked {
                request,
                https,
                reason,
            } => write!(
                f,
                "blocked insecure navigation {request}: https upgrade to {https} failed ({reason})"
            ),
            NetError::Transport { url, message } => write!(f, "fetch {url}: {message}"),
            NetError::Io { url, message } => write!(f, "read {url}: {message}"),
            NetError::Unsupported { url } => write!(f, "unsupported scheme: {url}"),
        }
    }
}

impl std::error::Error for NetError {}

impl NetError {
    /// Error-page HTML for this failure, if we render one: Blocked, and
    /// any transport failure the proxy is to blame for (message carrying
    /// [`PROXY_FAIL_MARK`] — see [`attribute_send_error`]).
    pub fn error_page(&self) -> Option<String> {
        match self {
            NetError::Blocked {
                request,
                https,
                reason,
            } => Some(blocked_error_page(request, https, reason)),
            NetError::Transport { url, message }
                if message.starts_with(PROXY_FAIL_MARK) =>
            {
                Some(proxy_error_page(url, message))
            }
            _ => None,
        }
    }
}

/// Shared client for main-document fetches (the runtime may also pass its own
/// [`FetchWorker`] to the provider so everything shares one rustls pool).
fn shared_worker() -> Arc<FetchWorker> {
    static WORKER: OnceLock<Arc<FetchWorker>> = OnceLock::new();
    WORKER.get_or_init(FetchWorker::new).clone()
}

/// Fetch a **main document** under `policy` (never via the NetProvider, per
/// ARCHITECTURE). `file:` is read from disk and never upgraded;
/// `about:`/local fixture pages are answered from built-ins; `data:` decodes
/// inline. Both policies send and capture container cookies on the
/// navigation (the jar is consulted per hop through [`managed_http_fetch`],
/// including redirects and redirect chains across origins); `Managed` adds
/// [`NetCache`] service/storage and https-upgrades `http:` navigations —
/// blocking with [`NetError::Blocked`] (carrying the error page) when the
/// upgrade fails.
#[allow(dead_code)] // quiet convenience wrapper (default UA; the live tab reports with its own cell)
pub fn fetch_document(
    url: &Url,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
) -> Result<DocFetch, NetError> {
    fetch_document_post(url, None, policy, jar, cache, now)
}

/// The default effective-UA cell the convenience wrappers below carry (the
/// built-in [`USER_AGENT`]); the live tab always passes its own cell.
fn default_user_agent() -> SharedUserAgent {
    new_user_agent(None)
}

/// [`fetch_document`] with the DevTools reporter attached (see
/// [`fetch_document_post_report`]). The live tab uses
/// [`fetch_document_report_shared`] (the shared jar/cache handles).
#[allow(dead_code)] // quiet convenience wrapper (tests and embedders; the live tab shares)
pub fn fetch_document_report(
    url: &Url,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    fetch_document_post_report(url, None, policy, jar, cache, now, user_agent, report)
}

/// [`fetch_document`] with an HTML form submission's POST body attached
/// (`post = Some(..)`): only http(s) accepts a POST; every local scheme
/// (`file:`/`about:`/`data:`/`ironsurf:`) is answered as GET and refuses a
/// POST body outright, so the caller (the tab's form layer) routes those
/// before ever calling here.
#[allow(dead_code)] // quiet convenience wrapper (default UA; the live tab posts through the report form)
pub fn fetch_document_post(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
) -> Result<DocFetch, NetError> {
    fetch_document_post_report(
        url,
        post,
        policy,
        jar,
        cache,
        now,
        &default_user_agent(),
        None,
    )
}

/// [`fetch_document_post`] with the DevTools reporter attached: one
/// [`NetEvent`] per completed or failed main-document request, including one
/// per followed redirect hop (the redirect chain is followed inside
/// [`managed_http_fetch_report`]). Local-scheme loads (`file:`/`about:`/
/// `ironsurf:`/`data:`) are reported as instant 200s; an https-upgrade that
/// gets blocked is reported against the **requested** URL with the policy
/// reason, and a transport failure against the URL that was tried.
pub fn fetch_document_post_report(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    let worker = shared_worker();
    fetch_document_with_worker_report(url, post, policy, jar, cache, now, &worker, user_agent, report)
}

/// [`fetch_document_post_report`] for a jar and a cache the tab shares with
/// the Blitz subresource provider (`Arc<Mutex<..>>` handles, one per tab).
///
/// The locks are taken **per cookie/cache operation** instead of around the
/// exchange: a main-document load can never serialise the images, stylesheets
/// and fonts that the provider is downloading at the same moment (nor block
/// on them), and every shape of request reads and writes the very same jar
/// and cache entries.
#[allow(clippy::too_many_arguments)]
pub fn fetch_document_post_report_shared(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &Mutex<CookieJar>,
    cache: &Mutex<NetCache>,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    let worker = shared_worker();
    let mut jar: &Mutex<CookieJar> = jar;
    fetch_document_with_worker_report(url, post, policy, &mut jar, cache, now, &worker, user_agent, report)
}

/// [`fetch_document_report`] against the tab's shared (mutex-guarded) jar and
/// cache — the shape the live tab uses for documents and `<script src>`
/// bodies. See [`fetch_document_post_report_shared`].
pub fn fetch_document_report_shared(
    url: &Url,
    policy: NetPolicy,
    jar: &Mutex<CookieJar>,
    cache: &Mutex<NetCache>,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    fetch_document_post_report_shared(url, None, policy, jar, cache, now, user_agent, report)
}

/// The main-document fetch on its own thread (task: faster, responsive
/// loading): the `http(s)` bytes arrive on a worker thread and the outcome
/// returns through the receiver, so the tab loop keeps ticking — painting,
/// answering events, scrolling the page still on screen — while a slow host
/// is mid-response, instead of freezing inside
/// [`fetch_document_post_report_shared`] (which is exactly what this call
/// replaces, and it runs it unchanged, only off-loop). The reporter runs on
/// the worker and its [`NetEvent`]s are collected and returned with the
/// result, so the tab emits them itself on arrival — document fetches still
/// precede every subresource event, exactly as the synchronous path did.
pub fn spawn_document_fetch_post_report_shared(
    url: Url,
    post: Option<MainDocPost>,
    policy: NetPolicy,
    jar: Arc<Mutex<CookieJar>>,
    cache: Arc<Mutex<NetCache>>,
    now: i64,
    user_agent: SharedUserAgent,
) -> Result<
    std::sync::mpsc::Receiver<(Result<DocFetch, NetError>, Vec<NetEvent>)>,
    NetError,
> {
    let (tx, rx) = std::sync::mpsc::channel();
    let err_url = url.clone();
    let spawned = std::thread::Builder::new()
        .name("ironsurf-maindoc".to_owned())
        .spawn(move || {
            let events = Mutex::new(Vec::<NetEvent>::new());
            let res = fetch_document_post_report_shared(
                &url,
                post.as_ref(),
                policy,
                &jar,
                &cache,
                now,
                &user_agent,
                Some(&|mut event: NetEvent| {
                    event.initiator = Some("document".to_owned());
                    events.lock().expect("doc net events mutex").push(event);
                }),
            );
            let events = events
                .lock()
                .expect("doc net events mutex")
                .drain(..)
                .collect();
            let _ = tx.send((res, events));
        });
    match spawned {
        Ok(_) => Ok(rx),
        Err(e) => Err(NetError::Transport {
            url: err_url,
            message: format!("cannot start document fetch thread: {e}"),
        }),
    }
}

#[allow(dead_code)] // quiet convenience wrapper (tests drive it; the runtime reports)
fn fetch_document_with_worker(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    worker: &FetchWorker,
) -> Result<DocFetch, NetError> {
    fetch_document_with_worker_report(
        url,
        post,
        policy,
        jar,
        cache,
        now,
        worker,
        &default_user_agent(),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn fetch_document_with_worker_report<J, C>(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut J,
    cache: &C,
    now: i64,
    worker: &FetchWorker,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError>
where
    J: JarAccess + ?Sized,
    C: CacheAccess + ?Sized
{
    // Snapshot the effective UA once per document load: a mid-load
    // `SetUserAgent` applies to the NEXT request, never mid-flight.
    let user_agent = read_user_agent(user_agent);
    if post.is_some() && !is_http_scheme(url) {
        // Defensive: a body to a local scheme has nowhere to go.
        warn!("refusing to POST to non-http scheme: {url}");
        return Err(NetError::Unsupported { url: url.clone() });
    }
    // Local schemes: identical under both policies. Reported as instant
    // 200s — the DevTools network panel lists the document either way.
    let local_ok = |url: &Url, body: Vec<u8>| {
        if let Some(report) = report {
            report(net_ok("GET", url, 200, body.len() as u64, 0));
        }
        DocFetch {
            url: url.clone(),
            status: 200,
            body,
            refresh: None,
            // A local page is never a download: no server spoke, so there is
            // no disposition and no media-type decision to make (task: file
            // downloads).
            content_type: None,
            content_disposition: None,
        }
    };
    match url.scheme() {
        "file" => {
            let body = read_file_url(url).map_err(|e| {
                warn!("cannot read {url}: {e}");
                if let Some(report) = report {
                    report(net_failed("GET", url, 0, e.clone()));
                }
                NetError::Io {
                    url: url.clone(),
                    message: e,
                }
            })?;
            debug!("document {url} read from disk ({} B)", body.len());
            return Ok(local_ok(url, body));
        }
        "about" => {
            if let Some(body) = builtin_page_bytes(url) {
                debug!("document {url} served from built-ins ({} B)", body.len());
                return Ok(local_ok(url, body));
            }
        }
        INTERNAL_SCHEME => {
            // Always local: no network, no cookies, no https-upgrade — whatever
            // the page (welcome or not-found), it is answered from built-ins.
            if let Some(body) = internal_page_bytes(url) {
                debug!("document {url} served from internal pages ({} B)", body.len());
                return Ok(local_ok(url, body));
            }
        }
        "data" => {
            if let Some(body) = decode_data_url(url) {
                debug!("document {} decoded ({} B)", url_for_log(url), body.len());
                return Ok(local_ok(url, body));
            }
        }
        _ => {}
    }

    if !is_http_scheme(url) {
        warn!("refusing to navigate to unsupported scheme: {url}");
        return Err(NetError::Unsupported { url: url.clone() });
    }

    // The jar always applies to top-level loads; only the cache follows the
    // managed policy.
    let cacheable = policy == NetPolicy::Managed;
    match plan_document(policy, url) {
        // Non-http scheme that still reached here (unknown scheme): refuse.
        Plan::Pass => {
            plain_http_fetch_report(&worker.client(), url, None, post, &user_agent, report)
                .map(doc_fetch_from)
                .map_err(|message| NetError::Transport {
                    url: url.clone(),
                    message,
                })
        }
        Plan::Fetch(target) => managed_http_fetch_report(
            &worker.managed_client(),
            &target,
            jar,
            cache,
            now,
            None,
            post,
            cacheable,
            &user_agent,
            report,
        )
        .map(doc_fetch_from)
        .map_err(|message| NetError::Transport {
            url: url.clone(),
            message,
        }),
        Plan::Upgrade(https) => {
            debug!("https-upgrade: {url} -> {https}");
            // The upgraded navigation is reported as exactly ONE event named
            // by the URL the user asked for: success carries the response the
            // https attempt produced, failure names the policy block (the raw
            // connect error would only name the upgraded URL, which the user
            // never asked for). `Managed` `https:` loads and Passthrough
            // navigations keep the full per-hop reporting above.
            let started = std::time::Instant::now();
            let method = if post.is_some() { "POST" } else { "GET" };
            match managed_http_fetch_report(
                &worker.managed_client(),
                &https,
                jar,
                cache,
                now,
                None,
                post,
                true,
                &user_agent,
                None,
            ) {
                Ok(o) => {
                    if let Some(report) = report {
                        report(net_ok(
                            method,
                            &o.final_url,
                            o.status,
                            o.body.len() as u64,
                            elapsed_ms(started),
                        ));
                    }
                    Ok(doc_fetch_from(o))
                }
                Err(reason) => {
                    warn!("https upgrade for {url} failed, navigation blocked: {reason}");
                    if let Some(report) = report {
                        report(net_failed(
                            method,
                            url,
                            elapsed_ms(started),
                            format!("https-upgrade to {https} blocked: {reason}"),
                        ));
                    }
                    Err(NetError::Blocked {
                        request: url.clone(),
                        https,
                        reason,
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Policy unit tests (no live network — every rule is a pure function)

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url")
    }

    fn cached(etag: Option<&str>, last_modified: Option<&str>) -> CachedEntry {
        CachedEntry {
            status: 200,
            headers: Vec::new(),
            body: b"cached body".to_vec(),
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            cached_at: 100,
        }
    }

    #[test]
    fn document_plan_fetches_under_both_policies_never_without_the_jar() {
        // The whole point: a navigation is never "Pass"ed to a cookie-less
        // plain fetch. Passthrough fetches the URL as asked (jar attached);
        // Managed upgrades http: first.
        assert_eq!(
            plan_document(NetPolicy::Passthrough, &url("https://example.com")),
            Plan::Fetch(url("https://example.com"))
        );
        assert_eq!(
            plan_document(NetPolicy::Passthrough, &url("http://example.com/x")),
            Plan::Fetch(url("http://example.com/x"))
        );
        assert_eq!(
            plan_document(NetPolicy::Managed, &url("https://example.com/page")),
            Plan::Fetch(url("https://example.com/page"))
        );
        assert_eq!(
            plan_document(NetPolicy::Managed, &url("http://example.com/page")),
            Plan::Upgrade(url("https://example.com/page"))
        );
    }

    #[test]
    fn document_plan_passes_non_http_schemes() {
        for scheme_url in [
            "file:///tmp/x/index.html",
            "about:blank",
            "data:text/html,x",
            "ironsurf://welcome",
        ] {
            assert_eq!(
                plan_document(NetPolicy::Managed, &url(scheme_url)),
                Plan::Pass,
                "{scheme_url} must pass through"
            );
            assert_eq!(
                plan_document(NetPolicy::Passthrough, &url(scheme_url)),
                Plan::Pass,
                "{scheme_url} must pass through"
            );
        }
    }

    #[test]
    fn https_upgrade_folding() {
        assert_eq!(
            upgrade_to_https(&url("http://example.com/"))
                .unwrap()
                .as_str(),
            "https://example.com/"
        );
        // The insecure default port is folded away…
        assert_eq!(
            upgrade_to_https(&url("http://example.com:80/x"))
                .unwrap()
                .as_str(),
            "https://example.com/x"
        );
        // …but a real port survives.
        assert_eq!(
            upgrade_to_https(&url("http://example.com:8080/x"))
                .unwrap()
                .as_str(),
            "https://example.com:8080/x"
        );
        assert!(upgrade_to_https(&url("https://example.com")).is_none());
    }

    #[test]
    fn refresh_values_parse_like_browsers_do() {
        assert_eq!(parse_refresh("5"), Some((5, None)));
        assert_eq!(parse_refresh("0; url=/next"), Some((0, Some("/next".into()))));
        assert_eq!(parse_refresh("2, url=/x"), Some((2, Some("/x".into()))));
        // Quoted target keeps its own `;`/`,` — the separator split happens first.
        assert_eq!(
            parse_refresh("7; URL=\"/q?a=1;b=2\""),
            Some((7, Some("/q?a=1;b=2".into())))
        );
        // Legacy generator shape: separator then a bare URL.
        assert_eq!(parse_refresh("1; /legacy"), Some((1, Some("/legacy".into()))));
        // Anubis `metarefresh`'s exact header.
        assert_eq!(
            parse_refresh("2; url=/.within.website/x/cmd/anubis/api/pass-challenge?challenge=abc&id=1"),
            Some((
                2,
                Some("/.within.website/x/cmd/anubis/api/pass-challenge?challenge=abc&id=1".into())
            ))
        );
        assert_eq!(parse_refresh("nope; url=/x"), None);
        assert_eq!(parse_refresh("  "), None);
    }

    #[test]
    fn refresh_header_resolves_against_the_response_url() {
        let headers = vec![
            ("cache-control".to_owned(), "no-store".to_owned()),
            ("Refresh".to_owned(), "2; url=/pass".to_owned()),
        ];
        let spec = refresh_from_headers(&headers, &url("https://gate.example/g/")).unwrap();
        assert_eq!(spec.delay, Duration::from_secs(2));
        assert_eq!(spec.url.as_str(), "https://gate.example/pass");
        // No header (or an unparseable one) → no directive.
        assert!(refresh_from_headers(&[], &url("https://a/")).is_none());
        assert!(refresh_from_headers(
            &[("refresh".to_owned(), "soon".to_owned())],
            &url("https://a/")
        )
        .is_none());
        // A target-less directive reloads the response URL.
        let reload =
            refresh_from_headers(&[("refresh".to_owned(), "3".to_owned())], &url("https://a/b"))
                .unwrap();
        assert_eq!(reload.url.as_str(), "https://a/b");
    }

    #[test]
    fn revalidation_serves_cached_only_on_matching_validators() {
        // 304 + matching ETag → cached.
        assert_eq!(
            revalidate(&cached(Some("W\"1\""), None), 304, Some("W\"1\""), None),
            Revalidate::ServeCached
        );
        // 304 + matching Last-Modified → cached.
        assert_eq!(
            revalidate(
                &cached(None, Some("Tue, 1 Jul 2025 00:00:00 GMT")),
                304,
                None,
                Some("Tue, 1 Jul 2025 00:00:00 GMT")
            ),
            Revalidate::ServeCached
        );
        // 304 without any validators → fresh.
        match revalidate(&cached(None, None), 304, None, None) {
            Revalidate::UseFresh { status, .. } => assert_eq!(status, 304),
            other => panic!("expected UseFresh, got {other:?}"),
        }
        // 200 always wins over the cache.
        assert!(matches!(
            revalidate(&cached(Some("a"), None), 200, Some("b"), None),
            Revalidate::UseFresh { status: 200, .. }
        ));
    }

    #[test]
    fn forwarded_headers_drop_hop_by_hop_and_transformed_body_headers() {
        let headers = vec![
            ("content-type".into(), "text/html".into()),
            ("Set-Cookie".into(), "a=1".into()),
            ("content-encoding".into(), "gzip".into()),
            ("Content-Length".into(), "9".into()),
            ("connection".into(), "keep-alive".into()),
            ("etag".into(), "\"v1\"".into()),
        ];
        let kept = forwarded_response_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["content-type", "etag"]);
    }

    #[test]
    fn managed_cookie_rules_over_the_container_jar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(9),
            dir.path(),
        );
        let mut jar = store.cookie_jar();
        let now = 1_700_000_000;

        // Secure cookie set over https is stored and only sent over https.
        capture_set_cookies(
            &mut jar,
            &url("https://example.com/"),
            ["sid=42; Secure; Path=/"],
            now,
        );
        assert_eq!(jar.cookies().len(), 1);
        assert_eq!(
            jar.cookie_header(&url("https://example.com/"), now)
                .as_deref(),
            Some("sid=42")
        );
        assert_eq!(
            jar.cookie_header(&url("http://example.com/"), now),
            None,
            "Secure cookies must never be sent over http"
        );

        // A Secure cookie may not be *set* over http at all.
        capture_set_cookies(
            &mut jar,
            &url("http://example.org/"),
            ["steal=1; Secure; Path=/"],
            now,
        );
        assert_eq!(jar.cookies().len(), 1, "http may not set Secure cookies");

        // Host-only cookie does not leak to subdomains; domain cookie does.
        capture_set_cookies(
            &mut jar,
            &url("https://a.example.com/"),
            ["h=1; Path=/", "shared=2; Domain=example.com; Path=/"],
            now,
        );
        let header = jar
            .cookie_header(&url("https://b.example.com/"), now)
            .expect("header");
        assert!(header.contains("shared=2"));
        assert!(
            !header.contains("h=1"),
            "host-only cookie leaked to subdomain"
        );

        // Persistence round-trip: exactly what the tab does on Destroy.
        jar.save().expect("save jar");
        let reloaded = store.cookie_jar();
        assert_eq!(reloaded.cookies().len(), 3);
        assert!(reloaded
            .cookies()
            .iter()
            .any(|c| c.name == "sid" && c.value == "42" && c.secure));
    }

    #[test]
    fn blocked_page_mentions_both_urls() {
        let page = blocked_error_page(
            &url("http://example.com/"),
            &url("https://example.com/"),
            "boom",
        );
        assert!(page.contains("http://example.com/"));
        assert!(page.contains("https://example.com/"));
        assert!(page.contains("boom"));
    }

    // -- task 7b: Blitz NetProvider + fetch_document --------------------------

    /// Test-only `NetHandler` recording the single callback the trait has.
    struct MockHandler {
        log: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    }

    impl NetHandler for MockHandler {
        fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
            *self.log.lock().unwrap() = vec![(resolved_url, bytes.to_vec())];
        }
    }

    fn mock_handler() -> (Box<dyn NetHandler>, Arc<Mutex<Vec<(String, Vec<u8>)>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(MockHandler {
                log: Arc::clone(&log),
            }),
            log,
        )
    }

    fn wait_for(log: &Arc<Mutex<Vec<(String, Vec<u8>)>>>) -> Vec<(String, Vec<u8>)> {
        for _ in 0..400 {
            {
                let guard = log.lock().unwrap();
                if !guard.is_empty() {
                    return guard.clone();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("no handler.bytes() delivery within 10s");
    }

    #[test]
    fn net_provider_is_send_sync_and_not_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let provider =
            BlitzNetProvider::shared(NetPolicy::Managed, store.cookie_jar(), store.cache());
        fn assert_net_provider(_: &Arc<dyn NetProvider>) {}
        let dyn_provider: Arc<dyn NetProvider> = provider;
        assert_net_provider(&dyn_provider);
        assert!(
            !dyn_provider.is_noop(),
            "our provider must never report noop (else Blitz blocks critical resources)"
        );
    }

    #[test]
    fn provider_delivers_file_and_data_schemes_to_handler() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let provider = BlitzNetProvider::new(NetPolicy::Managed, store.cookie_jar(), store.cache());

        // file: sub-resource (e.g. an <img> from a file:// fixture page).
        let fixture = dir.path().join("sub.txt");
        std::fs::write(&fixture, b"file bytes").expect("write fixture");
        let file_url = Url::from_file_path(&fixture).expect("file url");
        let (handler, log) = mock_handler();
        provider.fetch(1, Request::get(file_url.clone()), handler);
        let got = wait_for(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, file_url.as_str());
        assert_eq!(got[0].1, b"file bytes");

        // data: — percent-encoded and base64 payloads.
        let (handler, log) = mock_handler();
        provider.fetch(1, Request::get(url("data:text/html,hi%21")), handler);
        let got = wait_for(&log);
        assert_eq!(got[0].1, b"hi!");

        let (handler, log) = mock_handler();
        provider.fetch(
            1,
            Request::get(url("data:text/plain;base64,aGVsbG8=")),
            handler,
        );
        let got = wait_for(&log);
        assert_eq!(got[0].1, b"hello");
    }

    #[test]
    fn about_urls_answer_from_builtins() {
        let blank = url("about:blank");
        let bytes = builtin_page_bytes(&blank).expect("blank page");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("about:blank"));

        let about = url("about:ironsurf");
        let text =
            String::from_utf8(builtin_page_bytes(&about).expect("ironsurf page")).expect("utf8");
        assert!(text.contains("IronSurf"));

        assert!(builtin_page_bytes(&url("about:whatever")).is_none());
    }

    #[test]
    fn ironsurf_scheme_names_pages_locally() {
        // The default start page is the internal welcome page.
        assert_eq!(ironsurf_common::default_url(), "ironsurf://welcome");
        let welcome = url(&ironsurf_common::default_url());
        assert_eq!(welcome.scheme(), INTERNAL_SCHEME);
        assert_eq!(internal_page_name(&welcome), "welcome");
        // Authority vs path placement of non-special schemes all select welcome.
        assert_eq!(internal_page_name(&url("ironsurf:///welcome")), "welcome");
        assert_eq!(internal_page_name(&url("ironsurf://welcome/")), "welcome");
        assert_eq!(
            internal_page_name(&url("ironsurf://ironsurf/welcome")),
            "welcome"
        );
        assert_eq!(internal_page_name(&url("IRONSURF://Welcome")), "welcome");
        assert_eq!(internal_page_name(&url("ironsurf://about/config")), "about");
        // Not our scheme → not our business.
        assert_eq!(internal_page_name(&url("https://welcome")), "welcome");
        assert!(internal_page_bytes(&url("https://welcome")).is_none());
    }

    #[test]
    fn internal_welcome_page_is_self_contained_and_safe() {
        let bytes = internal_page_bytes(&url("ironsurf://welcome")).expect("welcome page");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("Welcome to IronSurf"));
        // Self-contained: no subresources, no scripts — must render with no network.
        assert!(!text.contains("<script"));
        assert!(!text.contains("src="));
        assert!(!text.contains("http://"));
        // An unknown page name is an honest internal 404 page, not an error…
        let gone = internal_page_bytes(&url("ironsurf://nope")).expect("not-found page");
        let gone = String::from_utf8(gone).expect("utf8");
        assert!(gone.contains("No such internal page"));
        assert!(gone.contains("ironsurf://welcome"));
        // …and the requested URL is escaped, never injected as markup. The URL
        // parser itself rejects raw `<` in a host, and an encoded hostile page
        // name must still produce no live script tag; the escape helper itself
        // is asserted directly since it is the last line of defence.
        assert_eq!(
            escape_html_text("<s\"'>"),
            "&lt;s&quot;&#39;&gt;"
        );
        let xss = internal_page_bytes(&url("ironsurf://%3Cscript%3Ealert%281%29"))
            .expect("not-found for hostile page name");
        let xss = String::from_utf8(xss).expect("utf8");
        assert!(!xss.contains("<script"));
    }

    #[test]
    fn the_welcome_page_documents_the_archive_server() {
        let text =
            String::from_utf8(internal_page_bytes(&url("ironsurf://welcome")).expect("welcome"))
                .expect("utf8");
        assert!(text.contains("Archive server"), "the section is on the start page");
        assert!(
            text.contains("href=\"ironsurf://archive\""),
            "a real link, not just prose"
        );
        assert!(text.contains("ironsurf://archive?open=1"));
        assert!(text.contains("$HOME/Downloads/ironsurf-tabs"));
        // Still self-contained: no new subresources, no scripts, no network.
        assert!(!text.contains("<script"));
        assert!(!text.contains("src="));
        assert!(!text.contains("http://"));
    }

    #[test]
    fn the_archive_page_names_the_shell_and_stays_static() {
        assert!(is_archive_page(&url("ironsurf://archive")));
        assert!(is_archive_page(&url("ironsurf:///archive")));
        assert!(is_archive_page(&url("IRONSURF://Archive?open=1")));
        assert!(!is_archive_page(&url("ironsurf://welcome")));

        // With no answer from the shell yet, the page says so honestly instead
        // of inventing an address.
        set_archive_server_status(None);
        let text =
            String::from_utf8(internal_page_bytes(&url("ironsurf://archive")).expect("archive"))
                .expect("utf8");
        assert!(text.contains("Archive server"));
        assert!(text.contains("Asking the browser"), "{}", &text[..400.min(text.len())]);
        assert!(text.contains("$HOME/Downloads/ironsurf-tabs"));
        assert!(text.contains(ARCHIVE_CONTROL_PATH.trim_start_matches('/')));
        assert!(!text.contains("<script"));
        assert!(!text.contains("src="));

        // Listening: the address is shown and linked.
        set_archive_server_status(Some(ArchiveServerStatus {
            ok: true,
            address: "http://127.0.0.1:41234".into(),
            error: None,
        }));
        let text = String::from_utf8(internal_page_bytes(&url("ironsurf://archive")).expect("x"))
            .expect("utf8");
        assert!(text.contains("http://127.0.0.1:41234"));
        assert!(text.contains("href=\"http://127.0.0.1:41234\""));
        assert!(text.contains("http://127.0.0.1:41234/ironsurf/serve"));

        // Failed: the reason is shown escaped — the shell's error text is data.
        set_archive_server_status(Some(ArchiveServerStatus {
            ok: false,
            address: String::new(),
            error: Some("cannot bind <b>127.0.0.1</b>".into()),
        }));
        let text = String::from_utf8(internal_page_bytes(&url("ironsurf://archive")).expect("x"))
            .expect("utf8");
        assert!(text.contains("not running"));
        assert!(text.contains("cannot bind &lt;b&gt;127.0.0.1&lt;/b&gt;"));
        assert!(!text.contains("<b>"));
        set_archive_server_status(None);
    }

    // One history page is one global: the tab renders the shell's last answer
    // out of a process-wide cell (every tab process has exactly one), so these
    // checks share it in one test rather than racing each other from threads.
    #[test]
    fn the_history_page_renders_what_the_shell_answered() {
        // ── The visits, newest first, one row each ───────────────────────────
        set_history_listing(Some(HistoryListing {
            entries: vec![
                ironsurf_common::HistoryVisit {
                    url: "https://example.org/two".into(),
                    title: Some("Second <page>".into()),
                    at: 1_700_000_200,
                },
                ironsurf_common::HistoryVisit {
                    url: "https://example.org/one".into(),
                    title: None,
                    at: 1_700_000_100,
                },
            ],
            total: 2,
        }));
        assert!(is_history_page(&url(HISTORY_URL)));
        assert_eq!(internal_page_name(&url(HISTORY_URL)), HISTORY_PAGE);
        let page = || {
            String::from_utf8(internal_page_bytes(&url(HISTORY_URL)).expect("history"))
                .expect("utf8")
        };
        let text = page();
        assert!(text.contains("<title>ironsurf: history</title>"));
        assert!(text.contains("<h1>History</h1>"));
        assert!(text.contains("2 visits shared by every container"));
        assert!(text.contains("href=\"https://example.org/two\""));
        assert!(
            text.find("example.org/two") < text.find("example.org/one"),
            "the visit log reads newest first, as the settings drawer's list did"
        );
        assert_eq!(text.matches("<span class=\"when\">").count(), 2, "every row says when");
        // A title is page data, so it is escaped; a visit without one is
        // labelled with its own address rather than left blank.
        assert!(text.contains("Second &lt;page&gt;"));
        assert!(!text.contains("Second <page>"));
        assert!(text.find(">https://example.org/one</a>").is_some());
        // Clearing the log is a link back to this page with a query.
        assert!(text.contains(&format!("href=\"{HISTORY_URL}?clear=1\"")));
        // No scripts and no subresources, like every internal page.
        assert!(!text.contains("<script"));
        assert!(!text.contains("src="));

        // ── No answer from the shell yet: it says so, and invents no rows ─────
        set_history_listing(None);
        let text = page();
        assert!(text.contains("Asking the browser for the visit log"));
        assert!(text.contains("<li class=\"wait\">One moment"));

        // ── An empty log says so plainly, in the count and in the list ────────
        set_history_listing(Some(HistoryListing { entries: vec![], total: 0 }));
        let text = page();
        assert!(text.contains("No browsing history yet"));
        assert!(text.contains("Nothing yet."));
        assert!(!text.contains("<li><a href="), "no rows to show");

        // ── A list the shell cut short admits how much it left out ────────────
        set_history_listing(Some(HistoryListing {
            entries: vec![ironsurf_common::HistoryVisit {
                url: "https://example.org/".into(),
                title: Some("Example".into()),
                at: 1_700_000_000,
            }],
            total: 9,
        }));
        let text = page();
        assert!(text.contains("9 visits shared by every container"));
        assert!(text.contains("the newest 1 listed"));

        // The clear link comes back to this same page: the query says the row
        // was Clear, it never turns the page into another page.
        assert_eq!(internal_page_name(&url(&format!("{HISTORY_URL}?clear=1"))), HISTORY_PAGE);
        assert!(is_history_page(&url(&format!("{HISTORY_URL}?clear=1"))));
        set_history_listing(None);
    }

    #[test]
    fn fetch_document_file_and_about_without_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let mut jar = store.cookie_jar();
        let cache = store.cache();
        let now = 1_700_000_000;

        // file:// documents load under BOTH policies (never upgraded).
        let fixture = dir.path().join("index.html");
        std::fs::write(&fixture, b"<html><body>fixture</body></html>").expect("write");
        let file_url = Url::from_file_path(&fixture).expect("file url");
        for policy in [NetPolicy::Passthrough, NetPolicy::Managed] {
            let doc = fetch_document_with_worker(
                &file_url,
                None,
                policy,
                &mut jar,
                &cache,
                now,
                worker_for_test(),
            )
            .expect("file document");
            assert_eq!(doc.status, 200);
            assert_eq!(doc.url, file_url);
            assert!(doc.body_text().contains("fixture"));
        }

        // about: built-ins.
        let doc = fetch_document_with_worker(
            &url("about:ironsurf"),
            None,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect("about page");
        assert!(doc.body_text().contains("IronSurf"));

        // ironsurf:// built-ins: answered locally under Managed too — never
        // upgraded, never on the wire, URL round-trips for UrlChanged.
        let doc = fetch_document_with_worker(
            &url("ironsurf://welcome"),
            None,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect("internal welcome page");
        assert_eq!(doc.status, 200);
        assert_eq!(doc.url.as_str(), "ironsurf://welcome");
        assert!(doc.body_text().contains("Welcome to IronSurf"));

        // Unsupported schemes are refused, not fetched.
        let err = fetch_document_with_worker(
            &url("javascript:alert(1)"),
            None,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect_err("javascript: must be refused");
        assert!(matches!(err, NetError::Unsupported { .. }));
    }

    #[test]
    fn managed_document_https_upgrade_failure_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let mut jar = store.cookie_jar();
        let cache = store.cache();
        // Port 1 on loopback: connect is refused instantly, no real network.
        let http = url("http://127.0.0.1:1/");
        let err = fetch_document_with_worker(
            &http,
            None,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            1_700_000_000,
            worker_for_test(),
        )
        .expect_err("upgraded fetch must fail (connection refused)");
        let blocked = match err {
            NetError::Blocked {
                request,
                https,
                reason,
            } => {
                assert_eq!(request, http);
                assert_eq!(https, url("https://127.0.0.1:1/"));
                assert!(!reason.is_empty());
                NetError::Blocked {
                    request,
                    https,
                    reason,
                }
            }
            other => panic!("expected Blocked, got {other:?}"),
        };
        let page = blocked.error_page().expect("blocked error page");
        assert!(page.contains("http://127.0.0.1:1/"));
        assert!(page.contains("https://127.0.0.1:1/"));

        // Passthrough to the same dead port is a plain transport error.
        let err = fetch_document_with_worker(
            &http,
            None,
            NetPolicy::Passthrough,
            &mut jar,
            &cache,
            1_700_000_000,
            worker_for_test(),
        )
        .expect_err("transport failure surfaces");
        assert!(matches!(err, NetError::Transport { .. }));
    }

    #[test]
    fn managed_cookie_header_shape_including_secure_only_over_https() {
        let mut jar = CookieJar::new();
        let now = 1_700_000_000;
        capture_set_cookies(
            &mut jar,
            &url("https://example.com/"),
            ["b=2; Path=/; Secure", "a=1; Path=/"],
            now,
        );
        // The header the managed fetcher would attach (jar-side assembly).
        let https_header = jar
            .cookie_header(&url("https://example.com/path"), now)
            .expect("https header");
        let pairs: std::collections::BTreeSet<&str> = https_header.split("; ").collect();
        assert_eq!(
            pairs,
            std::collections::BTreeSet::from(["a=1", "b=2"]),
            "unexpected assembled header {https_header:?}"
        );
        assert!(
            !https_header.contains(','),
            "pairs are '; '-joined, not comma"
        );
        // Over http the Secure cookie must drop out of the assembled header.
        let http_header = jar
            .cookie_header(&url("http://example.com/path"), now)
            .expect("http header");
        assert_eq!(http_header, "a=1");
    }

    #[test]
    fn netcache_etag_round_trip_decides_revalidation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let cache = store.cache();
        let now = 1_700_000_000;
        let target = "https://example.com/page";
        let headers = vec![
            ("content-type".to_string(), "text/html".to_string()),
            ("ETag".to_string(), "\"v7\"".to_string()),
        ];
        cache.store(target, 200, &headers, b"fresh body".to_vec(), now);

        let entry = cache.lookup(target).expect("stored entry");
        assert_eq!(entry.status, 200);
        assert_eq!(entry.body, b"fresh body");
        // Revalidation: 304 + same ETag → ServeCached (this is exactly the
        // decision managed_http_fetch makes from these values).
        let (etag, lm) = validators(&entry.headers);
        assert_eq!(etag.as_deref(), Some("\"v7\""));
        assert!(lm.is_none());
        assert_eq!(
            revalidate(&entry, 304, Some("\"v7\""), None),
            Revalidate::ServeCached
        );
        // A different ETag → fresh body replaces the cache entry.
        assert!(matches!(
            revalidate(&entry, 304, Some("\"v8\""), None),
            Revalidate::UseFresh { status: 304, .. }
        ));
    }

    #[test]
    fn data_url_decode_helpers() {
        assert_eq!(
            decode_base64("aGVsbG8=").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            decode_base64("aGVsbG8").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(decode_base64("").as_deref(), Some(b"".as_slice()));
        assert_eq!(
            decode_base64("////").as_deref(),
            Some([0xff, 0xff, 0xff].as_slice())
        );
        // URL-safe alphabet maps to the same values as the standard one.
        assert_eq!(
            decode_base64("____").as_deref(),
            Some([0xff, 0xff, 0xff].as_slice())
        );
        assert_eq!(decode_base64("__8="), decode_base64("//8="));
        assert_eq!(
            decode_base64("TWFuw6c=").as_deref(),
            Some("Man\u{E7}".as_bytes())
        );
        assert_eq!(
            decode_base64("TWF"), // truncated partial group must still decode to "Ma"
            decode_base64("TWF=")
        );
        assert_eq!(decode_base64("a!b"), None);
        assert_eq!(
            decode_data_url(&url("data:text/html,%3Cb%3Ehi")).as_deref(),
            Some(b"<b>hi".as_slice())
        );
        assert_eq!(
            decode_data_url(&url("data:text/html;charset=utf-8,x")),
            Some(b"x".to_vec())
        );
        assert_eq!(decode_data_url(&url("data:text/html")), None);
    }

    #[test]
    fn base64_encoder_round_trips_with_the_decoder() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"h"), "aA==");
        assert_eq!(encode_base64(b"he"), "aGU=");
        assert_eq!(encode_base64(b"hel"), "aGVs");
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64(&[0xff, 0xff, 0xff]), "////");
        // Every byte value survives the round trip.
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(
            decode_base64(&encode_base64(&all)).as_deref(),
            Some(all.as_slice())
        );
        // The canonical JS payload used by the data: tests.
        assert_eq!(
            encode_base64("console.log(\"hi\"); document.title = \"js\";".as_bytes()),
            JS_B64
        );
    }

    /// `data:text/javascript;base64,…` is the shape real pages use for inline
    /// program text: `console.log("hi"); document.title = "js";`.
    const JS_B64: &str = "Y29uc29sZS5sb2coImhpIik7IGRvY3VtZW50LnRpdGxlID0gImpzIjs=";

    #[test]
    fn data_url_javascript_base64_parses() {
        let data =
            parse_data_url(&url(&format!("data:text/javascript;base64,{JS_B64}"))).expect("parses");
        assert_eq!(data.mime, "text/javascript");
        assert_eq!(data.charset.as_deref(), None);
        assert!(data.base64);
        assert_eq!(data.text(), "console.log(\"hi\"); document.title = \"js\";");
        assert!(data.is_javascript());
        assert!(!data.is_html());
        // A program is displayed, never parsed/evaluated as a document.
        assert!(data.renders_as_source());
    }

    #[test]
    fn data_url_media_types_and_parameters() {
        // Uppercase scheme/parameters are normalised; `;base64` may follow a
        // charset parameter.
        let data = parse_data_url(&url(
            "data:Text/JavaScript;charset=UTF-8;BASE64,Y29uc29sZS5sb2coMSk=",
        ))
        .expect("parses");
        assert_eq!(data.mime, "text/javascript");
        assert_eq!(data.charset.as_deref(), Some("utf-8"));
        assert!(data.base64);
        assert_eq!(data.text(), "console.log(1)");

        // No media type at all ⇒ text/plain (percent-encoded), still source.
        let plain = parse_data_url(&url("data:,a%20b")).expect("parses");
        assert_eq!(plain.mime, "text/plain");
        assert!(!plain.base64);
        assert_eq!(plain.text(), "a b");
        assert!(plain.renders_as_source());

        // Markup keeps going through the parser.
        assert!(parse_data_url(&url("data:text/html,<b>x</b>")).is_some_and(|d| d.is_html()));
        assert!(!parse_data_url(&url("data:text/html,<b>x</b>"))
            .unwrap()
            .renders_as_source());
        assert!(parse_data_url(&url("data:application/xhtml+xml,x")).is_some_and(|d| d.is_html()));
        assert!(parse_data_url(&url("data:image/svg+xml,%3Csvg/%3E")).is_some_and(|d| d.is_html()));

        // Other script/stylesheet/JSON flavours are source too.
        for mime in [
            "application/javascript",
            "application/x-javascript",
            "text/ecmascript",
            "text/css",
            "application/json",
            "application/ld+json",
        ] {
            let d = parse_data_url(&url(&format!("data:{mime},x"))).expect("parses");
            assert!(
                d.renders_as_source(),
                "{mime} must render as source, not be parsed as HTML"
            );
        }

        // Binary types are not documents (unchanged behaviour, no sniffing).
        let png = parse_data_url(&url("data:image/png;base64,iVBORw0KGgo=")).expect("parses");
        assert!(!png.renders_as_source());
        assert_eq!(png.bytes, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    }

    #[test]
    fn data_url_parse_rejects_what_decoding_cannot_produce() {
        // Not a data: URL.
        assert_eq!(parse_data_url(&url("http://example.com/x")), None);
        assert_eq!(parse_data_url(&url("file:///tmp/x")), None);
        // No `,` separator.
        assert_eq!(parse_data_url(&url("data:text/javascript;base64")), None);
        // Invalid base64 characters in the payload (`(` is not in the alphabet).
        assert_eq!(
            parse_data_url(&url("data:text/javascript;base64,Y29uc29sZS(bG9nKDEp")),
            None
        );
        assert_eq!(
            parse_data_url(&url("data:text/javascript;base64,a!b")),
            None
        );
        // Empty payload after `,` is legal and decodes to nothing.
        assert_eq!(
            parse_data_url(&url("data:text/javascript;base64,")).map(|d| d.bytes),
            Some(Vec::new())
        );
    }

    #[test]
    fn data_url_round_trips_through_the_provider_and_the_fetcher() {
        let raw = format!("data:text/javascript;base64,{JS_B64}");
        let u = url(&raw);

        // Main-document fetch (what `Tab::navigate` uses).
        let mut jar = CookieJar::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(9),
            dir.path(),
        );
        let cache = store.cache();
        let fetch = fetch_document(&u, NetPolicy::Managed, &mut jar, &cache, now_unix())
            .expect("data: URLs always fetch");
        assert_eq!(fetch.status, 200);
        assert_eq!(fetch.url, u);
        assert_eq!(
            String::from_utf8(fetch.body).expect("utf8"),
            "console.log(\"hi\"); document.title = \"js\";"
        );

        // Subresource fetch (`<script src="data:…">`) delivers the same bytes.
        let provider = BlitzNetProvider::new(NetPolicy::Managed, store.cookie_jar(), store.cache());
        let (handler, log) = mock_handler();
        provider.fetch(1, Request::get(u.clone()), handler);
        let got = wait_for(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, u.as_str());
        assert_eq!(
            String::from_utf8(got[0].1.clone()).expect("utf8"),
            "console.log(\"hi\"); document.title = \"js\";"
        );
    }

    // Small helper: dedicated short-timeout client for connection-refused tests.
    #[allow(clippy::redundant_clone)]
    fn worker_for_test() -> &'static FetchWorker {
        static WORKER: OnceLock<Arc<FetchWorker>> = OnceLock::new();
        WORKER.get_or_init(|| FetchWorker::with_timeout(std::time::Duration::from_secs(5)))
    }

    // -- live localhost HTTP: the cookie goes out and comes back -------------
    //
    // A tiny HTTP/1.0 server on 127.0.0.1 answering fixed routes, used to
    // prove the *wire* behaviour the jar unit tests cannot: `Cookie` headers
    // actually riding on requests, `Set-Cookie` actually coming back, every
    // hop of a redirect chain handled against its own origin.

    enum Route {
        /// 302 to `to` with the listed Set-Cookie headers.
        Redirect { to: String, set_cookie: Vec<String> },
        /// 200 echoing the `Cookie` header it received (`-` when none).
        Echo,
    }

    /// Spawn the test server; returns its port. Serves one request per
    /// connection (HTTP/1.0: close after the response). Dual-stack bind so
    /// BOTH `127.0.0.1` and `localhost` (-> ::1) reach the same server on
    /// the same port — two distinct origin host names for the cross-origin
    /// redirect test.
    fn spawn_local_http_server(routes: Vec<(&str, Route)>) -> u16 {
        let listener = TcpListener::bind(("[::]", 0))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
            .expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let routes: Vec<(String, Route)> = routes
            .into_iter()
            .map(|(p, r)| (p.to_string(), r))
            .collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Read the request head (non-verbose GETs only).
                let mut buf: Vec<u8> = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.push(byte[0]),
                    }
                    if buf.len() > 64 * 1024 {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let cookie = head
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                    .map(|line| line["cookie:".len()..].trim().to_owned())
                    .unwrap_or_default();
                let route = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, r)| match r {
                        Route::Redirect { to, set_cookie } => Route::Redirect {
                            to: to.replace("{port}", &port.to_string()),
                            set_cookie: set_cookie.clone(),
                        },
                        Route::Echo => Route::Echo,
                    });
                let response = match route {
                    Some(Route::Redirect { to, set_cookie }) => {
                        let mut r = format!("HTTP/1.0 302 Found\r\nLocation: {to}\r\n");
                        for value in set_cookie {
                            r.push_str(&format!("Set-Cookie: {value}\r\n"));
                        }
                        r.push_str("Connection: close\r\n\r\n");
                        r
                    }
                    Some(Route::Echo) => {
                        let body = if cookie.is_empty() { "-".into() } else { cookie };
                        format!(
                            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    None => "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    fn test_cache(name: &str) -> NetCache {
        let dir = std::env::temp_dir().join(format!("ironsurf-tab-cookie-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        NetCache::with_limits(dir, 16, 4096)
    }

    fn get(url: &str) -> Url {
        Url::parse(url).expect("test url")
    }

    /// One jar, one worker, both redirect legs: cookies set on earlier hops
    /// must be on the later hops' requests, and vice versa.
    fn fetch(path: &str, port: u16, jar: &mut CookieJar, cache: &NetCache) -> FetchOutcome {
        managed_http_fetch(
            &worker_for_test().managed_client(),
            &get(&format!("http://127.0.0.1:{port}{path}")),
            jar,
            cache,
            now_unix(),
            None,
            None,
            false,
        )
        .expect("localhost fetch")
    }

    #[test]
    fn redirect_chain_cookies_are_captured_and_sent_per_hop() {
        let port = spawn_local_http_server(vec![
            (
                "/multi",
                Route::Redirect {
                    to: "/hop2".into(),
                    set_cookie: vec!["m1=1; Path=/".into()],
                },
            ),
            (
                "/hop2",
                Route::Redirect {
                    to: "/echo".into(),
                    set_cookie: vec!["m2=2; Path=/".into()],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let mut jar = CookieJar::new();
        let out = fetch("/multi", port, &mut jar, &test_cache("chain"));
        let echoed = String::from_utf8_lossy(&out.body).to_string();
        assert_eq!(out.final_url.as_str(), format!("http://127.0.0.1:{port}/echo"));
        // The final hop saw BOTH chain cookies — each captured on its own
        // hop and replayed by the jar on the following one.
        assert!(echoed.contains("m1=1"), "hop-1 cookie missing from {echoed:?}");
        assert!(echoed.contains("m2=2"), "hop-2 cookie missing from {echoed:?}");
    }

    #[test]
    fn path_and_domain_scope_are_enforced_on_the_wire() {
        let port = spawn_local_http_server(vec![
            (
                "/start",
                Route::Redirect {
                    to: "/echo".into(),
                    set_cookie: vec![
                        "hop=1; Path=/".into(),
                        // Scoped to /start: must NOT ride along to /echo.
                        "scoped=9; Path=/start".into(),
                        // Domain attributes against these hosts are invalid:
                        // foreign suffix and single-label both reject.
                        "badhost=7; Domain=example.com; Path=/".into(),
                        "single=8; Domain=localhost; Path=/".into(),
                        // Secure over plain http: rejected out right.
                        "sec=5; Path=/; Secure".into(),
                    ],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let mut jar = CookieJar::new();
        let out = fetch("/start", port, &mut jar, &test_cache("scope"));
        let echoed = String::from_utf8_lossy(&out.body).to_string();
        assert_eq!(echoed, "hop=1", "only the Path=/ cookie may reach /echo, got {echoed:?}");
        // The jar itself kept nothing but the valid host-only cookies.
        let domains: Vec<&str> = jar.cookies().iter().map(|c| c.domain.as_str()).collect();
        assert!(domains.iter().all(|d| *d == "127.0.0.1"), "{domains:?}");
        assert!(!jar.cookies().iter().any(|c| c.name == "sec"), "Secure cookie set over http");
    }

    #[test]
    fn host_only_cookie_never_follows_a_redirect_to_another_host() {
        // Reached via `localhost`, /cross sets a host-only cookie and
        // redirects to 127.0.0.1 — a DIFFERENT origin. The echo on that
        // host must not see the cookie, but the jar must still have it.
        let port = spawn_local_http_server(vec![
            (
                "/cross",
                Route::Redirect {
                    to: format!("http://127.0.0.1:{{port}}/echo"),
                    set_cookie: vec!["mk=keep; Path=/".into()],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let mut jar = CookieJar::new();
        let out = managed_http_fetch(
            &worker_for_test().managed_client(),
            &get(&format!("http://localhost:{port}/cross")),
            &mut jar,
            &test_cache("cross"),
            now_unix(),
            None,
            None,
            false,
        )
        .expect("localhost redirect fetch");
        let echoed = String::from_utf8_lossy(&out.body).to_string();
        assert_eq!(echoed, "-", "host-only cookie leaked to the other host: {echoed:?}");
        assert!(jar.cookies().iter().any(|c| c.name == "mk" && c.domain == "localhost"));
    }

    /// The load-time fix: two managed fetches that share ONE
    /// `Mutex<CookieJar>` and ONE `Mutex<NetCache>` overlap on the wire. The
    /// test server holds both responses until **both** requests have landed
    /// (a rendezvous counter), so a fetch that held the jar or the cache
    /// across `send()`/`bytes()` could never let the pair finish — the first
    /// request would sit waiting for the second until the 2 s deadline and
    /// answer 503, which the assertions below catch. (The wait is bounded on
    /// purpose: the bug fails the test, it never wedges the suite.)
    #[test]
    fn managed_fetches_share_the_jar_and_cache_without_serialising() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let arrived = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let arrived = Arc::clone(&arrived);
                // One thread per connection: the whole point is that two
                // requests are open at the same moment.
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf: Vec<u8> = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => buf.push(byte[0]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    // Rendezvous: this connection only gets an answer once the
                    // other one has arrived too (bounded, so a serialising
                    // regression fails the test instead of hanging it).
                    arrived.fetch_add(1, Ordering::SeqCst);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while arrived.load(Ordering::SeqCst) < 2
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    let response = if arrived.load(Ordering::SeqCst) >= 2 {
                        // Both requests are in flight together: answer now.
                        {
                            let label = path.trim_start_matches('/').to_owned();
                            let body = format!("served {label}");
                            format!(
                                "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: seen{label}=1; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        }
                    } else {
                        // The other fetch never arrived: something serialised
                        // them (a lock held across the network exchange).
                        "HTTP/1.0 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    };
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                });
            }
        });

        // One jar, one cache — the very handles the Blitz provider owns.
        let jar = Arc::new(Mutex::new(CookieJar::new()));
        let cache = Arc::new(Mutex::new(test_cache("interleave")));
        std::thread::scope(|scope| {
            for path in ["/a", "/b"] {
                let jar = Arc::clone(&jar);
                let cache = Arc::clone(&cache);
                scope.spawn(move || {
                    let label = path.trim_start_matches('/').to_owned();
                    let mut jar: &Mutex<CookieJar> = &jar;
                    let cache: &Mutex<NetCache> = &cache;
                    let outcome = managed_http_fetch_report(
                        &worker_for_test().managed_client(),
                        &get(&format!("http://127.0.0.1:{port}{path}")),
                        &mut jar,
                        &cache,
                        now_unix(),
                        None,
                        None,
                        true,
                        USER_AGENT,
                        None,
                    )
                    .expect("interleaved localhost fetch");
                    assert_eq!(outcome.status, 200, "{path}: the pair serialised");
                    assert_eq!(
                        String::from_utf8_lossy(&outcome.body),
                        format!("served {label}")
                    );
                });
            }
        });

        // And the shared state really was shared: both `Set-Cookie`s landed in
        // the one jar, both responses in the one cache.
        let names: std::collections::BTreeSet<String> = jar
            .lock()
            .expect("jar mutex")
            .cookies()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["seena".to_string(), "seenb".to_string()]),
            "both fetches must write the one shared jar"
        );
        let cache = cache.lock().expect("cache mutex");
        assert_eq!(cache.len(), 2, "both responses must land in the one shared cache");
        assert!(cache.lookup(&format!("http://127.0.0.1:{port}/a")).is_some());
        assert!(cache.lookup(&format!("http://127.0.0.1:{port}/b")).is_some());
    }

    #[test]
    fn main_document_navigations_carry_the_jar_even_under_passthrough() {
        // The headline rule: even the *default* (Passthrough) policy sends
        // cookies on top-level loads — a site that set a cookie on one
        // navigation must see it come back on the next. (Under Managed the
        // navigation rides the very same managed_http_fetch the tests above
        // exercise against this server; its only addition is the http→https
        // upgrade, which a plain-http loopback server cannot show.)
        let port = spawn_local_http_server(vec![
            (
                "/set",
                Route::Redirect {
                    to: "/echo".into(),
                    set_cookie: vec!["back=1; Path=/".into()],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let cache = test_cache("nav");
        let mut jar = CookieJar::new();
        let first = fetch_document(
            &get(&format!("http://127.0.0.1:{port}/set")),
            NetPolicy::Passthrough,
            &mut jar,
            &cache,
            now_unix(),
        )
        .expect("navigation 1");
        assert_eq!(first.url.as_str(), format!("http://127.0.0.1:{port}/echo"));
        let second = fetch_document(
            &get(&format!("http://127.0.0.1:{port}/echo")),
            NetPolicy::Passthrough,
            &mut jar,
            &cache,
            now_unix(),
        )
        .expect("navigation 2");
        let echoed = String::from_utf8_lossy(&second.body).to_string();
        assert_eq!(echoed, "back=1", "Passthrough navigation never returned the cookie");
    }

    // -- explicit freshness: a fresh entry never costs a round trip ---------
    //
    // A counting HTTP/1.0 server that answers each route with a fixed
    // `Cache-Control` and records EVERY request path. The assertion that
    // matters is on the counter: an explicitly fresh second fetch must be
    // invisible to the server (zero requests), while no-store/no-cache/
    // must-revalidate responses keep hitting the wire twice, as before.

    /// Spawn the counting `Cache-Control` server: `(path, cache_control)`
    /// routes, plus the shared request log (`path` per request received).
    fn spawn_cache_control_server(
        routes: Vec<(&str, &str)>,
    ) -> (u16, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let seen: std::sync::Arc<Mutex<Vec<String>>> = Default::default();
        let routes: Vec<(String, String)> = routes
            .into_iter()
            .map(|(p, cc)| (p.to_string(), cc.to_string()))
            .collect();
        let seen_thread = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf: Vec<u8> = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.push(byte[0]),
                    }
                    if buf.len() > 64 * 1024 {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                seen_thread.lock().expect("seen").push(path.clone());
                let cache_control = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, cc)| cc.clone())
                    .unwrap_or_default();
                let body = format!("served {path}");
                let mut response = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n",
                    body.len()
                );
                if !cache_control.is_empty() {
                    response.push_str(&format!("Cache-Control: {cache_control}\r\n"));
                }
                response.push_str("Connection: close\r\n\r\n");
                response.push_str(&body);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (port, seen)
    }

    #[test]
    fn explicitly_fresh_entry_is_served_with_zero_network_requests() {
        let (port, seen) = spawn_cache_control_server(vec![("/f", "max-age=3600")]);
        let mut jar = CookieJar::new();
        let cache = test_cache("fresh-direct");
        let url = get(&format!("http://127.0.0.1:{port}/f"));
        let client = worker_for_test().managed_client();

        let events: std::sync::Arc<Mutex<Vec<NetEvent>>> = Default::default();
        let report = |event: NetEvent| events.lock().expect("events").push(event);

        let first = managed_http_fetch_report(
            &client,
            &url,
            &mut jar,
            &cache,
            now_unix(),
            None,
            None,
            true,
            USER_AGENT,
            Some(&report),
        )
        .expect("first fetch");
        assert!(!first.from_cache, "the first fetch has no entry to serve");

        let second = managed_http_fetch_report(
            &client,
            &url,
            &mut jar,
            &cache,
            now_unix(),
            None,
            None,
            true,
            USER_AGENT,
            Some(&report),
        )
        .expect("second fetch");
        assert!(second.from_cache, "a max-age=3600 entry must be served fresh");
        assert_eq!(second.status, 200);
        assert_eq!(String::from_utf8_lossy(&second.body), "served /f");

        // The wire is the judge: the server saw exactly ONE request — the
        // second fetch never happened on the network at all.
        let requests = seen.lock().expect("seen").clone();
        assert_eq!(
            requests,
            vec!["/f".to_string()],
            "a fresh entry must not cost a revalidation round trip"
        );

        // DevTools: one event per fetch, the direct serve reporting the
        // cached entry's status/size with duration 0.
        let events = events.lock().expect("events").clone();
        assert_eq!(events.len(), 2, "one event per fetch");
        assert_eq!(events[1].status, Some(200));
        assert_eq!(events[1].duration_ms, Some(0));
        assert_eq!(
            events[1].url,
            format!("http://127.0.0.1:{port}/f"),
            "the direct serve reports the ORIGINAL url"
        );
    }

    #[test]
    fn no_store_no_cache_and_must_revalidate_always_hit_the_wire() {
        let (port, seen) = spawn_cache_control_server(vec![
            ("/ns", "no-store, max-age=3600"),
            ("/nc", "no-cache, max-age=3600"),
            ("/mr", "must-revalidate, max-age=3600"),
            ("/pr", "proxy-revalidate, max-age=3600"),
        ]);
        for path in ["/ns", "/nc", "/mr", "/pr"] {
            let mut jar = CookieJar::new();
            let cache = test_cache(&format!("stale-{path}").replace('/', "-"));
            let url = get(&format!("http://127.0.0.1:{port}{path}"));
            let client = worker_for_test().managed_client();
            for round in 1..=2 {
                let outcome =
                    managed_http_fetch(&client, &url, &mut jar, &cache, now_unix(), None, None, true)
                        .unwrap_or_else(|e| panic!("{path} round {round}: {e}"));
                assert!(
                    !outcome.from_cache,
                    "{path} round {round} must never be served from cache"
                );
            }
            let hits = seen
                .lock()
                .expect("seen")
                .iter()
                .filter(|p| p.as_str() == path)
                .count();
            assert_eq!(hits, 2, "{path}: both fetches must reach the server");
        }
    }

    // -- subresource fetch pool (thread limits must degrade, never kill) -----

    /// Loopback HTTP fixture that counts the requests it was actually served
    /// and answers every one with `body`. Returns `(port, hits)`.
    fn spawn_counting_http(body: &'static [u8]) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut head: Vec<u8> = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => head.push(byte[0]),
                    }
                    if head.len() > 64 * 1024 {
                        break;
                    }
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        (port, hits)
    }

    /// Poll `log` until it holds `want` deliveries (a `wait_for` for pages
    /// whose whole point is that many handlers settle, not one).
    fn wait_for_deliveries(
        log: &Arc<Mutex<Vec<(String, Vec<u8>)>>>,
        want: usize,
    ) -> Vec<(String, Vec<u8>)> {
        for _ in 0..800 {
            {
                let guard = log.lock().unwrap();
                if guard.len() >= want {
                    return guard.clone();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let got = log.lock().expect("log mutex").len();
        panic!("only {got} of {want} handler.bytes() deliveries landed within 20s");
    }

    /// A `NetHandler` that APPENDS its delivery: the shared `MockHandler`
    /// replaces its log (one delivery per handler), which would hide all but
    /// the last of a page's subresources.
    struct AppendingHandler {
        log: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    }

    impl NetHandler for AppendingHandler {
        fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
            self.log
                .lock()
                .unwrap()
                .push((resolved_url, bytes.to_vec()));
        }
    }

    fn provider_for_pool(policy: NetPolicy) -> (BlitzNetProvider, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        (
            BlitzNetProvider::new(policy, store.cookie_jar(), store.cache()),
            dir,
        )
    }

    /// The pool is a POOL, not a fan-out: far more jobs than threads must all
    /// run, and the thread count must stay where the cap put it.
    #[test]
    fn fetch_pool_runs_every_job_and_never_exceeds_its_cap() {
        let queue = FetchQueue::new(4);
        let done = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let high_water = Arc::new(AtomicUsize::new(0));
        for _ in 0..40 {
            let (done, inflight, high_water) = (
                Arc::clone(&done),
                Arc::clone(&inflight),
                Arc::clone(&high_water),
            );
            queue.submit(Box::new(move || {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                high_water.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                inflight.fetch_sub(1, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while done.load(Ordering::SeqCst) < 40 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            done.load(Ordering::SeqCst),
            40,
            "all 40 jobs (10x the cap) must run"
        );
        assert!(
            high_water.load(Ordering::SeqCst) <= 4,
            "{} jobs ran at once with a cap of 4",
            high_water.load(Ordering::SeqCst)
        );
        assert!(
            queue.threads() <= queue.capacity(),
            "pool grew to {} threads (cap {})",
            queue.threads(),
            queue.capacity()
        );
        assert!(
            high_water.load(Ordering::SeqCst) >= 2,
            "a capped pool must still fetch in parallel (peak {})",
            high_water.load(Ordering::SeqCst)
        );
    }

    /// The tube-"scan"-page crash, deterministically: `thread::spawn` panics
    /// on `EAGAIN`, so a page's worth of `<img>` fetches under a thread limit
    /// used to take the process down. With the seam that refuses every spawn,
    /// a full page of subresources must still be fetched and delivered — zero
    /// fetch threads, zero panics, nothing lost.
    #[test]
    fn provider_delivers_every_subresource_with_no_fetch_threads_at_all() {
        const IMAGES: usize = 24;
        let (port, hits) = spawn_counting_http(b"png-bytes");
        let (provider, _dir) = provider_for_pool(NetPolicy::Passthrough);
        provider.subresources.fail_next_spawns(usize::MAX);

        let log = Arc::new(Mutex::new(Vec::new()));
        for i in 0..IMAGES {
            let image = url(&format!("http://127.0.0.1:{port}/img?i={i}"));
            provider.fetch(
                1,
                Request::get(image),
                Box::new(AppendingHandler {
                    log: Arc::clone(&log),
                }),
            );
        }

        let delivered = wait_for_deliveries(&log, IMAGES);
        assert_eq!(delivered.len(), IMAGES, "one delivery per image");
        for i in 0..IMAGES {
            let want = format!("http://127.0.0.1:{port}/img?i={i}");
            assert_eq!(
                delivered.iter().filter(|(url, _)| *url == want).count(),
                1,
                "{want} must settle exactly once"
            );
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            IMAGES,
            "every image was fetched on the wire"
        );
        assert_eq!(
            provider.subresources.threads(),
            0,
            "the whole page settled without a single fetch thread"
        );
    }

    /// A thread limit that clears is not fatal and not permanent: jobs
    /// submitted while spawns were refused still run, and the pool grows back
    /// on its own afterwards.
    #[test]
    fn fetch_pool_heals_itself_once_thread_creation_works_again() {
        const JOBS: usize = 16;
        let queue = FetchQueue::new(4);
        queue.fail_next_spawns(3);
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..JOBS {
            let done = Arc::clone(&done);
            queue.submit(Box::new(move || {
                std::thread::sleep(std::time::Duration::from_millis(5));
                done.fetch_add(1, Ordering::SeqCst);
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while done.load(Ordering::SeqCst) < JOBS && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(done.load(Ordering::SeqCst), JOBS, "nothing was stranded");
        assert!(
            queue.threads() >= 1,
            "the pool retried the refused spawn (threads: {})",
            queue.threads()
        );
    }

    /// A panicking fetch costs that one resource, not the worker: the pool
    /// keeps serving everything queued behind it.
    #[test]
    fn fetch_pool_survives_a_job_that_panics() {
        let queue = FetchQueue::new(1); // one worker: a lost thread would stall
        let done = Arc::new(AtomicUsize::new(0));
        queue.submit({
            let done = Arc::clone(&done);
            Box::new(move || {
                done.fetch_add(1, Ordering::SeqCst);
                panic!("the fetch job blew up");
            })
        });
        for _ in 0..200 {
            if done.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 1, "the panicking job ran");
        // And the worker is still there to take the next job.
        let after = Arc::new(AtomicUsize::new(0));
        {
            let after = Arc::clone(&after);
            queue.submit(Box::new(move || {
                after.fetch_add(1, Ordering::SeqCst);
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while after.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(after.load(Ordering::SeqCst), 1, "the queue kept working");
    }

    /// A load Blitz already cancelled must not occupy a pool slot behind the
    /// queue: it settles (by dropping the handler) without touching the wire.
    #[test]
    fn aborted_subresource_is_settled_without_reaching_the_network() {
        let (port, hits) = spawn_counting_http(b"png-bytes");
        let (provider, _dir) = provider_for_pool(NetPolicy::Passthrough);
        let controller = blitz_traits::net::AbortController::default();
        let signal = controller.signal.clone();
        controller.abort(); // deterministic: cancelled before it is queued
        let (handler, log) = mock_handler();
        provider.fetch(
            1,
            Request::get(url(&format!("http://127.0.0.1:{port}/gone"))).signal(signal),
            handler,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while hits.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no wire request");
        assert!(
            log.lock().expect("log mutex").is_empty(),
            "an aborted load delivers nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// Unit tests — effective User-Agent resolution (task: per-container UA)

#[cfg(test)]
mod user_agent_tests {
    use super::*;

    #[test]
    fn some_value_is_used_verbatim_once_trimmed() {
        assert_eq!(
            resolve_user_agent(Some("  IronRaven/12.0 (test)  ")).as_str(),
            "IronRaven/12.0 (test)"
        );
    }

    #[test]
    fn empty_or_whitespace_some_resets_to_default() {
        assert_eq!(resolve_user_agent(Some("")), USER_AGENT);
        assert_eq!(resolve_user_agent(Some("   \t \n")), USER_AGENT);
    }

    #[test]
    fn none_is_the_builtin_default() {
        assert_eq!(resolve_user_agent(None), USER_AGENT);
    }

    #[test]
    fn cell_starts_from_the_flag_and_follows_every_write() {
        let cell = new_user_agent(Some("  Flag/1.0  "));
        assert_eq!(read_user_agent(&cell), "Flag/1.0");
        // The flag is resolved exactly like a SetUserAgent: blank ⇒ default.
        assert_eq!(read_user_agent(&new_user_agent(Some("   "))), USER_AGENT);
        assert_eq!(read_user_agent(&new_user_agent(None)), USER_AGENT);
        // A write lands on every later read — the request builders read this
        // same cell per request, so the next fetch carries the new value.
        *cell.lock().expect("ua mutex") =
            resolve_user_agent(Some("Second/2.0"));
        assert_eq!(read_user_agent(&cell), "Second/2.0");
        *cell.lock().expect("ua mutex") = resolve_user_agent(None);
        assert_eq!(read_user_agent(&cell), USER_AGENT);
    }

}

#[cfg(test)]
mod dnt_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url")
    }

// -- Do Not Track (task: private browsing) --------------------------------

/// The DNT switch is process-wide (like the proxy cell); its fetch
/// tests serialize on this, exactly like the proxy tests do.
static DNT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Loopback server answering with the `DNT` header of the request it
/// received (empty body when the request carried none) — the honest
/// witness that the header rides the wire only while the switch is on.
fn spawn_dnt_echo() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut head: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
                if head.len() > 64 * 1024 {
                    break;
                }
            }
            // Case-insensitive scan of the request lines for `DNT:`.
            let mut value = String::new();
            for line in String::from_utf8_lossy(&head).lines() {
                if let Some(rest) = line.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case("dnt")).map(|(_, v)| v.trim()) {
                    value = rest.to_string();
                }
            }
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                value.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(value.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

#[test]
fn dnt_switch_defaults_off_and_follows_every_write() {
    // (Fresh process: the static is born off.)
    let _serial = DNT_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    assert!(!do_not_track(), "born off: no DNT unless a container is private");
    set_do_not_track(true);
    assert!(do_not_track());
    set_do_not_track(false);
    assert!(!do_not_track());
}

/// The header rides the wire exactly when the switch says so — on the
/// plain fetch path (which builds through [`build_request`], the same
/// builder the document/subresource loop uses).
#[test]
fn dnt_header_follows_the_switch() {
    let _serial = DNT_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let port = spawn_dnt_echo();
    let u = url(&format!("http://127.0.0.1:{port}/"));
    let client = reqwest::blocking::Client::new();
    set_do_not_track(false);
    assert_eq!(
        plain_http_fetch(&client, &u, None, None).expect("off fetch").body,
        b"",
        "a clear switch sends no DNT header at all"
    );
    set_do_not_track(true);
    assert_eq!(
        plain_http_fetch(&client, &u, None, None).expect("on fetch").body,
        b"1",
        "the next request after the flip carries DNT: 1"
    );
    set_do_not_track(false);
    assert_eq!(
        plain_http_fetch(&client, &u, None, None).expect("off again").body,
        b"",
        "clearing it stops the header again"
    );
}

/// …and on the XHR/`fetch()` path, which builds its own request beside
/// [`build_request`].
#[test]
fn dnt_header_follows_the_switch_on_the_xhr_path() {
    let _serial = DNT_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let port = spawn_dnt_echo();
    let u = url(&format!("http://127.0.0.1:{port}/"));
    let ua = new_user_agent(None);
    let mut jar = CookieJar::new();
    set_do_not_track(false);
    let off = xhr_fetch_report("GET", &u, &[], None, &mut jar, 0, &ua, None)
        .expect("xhr off fetch");
    assert_eq!(off.body, b"", "no DNT while the switch is clear");
    set_do_not_track(true);
    let on = xhr_fetch_report("GET", &u, &[], None, &mut jar, 0, &ua, None)
        .expect("xhr on fetch");
    assert_eq!(on.body, b"1", "XHR carries DNT: 1 while private");
    set_do_not_track(false);
}

}


mod proxy_tests {
    use super::*;

    #[test]
    fn resolve_proxy_blank_means_direct() {
        assert!(resolve_proxy(None).unwrap().is_none());
        assert!(resolve_proxy(Some("")).unwrap().is_none());
        assert!(
            resolve_proxy(Some("   \t ")).unwrap().is_none(),
            "blank after trim is the contract's 'back to direct'"
        );
    }

    #[test]
    fn resolve_proxy_parses_the_four_schemes() {
        // reqwest 0.13 gives Proxy no url getter, so proof is "parsed Ok" —
        // and every scheme the contract's validate_proxy_url admits must
        // survive here too, or the setting would strand traffic direct.
        for url in [
            "socks5://127.0.0.1:1080",
            "socks5h://proxy.example",
            "http://user:pass@proxy.internal:3128",
            "https://[::1]:8443",
        ] {
            assert!(
                resolve_proxy(Some(url)).unwrap().is_some(),
                "{url} must resolve"
            );
        }
        // Surrounding blanks are stripped before parsing.
        assert!(resolve_proxy(Some("  socks5://h:1080 ")).unwrap().is_some());
        // The contract and the resolver must agree on the scheme list.
        for url in [
            "socks5://h:1", "socks5h://h:1", "http://h:1", "https://h:1",
        ] {
            assert!(ironsurf_common::validate_proxy_url(url).is_ok());
            assert!(resolve_proxy(Some(url)).unwrap().is_some(), "{url}");
        }
    }

    #[test]
    fn resolve_proxy_rejects_what_reqwest_rejects() {
        // No scheme, a scheme reqwest never speaks, and a broken URL all
        // come back as Err — the runtime logs them and stays direct.
        assert!(resolve_proxy(Some("proxy:1080")).is_err());
        assert!(resolve_proxy(Some("gopher://proxy:1080")).is_err());
        assert!(resolve_proxy(Some("socks5://")).is_err());
    }

    #[test]
    fn an_invalid_write_leaves_the_cell_untouched() {
        // The cell is process-wide and starts direct; a rejected seed must
        // not disturb whatever it held (here: still direct). The lock
        // serialises every proxy_tests member that touches the cell —
        // the suite runs one process, and a half-set route would let a
        // concurrent fetch read someone else's route.
        let _serial = PROXY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        assert!(current_proxy().is_none(), "tests start direct");
        assert!(set_process_proxy(Some("gopher://proxy")).is_err());
        assert!(current_proxy().is_none(), "a rejected write changes nothing");
    }

    /// Serialises the members of this module that write the process-wide
    /// route cell (the rest fetch direct and must not glimpse a proxy).
    static PROXY_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// End-to-end attribution over real sockets: a refused proxy, a proxy
    /// that accepts and never answers, and the direct control that must
    /// keep blaming the site. One test, sequential phases: the route cell
    /// is process-wide.
    #[test]
    fn proxy_failures_are_blamed_on_the_proxy() {
        let _serial = PROXY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dead = "http://127.0.0.1:1"; // nothing ever listens on port 1
        let site = Url::parse("http://127.0.0.1:9/").expect("loopback site url");

        // ---- control: direct, refused is the site's own failure ----------
        let worker = FetchWorker::with_timeout(std::time::Duration::from_secs(5));
        let err = plain_http_fetch_report(&worker.client(), &site, None, None, "ua", None)
            .expect_err("port 9 refuses a direct connection");
        assert!(
            !err.contains(PROXY_FAIL_MARK),
            "direct must not blame a proxy: {err}"
        );
        let net = NetError::Transport {
            url: site.clone(),
            message: err.clone(),
        };
        assert!(net.error_page().is_none(), "direct transport failures stay pageless: {err}");

        // ---- a refused proxy becomes the proxy's failure ------------------
        set_process_proxy(Some(dead)).expect("dead proxy resolves");
        worker.set_proxy(current_proxy());
        let err = plain_http_fetch_report(&worker.client(), &site, None, None, "ua", None)
            .expect_err("the dead proxy refuses");
        assert!(
            err.starts_with(PROXY_FAIL_MARK),
            "a refused proxy connection must read as a proxy failure: {err}"
        );
        assert!(err.contains(dead), "the message names the proxy: {err}");
        assert!(err.to_lowercase().contains("refused"), "says what happened: {err}");
        assert!(
            err.contains("never contacted"),
            "says the site was not at fault: {err}"
        );
        let net = NetError::Transport {
            url: site.clone(),
            message: err.clone(),
        };
        let page = net.error_page().expect("proxy failures render an error page");
        assert!(page.contains("Proxy failure") && page.contains(&site.to_string()));

        // ---- a proxy that accepts and never answers reads as a timeout ---
        let blackhole = std::net::TcpListener::bind("127.0.0.1:0").expect("blackhole");
        let blackhole_url = format!("http://{}/", blackhole.local_addr().expect("addr"));
        std::thread::spawn(move || {
            // Accept the connection and then say nothing forever: the
            // client's overall timeout is what ends the request.
            let _accepted = blackhole.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        set_process_proxy(Some(&blackhole_url)).expect("blackhole proxy resolves");
        worker.set_proxy(current_proxy());
        let err = plain_http_fetch_report(&worker.client(), &site, None, None, "ua", None)
            .expect_err("the blackhole proxy never answers");
        assert!(
            err.starts_with(PROXY_FAIL_MARK),
            "a proxy timeout must read as a proxy failure: {err}"
        );
        assert!(err.to_lowercase().contains("timed out"), "says what happened: {err}");

        // ---- back to direct: the cell, not just the message ---------------
        set_process_proxy(None).expect("direct");
        assert!(current_proxy_url().is_none());
        let err = plain_http_fetch_report(&worker.client(), &site, None, None, "ua", None)
            .expect_err("still refused");
        assert!(!err.contains(PROXY_FAIL_MARK), "direct again: {err}");
    }
}
