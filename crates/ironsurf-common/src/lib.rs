//! ironsurf-common — shared types and the IPC codec for IronSurf.
//!
//! Framing: `u32` LE length prefix + bincode payload, over stdin/stdout pipes.
//! See `/workspace/ARCHITECTURE.md` §ironsurf-common (single source of truth).

pub mod logging;

use serde::{Deserialize, Serialize};

/// Identifier of a container (isolated session).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContainerId(pub u32);

/// Identifier of a tab (one OS process per tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(pub u32);

/// Both ids print as bare numbers, which is what log lines want (`tab 3`).
impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for ContainerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A named, colored, isolated browsing session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub id: ContainerId,
    pub name: String,
    /// `"#rrggbb"` hex, used for the tab stripe.
    pub color: String,
    pub is_default: bool,
    /// Per-container **User-Agent** override, pushed to every tab of this
    /// container as [`ToTab::SetUserAgent`]. `None` = the engine's built-in
    /// default UA. `serde(default)` keeps legacy `containers.json` files
    /// (written before this field existed) loading as `None`.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Per-container **proxy** choice: the *name* of one of the user's
    /// global [`ProxyDef`]s (shell settings, `ui.json` → `proxies`). The
    /// shell resolves the name to its URL and pushes it to every tab of
    /// this container as [`ToTab::SetProxy`]; `None` (or a name that no
    /// longer matches a definition) = direct connections. Like
    /// `user_agent`, `serde(default)` keeps legacy `containers.json`
    /// loading.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Per-container **private browsing** mode (task: private browsing).
    /// While set, the shell (a) pushes [`ToTab::SetDoNotTrack`] with
    /// `enabled = true` to every tab of this container, so each outbound
    /// request carries `DNT: 1`, and (b) records **nothing** into the
    /// shared browsing history for navigations from this container.
    /// Cookies, cache and per-container storage are untouched — private
    /// browsing here means "ask not to be tracked, leave no history".
    /// `serde(default)` keeps legacy `containers.json` files loading.
    #[serde(default)]
    pub private: bool,
}

/// A user-defined, **named proxy** (the global pool kept in the shell's
/// settings — `ui.json` → `proxies`; a container selects one by name via
/// [`Container::proxy`]). The URL is validated on entry by
/// [`validate_proxy_url`]: a scheme from [`PROXY_SCHEMES`] plus a host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyDef {
    /// Menu label, unique among the definitions (the shell enforces).
    pub name: String,
    /// `socks5://` | `socks5h://` | `http://` | `https://` endpoint,
    /// optionally with `user:pass@` credentials and `:port`.
    pub url: String,
}

/// The proxy URL schemes IronSurf accepts (reqwest's `Proxy::all` covers
/// all four; `socks5` resolves DNS locally, `socks5h` at the proxy).
pub const PROXY_SCHEMES: [&str; 4] = ["socks5h", "socks5", "https", "http"];

/// Upper bound on a proxy definition's display name (unicode chars).
pub const MAX_PROXY_NAME_CHARS: usize = 64;

/// Upper bound on a proxy URL (unicode chars) — credentials plus a long
/// hostname stay well under this; beyond it the definition is a typo.
pub const MAX_PROXY_URL_CHARS: usize = 512;

/// Why a proxy URL was rejected (see [`validate_proxy_url`]). Hand-rolled
/// `Display` — this crate deliberately carries no thiserror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyError {
    EmptyUrl,
    UnsupportedScheme,
    MissingHost,
    InvalidCharacters,
    TooLong { length: usize, max: usize },
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::EmptyUrl => write!(f, "proxy URL must not be empty"),
            ProxyError::UnsupportedScheme => write!(
                f,
                "proxy URL must start with one of: socks5://, socks5h://, http://, https://"
            ),
            ProxyError::MissingHost => write!(f, "proxy URL must have a host"),
            ProxyError::InvalidCharacters => {
                write!(f, "proxy URL must not contain whitespace or control characters")
            }
            ProxyError::TooLong { length, max } => write!(
                f,
                "proxy URL too long: {length} chars (max {max})"
            ),
        }
    }
}

impl std::error::Error for ProxyError {}

/// Split `url` into (canonical scheme, remainder-after-`://`) when the
/// scheme is one of [`PROXY_SCHEMES`] (matched case-insensitively).
fn split_proxy_url(url: &str) -> Option<(&'static str, &str)> {
    let lower = url.to_ascii_lowercase();
    for scheme in PROXY_SCHEMES {
        if lower.starts_with(&format!("{scheme}://")) {
            return Some((scheme, &url[scheme.len() + 3..]));
        }
    }
    None
}

/// The proxy scheme of `url` (`Some("socks5")` etc.) when it is a proxy
/// URL IronSurf understands, `None` otherwise. Cheap and dependency-free
/// (no DNS, no full URL parse) — the tab hands the string to reqwest, which
/// does the real parsing.
pub fn proxy_scheme(url: &str) -> Option<&'static str> {
    split_proxy_url(url).map(|(scheme, _)| scheme)
}

/// The authority's host part of a proxy URL (`user:pass@` stripped, `:port`
/// stripped, `[ipv6]` unwrapped). `None` when the URL has no scheme we
/// support or no host at all (`socks5://:1080`).
pub fn proxy_host(url: &str) -> Option<&str> {
    let (_, rest) = split_proxy_url(url)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = hostport.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    (!host.is_empty()).then_some(host)
}

/// Validate a proxy definition's URL and return it trimmed: scheme must be
/// one of [`PROXY_SCHEMES`] (case-insensitive), the authority must carry a
/// host, no whitespace or control characters may hide inside (a URL is one
/// token), and at most [`MAX_PROXY_URL_CHARS`] characters. Everything else
/// — reachability, credentials, port range — is reqwest's problem at the
/// tab, which is what actually opens the socket.
pub fn validate_proxy_url(input: &str) -> Result<String, ProxyError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ProxyError::EmptyUrl);
    }
    let length = trimmed.chars().count();
    if length > MAX_PROXY_URL_CHARS {
        return Err(ProxyError::TooLong { length, max: MAX_PROXY_URL_CHARS });
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ProxyError::InvalidCharacters);
    }
    if proxy_scheme(trimmed).is_none() {
        return Err(ProxyError::UnsupportedScheme);
    }
    if proxy_host(trimmed).is_none() {
        return Err(ProxyError::MissingHost);
    }
    Ok(trimmed.to_string())
}

/// One tab-to-shell message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FromTab {
    Ready {
        tab_id: TabId,
    },
    /// RGBA8, straight alpha, top-down; `rgba.len() == width * height * 4`.
    Frame {
        width: u32,
        height: u32,
        scale: f32,
        rgba: Vec<u8>,
    },
    TitleChanged {
        title: Option<String>,
    },
    UrlChanged {
        url: Option<String>,
    },
    LoadingChanged {
        loading: bool,
    },
    HistoryChanged {
        can_go_back: bool,
        can_go_forward: bool,
    },
    StatusText {
        text: Option<String>,
    },
    /// Middle-click on a link (or a **primary-modifier left-click**: Ctrl on
    /// Linux/Windows, ⌘ on macOS), or a **popup the blocker allowed** (task:
    /// popup blocker): the shell must open `url` in a **new tab** (same
    /// container as the requesting tab) without navigating this tab. The tab never spawns
    /// anything itself — the shell owns every tab — so this is the only way a
    /// page's `window.open`/`target="_blank"` becomes a window at all.
    OpenLinkInNewTab {
        url: String,
    },
    /// Right-click on the page: the tab has hit-tested the element under the
    /// cursor ([`ContextTarget`]); the shell opens the context menu at
    /// (`x`, `y`) — logical px in the page viewport, the same space as
    /// `ToTab::Resize`. Menu choices come back as `ToTab::ContextAction`
    /// (engine-side actions) or are handled by the shell itself (open in
    /// new tab, clipboard, navigation).
    ContextMenu {
        x: f32,
        y: f32,
        target: ContextTarget,
    },
    /// The page copied text (Ctrl+C / Ctrl+X / editor cut — surfaced through
    /// blitz's `ShellProvider`): the shell stores it in the session clipboard.
    ClipboardText {
        text: String,
    },
    /// DevTools console entry. `source` names the subsystem the entry came
    /// from: `"js"` (script `console.*` / engine JS diagnostics), `"css"`
    /// (stylesheet parse/style problems), `"dom"` (markup/DOM diagnostics),
    /// `"page"` (anything else about the document itself) or `"userscript"`
    /// (a user script ran: its `console.*` output or its uncaught error).
    /// A `"userscript"` entry always **leads with the script's name** —
    /// `"{name}: {message}"` — so the culprit is readable in the panel.
    ConsoleLog {
        /// `"js"` | `"css"` | `"dom"` | `"page"` | `"userscript"`.
        source: String,
        level: String,
        message: String,
    },
    /// DevTools network entry: one request the tab made for the page
    /// (document, subresource, XHR, media…). Emitted once per request when
    /// it finishes (or fails); `NetEvent::error` carries the failure.
    NetworkEvent {
        event: NetEvent,
    },
    /// Page JavaScript opened a modal dialog (`alert` / `confirm` / `prompt`).
    /// The tab **blocks** on the poll loop until the shell answers with
    /// [`ToTab::DialogResponse`] carrying the same `dialog_id` (sync JS
    /// dialogs, like every real browser); every other `ToTab` message that
    /// arrives meanwhile is deferred, in order, until the dialog settles.
    /// `default` is the `prompt` prefill (always `None` for alert/confirm).
    DialogRequest {
        dialog_id: u32,
        kind: DialogKind,
        /// Dialog body text (bounded by [`MAX_DIALOG_MESSAGE`]).
        message: String,
        /// `prompt` default value; `None` for alert/confirm.
        default: Option<String>,
    },
    /// The pointer shape for what the mouse is over **in the page viewport**
    /// (task: hover cursor). Emitted whenever the shape *changes* (the tab
    /// change-detects, like `StatusText`): CSS `cursor` on the hovered
    /// element first (blitz resolves it), then the engine's clickability
    /// heuristics — links, buttons, clickable form controls, a video's
    /// play/pause area. `CursorIcon::Default` means the plain arrow.
    CursorChanged {
        cursor: CursorIcon,
    },
    /// **Archive mode** (the 💾 toolbar toggle): one finished http(s) fetch
    /// while the mode was on — the tab ships the response body verbatim and
    /// the **shell** writes it under the configured archive root. `url` is
    /// the final (post-redirect) request URL; `is_document` marks a
    /// main-document navigation (saved as `<host>/index.html`), every other
    /// entry is a subresource (media, script, style, image — saved under its
    /// URL path). Emitted per successful (2xx/3xx-delivered) response, like
    /// [`FromTab::NetworkEvent`] but with the bytes attached; failures are
    /// never archived.
    ArchivedFetch {
        url: String,
        is_document: bool,
        bytes: Vec<u8>,
    },
    /// **A download began** (see [`DownloadHint`]): the tab decided a
    /// top-level fetch is a file rather than a page (attachment header, a
    /// media type no page is made of, a clicked `<a download>`, or a shell
    /// [`ToTab::DownloadUrl`]) and is fetching it. The **shell** files a row
    /// and opens the file — the tab never touches the disk. Every download
    /// is `DownloadStart` → `DownloadChunk`* → [`Self::DownloadEnd`] for one
    /// `id`, in that order; nothing else shares the `id`.
    DownloadStart {
        hint: DownloadHint,
    },
    /// Body bytes of the download `id`, in order, each message at most
    /// [`MAX_DOWNLOAD_CHUNK_BYTES`] long (a frame must stay under
    /// [`MAX_FRAME_BYTES`]). A download that failed before its body settled
    /// sends **no** chunk, so a row's byte count is real progress.
    DownloadChunk {
        id: u64,
        bytes: Vec<u8>,
    },
    /// The download `id` settled. `error: Some(reason)` = the fetch failed
    /// (DNS, connection, a body past [`MAX_DOWNLOAD_BYTES`]) and no chunk
    /// follows; `None` = every byte was sent. Always the last message for an
    /// `id` — even when the fetch failed before anything was shipped.
    DownloadEnd {
        id: u64,
        error: Option<String>,
    },
    /// **Find in page** result (see [`ToTab::SetFind`]): the tab counted
    /// `matches` hits for `query` and the **1-based** index of the hit drawn
    /// in light blue is `active` (`0` = none, which only happens when
    /// `matches == 0`). Emitted after every find message that changed
    /// something, and after a load that re-ran a live query — `query` is the
    /// text these numbers describe, so the shell can drop a stale result that
    /// raced ahead of its own find bar. The tab never echoes for a query it
    /// no longer holds.
    FindResult {
        query: String,
        matches: usize,
        active: usize,
    },
    /// **DevTools Inspector**: a snapshot of the live document tree. Emitted
    /// when the shell opens the Inspector pane ([`ToTab::SetInspect`]), after
    /// every navigation while the pane stays open, whenever a
    /// [`ToTab::InspectPickAt`] / [`FromTab::InspectPicked`] changes what is
    /// selected, and whenever the markup changes under a live inspector (the
    /// tab re-walks the tree on its poll tick and stays **silent** while it is
    /// unchanged, so an open pane never floods the pipe). `nodes` is the whole
    /// document in pre-order, bounded by [`MAX_INSPECT_NODES`], and every
    /// [`InspectNode::id`] belongs to this snapshot's `generation` alone.
    InspectSnapshot {
        snapshot: InspectSnapshot,
    },
    /// **Inspect element** ([`ToTab::SetInspectPick`] armed): the deepest
    /// element under the cursor changed (`None` = the pointer left the page,
    /// or nothing hit-answers). The tab paints that element's box over the
    /// page while the picker is armed; the page itself sees nothing — see
    /// [`ToTab::SetInspectPick`].
    InspectHover {
        node: Option<u64>,
    },
    /// **Inspect element**: the user clicked with the picker armed. The tab
    /// selected `node` (box highlighted, scrolled into view), answered with a
    /// fresh [`Self::InspectSnapshot`] carrying it as
    /// [`InspectSnapshot::selected`], and **disarmed** the picker — one pick
    /// per arming, exactly like Firefox.
    InspectPicked {
        node: u64,
    },
    /// **Archive browsing server** (`ironsurf://archive`): the tab asks the
    /// shell to bring up the localhost server that serves the archive folder
    /// (or to talk to the one already running). Tabs cannot serve — they are
    /// sandboxed and the archive tree belongs to the shell — so only the
    /// shell binds the socket and reads the folder. The shell answers with
    /// [`ToTab::ArchiveServerInfo`]. Every override is optional: `None`/blank
    /// means "use the shell's configured value" (folder default
    /// `$HOME/Downloads/ironsurf-tabs`, host `127.0.0.1`, port = random).
    ArchiveServerStart {
        /// Folder to serve (`None`/blank = the configured archive root).
        root: Option<String>,
        /// Host to bind (`None`/blank = `127.0.0.1`).
        host: Option<String>,
        /// Port to bind (`None` or `0` = a random free port).
        port: Option<u16>,
        /// `true` (a link written `ironsurf://archive?open=1`): serve the
        /// archive **root** in a **new tab** of this container and leave this
        /// tab on the summary page; `false`: send *this* tab to the server's
        /// control page, where the folder, host and port are editable.
        open_in_new_tab: bool,
    },
    /// Settings applied from the server's own control page: the shell mirrors
    /// them into its configuration and persists them (best effort).
    ArchiveServerSettingsChanged {
        /// Absolute folder now being served.
        root: String,
        /// Host the server listens on now.
        host: String,
        /// Port the server listens on now (`0` = the server is stopped).
        port: u16,
    },
    /// **Reader mode** (task: reader view): the tab's current view state,
    /// reported every time it changes — after a successful
    /// [`ToTab::SetReader`], after the request was refused (no article
    /// content found: the mode stays off and this is the answer), and on any
    /// real navigation, which always leaves reader mode (`false`). The shell
    /// paints the 📄 toolbar button from this, never from its own request.
    ReaderState {
        enabled: bool,
    },
    /// **Scroll geometry** of the tab's document viewport — what a scroll bar
    /// drawn over the page needs to know, and nothing else can: only the tab
    /// has laid the page out. Emitted whenever the geometry **changes** (an
    /// offset moves, a resize / zoom / navigation changes the scrollable
    /// range), never on a timer, and never for an offset that did not move.
    /// The shell draws the overlay scroll bar from it and hides it a few
    /// seconds after the last movement.
    ScrollState {
        state: ScrollState,
    },
    /// **Popup blocked** (task: popup blocker): the page tried to open a new
    /// tab — `window.open(…)`, or a script activating a `target="_blank"`
    /// link — with no recent user gesture behind it, and this tab's popup
    /// policy refused it. `url` is the request's target resolved against the
    /// document URL, capped at [`MAX_POPUP_URL_CHARS`] chars (a `data:` target
    /// can be megabytes). Nothing else happens: no tab is spawned, this tab
    /// keeps its document. The message exists so the user can see a refusal
    /// happened and allow the site — only the shell can open a tab, so only
    /// the shell can say whether one opened.
    PopupBlocked {
        url: String,
    },
    /// **Browsing history page** (`ironsurf://history`): the tab renders the
    /// visit list, but the log belongs to the **shell** (it lives at the data
    /// root, outside every container's sandbox), so the page asks over IPC and
    /// the shell answers with [`ToTab::HistoryList`] — the same favour
    /// `ironsurf://archive` asks of its server. `clear` (the page's
    /// `?clear=1` row) means "delete the whole shared log, then read it back".
    HistoryListRequest {
        clear: bool,
    },
    Closed,
    Error {
        message: String,
    },
}

/// One visit as the `ironsurf://history` page sees it: the address, the page
/// title the shell learned for it (`None`/blank = title the URL), and the
/// moment of the visit in whole seconds since the Unix epoch. The shell reads
/// the shared `history.jsonl` and formats; tabs never see that file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryVisit {
    pub url: String,
    pub title: Option<String>,
    pub at: i64,
}

/// Visits one `ToTab::HistoryList` carries: the newest N, newest first. The
/// log itself is bounded by `ironsurf_storage::MAX_HISTORY_ENTRIES`; the page
/// shows this many and says so (`total` is the whole log).
pub const MAX_HISTORY_PAGE_ROWS: usize = 500;

/// One shell-to-tab message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToTab {
    Navigate {
        url: String,
    },
    Reload,
    /// v1: no-op placeholder.
    Stop,
    Back,
    Forward,
    Resize {
        width: u32,
        height: u32,
        scale: f32,
    },
    /// Page zoom factor (`1.0` = 100%). The shell owns the chord steps and
    /// clamping (Ctrl-`+` / Ctrl-`-` / Ctrl-`0`); the tab applies the factor
    /// to the document viewport and maps pointer/scroll coordinates through
    /// it. The factor persists across navigations for the tab's lifetime.
    SetZoom {
        zoom: f32,
    },
    /// Scroll-speed multiplier from the settings menu's slider (the ☰ panel):
    /// `1.0` = the tab's default wheel damping. A browser-wide setting — the
    /// shell pushes it to every running tab when the slider moves and seeds
    /// fresh tabs with it at spawn; the tab clamps nonsense to its sane range.
    SetScrollSpeed {
        speed: f32,
    },
    /// Throttle background rendering.
    SetVisible {
        visible: bool,
    },
    Pointer(PointerEvent),
    Key(KeyEvent),
    Scroll(ScrollEvent),
    /// Replace the tab-side clipboard cache the editors' Ctrl+V reads (blitz
    /// asks its `ShellProvider`, which answers from this cache). The tab
    /// process cannot reach the session clipboard itself; the shell pushes the
    /// current contents just before relaying a Ctrl+V keystroke.
    Clipboard {
        text: String,
    },
    /// **Download linked file** ("Download linked file" in the context menu,
    /// and the only shell-side menu row that reaches the tab): fetch `url`
    /// with this tab's own jar/UA/redirects/cache and ship it back as a
    /// download — `DownloadStart` (the tab mints the `id`) → chunks →
    /// `DownloadEnd` is the reply. The URL is **never** rendered, even an
    /// `http(s)` one that would paint; a URL the tab cannot fetch (a bad
    /// scheme, an internal `ironsurf://` page) still answers
    /// `DownloadStart` + `DownloadEnd { error }` so the row always settles.
    /// `suggested_name` is a name hint (never trusted — see
    /// [`sanitize_file_name`]); `None` = derive one from the URL.
    DownloadUrl {
        url: String,
        suggested_name: Option<String>,
    },
    /// The user picked an entry in a context menu that the *engine* services
    /// (paste, video play/pause); shell-side entries never reach the tab.
    ContextAction(ContextAction),
    /// Replace the tab-side userscript set. The shell sends **all enabled**
    /// scripts (patterns included); the **tab** filters them against the
    /// document URL on every navigation (see [`userscripts_for_url`]), so a
    /// script still applies after JS-driven redirects. Disabled scripts are
    /// never sent; an empty vec clears the tab-side set entirely.
    SetUserscripts {
        scripts: Vec<Userscript>,
    },
    /// Per-container **User-Agent** override (the shell owns the choice and
    /// re-sends it on every tab spawn and whenever the user switches presets
    /// or edits the string — see [`BUILTIN_USER_AGENTS`]).
    ///
    /// `Some(v)` = send **exactly** `v` as the HTTP `User-Agent` header on
    /// every subsequent request this tab makes (the shell trims/caps it at
    /// [`MAX_USER_AGENT_CHARS`]); `None` = reset to the tab's built-in
    /// default UA. It applies for the tab's lifetime (across navigations).
    SetUserAgent {
        user_agent: Option<String>,
    },
    /// Per-container **proxy** (the shell resolves the container's
    /// [`Container::proxy`] definition name to its URL and re-sends it on
    /// every tab spawn, on every definition edit, and whenever the user
    /// re-selects or clears the choice).
    ///
    /// `Some(v)` = route **every** outbound request this tab makes from
    /// now on through that proxy — a URL validated by
    /// [`validate_proxy_url`] (`socks5://`, `socks5h://`, `http://` or
    /// `https://`, optional `user:pass@` and `:port`); `None` (or a value
    /// blank after trimming) = direct connections, the default. Applies
    /// for the tab's lifetime (across navigations); nothing reloads — the
    /// live document stands until its next request.
    SetProxy {
        proxy: Option<String>,
    },
    /// Per-container **private browsing** (task: private browsing): the
    /// shell sends this whenever the container's [`Container::private`]
    /// flag flips, alongside the `--private` spawn flag for new tabs.
    ///
    /// `enabled` = send a `DNT: 1` header on **every** outbound request
    /// this tab makes (documents, subresources, XHR/fetch, form POSTs,
    /// redirect hops) until the next such message; `false` = send no DNT
    /// header at all. History recording lives shell-side and is not this
    /// tab's concern. Nothing reloads — the live document stands.
    SetDoNotTrack {
        enabled: bool,
    },
    /// **Archive mode** master switch (the 💾 toolbar menu; global, not
    /// per-container). While `enabled`, the tab emits [`FromTab::ArchivedFetch`]
    /// for every successful http(s) response it fetches — documents,
    /// subresources, and media — from that moment until switched off; the
    /// **shell** owns the destination (root folder + per-tab timestamp
    /// directory) and does the writing (tabs stay sandboxed). Newly spawned
    /// tabs start with the current setting via the `--archive-mode` flag;
    /// the shell rebroadcasts this message to every live tab on each toggle.
    SetArchiveMode {
        enabled: bool,
    },
    /// **Archive browsing server** reply to [`FromTab::ArchiveServerStart`] (the
    /// `ironsurf://archive` page). `ok` = the server is listening (or already
    /// was) and `address` is its `http://host:port` root; on failure `address`
    /// is empty and `error` says why (port busy, folder refused, …). The tab
    /// sends itself to `address + "/ironsurf/serve"` (the server's own control
    /// page) when the request did not ask for a new tab, and the shell opens
    /// `address` in a new tab when it did.
    ArchiveServerInfo {
        tab_id: TabId,
        ok: bool,
        address: String,
        error: Option<String>,
    },
    /// Answer a [`FromTab::DialogRequest`] (matched by `dialog_id`):
    /// `confirmed` is the button the user pressed (alert: always true once
    /// dismissed; confirm: the answer JS returns; prompt: OK vs Cancel).
    /// `text` is the edited prompt value — `Some` exactly when a **prompt**
    /// was accepted (an empty box is `Some("")`).
    DialogResponse {
        dialog_id: u32,
        confirmed: bool,
        text: Option<String>,
    },
    /// **Find in page** (`Ctrl-F`): install/replace the live search query and
    /// re-run it from the top of the document. The shell owns the find bar;
    /// the **tab** owns the document, so it does the matching over the text it
    /// laid out (case-insensitive, document order) and paints the result:
    /// **every** hit gets a yellow background and the **active** hit a light
    /// blue one. The first hit becomes active and is scrolled into view; the
    /// empty query clears the query, the highlights and the count. Answers
    /// with [`FromTab::FindResult`]. The query survives navigations: each new
    /// document re-runs it once loaded.
    SetFind {
        query: String,
    },
    /// Jump to the next hit (wrapping at the end): it becomes the active
    /// (light-blue) one and is scrolled into view. No-op while there are no
    /// hits — the query stands, so a later [`ToTab::SetFind`] restarts.
    FindNext,
    /// Jump to the previous hit (wrapping at the start) — otherwise exactly
    /// [`ToTab::FindNext`].
    FindPrevious,
    /// DevTools **Inspector** pane open/closed for this tab. `true`: the tab
    /// snapshots the document immediately and keeps the shell's tree current
    /// (a fresh snapshot after every navigation, and on its poll tick whenever
    /// the markup actually changed). `false`: the tab stops snapshotting and
    /// drops the on-page highlight. While open the pane costs the tab one tree
    /// walk per change, never a per-frame one.
    SetInspect {
        open: bool,
    },
    /// **Inspect element** picker armed/disarmed — Firefox's crosshair, and
    /// the context menu's "Inspect Element". While armed the tab **swallows**
    /// left-pointer events instead of feeding them to the engine (no hover DOM
    /// events, no click actuation, no selection drag), reports
    /// [`FromTab::InspectHover`] as what is under the cursor changes, and
    /// answers the click with [`FromTab::InspectPicked`] — after which the
    /// picker is disarmed and the page takes pointers again. Arming it for a
    /// page that is still loading is harmless: the picker simply has no
    /// document to hit-test yet.
    SetInspectPick {
        enabled: bool,
    },
    /// The shell selected a node of the inspector tree (a click on a row,
    /// a breadcrumb step): the tab highlights that element's box over the page
    /// and scrolls it into view when it is not fully visible. `None` clears the
    /// highlight. An id from a **previous generation** is ignored, never an
    /// error — the tree on screen can lag a navigation by one message.
    InspectSelect {
        node: Option<u64>,
    },
    /// Right-click → **Inspect Element**: hit-test the page point (`x`, `y`)
    /// — logical px, the same space as [`ToTab::Pointer`] — and select the
    /// deepest element there, exactly as if that point had been clicked with
    /// [`ToTab::SetInspectPick`] armed, **minus** the pick event (the shell
    /// asked, so it already knows a selection happened). The tab answers with a
    /// fresh [`FromTab::InspectSnapshot`] whose
    /// [`InspectSnapshot::selected`] is the hit node.
    InspectPickAt {
        x: f32,
        y: f32,
    },
    /// **Reader mode** (task: reader view, Ctrl-Alt-`R` / the 📄 toolbar
    /// button): render the CURRENT page as cleaned-up text and images only.
    /// The source is the document as it stands the moment the request lands
    /// — including everything JavaScript and XHR already put into the live
    /// DOM — with a fallback to the fetched source when the running scripts
    /// had cleared richer content out of the DOM. The reader document
    /// replaces the live one **in this history entry** (no navigation,
    /// no network); `false` restores the real document (scripts run again).
    /// A real navigation always exits reader mode. The tab answers every
    /// request with [`FromTab::ReaderState`] — the authoritative state, which
    /// stays `false` when the page holds nothing readable.
    SetReader {
        enabled: bool,
    },
    /// **Popup policy** (task: popup blocker): whether this tab refuses the
    /// popups its page asks for. `block` is the master switch (`true` = block
    /// anything a user gesture did not just authorize); `allowed` lists the
    /// document origins whose popups are never blocked — each entry an ASCII
    /// origin (`https://example.com`, no path, lower-case), matched against the
    /// document URL's origin at the moment of the request, so a page on
    /// `accounts.example.com` is a different entry than `example.com`.
    ///
    /// Sent to a tab at spawn (before its first load), and rebroadcast to
    /// every live tab whenever either half changes — a change applies to the
    /// page on screen, not just to tabs opened afterwards. A tab that never
    /// receives this message blocks popups with an empty allow-list: the safe
    /// reading is the default.
    SetPopupPolicy {
        block: bool,
        allowed: Vec<String>,
    },
    /// Reply to [`FromTab::HistoryListRequest`] (the `ironsurf://history`
    /// page): `entries` are the newest [`MAX_HISTORY_PAGE_ROWS`] visits in the
    /// shared log, newest first, and `total` counts the **whole** log so the
    /// page can admit it truncated the list.
    HistoryList {
        tab_id: TabId,
        entries: Vec<HistoryVisit>,
        total: usize,
    },
    Destroy,
}

// ---------------------------------------------------------------------------
// DevTools Inspector (the HTML tree + "inspect element")

/// What kind of DOM node an [`InspectNode`] is. `Document` is the tree's root
/// (exactly one, `depth` 0); `Doctype` is the `<!DOCTYPE html>` node when the
/// parser kept one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InspectKind {
    Element,
    Text,
    Comment,
    Doctype,
    Document,
}

/// The border box of an inspected element, in **viewport CSS px** — the space
/// of `Element.getBoundingClientRect()`: the origin already carries the
/// document's scroll offset and page zoom is **not** applied. Negative or
/// partly-offscreen coordinates are honest (the element is scrolled away).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One node of the inspector tree. The tree is **flat**: children follow their
/// parent in pre-order and carry its [`InspectNode::id`] plus their own
/// [`InspectNode::depth`], so the shell renders/indents it without re-parsing
/// anything, and a click on a row hands the id straight back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InspectNode {
    /// Handle, unique within one snapshot generation. The tab mints ids from
    /// 0 in document order; they mean nothing in another generation (blitz
    /// node ids are arena indices that a navigation recycles).
    pub id: u64,
    /// Parent element's id (`None` at the root; text/comment nodes hanging off
    /// the document root carry the root's id).
    pub parent: Option<u64>,
    pub kind: InspectKind,
    /// Tag name for [`InspectKind::Element`] — lower-cased in HTML, the real
    /// spelling in foreign content, with an `xlink:`-style prefix when the
    /// element has one. `""` for every other kind.
    pub tag: String,
    /// Content attributes in document order (JS-set properties that never
    /// touched an attribute are not here, exactly like Firefox's markup view).
    /// Bounded by [`MAX_INSPECT_ATTRS`] entries, each value by
    /// [`MAX_INSPECT_ATTR_VALUE`] chars with a trailing `…` when cut.
    pub attrs: Vec<(String, String)>,
    /// Text node text / comment body / doctype subset (`html` for
    /// `<!DOCTYPE html>`). `""` for elements — an element's own text lives in
    /// its `Text` children. Runs of whitespace-only text are **dropped** by
    /// the tab (a formatted document would otherwise be mostly blank rows),
    /// other text is collapsed to single spaces and capped at
    /// [`MAX_INSPECT_TEXT`] chars with a trailing `…`.
    pub text: String,
    /// Nesting depth (`0` = the `Document` node) — the shell's indentation
    /// unit.
    pub depth: usize,
    /// `Some` for an element the engine laid out with a non-empty box; `None`
    /// for `display: none`, `display: contents`, `visibility`-only nodes,
    /// never-laid-out subtrees (`<head>`), and every non-element kind.
    pub bounds: Option<InspectBox>,
}

/// A whole document tree, as one flat pre-order list — the DevTools Inspector's
/// data. Size-bounded: at most [`MAX_INSPECT_NODES`] nodes, and the walk stops
/// there rather than truncating the tree in the middle of a subtree
/// ([`InspectSnapshot::truncated`] then says so).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectSnapshot {
    /// Bumped by the tab for every **new document** (navigation): node ids are
    /// only meaningful inside one generation.
    pub generation: u64,
    /// Document URL the tree belongs to (after redirects).
    pub url: String,
    pub nodes: Vec<InspectNode>,
    /// The node the tab highlights on the page — its view of the selection.
    /// The shell owns selection when it drives it (tree clicks), so it never
    /// has to wait for this field to agree.
    pub selected: Option<u64>,
    /// The walk or a field cap gave up somewhere (too many nodes, a huge
    /// attribute value, a long text run): the pane shows it dimmed at the
    /// bottom instead of pretending the tree is complete.
    pub truncated: bool,
}

/// Nodes in one inspector snapshot. A real page runs to tens of thousands of
/// DOM nodes; the pane shows a tree, not an audit, so the walk stops here.
pub const MAX_INSPECT_NODES: usize = 4096;
/// Attributes carried per element row.
pub const MAX_INSPECT_ATTRS: usize = 24;
/// Characters per attribute value (a `srcset` or a data-URI `src` is not worth
/// a megabyte of IPC).
pub const MAX_INSPECT_ATTR_VALUE: usize = 256;
/// Characters per text-node row.
pub const MAX_INSPECT_TEXT: usize = 200;

/// Which of the three synchronous window dialogs the page asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DialogKind {
    /// `window.alert(msg)` — one OK, returns `undefined`.
    Alert,
    /// `window.confirm(msg)` — OK/Cancel, returns the boolean.
    Confirm,
    /// `window.prompt(msg, def)` — text box + OK/Cancel, returns the text or
    /// `null`.
    Prompt,
}

/// Cap for the text carried in a dialog message (the page hands over
/// whatever string it likes; the dialog is UI, not an exfil channel).
pub const MAX_DIALOG_MESSAGE: usize = 64 * 1024;

/// What the cursor was over when the user right-clicked. Every field is
/// "under the cursor at right-click time"; the shell decides from these
/// which menu entries exist (Firefox-style: link/image/video/selection
/// entries only when the thing is there).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextTarget {
    /// Nearest `<a href>`, resolved against the document URL.
    pub link: Option<String>,
    /// `<img src>` (resolved), if over an image.
    pub image: Option<String>,
    /// `<video src>` (resolved, may be empty if the element has no `src`),
    /// if over a video.
    pub video: Option<String>,
    /// Whether that `<video>` is currently playing (`Pause video` vs `Play video`).
    pub video_playing: bool,
    /// Selected page text (bounded by [`MAX_CONTEXT_TEXT`]), if any.
    pub selection: Option<String>,
    /// Whether a text control (the one under the cursor or the focused one)
    /// can take a paste.
    pub editable: bool,
}

/// Context-menu entries the tab process services (engine state lives there:
/// the editor caret, the media players).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextAction {
    /// Insert `text` at the focused text control's caret (the tab feeds the
    /// clipboard cache to blitz and synthesizes Ctrl+V).
    Paste { text: String },
    /// Play/pause the `<video>` that was under the cursor at right-click
    /// (the tab keeps that element's player around until the next click).
    ToggleVideo,
}

/// Cap for text carried in context-menu messages (`ContextTarget::selection`)
/// — the selection is a convenience copy path, not an exfil channel.
pub const MAX_CONTEXT_TEXT: usize = 256 * 1024;

/// Characters of a blocked popup's URL the tab reports (task: popup blocker).
/// The notice is one line of UI; a `data:text/html,…` target is arbitrarily
/// large, so the tail is dropped rather than shipped.
pub const MAX_POPUP_URL_CHARS: usize = 512;

/// Origins a popup allow-list may hold (task: block popups). It is user data
/// carried on every `ToTab::SetPopupPolicy`, so it is bounded — and bounded
/// *identically* on both ends, because a list the shell trimmed differently
/// from the list the tab judged with is a site the user allowed that is
/// silently blocked anyway.
pub const MAX_POPUP_ALLOWED_ORIGINS: usize = 64;

/// The origin of an already-parsed URL, as the popup policy names it: tuple
/// schemes only, ASCII-serialized, port included when it is not the scheme's
/// default (`https://example.com:8443/a?b` → `https://example.com:8443`).
/// `None` = an origin nothing can match — `about:blank`, a `data:`/`blob:`
/// document, a `javascript:` run — which is exactly the reading a policy wants:
/// a document with no origin is never covered by an allow-list entry.
pub fn origin_of(url: &url::Url) -> Option<String> {
    let origin = url.origin();
    if !origin.is_tuple() {
        return None;
    }
    let serialized = origin.ascii_serialization();
    (!serialized.is_empty() && serialized != "null").then_some(serialized)
}

/// [`origin_of`] for a URL given as text (what the shell has: the address bar,
/// a `FromTab::UrlChanged`). A URL it cannot parse has no origin.
pub fn popup_origin(url: &str) -> Option<String> {
    url::Url::parse(url.trim()).ok().and_then(|url| origin_of(&url))
}

/// What a **user** types for a site, normalized to the origin the policy
/// stores: `Example.COM/path` → `https://example.com`,
/// `http://localhost:8080/x` → `http://localhost:8080`. A bare host is read as
/// `https`, because that is what naming a site today means. `None` = not a site
/// name at all (empty, whitespace, a host-less URL) — dropped, never stored
/// broken, rather than kept as an entry that matches nothing.
pub fn normalize_popup_origin(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    parsed.host_str()?;
    origin_of(&parsed)
}

/// Replace an allow-list entry for `origin` (task: block popups): `allow` adds
/// it (deduped, list capped at [`MAX_POPUP_ALLOWED_ORIGINS`] — oldest entries
/// give way, since the one the user just acted on must never be the one that
/// falls off) and `false` removes it. Both ends of the wire use it so a list
/// the shell shows is a list the tab judges with, byte for byte.
pub fn set_popup_allowed(allowed: &[String], origin: &str, allow: bool) -> Vec<String> {
    let mut list: Vec<String> = allowed
        .iter()
        .filter(|entry| entry.as_str() != origin)
        .cloned()
        .collect();
    if allow {
        list.push(origin.to_owned());
        // Oldest entries give way: the one the user has just acted on must
        // never be the one that falls off the capped list.
        let overflow = list.len().saturating_sub(MAX_POPUP_ALLOWED_ORIGINS);
        list.drain(..overflow);
    }
    list
}

/// What made a top-level fetch a **download** instead of a page. The tab
/// decides (it owns the response), the shell only displays it, so the
/// ordering of the reasons is a tab-side rule (see the tab's
/// `downloads::attachment_for`): `Disposition` beats `MediaType` beats
/// `AnchorAttribute`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DownloadTrigger {
    /// `Content-Disposition: attachment` on the response — the server said
    /// "this is a file" and nothing else in the contract overrides it.
    Disposition,
    /// The response's media type is one no page is made of (see
    /// [`is_download_media_type`]): an archive, a disk image, an
    /// installer, a font.
    MediaType,
    /// A clicked `<a>`/`<area>` carried the `download` content attribute, so
    /// the link is a file *promise* even when the URL would paint
    /// (`<a href="report.html" download>`).
    AnchorAttribute,
    /// The shell asked for this URL to be saved ([`ToTab::DownloadUrl`],
    /// "Download linked file").
    Requested,
}

/// What the tab knows about a download when it starts: shipped as
/// [`FromTab::DownloadStart`] before a byte has arrived, so the shell can
/// file a row and name the file while the fetch is still running.
///
/// `id` is **minted by the tab process** (a per-tab counter, never global):
/// the shell keys rows by `(tab_id, id)`, so two tabs may both say `1`.
/// `name` is only a *suggestion* — the `Content-Disposition` filename or a
/// clicked `<a download>` value — so it is `None` more often than not and
/// always untrusted: the shell sanitizes it (see [`sanitize_file_name`]) and
/// falls back to [`filename_from_url`]. `url` is the post-redirect URL the
/// tab actually fetched, `media_type` the response's `Content-Type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadHint {
    pub id: u64,
    pub url: String,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub trigger: DownloadTrigger,
}

/// A download body larger than this is refused **before it is buffered**:
/// the tab buffers the whole response in RAM before shipping it, so a cap
/// keeps a 4 GiB `.iso` link from killing the process. 512 MiB.
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Cap for one [`FromTab::DownloadChunk::bytes`] — a chunk plus its frame
/// header must stay well under [`MAX_FRAME_BYTES`].
pub const MAX_DOWNLOAD_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// The longest file name a download can carry (one path component, so the
/// shell can still `mkdir` its way out of a pathological name).
pub const MAX_DOWNLOAD_NAME: usize = 128;

/// Whether a top-level response of this media type is a **file** rather
/// than a page. IronSurf renders nothing but markup — no image viewer, no
/// PDF viewer, no plugin — so the rule is what *this* browser can put on
/// screen, not what Chrome would do:
///
/// * `text/*` is a page (a `text/plain` document shows its source, as
///   today), and so is a top-level `image/*` (a PNG paints, which is why a
///   "download the picture" link keeps working as a view), `audio/*` and
///   `video/*` (the tab's own player shows them).
/// * `application/xml`, `application/xhtml+xml`, `application/atom+xml`,
///   `application/rss+xml`, `application/\*+json` and
///   `application/\*+xml` are pages — Blitz parses them as documents.
/// * `application/json`, `application/javascript` (and the JS spellings)
///   stay pages: the tab has no viewer, yet it *does* put them on screen as
///   source text today, and a download that replaced a visible document
///   would be the regression.
/// * Every other `application/…` is a file: `octet-stream`, `zip`, `x-…`,
///   `gzip`, `pdf`, `wasm`, `vnd.…`, `java-archive`, `x-msdownload`, …
/// * `font/*` and `model/*` are files (nothing renders them).
/// * An empty or absent media type is **not** a download — a sniffed
///   `file:` document keeps the behaviour it has today.
///
/// Media type parameters (`; charset=…`) and casing are ignored; a
/// malformed type (`foo`, `application`, `app/zip/`) is not a download.
pub fn is_download_media_type(media_type: &str) -> bool {
    let bare = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if bare.is_empty() {
        return false;
    }
    let Some((top, sub)) = bare.split_once('/') else {
        return false;
    };
    if !(top == "application" || top == "font" || top == "model") {
        // text/*, image/*, audio/*, video/*, multipart/, message/ → page.
        return false;
    }
    if top != "application" {
        return true;
    }
    // Structured documents Blitz parses, plus the source-text documents
    // the tab already shows verbatim (JSON, scripts) — none of them is a
    // file anyone has to save to read.
    if matches!(
        sub,
        "xhtml+xml"
            | "xml"
            | "json"
            | "javascript"
            | "x-javascript"
            | "ecmascript"
            | "x-ecmascript"
    ) || sub.ends_with("+xml")
        || sub.ends_with("+json")
    {
        return false;
    }
    // `application/pdf`, `application/octet-stream` and every other
    // `application/…` is a file.
    !sub.is_empty()
}

/// One file-system **component** a download may use, or `None` when the
/// suggestion cannot become a file name at all.
///
/// Suggestions come from the network (`Content-Disposition`, an anchor
/// value, [`ToTab::DownloadUrl::suggested_name`]) and are treated as
/// hostile input: no `/` or `\`, no NUL or other control character, never
/// `.`/`..`, no leading dot or whitespace (a dotfile, a hidden file),
/// trimmed, clamped to [`MAX_DOWNLOAD_NAME`] chars. Both ends call this so
/// a suggestion the tab *makes* is already safe, yet the shell **still**
/// sanitizes — the tab is not trusted.
pub fn sanitize_file_name(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = 0usize;
    for ch in name.chars() {
        // Chrome's own list of characters a file name may not carry.
        if ch.is_control()
            || matches!(
                ch,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            )
        {
            // A separator inside a suggestion is the end of the useful part
            // (`a/b` → `a`); a leading one means the name has nothing left.
            if out.is_empty() {
                return None;
            }
            break;
        }
        if chars >= MAX_DOWNLOAD_NAME {
            break;
        }
        out.push(ch);
        chars += 1;
    }
    // A leading `.` (or whitespace) is what makes `.`/`..`/`.hidden` not a
    // name; strip it and whatever is left is the name (`\u{2e}.hidden` →
    // `hidden`). Digit-only names (`/f/1` → `1`) are names like any other.
    let trimmed = out.trim().trim_start_matches(['.', ' ']).trim_end();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// A file name taken from a URL: the last non-empty path segment,
/// percent-decoded (`%E2%98%83.zip` → `☃.zip`), then sanitized (see
/// [`sanitize_file_name`]). `None` when the URL has no usable path segment
/// (a bare `https://host`, `/?x`, an opaque `data:` URL).
pub fn filename_from_url(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    // No `/` after the authority means no path at all (`https://host`), so
    // there is nothing to name a file after.
    let idx = rest.find(['/', '?', '#'])?;
    let path = rest[idx..].split(['?', '#']).next().unwrap_or("");
    // A trailing `/` names a directory, not a file — but the directory is
    // still the best name on offer (`/files/report/` → `report`), so the
    // last non-empty segment wins.
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if last.is_empty() {
        return None;
    }
    let bytes = percent_decode(last.as_bytes());
    let decoded = String::from_utf8_lossy(&bytes);
    sanitize_file_name(decoded.trim_start_matches('/'))
}

/// Percent-decode bytes the way a browser decodes a URL before using it as
/// a file name; an invalid `%xx` is passed through rather than dropped.
pub(crate) fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The file name a `Content-Disposition` value suggests, or `None`.
///
/// `filename*=UTF-8''report%20é.zip` (RFC 6266) is percent-decoded with its
/// charset ignored (UTF-8 is what every server writes in 2026); the bare
/// `filename="report.zip"` spelling is backslash-unescaped. When a header
/// carries **both** spellings, the `*`-suffixed one wins
/// (`filename="bad.zip"; filename*=UTF-8''good.zip` → `good.zip`), exactly
/// as browsers resolve it. The value is a suggestion, never a path — the
/// shell sanitizes it (see [`sanitize_file_name`]).
pub fn content_disposition_filename(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut starred: Option<String> = None;
    for part in value.split(';') {
        let part = part.trim();
        if part
            .get(.."filename*=".len())
            .is_some_and(|head| head.eq_ignore_ascii_case("filename*="))
        {
            let raw = part["filename*=".len()..].trim();
            // `UTF-8''name` → `name`: skip the charset and its `''`
            // terminator (any charset is decoded as UTF-8 — the one every
            // server writes in practice).
            let raw = raw
                .split_once("''")
                .or_else(|| raw.split_once("%27%27"))
                .map(|(_, tail)| tail)
                .unwrap_or(raw);
            starred = Some(String::from_utf8_lossy(&percent_decode(raw.as_bytes())).into_owned());
        } else if part
            .get(.."filename=".len())
            .is_some_and(|head| head.eq_ignore_ascii_case("filename="))
        {
            let raw = part["filename=".len()..].trim();
            let raw = raw.strip_prefix('"').unwrap_or(raw).trim_end_matches('"');
            // The bare spelling backslash-escapes quotes (`\"` → `"`), the
            // one thing the RFC 6266 spelling does not need.
            let mut unescaped = String::new();
            let mut escape = false;
            for ch in raw.chars() {
                if escape {
                    unescaped.push(ch);
                    escape = false;
                } else if ch == '\\' {
                    escape = true;
                } else {
                    unescaped.push(ch);
                }
            }
            if plain.is_none() {
                plain = Some(unescaped);
            }
        }
    }
    let candidate = starred.or(plain)?;
    sanitize_file_name(candidate.trim().trim_matches('"'))
}

/// A user-defined JavaScript program that runs on matching pages
/// (Tampermonkey/Greasemonkey-style). The shell owns the script set and
/// ships it wholesale via [`ToTab::SetUserscripts`]; the tab decides, per
/// navigation, which scripts apply to the current document URL
/// (see [`userscripts_for_url`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Userscript {
    /// Stable id chosen by the shell (manager-UI bookkeeping / dedup).
    pub id: u64,
    /// Human-readable name shown in the manager UI (may be unicode).
    pub name: String,
    /// URL patterns this script runs on — exact or wildcard, matched by
    /// [`userscript_matches`]. An empty pattern list matches **nothing**.
    pub patterns: Vec<String>,
    /// The JavaScript source, verbatim.
    pub code: String,
    /// Disabled scripts are never sent to the tab
    /// (see [`ToTab::SetUserscripts`]).
    pub enabled: bool,
}

/// One network request observed by the tab, reported to the shell as
/// [`FromTab::NetworkEvent`] (DevTools network panel).
///
/// A request that *completed* carries `status` (and `response_bytes` /
/// `duration_ms` when the engine measured them) and `error == None`. A
/// request that *failed* (DNS, TLS, connection refused, blocked by policy,
/// …) has `status == None` and `error == Some(reason)` — never both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetEvent {
    /// HTTP method (`"GET"`, `"POST"`, …), upper-cased by the engine.
    pub method: String,
    /// Request URL after the engine resolved it (redirects are separate
    /// events, one per hop).
    pub url: String,
    /// Response status code; `None` when no response arrived (see `error`).
    pub status: Option<u16>,
    /// Response body size in bytes, when known.
    pub response_bytes: Option<u64>,
    /// Wall-clock duration of the request in milliseconds, when measured.
    pub duration_ms: Option<u64>,
    /// Failure reason; `Some` exactly when the request failed (`status` is
    /// then `None`).
    pub error: Option<String>,
    /// What initiated the request, DevTools-style: `"document"` (a
    /// navigation / form POST), `"script"` (`<script src>`, ES modules),
    /// `"resource"` (any other subresource: images, CSS, fonts), `"xhr"`
    /// (`XMLHttpRequest`), `"fetch"` (`window.fetch`), or `"download"` (a
    /// saved file — see [`DownloadHint`], whose request shows up here too).
    /// `None` when the engine did not say (older events, bare unit tests).
    pub initiator: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerEvent {
    pub x: f32,
    pub y: f32,
    pub kind: PointerKind,
    pub button: MouseButton,
    /// Keyboard modifiers held during the event (`ToTab::Pointer` carries the
    /// shell's `TouchArea` modifiers; a synthesised event says `NONE`). The tab
    /// reads it for the chords that live on the *mouse*: a
    /// [`Modifiers::action`]-held left click on a link is "open in new tab".
    #[serde(default)]
    pub mods: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerKind {
    Move,
    Down,
    Up,
    Over,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A cursor (pointer) shape, as reported by the tab for what is under the
/// mouse ([`FromTab::CursorChanged`]). Mirrors the subset every layer of the
/// stack agrees on: the CSS `cursor` keywords blitz resolves, the
/// `cursor-icon` crate's vocabulary it reports them in, and the shapes
/// Slint's `MouseCursor` can actually draw (the names follow CSS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorIcon {
    /// The platform's plain arrow.
    #[default]
    Default,
    /// `cursor: none` — the pointer is hidden.
    Hidden,
    Help,
    /// The pointing hand: the thing under the cursor is clickable.
    Pointer,
    Progress,
    Wait,
    Crosshair,
    /// The I-beam: text (or a text control) is under the cursor.
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    ColResize,
    RowResize,
    NResize,
    EResize,
    SResize,
    WResize,
    NeResize,
    NwResize,
    SeResize,
    SwResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// DOM `key`.
    pub key: String,
    /// DOM `code`.
    pub code: String,
    pub text: Option<String>,
    pub repeat: bool,
    pub mods: Modifiers,
}

/// Wheel/trackpad scroll over the page viewport.
///
/// Sign convention (winit/AccessKit, which both ends of the pipe speak): a
/// POSITIVE `delta_y` scrolls UP (reveals content above; `scrollTop` shrinks),
/// a negative one scrolls DOWN. Same for `delta_x` and LEFT. The shell passes
/// the toolkit's deltas through untouched — do not negate them on either side.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollEvent {
    /// Pointer position (logical px, same space as `ToTab::Resize`).
    pub x: f32,
    pub y: f32,
    pub delta_x: f32,
    pub delta_y: f32,
}

/// The document viewport's vertical scroll geometry, in **CSS px** (the space
/// the page is laid out in: window height and offsets divided by the tab's
/// total scale, zoom included), as reported by the tab in
/// [`FromTab::ScrollState`].
///
/// The three numbers describe the whole scroll range without naming the
/// document: `range` is how far the page still goes **below** the offset's
/// ceiling — `0 ≤ y ≤ range` — and the window itself is `viewport_h` tall, so
/// the content spans `viewport_h + range`. The shell turns that into a thumb:
/// `height ∝ viewport_h / (viewport_h + range)`, position `∝ y / range` — the
/// thumb's bottom edge lands exactly at the track's end at `y == range`.
///
/// `range` includes the engine's overscroll (a document that scrolls lets the
/// user bring its last lines up to mid-window, so it runs half a window past
/// the content bottom); a document that fits its window has `range == 0` and
/// never scrolls — and gets no bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScrollState {
    /// Current scroll offset from the top, CSS px (`0` = at the top).
    pub y: f32,
    /// Largest offset this document answers, CSS px (`0` = it fits, no scroll).
    pub range: f32,
    /// Viewport (window) height, CSS px.
    pub viewport_h: f32,
}

impl ScrollState {
    /// Whether this document scrolls at all (`range > 0`), i.e. whether a
    /// scroll bar over its page could say anything true.
    pub fn is_scrollable(&self) -> bool {
        self.range > 0.0
    }

    /// Same geometry within `eps` CSS px — the tolerance the tab reports with,
    /// so sub-pixel layout jitter never produces a message.
    pub fn same_within(&self, other: &Self, eps: f32) -> bool {
        (self.y - other.y).abs() <= eps
            && (self.range - other.range).abs() <= eps
            && (self.viewport_h - other.viewport_h).abs() <= eps
    }
}

/// Keyboard modifier state as a bitflags-style `u8` newtype.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const CTRL_BIT: u8 = 1 << 0;
    pub const SHIFT_BIT: u8 = 1 << 1;
    pub const ALT_BIT: u8 = 1 << 2;
    pub const META_BIT: u8 = 1 << 3;

    #[inline]
    pub const fn new(bits: u8) -> Self {
        Modifiers(bits)
    }

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[inline]
    pub const fn empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn ctrl(self) -> bool {
        self.0 & Self::CTRL_BIT != 0
    }

    #[inline]
    pub const fn shift(self) -> bool {
        self.0 & Self::SHIFT_BIT != 0
    }

    #[inline]
    pub const fn alt(self) -> bool {
        self.0 & Self::ALT_BIT != 0
    }

    #[inline]
    pub const fn meta(self) -> bool {
        self.0 & Self::META_BIT != 0
    }

    #[inline]
    pub const fn with_ctrl(mut self) -> Self {
        self.0 |= Self::CTRL_BIT;
        self
    }

    #[inline]
    pub const fn with_shift(mut self) -> Self {
        self.0 |= Self::SHIFT_BIT;
        self
    }

    #[inline]
    pub const fn with_alt(mut self) -> Self {
        self.0 |= Self::ALT_BIT;
        self
    }

    #[inline]
    pub const fn with_meta(mut self) -> Self {
        self.0 |= Self::META_BIT;
        self
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Modifiers(self.0 | other.0)
    }

    /// The platform's **primary** ("action") modifier as it reaches this
    /// process from the shell: `Ctrl` everywhere, `Command` on macOS.
    ///
    /// Why both flags on macOS: Slint swaps Command and Control in its own
    /// modifier state (`KeyboardModifiers`: "on macOS, Slint maps the Command
    /// key to the control modifier, and the Control key to the meta
    /// modifier"), so a ⌘ chord arrives as `ctrl` there — while backends that
    /// do not swap report it in `meta`. Accepting either is what makes one
    /// chord work on both, and the real Control key answering the same chord
    /// on a Mac costs nothing.
    #[inline]
    pub const fn action(self) -> bool {
        if cfg!(target_os = "macos") {
            self.ctrl() || self.meta()
        } else {
            self.ctrl() && !self.meta()
        }
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

/// Guard against corrupt streams (frames larger than this are rejected).
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The browser's built-in start page. `ironsurf:` is an internal scheme:
/// every `ironsurf://…` URL is answered locally by the tab process (see
/// `ironsurf_tab::netmod::internal_page_bytes`) and never touches the network.
pub const WELCOME_URL: &str = "ironsurf://welcome";

/// The browsing-history page (`ironsurf://history`): the shared visit log as a
/// page of its own, rendered by the tab from the shell's answer to
/// [`FromTab::HistoryListRequest`] and opened by the ☰ settings panel's
/// **History** button.
pub const HISTORY_URL: &str = "ironsurf://history";

/// Default start page URL (the built-in `ironsurf://welcome` page).
pub fn default_url() -> String {
    WELCOME_URL.to_string()
}

/// Split a lowercased URL into `(scheme, host)`. The host is `None` when the
/// URL carries no `://` authority (`data:…`, `about:…`, plain `ironsurf://`
/// with an empty authority, or anything without a scheme). Credentials and
/// `:port` are stripped; IPv6 literal hosts are not special-cased (contract-
/// level matching only).
fn scheme_and_host(url: &str) -> (Option<&str>, Option<&str>) {
    let idx = match url.find("://") {
        Some(i) if i > 0 => i,
        _ => return (None, None),
    };
    let scheme = &url[..idx];
    let rest = &url[idx + 3..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = host.split(':').next().unwrap_or("");
    (
        Some(scheme),
        if host.is_empty() { None } else { Some(host) },
    )
}

/// Whether the URL speaks `http:` or `https:` (the only schemes a wildcard
/// domain pattern may ever match).
fn is_http_like(scheme: Option<&str>) -> bool {
    matches!(scheme, Some("http") | Some("https"))
}

/// Match one userscript URL pattern against a document URL.
///
/// Comparison is fully case-insensitive: **both** strings are lowercased
/// first, so paths compare case-insensitively too (documented simplification).
///
/// * A blank (empty/whitespace) pattern matches nothing; skip blanks.
/// * The bare pattern `"*"` matches every URL (any scheme).
/// * A pattern **not** starting with `*` is compared to the **full URL
///   string** exactly (after lowercasing) — this is how non-http(s) URLs
///   like `ironsurf://settings/plugins` can still be targeted literally.
///   Additionally, a pattern with no `*` and no `://` (e.g.
///   `example.com/docs`) is `*://example.com/docs*` **sugar**: it matches
///   http(s) URLs whose host *equals* the pattern's host part and whose
///   path starts with the pattern's path (a pattern with no path — just a
///   bare host — matches any path on that exact host).
/// * A pattern starting with `*` (but not the bare `"*"`) strips the leading
///   `*` into remainder `R`, split at the first `/` into a **domain part**
///   and a **path part**. The URL must be http(s). Host boundary rule: the
///   URL's host matches iff it equals the domain (after stripping an
///   optional leading `*.` / `.`) or *ends with* `.{domain}` — a lookalike
///   like `notexample.com` NEVER matches `example.com`. An empty domain part
///   (`*/api`) means any host. If R has a path part, the lowercased URL must
///   `ends_with` that path string verbatim (no percent-decoding).
///
/// So `*example.com` matches `https://example.com/` and
/// `http://x.example.com/page` but NOT `https://notexample.com/`, and
/// `*example.com/api` matches URLs on that host boundary whose URL ends in
/// `/api`. Non-http(s) schemes (`data:`, `ironsurf:`, `about:`) never match
/// wildcard domain patterns; exact patterns can still match them literally.
pub fn userscript_matches(pattern: &str, url: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let url = url.trim().to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if let Some(domain) = pattern.strip_prefix('*') {
        let (domain, path) = match domain.split_once('/') {
            Some((d, p)) => (d, Some(p)),
            None => (domain, None),
        };
        let domain = domain
            .strip_prefix("*.")
            .or_else(|| domain.strip_prefix('.'))
            .unwrap_or(domain);
        let (scheme, host) = scheme_and_host(&url);
        if !is_http_like(scheme) {
            return false;
        }
        let host = match host {
            Some(h) => h,
            None => return false,
        };
        let host_ok = domain.is_empty() || host == domain || host.ends_with(&format!(".{domain}"));
        host_ok && path.map(|p| url.ends_with(p)).unwrap_or(true)
    } else {
        if url == pattern {
            return true;
        }
        // `example.com/docs` sugar = `*://example.com/docs*` (host+prefix).
        if !pattern.contains("://") {
            let (pat_host, pat_path) = match pattern.split_once('/') {
                Some((h, p)) => (h, p),
                None => (pattern.as_str(), ""),
            };
            let (scheme, host) = scheme_and_host(&url);
            if is_http_like(scheme) && host == Some(pat_host) {
                let url_path = url
                    .split_once("://")
                    .map(|(_, r)| {
                        let after_auth = r.split_once('/').map(|(_, p)| p).unwrap_or("");
                        after_auth.split(['?', '#']).next().unwrap_or("")
                    })
                    .unwrap_or("");
                return url_path.starts_with(pat_path);
            }
        }
        false
    }
}

/// The scripts that apply to `url`: enabled scripts whose **any** pattern
/// matches (see [`userscript_matches`]). Scripts with an empty/whitespace
/// pattern list match nothing and are never returned.
pub fn userscripts_for_url<'a>(scripts: &'a [Userscript], url: &str) -> Vec<&'a Userscript> {
    scripts
        .iter()
        .filter(|s| s.enabled && s.patterns.iter().any(|p| userscript_matches(p, url)))
        .collect()
}

/// Built-in **User-Agent** presets the shell's switcher offers, as
/// `(label, UA string)` in menu order. The labels are display-only; the UA
/// strings are what actually goes on the wire (stored verbatim in
/// [`Container::user_agent`], shipped in [`ToTab::SetUserAgent`]). A user may
/// still supply any other string — these are conveniences, not a whitelist.
pub const BUILTIN_USER_AGENTS: &[(&str, &str)] = &[
    ("Firefox", "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0"),
    ("Chrome", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"),
    ("Bingbot", "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; Bingbot/2.0; +http://www.bing.com/bingbot.htm) Chrome/116.0.1950.0 Safari/537.36"),
    ("Googlebot", "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"),
    ("Android Chrome", "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.6478.122 Mobile Safari/537.36"),
];

/// Label of `ua` when it is one of [`BUILTIN_USER_AGENTS`] — an **exact**
/// string match (no case folding, no trimming). `None` means `ua` is a
/// custom (non-preset) UA string.
pub fn builtin_user_agent_label(ua: &str) -> Option<&'static str> {
    BUILTIN_USER_AGENTS
        .iter()
        .find(|(_, v)| *v == ua)
        .map(|(label, _)| *label)
}

/// Whether `ua` is one of the [`BUILTIN_USER_AGENTS`] presets (exact match);
/// shorthand for [`builtin_user_agent_label`].is_some().
pub fn is_builtin_user_agent(ua: &str) -> bool {
    builtin_user_agent_label(ua).is_some()
}

/// Cap on a user-edited **User-Agent** string (in `char`s): the field is a
/// header value, not a place to paste a novel. Presets are all well under it.
pub const MAX_USER_AGENT_CHARS: usize = 512;

fn invalid_data<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

/// Serialize `msg` with bincode and write it as
/// `u32 LE length prefix + payload`. Flushes the writer.
pub fn write_frame<W: std::io::Write, T: serde::Serialize>(
    w: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let payload = bincode::serialize(msg).map_err(invalid_data)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "frame too large: {} bytes (max {})",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read one length-prefixed bincode frame.
pub fn read_frame<R: std::io::Read, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_BYTES})"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(invalid_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event() -> KeyEvent {
        KeyEvent {
            key: "a".into(),
            code: "KeyA".into(),
            text: Some("a".into()),
            repeat: false,
            mods: Modifiers::NONE.with_ctrl(),
        }
    }

    fn pointer_event() -> PointerEvent {
        PointerEvent {
            x: 1.5,
            y: -2.25,
            kind: PointerKind::Down,
            button: MouseButton::Left,
            mods: Modifiers::NONE,
        }
    }

    fn scroll_event() -> ScrollEvent {
        ScrollEvent {
            x: 10.0,
            y: 20.0,
            delta_x: 0.0,
            delta_y: -3.0,
        }
    }

    fn frame_round_trip<T>(msg: T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).expect("write_frame");
        let got: T = read_frame(&mut buf.as_slice()).expect("read_frame");
        assert_eq!(got, msg);
        got
    }

    #[test]
    fn ids_and_container_round_trip() {
        frame_round_trip(ContainerId(7));
        frame_round_trip(TabId(42));
        frame_round_trip(Container {
            id: ContainerId(1),
            name: "personal".into(),
            color: "#ff5500".into(),
            is_default: true,
            user_agent: None,
            proxy: None,
            private: false,
        });
        assert_eq!(ContainerId(1), ContainerId(1));
        assert_ne!(ContainerId(1), ContainerId(2));
    }

    /// `ToTab::SetUserAgent` covers both halves of the contract: `Some(v)`
    /// (a preset or a hand-typed string, unicode included) ships verbatim,
    /// `None` resets to the tab's default.
    #[test]
    fn set_user_agent_round_trips() {
        frame_round_trip(ToTab::SetUserAgent { user_agent: None });
        frame_round_trip(ToTab::SetUserAgent {
            user_agent: Some("Mozilla/5.0 (custom UA // IronSurf-测试)".into()),
        });
        let preset = builtin_user_agent_label(
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        );
        assert_eq!(preset, Some("Googlebot"));
        let msg = frame_round_trip(ToTab::SetUserAgent {
            user_agent: Some(BUILTIN_USER_AGENTS[0].1.to_string()),
        });
        match msg {
            ToTab::SetUserAgent { user_agent } => {
                assert_eq!(user_agent.as_deref(), Some(BUILTIN_USER_AGENTS[0].1));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// The preset table is menu-ordered, every entry is labelable, and only
    /// an **exact** string counts as a preset (lookalikes are custom).
    #[test]
    fn builtin_user_agent_presets() {
        let labels: Vec<&str> = BUILTIN_USER_AGENTS.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "Firefox",
                "Chrome",
                "Bingbot",
                "Googlebot",
                "Android Chrome"
            ]
        );
        for (label, ua) in BUILTIN_USER_AGENTS {
            assert_eq!(builtin_user_agent_label(ua), Some(*label));
            assert!(is_builtin_user_agent(ua));
            assert!(ua.chars().count() < MAX_USER_AGENT_CHARS);
        }
        // Exact match only: case, whitespace and near-misses are custom UAs.
        assert_eq!(builtin_user_agent_label("firefox"), None);
        assert!(!is_builtin_user_agent("firefox"));
        assert_eq!(builtin_user_agent_label(""), None);
        assert!(!is_builtin_user_agent(""));
        assert_eq!(
            builtin_user_agent_label(
                "Mozilla/5.0 (X11; Linux x86_64; rv:999.0) Gecko/20100101 Firefox/999.0"
            ),
            None
        );
        let firefox = BUILTIN_USER_AGENTS[0].1;
        assert_eq!(builtin_user_agent_label(&format!(" {firefox}")), None);
    }

    /// A UA override survives both codecs, and a legacy `containers.json`
    /// written **without** the `user_agent` key still loads (as `None`).
    #[test]
    fn container_user_agent_field_round_trips_and_defaults() {
        for ua in [
            None,
            Some(BUILTIN_USER_AGENTS[4].1.to_string()),
            Some("MyCustomAgent/1.0 ✨".to_string()),
        ] {
            let container = Container {
                id: ContainerId(3),
                name: "work".into(),
                color: "#00ff88".into(),
                is_default: false,
                user_agent: ua.clone(),
                proxy: None,
                private: false,
            };
            frame_round_trip(container.clone());
            let json = serde_json::to_string(&container).expect("to json");
            let back: Container = serde_json::from_str(&json).expect("from json");
            assert_eq!(back, container);
        }

        // Legacy `containers.json` (a raw array of containers, see
        // ironsurf-storage) written before this field existed: no
        // `user_agent` key at all.
        let legacy = r##"[{"id":1,"name":"personal","color":"#ff5500","is_default":true}]"##;
        let back: Vec<Container> = serde_json::from_str(legacy).expect("legacy json");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].user_agent, None);
        // An explicit null also reads as None.
        let nul = r##"[{"id":1,"name":"personal","color":"#ff5500","is_default":true,"user_agent":null}]"##;
        let back: Vec<Container> = serde_json::from_str(nul).expect("null json");
        assert_eq!(back[0].user_agent, None);
    }

    /// The same contract for the proxy field: round-trips through both
    /// codecs, and a `containers.json` written before `proxy` existed (or
    /// with no proxy at all — plain `user_agent`-era files) still loads.
    #[test]
    fn container_proxy_field_round_trips_and_defaults() {
        for proxy in [None, Some("work-socks".to_string()), Some("home ✨".to_string())] {
            let container = Container {
                id: ContainerId(4),
                name: "lab".into(),
                color: "#0088ff".into(),
                is_default: false,
                user_agent: None,
                proxy: proxy.clone(),
                private: false,
            };
            frame_round_trip(container.clone());
            let json = serde_json::to_string(&container).expect("to json");
            let back: Container = serde_json::from_str(&json).expect("from json");
            assert_eq!(back, container);
        }

        // Legacy files: written before `proxy` existed, in both the
        // pre-UA and the UA-only layouts.
        let legacy = r##"[{"id":1,"name":"personal","color":"#ff5500","is_default":true}]"##;
        let back: Vec<Container> = serde_json::from_str(legacy).expect("legacy json");
        assert_eq!(back[0].proxy, None);
        let ua_only =
            r##"[{"id":1,"name":"p","color":"#f00","is_default":true,"user_agent":"UA/1"}]"##;
        let back: Vec<Container> = serde_json::from_str(ua_only).expect("ua-era json");
        assert_eq!(back[0].proxy, None);
    }

    /// Private browsing: the flag round-trips through both codecs, and a
    /// `containers.json` written before it existed loads as `false` — the
    /// same legacy-file contract as `user_agent` and `proxy`.
    #[test]
    fn container_private_field_round_trips_and_defaults() {
        for private in [false, true] {
            let container = Container {
                id: ContainerId(5),
                name: "incognito".into(),
                color: "#aa00ff".into(),
                is_default: false,
                user_agent: None,
                proxy: None,
                private,
            };
            frame_round_trip(container.clone());
            let json = serde_json::to_string(&container).expect("to json");
            let back: Container = serde_json::from_str(&json).expect("from json");
            assert_eq!(back, container);
        }

        // Any file from before the field — pre-UA, UA-only, proxy-era —
        // loads with private browsing off.
        let legacy = r##"[{"id":1,"name":"personal","color":"#ff5500","is_default":true}]"##;
        let back: Vec<Container> = serde_json::from_str(legacy).expect("legacy json");
        assert!(!back[0].private);
    }

    /// `ToTab::SetDoNotTrack` survives the frame codec, both ways.
    #[test]
    fn set_do_not_track_round_trips() {
        frame_round_trip(ToTab::SetDoNotTrack { enabled: true });
        let msg = frame_round_trip(ToTab::SetDoNotTrack { enabled: false });
        match msg {
            ToTab::SetDoNotTrack { enabled } => assert!(!enabled),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn set_proxy_message_round_trips() {
        frame_round_trip(ToTab::SetProxy { proxy: None });
        for url in [
            "socks5://127.0.0.1:1080",
            "socks5h://user:pa ss%20@proxy.example:1080",
            "http://proxy.internal:3128",
            "https://Secure.Proxy.example",
        ] {
            frame_round_trip(ToTab::SetProxy {
                proxy: Some(url.to_string()),
            });
        }
    }

    #[test]
    fn proxy_scheme_recognises_the_four_schemes() {
        assert_eq!(proxy_scheme("socks5://h:1"), Some("socks5"));
        assert_eq!(proxy_scheme("socks5h://h:1"), Some("socks5h"));
        assert_eq!(proxy_scheme("http://h:1"), Some("http"));
        assert_eq!(proxy_scheme("https://h:1"), Some("https"));
        // Case-insensitive scheme, exact-match prefix.
        assert_eq!(proxy_scheme("SOCKS5H://h"), Some("socks5h"));
        assert_eq!(proxy_scheme("socks4://h"), None);
        assert_eq!(proxy_scheme("socks5:h"), None);
        assert_eq!(proxy_scheme("socks5h"), None);
        assert_eq!(proxy_scheme("ftp://h"), None);
        assert_eq!(proxy_scheme(""), None);
        // "socks5h" must not be stolen by the "socks5" prefix.
        assert_eq!(proxy_scheme("socks5h://x"), Some("socks5h"));
    }

    #[test]
    fn proxy_host_strips_userinfo_and_port() {
        assert_eq!(proxy_host("socks5://proxy.example:1080"), Some("proxy.example"));
        assert_eq!(proxy_host("http://user:pass@proxy:3128"), Some("proxy"));
        assert_eq!(proxy_host("https://[::1]:8443"), Some("::1"));
        assert_eq!(proxy_host("socks5://127.0.0.1"), Some("127.0.0.1"));
        // A path is not a host, and a bare scheme has none.
        assert_eq!(proxy_host("http://proxy/pac"), Some("proxy"));
        assert_eq!(proxy_host("socks5://:1080"), None, "port without host");
        assert_eq!(proxy_host("socks5://"), None);
        assert_eq!(proxy_host("http://@:1"), None);
        assert_eq!(proxy_host("gopher://host"), None);
    }

    #[test]
    fn validate_proxy_url_accepts_and_trims() {
        assert_eq!(
            validate_proxy_url("  socks5://127.0.0.1:1080 ").as_deref(),
            Ok("socks5://127.0.0.1:1080")
        );
        assert_eq!(
            validate_proxy_url("HTTPS://User:p@ss/proxy.example").as_deref(),
            Ok("HTTPS://User:p@ss/proxy.example")
        );
    }

    #[test]
    fn validate_proxy_url_rejects_the_obvious() {
        assert_eq!(validate_proxy_url(""), Err(ProxyError::EmptyUrl));
        assert_eq!(validate_proxy_url("   \t"), Err(ProxyError::EmptyUrl));
        assert_eq!(
            validate_proxy_url("socks4://proxy:1080"),
            Err(ProxyError::UnsupportedScheme)
        );
        assert_eq!(
            validate_proxy_url("proxy.example:1080"),
            Err(ProxyError::UnsupportedScheme)
        );
        assert_eq!(
            validate_proxy_url("socks5://:1080"),
            Err(ProxyError::MissingHost)
        );
        assert_eq!(
            validate_proxy_url("http://proxy:3128 /x"),
            Err(ProxyError::InvalidCharacters),
            "whitespace anywhere is fatal"
        );
        assert_eq!(
            validate_proxy_url("http://proxy\n:3128"),
            Err(ProxyError::InvalidCharacters)
        );
        let long = format!("socks5://{}:1080", "a".repeat(MAX_PROXY_URL_CHARS));
        assert_eq!(
            validate_proxy_url(&long),
            Err(ProxyError::TooLong {
                length: long.chars().count(),
                max: MAX_PROXY_URL_CHARS,
            })
        );
        // The boundary itself passes.
        let exact = format!(
            "socks5://{}",
            "a".repeat(MAX_PROXY_URL_CHARS - "socks5://".len())
        );
        assert_eq!(exact.chars().count(), MAX_PROXY_URL_CHARS);
        assert!(validate_proxy_url(&exact).is_ok());
    }

    #[test]
    fn proxy_def_round_trips() {
        let def = ProxyDef {
            name: "work-socks".into(),
            url: "socks5h://proxy.corp:1080".into(),
        };
        frame_round_trip(def.clone());
        let json = serde_json::to_string(&def).expect("to json");
        let back: ProxyDef = serde_json::from_str(&json).expect("from json");
        assert_eq!(back, def);
    }

    #[test]
    fn from_tab_all_variants_round_trip() {
        frame_round_trip(FromTab::Ready { tab_id: TabId(3) });
        frame_round_trip(FromTab::Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        });
        frame_round_trip(FromTab::TitleChanged {
            title: Some("t".into()),
        });
        frame_round_trip(FromTab::TitleChanged { title: None });
        frame_round_trip(FromTab::UrlChanged {
            url: Some("https://example.com".into()),
        });
        frame_round_trip(FromTab::UrlChanged { url: None });
        frame_round_trip(FromTab::LoadingChanged { loading: true });
        frame_round_trip(FromTab::LoadingChanged { loading: false });
        frame_round_trip(FromTab::HistoryChanged {
            can_go_back: true,
            can_go_forward: false,
        });
        frame_round_trip(FromTab::StatusText {
            text: Some("s".into()),
        });
        frame_round_trip(FromTab::StatusText { text: None });
        frame_round_trip(FromTab::OpenLinkInNewTab {
            url: "https://example.com/link".into(),
        });
        frame_round_trip(FromTab::ConsoleLog {
            source: "js".into(),
            level: "warn".into(),
            message: "m".into(),
        });
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "GET".into(),
                url: "https://example.com/".into(),
                status: Some(301),
                response_bytes: Some(0),
                duration_ms: Some(7),
                error: None,
                initiator: Some("document".into()),
            },
        });
        frame_round_trip(FromTab::ScrollState {
            state: ScrollState {
                y: 0.0,
                range: 0.0,
                viewport_h: 600.0,
            },
        });
        frame_round_trip(FromTab::ScrollState {
            state: ScrollState {
                y: 1234.5,
                range: 2000.0,
                viewport_h: 600.0,
            },
        });
        frame_round_trip(FromTab::CursorChanged {
            cursor: CursorIcon::Default,
        });
        frame_round_trip(FromTab::CursorChanged {
            cursor: CursorIcon::Pointer,
        });
        frame_round_trip(FromTab::DownloadStart { hint: hint(1) });
        frame_round_trip(FromTab::DownloadChunk {
            id: 1,
            bytes: vec![1, 2, 3],
        });
        frame_round_trip(FromTab::DownloadEnd {
            id: 1,
            error: Some("boom".into()),
        });
        frame_round_trip(FromTab::Closed);
        frame_round_trip(FromTab::Error {
            message: "boom".into(),
        });
        frame_round_trip(FromTab::ContextMenu {
            x: 120.5,
            y: 64.25,
            target: ContextTarget {
                link: Some("https://example.com/a".into()),
                image: Some("https://example.com/i.png".into()),
                video: Some("https://example.com/v.mp4".into()),
                video_playing: true,
                selection: Some("selected words".into()),
                editable: true,
            },
        });
        frame_round_trip(FromTab::ContextMenu {
            x: 0.0,
            y: 0.0,
            target: ContextTarget::default(),
        });
        frame_round_trip(FromTab::ClipboardText {
            text: "copied".into(),
        });
        frame_round_trip(FromTab::DialogRequest {
            dialog_id: 1,
            kind: DialogKind::Alert,
            message: "Page says hi".into(),
            default: None,
        });
    }

    /// The two things both ends read off [`ScrollState`]: a page that fits its
    /// window never gets a bar, and the tab's change gate ignores sub-pixel
    /// movement but not a whole pixel of it.
    #[test]
    fn scroll_state_reports_only_real_scroll() {
        let fits = ScrollState {
            y: 0.0,
            range: 0.0,
            viewport_h: 600.0,
        };
        assert!(!fits.is_scrollable());
        let scrolls = ScrollState {
            y: 0.0,
            range: 1200.0,
            viewport_h: 600.0,
        };
        assert!(scrolls.is_scrollable());
        assert!(
            !scrolls.same_within(&fits, 0.5),
            "a range change is not jitter"
        );
        assert!(scrolls.same_within(&scrolls, 0.5));
        let nudged = ScrollState { y: 0.4, ..scrolls };
        assert!(scrolls.same_within(&nudged, 0.5), "half a pixel is jitter");
        assert!(
            !scrolls.same_within(&ScrollState { y: 1.0, ..scrolls }, 0.5),
            "a whole pixel is a scroll"
        );
    }

    /// Every dialog kind survives the codec with its id/message/default
    /// intact (the prompt default travels as `Some`, alert/confirm as `None`).
    #[test]
    fn dialog_requests_round_trip() {
        frame_round_trip(FromTab::DialogRequest {
            dialog_id: 0,
            kind: DialogKind::Alert,
            message: "".into(),
            default: None,
        });
        frame_round_trip(FromTab::DialogRequest {
            dialog_id: 7,
            kind: DialogKind::Confirm,
            message: "Leave?".into(),
            default: None,
        });
        let msg = frame_round_trip(FromTab::DialogRequest {
            dialog_id: 42,
            kind: DialogKind::Prompt,
            message: "Your name:".into(),
            default: Some("Ada".into()),
        });
        match msg {
            FromTab::DialogRequest {
                dialog_id,
                kind,
                message,
                default,
            } => {
                assert_eq!(dialog_id, 42);
                assert_eq!(kind, DialogKind::Prompt);
                assert_eq!(message, "Your name:");
                assert_eq!(default.as_deref(), Some("Ada"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// `source` is one of the four documented DevTools origins; every one of
    /// them must survive the frame codec with `level`/`message` intact.
    #[test]
    fn console_log_source_round_trips() {
        for source in ["js", "css", "dom", "page"] {
            let msg = frame_round_trip(FromTab::ConsoleLog {
                source: source.into(),
                level: "error".into(),
                message: "unclosed string".into(),
            });
            match msg {
                FromTab::ConsoleLog {
                    source: s,
                    level,
                    message,
                } => {
                    assert_eq!(s, source);
                    assert_eq!(level, "error");
                    assert_eq!(message, "unclosed string");
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    #[test]
    fn network_event_round_trips() {
        // A completed request: status + sizes, no error.
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "GET".into(),
                url: "https://example.com/style.css".into(),
                status: Some(200),
                response_bytes: Some(4096),
                duration_ms: Some(42),
                error: None,
                initiator: Some("resource".into()),
            },
        });
        // A failed request: no status, an error instead.
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "POST".into(),
                url: "https://example.com/api".into(),
                status: None,
                response_bytes: None,
                duration_ms: None,
                error: Some("connection refused".into()),
                initiator: Some("xhr".into()),
            },
        });
        // A bare minimum event (only method + url known).
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "HEAD".into(),
                url: "ironsurf://welcome".into(),
                status: None,
                response_bytes: None,
                duration_ms: None,
                error: None,
                initiator: None,
            },
        });
    }

    #[test]
    fn to_tab_all_variants_round_trip() {
        frame_round_trip(ToTab::Navigate { url: default_url() });
        frame_round_trip(ToTab::DownloadUrl {
            url: "https://example.com/file.zip".into(),
            suggested_name: Some("file.zip".into()),
        });
        frame_round_trip(ToTab::Reload);
        frame_round_trip(ToTab::Stop);
        frame_round_trip(ToTab::Back);
        frame_round_trip(ToTab::Forward);
        frame_round_trip(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 2.0,
        });
        frame_round_trip(ToTab::SetZoom { zoom: 1.0 });
        frame_round_trip(ToTab::SetZoom { zoom: 1.33 });
        frame_round_trip(ToTab::SetZoom { zoom: 0.5 });
        frame_round_trip(ToTab::SetVisible { visible: false });
        for kind in [
            PointerKind::Move,
            PointerKind::Down,
            PointerKind::Up,
            PointerKind::Over,
            PointerKind::Out,
        ] {
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                frame_round_trip(ToTab::Pointer(PointerEvent {
                    kind,
                    button,
                    ..pointer_event()
                }));
            }
        }
        frame_round_trip(ToTab::Key(key_event()));
        frame_round_trip(ToTab::Scroll(scroll_event()));
        frame_round_trip(ToTab::Clipboard {
            text: "clip".into(),
        });
        frame_round_trip(ToTab::ContextAction(ContextAction::Paste {
            text: "hi".into(),
        }));
        frame_round_trip(ToTab::ContextAction(ContextAction::ToggleVideo));
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 1,
            confirmed: true,
            text: None,
        });
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 2,
            confirmed: false,
            text: None,
        });
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 3,
            confirmed: true,
            text: Some("Ada Lovelace".into()),
        });
        // An accepted empty prompt box is Some(""), not None.
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 4,
            confirmed: true,
            text: Some(String::new()),
        });
        frame_round_trip(ToTab::Destroy);
    }

    /// The userscript contract rides the wire intact: empty set (clear),
    /// one script with several patterns, and a unicode name.
    #[test]
    fn set_userscripts_round_trips() {
        frame_round_trip(ToTab::SetUserscripts { scripts: vec![] });
        frame_round_trip(ToTab::SetUserscripts {
            scripts: vec![Userscript {
                id: 1,
                name: "Darkify".into(),
                patterns: vec!["*example.com".into(), "example.org/*".into()],
                code: "console.log('hi')".into(),
                enabled: true,
            }],
        });
        let msg = frame_round_trip(ToTab::SetUserscripts {
            scripts: vec![
                Userscript {
                    id: 7,
                    name: "🔧 Т工具 — ünïcode Ñame 日".into(),
                    patterns: vec![],
                    code: "".into(),
                    enabled: false,
                },
                Userscript {
                    id: 8,
                    name: "second".into(),
                    patterns: vec!["*".into()],
                    code: "var x = 1;".into(),
                    enabled: true,
                },
            ],
        });
        match msg {
            ToTab::SetUserscripts { scripts } => {
                assert_eq!(scripts.len(), 2);
                assert_eq!(scripts[0].name, "🔧 Т工具 — ünïcode Ñame 日");
                assert!(!scripts[0].enabled);
                assert_eq!(scripts[1].patterns, vec!["*".to_string()]);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// Table-driven coverage of the documented URL-pattern semantics: exact
    /// match ±case, wildcard host boundary (never a lookalike), wildcard +
    /// path, bare `*`, no-scheme sugar, blank patterns, internal schemes.
    #[test]
    fn userscript_url_matching() {
        type Case = (&'static str, &'static str, bool);
        let cases: &[Case] = &[
            // Exact patterns: full-URL string equality, case-insensitive.
            ("https://example.com/x", "https://example.com/x", true),
            ("https://example.com/x", "https://example.com/y", false),
            ("HTTPS://Example.COM/X", "https://example.com/x", true),
            ("https://example.com/x", "https://example.com/x?q=1", false),
            // Bare `*` matches every URL (any scheme).
            ("*", "https://example.com/", true),
            ("*", "data:text/html,hi", true),
            ("*", "ironsurf://welcome", true),
            // Wildcard domain boundary: exact host and subdomains match.
            ("*example.com", "https://example.com/", true),
            ("*example.com", "http://x.example.com/page", true),
            (
                "*example.com",
                "https://a.b.example.com/deep/path?q=1",
                true,
            ),
            ("*example.com", "https://EXAMPLE.COM/Upper/Path", true),
            // …a lookalike suffix NEVER matches (host boundary rule).
            ("*example.com", "https://notexample.com/", false),
            ("*example.com", "https://evil.com/?u=example.com", false),
            ("*example.com", "https://xexample.com/", false),
            // Leading `*.` / `.` on the domain behave like `*example.com`.
            ("*.example.com", "https://example.com/", true),
            ("*.example.com", "https://sub.example.com/a", true),
            ("*.example.com", "https://notexample.com/", false),
            ("*sub.example.com", "https://example.com/", false),
            // Wildcard + path: the lowercased URL must end with the path part.
            ("*example.com/api", "https://example.com/api", true),
            ("*example.com/api", "http://sub.example.com/v1/api", true),
            ("*example.com/api", "https://example.com/api/extra", false),
            ("*example.com/api", "https://notexample.com/api", false),
            ("*example.com/", "https://example.com/anything", true),
            // No-scheme sugar: `host` = that exact host, any path; `host/path`
            // = path prefix.
            ("example.com", "https://example.com/", true),
            ("example.com", "https://example.com/deep/path?x=1", true),
            ("example.com", "https://x.example.com/", false),
            ("example.com", "https://notexample.com/", false),
            ("example.com/docs", "https://example.com/docs", true),
            ("example.com/docs", "https://example.com/docs/intro", true),
            ("example.com/docs", "https://example.com/other", false),
            ("example.com/docs", "http://example.com/docs", true),
            ("example.com/docs", "https://notexample.com/docs", false),
            // Non-http(s) schemes never match sugar or wildcard domains…
            ("example.com", "ironsurf://example.com/docs", false),
            ("*example.com", "ironsurf://example.com/", false),
            ("*example.com", "data:text/html,example.com", false),
            // …but exact patterns match internal schemes literally.
            ("ironsurf://welcome", "ironsurf://welcome", true),
            ("ironsurf://welcome", "ironsurf://settings", false),
            (
                "ironsurf://settings/plugins",
                "ironsurf://settings/plugins",
                true,
            ),
            // Blank patterns match nothing.
            ("", "https://example.com/", false),
            ("   ", "https://example.com/", false),
        ];
        for (pattern, url, want) in cases {
            assert_eq!(
                userscript_matches(pattern, url),
                *want,
                "pattern {pattern:?} vs url {url:?} (expected {want})"
            );
        }
    }

    /// `userscripts_for_url` returns enabled scripts with a matching pattern
    /// only — disabled scripts, non-matching patterns, and empty/whitespace
    /// pattern lists are all excluded.
    #[test]
    fn userscripts_for_url_filters_enabled_and_patterns() {
        let scripts = vec![
            Userscript {
                id: 1,
                name: "on-example".into(),
                patterns: vec!["*example.com".into()],
                code: "1".into(),
                enabled: true,
            },
            Userscript {
                id: 2,
                name: "disabled".into(),
                patterns: vec!["*".into()],
                code: "2".into(),
                enabled: false,
            },
            Userscript {
                id: 3,
                name: "no-patterns".into(),
                patterns: vec![],
                code: "3".into(),
                enabled: true,
            },
            Userscript {
                id: 4,
                name: "blank-patterns".into(),
                patterns: vec!["".into(), "  ".into()],
                code: "4".into(),
                enabled: true,
            },
            Userscript {
                id: 5,
                name: "other-host".into(),
                patterns: vec!["example.org".into()],
                code: "5".into(),
                enabled: true,
            },
            Userscript {
                id: 6,
                name: "multi-pattern".into(),
                patterns: vec![
                    "example.org".into(),
                    "  ".into(),
                    "*docs.example.net/guide".into(),
                ],
                code: "6".into(),
                enabled: true,
            },
        ];

        let on_example = userscripts_for_url(&scripts, "https://example.com/page");
        let ids: Vec<u64> = on_example.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![1], "only the enabled matching script");

        let on_docs = userscripts_for_url(&scripts, "https://docs.example.net/guide");
        assert_eq!(
            on_docs.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![6],
            "a later pattern in the list matches"
        );

        assert!(userscripts_for_url(&scripts, "https://ironsurf.internal/x").is_empty());
        assert!(userscripts_for_url(&scripts, "data:text/html,x").is_empty());
        assert!(userscripts_for_url(&[], "https://example.com/").is_empty());

        // The returned references point at the original scripts.
        assert_eq!(on_example[0].code, "1");
    }

    #[test]
    fn key_event_all_modifier_combinations_round_trip() {
        let base = key_event();
        for bits in 0u8..=0b1111 {
            let mut ev = base.clone();
            ev.mods = Modifiers::new(bits);
            frame_round_trip(ev);
        }
    }

    #[test]
    fn pointer_events_carry_their_modifiers() {
        // The click chords (Ctrl-left / ⌘-left on a link) are decided tab-side
        // from the modifiers the shell saw, so they have to survive the wire —
        // and a peer that sends the older four-field event still decodes.
        for bits in 0u8..=0b1111 {
            frame_round_trip(ToTab::Pointer(PointerEvent {
                mods: Modifiers::new(bits),
                ..pointer_event()
            }));
        }
    }

    #[test]
    fn the_action_modifier_is_ctrl_off_macos_and_command_on_it() {
        // `action()` is what a click chord tests: Ctrl yes, Alt/Shift on their
        // own never, and on macOS the Command key whichever flag the backend
        // reported it in (Slint swaps the two, other backends do not).
        assert!(!Modifiers::NONE.action());
        assert!(!Modifiers::NONE.with_alt().action());
        assert!(!Modifiers::NONE.with_shift().action());
        assert!(Modifiers::NONE.with_ctrl().action());
        if cfg!(target_os = "macos") {
            assert!(Modifiers::NONE.with_meta().action(), "⌘ arrives as meta too");
        } else {
            assert!(!Modifiers::NONE.with_meta().action(), "the Windows key is not Ctrl");
        }
    }

    #[test]
    fn modifiers_helpers() {
        assert!(Modifiers::NONE.empty());
        assert!(!Modifiers::NONE.with_ctrl().empty());
        let m = Modifiers::NONE
            .with_ctrl()
            .with_shift()
            .with_alt()
            .with_meta();
        assert!(m.ctrl() && m.shift() && m.alt() && m.meta());
        assert_eq!(m.bits(), 0b1111);
        assert_eq!(
            Modifiers::NONE.with_ctrl() | Modifiers::NONE.with_alt(),
            Modifiers::NONE.with_ctrl().with_alt()
        );
        assert!(Modifiers::default().empty());
    }

    #[test]
    fn every_cursor_icon_round_trips() {
        // The full vocabulary the tab may report — the shell must be able to
        // decode any of it (task: hover cursor).
        let all = [
            CursorIcon::Default,
            CursorIcon::Hidden,
            CursorIcon::Help,
            CursorIcon::Pointer,
            CursorIcon::Progress,
            CursorIcon::Wait,
            CursorIcon::Crosshair,
            CursorIcon::Text,
            CursorIcon::VerticalText,
            CursorIcon::Alias,
            CursorIcon::Copy,
            CursorIcon::Move,
            CursorIcon::NoDrop,
            CursorIcon::NotAllowed,
            CursorIcon::Grab,
            CursorIcon::Grabbing,
            CursorIcon::ColResize,
            CursorIcon::RowResize,
            CursorIcon::NResize,
            CursorIcon::EResize,
            CursorIcon::SResize,
            CursorIcon::WResize,
            CursorIcon::NeResize,
            CursorIcon::NwResize,
            CursorIcon::SeResize,
            CursorIcon::SwResize,
            CursorIcon::EwResize,
            CursorIcon::NsResize,
            CursorIcon::NeswResize,
            CursorIcon::NwseResize,
        ];
        assert_eq!(all.len(), 30, "every CursorIcon variant is covered");
        for cursor in all {
            frame_round_trip(FromTab::CursorChanged { cursor });
        }
        assert_eq!(CursorIcon::default(), CursorIcon::Default);
    }

    #[test]
    fn multiple_frames_stream_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ToTab::Reload).unwrap();
        write_frame(
            &mut buf,
            &ToTab::Navigate {
                url: "https://a.test".into(),
            },
        )
        .unwrap();
        write_frame(&mut buf, &FromTab::Closed).unwrap();

        let mut cur = std::io::Cursor::new(&buf);
        assert_eq!(read_frame::<_, ToTab>(&mut cur).unwrap(), ToTab::Reload);
        assert_eq!(
            read_frame::<_, ToTab>(&mut cur).unwrap(),
            ToTab::Navigate {
                url: "https://a.test".into()
            }
        );
        assert_eq!(read_frame::<_, FromTab>(&mut cur).unwrap(), FromTab::Closed);
        // Stream exhausted -> error, not a hang.
        assert!(read_frame::<_, FromTab>(&mut cur).is_err());
    }

    #[test]
    fn truncated_frame_is_error() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &ToTab::Navigate {
                url: "https://example.com/x".into(),
            },
        )
        .unwrap();
        // Cut the payload in half (length prefix intact).
        let cut = buf.len() - 5;
        let mut truncated = &buf[..cut];
        let res = read_frame::<_, ToTab>(&mut truncated);
        assert!(res.is_err(), "truncated frame must error, got {res:?}");

        // Truncated length prefix (fewer than 4 bytes) is also an error.
        let res2 = read_frame::<_, ToTab>(&mut &[0u8, 1, 2][..]);
        assert!(res2.is_err());
    }

    #[test]
    fn empty_reader_is_error() {
        let res = read_frame::<_, FromTab>(&mut std::io::empty());
        assert!(res.is_err());
    }

    #[test]
    fn oversize_frame_is_rejected() {
        // Craft a header claiming more than MAX_FRAME_BYTES.
        let header = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        let res = read_frame::<_, ToTab>(&mut &header[..]);
        assert!(
            res.is_err(),
            "oversize length prefix must error, got {res:?}"
        );

        let u32_max = u32::MAX.to_le_bytes();
        let res2 = read_frame::<_, ToTab>(&mut &u32_max[..]);
        assert!(res2.is_err());
        assert_eq!(res2.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

        // A frame exactly at MAX_FRAME_BYTES is not rejected by the guard
        // (it then fails on truncated payload, which is a different path).
        let exact = (MAX_FRAME_BYTES as u32).to_le_bytes();
        let res3 = read_frame::<_, ToTab>(&mut &exact[..]);
        assert!(res3.is_err());
        assert_ne!(res3.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn garbage_payload_is_invalid_data() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[9u8; 4]); // plausible tiny length
        buf.extend_from_slice(b"not bincode at all, definitely garbage bytes!!");
        let res = read_frame::<_, ToTab>(&mut &buf[..]);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn default_url_is_the_internal_welcome_page() {
        assert_eq!(default_url(), "ironsurf://welcome");
    }

    // --- downloads (task: file downloads) ---------------------------------

    fn hint(id: u64) -> DownloadHint {
        DownloadHint {
            id,
            url: "https://example.com/files/report.zip".into(),
            name: Some("report.zip".into()),
            media_type: Some("application/zip".into()),
            trigger: DownloadTrigger::Disposition,
        }
    }

    #[test]
    fn download_messages_round_trip() {
        for trigger in [
            DownloadTrigger::Disposition,
            DownloadTrigger::MediaType,
            DownloadTrigger::AnchorAttribute,
            DownloadTrigger::Requested,
        ] {
            let mut hint = hint(7);
            hint.trigger = trigger;
            frame_round_trip(FromTab::DownloadStart { hint });
        }
        frame_round_trip(FromTab::DownloadStart { hint: none_name() });
        frame_round_trip(FromTab::DownloadChunk {
            id: 3,
            bytes: vec![0, 128, 255, 7],
        });
        // An empty chunk (a zero-byte body) and an error both travel fine.
        frame_round_trip(FromTab::DownloadChunk {
            id: 3,
            bytes: Vec::new(),
        });
        frame_round_trip(FromTab::DownloadEnd { id: 3, error: None });
        frame_round_trip(FromTab::DownloadEnd {
            id: 3,
            error: Some("connection refused".into()),
        });
        frame_round_trip(ToTab::DownloadUrl {
            url: "https://example.com/x.bin".into(),
            suggested_name: Some("x.bin".into()),
        });
        frame_round_trip(ToTab::DownloadUrl {
            url: "https://example.com/x.bin".into(),
            suggested_name: None,
        });
        // A whole chunk stream survives a real pipe: encode N frames, decode
        // them back in order (what the shell does to rebuild a file).
        let mut stream = Vec::new();
        let chunks: Vec<Vec<u8>> = (0..3)
            .map(|n| (0..64).map(|i| (n * 64 + i) as u8).collect())
            .collect();
        for bytes in &chunks {
            write_frame(
                &mut stream,
                &FromTab::DownloadChunk {
                    id: 9,
                    bytes: bytes.clone(),
                },
            )
            .unwrap();
        }
        write_frame(&mut stream, &FromTab::DownloadEnd { id: 9, error: None }).unwrap();
        let mut cursor = stream.as_slice();
        let mut rebuilt = Vec::new();
        loop {
            let msg: FromTab = read_frame(&mut cursor).unwrap();
            match msg {
                FromTab::DownloadChunk { bytes, .. } => rebuilt.extend_from_slice(&bytes),
                FromTab::DownloadEnd { id, error } => {
                    assert_eq!(id, 9);
                    assert_eq!(error, None);
                    break;
                }
                msg => unreachable!("{msg:?}"), // the loop breaks on DownloadEnd
            }
        }
        assert_eq!(rebuilt.len(), 192);
        assert_eq!(rebuilt, chunks.concat());
    }

    fn none_name() -> DownloadHint {
        DownloadHint {
            id: 8,
            url: "https://example.com/thing".into(),
            name: None,
            media_type: None,
            trigger: DownloadTrigger::MediaType,
        }
    }

    #[test]
    fn download_names_are_suggested_not_trusted() {
        // A `*`-suffixed parameter wins over its unsuffixed twin, RFC 6266.
        assert_eq!(
            content_disposition_filename(
                r#"attachment; filename="bad.zip"; filename*=UTF-8''good.zip"#
            ),
            Some("good.zip".into())
        );
        // Percent-decoded, quoting ignored, case-insensitive parameters.
        assert_eq!(
            content_disposition_filename("ATTACHMENT; filename*=UTF-8''report%20%C3%A9.zip"),
            Some("report é.zip".into())
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=\"quarterly Q3.pdf\""),
            Some("quarterly Q3.pdf".into())
        );
        // The bare spelling backslash-escapes quotes: the header meant
        // `report"2019.zip`, and `"` is not a file-name character.
        assert_eq!(
            content_disposition_filename(r#"attachment; filename="report\"2019.zip""#),
            Some("report".into())
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=unquoted.tar.gz"),
            Some("unquoted.tar.gz".into())
        );
        // No filename at all — the disposition alone is not a name.
        assert_eq!(content_disposition_filename("attachment"), None);
        assert_eq!(
            content_disposition_filename("inline; filename=page.html"),
            Some("page.html".into())
        );
        // Hostile values: a suggestion is never a path.
        for hostile in [
            r#"attachment; filename="../../../.ssh/authorized_keys""#,
            r#"attachment; filename*=UTF-8''..%2F..%2F..%2Fetc%2Fpasswd"#,
            r#"attachment; filename="/etc/shadow""#,
            r#"attachment; filename="..""#,
            r#"attachment; filename="\r\n""#,
        ] {
            let named = content_disposition_filename(hostile);
            assert!(
                named.is_none() || !named.as_ref().unwrap().contains(['/', '\\']),
                "hostile value {hostile:?} → {named:?}"
            );
            if let Some(name) = named {
                assert!(
                    !name.starts_with("..") || name == "authorized_keys",
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn file_names_survive_their_own_suggestions() {
        assert_eq!(sanitize_file_name("report.zip"), Some("report.zip".into()));
        assert_eq!(
            sanitize_file_name("  spaced name .tar.gz  "),
            Some("spaced name .tar.gz".into())
        );
        // A separator ends the useful part; a leading one leaves nothing.
        assert_eq!(sanitize_file_name("a/b"), Some("a".into()));
        assert_eq!(sanitize_file_name("/a"), None);
        assert_eq!(sanitize_file_name(".."), None);
        assert_eq!(sanitize_file_name("."), None);
        assert_eq!(sanitize_file_name(""), None);
        // dotfiles and all-dot names are not files anyone wants.
        assert_eq!(sanitize_file_name(".hidden"), Some("hidden".into()));
        assert_eq!(sanitize_file_name("...."), None);
        // A long name is clamped, never rejected.
        let long = format!("{}{}.zip", "a".repeat(300), "");
        let clamped = sanitize_file_name(&long).expect("clamped");
        assert_eq!(clamped.chars().count(), MAX_DOWNLOAD_NAME);
        assert!(clamped.ends_with('a'));
        // A unicode name keeps its characters (a name is not an ASCII-only field).
        assert_eq!(
            sanitize_file_name("☃.zip"),
            Some("☃.zip".into()),
            "unicode survives"
        );
        assert_eq!(
            sanitize_file_name("<con>:?.zip"),
            None,
            "every reserved char kills the head"
        );
    }

    #[test]
    fn names_come_from_urls_when_the_server_gave_none() {
        assert_eq!(
            filename_from_url("https://example.com/files/report.zip"),
            Some("report.zip".into())
        );
        assert_eq!(
            filename_from_url("https://example.com/files/report.zip?size=full#top"),
            Some("report.zip".into())
        );
        assert_eq!(
            filename_from_url("https://example.com/a/b/quarterly%E2%98%83.pdf"),
            Some("quarterly☃.pdf".into())
        );
        // A trailing slash means the last segment is the directory above it.
        assert_eq!(
            filename_from_url("https://example.com/files/report/"),
            Some("report".into())
        );
        // Nothing to name after: a bare host, a root-only path.
        assert_eq!(filename_from_url("https://example.com"), None);
        assert_eq!(filename_from_url("https://example.com/"), None);
        assert_eq!(filename_from_url("https://example.com/?q=x"), None);
        assert_eq!(filename_from_url("data:application/zip;base64,AAAA"), None);
        assert_eq!(filename_from_url("https://example.com/"), None);
        // A dot-dot in the path cannot escape: the name is the last segment.
        assert_eq!(
            filename_from_url("https://example.com/../../etc/shadow"),
            Some("shadow".into())
        );
    }

    #[test]
    fn only_media_types_that_are_not_pages_download() {
        let files = [
            "application/octet-stream",
            "application/zip",
            "application/x-debian-package",
            "application/x-msdownload",
            "application/gzip",
            "application/x-gzip",
            "application/pdf",
            "application/wasm",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/java-archive",
            "application/x-iso9660-image",
            "application/zip; charset=binary",
            "APPLICATION/OCTET-STREAM",
            "font/woff2",
            "model/gltf-binary",
        ];
        for media in files {
            assert!(is_download_media_type(media), "{media} should download");
        }
        let pages = [
            "text/html",
            "text/plain",
            "text/css",
            "text/csv",
            "text/html; charset=utf-8",
            "application/xml",
            "application/xhtml+xml",
            "application/atom+xml",
            "application/rss+xml",
            "application/json",
            "application/geo+json",
            "application/ld+json",
            "application/javascript",
            "image/png",
            "image/svg+xml",
            "audio/mp4",
            "video/mp4",
            "video/webm",
            "",
            "foo",
            "application",
            "app/zip",
        ];
        for media in pages {
            assert!(!is_download_media_type(media), "{media} should render");
        }
        // The two `application/…` types the tab shows as source text today
        // stay pages (see the table in the function docs).
        assert!(!is_download_media_type("application/json"));
        assert!(!is_download_media_type("application/javascript"));
    }
}
