//! Runtime glue: Slint `AppWindow` ⇄ tab processes ⇄ container registry.
//!
//! All state lives inside `Rc<Core>` so the Slint callbacks, the message
//! drain timer and the spinner timer can share it mutably (RefCells +
//! methods on `Rc<Core>`). `Frame` messages are coalesced to the newest per
//! drain tick; the model is pushed once per tick when anything changed.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::sync::mpsc::{channel, Receiver, Sender};

use ironsurf_common::{debug, error, info, trace, warn};
use ironsurf_common::{
    default_url, ContextAction, ContextTarget, ContainerId, DialogKind, FromTab, HistoryVisit,
    ScrollState, TabId, ToTab, Userscript, HISTORY_URL, MAX_HISTORY_PAGE_ROWS,
};
use ironsurf_storage::default_data_root;
use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    Timer, TimerMode, VecModel, Weak,
};
// The language's `MouseCursor` enum (the type of the page TouchArea's
// `mouse-cursor` / the generated `set_page_cursor`): a builtin enum, re-
// exported for Rust code next to the generated accessors themselves.
use slint::private_unstable_api::re_exports::MouseCursor;

use crate::archive;
use crate::archive_server::{self, ServeEvent, ServeHandle, Spec};
use crate::classify::classify_input;
use crate::downloads;
use crate::keymap::{self, Shortcut};
use crate::state::{
    net_row_clipboard, scrollbar_thumb, DownloadRow, DownloadStore, InspectorUi, TabState,
    SCROLLBAR_HOLD,
};
use crate::tabs::{PumpedMessage, TabProc};
use crate::uisettings::UiSettings;
use crate::ProxyRow;
use crate::{
    AppWindow, ContainerInfo, DownloadItem, MenuItem, SuggestItem, TabInfo,
};
// The Slint-side row structs the DevTools panel renders (`state`'s own
// view-models of the same name are converted into these).
use crate::{ConsoleRow, CrumbItem, InspectRow, NetRow, Tok};
// The pure userscript-menu helpers (src/userscripts.rs).
use crate::userscripts::{build_new_userscript, enabled_userscripts, parse_patterns, userscript_rows};
// The pure User-Agent-switcher helpers (src/user_agents.rs).
use crate::user_agents::{normalize_custom_ua, ua_display, ua_menu_rows};

/// The tab's built-in User-Agent — what a container **without** an override
/// sends, and what the UA menu's Copy button copies in that case. Duplicate
/// of `ironsurf-tab`'s `netmod::USER_AGENT` — `ironsurf-tab` depends on this crate
/// to build the single `ironsurf` binary, so this crate must not depend back on
/// it; keep the two strings in sync.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; IronSurf/0.1)";

/// The stored UA override of `container` (trimmed; blank stored values are
/// treated as "no override", matching what `Store::set_container_user_agent`
/// writes). `None` = the container uses the tab's built-in default.
fn container_user_agent(store: &ironsurf_storage::Store, container: ContainerId) -> Option<String> {
    store
        .container(container)
        .and_then(|c| c.user_agent.clone())
        .filter(|ua| !ua.trim().is_empty())
}

/// The stored **proxy definition name** of `container` (blank stored values
/// are "no proxy", matching what `Store::set_container_proxy` writes).
/// The name refers to a [`ironsurf_common::ProxyDef`] in the shell's
/// settings — the container stores *which* proxy, not how to reach it
/// (task: proxies). `None` = direct connections.
fn container_proxy(store: &ironsurf_storage::Store, container: ContainerId) -> Option<String> {
    store
        .container(container)
        .and_then(|c| c.proxy.clone())
        .filter(|n| !n.trim().is_empty())
}

/// Whether `container` browses **privately** (task: private browsing):
/// its tabs send `DNT: 1` (via the `--private` spawn flag and
/// [`ironsurf_common::ToTab::SetDoNotTrack`]) and none of its visits is
/// written to the shared browsing history. An unknown container reads as
/// not private.
fn container_private(store: &ironsurf_storage::Store, container: ContainerId) -> bool {
    store.container(container).is_some_and(|c| c.private)
}

/// Resolve a container's stored definition *name* into the URL live tabs
/// ride: the settings' definition with that exact name whose URL still
/// validates (`validate_proxy_url` guards entry, but `ui.json` may hold
/// hand-edited junk). Everything unresolved — no name, a definition deleted
/// since, an invalid URL — means **direct**: a container never silently
/// rides a route its user did not pick, and the stored name survives so
/// re-adding the definition reconnects it.
fn proxy_url_for(settings: &UiSettings, name: Option<&str>) -> Option<String> {
    let name = name?;
    settings
        .proxies
        .iter()
        .find(|d| d.name == name)
        .and_then(|d| ironsurf_common::validate_proxy_url(&d.url).ok())
}

/// Fixed palette — **keep in sync** with `palette-color()` in ui/ironsurf.slint.
pub const PALETTE: [&str; 7] = [
    "#4a90d9", "#e06666", "#6aa84f", "#f6b26b", "#8e7cc3", "#6d4c41", "#0097a7",
];

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Half-length of the loading-bar sweep, in 4 px steps along the status bar's
/// 96 px track: the lit segment walks positions 0 … 11 out and back, one full
/// round trip every 24 spinner ticks (≈ 2.9 s at 120 ms a tick). Keep in sync
/// with the `load-track` geometry in ui/ironsurf.slint.
pub const LOAD_SWEEP_STEPS: usize = 12;

/// Sweep position of the loading bar after `step` ticks: a triangle wave
/// between the track's ends — out (`0 … 11`), then back (`11 … 0`) — so the
/// segment moves on every tick but the single one where it turns, and an
/// in-flight load never reads as a stalled bar.
/// `LOAD_SWEEP_STEPS` (one track length) is not a position; the far end is
/// `LOAD_SWEEP_STEPS - 1`, where the lit segment shrinks against the track's
/// end instead of hanging off it.
pub fn load_sweep_position(step: usize) -> usize {
    let period = LOAD_SWEEP_STEPS * 2;
    let n = step % period;
    if n < LOAD_SWEEP_STEPS {
        n
    } else {
        (period - 1) - n
    }
}

/// Put the page's overlay scroll bar on the window — or take it off (task:
/// scroll bar). `scroll` is what the tab on screen last said its document
/// viewport is, `moved` the moment that document was last under the reader's
/// wheel: the tab's own report of a *moving offset*, or the shell's wheel click
/// on that page (see `Core::page_wheel`).
///
/// The bar is up while both hold — the page has somewhere to scroll **and** it
/// moved within [`SCROLLBAR_HOLD`]. It answers the scroll and then gets out of
/// the reader's way, and nothing has to schedule the hiding: whoever next
/// pushes the bar (the spinner heartbeat, every 120 ms) finds the hold expired
/// and draws it away. Where the thumb sits is [`scrollbar_thumb`]'s answer, so
/// a page at either end of its range sits at either end of the track.
fn push_scrollbar(ui: &AppWindow, scroll: &ScrollState, moved: Option<Instant>) {
    let thumb = scrollbar_thumb(scroll, ui.get_page_height());
    let visible = thumb.is_some() && moved.is_some_and(|at| at.elapsed() < SCROLLBAR_HOLD);
    if ui.get_scrollbar_visible() != visible {
        ui.set_scrollbar_visible(visible);
    }
    // A page with nothing to scroll has no thumb to place; its stale numbers
    // do nobody any harm while the bar is hidden.
    if let Some((top, height)) = thumb {
        if ui.get_scrollbar_thumb_y() != top {
            ui.set_scrollbar_thumb_y(top);
        }
        if ui.get_scrollbar_thumb_height() != height {
            ui.set_scrollbar_thumb_height(height);
        }
    }
}

struct Proc {
    tab: TabProc,
    state: TabState,
    /// Archive-mode timestamp folder for this tab process (task: archive
    /// mode): taken at spawn — or re-taken the moment archive mode is
    /// switched on for an already-live tab — and used for every
    /// `FromTab::ArchivedFetch` the process ships, so one tab generation
    /// lands in one `<root>/<ISO8601>/…` folder.
    visit_dir: String,
    /// Page zoom factor (Ctrl-`+`/`-`/`0`, shell-owned policy; see
    /// `keymap::zoom_in`/`zoom_out`). Sent to the tab as `ToTab::SetZoom`.
    zoom: f32,
    /// When *this* tab's document last moved under the reader's wheel (task:
    /// scroll bar). The overlay bar over the page is up for
    /// [`SCROLLBAR_HOLD`] after it — per tab, so a wheel that scrolled a
    /// background page cannot keep the page on screen looking busy. `None` =
    /// never since the process started.
    scroll_moved: Option<Instant>,
    /// The child process died without saying `Closed` (crash) or a send hit
    /// a broken pipe. The tab stays in the strip showing its last frame;
    /// `state.error` carries the “has crashed” status-line message.
    crashed: bool,
    /// DevTools panel open for *this* tab (F12 / Ctrl-`I`). Per tab, like the
    /// zoom factor: switching tabs switches the panel to the new tab's feed,
    /// and a freshly opened tab starts closed.
    devtools_open: bool,
    /// A document tree was asked of this tab — its Inspector pane is open, ⌖ is
    /// armed for it, or its context menu asked what is under the cursor — and no
    /// `FromTab::InspectSnapshot` has come back since. That is the pane's
    /// “asking the tab…” state, and what stops the shell from asking one tab for
    /// the same tree on every redraw.
    /// The last `ToTab::SetInspect` sent to this tab: `true` while the tab is
    /// under instruction to keep its Inspector pane’s document up to date. The
    /// “we asked and no snapshot has landed yet” state is `InspectorUi::pending`;
    /// conflating the two means a pane that closes never re-asks when it opens.
    inspect_stream: bool,
    /// The Inspector pane’s view state for this tab: the tree the tab last sent
    /// plus what the shell owns about it — folds, selection, the ⌖ flag. `None`
    /// while nobody has asked this tab for a document. It lives on the tab
    /// record, not in the window, so switching tabs switches documents instead of
    /// repainting the last one under a new title, and a background tab never
    /// keeps a tree it cannot show (≤ 400 KB of it).
    inspector: Option<InspectorUi>,
    /// Which document `inspector` describes. Node ids are positional handles
    /// inside one snapshot of one document, so the window is only ever painted
    /// from a record whose target is the tab on screen.
    inspector_target: Option<TabId>,
    /// Which pane the panel shows for this tab (Console / Network / Inspector).
    devtools_view: DevToolsView,
    /// How tall this tab's DevTools panel is, in logical pixels, as the reader
    /// left it by dragging the panel's top edge. `None` = never dragged, so the
    /// panel keeps the height it was given (`default_devtools_height`); per tab
    /// like the pane and the zoom, so resizing this tab's panel leaves the other
    /// tabs' alone. A height the window outgrew is re-fitted on the way to the
    /// UI (see `clamp_devtools_height`), never here, so the reader's own number
    /// survives a shrink-then-grow.
    devtools_height: Option<f32>,
    /// The Console row this tab has highlighted **for copy** (task: copy in
    /// the DevTools tabs) — an index into the feed, set by clicking a row, and
    /// what the pane's `Copy` button copies. `None` = nothing highlighted.
    dt_sel_console: Option<usize>,
    /// The same for the Network pane.
    dt_sel_network: Option<usize>,
    /// Find-in-page bar open for *this* tab (Ctrl-`F`). Per tab like the panel:
    /// switching tabs switches the bar to the other tab's search, and the
    /// numbers beside it come from that tab (`state.find`).
    find_open: bool,
    /// The downloads this tab process has filed (task: file downloads). Per
    /// tab like the zoom: a tab's downloads **die with its process**, so an
    /// unfinished row is `Failed` and its partial file deleted when the tab
    /// crashes ([`DownloadStore::reap]).
    downloads: DownloadStore,
    /// The "Save page" dialog's armed save for this tab (task: save-page
    /// selector): folder + name picked with Ctrl-`S`, per tab like every
    /// other fact of a tab. `None` until that tab opens the dialog.
    save_target: Option<SaveTarget>,
}

/// What the "Save page" dialog (task: save-page selector) has armed for
/// one tab: the page Ctrl-`S` was pressed on, the folder its save was
/// pointed at in the dialog (defaulting to the ⬇ root, i.e.
/// `$HOME/Downloads`), and the name the dialog holds for it. Per tab like
/// everything about a tab, and **consumed once** by the matching
/// `DownloadStart` (see `Core::download_write`): the chosen folder is never
/// persisted, because the ⬇ root must go on receiving everything else.
#[derive(Clone, Debug)]
struct SaveTarget {
    page: String,
    folder: PathBuf,
    name: Option<String>,
}

/// The shell-side half of a JS dialog being shown (task: window.alert /
/// confirm / prompt): which tab is blocked on the pipe, under which request
/// id, waiting for the user's button. Exactly one can exist at a time — the
/// requesting tab does not answer its stdin while it waits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingDialog {
    tab: TabId,
    dialog_id: u32,
    kind: DialogKind,
    message: String,
    default: Option<String>,
}

impl PendingDialog {
    /// Only a prompt carries text back (the contract: `text` is `Some`
    /// exactly when a prompt was accepted).
    fn prompt(&self) -> bool {
        self.kind == DialogKind::Prompt
    }
}

/// Which DevTools pane the panel shows. Kept as an index (`view_index`) for
/// the Slint property, so the UI stays a number and Rust keeps the names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DevToolsView {
    #[default]
    Console,
    Network,
    /// The **Inspector**: the element tree of this tab's document (the panel's
    /// third pane, `devtools-view` index 2).
    Inspector,
}

/// The DevTools panel's height in logical pixels: what a panel nobody has
/// dragged comes with, the thinnest a reader can drag it to, and the most of
/// the page area an untouched panel and a dragged one may take. The two
/// fractions differ on purpose: 65 % is a sane default on a tall page, while a
/// reader who drags to the top of the range is asking for the panel, so only a
/// tenth of the page has to stay legible.
pub const DEVTOOLS_DEFAULT_HEIGHT: f32 = 240.;
pub const DEVTOOLS_MIN_HEIGHT: f32 = 80.;
const DEVTOOLS_DEFAULT_FRACTION: f32 = 0.65;
const DEVTOOLS_MAX_FRACTION: f32 = 0.9;

/// The height of a panel nobody has dragged, for a page area `page_height`
/// tall: `DEVTOOLS_DEFAULT_HEIGHT`, or the share of the area it fits in —
/// which is what the panel measured before it could be dragged at all.
pub fn default_devtools_height(page_height: f32) -> f32 {
    if page_height <= 0.0 {
        return DEVTOOLS_DEFAULT_HEIGHT; // not laid out yet: keep the number
    }
    DEVTOOLS_DEFAULT_HEIGHT.min(page_height * DEVTOOLS_DEFAULT_FRACTION)
}

/// A height the reader dragged to, folded into what a page area `page_height`
/// tall can show: never thinner than `DEVTOOLS_MIN_HEIGHT`, never so tall that
/// the page is left under a tenth of itself, and never over the area itself.
pub fn clamp_devtools_height(requested: f32, page_height: f32) -> f32 {
    if page_height <= 0.0 {
        return requested.max(DEVTOOLS_MIN_HEIGHT);
    }
    let floor = DEVTOOLS_MIN_HEIGHT.min(page_height);
    let cap = (page_height * DEVTOOLS_MAX_FRACTION).max(floor);
    requested.clamp(floor, cap)
}

/// The height a tab's panel paints at: the height that tab's own drag left it at
/// (`Some`), or the height a panel nobody pulled comes with, and either way
/// fitted to the page area it docks into by [`clamp_devtools_height`].
pub fn desired_devtools_height(stored: Option<f32>, page_height: f32) -> f32 {
    let want = stored.unwrap_or_else(|| default_devtools_height(page_height));
    clamp_devtools_height(want, page_height)
}

/// Which of the three moments of a drag on the panel's top edge a
/// `devtools-edge` callback reports. The Slint side sends these as plain
/// integers, like every other index across the boundary.
pub const DEVTOOLS_EDGE_PRESS: i32 = 0;
pub const DEVTOOLS_EDGE_MOVE: i32 = 1;
pub const DEVTOOLS_EDGE_RELEASE: i32 = 2;

impl DevToolsView {
    pub fn index(self) -> i32 {
        match self {
            DevToolsView::Console => 0,
            DevToolsView::Network => 1,
            DevToolsView::Inspector => 2,
        }
    }

    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => DevToolsView::Network,
            2 => DevToolsView::Inspector,
            _ => DevToolsView::Console,
        }
    }
}

/// What the Inspector pane says about a tab that was never asked for a document
/// (the pane is per tab, and a tree only exists after `ToTab::SetInspect`).
const NO_TREE_HINT: &str =
    "no document tree yet \u{2014} this tab sends one when the pane opens";

/// A read of the Inspector pane; see [`Core::inspector_probe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorProbe {
    /// The panel is open on this tab and its Inspector pane is showing.
    pub pane_open: bool,
    /// This tab has been asked for its document and has not answered yet.
    pub stream_wanted: bool,
    /// The tab the pane's tree was captured from.
    pub target: Option<TabId>,
    /// The tree belongs to the tab on screen (else the window shows nothing).
    pub tree_for_active: bool,
    /// Whose document, and how far through it.
    pub generation: Option<u64>,
    /// Rows the tab sent (folded away or not), of which real elements.
    pub nodes: usize,
    pub elements: usize,
    /// Subtrees the shell has folded up over them.
    pub collapsed: usize,
    /// The selected row, if any.
    pub selected: Option<u64>,
    /// ⌖ is armed for this tab.
    pub pick: bool,
}

/// A tab process that is gone: log it loudly (with whatever stderr — hence
/// panic message + backtrace — the shell captured) and put the crash on the
/// tab's status line. `detail` says how the death was noticed.
fn note_tab_crash(p: &mut Proc, detail: std::fmt::Arguments<'_>) {
    let id = p.tab.id;
    p.crashed = true;
    p.state.loading = false;
    p.state.error = Some(format!("Tab {} has crashed \u{2014} {detail}", id.0));
    error!("tab {id} has crashed: {detail}");
    let tail = p.tab.stderr_tail();
    if tail.trim().is_empty() {
        error!("tab {id} left no stderr behind (it inherited the terminal's, or crashed before logging)");
    } else {
        error!("tab {id} stderr (backtrace if it panicked):\n{tail}");
    }
}

/// Shared runtime core (one per window; kept in an `Rc`).
pub struct Core {
    store: RefCell<ironsurf_storage::Store>,
    ui_path: PathBuf,
    settings: RefCell<UiSettings>,
    procs: RefCell<Vec<Proc>>,
    active: Cell<usize>,
    next_tab_id: Cell<u32>,
    sink: Sender<PumpedMessage>,
    rx: RefCell<Receiver<PumpedMessage>>,
    shell: RefCell<Option<Weak<AppWindow>>>,
    tab_bin: PathBuf,
    extra_tab_args: Vec<String>,
    spawn_error: RefCell<Option<String>>,
    spinner: Cell<usize>,
    /// Loading-bar state (status bar, left-hand end): the sweep step (0…12
    /// along the track) and whether the **previous** spin-tick saw the active
    /// tab loading — the transition between the two is what re-arms the sweep
    /// from the left edge (see [`Core::tick_spinner`]).
    load_step: Cell<usize>,
    prev_loading: Cell<bool>,
    last_page: Cell<(f32, f32)>,
    /// Pointer-move rate limiting (see [`move_due`]): when the last move was
    /// forwarded, and the newest move that was dropped because it was sent too
    /// soon after it (flushed by the drain tick so the final position is lost
    /// for at most one tick).
    last_move_at: Cell<Option<Instant>>,
    pending_move: Cell<Option<(f32, f32)>>,
    /// The session clipboard (task: context menu). Tabs cannot reach a
    /// system clipboard from inside the sandbox, so every copy — menu
    /// entries, page Ctrl+C/X — funnels here, and every paste source (menu
    /// Paste, Ctrl+V) is pushed back out through `ToTab::Clipboard`.
    /// With a system clipboard reachable ([`crate::clipboard::OsClipboard`])
    /// every copy is mirrored out and every paste source prefers the system
    /// text (task: text-selection copy & paste).
    clipboard: RefCell<String>,
    /// Bridge to the desktop clipboard; `None`-inside = no system clipboard,
    /// and the session clipboard alone is the whole story (as before).
    os_clip: RefCell<crate::clipboard::OsClipboard>,
    /// The actions behind the currently-open context menu, index-aligned
    /// with the Slint `menu-items` model.
    menu_items: RefCell<Vec<MenuAction>>,
    /// The JS dialogs awaiting an answer (task: window.alert/confirm/prompt),
    /// FIFO. Every requesting tab is **blocked** on its pipe, so several
    /// tabs can queue at once; the overlay shows the front one (its tab
    /// activated), and the `dialog_shown` marker says whether that front is
    /// already on screen. Ids tie each answer to its request
    /// (`ToTab::DialogResponse`).
    pending_dialogs: RefCell<VecDeque<PendingDialog>>,
    /// `(tab, dialog id)` of the dialog currently on screen (`None` = the
    /// overlay is closed, and the front of the queue is waiting for it).
    dialog_shown: RefCell<Option<(TabId, u32)>>,
    /// The DevTools console table. Shared with the window (`console-rows`), so
    /// it is patched in place instead of rebuilt per drain tick.
    dt_console: Rc<VecModel<ConsoleRow>>,
    /// The DevTools network table (same reason).
    dt_net: Rc<VecModel<NetRow>>,
    /// The Inspector tree and the breadcrumb as the window sees them. Rebuilt
    /// only when the view-model changes (see `dt_inspect_stamp`): folding a node
    /// rewrites the rows below it, and neither folding nor the picker may cost a
    /// document walk — that is the tab's job, and only when asked.
    dt_inspect: Rc<VecModel<InspectRow>>,
    dt_crumb: Rc<VecModel<CrumbItem>>,
    dt_inspect_stamp: RefCell<Option<(usize, Option<u64>, usize, Option<u64>, usize)>>,
    /// The panel's height as it stood the moment its top edge was grabbed, which
    /// is what every move of that one drag is measured off (see
    /// `Core::devtools_edge`); `None` between drags.
    dt_edge_start: Cell<Option<f32>>,
    /// Script id of each userscript-menu row, in `us-scripts` model order:
    /// the overlay mutates by row index, the Store (and the wire) speak ids.
    /// Refreshed from the Store whenever the menu list is re-pushed.
    us_ids: RefCell<Vec<u64>>,
    /// The script the inline editor is **editing** (row clicked): Some(id)
    /// upserts in place on Save (same id, same list position, `enabled`
    /// carried through); `None` is a fresh "+ New script" draft that takes
    /// the next id. Cleared on Save, Cancel, and every menu (re)open.
    us_edit_id: RefCell<Option<u64>>,
    /// Archive-mode root folder in force (the 💾 menu's folder LineEdit →
    /// Apply re-points it): the effective value of the persisted setting,
    /// defaulting to [`archive::default_archive_root`].
    archive_dir: RefCell<PathBuf>,
    /// Files written by archive mode this shell session (the one-line
    /// overlay status; the counter is not persisted — the files are).
    archive_saved: Cell<usize>,
    /// Downloads folder in force (the ⬇ menu's folder LineEdit → Apply
    /// re-points it): the effective value of the persisted setting,
    /// defaulting to [`downloads::download_root`] with the env falling
    /// through (task: file downloads).
    download_dir: RefCell<PathBuf>,
    /// Counter behind every row's [`DownloadRow::stamp`]: the panel sorts by
    /// it so rows from several tabs stay in one honest arrival order.
    download_stamp: Cell<u64>,
    /// The **archive browsing server** (`ironsurf://archive`): started on the
    /// first request, serving the archive folder over `http://127.0.0.1:<random>`
    /// from a pair of background threads. The handle talks to the control
    /// socket; [`Self::archive_server_rx`] carries what the server reports back
    /// (its address, applied folder/host/port, failures) into this pump.
    archive_server: RefCell<Option<ServeHandle>>,
    archive_server_rx: RefCell<Option<Receiver<ServeEvent>>>,
    /// The server's end of the event queue, handed out once and kept (a fresh
    /// `Sender` per start would fork the queue and lose the first reports).
    archive_server_sink: RefCell<Option<Sender<ServeEvent>>>,
    /// Address last announced to the tabs, so a drain tick that changes
    /// nothing does not push a message nobody asked for.
    archive_server_broadcast: RefCell<Option<Option<String>>>,
    /// The tab whose address the user is drafting in the URL bar (task: a new
    /// tab opens with the bar focused and its whole address highlighted, like
    /// F6). While it names the **active** tab, `sync_ui` stops pushing that
    /// tab's URL, so a page that is still loading — or a redirect it follows —
    /// can never overwrite what is being typed. Armed when a tab opens and by
    /// Ctrl-L / F6; dropped by Submit (the draft became a navigation), by
    /// Escape, by a click in the page (which hands keyboard focus back to it),
    /// by a switch to another tab, and by that tab going away.
    url_draft: Cell<Option<TabId>>,
    /// The **reopen closed tab** stack (task: browsing history): closed tabs,
    /// oldest first, capped at [`CLOSED_TABS_MAX`]; Ctrl-Alt-T pops the newest
    /// and reopens it in the container it lived in. Deliberately in-memory —
    /// closed tabs belong to the session, while visits themselves persist in
    /// the history log shared by every container.
    closed_tabs: RefCell<Vec<ClosedTabRecord>>,
}

/// Everything Ctrl-Alt-T needs to recreate a closed tab (task: browsing
/// history): where it pointed and which container held it.
struct ClosedTabRecord {
    url: String,
    container: ContainerId,
}

/// How many closed tabs Ctrl-Alt-T can walk back through.
const CLOSED_TABS_MAX: usize = 50;

/// How many visits the `ironsurf://history` page lists (newest first) — the
/// shell-side half of [`ironsurf_common::MAX_HISTORY_PAGE_ROWS`], which says
/// how many it will hand over. Clearing is all-or-nothing on the whole shared
/// log regardless of this display window.
const HISTORY_PAGE_ROWS: usize = MAX_HISTORY_PAGE_ROWS;

/// Rows the URL bar dropdown shows at most (task: URL bar autocomplete) —
/// Chrome's own count; the rest of the visit log stays reachable through the
/// History menu.
const SUGGEST_ROWS: usize = 8;

/// The assembled application: core + the timers that drive it (owned so
/// they are never dropped while the window runs).
pub struct App {
    core: Rc<Core>,
    _timers: Vec<Timer>,
}

impl App {
    /// Open the container store, create the core, attach every Slint
    /// callback and start the pump/spinner timers.
    pub fn new(
        ui: &AppWindow,
        tab_bin: PathBuf,
        extra_tab_args: Vec<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let store = ironsurf_storage::Store::open(default_data_root())?;
        let ui_path = store.root().join("ui.json");
        let settings = UiSettings::load(&ui_path);
        // Effective archive root before `settings` moves into the core:
        // the persisted folder, or the built-in ~/Downloads/ironsurf-tabs.
        let archive_root = settings
            .archive_dir
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(archive::default_archive_root);
        // …and the downloads folder (task: file downloads): the persisted
        // folder, else `$IRONSURF_DOWNLOAD_DIR`, else `$XDG_DOWNLOAD_DIR`, else
        // `~/Downloads`.
        let download_root =
            downloads::download_root(settings.download_dir.as_deref());
        let download_root =
            downloads::download_root(settings.download_dir.as_deref());
        let (sink, rx) = channel();
        let core = Rc::new(Core {
            store: RefCell::new(store),
            ui_path,
            settings: RefCell::new(settings),
            procs: RefCell::new(Vec::new()),
            active: Cell::new(0),
            next_tab_id: Cell::new(1),
            sink,
            rx: RefCell::new(rx),
            shell: RefCell::new(Some(ui.as_weak())),
            tab_bin,
            extra_tab_args,
            spawn_error: RefCell::new(None),
            spinner: Cell::new(0),
            load_step: Cell::new(0),
            prev_loading: Cell::new(false),
            last_page: Cell::new((0.0, 0.0)),
            last_move_at: Cell::new(None),
            pending_move: Cell::new(None),
            clipboard: RefCell::new(String::new()),
            os_clip: RefCell::new(crate::clipboard::OsClipboard::new()),
            menu_items: RefCell::new(Vec::new()),
            pending_dialogs: RefCell::new(VecDeque::new()),
            dialog_shown: RefCell::new(None),
            dt_console: Rc::new(VecModel::default()),
            dt_net: Rc::new(VecModel::default()),
            dt_inspect: Rc::new(VecModel::default()),
            dt_crumb: Rc::new(VecModel::default()),
            dt_inspect_stamp: RefCell::new(None),
            dt_edge_start: Cell::new(None),
            us_ids: RefCell::new(Vec::new()),
            us_edit_id: RefCell::new(None),
            archive_dir: RefCell::new(archive_root),
            archive_saved: Cell::new(0),
            download_dir: RefCell::new(download_root),
            download_stamp: Cell::new(0),
            archive_server: RefCell::new(None),
            archive_server_rx: RefCell::new(None),
            archive_server_sink: RefCell::new(None),
            archive_server_broadcast: RefCell::new(None),
            url_draft: Cell::new(None),
            closed_tabs: RefCell::new(Vec::new()),
        });

        core.attach_callbacks(ui);

        // Message pump: drain every tab's FromTab stream at ~30 Hz.
        let drain_timer = Timer::default();
        let c = core.clone();
        drain_timer.start(TimerMode::Repeated, std::time::Duration::from_millis(33), move || {
            c.drain();
        });

        // Status-bar spinner (only repaints while something is loading).
        let spinner_timer = Timer::default();
        let c = core.clone();
        spinner_timer.start(TimerMode::Repeated, std::time::Duration::from_millis(120), move || {
            c.tick_spinner();
        });

        Ok(App { core, _timers: vec![drain_timer, spinner_timer] })
    }

    /// Open the initial tab (`Some(url)` overrides the default start page).
    pub fn start(&self, url: Option<&str>) {
        self.core.open_first_tab(url);
    }

    /// Persist settings and shut every tab process down (idempotent).
    pub fn shutdown(&self) {
        let procs = std::mem::take(&mut *self.core.procs.borrow_mut());
        info!("shutting down: saving settings, closing {} tab(s)", procs.len());
        // The archive browsing server is a pair of threads owned by the shell:
        // without this the control socket would outlive the process's intent.
        if let Some(handle) = self.core.archive_server.borrow_mut().take() {
            handle.shutdown();
        }
        self.core.save_settings();
        for mut p in procs {
            p.tab.destroy();
        }
    }

    /// Access to the runtime core (used by the headless integration tests).
    pub fn core(&self) -> Rc<Core> {
        self.core.clone()
    }
}

impl Core {
    // ---- accessors -----------------------------------------------------------

    /// Snapshot of the per-tab states (integration tests / debugging).
    pub fn tab_snapshot(&self) -> Vec<TabState> {
        self.procs.borrow().iter().map(|p| p.state.clone()).collect()
    }

    /// The JS dialogs awaiting the user (task: window.alert/confirm/prompt):
    /// `(tab, dialog id)` in FIFO order (integration tests peek here).
    pub fn pending_dialogs(&self) -> Vec<(TabId, u32)> {
        self.pending_dialogs
            .borrow()
            .iter()
            .map(|p| (p.tab, p.dialog_id))
            .collect()
    }

    /// The DevTools panel as the tabs have it: `(open, pane)` per tab, in strip
    /// order. The flag is per tab, so this is what survives switching tabs.
    pub fn devtools_state(&self) -> Vec<(bool, DevToolsView)> {
        self.procs
            .borrow()
            .iter()
            .map(|p| (p.devtools_open, p.devtools_view))
            .collect()
    }

    /// The height each tab's own drag left its DevTools panel at, in strip
    /// order: `None` while nobody has pulled that tab's top edge. What the
    /// window paints is the active one of these fitted to the page area (see
    /// `desired_devtools_height`), so this is what survives switching tabs.
    pub fn devtools_heights(&self) -> Vec<Option<f32>> {
        self.procs
            .borrow()
            .iter()
            .map(|p| p.devtools_height)
            .collect()
    }

    /// What the session clipboard holds (task: copy in the DevTools tabs): the
    /// copy the last gesture filed — a page selection, a menu `Copy`, a
    /// DevTools row — without pulling the desktop clipboard in. A test asserts
    /// a copy landed by reading this; pasting still prefers the desktop
    /// clipboard when it has text (see `paste_text`).
    pub fn session_clipboard(&self) -> String {
        self.clipboard.borrow().clone()
    }

    fn ui(&self) -> Option<AppWindow> {
        self.shell.borrow().as_ref().and_then(|w| w.upgrade())
    }

    fn active_proc_index(&self) -> Option<usize> {
        let procs = self.procs.borrow();
        if procs.is_empty() {
            None
        } else {
            Some(self.active.get().min(procs.len() - 1))
        }
    }

    fn active_tab_id(&self) -> Option<TabId> {
        let i = self.active_proc_index()?;
        Some(self.procs.borrow()[i].tab.id)
    }

    fn send_active(&self, msg: ToTab) {
        let Some(id) = self.active_tab_id() else {
            debug!("dropped {msg:?}: no active tab");
            return;
        };
        let mut crashed = false;
        {
            let mut procs = self.procs.borrow_mut();
            if let Some(p) = procs.iter_mut().find(|p| p.tab.id == id) {
                if p.crashed {
                    // Already reported dead: the status line says so; do not
                    // re-log the same crash for every keystroke.
                    debug!("dropped {msg:?}: tab {id} has crashed");
                    return;
                }
                if let Err(err) = p.tab.send(msg) {
                    // `msg` is consumed by send(); the command itself is
                    // already on the record via `tabs::TabProc::send`'s trace
                    // line. A write error means the child is gone (broken
                    // pipe): say so on the tab's status line — never silently.
                    let status = p.tab.try_wait_status();
                    match status {
                        Some(status) => note_tab_crash(
                            p,
                            format_args!("the process exited ({status}) after a command failed to reach it: {err}"),
                        ),
                        None => note_tab_crash(
                            p,
                            format_args!("its stdin is dead ({err}) — the process is no longer reachable"),
                        ),
                    }
                    // …and a half-written download goes back out of the folder
                    // (task: file downloads).
                    for path in p.downloads.reap(p.tab.id) {
                        downloads::discard(&path);
                    }
                    crashed = true;
                }
            }
        }
        if crashed {
            self.sync_ui();
        }
    }

    /// Container for new tabs: the remembered one if it still exists, else
    /// the registry default ("remember last chosen" per ui.json).
    fn new_tab_container(&self) -> ContainerId {
        let store = self.store.borrow();
        let remembered = self.settings.borrow().new_tab_container.map(ContainerId);
        match remembered.filter(|id| store.container(*id).is_some()) {
            Some(id) => id,
            None => store.default_container_id(),
        }
    }

    fn page_size(&self) -> (f32, f32) {
        match self.ui() {
            Some(ui) => (ui.get_page_width(), ui.get_page_height()),
            None => self.last_page.get(),
        }
    }

    // ---- tabs ------------------------------------------------------------------

    fn open_first_tab(&self, url: Option<&str>) {
        let container = self.new_tab_container();
        let url = url.map(str::to_string).unwrap_or_else(default_url);
        self.open_tab(container, Some(&url));
        if let Some(ui) = self.ui() {
            // Bumping past 0 arms the focus guards in the .slint file.
            ui.set_focus_page_token(ui.get_focus_page_token() + 1);
        }
        self.sync_ui();
    }

    fn open_tab(&self, container: ContainerId, url: Option<&str>) {
        // Per-spawn argument list: the shared `extra_tab_args` plus this
        // container's UA override (task: per-container UA), so the very first
        // page load of a fresh tab already sends the stored string. Built
        // per spawn — `self.extra_tab_args` is never mutated.
        let (profile_dir, tab_bin, extra) = {
            let store = self.store.borrow();
            let mut extra = self.extra_tab_args.clone();
            if let Some(ua) = container_user_agent(&store, container) {
                extra.push("--user-agent".to_string());
                extra.push(ua);
            }
            // …and the container's proxy, resolved from the global pool
            // (task: proxies) so the very first page load of a fresh tab
            // already rides it; unresolved (none/deleted/junk) = no flag.
            if let Some(url) = proxy_url_for(
                &self.settings.borrow(),
                container_proxy(&store, container).as_deref(),
            ) {
                extra.push("--proxy".to_string());
                extra.push(url);
            }
            // …and the container's **private browsing** flag (task: private
            // browsing), so the very first page load of a fresh tab already
            // sends DNT: 1.
            if container_private(&store, container) {
                extra.push("--private".to_string());
            }
            // A fresh tab starts archiving from its very first fetch when
            // the 💾 mode is on (task: archive mode).
            if self.settings.borrow().archive_enabled {
                extra.push("--archive-mode".to_string());
            }
            (store.profile_dir(container), self.tab_bin.clone(), extra)
        };
        let id = {
            let id = TabId(self.next_tab_id.get());
            self.next_tab_id.set(self.next_tab_id.get() + 1);
            id
        };
        let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
        debug!("open tab {id} in container {}: url {:?}", container.0, url);
        let spawned = match TabProc::spawn(&tab_bin, id, container, &profile_dir, url, &extra_refs, &self.sink) {
            Ok(tab) => {
                *self.spawn_error.borrow_mut() = None;
                let (w, h) = self.page_size();
                {
                    let mut procs = self.procs.borrow_mut();
                    procs.push(Proc {
                        visit_dir: archive::visit_dir_name(),
                        tab,
                        state: TabState::default(),
                        zoom: 1.0,
                        scroll_moved: None,
                        crashed: false,
                        devtools_open: false,
                        inspect_stream: false,
                        inspector: None,
                        inspector_target: None,
                        devtools_view: DevToolsView::default(),
                        devtools_height: None,
                        dt_sel_console: None,
                        dt_sel_network: None,
                        find_open: false,
                        downloads: DownloadStore::default(),
                        save_target: None,
                    });
                    self.active.set(procs.len() - 1);
                }
                if w > 0.0 && h > 0.0 {
                    self.send_active(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
                }
                // A tab process starts with an empty script set: seed it with
                // the enabled userscripts at once (task: userscript menu).
                self.seed_userscripts(id);
                // …and with the settings menu's scroll speed (task: settings
                // menu), so a fresh wheel turns at the chosen speed too.
                if let Some(speed) = self.settings.borrow().scroll_speed {
                    let _ = self.send_to(id, ToTab::SetScrollSpeed { speed });
                }
                true
            }
            Err(err) => {
                error!("spawn {} failed: {err}", tab_bin.display());
                *self.spawn_error.borrow_mut() =
                    Some(format!("spawn {} failed: {err}", tab_bin.display()));
                false
            }
        };
        self.set_visible_except();
        self.sync_ui();
        // A new tab activates: an open UA menu re-points to its container.
        self.ua_refresh_if_open();
        self.proxy_refresh_if_open();
        // …and a new tab opens *in the address bar*: it takes focus with the
        // whole address highlighted exactly like F6 would, so the first
        // keystroke replaces it (task: new tabs focus the URL bar). The bar is
        // filled from the spawn URL first (the tab has not reported one yet)
        // and the token bumps after `sync_ui`, so nothing writes over the
        // selection; the edit session then keeps the loading tab's own URL
        // from clobbering what the user types.
        if spawned {
            if let (Some(ui), Some(url)) = (self.ui(), url) {
                ui.set_url_text(url.into());
            }
            self.focus_url_bar();
        }
    }

    /// Move keyboard focus to the URL bar with its whole address highlighted
    /// (the F6 / Ctrl-L behaviour, reused for every newly opened tab): bump
    /// the token the `UrlBar` watches and arm the edit session for whichever
    /// tab is active now.
    fn focus_url_bar(&self) {
        if let Some(ui) = self.ui() {
            let t = ui.get_focus_url_token() + 1;
            ui.set_focus_url_token(t);
            // A fresh focus of the bar starts with no dropdown: suggestions
            // appear as soon as the user types (task: URL bar autocomplete).
            self.suggest_close(&ui);
        }
        self.url_draft.set(self.active_tab_id());
    }

    fn close_tab(&self, idx: usize) {
        // A tab whose modal is still open must not be left waiting on an
        // answer the overlay will never produce again (task: JS dialogs):
        // close it cancelled first (which also unblocks the tab process).
        if let Some(tid) = self.procs.borrow().get(idx).map(|p| p.tab.id) {
            self.clear_dialog_for(tid, true);
        }
        let mut procs = self.procs.borrow_mut();
        if idx >= procs.len() {
            return;
        }
        info!("closing tab {} ({} still open)", procs[idx].tab.id.0, procs.len() - 1);
        // Push the tab onto the **reopen closed tab** stack (task: browsing
        // history) — only if it had an address: a blank tab that never
        // navigated is not worth bringing back.
        if let Some(url) = procs[idx].state.url.clone().filter(|u| reopenable(u)) {
            let mut closed = self.closed_tabs.borrow_mut();
            closed.push(ClosedTabRecord { url, container: procs[idx].tab.container });
            if closed.len() > CLOSED_TABS_MAX {
                closed.remove(0);
            }
        }
        let mut proc = procs.remove(idx);
        proc.tab.destroy();
        // A tab closed mid-download takes its partial file out of the folder
        // too — the rows vanish with the process, so nothing is left to say
        // what the orphan was (task: file downloads).
        for path in proc.downloads.reap(proc.tab.id) {
            downloads::discard(&path);
        }
        let empty = procs.is_empty();
        if !empty {
            self.active.set(self.active.get().min(procs.len() - 1));
        }
        drop(procs);
        if empty {
            // Last tab closed ⇒ the window (and every helper thread) is done.
            let _ = slint::quit_event_loop();
        } else {
            self.set_visible_except();
        }
        self.sync_ui();
    }

    // ---- JS dialogs (window.alert/confirm/prompt) --------------------------

    /// A tab reported `FromTab::DialogRequest`: that tab is now blocked on
    /// its pipe (task: window.alert/confirm/prompt). The request joins the
    /// FIFO queue; a request from a tab that is gone or crashed is answered
    /// cancelled right away, so a dying tab never waits on a dialog nobody
    /// can show.
    fn open_dialog(&self, tid: TabId, dialog_id: u32, kind: DialogKind, message: &str, default: Option<&str>) {
        let alive = self
            .procs
            .borrow()
            .iter()
            .find(|p| p.tab.id == tid)
            .is_some_and(|p| !p.crashed && !p.state.closed);
        if !alive {
            warn!("tab {tid} asked for a dialog but is gone or crashed — answering cancelled");
            let _ = self.send_to(tid, ToTab::DialogResponse { dialog_id, confirmed: false, text: None });
            return;
        }
        self.pending_dialogs.borrow_mut().push_back(PendingDialog {
            tab: tid,
            dialog_id,
            kind,
            message: message.to_string(),
            default: default.map(str::to_string),
        });
        self.show_next_dialog();
    }

    /// Put the front of the queue on screen — unless a dialog is already
    /// showing. The requesting tab is activated: a background tab's modal
    /// must not sit unseen while its process waits. A queued tab that
    /// vanished since is dropped (its pump answers cancelled at EOF).
    fn show_next_dialog(&self) {
        if self.dialog_shown.borrow().is_some() {
            return; // one modal at a time; the next waits its turn
        }
        let Some(next) = self.pending_dialogs.borrow().front().cloned() else { return };
        let at = self.procs.borrow().iter().position(|p| p.tab.id == next.tab);
        let Some(at) = at else {
            self.pending_dialogs.borrow_mut().retain(|p| p != &next);
            return self.show_next_dialog();
        };
        let title = format!("Tab {} — {}", next.tab.0, self.procs.borrow()[at].state.title);
        let Some(ui) = self.ui() else { return };
        if self.active.get() != at {
            info!("tab {} has a dialog — switching to its tab", next.tab.0);
            self.activate_tab(at);
        }
        *self.dialog_shown.borrow_mut() = Some((next.tab, next.dialog_id));
        ui.set_dialog_kind(match next.kind {
            DialogKind::Alert => 0,
            DialogKind::Confirm => 1,
            DialogKind::Prompt => 2,
        });
        ui.set_dialog_message(next.message.as_str().into());
        ui.set_dialog_title(title.into());
        // The prompt box starts at the page's default; the other kinds
        // clear it so a stale answer can never leak into the next dialog.
        ui.set_dialog_input(next.default.as_deref().unwrap_or_default().into());
        ui.set_dialog_open(true);
    }

    /// The user dismissed the open dialog: OK (accepted) or Cancel/Esc
    /// (cancelled). The answer rides back as `ToTab::DialogResponse`; the
    /// blocked tab's JS call returns from there, and the next queued dialog
    /// (from any tab) takes the overlay.
    fn answer_dialog(&self, confirmed: bool) {
        let Some(shown) = self.dialog_shown.borrow_mut().take() else {
            debug!("dialog answer with no dialog on screen — ignored");
            return;
        };
        let pending = {
            let mut queue = self.pending_dialogs.borrow_mut();
            let at = queue.iter().position(|p| (p.tab, p.dialog_id) == shown);
            match at.map(|i| queue.remove(i).expect("position came from this queue")) {
                Some(p) => p,
                None => {
                    // The request was already cleared (its tab went away);
                    // the tab we answered is the same request — drop it.
                    debug!("dialog answer for an already-cleared request — ignored");
                    if let Some(ui) = self.ui() {
                        ui.set_dialog_open(false);
                    }
                    return;
                }
            }
        };
        let ui = self.ui();
        let text = ui
            .as_ref()
            .filter(|_| confirmed && pending.prompt())
            .map(|ui| ui.get_dialog_input().to_string());
        if let Some(ui) = ui {
            ui.set_dialog_open(false);
        }
        info!(
            "dialog #{} ({:?}) on tab {} answered {}",
            pending.dialog_id,
            pending.kind,
            pending.tab.0,
            if confirmed { "OK" } else { "cancel" }
        );
        self.send_to(pending.tab, ToTab::DialogResponse { dialog_id: pending.dialog_id, confirmed, text });
        self.show_next_dialog();
    }

    /// Drop every request queued or shown for *that* tab — it is going away
    /// (closed, reaped, or crashed) and its blocked calls can no longer be
    /// answered from the overlay. The tab is woken with cancelled answers
    /// while it can still receive them (`notify_tab`); the overlay closes
    /// (or rolls to the next queued dialog).
    fn clear_dialog_for(&self, tid: TabId, notify_tab: bool) {
        let dropped: Vec<PendingDialog> = {
            let queue = self.pending_dialogs.borrow();
            let dropped = queue.iter().filter(|p| p.tab == tid).cloned().collect::<Vec<_>>();
            if dropped.is_empty() {
                return;
            }
            dropped
        };
        self.pending_dialogs.borrow_mut().retain(|p| p.tab != tid);
        let shown_was_ours =
            self.dialog_shown.borrow().as_ref().is_some_and(|(t, _)| *t == tid);
        if shown_was_ours {
            *self.dialog_shown.borrow_mut() = None;
            if let Some(ui) = self.ui() {
                ui.set_dialog_open(false);
            }
        }
        if notify_tab {
            for p in &dropped {
                let _ = self.send_to(
                    tid,
                    ToTab::DialogResponse { dialog_id: p.dialog_id, confirmed: false, text: None },
                );
            }
        }
        if shown_was_ours {
            self.show_next_dialog();
        }
    }

    /// Send one command to a tab by id (the pending dialog's tab is not
    /// necessarily the active one at the moments this is called).
    fn send_to(&self, tid: TabId, msg: ToTab) -> std::io::Result<()> {
        let mut procs = self.procs.borrow_mut();
        match procs.iter_mut().find(|p| p.tab.id == tid) {
            Some(p) => p.tab.send(msg),
            None => Err(std::io::Error::other("tab is gone")),
        }
    }

    fn activate_tab(&self, idx: usize) {
        {
            let procs = self.procs.borrow();
            if idx >= procs.len() {
                debug!("activate ignored: no tab at index {idx}");
                return;
            }
            debug!("activate tab {} (index {idx})", procs[idx].tab.id.0);
        }
        self.active.set(idx);
        // The dropdown belongs to the bar's draft, which the activation just
        // replaced with the new tab's address (task: URL bar autocomplete).
        if let Some(ui) = self.ui() {
            self.suggest_close(&ui);
        }
        self.set_visible_except();
        // The find bar follows the activation like the DevTools panel does: the
        // new tab's own query (and its own open flag) replace the old one's.
        self.show_find_query();
        if let Some(id) = self.active_tab_id() {
            let (w, h) = self.page_size();
            if w > 0.0 && h > 0.0 {
                let procs = self.procs.borrow();
                if let Some(p) = procs.iter().find(|p| p.tab.id == id) {
                    let _ = p.tab.send(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
                }
            }
        }
        self.sync_ui();
        // The UA menu (if open) follows the activation: the new tab's
        // container's value, rows, and error line replace the previous one's.
        self.ua_refresh_if_open();
        self.proxy_refresh_if_open();
    }

    fn cycle_tab(&self) {
        let len = self.procs.borrow().len();
        if len > 1 {
            let next = (self.active.get() as usize + 1) % len;
            self.activate_tab(next);
        }
    }

    /// `Alt-1` … `Alt-9` (`⌘1` … `⌘9` on macOS): activate the N-th tab, resolved
    /// against the tab list **as it is right now** (see
    /// [`keymap::tab_index_for_digit`]): digit 9 is the last tab from nine tabs
    /// on, so the mapping slides with every tab that opens or closes. A digit
    /// with no tab behind it does nothing.
    fn select_tab_by_digit(&self, digit: u8) {
        let mod_label = keymap::Chords::CURRENT.digit_mod_label();
        let count = self.procs.borrow().len();
        let Some(idx) = keymap::tab_index_for_digit(digit, count) else {
            debug!("{mod_label}-{digit} dropped: no tab for that key with {count} tab(s)");
            return;
        };
        if Some(idx) == self.active_proc_index() {
            debug!("{mod_label}-{digit}: tab {idx} is already active");
            return;
        }
        self.activate_tab(idx);
    }

    /// Only the active tab renders; background tabs throttle themselves.
    fn set_visible_except(&self) {
        let active = self.active_tab_id();
        let procs = self.procs.borrow();
        for p in procs.iter() {
            let _ = p.tab.send(ToTab::SetVisible { visible: Some(p.tab.id) == active });
        }
    }

    fn navigate(&self, text: &str) {
        // The draft became an address — the dropdown has served its purpose
        // (task: URL bar autocomplete).
        if let Some(ui) = self.ui() {
            self.suggest_close(&ui);
        }
        let Some(url) = classify_input(text) else {
            debug!("address bar: {text:?} is not a URL/search term, ignored");
            return;
        };
        info!("address bar: {text:?} → navigating to {url}");
        self.send_active(ToTab::Navigate { url: url.clone() });
        // The draft became a navigation: from here the tab's own URL drives
        // the bar again (see `sync_ui`).
        self.url_draft.set(None);
        if let Some(ui) = self.ui() {
            ui.set_url_text(url.into());
            // After navigating from the URL bar, focus returns to the page.
            ui.set_focus_page_token(ui.get_focus_page_token() + 1);
        }
    }

    /// Resize the active tab when the page area changed size.
    fn check_resize(&self) {
        let size = self.page_size();
        if size == self.last_page.get() || size.0 <= 0.0 || size.1 <= 0.0 {
            return;
        }
        self.last_page.set(size);
        self.send_active(ToTab::Resize { width: size.0 as u32, height: size.1 as u32, scale: 1.0 });
    }

    // ---- input routing ---------------------------------------------------------

    fn page_key(&self, pressed: bool, text: &str, ctrl: bool, shift: bool, alt: bool, meta: bool, repeat: bool) {
        if !pressed {
            return; // releases are not forwarded: the tab protocol models presses only
        }
        match keymap::shortcut_for(text, ctrl, alt, meta) {
            Some(sc) => debug!("shortcut {sc:?} (key {text:?} ctrl={ctrl} alt={alt} meta={meta})"),
            None => trace!("key {text:?} ctrl={ctrl} alt={alt} meta={meta} → page"),
        }
        if let Some(sc) = keymap::shortcut_for(text, ctrl, alt, meta) {
            self.dispatch_page_shortcut(sc);
            return;
        }
        self.forward_page_key(text, ctrl, shift, alt, meta, repeat);
    }

    /// A plain (non-chord) key goes to the page — clipboard pre-push for the
    /// paste chord included.
    fn forward_page_key(&self, text: &str, ctrl: bool, shift: bool, alt: bool, meta: bool, repeat: bool) {
        // Ctrl+V: the sandboxed tab cannot read the session clipboard, so
        // push it in before the keystroke is relayed (blitz's editors pull
        // the paste from `ShellProvider::get_clipboard_text`, which answers
        // from this cache).
        if is_paste_chord(text, ctrl, alt, meta) {
            // The system clipboard wins when it has text (the user copied
            // from another app a moment ago); else the session copy stands.
            let clip = self.paste_text();
            debug!("clipboard: pushed {} char(s) ahead of Ctrl+V", clip.chars().count());
            self.send_active(ToTab::Clipboard { text: clip });
        }
        let ev = keymap::to_ipc_key(text, ctrl, shift, alt, meta, repeat);
        self.send_active(ToTab::Key(ev));
    }

    /// The browser-chord table, shared by page focus and by the text bars'
    /// [`bar_chord`] (task: keep keyboard shortcuts usable while the URL bar
    /// is focused).
    fn dispatch_page_shortcut(&self, sc: Shortcut) {
        match Some(sc) {
            Some(Shortcut::FocusUrl) => {
                self.focus_url_bar();
                return;
            }
            Some(Shortcut::CloseTab) => {
                let idx = self.active_proc_index().unwrap_or(0);
                self.close_tab(idx);
                return;
            }
            Some(Shortcut::Reload) => {
                self.send_active(ToTab::Reload);
                return;
            }
            Some(Shortcut::NewTab) => {
                let cid = self.new_tab_container();
                self.open_tab(cid, Some(&default_url()));
                return;
            }
            Some(Shortcut::CycleTab) => {
                self.cycle_tab();
                return;
            }
            Some(Shortcut::SelectTab(digit)) => {
                self.select_tab_by_digit(digit);
                return;
            }
            // History: the same messages the ‹ › toolbar buttons send, so the tab
            // answers a chord exactly as it answers a click, and a tab with
            // nothing left in its history simply has nothing to do.
            Some(Shortcut::GoBack) => {
                self.send_active(ToTab::Back);
                return;
            }
            Some(Shortcut::GoForward) => {
                self.send_active(ToTab::Forward);
                return;
            }
            Some(Shortcut::ZoomIn) => {
                self.step_zoom(keymap::zoom_in);
                return;
            }
            Some(Shortcut::ZoomOut) => {
                self.step_zoom(keymap::zoom_out);
                return;
            }
            Some(Shortcut::ZoomReset) => {
                self.apply_zoom(1.0);
                return;
            }
            Some(Shortcut::Find) => {
                self.open_find();
                return;
            }
            Some(Shortcut::ToggleDevTools) => {
                // Over the page the chord arrives from the page's
                // `FocusScope`; while a text bar holds focus the bar offers it
                // to `bar_chord` first (task: keep keyboard shortcuts usable
                // while the URL bar is focused) — no second `FocusScope`
                // around the `LineEdit`, so ordinary typing stays untouched.
                self.toggle_devtools();
                return;
            }
            Some(Shortcut::Inspector) => {
                // Ctrl-Shift-C: ⌖ *inspect element* for this tab. Page focus only,
                // exactly like F12 — it is a page tool, and it opens the panel and
                // its Inspector pane when they are not up yet.
                self.toggle_inspect_pick();
                return;
            }
            Some(Shortcut::PageFocus) => {
                // Escape closes an open find bar first (that is what the reader
                // is aiming at), then dismisses an open context menu, and
                // abandons an address being typed: focus goes back to the page
                // and the bar shows the tab's real URL again. An armed ⌖ is
                // dropped first, before focus moves — the picker would otherwise
                // keep eating the page's clicks with nothing on screen to show.
                // The "Save page" dialog answers Esc first (task:
                // save-page selector): it just shuts the dialog, like the
                // find bar before it.
                if self.ui().is_some_and(|ui| ui.get_save_menu_open()) {
                    self.save_dialog(false);
                    return;
                }
                if self.find_open_active() {
                    self.close_find();
                    return;
                }
                if let Some(i) = self.active_proc_index() {
                    self.disarm_inspect_pick(i);
                }
                self.close_context_menu();
                self.url_draft.set(None);
                if let Some(ui) = self.ui() {
                    let t = ui.get_focus_page_token() + 1;
                    ui.set_focus_page_token(t);
                }
                self.sync_ui();
                return;
            }
            Some(Shortcut::ReopenClosedTab) => {
                // Ctrl-Alt-T (task: browsing history): pop the most recently
                // closed tab and reopen its address in its old container.
                self.reopen_closed_tab();
                return;
            }
            Some(Shortcut::HistoryMenu) => {
                // Ctrl-Alt-H (task: browsing history): the same menu as the ☰
                // toolbar button, for keyboard-first users.
                self.toggle_history_menu();
                return;
            }
            Some(Shortcut::DownloadsMenu) => {
                // Ctrl-Shift-D (task: file downloads): the ⬇ panel, the same
                // overlay as the toolbar's ⬇ button.
                self.toggle_download_menu();
                return;
            }
            Some(Shortcut::ToggleReader) => {
                // Ctrl-Alt-R (task: reader mode): the 📄 button's chord — the
                // tab decides what to make of it and answers with
                // `ReaderState`, which moves the button (or doesn't).
                self.toggle_reader();
                return;
            }
            Some(Shortcut::PopupAllowSite) => {
                // Ctrl-Alt-P (task: block popups): allow this site to open
                // windows; the tab applies the new policy to the page it is
                // showing, so the reload the user would otherwise reach for is
                // not needed.
                self.allow_popup_site();
                return;
            }
            Some(Shortcut::SavePage) => {
                // Ctrl-S (task: save-page selector): the "Save page" dialog —
                // pick the folder (default `~/Downloads`) and the name, and
                // the current page lands there through the download
                // machinery. Consumed either way: Ctrl-S is the browser's
                // chord, not the page's.
                self.toggle_save_dialog();
                return;
            }
            None => {}
        }
    }

    /// A chord delivered while a text bar (URL or find) holds focus
    /// (task: keep keyboard shortcuts usable while the URL bar is focused):
    /// the tabled browser chords fire exactly as they do over the page —
    /// Ctrl-T still makes a tab, the digits still switch tabs — while plain
    /// typing, caret motion and the entry's own editing chords stay local to
    /// the entry (Esc included, so the completion dropdown keeps its
    /// controls). `true` means the chord was consumed.
    fn bar_chord(&self, text: &str, ctrl: bool, alt: bool, meta: bool) -> bool {
        let Some(sc) = keymap::bar_shortcut(text, ctrl, alt, meta) else {
            return false;
        };
        debug!("shortcut {sc:?} (key {text:?}) with a text bar focused");
        self.dispatch_page_shortcut(sc);
        true
    }

    /// One zoom step (in/out) from the active tab's current factor.
    fn step_zoom(&self, step: impl Fn(f32) -> f32) {
        let zoom = self
            .active_proc_index()
            .map(|i| step(self.procs.borrow()[i].zoom))
            .unwrap_or(1.0);
        self.apply_zoom(zoom);
    }

    /// Push a new zoom factor to the active tab (Ctrl-`+`/`-`/`0` chords);
    /// the tab rescales its document viewport and reports back with fresh
    /// frames. The status bar says "Zoom N%" until the page next speaks.
    fn apply_zoom(&self, zoom: f32) {
        let Some(i) = self.active_proc_index() else {
            debug!("zoom {zoom:.0}% dropped: no active tab");
            return;
        };
        let label = format!("Zoom {:.0}%", zoom * 100.0);
        info!("{label} (active tab)");
        {
            let mut procs = self.procs.borrow_mut();
            let p = &mut procs[i];
            p.zoom = zoom;
            p.state.status = Some(label);
            if let Err(err) = p.tab.send(ToTab::SetZoom { zoom }) {
                warn!("tab {} could not set zoom: {err}", p.tab.id);
            }
        }
        self.sync_ui();
    }

    // ---- DevTools panel --------------------------------------------------------

    /// F12 / Ctrl-`I`: flip the DevTools panel of the active tab. The flag sits
    /// on the tab record (like the zoom factor), so switching tabs switches the
    /// panel to the new tab's feed and a freshly spawned tab starts closed.
    fn toggle_devtools(&self) {
        let Some(i) = self.active_proc_index() else {
            debug!("DevTools toggle dropped: no active tab");
            return;
        };
        let mut procs = self.procs.borrow_mut();
        let p = &mut procs[i];
        p.devtools_open = !p.devtools_open;
        let (id, open) = (p.tab.id, p.devtools_open);
        drop(procs);
        info!("DevTools {} on tab {id}", if open { "opened" } else { "closed" });
        // Opening the panel puts the tree on screen for the first time since any
        // page-side selection, so it is asked to scroll there again.
        if open {
            self.rearm_reveal(i);
        }
        self.sync_ui();
        // The Inspector pane is one of the panes, so F12 is also the moment this
        // tab starts or stops walking its document.
        self.sync_inspect_stream(i);
    }

    /// The panel's “✕” row (and nothing else) closes it for the active tab.
    fn close_devtools(&self) {
        let Some(i) = self.active_proc_index() else { return };
        {
            let mut procs = self.procs.borrow_mut();
            if !procs[i].devtools_open {
                return;
            }
            procs[i].devtools_open = false;
        }
        self.sync_ui();
        self.sync_inspect_stream(i);
    }

    /// A header row picked a pane (Console / Network / Inspector).
    fn set_devtools_view(&self, view: DevToolsView) {
        let Some(i) = self.active_proc_index() else { return };
        {
            let mut procs = self.procs.borrow_mut();
            if procs[i].devtools_view == view {
                return;
            }
            procs[i].devtools_view = view;
        }
        // Same for coming back to the Inspector pane: see `rearm_reveal`.
        if view == DevToolsView::Inspector {
            self.rearm_reveal(i);
        }
        self.sync_ui();
        // Moving to or from the Inspector pane starts and stops this tab's
        // snapshot stream; the other two panes never ask for a document.
        self.sync_inspect_stream(i);
    }

    /// The height tab `i`'s DevTools panel paints: the height its own drag left
    /// it at, or the default for a panel nobody pulled — either way fitted to
    /// the page area it docks into, which a window resize changes under a height
    /// that is stored as a number.
    fn desired_devtools_height(&self, i: usize) -> f32 {
        let stored = self.procs.borrow().get(i).and_then(|p| p.devtools_height);
        desired_devtools_height(stored, self.page_size().1)
    }

    /// Hand the window a panel height when it is not the height it already has —
    /// compared rather than written blind, because that length is what the panel
    /// binds its own geometry to and a write every tick would relayout the window
    /// for nothing.
    fn push_devtools_height(&self, ui: &AppWindow, stored: Option<f32>) {
        let want = desired_devtools_height(stored, self.page_size().1);
        if (ui.get_devtools_height() - want).abs() > 0.01 {
            ui.set_devtools_height(want);
        }
    }

    /// Refit the active tab's panel to the page area it docks into, on a tick
    /// with nothing to report: the one thing a tab that has fallen silent still
    /// has to notice is that the window — and therefore the area above its panel
    /// — changed size under a height remembered as a number.
    fn sync_devtools_height(&self) {
        let (Some(ui), Some(i)) = (self.ui(), self.active_proc_index()) else { return };
        let stored = self.procs.borrow().get(i).and_then(|p| p.devtools_height);
        self.push_devtools_height(&ui, stored);
    }

    /// A drag on the DevTools panel's top edge: `kind` says which moment it is
    /// (`DEVTOOLS_EDGE_PRESS`/`_MOVE`/`_RELEASE`) and `travel` is how far the
    /// pointer has come from the point it was grabbed at, positive **up** — the
    /// direction that makes the panel taller, since the panel is docked to the
    /// bottom of the page area.
    ///
    /// The height is Rust's, per tab: the press notes where the panel stood and
    /// every move is applied to that note, never to the last height. A panel
    /// held at the floor or the ceiling then cannot drift out of place when the
    /// pointer wanders back the other way, and a tab nobody dragged keeps its
    /// default height no matter what the tab beside it was pulled into.
    fn devtools_edge(&self, kind: i32, travel: f32) {
        let Some(i) = self.active_proc_index() else { return };
        match kind {
            DEVTOOLS_EDGE_PRESS => {
                let start = self.desired_devtools_height(i);
                self.dt_edge_start.set(Some(start));
                debug!("DevTools edge grabbed on tab {i} at {start:.0}px");
            }
            DEVTOOLS_EDGE_MOVE => {
                let start =
                    self.dt_edge_start.get().unwrap_or_else(|| self.desired_devtools_height(i));
                let want = clamp_devtools_height(start + travel, self.page_size().1);
                self.procs.borrow_mut()[i].devtools_height = Some(want);
                // The panel follows the pointer move for move — and a drag is
                // what makes a never-dragged panel have a height of its own, so
                // this is the one place the height is written rather than
                // noticed to be different.
                if let Some(ui) = self.ui() {
                    self.push_devtools_height(&ui, Some(want));
                }
            }
            _ => {
                let h = self.procs.borrow().get(i).and_then(|p| p.devtools_height);
                self.dt_edge_start.set(None);
                if let Some(h) = h {
                    info!("DevTools panel of tab {i} resized to {h:.0}px");
                }
            }
        }
    }

    // ---- DevTools **Inspector** pane ---------------------------------------

    /// The tree has just come on screen, and a `ListView` that did not exist
    /// when the page selected a node cannot have scrolled to it: an owed scroll
    /// is offered to the pane again (see [`InspectorUi::rearm_reveal`]).
    fn rearm_reveal(&self, i: usize) {
        self.with_inspector(i, |ui| ui.rearm_reveal());
    }

    /// This tab's Inspector view-model, created on first use and stamped with the
    /// tab it belongs to (a node id means nothing outside its own document, which
    /// is why the stamp is a real field and not an assumption).
    fn with_inspector<T>(&self, i: usize, f: impl FnOnce(&mut InspectorUi) -> T) -> Option<T> {
        let mut procs = self.procs.borrow_mut();
        let p = procs.get_mut(i)?;
        let out = f(p.inspector.get_or_insert_with(InspectorUi::default));
        p.inspector_target = Some(p.tab.id);
        Some(out)
    }

    /// The tab the window's Inspector pane is bound to, and whether ⌖ is armed
    /// for it (`(tab, pick)`; integration tests assert on this).
    pub fn inspector_target(&self) -> Option<(TabId, bool)> {
        let procs = self.procs.borrow();
        let i = self.active_proc_index()?;
        let p = &procs[i];
        p.inspector_target.map(|id| (id, p.inspector.as_ref().is_some_and(|ui| ui.pick())))
    }

    /// Everything the Inspector pane of the active tab is showing, in one
    /// snapshot of state, for tests and diagnostics (`#[derive(PartialEq)]` so a
    /// test can compare a whole situation at once). `tree_for_active` says the
    /// tree the numbers describe belongs to the tab on screen; a tab with the
    /// pane open and no answer yet has `stream_wanted` and no tree.
    pub fn inspector_probe(&self) -> InspectorProbe {
        let procs = self.procs.borrow();
        let Some(p) = self.active_proc_index().and_then(|i| procs.get(i)) else {
            return InspectorProbe::default();
        };
        let tree_for_active = p.inspector_target == Some(p.tab.id);
        let ui = p.inspector.as_ref().filter(|_| tree_for_active);
        InspectorProbe {
            pane_open: p.devtools_open && p.devtools_view == DevToolsView::Inspector,
            stream_wanted: p.inspect_stream,
            target: p.inspector_target,
            tree_for_active,
            generation: ui.and_then(|ui| ui.generation()),
            nodes: ui.map(|ui| ui.node_count()).unwrap_or(0),
            elements: ui.map(|ui| ui.element_count()).unwrap_or(0),
            collapsed: ui.map(|ui| ui.collapsed_len()).unwrap_or(0),
            selected: ui.and_then(|ui| ui.selected()),
            pick: ui.is_some_and(|ui| ui.pick()),
        }
    }

    /// Tell a tab whether it has to walk its document for us. `ToTab::SetInspect`
    /// is per tab, and so are the panel and its pane, so `devtools_open` and
    /// `devtools_view` decide it together. It goes out only on a real change: a
    /// tab answers every `open: true` with a fresh snapshot, and an open pane
    /// must not turn into a re-walk per redraw.
    fn sync_inspect_stream(&self, i: usize) {
        let Some((id, want, streaming)) = self.procs.borrow().get(i).map(|p| {
            (
                p.tab.id,
                p.devtools_open && p.devtools_view == DevToolsView::Inspector,
                p.inspect_stream,
            )
        }) else {
            return;
        };
        if want == streaming {
            return;
        }
        if !want && self.with_inspector(i, |ui| ui.pick()).unwrap_or(false) {
            // Leaving the Inspector pane ends ⌖ as well: a page that swallows
            // pointers for a picker nobody can see is a bug, not a mode.
            self.disarm_inspect_pick(i);
        }
        debug!("DevTools inspector: tab {id} should stream its document: {want}");
        if let Err(err) = self.send_to(id, ToTab::SetInspect { open: want }) {
            warn!("tab {id} would not take the inspector stream switch: {err}");
        }
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.get_mut(i) else { return };
        p.inspect_stream = want;
        if want {
            p.inspector_target = Some(id);
            p.inspector.get_or_insert_with(InspectorUi::default).expect_tree();
        } else if let Some(ui) = p.inspector.as_mut() {
            // The tree belongs to this document, and so do the folds on it: with
            // the pane shut there is nothing to paint, and a background tab that
            // keeps a snapshot it cannot show is only memory.
            ui.clear_tree();
        }
    }

    /// ⌖ *inspect element* (Ctrl-Shift-C, or the pane's ⌖ button): the picking is
    /// the tab's, the arming is the shell's. Arming pulls the panel and its
    /// Inspector pane up first, because the point of the mode is to land on a row
    /// in that pane, and the same chord again disarms it. While armed the page
    /// swallows pointers for the picker, so the ⌖ button stays lit.
    fn toggle_inspect_pick(&self) {
        let Some(i) = self.active_proc_index() else {
            debug!("⌖ inspect-element toggle dropped: no active tab");
            return;
        };
        let armed = self.procs.borrow()[i]
            .inspector
            .as_ref()
            .is_some_and(|ui| ui.pick());
        let id = self.procs.borrow()[i].tab.id;
        if armed {
            self.set_inspect_pick(i, false);
        } else {
            {
                let mut procs = self.procs.borrow_mut();
                if let Some(p) = procs.get_mut(i) {
                    p.devtools_open = true;
                    p.devtools_view = DevToolsView::Inspector;
                }
            }
            self.sync_inspect_stream(i);
            self.set_inspect_pick(i, true);
            info!("⌖ inspect element armed on tab {id}: the next click in the page names a node");
        }
        if let Err(err) = self.send_to(id, ToTab::SetInspectPick { enabled: !armed }) {
            warn!("tab {id} would not take the inspect-element pick: {err}");
        }
        self.sync_ui();
    }

    /// Move the ⌖ flag on the record (the message to the tab is the caller's, so
    /// one exit path can disarm without re-arming another mode behind its back).
    fn set_inspect_pick(&self, i: usize, enabled: bool) {
        self.with_inspector(i, |ui| ui.set_pick(enabled));
    }

    /// Disarm the picker of one tab, reporting whether it was armed. Escape does
    /// this before handing focus back to the page, and closing the panel or
    /// leaving the Inspector pane does it too — a page that eats clicks for a
    /// picker nobody can see is a bug, not a mode.
    fn disarm_inspect_pick(&self, i: usize) -> bool {
        let Some(id) = self.procs.borrow().get(i).map(|p| p.tab.id) else { return false };
        if !self.with_inspector(i, |ui| ui.pick()).unwrap_or(false) {
            return false;
        }
        self.set_inspect_pick(i, false);
        if let Err(err) = self.send_to(id, ToTab::SetInspectPick { enabled: false }) {
            warn!("tab {id} would not disarm the inspect-element pick: {err}");
        }
        info!("⌖ inspect element disarmed on tab {id}");
        true
    }

    /// The context menu's “⌖ Inspect Element”: ask the tab what is under the
    /// cursor (`ToTab::InspectPickAt`) with the Inspector pane open to show the
    /// answer. The tab replies with a snapshot whose `selected` is the hit node,
    /// so no second selection message is expected — and none is invented here.
    fn inspect_element_at(&self, x: f32, y: f32) {
        let Some(i) = self.active_proc_index() else {
            debug!("Inspect Element dropped: no active tab");
            return;
        };
        let id = self.procs.borrow()[i].tab.id;
        {
            let mut procs = self.procs.borrow_mut();
            if let Some(p) = procs.get_mut(i) {
                p.devtools_open = true;
                p.devtools_view = DevToolsView::Inspector;
            }
        }
        self.sync_inspect_stream(i);
        self.with_inspector(i, |ui| ui.expect_tree());
        info!("Inspect Element: asking tab {id} what is under ({x:.0},{y:.0})");
        if let Err(err) = self.send_to(id, ToTab::InspectPickAt { x, y }) {
            warn!("tab {id} would not answer Inspect Element: {err}");
        }
        self.sync_ui();
    }

    /// A row of the tree was clicked. Selection is the shell's own state; the tab
    /// only paints the box (`ToTab::InspectSelect`). An id the current tree does
    /// not know means the snapshot is stale: wait for the next one instead of
    /// guessing at a node the tab may not have any more.
    fn inspector_select(&self, node: i32) {
        let Some(i) = self.active_proc_index() else { return };
        let id = self.procs.borrow()[i].tab.id;
        let known = self
            .with_inspector(i, |ui| {
                // Membership first: an id from a snapshot the tab has replaced
                // must not become the selection, or the pane highlights a node
                // that does not exist and tells the tab to paint one too.
                let known = ui
                    .snapshot()
                    .is_some_and(|s| s.nodes.iter().any(|n| n.id == node as u64));
                if known {
                    ui.select(node as u64);
                }
                known
            })
            .unwrap_or(false);
        if !known {
            debug!("inspector row {node} is not in tab {id}'s tree — waiting for a fresh snapshot");
            self.sync_ui();
            return;
        }
        if let Err(err) = self.send_to(id, ToTab::InspectSelect { node: Some(node as u64) }) {
            warn!("tab {id} would not take the inspector selection: {err}");
        }
        self.sync_ui();
    }

    /// A `+`/`−` twisty: folding is the shell's own business. The tab is not
    /// asked to re-send its document, and must not be.
    fn inspector_toggle_fold(&self, node: i32) {
        let Some(i) = self.active_proc_index() else { return };
        let id = self.procs.borrow()[i].tab.id;
        let collapsed = self
            .with_inspector(i, |ui| ui.toggle_fold(node as u64))
            .unwrap_or(false);
        debug!("inspector: tab {id} node {node} {}", if collapsed { "folded" } else { "unfolded" });
        self.sync_ui();
    }

    /// A tree landed. It belongs to one tab and to one generation of that tab's
    /// document, so the record of *that* tab takes it and the “asking the tab…”
    /// state ends; the window refreshes when that tab is the one on screen.
    fn on_inspect_snapshot(&self, tid: TabId, snapshot: ironsurf_common::InspectSnapshot) {
        let i = {
            let procs = self.procs.borrow();
            match procs.iter().position(|p| p.tab.id == tid) {
                Some(i) => i,
                None => {
                    debug!("inspector snapshot for tab {tid} dropped: no such tab");
                    return;
                }
            }
        };
        let (nodes, generation) = (snapshot.nodes.len(), snapshot.generation);
        let on_screen = self.active_tab_id() == Some(tid);
        let selected = snapshot.selected;
        self.with_inspector(i, |ui| ui.set_snapshot(snapshot));
        {
            let mut procs = self.procs.borrow_mut();
            if let Some(p) = procs.get_mut(i) {
                // The snapshot does not end the instruction — the tab keeps the
                // pane’s document current (a reload sends the next generation).
                p.inspector_target = Some(tid);
            }
        }
        info!(
            "tab {tid} inspector: {nodes} node(s) at generation {generation}{}",
            selected.map(|n| format!(", selected {n}")).unwrap_or_default()
        );
        if on_screen {
            self.sync_ui();
        }
    }

    /// The page answered ⌖: the node the cursor was on. One pick per arming — the
    /// tab disarms its own picker as it answers (a click that hits nothing gets
    /// no message at all, so the mode stays armed), and the snapshot that names
    /// the node normally follows on the same tick.
    fn on_inspect_picked(&self, tid: TabId, node: u64) {
        let Some(i) = self.procs.borrow().iter().position(|p| p.tab.id == tid) else { return };
        debug!("tab {tid} ⌖ picked node {node}");
        self.disarm_inspect_pick(i);
        // The pick happened *in the page*, so the tree follows it: select the row
        // and ask for it to be scrolled into view (`select_from_page`).
        self.with_inspector(i, |ui| ui.select_from_page(node));
        if self.active_tab_id() == Some(tid) {
            self.sync_ui();
        }
    }

    /// `Clear` in the panel header: the Console and Network panes each empty their
    /// own bounded feed on this tab's record. The Inspector pane has no feed of
    /// its own — its contents *are* the document — so the header disables the
    /// button there and this refuses to guess.
    fn devtools_clear(&self) {
        let Some(i) = self.active_proc_index() else { return };
        let view = self.procs.borrow()[i].devtools_view;
        if view == DevToolsView::Inspector {
            debug!("DevTools Clear ignored on the Inspector pane: the tree is the document");
            return;
        }
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.get_mut(i) else { return };
        match view {
            DevToolsView::Console => {
                p.state.console.clear();
                p.dt_sel_console = None;
            }
            DevToolsView::Network => {
                p.state.network.clear();
                p.dt_sel_network = None;
            }
            DevToolsView::Inspector => {}
        }
        info!("DevTools: cleared the {view:?} feed on tab {}", p.tab.id);
        drop(procs);
        self.sync_ui();
    }

    /// The copy highlight (task: copy in the DevTools tabs): one click on a
    /// Console (pane 0) or Network (pane 1) row marks that row, and *that* is
    /// what the pane's `Copy` button copies. An index the feed does not have is
    /// a feed that moved under the pointer — the click is dropped, so the
    /// highlight never jumps onto a row nobody touched.
    fn devtools_select_row(&self, pane: i32, index: i32) {
        let Some(i) = self.active_proc_index() else { return };
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.get_mut(i) else { return };
        let index = index as usize; // a negative index fails the bounds check below
        let (sel, len) = match pane {
            0 => (&mut p.dt_sel_console, p.state.console.len()),
            1 => (&mut p.dt_sel_network, p.state.network.len()),
            _ => return,
        };
        if index >= len {
            return;
        }
        *sel = Some(index);
        drop(procs);
        self.sync_ui();
    }

    /// The text a row of a pane carries: a Console row is its whole rendered
    /// line (`[source] message`), a Network row its columns joined (see
    /// [`crate::state::net_row_clipboard`]), and an Inspector row — where
    /// `index` is the node id — that node's markup (see
    /// [`crate::state::InspectorUi::copy_text`]).
    fn devtools_row_text(&self, i: usize, pane: i32, index: usize) -> Option<String> {
        let procs = self.procs.borrow();
        let p = procs.get(i)?;
        match pane {
            0 => p.state.console_rows().into_iter().nth(index).map(|r| r.text),
            1 => p.state.net_rows().into_iter().nth(index).map(|r| net_row_clipboard(&r)),
            2 => p
                .inspector
                .as_ref()
                .filter(|_| p.inspector_target == Some(p.tab.id))
                .and_then(|ui| ui.copy_text(index as u64)),
            _ => None,
        }
    }

    /// The panel header's `Copy` button: whatever the pane it sits in has
    /// highlighted — a Console/Network row, or the Inspector's selected node.
    fn devtools_copy(&self) {
        let Some(i) = self.active_proc_index() else { return };
        let (pane, index) = {
            let procs = self.procs.borrow();
            let Some(p) = procs.get(i) else { return };
            match p.devtools_view {
                DevToolsView::Console => match p.dt_sel_console {
                    Some(s) => (0, s),
                    None => return,
                },
                DevToolsView::Network => match p.dt_sel_network {
                    Some(s) => (1, s),
                    None => return,
                },
                DevToolsView::Inspector => {
                    match p.inspector.as_ref().and_then(|ui| ui.selected()) {
                        Some(n) => (2, n as usize),
                        None => return,
                    }
                }
            }
        };
        match self.devtools_row_text(i, pane, index) {
            Some(text) => self.clipboard_set(&text),
            None => debug!("DevTools copy: pane {pane} row {index} has nothing to copy"),
        }
    }

    /// Double-clicking a row: highlight and copy it in one gesture, desktop
    /// style. On the Inspector pane (`index` is the node id) the double-click
    /// selects too, so the highlight ends up matching what was copied.
    fn devtools_copy_row(&self, pane: i32, index: i32) {
        match pane {
            0 | 1 => self.devtools_select_row(pane, index),
            2 => self.inspector_select(index),
            _ => return,
        }
        let Some(i) = self.active_proc_index() else { return };
        match self.devtools_row_text(i, pane, index as usize) {
            Some(text) => self.clipboard_set(&text),
            None => debug!("DevTools copy: nothing at pane {pane} row {index}"),
        }
    }

    // ---- find bar (Ctrl-F) ------------------------------------------------

    /// Whether the active tab's find bar is open.
    fn find_open_active(&self) -> bool {
        let Some(i) = self.active_proc_index() else { return false };
        self.procs.borrow()[i].find_open
    }

    /// Ctrl-`F`: open the active tab's bar, focus the entry, and re-assert the
    /// query that tab was last searching (its own, per tab — the tab process
    /// never dropped its highlights, so reopening the bar finds the page exactly
    /// where the reader left it).
    /// Toggle **reader view** (task: reader mode) on the active tab: the
    /// command behind both the 📄 toolbar button and Ctrl-Alt-`R`. Sent
    /// inverted against the state the tab last reported — the tab is
    /// authoritative, and its `FromTab::ReaderState` answer (including a
    /// refusal: nothing readable) is what updates `TabState::reader` and
    /// repaints the button.
    fn toggle_reader(&self) {
        let enabled = self
            .active_proc_index()
            .is_some_and(|i| !self.procs.borrow()[i].state.reader);
        info!("reader mode: toggling {enabled} on tab {}", self.active_tab_id().map(|id| id.0).unwrap_or(0));
        self.send_active(ToTab::SetReader { enabled });
    }

    /// ⛔ — raise or lower the active tab's popup blocker (task: block popups).
    /// The policy is per tab (each tab is its own process) and only ever says
    /// "a page may not open a window nobody asked for": the user's own clicks
    /// keep working with the blocker up. The tab blocks by default, so a shell
    /// that never sends this message still blocks.
    fn set_popup_block(&self, block: bool) {
        let Some(i) = self.active_proc_index() else {
            debug!("popup switch dropped: no active tab");
            return;
        };
        let msg = self.procs.borrow_mut()[i].state.set_popup_block(block);
        info!("popups: block={block} on tab {}", self.procs.borrow()[i].tab.id.0);
        self.send_active(msg);
        // …and the ⛔ moves now. A tab accepts a policy and never answers it —
        // there is no message coming back with the switch on it — so the button
        // that was just pressed is repainted here or nowhere: waiting for the
        // next `sync_ui` left the reader pressing an icon that only changed
        // once they switched tabs and came back (task: popup blocker tooltip).
        self.popup_switch_refresh();
    }

    /// Re-push the ⛔ and what it has to say for itself on hover, after the
    /// switch moved without a tab ever reporting it (task: popup blocker
    /// tooltip). `sync_ui` pushes the same two on a full snapshot; this is the
    /// one thing in it the reader must not wait for a tab switch to see.
    fn popup_switch_refresh(&self) {
        let Some(ui) = self.ui() else { return };
        let procs = self.procs.borrow();
        if let Some(p) = self.active_proc_index().and_then(|i| procs.get(i)) {
            ui.set_popup_blocked(!p.state.popup_block_off);
            ui.set_popup_tip(p.state.popup_tip().into());
        }
    }

    /// "Allow popups from this site" (Ctrl-Alt-`P`, task: block popups): the
    /// page's own origin goes on this tab's allow-list and the policy is re-sent,
    /// so it applies to the page already on screen — no reload. A page with no
    /// site origin (`about:blank`, `data:`, our internal pages) has nothing to
    /// allow, and saying so beats writing a rule that matches every site.
    fn allow_popup_site(&self) {
        let Some(i) = self.active_proc_index() else {
            debug!("popup allow dropped: no active tab");
            return;
        };
        let msg = {
            let mut procs = self.procs.borrow_mut();
            let Some(msg) = procs[i].state.allow_popup_site(true) else {
                info!(
                    "popups: nothing to allow on tab {} (the page has no site origin)",
                    procs[i].tab.id.0
                );
                return;
            };
            info!("popups: allowed {}", procs[i].state.popup_allowed.join(", "));
            msg
        };
        // The notice the status line was showing is now about a site the user has
        // just allowed; no tab says so, so the line is refreshed by hand.
        self.status_refresh();
        self.send_active(msg);
    }

    fn open_find(&self) {
        let Some(i) = self.active_proc_index() else {
            debug!("find bar dropped: no active tab");
            return;
        };
        let query = self.procs.borrow()[i].state.find.query.clone();
        {
            let mut procs = self.procs.borrow_mut();
            procs[i].find_open = true;
        }
        info!("find bar opened on tab {} (query {query:?})", self.active_tab_id().map(|id| id.0).unwrap_or(0));
        // An empty query here is not a no-op: it tells the tab the bar is
        // searching for nothing yet, which is also how its highlights get
        // cleared after a previous bar was emptied rather than closed.
        self.send_active(ToTab::SetFind { query });
        self.show_find_query();
        self.sync_ui();
        if let Some(ui) = self.ui() {
            let t = ui.get_find_focus_token() + 1;
            ui.set_find_focus_token(t);
        }
    }

    /// The bar's ✕ (and Escape, from inside the entry or from the page): stop
    /// searching — the tab drops its highlights — and hand focus back to the
    /// page. The query stays on the tab's record, so reopening resumes it.
    fn close_find(&self) {
        let Some(i) = self.active_proc_index() else { return };
        if !self.procs.borrow()[i].find_open {
            return;
        }
        self.procs.borrow_mut()[i].find_open = false;
        self.send_active(ToTab::SetFind { query: String::new() });
        self.sync_ui();
        // The page gets the caret back, exactly as Escape left of a typed
        // address does.
        if let Some(ui) = self.ui() {
            let t = ui.get_focus_page_token() + 1;
            ui.set_focus_page_token(t);
        }
    }

    /// Text typed into the entry: search live. The highlight colours are the
    /// tab's business (every match yellow, the current one light blue); the
    /// tab answers with the new counts, which `sync_ui` puts in the bar.
    fn find_text_changed(&self, query: String) {
        if !self.find_open_active() {
            // A stray signal from a bar that was just closed (Slint fires the
            // entry's `changed` on programmatic writes too).
            return;
        }
        self.send_active(ToTab::SetFind { query });
    }

    /// ▲/▼, Enter/Shift-Enter: jump to the previous/next match, wrapping. The
    /// tab scrolls it into view and repaints with the light-blue marker there.
    fn find_jump(&self, forward: bool) {
        if !self.find_open_active() {
            return;
        }
        self.send_active(if forward {
            ToTab::FindNext
        } else {
            ToTab::FindPrevious
        });
    }

    /// Put the active tab's own query into the entry — the only place the query
    /// is ever *written* into the bar, so a sync triggered by something else can
    /// never overwrite half-typed text with the tab's last-confirmed query.
    fn show_find_query(&self) {
        let Some(i) = self.active_proc_index() else { return };
        let query = self.procs.borrow()[i].state.find.query.clone();
        if let Some(ui) = self.ui() {
            if ui.get_find_text().as_str() != query.as_str() {
                ui.set_find_text(query.into());
            }
        }
    }

    /// The find bar's view-model for the active tab: whether it is open, and the
    /// counter the tab reported (`3 / 12`, `No results`). The entry's *text* is
    /// written by [`Core::show_find_query`] only — never here — so a sync
    /// triggered by an unrelated event cannot stomp half-typed text.
    fn write_find(&self, ui: &AppWindow, procs: &[Proc]) {
        let active = self.active.get().min(procs.len().saturating_sub(1));
        let Some(p) = procs.get(active) else {
            ui.set_find_open(false);
            ui.set_find_count("".into());
            return;
        };
        ui.set_find_open(p.find_open);
        ui.set_find_count(p.state.find.label().into());
    }

    /// Write the panel's whole view-model (pane, counts, rows) for the active
    /// tab. Called from `sync_ui`, and again from the drain tick whenever a
    /// console/network message landed: those never report `changed` (they must
    /// not dirty the frame), so nothing else would push them while open.
    fn write_devtools(&self, ui: &AppWindow, procs: &[Proc]) {
        let active = self.active.get().min(procs.len().saturating_sub(1));
        let Some(p) = procs.get(active) else {
            ui.set_devtools_open(false);
            ui.set_console_count(0);
            ui.set_network_count(0);
            return;
        };
        ui.set_devtools_open(p.devtools_open);
        ui.set_devtools_view(p.devtools_view.index());
        // The height belongs to this tab too, so it switches with the pane.
        self.push_devtools_height(ui, p.devtools_height);

        // The models are shared (`Rc<VecModel>` installed once): appending the
        // few rows that arrived keeps the list's scroll position, which a fresh
        // model per tick would throw away.
        // The copy highlight rides the rows themselves: one click highlighted
        // this index, and only an index the feed still has paints or copies.
        let sel_console = p.dt_sel_console;
        let sel_network = p.dt_sel_network;
        sync_model(&self.dt_console, &p.state.console_rows().into_iter().enumerate().map(|(idx, r)| ConsoleRow {
            level: r.level.into(),
            text: r.text.into(),
            selected: sel_console == Some(idx),
        }).collect::<Vec<_>>());
        sync_model(&self.dt_net, &p.state.net_rows().into_iter().enumerate().map(|(idx, r)| NetRow {
            method: r.method.into(),
            url: r.url.into(),
            initiator: r.initiator.into(),
            status: r.status.into(),
            size: r.size.into(),
            time: r.time.into(),
            failed: r.failed,
            selected: sel_network == Some(idx),
        }).collect::<Vec<_>>());
        ui.set_console_count(self.dt_console.row_count() as i32);
        ui.set_network_count(self.dt_net.row_count() as i32);

        // ---- the Inspector pane ------------------------------------------
        // Only a tree captured *for this tab* may paint: node ids are positional
        // handles inside one document, so a record of another one is dropped
        // rather than clicked on blind.
        let model = (p.inspector_target == Some(p.tab.id))
            .then(|| p.inspector.as_ref())
            .flatten();
        // `Copy` answers for the pane it sits in: the row the pane has
        // highlighted, and on the Inspector the selected node (which a tree
        // always has).
        ui.set_devtools_copy_available(match p.devtools_view {
            DevToolsView::Console => sel_console.is_some(),
            DevToolsView::Network => sel_network.is_some(),
            DevToolsView::Inspector => model.is_some_and(|m| m.selected().is_some()),
        });
        ui.set_inspector_pick(model.is_some_and(|ui| ui.pick()));
        ui.set_inspector_truncated(model.is_some_and(|ui| ui.truncated()));
        ui.set_inspector_count(model.map(|ui| ui.element_count()).unwrap_or(0) as i32);
        ui.set_inspector_summary(
            model
                 .map(|ui| ui.summary(Some(p.state.title.as_str())).into())
                .unwrap_or_default(),
        );
        ui.set_inspector_empty(model.is_none_or(|ui| ui.node_count() == 0));
        ui.set_inspector_hint(
            model
                .map(|ui| ui.empty_hint().into())
                .unwrap_or_else(|| NO_TREE_HINT.into()),
        );
        // The tree and its breadcrumb are rebuilt when the view-model changed —
        // a new snapshot, a different selection, a different set of folds — and
        // not on every console line that scrolls past the panel.
        let stamp = model
            .map(|ui| (p.tab.id.0 as usize, ui.generation(), ui.node_count(), ui.selected(), ui.collapsed_len()));
        if *self.dt_inspect_stamp.borrow() != stamp {
            *self.dt_inspect_stamp.borrow_mut() = stamp;
            let rows: Vec<InspectRow> = model
                .map(|ui| {
                    ui.rows()
                        .into_iter()
                        .map(|r| InspectRow {
                            // ids live below `MAX_INSPECT_NODES` inside one
                            // generation, so they fit the window's `int`
                            node: r.node.try_into().unwrap_or(i32::MAX),
                            depth: r.depth as i32,
                            twisty: if !r.has_children {
                                String::new().into()
                            } else if r.collapsed {
                                "+".into()
                            } else {
                                "\u{2212}".into()
                            },
                            // `state::TOK_*` is the colour table the window reads
                            toks: ModelRc::from(Rc::new(VecModel::from(
                                r.toks
                                    .into_iter()
                                    .map(|(text, kind)| Tok { text: text.into(), kind: kind as i32 })
                                    .collect::<Vec<_>>(),
                            ))),
                            selected: r.selected,
                            italic: r.italic,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let crumb: Vec<CrumbItem> = model
                .map(|ui| {
                    ui.crumb_items()
                        .into_iter()
                        .map(|(id, tag, ident)| CrumbItem {
                            node: id.try_into().unwrap_or(i32::MAX),
                            tag: tag.into(),
                            ident: ident.into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            sync_model(&self.dt_inspect, &rows);
            sync_model(&self.dt_crumb, &crumb);
        }
        // …and which row the tree owes a scroll to. This is pushed outside the
        // stamp guard above because picking the *same* node twice leaves the tree
        // identical (no stamp change) and still has to move: the request carries
        // a count that only ever goes up, so the same row asked for again is a
        // change. Setting the row first means the pane computes the offset from
        // the rows it was just given. `-1` says nothing is owed.
        match model.and_then(|ui| ui.reveal_request()) {
            Some((row, token)) => {
                ui.set_inspector_reveal_row(row as i32);
                ui.set_inspector_reveal_token(token as i32);
            }
            None => ui.set_inspector_reveal_row(-1),
        }
    }

    /// Forward one page pointer event. The modifiers come along because two of
    /// the browser's gestures are chords of the *mouse*: a middle click and a
    /// primary-modifier left click (Ctrl here, ⌘ on macOS) both mean "open the
    /// link under the cursor in a new tab", and only the tab knows whether a
    /// link is under the cursor.
    fn page_pointer(
        &self,
        x: f32,
        y: f32,
        kind: i32,
        button: i32,
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    ) {
        use ironsurf_common::{MouseButton, PointerKind};
        let kind = match kind {
            1 => PointerKind::Down,
            2 => PointerKind::Up,
            4 => PointerKind::Out,
            _ => PointerKind::Move,
        };
        let button = match button {
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        if matches!(kind, PointerKind::Down) {
            // A click in the page hands keyboard focus back to it (see the
            // page `TouchArea`), so an address being typed in the bar is
            // abandoned here — from now on the tab's own URL owns the bar
            // again (`sync_ui`).
            self.url_draft.set(None);
        }
        // Hover moves are rate limited: a moving mouse produces far more of
        // them than the tab can hit-test, and every stale one delays the click
        // that follows. Presses, releases and buttons never wait.
        if matches!(kind, PointerKind::Move) {
            let now = Instant::now();
            if !move_due(now, self.last_move_at.get()) {
                self.pending_move.set(Some((x, y)));
                return;
            }
            self.last_move_at.set(Some(now));
            self.pending_move.set(None);
        } else {
            // A press makes the current position authoritative.
            self.pending_move.set(None);
            self.last_move_at.set(Some(Instant::now()));
        }
        self.send_active(ToTab::Pointer(ironsurf_common::PointerEvent {
            x,
            y,
            kind,
            button,
            mods: keymap::mods(ctrl, shift, alt, meta),
        }));
    }

    /// Forward the newest hover move that was dropped as too soon after the
    /// previous one (called from the 33 Hz drain tick, so the cursor position
    /// the tab is showing lags by at most one tick).
    fn flush_pending_move(&self) {
        let Some((x, y)) = self.pending_move.take() else { return };
        let now = Instant::now();
        if !move_due(now, self.last_move_at.get()) {
            self.pending_move.set(Some((x, y)));
            return;
        }
        self.last_move_at.set(Some(now));
        self.send_active(ToTab::Pointer(ironsurf_common::PointerEvent {
            x,
            y,
            kind: ironsurf_common::PointerKind::Move,
            button: ironsurf_common::MouseButton::Left,
            // A hover move that was late, not a different one: the modifiers it
            // was dropped under are nobody's business (only clicks read them).
            mods: ironsurf_common::Modifiers::NONE,
        }));
    }

    fn page_wheel(&self, x: f32, y: f32, delta_x: f32, delta_y: f32) {
        // The reader is scrolling the page on screen *now*, so its overlay
        // scroll bar comes up now too — the tab's own report of the new offset
        // lands a moment later, and the hold window starts at the first wheel
        // click rather than at the last answer (task: scroll bar). The stamp is
        // per tab: the page on screen is the one being scrolled.
        let active = self.active_proc_index();
        let stamped = {
            let mut procs = self.procs.borrow_mut();
            active.map(|i| {
                procs[i].scroll_moved = Some(Instant::now());
                (procs[i].state.scroll, procs[i].scroll_moved)
            })
        };
        self.send_active(ToTab::Scroll(ironsurf_common::ScrollEvent { x, y, delta_x, delta_y }));
        if let (Some(ui), Some((scroll, moved))) = (self.ui(), stamped) {
            push_scrollbar(&ui, &scroll, moved);
        }
    }

    /// Land a tab's hover-cursor report on the UI (task: cursor icon): the
    /// page's `TouchArea` draws the shape, and — because Slint re-reads an
    /// item's `mouse-cursor` only *while processing a pointer event*, and the
    /// mouse may have stopped the very instant the tab answered — pokes a
    /// synthetic pointer move at the last position over the page so the new
    /// shape appears without the user moving. The poke is a plain move (a
    /// press is never synthesised), the rate limiter absorbs it, and the tab
    /// — same point, same cursor — stays silent, so it cannot loop.
    fn apply_page_cursor(&self, cursor: ironsurf_common::CursorIcon) {
        let Some(ui) = self.ui() else { return };
        ui.set_page_cursor(slint_cursor(cursor));
        if ui.get_page_pointer_inside() {
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(
                    ui.get_pointer_x_in_window(),
                    ui.get_pointer_y_in_window(),
                ),
            });
        }
    }

    // ---- context menu --------------------------------------------------------

    /// A tab reported a right-click: build the menu for what the cursor
    /// found (plus the tab's history state and the session clipboard) and
    /// open it at the click point, in page-area coordinates.
    fn open_context_menu(&self, x: f32, y: f32, target: &ContextTarget) {
        let Some(ui) = self.ui() else { return };
        let (url, can_back, can_forward) = match self.active_proc_index() {
            Some(i) => {
                let procs = self.procs.borrow();
                let p = &procs[i];
                (p.state.url.clone(), p.state.can_back, p.state.can_forward)
            }
            None => (None, false, false),
        };
        // Pull the system clipboard before deciding rows, so the Paste row's
        // presence matches exactly what picking it will insert.
        let clipboard_empty = self.paste_text().is_empty();
        let items = context_menu_items(target, url.as_deref(), can_back, can_forward, clipboard_empty);
        debug!("context menu: {} entries", items.len());
        self.menu_items
            .replace(items.iter().map(|(_, a)| a.clone()).collect());
        ui.set_menu_items(ModelRc::from(Rc::new(VecModel::from(
            items
                .into_iter()
                .map(|(label, _)| MenuItem { label: label.into() })
                .collect::<Vec<_>>(),
        ))));
        ui.set_menu_x(x);
        ui.set_menu_y(y);
        ui.set_menu_open(true);
    }

    fn close_context_menu(&self) {
        if let Some(ui) = self.ui() {
            ui.set_menu_open(false);
        }
    }

    /// A context-menu row was picked (or the menu was closed): run the
    /// stored action. Shell-side entries never reach the tab; engine-side
    /// ones go over as `ToTab::ContextAction`.
    fn menu_pick(&self, idx: usize) {
        let action = self.menu_items.borrow().get(idx).cloned();
        self.close_context_menu();
        let Some(action) = action else {
            debug!("context menu pick {idx} has no action (stale menu?)");
            return;
        };
        debug!("context menu: {action:?}");
        match action {
            MenuAction::Back => self.send_active(ToTab::Back),
            MenuAction::Forward => self.send_active(ToTab::Forward),
            MenuAction::Reload => self.send_active(ToTab::Reload),
            MenuAction::OpenNewTab(url) => {
                // Same container as the requesting tab, per the middle-click
                // rule the tab already follows for links.
                let container = self
                    .active_proc_index()
                    .map(|i| self.procs.borrow()[i].tab.container);
                if let Some(container) = container {
                    self.open_tab(container, Some(&url));
                }
            }
            MenuAction::CopyText(text) => self.clipboard_set(&text),
            MenuAction::Paste => {
                let text = self.paste_text();
                if !text.is_empty() {
                    // One stdin batch, two messages: the tab loads the
                    // clipboard into its engine, then synthesizes the Ctrl+V
                    // that consumes it.
                    self.send_active(ToTab::Clipboard { text: text.clone() });
                    self.send_active(ToTab::ContextAction(ContextAction::Paste { text }));
                }
            }
            // The point the menu opened at is the point to ask about: Slint
            // keeps it in `menu-x`/`menu-y` for as long as the menu is up.
            MenuAction::InspectElement => {
                let (x, y) = self
                    .ui()
                    .map(|ui| (ui.get_menu_x(), ui.get_menu_y()))
                    .unwrap_or((0.0, 0.0));
                self.close_context_menu();
                self.inspect_element_at(x, y);
            }
            MenuAction::ToggleVideo => {
                self.send_active(ToTab::ContextAction(ContextAction::ToggleVideo))
            }
            MenuAction::DownloadLink(url) => {
                // The name comes from the URL (`filename_from_url`) and the
                // server's `Content-Disposition`, never from the anchor's
                // text: the shell has no reason to guess.
                self.send_active(ToTab::DownloadUrl {
                    url,
                    suggested_name: None,
                });
            }
        }
    }

    /// Put text on the session clipboard **and** mirror it to the system
    /// clipboard (task: text-selection copy & paste — what the user copies
    /// in a page must be pasteable outside the browser too). An empty copy
    /// is dropped — the clipboard keeps whatever it held, as every browser
    /// does. A system failure is only a log line: the session copy stands.
    fn clipboard_set(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        info!("clipboard: copied {} char(s)", text.chars().count());
        *self.clipboard.borrow_mut() = text.to_owned();
        self.os_clip.borrow_mut().set(text);
    }

    /// The text a paste (Ctrl+V chord or the menu's Paste row) carries:
    /// the system clipboard when it holds text — pulled into the session
    /// clipboard on the way, so the two never diverge backwards — else the
    /// session copy of the last in-browser copy.
    fn paste_text(&self) -> String {
        if let Some(text) = self.os_clip.borrow_mut().get() {
            if *self.clipboard.borrow() != text {
                debug!("clipboard: pulled {} char(s) from the system clipboard", text.chars().count());
                *self.clipboard.borrow_mut() = text.clone();
            }
        }
        self.clipboard.borrow().clone()
    }

    // ---- container manager -----------------------------------------------------

    fn containers_snapshot(&self) -> Vec<ContainerId> {
        self.store.borrow().containers().iter().map(|c| c.id).collect()
    }

    fn container_index(&self, id: ContainerId) -> usize {
        self.containers_snapshot().iter().position(|c| *c == id).unwrap_or(0)
    }

    fn manager_open(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        if open {
            // Two full-window overlays never stack: opening the manager shuts
            // the userscript menu, the UA switcher, and the archive menu
            // (and vice versa — see `userscript_menu`, `ua_menu`,
            // `archive_menu`).
            ui.set_us_menu_open(false);
            ui.set_ua_menu_open(false);
            ui.set_archive_menu_open(false);
            ui.set_history_menu_open(false);
            ui.set_download_menu_open(false);
            ui.set_save_menu_open(false);
            let active_container = self
                .active_proc_index()
                .map(|i| self.procs.borrow()[i].tab.container)
                .unwrap_or_else(|| self.store.borrow().default_container_id());
            let idx = self.container_index(active_container);
            ui.set_selected_container(idx as i32);
            ui.set_editing_index(-1);
        }
        ui.set_manager_open(open);
    }

    fn mgr_new_tab_in(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        self.settings.borrow_mut().new_tab_container = Some(cid.0);
        self.save_settings();
        self.open_tab(cid, Some(&default_url()));
        self.manager_open(false);
    }

    /// Move = re-spawn the active tab in another container keeping its URL
    /// (a tab process is bound to exactly one container/profile/sandbox).
    fn mgr_move_tab_to(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let Some(at) = self.active_proc_index() else { return };
        let (url, id, same) = {
            let procs = self.procs.borrow();
            let p = &procs[at];
            (p.state.url.clone(), p.tab.id, p.tab.container == cid)
        };
        if same {
            self.manager_open(false);
            return;
        }
        let profile_dir = self.store.borrow().profile_dir(cid);
        let (w, h) = self.page_size();
        info!("moving tab {id} to container {}: respawning the process", cid.0);
        // The respawn rides the destination container's UA exactly like
        // `open_tab` does — a per-spawn list, `extra_tab_args` untouched.
        let extra = {
            let mut extra = self.extra_tab_args.clone();
            if let Some(ua) = container_user_agent(&self.store.borrow(), cid) {
                extra.push("--user-agent".to_string());
                extra.push(ua);
            }
            // …and the destination container's proxy (task: proxies).
            if let Some(url) = proxy_url_for(
                &self.settings.borrow(),
                container_proxy(&self.store.borrow(), cid).as_deref(),
            ) {
                extra.push("--proxy".to_string());
                extra.push(url);
            }
            // …and the destination container's private-browsing flag.
            if container_private(&self.store.borrow(), cid) {
                extra.push("--private".to_string());
            }
            if self.settings.borrow().archive_enabled {
                extra.push("--archive-mode".to_string());
            }
            extra
        };
        let replacement = TabProc::spawn(
            &self.tab_bin,
            id,
            cid,
            &profile_dir,
            url.as_deref(),
            &extra.iter().map(String::as_str).collect::<Vec<&str>>(),
            &self.sink,
        );
        if let Ok(tab) = replacement {
            let old = {
                let mut procs = self.procs.borrow_mut();
                let zoom = procs[at].zoom;
                let (devtools_open, devtools_view, devtools_height, find_open) = (
                    procs[at].devtools_open,
                    procs[at].devtools_view,
                    procs[at].devtools_height,
                    procs[at].find_open,
                );
                std::mem::replace(
                    &mut procs[at],
                    Proc {
                        visit_dir: archive::visit_dir_name(),
                        tab,
                        state: TabState::default(),
                        zoom,
                        scroll_moved: None,
                        crashed: false,
                        devtools_open,
                        inspect_stream: false,
                        inspector: None,
                        inspector_target: None,
                        devtools_view,
                        devtools_height,
                        // The old process's feeds are gone; nothing on the new
                        // one is highlighted for copy yet.
                        dt_sel_console: None,
                        dt_sel_network: None,
                        find_open,
                        // A crashed tab's unfinished downloads are reaped
                        // before its process is replaced; the new process
                        // starts with an empty panel (its ids restart at 1),
                        // and with no save armed for the dialog either.
                        downloads: DownloadStore::default(),
                        save_target: None,
                    },
                )
            };
            if w > 0.0 && h > 0.0 {
                let _ = old.tab.send(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
            }
            let mut old = old;
            old.tab.destroy();
            // Downloads do not move with a tab: the old process dies with its
            // unfinished rows `Failed` and their partial files out of the
            // folder, and the new process starts an empty panel (task: file
            // downloads).
            for path in old.downloads.reap(old.tab.id) {
                downloads::discard(&path);
            }
            // The replacement process is new: seed its userscript set too, and
            // re-issue the Inspector instruction — the pane is still open on this
            // tab, and the new process has never been asked for its document.
            self.seed_userscripts(id);
            self.sync_inspect_stream(at);
        }
        self.manager_open(false);
        self.sync_ui();
    }

    fn mgr_delete(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        // Store::delete_container refuses the default and removes the
        // profile dir — that is the data wipe.
        info!("deleting container {} (profile wipe)", cid.0);
        if self.store.borrow_mut().delete_container(cid).is_err() {
            warn!("container {} could not be deleted (default container?)", cid.0);
            self.manager_open(false);
            self.sync_ui();
            return;
        }
        {
            let mut procs = self.procs.borrow_mut();
            procs.retain(|p| p.tab.container != cid);
            if !procs.is_empty() {
                self.active.set(self.active.get().min(procs.len() - 1));
            }
            let empty = procs.is_empty();
            drop(procs);
            if empty {
                let _ = slint::quit_event_loop();
            }
        }
        if self.settings.borrow().new_tab_container == Some(cid.0) {
            self.settings.borrow_mut().new_tab_container = None;
        }
        self.save_settings();
        self.manager_open(false);
    }

    fn mgr_start_rename(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let name = self.store.borrow().container(cid).map(|c| c.name.clone()).unwrap_or_default();
        ui.set_container_edit_text(name.into());
        ui.set_editing_index(idx as i32);
    }

    fn mgr_rename(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let name = self.ui().map(|ui| ui.get_container_edit_text().to_string()).unwrap_or_default();
        if !name.trim().is_empty() {
            let _ = self.store.borrow_mut().rename_container(cid, name.trim());
        }
        if let Some(ui) = self.ui() {
            ui.set_editing_index(-1);
            ui.set_container_edit_text(String::new().into());
        }
        self.sync_ui();
    }

    fn mgr_add(&self) {
        let name = self.ui().map(|ui| ui.get_container_edit_text().trim().to_string()).unwrap_or_default();
        if !name.is_empty() {
            let _ = self.store.borrow_mut().create_container(&name, PALETTE[0]);
        }
        if let Some(ui) = self.ui() {
            ui.set_container_edit_text(String::new().into());
        }
        self.sync_ui();
    }

    fn mgr_recolor(&self, idx: usize, palette: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let color = PALETTE.get(palette).copied().unwrap_or(PALETTE[0]);
        let _ = self.store.borrow_mut().set_container_color(cid, color);
        self.sync_ui();
    }

    fn mgr_select(&self, idx: i32) {
        if let Some(ui) = self.ui() {
            ui.set_selected_container(idx);
        }
    }

    /// The container manager's **Private browsing** checkbox (task: private
    /// browsing): flip the flag on the container, then push the new state to
    /// every **live** tab of that container only — tabs in other containers
    /// keep sending what they were sending ([`Core::private_broadcast`]).
    /// New tabs of the container get the `--private` spawn flag from
    /// `open_tab`. The history half needs no broadcast: recording consults
    /// the flag per navigation, so visits simply stop (or resume) at once.
    fn mgr_toggle_private(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let enabled = !container_private(&self.store.borrow(), cid);
        match self.store.borrow_mut().set_container_private(cid, enabled) {
            Ok(()) => info!("container {} private browsing → {enabled}", cid.0),
            Err(err) => {
                warn!("private browsing not stored for container {}: {err}", cid.0);
                return;
            }
        }
        self.private_broadcast(cid, enabled);
        self.sync_ui();
    }

    /// Push `ToTab::SetDoNotTrack` to every **live** tab of `container`
    /// (crashed and closed tabs skipped, send failures only logged — the
    /// exact [`Core::ua_broadcast`] rules).
    /// Whether the live tab `tid` belongs to a **private** container (task:
    /// private browsing) — what the history recording consults per
    /// navigation. A tid unknown to the process list reads as not private.
    fn tab_is_private(&self, tid: TabId) -> bool {
        self.procs
            .borrow()
            .iter()
            .find(|p| p.tab.id == tid)
            .is_some_and(|p| container_private(&self.store.borrow(), p.tab.container))
    }

    fn private_broadcast(&self, container: ContainerId, enabled: bool) {
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.tab.container != container || p.crashed || p.state.closed {
                continue;
            }
            if let Err(err) = p.tab.send(ToTab::SetDoNotTrack { enabled }) {
                debug!("do-not-track not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    // ---- userscript menu (task: userscript menu) -----------------------------

    /// Every stored userscript, in storage order (integration tests).
    pub fn userscripts(&self) -> Vec<Userscript> {
        self.store.borrow().userscripts()
    }

    /// The id of each userscript-menu row, in `us-scripts` model order
    /// (integration tests).
    pub fn userscript_row_ids(&self) -> Vec<u64> {
        self.us_ids.borrow().clone()
    }

    /// Re-read the list from the Store: refresh the row-id mirror and push the
    /// rows (`name`, patterns joined by `", "`, enabled) onto the menu model.
    fn us_refresh(&self) {
        if let Some(ui) = self.ui() {
            let scripts = self.store.borrow().userscripts();
            *self.us_ids.borrow_mut() = scripts.iter().map(|s| s.id).collect();
            ui.set_us_scripts(ModelRc::from(Rc::new(VecModel::from(userscript_rows(&scripts)))));
        }
    }

    /// Open/close the menu overlay. Opening re-reads the Store, resets the
    /// editor, and closes the container manager: two full-window overlays must
    /// never stack — and the manager closes this one the same way (see
    /// [`Core::manager_open`]).
    fn userscript_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("userscript menu {}", if open { "opened" } else { "closed" });
        if open {
            // Never stack: the manager, the UA switcher, and the archive
            // menu all step aside — and close this menu the same way from
            // their own open legs.
            ui.set_manager_open(false);
            ui.set_ua_menu_open(false);
            ui.set_archive_menu_open(false);
            ui.set_history_menu_open(false);
            ui.set_download_menu_open(false);
            ui.set_save_menu_open(false);
            ui.set_us_editing(false);
            ui.set_us_editing_existing(false);
            ui.set_us_error(SharedString::default());
            *self.us_edit_id.borrow_mut() = None;
            self.us_refresh();
        }
        ui.set_us_menu_open(open);
    }

    /// The toolbar's "</>" button: flip the menu overlay.
    fn toggle_userscript_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_us_menu_open());
        self.userscript_menu(!open);
    }

    /// A row's CheckBox was flipped: persist through the Store **at once** and
    /// rebroadcast. `toggled` fires after the checkbox flipped its own
    /// `checked`, so the wanted state is the negation of the stored one.
    fn us_toggle(&self, idx: usize) {
        let Some(id) = self.us_ids.borrow().get(idx).copied() else { return };
        let now = !self
            .store
            .borrow()
            .userscripts()
            .iter()
            .any(|s| s.id == id && s.enabled);
        if let Err(err) = self.store.borrow_mut().set_userscript_enabled(id, now) {
            warn!("userscript {id} could not be toggled: {err}");
        }
        self.us_broadcast();
        self.us_refresh();
    }

    /// A row's ✕: remove the script (its data is the script itself — nothing
    /// else to wipe) and rebroadcast the rest.
    fn us_delete(&self, idx: usize) {
        let Some(id) = self.us_ids.borrow().get(idx).copied() else { return };
        if let Err(err) = self.store.borrow_mut().delete_userscript(id) {
            warn!("userscript {id} could not be deleted: {err}");
        }
        self.us_broadcast();
        self.us_refresh();
    }

    /// "+ New script": reveal the inline editor with empty fields.
    fn us_new(&self) {
        let Some(ui) = self.ui() else { return };
        *self.us_edit_id.borrow_mut() = None;
        ui.set_us_name(SharedString::default());
        ui.set_us_patterns(SharedString::default());
        ui.set_us_code(SharedString::default());
        ui.set_us_error(SharedString::default());
        ui.set_us_editing_existing(false);
        ui.set_us_editing(true);
    }

    /// A row was clicked (anywhere but its CheckBox / ✕): prefill the editor
    /// with that script and mark the session as an **edit** — Save upserts
    /// the same id instead of minting a new one. Clicking another row while
    /// the editor is up simply retargets it (the unsaved draft is dropped).
    fn us_edit(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        // Row indices resolve through the row-id mirror, exactly like toggle
        // and delete; the script itself comes from the Store (the source of
        // truth, and the only place holding the full code).
        let Some(id) = self.us_ids.borrow().get(idx).copied() else { return };
        let Some(script) = self.store.borrow().userscripts().into_iter().find(|s| s.id == id)
        else { return }; // vanished between refresh and click — nothing to edit
        *self.us_edit_id.borrow_mut() = Some(id);
        ui.set_us_name(script.name.as_str().into());
        ui.set_us_patterns(script.patterns.join(crate::userscripts::PATTERN_SEPARATOR).into());
        ui.set_us_code(script.code.as_str().into());
        ui.set_us_error(SharedString::default());
        ui.set_us_editing_existing(true);
        ui.set_us_editing(true);
    }

    /// Save the editor — as a **new** script (next id) or **in place** (the
    /// id of the row that opened the editor; `enabled` and list position are
    /// the stored script's, everything else the editor's). Validation is the
    /// Store's (`upsert_userscript`): a rejected script is **not** added,
    /// the editor stays up, and the `StoreError` text becomes the menu's
    /// one-line error. An accepted Save rebroadcasts the new enabled set.
    fn us_save(&self) {
        let Some(ui) = self.ui() else { return };
        let (name, patterns, code) = (
            ui.get_us_name().to_string(),
            ui.get_us_patterns().to_string(),
            ui.get_us_code().to_string(),
        );
        let editing = *self.us_edit_id.borrow();
        let script = match editing {
            // Edit: keep the id and the enabled flag (the CheckBox owns that);
            // if the row was deleted out from under the editor, the upsert
            // simply re-inserts under the still-unused id.
            Some(id) => Userscript {
                id,
                name: name.trim().to_string(),
                patterns: parse_patterns(&patterns),
                code,
                enabled: self
                    .store
                    .borrow()
                    .userscripts()
                    .iter()
                    .find(|s| s.id == id)
                    .is_some_and(|s| s.enabled),
            },
            // New: one id per Save, read from the Store at this instant — the
            // shell is the only writer and Save is sequential, so
            // `next_userscript_id` is fresh here; two Saves always produce two
            // distinct scripts. (An allocator that reads `next` up-front and
            // upserts later hands the same id out twice — the silent overwrite
            // the userscripts e2e hit.)
            None => build_new_userscript(self.store.borrow().next_userscript_id(), &name, &patterns, &code),
        };
        if let Err(err) = self.store.borrow_mut().upsert_userscript(script) {
            warn!("userscript {name:?} rejected: {err}");
            ui.set_us_error(err.to_string().into());
            return; // script not added/changed; the editor keeps the draft
        }
        *self.us_edit_id.borrow_mut() = None;
        ui.set_us_error(SharedString::default());
        ui.set_us_editing(false);
        ui.set_us_editing_existing(false);
        ui.set_us_name(SharedString::default());
        ui.set_us_patterns(SharedString::default());
        ui.set_us_code(SharedString::default());
        self.us_broadcast();
        self.us_refresh();
    }

    /// "Cancel": drop the draft (or the edit), keep the list.
    fn us_cancel(&self) {
        let Some(ui) = self.ui() else { return };
        *self.us_edit_id.borrow_mut() = None;
        ui.set_us_editing(false);
        ui.set_us_editing_existing(false);
        ui.set_us_error(SharedString::default());
    }

    /// The script set as it rides the wire: **all enabled** scripts (contract:
    /// disabled ones are never sent; an empty set clears a tab).
    fn us_payload(&self) -> Vec<Userscript> {
        enabled_userscripts(&self.store.borrow().userscripts())
    }

    /// After any mutation (toggle / save / delete): push the new enabled set to
    /// every **live** tab, best effort. Crashed and already-closed tabs are
    /// skipped, and a send that fails on a tab the drain tick has not yet
    /// noticed dying is ignored — `check_crashes` makes that noise, matching
    /// the existing crash handling (see `set_visible_except`).
    fn us_broadcast(&self) {
        let scripts = self.us_payload();
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.crashed || p.state.closed {
                continue;
            }
            if let Err(err) = p.tab.send(ToTab::SetUserscripts { scripts: scripts.clone() }) {
                debug!("userscripts not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    /// Hand the current enabled set to one freshly spawned tab process: a new
    /// process starts empty, so the shell seeds it (the empty set is sent too
    /// — it provably clears a respawned tab).
    fn seed_userscripts(&self, tid: TabId) {
        let scripts = self.us_payload();
        let _ = self.send_to(tid, ToTab::SetUserscripts { scripts });
    }

    // ---- User-Agent switcher (task: per-container UA) -------------------------

    /// The container of the active tab — the one the UA menu reads and
    /// mutates (and the container manager selects on open).
    fn active_container(&self) -> ContainerId {
        self.active_proc_index()
            .map(|i| self.procs.borrow()[i].tab.container)
            .unwrap_or_else(|| self.store.borrow().default_container_id())
    }

    /// Push the menu's live state for the active tab's container: the
    /// `ua-current` display label and the `ua-rows` list (default row first,
    /// then the five presets, the container's value highlighted). Called on
    /// every (re)open, after every mutation, and on every tab switch.
    fn ua_state(&self, ui: &AppWindow) {
        let current = container_user_agent(&self.store.borrow(), self.active_container());
        ui.set_ua_current(ua_display(current.as_deref()).as_str().into());
        ui.set_ua_rows(ModelRc::from(Rc::new(VecModel::from(ua_menu_rows(
            current.as_deref(),
        )))));
    }

    /// A tab switch moved the menu to another container: re-point rows and
    /// current label (and drop the previous container's stale error line).
    fn ua_refresh_if_open(&self) {
        if let Some(ui) = self.ui() {
            if ui.get_ua_menu_open() {
                self.ua_state(&ui);
                ui.set_ua_error(SharedString::default());
            }
        }
    }

    /// Open/close the UA overlay. Opening re-reads the Store for the active
    /// tab's container, prefills the custom draft with the container's raw
    /// value, and closes the userscript menu **and** the container manager —
    /// two full-window overlays never stack, and both close this one the same
    /// way (see [`Core::manager_open`] / [`Core::userscript_menu`]).
    fn ua_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("user-agent menu {}", if open { "opened" } else { "closed" });
        if open {
            ui.set_manager_open(false);
            ui.set_us_menu_open(false);
            ui.set_archive_menu_open(false);
            ui.set_history_menu_open(false);
            ui.set_download_menu_open(false);
            ui.set_save_menu_open(false);
            ui.set_ua_error(SharedString::default());
            let current = container_user_agent(&self.store.borrow(), self.active_container());
            // Prefill the draft with exactly what the container holds (a
            // custom string shows itself; a preset or the default starts
            // blank — like the userscript editor prefilling its row).
            let draft = current
                .filter(|ua| ironsurf_common::builtin_user_agent_label(ua).is_none())
                .unwrap_or_default();
            ui.set_ua_custom(draft.as_str().into());
            self.ua_state(&ui);
        }
        ui.set_ua_menu_open(open);
    }

    /// The toolbar's "UA" button: flip the overlay.
    fn toggle_ua_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_ua_menu_open());
        self.ua_menu(!open);
    }

    /// Persist `ua` for the container the menu currently targets and push it
    /// onto that container's live tabs. A Store rejection (bad characters,
    /// too long) rides the one-line `ua-error` and changes nothing at all —
    /// rows, stored value, and the wire stay as they were, exactly like the
    /// userscripts Save leg.
    fn apply_ua(&self, ui: &AppWindow, ua: Option<&str>) {
        let cid = self.active_container();
        if let Err(err) = self.store.borrow_mut().set_container_user_agent(cid, ua) {
            warn!("container {} rejected a user agent: {err}", cid.0);
            ui.set_ua_error(err.to_string().into());
            return;
        }
        ui.set_ua_error(SharedString::default());
        self.ua_broadcast(cid, ua);
        self.ua_state(ui);
    }

    /// A row was clicked: select that UA for the container. Row indices
    /// resolve through the pushed `ua-rows` model — row 0 (empty `ua`) is
    /// "Default (IronSurf)", the others carry their preset string verbatim.
    fn ua_select(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        let Some(row) = ui.get_ua_rows().row_data(idx) else { return };
        let ua = if row.ua.is_empty() { None } else { Some(row.ua.as_str()) };
        self.apply_ua(&ui, ua);
    }

    /// "Restore default": the container drops its override.
    fn ua_restore_default(&self) {
        if let Some(ui) = self.ui() {
            self.apply_ua(&ui, None);
        }
    }

    /// The custom-UA LineEdit → Apply: [`normalize_custom_ua`] first (trim;
    /// all-blank = restore the default), then the same Store leg as a row
    /// pick. A rejected string shows on the one line **with the draft kept**;
    /// an accepted one clears the draft and updates rows and label.
    fn ua_apply_custom(&self, input: &str) {
        let Some(ui) = self.ui() else { return };
        let normalized = normalize_custom_ua(input);
        self.apply_ua(&ui, normalized.as_deref());
        if container_user_agent(&self.store.borrow(), self.active_container()) == normalized {
            // only after success — on rejection the draft must survive as typed
            ui.set_ua_custom(SharedString::default());
        }
    }

    /// "Copy": the *effective* UA of the menu's container on the session
    /// clipboard — the override if there is one, the tab default otherwise.
    fn ua_copy(&self) {
        let effective = container_user_agent(&self.store.borrow(), self.active_container())
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        self.clipboard_set(&effective);
    }

    /// After any accepted mutation: push the new value to every **live** tab
    /// of that container only — tabs in other containers must not receive it
    /// (each tab applies its own container's value). Crashed and closed tabs
    /// are skipped and send failures ignored, exactly like
    /// [`Core::us_broadcast`] — the crash reporter owns those tabs.
    fn ua_broadcast(&self, container: ContainerId, ua: Option<&str>) {
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.tab.container != container || p.crashed || p.state.closed {
                continue;
            }
            let msg = ToTab::SetUserAgent { user_agent: ua.map(str::to_string) };
            if let Err(err) = p.tab.send(msg) {
                debug!("user agent not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    // ---- Proxy switcher (task: proxies) ------------------------------------

    /// Push the menu's live state for the active tab's container: the
    /// `proxy-current` label ("Direct" or the stored definition name — a
    /// dangling name still shows, just with no highlighted row) and the
    /// `proxy-rows` list (Direct first, then every global definition from
    /// the settings, the container's pick marked active). Called on every
    /// (re)open, after every mutation, and on every tab switch — like
    /// [`Core::ua_state`].
    fn proxy_state(&self, ui: &AppWindow) {
        let name = container_proxy(&self.store.borrow(), self.active_container());
        ui.set_proxy_current(
            name.clone()
                .unwrap_or_else(|| "Direct".to_string())
                .as_str()
                .into(),
        );
        let settings = self.settings.borrow();
        let mut rows: Vec<ProxyRow> = Vec::with_capacity(settings.proxies.len() + 1);
        rows.push(ProxyRow {
            name: "Direct".into(),
            url: SharedString::default(),
            active: name.is_none(),
        });
        for d in &settings.proxies {
            rows.push(ProxyRow {
                name: d.name.as_str().into(),
                url: d.url.as_str().into(),
                active: name.as_deref() == Some(d.name.as_str()),
            });
        }
        ui.set_proxy_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    /// A tab switch moved the drawer's proxy section to another container:
    /// re-point rows and current label (and drop the stale error line) —
    /// the UA menu's rule, now gated on the settings drawer being open
    /// since that is where the section lives.
    fn proxy_refresh_if_open(&self) {
        if let Some(ui) = self.ui() {
            if ui.get_history_menu_open() {
                self.proxy_state(&ui);
                ui.set_proxy_error(SharedString::default());
            }
        }
    }

    /// A row was clicked: route the menu's container through that proxy
    /// (row 0 = Direct). The *name* persists on the container in
    /// `containers.json`; the container's live tabs get the resolved URL, so
    /// every later request — documents, subresources, XHR, media, downloads
    /// — rides it. Requests already in flight finish on the old route.
    fn proxy_select(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        let Some(row) = ui.get_proxy_rows().row_data(idx) else { return };
        let name = if row.url.is_empty() {
            None // the Direct row
        } else {
            Some(row.name.as_str())
        };
        let cid = self.active_container();
        if let Err(err) = self.store.borrow_mut().set_container_proxy(cid, name) {
            warn!("container {} rejected a proxy: {err}", cid.0);
            ui.set_proxy_error(err.to_string().into());
            return;
        }
        ui.set_proxy_error(SharedString::default());
        let url = proxy_url_for(&self.settings.borrow(), name);
        self.proxy_broadcast(cid, url);
        self.proxy_state(&ui);
        // The manager rows (when it is next opened, or already up behind)
        // restate each container's proxy suffix.
        self.sync_ui();
    }

    /// Push an already-resolved proxy URL (`None` = direct) onto every
    /// **live** tab of one container only — tabs of other containers keep
    /// their own route. Skips and quiet-fail exactly like
    /// [`Core::ua_broadcast`].
    fn proxy_broadcast(&self, container: ContainerId, url: Option<String>) {
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.tab.container != container || p.crashed || p.state.closed {
                continue;
            }
            let msg = ToTab::SetProxy { proxy: url.clone() };
            if let Err(err) = p.tab.send(msg) {
                debug!("proxy not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    /// After a **global** definition changed (added or deleted): re-resolve
    /// every live tab's route. Deleting strands each container that picked
    /// it (name kept, traffic direct); re-adding under the same name
    /// reconnects them — one uniform pass over the live tabs keeps every
    /// route honest without hunting for which containers referenced a name.
    fn proxy_rebroadcast_all(&self) {
        // Routes resolved once per container (registry + settings cloned
        // out of their RefCells, since sending runs with `procs` borrowed
        // mutably) — no live tab of a container is ever sent a stale route.
        let settings = self.settings.borrow().clone();
        let routes: Vec<(ContainerId, Option<String>)> = {
            let store = self.store.borrow();
            store
                .containers()
                .iter()
                .map(|c| {
                    (
                        c.id,
                        proxy_url_for(&settings, container_proxy(&store, c.id).as_deref()),
                    )
                })
                .collect()
        };
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.crashed || p.state.closed {
                continue;
            }
            let Some((_, url)) = routes.iter().find(|(cid, _)| *cid == p.tab.container) else {
                continue;
            };
            let msg = ToTab::SetProxy {
                proxy: url.clone(),
            };
            if let Err(err) = p.tab.send(msg) {
                debug!("proxy not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    /// "Del" on a definition row: drop it from the global pool, persist the
    /// settings, and re-resolve every live tab. Containers that had picked
    /// it keep the stored name (a re-add under the same name reconnects
    /// them) and ride direct in the meantime — the resolution rule, not a
    /// rewrite of the registry. Row 0 (Direct) has no button and is
    /// undeletable; an out-of-range index is ignored.
    fn proxy_delete(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        if idx == 0 {
            return;
        }
        {
            let mut settings = self.settings.borrow_mut();
            if settings.proxies.len() + 1 != ui.get_proxy_rows().row_count() {
                // Stale model (settings changed under the menu): re-push and
                // make the user click again rather than delete a neighbour.
                self.proxy_state(&ui);
                return;
            }
            settings.proxies.remove(idx - 1);
        }
        self.save_settings();
        ui.set_proxy_error(SharedString::default());
        self.proxy_rebroadcast_all();
        self.proxy_state(&ui);
        self.sync_ui();
    }

    /// "Add" a global definition: name unique and bounded, URL validated by
    /// [`ironsurf_common::validate_proxy_url`] (trimmed on success). A
    /// rejection rides the one-line `proxy-error` with **both drafts kept**
    /// (UA-menu style); success persists `ui.json`, clears the drafts, and
    /// re-resolves the live tabs (a container whose dangling name just got
    /// re-added rides the new URL from here).
    fn proxy_add(&self) {
        let Some(ui) = self.ui() else { return };
        let name = ui.get_proxy_add_name().trim().to_string();
        let reject = |ui: &AppWindow, msg: String| {
            warn!("proxy definition rejected: {msg}");
            ui.set_proxy_error(msg.into());
        };
        if name.is_empty() {
            return reject(&ui, "proxy definitions need a name".to_string());
        }
        if name.chars().any(|c| c.is_control()) {
            return reject(&ui, "proxy name must not contain control characters".to_string());
        }
        if name.chars().count() > ironsurf_common::MAX_PROXY_NAME_CHARS {
            return reject(
                &ui,
                format!(
                    "proxy name too long: max {} characters",
                    ironsurf_common::MAX_PROXY_NAME_CHARS
                ),
            );
        }
        if self.settings.borrow().proxies.iter().any(|d| d.name == name) {
            return reject(&ui, format!("a proxy named \"{name}\" is already defined"));
        }
        let url = match ironsurf_common::validate_proxy_url(ui.get_proxy_add_url().as_str()) {
            Ok(url) => url,
            Err(err) => return reject(&ui, err.to_string()),
        };
        self.settings
            .borrow_mut()
            .proxies
            .push(ironsurf_common::ProxyDef { name, url });
        self.save_settings();
        ui.set_proxy_add_name(SharedString::default());
        ui.set_proxy_add_url(SharedString::default());
        ui.set_proxy_error(SharedString::default());
        self.proxy_rebroadcast_all();
        self.proxy_state(&ui);
    }

    // ---- archive mode (task: archive mode) ---------------------------------

    /// The effective archive root as display text.
    fn archive_root_text(&self) -> String {
        self.archive_dir.borrow().display().to_string()
    }

    /// The overlay's one-line status: state, effective root, and how many
    /// files this shell session has written so far.
    fn archive_status(&self) -> String {
        if self.settings.borrow().archive_enabled {
            format!(
                "On \u{2014} saving to {} ({} file(s) saved this session)",
                self.archive_root_text(),
                self.archive_saved.get()
            )
        } else {
            format!(
                "Off \u{2014} pages and files would be saved under {}",
                self.archive_root_text()
            )
        }
    }

    /// Re-push the menu's live values (CheckBox + status line).
    fn archive_state(&self, ui: &AppWindow) {
        ui.set_archive_enabled(self.settings.borrow().archive_enabled);
        ui.set_archive_status(self.archive_status().as_str().into());
    }

    /// Refresh the status line while the menu is open (a file just landed).
    fn archive_refresh_if_open(&self) {
        if let Some(ui) = self.ui() {
            if ui.get_archive_menu_open() {
                self.archive_state(&ui);
            }
        }
    }

    /// Open/close the archive overlay. Opening closes the manager, the
    /// userscript menu, and the UA switcher (two full-window overlays never
    /// stack; the other three menus close this one the same way from their
    /// own open legs) and prefills the folder draft with the effective root.
    fn archive_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("archive menu {}", if open { "opened" } else { "closed" });
        if open {
            ui.set_manager_open(false);
            ui.set_us_menu_open(false);
            ui.set_ua_menu_open(false);
            ui.set_history_menu_open(false);
            ui.set_download_menu_open(false);
            ui.set_save_menu_open(false);
            ui.set_archive_error(SharedString::default());
            ui.set_archive_dir_text(self.archive_root_text().as_str().into());
        }
        ui.set_archive_menu_open(open);
        if open {
            self.archive_state(&ui);
        }
    }

    /// The toolbar's 💾 button: flip the overlay.
    fn toggle_archive_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_archive_menu_open());
        self.archive_menu(!open);
    }

    // ---- "Save page" dialog (task: save-page selector) ---------------------

    /// Open or close the "Save page" dialog (Ctrl-`S`). Opening closes every
    /// other menu (the same never-stacked rule as the ⬇ panel) and prefills
    /// both fields — the folder with the effective ⬇ root (defaulting to
    /// `$HOME/Downloads`), the name with the page's own file name when the
    /// URL has one, else the title wearing `.html` — and arms the tab's
    /// [`SaveTarget`], which is what the folder chosen in the dialog then
    /// sits on. A tab with no real address has nothing to save: the dialog
    /// says so on its one error line instead of opening empty.
    fn save_dialog(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("save page dialog {}", if open { "opened" } else { "closed" });
        if !open {
            ui.set_save_menu_open(false);
            return;
        }
        ui.set_manager_open(false);
        ui.set_us_menu_open(false);
        ui.set_ua_menu_open(false);
        ui.set_archive_menu_open(false);
        ui.set_history_menu_open(false);
        ui.set_download_menu_open(false);
        let Some(i) = self.active_proc_index() else { return };
        let (page, title, saved) = {
            let procs = self.procs.borrow();
            match procs.get(i) {
                Some(p) => (
                    p.state.url.clone().unwrap_or_default(),
                    p.state.title.clone(),
                    p.save_target.clone(),
                ),
                None => return,
            }
        };
        if page.is_empty() || page.starts_with("about:") || page.starts_with("ironsurf://") {
            ui.set_save_error("There is no page here to save yet.".into());
            return;
        }
        let folder = saved
            .as_ref()
            .map(|t| t.folder.clone())
            .unwrap_or_else(|| self.download_dir.borrow().clone());
        let name = saved
            .as_ref()
            .and_then(|t| t.name.clone())
            .unwrap_or_else(|| downloads::save_page_name(&page, &title));
        let name = downloads::save_name_or(&name, "page.html");
        if let Some(p) = self.procs.borrow_mut().get_mut(i) {
            p.save_target = Some(SaveTarget {
                page: page.clone(),
                folder: folder.clone(),
                name: Some(name.clone()),
            });
        }
        let folder = folder.display().to_string();
        ui.set_save_page(page.as_str().into());
        ui.set_save_dir_text(folder.as_str().into());
        ui.set_save_name_text(name.as_str().into());
        ui.set_save_error(SharedString::default());
        ui.set_save_menu_open(true);
    }

    /// Ctrl-`S` (task: save-page selector): flip the dialog.
    fn toggle_save_dialog(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_save_menu_open());
        self.save_dialog(!open);
    }

    /// The dialog's "Choose \u{1F4C1}": the typed folder follows the ⬇ panel's
    /// rules exactly (`~` expanded, absolute or refused, created, and a
    /// rejection keeps the draft while the one line carries the reason) and
    /// becomes where **this** save lands. The ⬇ root is not re-pointed —
    /// saving one page elsewhere is exactly what the dialog is for.
    fn save_pick_folder(&self, text: &str) {
        let Some(ui) = self.ui() else { return };
        match downloads::validate_dir(text) {
            Ok(path) => {
                if let Some(i) = self.active_proc_index() {
                    if let Some(p) = self.procs.borrow_mut().get_mut(i) {
                        if let Some(t) = p.save_target.as_mut() {
                            t.folder = path.clone();
                        }
                    }
                }
                let shown = path.display().to_string();
                ui.set_save_dir_text(shown.as_str().into());
                ui.set_save_error(SharedString::default());
                info!("save page dialog: folder \u{2192} {shown}");
            }
            Err(err) => ui.set_save_error(err.as_str().into()),
        }
    }

    /// The dialog's "Save": the name field is law (sanitized to one
    /// component; blank or traversal falls back to the armed name), the tab
    /// is then asked to fetch **its current** page through the ordinary
    /// [`ToTab::DownloadUrl`] channel with that name as the hint, and the
    /// armed [`SaveTarget`] steers the bytes into the chosen folder (see
    /// `download_write`). The row appears in the ⬇ panel like any other
    /// save, and shutting the dialog does not stop the write. A dead pipe
    /// says so on the dialog's one line instead of pretending a save.
    fn save_page_confirm(&self, text: &str) {
        let Some(ui) = self.ui() else { return };
        let Some(i) = self.active_proc_index() else { return };
        let page = {
            let procs = self.procs.borrow();
            procs.get(i).and_then(|p| p.state.url.clone()).unwrap_or_default()
        };
        if page.is_empty() || page.starts_with("about:") || page.starts_with("ironsurf://") {
            ui.set_save_error("There is no page here to save yet.".into());
            return;
        }
        let fallback = {
            let procs = self.procs.borrow();
            procs
                .get(i)
                .and_then(|p| p.save_target.as_ref())
                .and_then(|t| t.name.clone())
        }
        .unwrap_or_else(|| downloads::save_page_name(&page, ""));
        let name = downloads::save_name_or(text, &downloads::save_name_or(&fallback, "page.html"));
        // Re-arm for the page as it stands now (it may have navigated since
        // the dialog opened), holding the folder the dialog shows.
        let folder = {
            let mut procs = self.procs.borrow_mut();
            let Some(p) = procs.get_mut(i) else { return };
            let target = p.save_target.get_or_insert(SaveTarget {
                page: page.clone(),
                folder: self.download_dir.borrow().clone(),
                name: None,
            });
            target.page = page.clone();
            target.name = Some(name.clone());
            target.folder.clone()
        };
        let sent = {
            let procs = self.procs.borrow();
            procs
                .get(i)
                .map(|p| {
                    p.tab.send(ToTab::DownloadUrl {
                        url: page.clone(),
                        suggested_name: Some(name.clone()),
                    })
                })
                .map_or(false, |r| r.is_ok())
        };
        if !sent {
            ui.set_save_error("The tab will not take the save \u{2014} its process is gone.".into());
            return;
        }
        info!("save page {page} \u{2192} {}/{name}", folder.display());
        self.save_dialog(false);
    }

    // ---- file downloads (task: file downloads) -------------------------------

    /// The effective downloads folder as display text (the ⬇ panel's summary
    /// and the LineEdit's prefill: one folder, named once).
    fn download_root_text(&self) -> String {
        self.download_dir.borrow().display().to_string()
    }

    /// The ⬇ panel's one-line summary (rows of the tab on screen + the folder
    /// receiving them). Rows are per process, so the count says what the
    /// reader can see, not what every tab ever saved.
    fn download_summary(&self) -> String {
        let procs = self.procs.borrow();
        let count = self
            .active_proc_index()
            .and_then(|i| procs.get(i))
            .map(|p| p.downloads.rows().len())
            .unwrap_or(0);
        downloads::summary_line(count, &self.download_dir.borrow())
    }

    /// The active tab's downloads, newest first, capped to
    /// [`downloads::MENU_ROWS`]: the panel is a window onto the store, which
    /// keeps more ([`crate::state::MAX_DOWNLOAD_ROWS]). A row quotes its full
    /// path once the file is complete; until then it says how much has landed
    /// and where from.
    fn download_rows(&self) -> Vec<DownloadItem> {
        let procs = self.procs.borrow();
        let Some(active) = self
            .active_proc_index()
            .and_then(|i| procs.get(i))
        else {
            return Vec::new();
        };
        let mut rows: Vec<&DownloadRow> = active.downloads.rows().iter().collect();
        rows.sort_by(|a, b| b.stamp.cmp(&a.stamp));
        rows.truncate(downloads::MENU_ROWS);
        rows
            .into_iter()
            .map(|row| {
                let sub = row
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| downloads::sub_line(row.bytes, &row.url));
                DownloadItem {
                    name: row.name.clone().into(),
                    status: downloads::status_label(row.state, row.bytes).into(),
                    sub: sub.into(),
                }
            })
            .collect()
    }

    /// Push the panel's live values (folder summary + rows).
    fn download_state(&self, ui: &AppWindow) {
        ui.set_download_status(self.download_summary().as_str().into());
        ui.set_downloads(ModelRc::from(Rc::new(VecModel::from(self.download_rows()))));
    }

    /// Refresh the panel while it is open (a chunk just landed).
    fn download_refresh_if_open(&self) {
        if let Some(ui) = self.ui() {
            if ui.get_download_menu_open() {
                self.download_state(&ui);
            }
        }
    }

    /// The status bar's **one** message for a tab: its error, its own status
    /// text, a spawn failure, then a download — the newest row of that tab, a
    /// `Saving` one before a settled one — and only then the generic
    /// "Loading …" (task: file downloads: a download is worth naming before
    /// the page is worth spinning). Extracted from `sync_ui` so a chunk
    /// updates the same line the tab switch would.
    fn status_text(&self, p: &Proc) -> String {
        let download = p
            .downloads
            .newest()
            .map(|row| downloads::status_line(&row.name, row.state, row.bytes));
        p.state
            .error
            .clone()
            // A page the tab stopped from opening windows outranks the tab's own
            // idle status line (task: block popups) — but never a load in flight,
            // because "Waiting for host…" is the newer fact.
            .or_else(|| {
                (!p.state.loading)
                    .then(|| p.state.popup_notice())
                    .flatten()
            })
            .or_else(|| p.state.status.clone())
            .or_else(|| self.spawn_error.borrow().clone())
            .or(download)
            .or_else(|| p.state.loading.then(|| "Loading \u{2026}".to_string()))
            .unwrap_or_default()
    }

    /// Re-push the status line after a row moved, so a download is visible
    /// without the panel (the newest row of the tab on screen wins).
    /// Re-push the active tab's status line after something it reports changed
    /// without a message from any tab — here, allowing a site retires the
    /// blocked-popup notice (task: block popups).
    fn status_refresh(&self) {
        let Some(ui) = self.ui() else { return };
        let procs = self.procs.borrow();
        if let Some(status) = self
            .active_proc_index()
            .and_then(|i| procs.get(i))
            .map(|p| self.status_text(p))
        {
            ui.set_status_text(status.into());
        }
    }

    fn download_status_refresh(&self) {
        let Some(ui) = self.ui() else { return };
        let procs = self.procs.borrow();
        if let Some(status) = self
            .active_proc_index()
            .and_then(|i| procs.get(i))
            .map(|p| self.status_text(p))
        {
            ui.set_status_text(status.into());
        }
    }

    /// Open/close the ⬇ overlay. Opening closes every other menu (the same
    /// never-stacked rule as the archive overlay) and prefills the folder
    /// draft with the effective root.
    fn download_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("downloads menu {}", if open { "opened" } else { "closed" });
        if open {
            ui.set_manager_open(false);
            ui.set_us_menu_open(false);
            ui.set_ua_menu_open(false);
            ui.set_history_menu_open(false);
            ui.set_archive_menu_open(false);
            ui.set_save_menu_open(false);
            ui.set_download_error(SharedString::default());
            ui.set_download_dir_text(self.download_root_text().as_str().into());
        }
        ui.set_download_menu_open(open);
        if open {
            self.download_state(&ui);
        }
    }

    /// The toolbar's ⬇ button (and Ctrl-Shift-D): flip the overlay.
    fn toggle_download_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_download_menu_open());
        self.download_menu(!open);
    }

    /// A folder typed into the ⬇ panel (or Enter in its LineEdit). Accepted →
    /// effective at once and persisted; rejected → the draft stays with the
    /// reason under it. A blank draft reverts to the default folder (the
    /// archive folder's rule, inverted on purpose: a blank says "give it
    /// back to the browser").
    fn download_apply_dir(&self, text: &str) {
        let Some(ui) = self.ui() else { return };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            *self.download_dir.borrow_mut() = downloads::download_root(None);
            self.settings.borrow_mut().download_dir = None;
            self.save_settings();
            ui.set_download_error(SharedString::default());
            let root = self.download_root_text();
            ui.set_download_dir_text(root.as_str().into());
            info!("download folder \u{2192} default ({root})");
            self.download_state(&ui);
            return;
        }
        match downloads::validate_dir(trimmed) {
            Ok(path) => {
                let shown = path.display().to_string();
                *self.download_dir.borrow_mut() = path;
                self.settings.borrow_mut().download_dir = Some(shown.clone());
                self.save_settings();
                ui.set_download_error(SharedString::default());
                ui.set_download_dir_text(shown.as_str().into());
                info!("download folder \u{2192} {shown}");
            }
            Err(err) => {
                ui.set_download_error(err.as_str().into());
                return;
            }
        }
        self.download_state(&ui);
    }

    /// `Clear finished`: drop the settled rows of every tab, keep the ones
    /// still writing. The files are left where they are — clearing a list is
    /// not deleting what the list named.
    fn download_clear_finished(&self) {
        let mut dropped = 0;
        for p in self.procs.borrow_mut().iter_mut() {
            dropped += p.downloads.clear_finished().len();
        }
        info!("downloads: cleared {dropped} finished row(s)");
        if let Some(ui) = self.ui() {
            if ui.get_download_menu_open() {
                self.download_state(&ui);
            }
        }
        self.download_status_refresh();
    }

    /// A [`ironsurf_common::FromTab::DownloadStart`] from this tab: file the
    /// row, name the file, and **create it now** (`O_CREAT | O_EXCL` through
    /// the unique path), so a crash mid-fetch leaves a partial file the shell
    /// can delete. A folder that refuses the file (read-only, gone) fails the
    /// row at once — a row that says `Saving` with no file would lie.
    fn download_write(&self, tid: TabId, hint: &ironsurf_common::DownloadHint) {
        // Paths and the stamp counter first: `procs` is borrowed for the
        // store, and no UI refresh may happen while it is held.
        // "Save page" (task: save-page selector): a folder chosen in the
        // dialog is one-shot — it is consumed by the save it was armed for
        // (the row whose URL is that very page), so the page lands where the
        // user pointed while the ⬇ root keeps receiving everything else.
        let root = {
            let mut procs = self.procs.borrow_mut();
            match procs.iter_mut().find(|p| p.tab.id == tid) {
                Some(p) => match p.save_target.take() {
                    Some(t) if t.page == hint.url => t.folder,
                    _ => self.download_dir.borrow().clone(),
                },
                None => self.download_dir.borrow().clone(),
            }
        };
        let name = downloads::display_name(hint.name.as_deref(), &hint.url);
        let path = downloads::download_path(&root, hint.name.as_deref(), &hint.url);
        let stamp = self.download_stamp.get() + 1;
        self.download_stamp.set(stamp);
        let created = match downloads::create(&path) {
            Ok(_file) => Some(path.clone()),
            Err(err) => {
                error!("tab {tid} download #{}: {err}", hint.id);
                None
            }
        };
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.iter_mut().find(|p| p.tab.id == tid) else {
            // A tab that died between its Start and this message: the file it
            // made is nothing but a partial, so it never gets to stay.
            if created.is_some() {
                downloads::discard(&path);
            }
            return;
        };
        p.downloads.start(tid, hint, name.clone(), created.clone(), stamp);
        if created.is_none() {
            // Nothing was ever written: the row says `Failed` and has no
            // path, which is the truth about a disk that refused the file.
            let reason = format!("cannot create {}", path.display());
            p.downloads.finish(tid, hint.id, Some(&reason));
        }
        drop(procs);
        info!(
            "tab {tid} download #{} {name} \u{2192} {} ({:?})",
            hint.id,
            path.display(),
            hint.trigger
        );
        // A save announces itself (task: file downloads): the ⬇ panel opens at
        // a row that just started, so the reader sees the bytes land instead
        // of having to ask — for **this** tab, whose rows the panel shows (a
        // background tab's save keeps its status line and its panel shut).
        if self.active_tab_id() == Some(tid)
            && !self
                .ui()
                .is_some_and(|ui| ui.get_download_menu_open())
        {
            debug!("tab {tid} started a download: the \u{2B07} panel opens");
            self.download_menu(true);
        }
        self.download_shown(tid);
    }

    /// A [`ironsurf_common::FromTab::DownloadChunk`]: append it to the row's
    /// file (written as it arrives — the shell never buffers a file the size
    /// of an `.iso` in RAM), then count it. A row with no file left (the disk
    /// refused, or a write already failed) drops the chunk: there is nothing
    /// to trust.
    fn download_chunk(&self, tid: TabId, id: u64, bytes: &[u8]) {
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.iter_mut().find(|p| p.tab.id == tid) else {
            return;
        };
        let Some(path) = p.downloads.target(tid, id) else {
            // The disk refused this row, or it settled under an earlier
            // `End`: a late chunk has no file to trust.
            return;
        };
        if let Err(err) = downloads::append(&path, bytes) {
            error!("tab {tid} download #{id}: {err}");
            let _ = p.downloads.finish(tid, id, Some(&err));
            // A half-written file is not a download: take it out of the
            // folder instead of letting a `Failed` row point at it.
            downloads::discard(&path);
            drop(procs);
            self.download_shown(tid);
            return;
        }
        p.downloads.add_bytes(tid, id, bytes.len());
        drop(procs);
        self.download_shown(tid);
    }

    /// A [`ironsurf_common::FromTab::DownloadEnd`]: the row settles — `Done`
    /// with its path, or `Failed` (and a file that never got a byte goes back
    /// out of the folder, because an empty `report.zip` is not a download).
    fn download_end(&self, tid: TabId, id: u64, error: Option<&str>) {
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.iter_mut().find(|p| p.tab.id == tid) else {
            return;
        };
        let Some(finished) = p.downloads.finish(tid, id, error) else {
            warn!("tab {tid} download #{id} ended twice (no row is writing)");
            return;
        };
        drop(procs);
        if let Some(delete) = &finished.delete {
            info!(
                "tab {tid} download #{id} failed with no bytes: {} deleted",
                delete.display()
            );
            downloads::discard(delete);
        } else if let Some(err) = error {
            warn!("tab {tid} download #{id} failed: {err}");
        } else {
            info!(
                "tab {tid} download #{id} done: {} ({} B)",
                finished
                    .row
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                finished.row.bytes
            );
        }
        self.download_shown(tid);
    }

    /// Where a row's news is visible: the panel while it is open, and the
    /// status line whenever the tab is the one on screen.
    fn download_shown(&self, tid: TabId) {
        self.download_refresh_if_open();
        if self.active_tab_id() == Some(tid) {
            self.download_status_refresh();
        }
    }

    /// A tab process died mid-download (crash, or the last tab closed): its
    /// unfinished rows are `Failed`, and their partial files leave the folder
    /// — no orphan `.zip` is left growing where nothing writes any more.
    fn download_reap(&self, tid: TabId) {
        for p in self.procs.borrow_mut().iter_mut() {
            let orphans = p.downloads.reap(tid);
            for path in orphans {
                info!(
                    "tab {} died mid-download: {} deleted",
                    p.tab.id.0,
                    path.display()
                );
                downloads::discard(&path);
            }
        }
        self.download_refresh_if_open();
        self.download_status_refresh();
    }

    // ---- browsing history & reopen closed tab (task: browsing history) ----

    /// Rebuild the settings drawer's one-line history summary from the shared
    /// log. The visits themselves live on their own page
    /// ([`HISTORY_URL`], a tab like any other) — the drawer is settings, and
    /// all it keeps of the log is the count that explains what **Clear
    /// history** is about to delete.
    fn history_state(&self, ui: &AppWindow) {
        let n = ironsurf_storage::History::open(self.store.borrow().root())
            .entries()
            .len();
        ui.set_history_status(
            format!("{n} visit{} shared by every container", if n == 1 { "" } else { "s" })
                .into(),
        );
    }

    /// Answer the `ironsurf://history` page (task: history page). The log is
    /// the shell's — one `<data-root>/history.jsonl` for every container, out
    /// of reach of every sandbox — so the page asks and this replies with the
    /// newest [`HISTORY_PAGE_ROWS`] visits and the size of the whole log.
    /// `clear` (the page's `?clear=1` link) empties the log **first**, so the
    /// answer that follows is the truth after the deletion.
    fn history_list_for(&self, tid: TabId, clear: bool) {
        if clear {
            let history = ironsurf_storage::History::open(self.store.borrow().root());
            match history.clear() {
                Ok(n) => info!("history page: cleared {n} shared history entries"),
                Err(err) => warn!("clearing browsing history failed: {err}"),
            }
        }
        let (visits, total) = self.history_page_rows();
        let _ = self.send_to(tid, ToTab::HistoryList { tab_id: tid, entries: visits, total });
    }

    /// The `ironsurf://history` page's body: the newest [`HISTORY_PAGE_ROWS`] visits of the
    /// shared log, newest first, plus the size of the **whole** log so the page can say what
    /// it left out. Reading that file is this process's job — a tab never sees it.
    fn history_page_rows(&self) -> (Vec<HistoryVisit>, usize) {
        let entries = ironsurf_storage::History::open(self.store.borrow().root()).entries();
        let total = entries.len();
        (
            entries
                .into_iter()
                .take(HISTORY_PAGE_ROWS)
                .map(|e| HistoryVisit { url: e.url, title: e.title, at: e.at })
                .collect(),
            total,
        )
    }

    /// The settings drawer's **History** button: open the browsing-history page
    /// (`ironsurf://history`) in a new tab of the container new tabs open in,
    /// and put the drawer away — the visits are a page now, not a panel.
    fn open_history_page(&self) {
        self.history_menu(false);
        let container = self.new_tab_container();
        info!("history: opening {HISTORY_URL} in a new tab (container {})", container.0);
        self.open_tab(container, Some(HISTORY_URL));
    }

    /// Open/close the History overlay. Opening closes the manager, the
    /// userscript menu, the UA switcher, and the archive menu (two full-window
    /// overlays never stack; each of those four closes this one from its own
    /// open leg). Pinned to the right edge like its neighbours.
    fn history_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("history menu {}", if open { "opened" } else { "closed" });
        if open {
            ui.set_manager_open(false);
            ui.set_us_menu_open(false);
            ui.set_ua_menu_open(false);
            ui.set_archive_menu_open(false);
            ui.set_download_menu_open(false);
            ui.set_save_menu_open(false);
            self.history_state(&ui);
            // The drawer also hosts the proxy section (task: proxies):
            // re-point it at the active tab's container and drop any
            // stale one-line error from a previous visit.
            ui.set_proxy_error(SharedString::default());
            self.proxy_state(&ui);
        }
        ui.set_history_menu_open(open);
    }

    /// The ☰ toolbar button and Ctrl-Alt-H: flip the overlay.
    fn toggle_history_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_history_menu_open());
        self.history_menu(!open);
    }

    /// Clear: delete the **entire shared log** at once (there is no
    /// per-container slice to clear — that is the point of the task), then
    /// re-read so the open menu shows itself empty. The reopen stack is
    /// untouched: those tabs really were open in this session.
    fn history_clear(&self) {
        let history = ironsurf_storage::History::open(self.store.borrow().root());
        match history.clear() {
            Ok(n) => info!("history menu: cleared {n} shared history entries"),
            Err(err) => warn!("clearing browsing history failed: {err}"),
        }
        if let Some(ui) = self.ui() {
            self.history_state(&ui);
        }
        self.refresh_history_pages();
    }

    /// Tell every tab sitting on `ironsurf://history` that the log moved under
    /// it (a Clear from the drawer, a visit from another window), so the page
    /// on screen is never left holding an older list. The tab compares the
    /// answer with what it rendered, so a broadcast that changes nothing costs
    /// a re-render of nothing.
    fn refresh_history_pages(&self) {
        let open: Vec<TabId> = self
            .procs
            .borrow()
            .iter()
            .filter(|p| {
                p.state
                    .url
                    .as_deref()
                    .is_some_and(|u| u.starts_with(HISTORY_URL))
            })
            .map(|p| p.tab.id)
            .collect();
        if open.is_empty() {
            return;
        }
        let (visits, total) = self.history_page_rows();
        for tid in open {
            let _ = self.send_to(
                tid,
                ToTab::HistoryList { tab_id: tid, entries: visits.clone(), total },
            );
        }
    }

    // ---- URL bar autocomplete (task: URL bar autocomplete suggestions) ----

    /// Recompute the address-bar dropdown from the shared visit log: rows
    /// whose URL or title contains what has been typed so far, best match
    /// first — URL prefix, then URL substring, then title — with visiting
    /// recency breaking ties (the log is newest-first and the sort is
    /// stable). Repeats of a URL collapse to their newest visit, and a row
    /// that IS the full address (typed out exactly) drops out — once there
    /// is nothing left to complete, the dropdown closes itself. The draft is
    /// never rewritten behind the user: the text stays as typed until a row
    /// is picked.
    fn suggest_refresh(&self, text: &str) {
        let Some(ui) = self.ui() else { return };
        let query = text.trim().to_lowercase();
        if query.is_empty() {
            self.suggest_close(&ui);
            return;
        }
        let entries = ironsurf_storage::History::open(self.store.borrow().root()).entries();
        let mut seen: Vec<String> = Vec::new();
        let mut rows: Vec<(u8, SuggestItem)> = Vec::new();
        for e in entries.iter() {
            let url_l = e.url.to_lowercase();
            // Newest visit wins — the log is newest-first, so once a URL has
            // been taken its older rows are dropped — and typing an address
            // out exactly leaves nothing to suggest about it.
            if seen.contains(&url_l) || url_l == query {
                continue;
            }
            let title = e
                .title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or(&e.url);
            let rank = if url_l.starts_with(&query) {
                0
            } else if url_l.contains(&query) {
                1
            } else if title.to_lowercase().contains(&query) {
                2
            } else {
                continue;
            };
            seen.push(url_l);
            rows.push((rank, SuggestItem { title: title.into(), url: e.url.as_str().into() }));
        }
        rows.sort_by_key(|(rank, _)| *rank);
        rows.truncate(SUGGEST_ROWS);
        if rows.is_empty() {
            self.suggest_close(&ui);
            return;
        }
        let items: Vec<SuggestItem> = rows.into_iter().map(|(_, row)| row).collect();
        ui.set_suggest_items(ModelRc::from(Rc::new(VecModel::from(items))));
        ui.set_suggest_selected(0);
        ui.set_suggest_open(true);
    }

    /// Move the highlight (task: URL bar autocomplete): ↑/↓ wrap around —
    /// stepping down from "nothing selected yet" lands on the best match,
    /// stepping up from there lands on the last row.
    fn suggest_move(&self, delta: i32) {
        let Some(ui) = self.ui() else { return };
        let n = ui.get_suggest_items().row_count() as i32;
        if n == 0 {
            self.suggest_close(&ui);
            return;
        }
        let cur = ui.get_suggest_selected();
        let next = if delta < 0 { (cur - 1).rem_euclid(n) } else { (cur + 1).rem_euclid(n) };
        ui.set_suggest_selected(next);
    }

    /// Commit a row — Enter on the highlighted row, or a click on any row:
    /// close the dropdown and navigate exactly like typing the address.
    fn suggest_pick(&self, idx: i32) {
        let Some(ui) = self.ui() else { return };
        let row = ui.get_suggest_items().row_data(idx.max(0) as usize);
        match row {
            Some(item) if !item.url.as_str().is_empty() => {
                info!("url autocomplete: picked row {idx} → {}", item.url);
                self.navigate(item.url.as_str());
            }
            // The list was rebuilt under the pick (the log moved between the
            // key press and the callback): do nothing rather than navigate to
            // a row the user was never shown.
            _ => warn!("url autocomplete row {idx} is gone — nothing picked"),
        }
    }

    /// Dismiss the dropdown (Esc, a draft matching nothing, navigation, a tab
    /// switch, a fresh focus of the bar). Idempotent: the writes are guarded
    /// so housekeeping paths may call it freely.
    fn suggest_close(&self, ui: &AppWindow) {
        if !ui.get_suggest_open() && ui.get_suggest_selected() < 0 {
            return;
        }
        ui.set_suggest_open(false);
        ui.set_suggest_selected(-1);
    }

    /// Ctrl-Alt-T: reopen the most recently closed tab (task: browsing
    /// history). The stack remembers URL and container per close; an empty
    /// stack (or every address dropped as blank) is a no-op.
    fn reopen_closed_tab(&self) {
        let Some(record) = self.closed_tabs.borrow_mut().pop() else {
            debug!("reopen closed tab: nothing to reopen");
            return;
        };
        info!(
            "reopen closed tab: {} (container {})",
            record.url,
            record.container.0
        );
        self.open_tab(record.container, Some(&record.url));
    }

    /// The CheckBox: persist **at once**, rebroadcast to every live tab, and
    /// (switching on) stamp a fresh ISO8601 folder onto every live tab
    /// process — turning the mode off and on again starts a new archive run
    /// instead of appending to the old one. Tabs spawned later ride the
    /// setting through `--archive-mode` and take their own spawn-time folder.
    fn archive_set_enabled(&self, on: bool) {
        let Some(ui) = self.ui() else { return };
        self.settings.borrow_mut().archive_enabled = on;
        self.save_settings();
        if on {
            let now = archive::visit_dir_name();
            for p in self.procs.borrow_mut().iter_mut() {
                p.visit_dir = now.clone();
            }
        }
        self.archive_broadcast(on);
        info!(
            "archive mode \u{2192} {} (root {})",
            if on { "on" } else { "off" },
            self.archive_root_text()
        );
        self.archive_state(&ui);
    }

    /// Push the switch to every live, un-crashed tab (global — containers
    /// are irrelevant here). Send failures are ignored: the crash reporter
    /// owns those tabs, exactly like [`Core::us_broadcast`].
    fn archive_broadcast(&self, enabled: bool) {
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.crashed || p.state.closed {
                continue;
            }
            if let Err(err) = p.tab.send(ToTab::SetArchiveMode { enabled }) {
                debug!("archive mode not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    /// The folder LineEdit → Apply: trim, expand a leading `~`, insist on an
    /// absolute path, and require that the tree can actually be created — a
    /// rejected folder rides the one-line `archive-error` **with the draft
    /// kept** and changes nothing (the UA-menu Save leg, same rule).
    fn archive_apply_dir(&self, input: &str) {
        let Some(ui) = self.ui() else { return };
        let text = expand_tilde(input.trim());
        if text.is_empty() {
            ui.set_archive_error("archive folder must not be empty".into());
            return;
        }
        let path = PathBuf::from(&text);
        if !path.is_absolute() {
            ui.set_archive_error(
                format!("archive folder must be an absolute path: {text}").as_str().into(),
            );
            return;
        }
        if let Err(err) = std::fs::create_dir_all(&path) {
            ui.set_archive_error(format!("cannot create {text}: {err}").as_str().into());
            return;
        }
        ui.set_archive_error(SharedString::default());
        *self.archive_dir.borrow_mut() = path;
        self.settings.borrow_mut().archive_dir = Some(text);
        self.save_settings();
        info!("archive root \u{2192} {}", self.archive_root_text());
        // A browsing server that was never aimed at a folder of its own keeps
        // following the archive folder (its own control page, once used, makes
        // the choice the server's).
        if follows_archive_root(self.settings.borrow().archive_server.root.as_deref())
            && self.archive_server.borrow().is_some()
        {
            let spec = self.archive_server_spec();
            if let Err(err) = self.archive_server_apply(spec) {
                warn!("archive server: the archive folder moved, the server did not: {err}");
            }
        }
        self.archive_state(&ui);
    }

    /// Write one archived body under the tab's timestamp folder (task:
    /// archive mode). The tab ships bytes only while the mode is on, but the
    /// setting is re-checked here too: a message already in flight when the
    /// user switched the mode off is dropped, never written to a stale run.
    fn archive_write(&self, tid: TabId, is_document: bool, url: &str, bytes: &[u8]) {
        if !self.settings.borrow().archive_enabled {
            return;
        }
        let Some(rel) = archive::archive_relative_path(url, is_document) else {
            debug!("archive: {url} has no archive path (not a web url?) — dropped");
            return;
        };
        let visit = {
            let procs = self.procs.borrow();
            procs.iter().find(|p| p.tab.id == tid).map(|p| p.visit_dir.clone())
        };
        let Some(visit) = visit else { return }; // the tab died mid-flight
        let root = self.archive_dir.borrow().clone();
        match archive::write_archived(&root, &visit, &rel, bytes) {
            Ok(()) => {
                let n = self.archive_saved.get() + 1;
                self.archive_saved.set(n);
                debug!("archive: wrote {} ({n} file(s) this session)", rel.display());
                self.archive_refresh_if_open();
            }
            Err(err) => warn!(
                "archive: cannot write {} under {}: {err}",
                rel.display(),
                root.display()
            ),
        }
    }

    // ---- archive browsing server (`ironsurf://archive`) --------------------------

    /// What the server should serve, from the persisted settings: the folder
    /// the archive server was pointed at, else the effective archive root; the
    /// host it binds (default `127.0.0.1`); the port (default a random one).
    fn archive_server_spec(&self) -> Spec {
        let (saved_root, saved_host, saved_port) = {
            let settings = self.settings.borrow();
            (
                settings.archive_server.root.clone(),
                settings.archive_server.host.clone(),
                settings.archive_server.port,
            )
        };
        let fallback = self.archive_dir.borrow().clone();
        archive_server::spec_from(saved_root.as_deref(), fallback, saved_host, saved_port)
    }

    /// Bring the archive server up (idempotent). A failed start is returned as
    /// the reason, so `ironsurf://archive` can say it out loud instead of
    /// showing a page that pretends to have a server.
    fn ensure_archive_server(&self) -> Result<(), String> {
        if self.archive_server.borrow().is_some() {
            return Ok(());
        }
        if self.archive_server_rx.borrow().is_none() {
            let (tx, rx) = channel();
            *self.archive_server_rx.borrow_mut() = Some(rx);
            // Keep the sender alive: it is the one event path the server has.
            *self.archive_server_sink.borrow_mut() = Some(tx);
        }
        let sink = self
            .archive_server_sink
            .borrow()
            .clone()
            .ok_or_else(|| "the archive server event queue is gone".to_string())?;
        let handle = ServeHandle::start(self.archive_server_spec(), sink)?;
        *self.archive_server_broadcast.borrow_mut() = Some(handle.address.clone());
        *self.archive_server.borrow_mut() = Some(handle);
        Ok(())
    }

    /// Re-point the running server (used when the archive root changed, or
    /// when a request carries an override). Never starts one: callers that
    /// want that call [`Self::ensure_archive_server`] first.
    fn archive_server_apply(&self, spec: Spec) -> Result<(), String> {
        match self.archive_server.borrow().as_ref() {
            Some(handle) => handle.set_spec(spec).map_err(|e| format!("archive server: {e}")),
            None => Err("the archive server is not running".to_string()),
        }
    }

    /// Serve the archive for one `ironsurf://archive` request: bring the
    /// server up (honouring any override the URL carried), then answer the
    /// requesting tab — and open the archive listing in a new tab when the
    /// page asked for it (`ironsurf://archive?open=1`).
    fn start_archive_server(
        &self,
        tid: TabId,
        root: Option<&str>,
        host: Option<&str>,
        port: Option<u16>,
        open_in_new_tab: bool,
    ) {
        // An override on the URL wins over the saved settings for this one
        // request; the server keeps serving it afterwards, which is what the
        // page's form says it does.
        let overrides = root.is_some() || host.is_some() || port.is_some();
        if let Err(err) = self.ensure_archive_server() {
            warn!("archive server: {err}");
            let _ = self.send_to(
                tid,
                ToTab::ArchiveServerInfo {
                    tab_id: tid,
                    ok: false,
                    address: String::new(),
                    error: Some(err),
                },
            );
            return;
        }
        if overrides {
            let base = self.archive_server_spec();
            let spec = archive_server::spec_from(
                root.or(base.root.to_str()),
                base.root.clone(),
                host.map(str::to_owned).or(Some(base.host.clone())),
                port.or(Some(base.port)),
            );
            if let Err(err) = self.archive_server_apply(spec) {
                warn!("archive server: {err}");
            }
        }
        // The server announces itself on the event queue; until then the
        // handle knows the address it bound, which is what the page shows.
        let address = self
            .archive_server
            .borrow()
            .as_ref()
            .and_then(|h| h.address.clone());
        match address {
            Some(address) => {
                if open_in_new_tab {
                    let container = {
                        let procs = self.procs.borrow();
                        procs
                            .iter()
                            .find(|p| p.tab.id == tid)
                            .map(|p| p.tab.container)
                    };
                    if let Some(container) = container {
                        self.open_tab(container, Some(&address));
                    }
                }
                let _ = self.send_to(
                    tid,
                    ToTab::ArchiveServerInfo {
                        tab_id: tid,
                        ok: true,
                        address: address.clone(),
                        error: None,
                    },
                );
                info!("archive server: answering tab {tid} with {address}");
            }
            None => {
                let err = "the archive folder is bound, but no address came back".to_string();
                let _ = self.send_to(
                    tid,
                    ToTab::ArchiveServerInfo {
                        tab_id: tid,
                        ok: false,
                        address: String::new(),
                        error: Some(err.clone()),
                    },
                );
                warn!("archive server: {err}");
            }
        }
    }

    /// Tell every live tab what the archive server is answering at (or that it
    /// is not). Only pushes a change: the address is what the summary page
    /// shows, and nobody wants a repaint they did not ask for.
    fn archive_server_announce(&self, ok: bool, address: Option<&str>, error: Option<String>) {
        let shown = address.map(str::to_owned);
        let ids: Vec<TabId> = self.procs.borrow().iter().map(|p| p.tab.id).collect();
        for tid in ids.clone() {
            let _ = self.send_to(
                tid,
                ToTab::ArchiveServerInfo {
                    tab_id: tid,
                    ok,
                    address: shown.clone().unwrap_or_default(),
                    error: error.clone(),
                },
            );
        }
        debug!(
            "archive server: announced {} of {} tab(s)",
            match &shown {
                Some(a) => format!("{a} to"),
                None => "shutdown to".to_string(),
            },
            ids.len()
        );
    }

    /// Drain what the archive server reported since the last tick: applied
    /// folder/host/port go into the settings (and out to disk), address
    /// changes go out to the tabs.
    fn drain_archive_server(&self) {
        let events: Vec<ServeEvent> = {
            let mut slot = self.archive_server_rx.borrow_mut();
            match slot.as_mut() {
                Some(rx) => rx.try_iter().collect(),
                None => return,
            }
        };
        for event in events {
            match event {
                ServeEvent::Spec { spec } => {
                    let mut settings = self.settings.borrow_mut();
                    let saved = &mut settings.archive_server;
                    saved.root = Some(spec.root.display().to_string());
                    saved.host = Some(spec.host.clone());
                    saved.port = Some(spec.port);
                    drop(settings);
                    self.save_settings();
                    info!(
                        "archive server: serving {} on {}:{}",
                        spec.root.display(),
                        spec.host,
                        spec.port
                    );
                }
                ServeEvent::Address { address } => {
                    let mut last = self.archive_server_broadcast.borrow_mut();
                    if last.as_ref() == Some(&address) {
                        continue;
                    }
                    *last = Some(address.clone());
                    drop(last);
                    match &address {
                        Some(addr) => self.archive_server_announce(true, Some(addr), None),
                        None => self.archive_server_announce(
                            false,
                            None,
                            Some("serving stopped from the control page".to_string()),
                        ),
                    }
                }
                ServeEvent::Failed { error } => warn!("archive server: {error}"),
                ServeEvent::Gone => debug!("archive server: a retired listener has ended"),
                ServeEvent::Closed => {
                    info!("archive server: the control socket has closed");
                    *self.archive_server.borrow_mut() = None;
                }
            }
        }
    }

    // ---- persistence -------------------------------------------------------------

    fn save_settings(&self) {
        let settings = self.settings.borrow().clone();
        let _ = settings.save(&self.ui_path); // best-effort
    }

    /// The settings menu's scroll-speed slider (the ☰ panel): a browser-wide
    /// wheel-travel multiplier, `1.0` = the default damping. Clamped to the
    /// slider's own range, persisted to ui.json at once, and pushed to every
    /// running tab — fresh ones are seeded at spawn, so the choice is global
    /// and outlives the session.
    fn set_scroll_speed(&self, speed: f32) {
        let speed = if speed.is_finite() {
            speed.clamp(0.2, 3.0)
        } else {
            1.0
        };
        info!("settings: scroll speed → {:.0}%", speed * 100.0);
        self.settings.borrow_mut().scroll_speed = Some(speed);
        self.save_settings();
        for p in self.procs.borrow().iter() {
            if let Err(err) = p.tab.send(ToTab::SetScrollSpeed { speed }) {
                warn!("tab {} could not set scroll speed: {err}", p.tab.id);
            }
        }
        if let Some(ui) = self.ui() {
            ui.set_scroll_speed(speed);
        }
    }

    // ---- message pump --------------------------------------------------------------

    /// Drain every queued `FromTab` message: reduce per-tab state, keep only
    /// the newest frame for the active tab, reap closed tabs, sync the UI.
    pub fn drain(&self) {
        // The archive server is not a tab: its reports are drained here so the
        // settings mirror and the summary pages stay honest even on a tick
        // with no tab traffic at all.
        self.drain_archive_server();
        // Superseded hover moves go out here, one per tick at the latest.
        self.flush_pending_move();
        // A child can die between messages (panic, kill): without a poll the
        // tab would go silent and the shell would keep painting its stale
        // frame as if nothing happened.
        self.check_crashes();
        let msgs: Vec<PumpedMessage> = self.rx.borrow_mut().try_iter().collect();
        if msgs.is_empty() {
            self.check_resize();
            // A page area that shrank under a dragged panel is the one thing a
            // tab with nothing to say still has to be told about.
            self.sync_devtools_height();
            return;
        }
        let active = self.active_tab_id();
        let mut dirty = false;
        // Console/network messages arrived this tick: `reduce` reports `false`
        // for them on purpose (they must never dirty the frame), so the open
        // panel needs its own nudge below.
        let mut devtools_feed = false;
        let mut newest_frame: Option<Image> = None;

        for (tid, msg) in msgs {
            // Diagnostics first: the message we drop below is the one thing
            // that must never be formatted (up to ~4 MB per frame).
            match &msg {
                FromTab::Frame { width, height, scale, rgba } => trace!(
                    "tab {tid} frame {}x{}@{scale:.2} ({} B)",
                    width,
                    height,
                    rgba.len()
                ),
                FromTab::Error { message } => error!("tab {tid} error: {message}"),
                FromTab::ConsoleLog { source, level, message } => {
                    devtools_feed = true;
                    info!("tab {tid} console[{source}:{level}]: {message}")
                }
                // DevTools network panel: the row data is the reduced per-tab
                // log (`TabState::network`); nothing to log per message.
                FromTab::NetworkEvent { .. } => devtools_feed = true,
                // DevTools **Inspector**: the tree is the record's own view-model
                // (handled below); the line says what arrived without naming the
                // markup, which is the one thing here worth not formatting.
                FromTab::InspectSnapshot { snapshot } => {
                    devtools_feed = true;
                    info!(
                        "tab {tid} sent its document: {} node(s), gen {}, selected {:?}{}",
                        snapshot.nodes.len(),
                        snapshot.generation,
                        snapshot.selected,
                        if snapshot.truncated { ", truncated" } else { "" }
                    )
                }
                // Every move would be a message: the pick is the interesting one.
                FromTab::InspectHover { node } => {
                    trace!("tab {tid} inspector hover: {node:?}")
                }
                FromTab::InspectPicked { node } => {
                    devtools_feed = true;
                    info!("tab {tid} ⌖ picked {node:?}")
                }
                // Archive mode: the bytes themselves go to disk below — the
                // log line stays tiny (never format a multi-MB body).
                FromTab::ArchivedFetch { url, is_document, bytes } => debug!(
                    "tab {tid} archived {}{url} ({} B)",
                    if *is_document { "document " } else { "subresource " },
                    bytes.len()
                ),
                // A download is a stream, so only its ends are worth a line;
                // the chunk count belongs to the panel, not the log (task:
                // file downloads).
                FromTab::DownloadStart { hint } => debug!(
                    "tab {tid} download #{} starts: {:?} ({:?})",
                    hint.id,
                    hint.name,
                    hint.trigger
                ),
                FromTab::DownloadChunk { id, bytes } => {
                    trace!("tab {tid} download #{id} ships {} B", bytes.len())
                }
                FromTab::DownloadEnd { id, error } => debug!(
                    "tab {tid} download #{id} ends ({})",
                    error.as_deref().unwrap_or("ok")
                ),
                FromTab::UrlChanged { url } => {
                    info!("tab {tid} url → {}", url.as_deref().unwrap_or("<none>"))
                }
                FromTab::TitleChanged { title } => {
                    info!("tab {tid} title → {}", title.as_deref().unwrap_or("<none>"))
                }
                FromTab::LoadingChanged { loading } => info!(
                    "tab {tid} loading {}",
                    if *loading { "started" } else { "finished" }
                ),
                FromTab::Closed => info!("tab {tid} says it is closed"),
                FromTab::OpenLinkInNewTab { url } => {
                    info!("tab {tid} middle-click: opening {url} in a new tab")
                }
                FromTab::ContextMenu { x, y, target } => {
                    info!("tab {tid} right-click at ({x:.0},{y:.0}): {target:?}")
                }
                FromTab::DialogRequest { dialog_id, kind, message, .. } => {
                    info!("tab {tid} opened a {kind:?} dialog #{dialog_id}: {message:?}")
                }
                // Archive browsing server: the summary page asked for the
                // server; the reply goes out below (the log line only says so).
                FromTab::ArchiveServerStart { root, host, port, open_in_new_tab } => {
                    info!(
                        "tab {tid} asked for the archive server (root {root:?}, host {host:?}, \
                         port {port:?}, new tab: {open_in_new_tab})"
                    )
                }
                FromTab::ArchiveServerSettingsChanged { root, host, port } => {
                    info!("tab {tid} reports the archive server moved to {host}:{port} ({root})")
                }
                // `ironsurf://history`: the page asked for the visit log (with
                // `clear` when the row it clicked was "Clear history"); the
                // reply goes out below.
                FromTab::HistoryListRequest { clear } => {
                    info!("tab {tid} asked for the browsing history (clear: {clear})")
                }
                FromTab::ClipboardText { text } => {
                    // Page copy (Ctrl+C/X, editor cut): the shell owns the
                    // session clipboard (task: context menu).
                    self.clipboard_set(text);
                }
                other => debug!("tab {tid} → {other:?}"),
            }
            if let FromTab::Frame { width, height, ref rgba, .. } = msg {
                if Some(tid) == active {
                    if let Some(img) = image_from_rgba(width, height, rgba) {
                        newest_frame = Some(img);
                    }
                }
                continue;
            }
            // Scroll geometry (task: scroll bar): a document whose *offset*
            // moved re-arms the clock the overlay bar shows on, and the bar is
            // drawn on the spot — a reader who just scrolled must not wait a
            // spinner tick to see where they ended up. A report that only
            // restates the range (a resize, a document that opens at the top)
            // moves nothing the bar cares about, and costs no stamp. A
            // background tab's scroll keeps its own clock and draws nothing.
            let changed = {
                let mut procs = self.procs.borrow_mut();
                match procs.iter_mut().find(|p| p.tab.id == tid) {
                    Some(p) => {
                        // What the report moves is read before the record takes
                        // the new numbers, which is what makes it a movement.
                        let moved = match &msg {
                            FromTab::ScrollState { state } => p.state.scroll_moves(state),
                            _ => false,
                        };
                        let changed = p.state.reduce(&msg);
                        if matches!(msg, FromTab::ScrollState { .. }) {
                            if moved {
                                p.scroll_moved = Some(Instant::now());
                            }
                            if Some(tid) == active {
                                if let Some(window) = self.ui() {
                                    push_scrollbar(&window, &p.state.scroll, p.scroll_moved);
                                } // the frame's own record keeps the numbers meanwhile
                            }
                        }
                        changed
                    }
                    None => false,
                }
            };
            dirty |= changed;
            // Browsing history (task: browsing history): one line per
            // navigation, written to the log **shared by every container**
            // (`<root>/history.jsonl` — never per profile), best effort.
            if let FromTab::UrlChanged { url: Some(url) } = &msg {
                // Blank tabs and IronSurf's own pages are plumbing, not visits.
                if !worth_recording(url) {
                    continue;
                }
                // Private browsing (task: private browsing): a tab of a
                // private container leaves **no** trace in the shared log —
                // the navigation happens, nothing is written, and no open
                // history view is refreshed.
                if self.tab_is_private(tid) {
                    debug!("history: {url} not recorded (private container)");
                    continue;
                }
                // The title is deliberately **not** read from the tab here: at
                // the moment the address changes the new document has not been
                // parsed, so the tab's title is still the *previous* page's —
                // which is how half the log ended up titled "Welcome to
                // IronSurf". The visit is written untitled (the page and the
                // completions fall back to the URL) and `TitleChanged` below
                // stamps the real one as soon as it exists.
                let history = ironsurf_storage::History::open(self.store.borrow().root());
                if let Err(err) = history.record(url, None) {
                    warn!("history entry for {url} not written: {err}");
                } else {
                    debug!("history: recorded {url} (shared log, title to follow)");
                }
                // Keep the drawer's count and any open history page current.
                if let Some(ui) = self.ui() {
                    if ui.get_history_menu_open() {
                        self.history_state(&ui);
                    }
                }
                self.refresh_history_pages();
            }
            // The page's own `<title>` landed (or a script renamed the document):
            // stamp it on the visit that is being read right now. Blank means
            // the page has no title, and an entry must not keep the one that
            // leaked in from the page before it.
            if let FromTab::TitleChanged { title } = &msg {
                let url = {
                    let procs = self.procs.borrow();
                    let proc = procs.iter().find(|p| p.tab.id == tid);
                    // Private browsing (task): the visit was never written
                    // for a private container, so there is nothing to title
                    // there either.
                    let private = proc
                        .is_some_and(|p| container_private(&self.store.borrow(), p.tab.container));
                    proc.and_then(|p| p.state.url.clone())
                        .filter(|u| worth_recording(u))
                        .filter(|_| !private)
                };
                let Some(url) = url else { continue };
                let history = ironsurf_storage::History::open(self.store.borrow().root());
                let clean = title.as_deref().map(str::trim).filter(|t| !t.is_empty());
                match history.set_latest_title(&url, clean) {
                    Ok(true) => debug!("history: {url} titled {clean:?}"),
                    Ok(false) => {}
                    Err(err) => warn!("titling the history entry for {url} failed: {err}"),
                }
                self.refresh_history_pages();
            }
            // Middle-click "open link in new tab": spawn a sibling tab in the
            // requesting tab's container; the requester itself never navigates
            // (its runtime swallows the middle-click release).
            if let FromTab::OpenLinkInNewTab { url } = &msg {
                let container = {
                    let procs = self.procs.borrow();
                    procs
                        .iter()
                        .find(|p| p.tab.id == tid)
                        .map(|p| p.tab.container)
                };
                if let Some(container) = container {
                    self.open_tab(container, Some(url));
                }
            }
            // `ironsurf://archive`: the tab cannot bind a socket or read the
            // archive tree (it is sandboxed), so the server lives here. Start
            // it on demand and answer the asking tab with its address; when
            // the page asked for `?open=1`, the archive listing itself opens
            // beside it in a new tab.
            if let FromTab::ArchiveServerStart { root, host, port, open_in_new_tab } = &msg {
                self.start_archive_server(tid, root.as_deref(), host.as_deref(), *port, *open_in_new_tab);
            }
            // `ironsurf://history`: the visit log lives here, so the page reads
            // it through this process.
            if let FromTab::HistoryListRequest { clear } = &msg {
                self.history_list_for(tid, *clear);
            }
            // The server's own control page applied a new folder/host/port:
            // remember it (the server reports the change itself; this is the
            // tab-relayed path, kept so a stale setting can never win).
            if let FromTab::ArchiveServerSettingsChanged { root, host, port } = &msg {
                let mut settings = self.settings.borrow_mut();
                settings.archive_server.root = Some(root.clone());
                settings.archive_server.host = Some(host.clone());
                settings.archive_server.port = Some(*port);
                drop(settings);
                self.save_settings();
            }
            // Right-click → the context menu opens over the page at the
            // click point (task: context menu).
            if let FromTab::ContextMenu { x, y, target } = &msg {
                self.open_context_menu(*x, *y, target);
            }
            // The Inspector's document: filed on *this* tab's record (ids are
            // meaningless in another tab's tree), then pushed if we are looking
            // at that tab.
            if let FromTab::InspectSnapshot { snapshot } = &msg {
                self.on_inspect_snapshot(tid, snapshot.clone());
            }
            // ⌖ answered: the mode ends and the pane selects what was clicked.
            if let FromTab::InspectPicked { node } = &msg {
                self.on_inspect_picked(tid, *node);
            }
            // `alert`/`confirm`/`prompt` → the tab is now blocked on the
            // pipe; the overlay opens (on the requesting tab, brought to
            // the front) and the answer goes back via OK/Cancel.
            if let FromTab::DialogRequest { dialog_id, kind, message, default } = &msg {
                self.open_dialog(tid, *dialog_id, *kind, message, default.as_deref());
            }
            // Hover cursor (task: cursor icon): the page says what shape the
            // pointer is over it. Only the active tab ever sees pointer
            // events, but a background message must not repaint the wrong
            // page — the per-tab record (state.reduce) still keeps it.
            if let FromTab::CursorChanged { cursor } = &msg {
                if Some(tid) == active {
                    self.apply_page_cursor(*cursor);
                }
            }
            // Archive mode: the tab's shipped body lands under the tab's
            // timestamp folder (task: archive mode). Writing happens in the
            // pump — the tab process stays inside its sandbox.
            if let FromTab::ArchivedFetch { url, is_document, bytes } = &msg {
                self.archive_write(tid, *is_document, url, bytes);
            }
            // A download lands on disk from the pump — the tab that fetched it
            // never opens a file (task: file downloads).
            match &msg {
                FromTab::DownloadStart { hint } => self.download_write(tid, hint),
                FromTab::DownloadChunk { id, bytes } => self.download_chunk(tid, *id, bytes),
                FromTab::DownloadEnd { id, error } => {
                    self.download_end(tid, *id, error.as_deref())
                }
                _ => {}
            }
        }

        // A tab that closed cleanly while a download was still writing leaves
        // its partial file out of the folder before its rows go away (task:
        // file downloads).
        let orphans: Vec<PathBuf> = {
            let mut paths = Vec::new();
            for p in self.procs.borrow_mut().iter_mut().filter(|p| p.state.closed) {
                paths.extend(p.downloads.reap(p.tab.id));
            }
            paths
        };
        // Reap closed tabs (dropping a TabProc closes stdin and reaps it).
        let (before, after) = {
            let mut procs = self.procs.borrow_mut();
            let before = procs.len();
            procs.retain(|p| !p.state.closed);
            let after = procs.len();
            if after == 0 {
                drop(procs);
                let _ = slint::quit_event_loop();
            } else {
                self.active.set(self.active.get().min(after - 1));
            }
            (before, after)
        };
        if before != after {
            // A tab that said `Closed` while its modal was queued or shown
            // takes the overlay with it (the request ids die with the
            // process); the queue rolls to the next live dialog.
            let queued = self
                .pending_dialogs
                .borrow()
                .iter()
                .map(|p| p.tab)
                .collect::<Vec<_>>();
            for tab in queued {
                if !self.procs.borrow().iter().any(|p| p.tab.id == tab) {
                    self.clear_dialog_for(tab, false);
                }
            }
        }
        for path in &orphans {
            info!("a tab closed mid-download: {} deleted", path.display());
            downloads::discard(path);
        }
        if before != after {
            info!("reaped {} closed tab(s); {} remain", before - after, after);
            dirty = true;
        }

        if dirty {
            self.sync_ui();
        } else if devtools_feed {
            // Panel open or not, keep the table current: pushing into a panel
            // nobody looks at is cheap, and it means the pane is complete the
            // instant F12 opens it.
            let procs = self.procs.borrow();
            if let Some(ui) = self.ui() {
                self.write_devtools(&ui, &procs);
            }
        }
        if let (Some(img), Some(ui)) = (newest_frame, self.ui()) {
            ui.set_page_image(img);
            ui.window().request_redraw();
        }
    }

    /// Notice tab processes that died without saying `Closed` (crash, kill):
    /// log the exit + captured backtrace and pin “Tab N has crashed” onto each
    /// victim's status line. Crashed tabs are kept in the strip (their frame
    /// is the last thing the user asked for); ✕ still closes them.
    fn check_crashes(&self) {
        let mut crashed = false;
        let mut victims = Vec::new();
        {
            let mut procs = self.procs.borrow_mut();
            for p in procs.iter_mut() {
                if p.crashed || p.state.closed {
                    continue;
                }
                if let Some(status) = p.tab.try_wait_status() {
                    note_tab_crash(p, format_args!("the process exited ({status}) without closing cleanly"));
                    victims.push(p.tab.id);
                    crashed = true;
                }
            }
        }
        // A crashed tab that was blocked on a modal dialog takes the overlay
        // down with it — the answer cannot reach a dead process (task: JS
        // dialogs).
        for tid in victims {
            // Its unfinished rows are `Failed` and their partial files leave
            // the folder (task: file downloads).
            self.download_reap(tid);
            self.clear_dialog_for(tid, false);
        }
        if crashed {
            self.sync_ui();
        }
    }

    /// Advance the status-bar spinner **and** the loading bar one tick. The
    /// spinner timer calls this every 120 ms whether or not anything is
    /// loading; everything below is a no-op (`" "`, hidden bar, step 0) while
    /// the active tab is idle.
    pub fn tick_spinner(&self) {
        let Some(ui) = self.ui() else { return };
        let loading = self
            .active_proc_index()
            .map(|i| self.procs.borrow()[i].state.loading)
            .unwrap_or(false);
        // Loading bar: the step only advances while a load is in flight, and
        // starts over the moment one begins — so every navigation sweeps from
        // the track's left end, wherever a previous (or background tab's)
        // load had got to.
        let step = if loading {
            if self.prev_loading.get() {
                (self.load_step.get() + 1) % (LOAD_SWEEP_STEPS * 2)
            } else {
                0
            }
        } else {
            0
        };
        self.prev_loading.set(loading);
        self.load_step.set(step);
        if ui.get_loading() != loading {
            ui.set_loading(loading);
        }
        let progress = load_sweep_position(step) as i32;
        if ui.get_load_progress() != progress {
            ui.set_load_progress(progress);
        }
        let want: SharedString = if loading {
            let i = (self.spinner.get() + 1) % SPINNER_FRAMES.len();
            self.spinner.set(i);
            SPINNER_FRAMES[i].to_string().into()
        } else {
            " ".into()
        };
        if ui.get_spinner() != want {
            ui.set_spinner(want);
        }
        // The page's overlay scroll bar rides this same heartbeat (task: scroll
        // bar): it fades out here, `SCROLLBAR_HOLD` after its document last
        // moved, which is why nothing needs a timer of its own for the hiding.
        // It is the tab on screen's bar and nobody else's — switching tabs
        // switches to that page's own movement clock.
        let active = self.active_proc_index();
        let procs = self.procs.borrow();
        match active.and_then(|i| procs.get(i)) {
            Some(p) => push_scrollbar(&ui, &p.state.scroll, p.scroll_moved),
            None => push_scrollbar(&ui, &ScrollState::default(), None),
        }
    }

    /// Push a full snapshot of the model into the window properties.
    pub fn sync_ui(&self) {
        let Some(ui) = self.ui() else { return };
        let procs = self.procs.borrow();
        let store = self.store.borrow();
        let active = self.active.get().min(procs.len().saturating_sub(1));

        let tabs: Vec<TabInfo> = procs
            .iter()
            .enumerate()
            .map(|(i, p)| TabInfo {
                id: p.tab.id.0 as i32,
                title: p.state.title.clone().into(),
                color: color_hex(
                    store
                        .container(p.tab.container)
                        .map(|c| c.color.as_str())
                        .unwrap_or("#9e9e9e"),
                ),
                active: i == active,
                // Which Alt-digit reaches this tab today (empty when none
                // does), recomputed on every snapshot so the strip always
                // shows the live mapping.
                shortcut: keymap::tab_shortcut_digit(i, procs.len())
                    .map(|d| format!("{d}").into())
                    .unwrap_or_default(),
            })
            .collect();
        ui.set_tabs(ModelRc::from(Rc::new(VecModel::from(tabs))));

        let conts: Vec<ContainerInfo> = store
            .containers()
            .iter()
            .map(|c| ContainerInfo {
                name: c.name.clone().into(),
                color: color_hex(&c.color),
                isDefault: c.is_default,
                // the chosen definition name ("" = direct) — the manager
                // rows say which container rides which proxy (task: proxies)
                proxy: c.proxy.clone().filter(|n| !n.trim().is_empty()).unwrap_or_default().into(),
                // the manager's Private checkbox mirrors the stored flag
                // (task: private browsing)
                private: c.private,
            })
            .collect();
        ui.set_containers(ModelRc::from(Rc::new(VecModel::from(conts))));
        // The settings menu's slider reads the persisted speed (task: settings
        // menu) — the panel opens wherever the last session left it.
        ui.set_scroll_speed(self.settings.borrow().scroll_speed.unwrap_or(1.0));

        if let Some(p) = procs.get(active) {
            ui.set_can_back(p.state.can_back);
            ui.set_can_fwd(p.state.can_forward);
            ui.set_reader_on(p.state.reader);
            // The popup blocker comes back with the tab too: one switch per
            // process, so a site allowed here says nothing about there — and it
            // says which of the two it is, in the words shown on hover.
            ui.set_popup_blocked(!p.state.popup_block_off);
            ui.set_popup_tip(p.state.popup_tip().into());
            // An address being typed belongs to exactly one tab: switching
            // tabs (or that tab going away) drops the draft, so the strip can
            // never leave a stale address in the bar. While it is live the
            // tab's own URL is *not* pushed — a page still loading, or a
            // redirect it is following, must not overwrite the half-typed
            // address the new tab just highlighted.
            if self.url_draft.get().is_some_and(|t| t != p.tab.id) {
                self.url_draft.set(None);
            }
            if self.url_draft.get() != Some(p.tab.id) {
                ui.set_url_text(p.state.url.clone().unwrap_or_default().into());
            }
            // The tab's hover cursor comes back with the tab (task: cursor
            // icon) — switching tabs must not leave the previous page's hand
            // (or I-beam) drawn over this one.
            ui.set_page_cursor(slint_cursor(p.state.cursor));
            // The status line is *one* message, always the newest: an error is
            // dropped when the next load starts (TabState::reduce), a load
            // with nothing reported yet still says so instead of going blank.
            let status = self.status_text(p);
            ui.set_status_text(status.into());
            let (glyph, color) = p.state.security().render();
            ui.set_sec_glyph(glyph.into());
            ui.set_sec_color(color_hex(color));
            if let Some(c) = store.container(p.tab.container) {
                ui.set_container_label(c.name.clone().into());
                // Private browsing (task): the top-bar badge lights up for
                // the container of the tab **on screen** — per container,
                // not global.
                ui.set_container_private(c.private);
            }
        } else {
            let status = self.spawn_error.borrow().clone().unwrap_or_default();
            ui.set_status_text(status.into());
            // No tab on screen, no private container to claim (task: private
            // browsing) — the badge falls dark with the label.
            ui.set_container_private(false);
            // No tab, nothing to claim about its popups: the ⛔ stays on screen
            // but falls silent rather than repeating the last tab's answer
            // (task: popup blocker tooltip).
            ui.set_popup_tip("".into());
        }
        ui.set_active_index(active as i32);
        // The DevTools panel renders the active tab too (pane, counts, rows):
        // toggling, switching tabs and closing it all land here.
        self.write_devtools(&ui, &procs);
        // …and so does the find bar (open flag + the tab's own counter).
        self.write_find(&ui, &procs);
        // …and so does the ⬇ panel: its rows belong to the tab on screen, so
        // switching tabs switches the download list with it (task: file
        // downloads).
        if ui.get_download_menu_open() {
            self.download_state(&ui);
        }
        // The spinner must react to LoadingChanged immediately, not on the next
        // tick of the spinner timer — a click on a link should start turning
        // the moment the tab says it is loading.
        self.tick_spinner();
    }

    // ---- wiring --------------------------------------------------------------------

    fn attach_callbacks(self: &Rc<Self>, ui: &AppWindow) {
        macro_rules! bind0 {
            ($setter:ident, $body:expr) => {{
                let c = self.clone();
                ui.$setter(move || $body(&c));
            }};
        }
        bind0!(on_go_back, |c: &Rc<Core>| c.send_active(ToTab::Back));
        bind0!(on_go_forward, |c: &Rc<Core>| c.send_active(ToTab::Forward));
        bind0!(on_reload, |c: &Rc<Core>| c.send_active(ToTab::Reload));
        bind0!(on_reader_toggled, |c: &Rc<Core>| c.toggle_reader());
        bind0!(on_popup_block_toggled, |c: &Rc<Core>| {
            // The button states the tab's switch, so the command is its flip.
            let blocking = c
                .active_proc_index()
                .is_some_and(|i| !c.procs.borrow()[i].state.popup_block_off);
            c.set_popup_block(!blocking);
        });
        bind0!(on_popup_allow_site, |c: &Rc<Core>| c.allow_popup_site());
        bind0!(on_new_tab, |c: &Rc<Core>| {
            let cid = c.new_tab_container();
            c.open_tab(cid, Some(&default_url()));
        });
        bind0!(on_find_next, |c: &Rc<Core>| c.find_jump(true));
        bind0!(on_find_previous, |c: &Rc<Core>| c.find_jump(false));
        bind0!(on_find_close, |c: &Rc<Core>| c.close_find());
        {
            let c = self.clone();
            ui.on_find_query_changed(move |q| c.find_text_changed(q.to_string()));
        }
        bind0!(on_open_manager, |c: &Rc<Core>| c.manager_open(true));
        bind0!(on_close_manager, |c: &Rc<Core>| c.manager_open(false));
        bind0!(on_mgr_add, |c: &Rc<Core>| c.mgr_add());

        {
            let c = self.clone();
            ui.on_navigate(move |url| c.navigate(url.as_str()));
        }
        {
            let c = self.clone();
            ui.on_activate_tab(move |idx| {
                if idx >= 0 {
                    c.activate_tab(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_close_tab(move |idx| {
                if idx >= 0 {
                    c.close_tab(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_new_tab_in(move |idx| {
                if idx >= 0 {
                    c.mgr_new_tab_in(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_move_tab_to(move |idx| {
                if idx >= 0 {
                    c.mgr_move_tab_to(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_delete(move |idx| {
                if idx >= 0 {
                    c.mgr_delete(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_start_rename(move |idx| {
                if idx >= 0 {
                    c.mgr_start_rename(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_rename(move |idx| {
                if idx >= 0 {
                    c.mgr_rename(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_recolor(move |idx, pal| {
                if idx >= 0 && pal >= 0 {
                    c.mgr_recolor(idx as usize, pal as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_select(move |idx| c.mgr_select(idx));
        }
        {
            let c = self.clone();
            ui.on_mgr_toggle_private(move |idx| {
                if idx >= 0 {
                    c.mgr_toggle_private(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_page_key(move |pressed, text, ctrl, shift, alt, meta, repeat| {
                c.page_key(pressed, text.as_str(), ctrl, shift, alt, meta, repeat)
            });
        }
        {
            let c = self.clone();
            ui.on_page_pointer(move |x, y, kind, button, ctrl, shift, alt, meta| {
                c.page_pointer(x, y, kind, button, ctrl, shift, alt, meta)
            });
        }
        {
            let c = self.clone();
            ui.on_page_wheel(move |x, y, dx, dy| c.page_wheel(x, y, dx, dy));
        }
        {
            // Task: keep keyboard shortcuts usable while the URL bar is
            // focused — the entry offers every chord it could swallow, and
            // Rust answers whether the browser claimed it.
            let c = self.clone();
            ui.on_url_chord(move |text, ctrl, shift, alt, meta| {
                let _ = shift;
                c.bar_chord(text.as_str(), ctrl, alt, meta)
            });
        }
        {
            let c = self.clone();
            ui.on_find_chord(move |text, ctrl, shift, alt, meta| {
                let _ = shift;
                c.bar_chord(text.as_str(), ctrl, alt, meta)
            });
        }
        {
            let c = self.clone();
            ui.on_menu_pick(move |idx| {
                if idx >= 0 {
                    c.menu_pick(idx as usize);
                }
            });
        }
        bind0!(on_menu_close, |c: &Rc<Core>| c.close_context_menu());

        // JS dialogs (task: window.alert/confirm/prompt): the two buttons of
        // the overlay — and the prompt box's Enter — answer the blocked tab.
        bind0!(on_dialog_accept, |c: &Rc<Core>| c.answer_dialog(true));
        bind0!(on_dialog_cancel, |c: &Rc<Core>| c.answer_dialog(false));

        // DevTools panel: the models are installed once and patched in place
        // (see `write_devtools`), the two intents come back to the core.
        ui.set_console_rows(ModelRc::from(self.dt_console.clone()));
        ui.set_net_rows(ModelRc::from(self.dt_net.clone()));
        ui.set_inspector_rows(ModelRc::from(self.dt_inspect.clone()));
        ui.set_inspector_crumb(ModelRc::from(self.dt_crumb.clone()));
        {
            let c = self.clone();
            ui.on_devtools_select_view(move |view| c.set_devtools_view(DevToolsView::from_index(view)));
        }
        bind0!(on_devtools_close, |c: &Rc<Core>| c.close_devtools());
        bind0!(on_devtools_clear, |c: &Rc<Core>| c.devtools_clear());
        bind0!(on_devtools_toggle_pick, |c: &Rc<Core>| c.toggle_inspect_pick());
        {
            let c = self.clone();
            ui.on_devtools_select_node(move |node| c.inspector_select(node));
        }
        {
            let c = self.clone();
            ui.on_devtools_toggle_fold(move |node| c.inspector_toggle_fold(node));
        }
        // Copy gestures in the panel (task: copy in the DevTools tabs): a row
        // click highlights, a double-click highlights and copies, and the
        // header's `Copy` copies what the pane has highlighted. Panes travel
        // as numbers — 0 Console, 1 Network, 2 Inspector (`index` is the node
        // id there) — because that is what the list rows know.
        {
            let c = self.clone();
            ui.on_devtools_select_row(move |pane, index| c.devtools_select_row(pane, index));
        }
        {
            let c = self.clone();
            ui.on_devtools_copy_row(move |pane, index| c.devtools_copy_row(pane, index));
        }
        bind0!(on_devtools_copy, |c: &Rc<Core>| c.devtools_copy());
        // Dragging the panel's top edge reports itself (press / move / release
        // plus the pointer's travel since the grab); the height, its bounds and
        // which tab it belongs to are all decided here. See `devtools_edge`.
        {
            let c = self.clone();
            ui.on_devtools_edge(move |kind, travel| c.devtools_edge(kind, travel));
        }

        // Userscript menu (task: userscript menu): the toolbar toggle, the
        // scrim/✕ close, and the row/footer intents — every mutation lands in
        // the Store at once and rebroadcasts the enabled set to all live tabs.
        bind0!(on_toggle_us_menu, |c: &Rc<Core>| c.toggle_userscript_menu());
        bind0!(on_close_us_menu, |c: &Rc<Core>| c.userscript_menu(false));
        bind0!(on_us_new, |c: &Rc<Core>| c.us_new());
        bind0!(on_us_save, |c: &Rc<Core>| c.us_save());
        bind0!(on_us_cancel, |c: &Rc<Core>| c.us_cancel());
        {
            let c = self.clone();
            ui.on_us_toggle(move |idx| {
                if idx >= 0 {
                    c.us_toggle(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_us_delete(move |idx| {
                if idx >= 0 {
                    c.us_delete(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_us_edit(move |idx| {
                if idx >= 0 {
                    c.us_edit(idx as usize);
                }
            });
        }

        // User-Agent switcher (task: per-container UA): the toolbar toggle,
        // the scrim/✕ close, and the row/footer intents — every accepted
        // mutation lands in the Store at once and rebroadcasts
        // ToTab::SetUserAgent to the live tabs of that container only.
        bind0!(on_toggle_ua_menu, |c: &Rc<Core>| c.toggle_ua_menu());
        bind0!(on_close_ua_menu, |c: &Rc<Core>| c.ua_menu(false));
        bind0!(on_ua_copy, |c: &Rc<Core>| c.ua_copy());
        bind0!(on_ua_restore_default, |c: &Rc<Core>| c.ua_restore_default());
        {
            let c = self.clone();
            ui.on_ua_select(move |idx| {
                if idx >= 0 {
                    c.ua_select(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_ua_apply_custom(move |text| c.ua_apply_custom(text.as_str()));
        }

        // Proxy switcher (task: proxies): the toolbar toggle, the scrim/✕
        // close, the row picks (persist the definition name on the
        // container, rebroadcast its URL to that container's live tabs),
        // and the global definition editor (Add/Delete persist ui.json and
        // re-resolve every live tab).
        bind0!(on_proxy_add, |c: &Rc<Core>| c.proxy_add());
        {
            let c = self.clone();
            ui.on_proxy_select(move |idx| {
                if idx >= 0 {
                    c.proxy_select(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_proxy_delete(move |idx| {
                if idx > 0 {
                    c.proxy_delete(idx as usize);
                }
            });
        }

        // Archive mode (task: archive mode): the 💾 toolbar toggle, the
        // scrim/✕ close, the CheckBox switch (persist + broadcast to every
        // live tab), and the root-folder Apply (validated, rejected drafts
        // survive on the one-line error) — all pure Core state.
        bind0!(on_toggle_archive_menu, |c: &Rc<Core>| c.toggle_archive_menu());
        bind0!(on_close_archive_menu, |c: &Rc<Core>| c.archive_menu(false));
        {
            let c = self.clone();
            ui.on_archive_set_enabled(move |on| c.archive_set_enabled(on));
        }
        {
            let c = self.clone();
            ui.on_archive_apply_dir(move |text| c.archive_apply_dir(text.as_str()));
        }

        // Downloads (task: file downloads): the ⬇ toolbar toggle (and
        // Ctrl-Shift-D), the scrim/✕ close, the folder Apply (validated, a
        // rejected draft survives on the one-line error) and `Clear finished`
        // — all pure Core state. Nothing here fetches: the tab does.
        bind0!(on_toggle_download_menu, |c: &Rc<Core>| c.toggle_download_menu());
        bind0!(on_close_download_menu, |c: &Rc<Core>| c.download_menu(false));
        {
            let c = self.clone();
            ui.on_download_apply_dir(move |text| c.download_apply_dir(text.as_str()));
        }
        bind0!(on_download_clear_finished, |c: &Rc<Core>| c.download_clear_finished());

        // "Save page" (task: save-page selector): the scrim/✕ close, the
        // folder Choose (validated by the ⬇ panel's rules), and the Save
        // confirm, which asks the tab for its page under the name the field
        // holds — the write itself runs on through the download machinery to
        // the folder dialog picked. All pure Core state; nothing fetches here.
        bind0!(on_close_save_menu, |c: &Rc<Core>| c.save_dialog(false));
        {
            let c = self.clone();
            ui.on_save_pick_folder(move |text| c.save_pick_folder(text.as_str()));
        }
        {
            let c = self.clone();
            ui.on_save_page_confirm(move |text| c.save_page_confirm(text.as_str()));
        }

        // Browsing history (task: browsing history): the ☰ toolbar toggle, the
        // scrim/✕ close, **History** (open the `ironsurf://history` page in a
        // new tab — the visits are a page of their own now), and Clear (wipe
        // the log shared by every container) — all pure Core state.
        bind0!(on_toggle_history_menu, |c: &Rc<Core>| c.toggle_history_menu());
        bind0!(on_close_history_menu, |c: &Rc<Core>| c.history_menu(false));
        bind0!(on_history_clear, |c: &Rc<Core>| c.history_clear());
        bind0!(on_history_page, |c: &Rc<Core>| c.open_history_page());
        // Settings (task: settings menu): the ☰ panel's scroll-speed slider,
        // live as it drags — apply, persist, broadcast (see `set_scroll_speed`).
        {
            let c = self.clone();
            ui.on_scroll_speed_set(move |speed| c.set_scroll_speed(speed as f32));
        }
        // URL bar autocomplete (task: URL bar autocomplete suggestions): every
        // keystroke in the bar re-ranks the dropdown from the shared visit log
        // (`edited` fires on user input only, so pushes of a tab's own address
        // never trigger it), and the entry's routed ↑/↓/Enter/Esc plus the
        // panel's row clicks come straight back here.
        {
            let c = self.clone();
            ui.on_url_edited(move |text| c.suggest_refresh(text.as_str()));
        }
        {
            let c = self.clone();
            ui.on_suggest_move(move |delta| c.suggest_move(delta));
        }
        {
            let c = self.clone();
            ui.on_suggest_pick(move |idx| c.suggest_pick(idx));
        }
        bind0!(on_suggest_close, |c: &Rc<Core>| {
            if let Some(ui) = c.ui() {
                c.suggest_close(&ui);
            }
        });
    }
}

/// Expand a leading `~` / `~/…` to `$HOME/…` (the archive-folder LineEdit
/// accepts the shell spelling; anything else passes through untouched).
fn expand_tilde(raw: &str) -> String {
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

/// Grow, shrink and patch a shared Slint model in place. Appending only the
/// rows that arrived keeps the list's scroll position (a fresh `VecModel` per
/// tick would throw it away, so a page's log would jump to the bottom every
/// 33 ms while the user read the middle of it).
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, want: &[T]) {
    let have = model.row_count();
    let shared = have.min(want.len());
    for i in 0..shared {
        if model.row_data(i).map_or(true, |row| row != want[i]) {
            model.set_row_data(i, want[i].clone());
        }
    }
    for i in shared..want.len() {
        model.push(want[i].clone());
    }
    while model.row_count() > want.len() {
        model.remove(model.row_count() - 1);
    }
}

// ---------------------------------------------------------------------------
// context menu (task: context menu)

/// What a context-menu row does when picked. Clipboard, new-tab and history
/// entries are serviced by the shell alone; the engine-side ones (paste,
/// video play/pause) travel to the tab as `ToTab::ContextAction`.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    Back,
    Forward,
    Reload,
    /// Same container as the requesting tab (the middle-click rule).
    OpenNewTab(String),
    CopyText(String),
    Paste,
    ToggleVideo,
    /// “⌖ Inspect Element”: the click point is the menu's own (the menu opened
    /// under the cursor), which is exactly the point `ToTab::InspectPickAt`
    /// wants — see `Core::menu_pick`.
    InspectElement,
    /// “⬇ Download linked file” (task: file downloads): the shell asks *this
    /// tab* to fetch the address. It never fetches for a menu click, never
    /// writes, and never guesses a folder — the tab answers `DownloadStart`,
    /// the pump files the bytes.
    DownloadLink(String),
}

/// The menu for one right-click, in display order. Entries exist exactly
/// when their thing is there: link rows only over a link, video rows only
/// over a video, Paste only next to a caret *and* a non-empty clipboard.
/// Pure (labels + actions), so the entry matrix is unit-testable headless.
pub fn context_menu_items(
    target: &ContextTarget,
    url: Option<&str>,
    can_back: bool,
    can_forward: bool,
    clipboard_empty: bool,
) -> Vec<(String, MenuAction)> {
    let mut items: Vec<(String, MenuAction)> = Vec::new();
    // **What to do with the thing under the cursor comes first** (task: a
    // link's right-click opens with "Open link in new tab"): the row the click
    // was about leads the menu, and the page's own navigation rows follow it.
    // An image leads the same way, but only when there is no link to lead with
    // — a link wrapped around an image is a link first.
    if let Some(link) = &target.link {
        items.push((
            "Open link in new tab".into(),
            MenuAction::OpenNewTab(link.clone()),
        ));
    } else if let Some(image) = &target.image {
        items.push((
            "Open image in new tab".into(),
            MenuAction::OpenNewTab(image.clone()),
        ));
    }
    if can_back {
        items.push(("← Back".into(), MenuAction::Back));
    }
    if can_forward {
        items.push(("→ Forward".into(), MenuAction::Forward));
    }
    items.push(("⟳ Reload".into(), MenuAction::Reload));
    // A link around an image gets both rows: the link's led the menu, and the
    // image's stays down here with the image's other entries.
    if target.link.is_some() {
        if let Some(image) = &target.image {
            items.push((
                "Open image in new tab".into(),
                MenuAction::OpenNewTab(image.clone()),
            ));
        }
    }
    if let Some(selection) = &target.selection {
        items.push(("Copy".into(), MenuAction::CopyText(selection.clone())));
    }
    if let Some(link) = &target.link {
        items.push((
            "Copy link address".into(),
            MenuAction::CopyText(link.clone()),
        ));
        // Its sibling, in the order every browser keeps them (task: file
        // downloads): copy the address, or put the file on the disk.
        items.push((
            "⬇ Download linked file".into(),
            MenuAction::DownloadLink(link.clone()),
        ));
    }
    if let Some(image) = &target.image {
        items.push((
            "Copy image address".into(),
            MenuAction::CopyText(image.clone()),
        ));
    }
    if target.video.is_some() {
        items.push((
            if target.video_playing {
                "❚❚ Pause video"
            } else {
                "▶ Play video"
            }
            .into(),
            MenuAction::ToggleVideo,
        ));
        // A `src`-less `<video>` has no address to copy.
        if target.video.as_deref().is_some_and(|v| !v.is_empty()) {
            items.push((
                "Copy video address".into(),
                MenuAction::CopyText(target.video.clone().unwrap_or_default()),
            ));
        }
    }
    if target.editable && !clipboard_empty {
        items.push(("Paste".into(), MenuAction::Paste));
    }
    if let Some(page) = url {
        if !page.is_empty() {
            items.push((
                "Copy page address".into(),
                MenuAction::CopyText(page.to_owned()),
            ));
        }
    }
    // Last, as in every browser: whatever was right-clicked, the tree of the
    // page it lives in is always reachable from here.
    items.push(("⌖ Inspect Element".into(), MenuAction::InspectElement));
    items
}

/// A Ctrl+V key press (Slint delivers control characters for chords, so
/// both spellings count).
pub fn is_paste_chord(text: &str, ctrl: bool, alt: bool, meta: bool) -> bool {
    (ctrl || meta) && !alt && matches!(text, "v" | "V" | "\u{16}")
}

// ---------------------------------------------------------------------------
// helpers

/// Coerce raw RGBA bytes (straight alpha, top-down) into a Slint image.
pub fn image_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Image> {
    if width == 0 || height == 0 {
        return None;
    }
    if rgba.len() != width as usize * height as usize * 4 {
        return None; // corrupt frame: drop it
    }
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(rgba);
    Some(Image::from_rgba8(buffer))
}

/// Rate limit for forwarded hover moves: one per UI frame at most. A 1000 Hz
/// gaming mouse or a fast hand across the page otherwise floods the tab with
/// positions it cannot hit-test in time, and the click at the end of the swipe
/// waits behind all of them.
pub const MOVE_INTERVAL: Duration = Duration::from_millis(33);

/// May a hover move be forwarded now? `last` is when the previous one went out
/// (`None` = none sent yet, so the first always passes).
/// Whether an address counts as a visit worth the shared history log: not
/// empty, not `about:blank`, and not one of IronSurf's own `ironsurf://`
/// pages (the welcome screen is chrome, not browsing).
fn worth_recording(url: &str) -> bool {
    !url.is_empty() && url != "about:blank" && !url.starts_with("ironsurf://")
}

/// Whether closing a tab leaves something worth reopening: any real address,
/// including the internal start page (closing a fresh welcome tab and
/// Ctrl-Alt-T'ing it back is a normal thing to do).
fn reopenable(url: &str) -> bool {
    !url.is_empty() && url != "about:blank"
}

pub fn move_due(now: Instant, last: Option<Instant>) -> bool {
    match last {
        Some(t) => now.duration_since(t) >= MOVE_INTERVAL,
        None => true,
    }
}

/// IPC cursor shape → the Slint shape the window draws (task: cursor icon).
/// Every IPC shape but `VerticalText` has a Slint twin (the I-beam is the
/// nearest stand-in); the mapping is exhaustive so a new engine shape can
/// never silently render as an arrow… it renders as exactly what was agreed.
pub fn slint_cursor(cursor: ironsurf_common::CursorIcon) -> MouseCursor {
    use ironsurf_common::CursorIcon;
    match cursor {
        CursorIcon::Default => MouseCursor::Default,
        CursorIcon::Hidden => MouseCursor::None,
        CursorIcon::Help => MouseCursor::Help,
        CursorIcon::Pointer => MouseCursor::Pointer,
        CursorIcon::Progress => MouseCursor::Progress,
        CursorIcon::Wait => MouseCursor::Wait,
        CursorIcon::Crosshair => MouseCursor::Crosshair,
        CursorIcon::Text => MouseCursor::Text,
        CursorIcon::VerticalText => MouseCursor::Text,
        CursorIcon::Alias => MouseCursor::Alias,
        CursorIcon::Copy => MouseCursor::Copy,
        CursorIcon::Move => MouseCursor::Move,
        CursorIcon::NoDrop => MouseCursor::NoDrop,
        CursorIcon::NotAllowed => MouseCursor::NotAllowed,
        CursorIcon::Grab => MouseCursor::Grab,
        CursorIcon::Grabbing => MouseCursor::Grabbing,
        CursorIcon::ColResize => MouseCursor::ColResize,
        CursorIcon::RowResize => MouseCursor::RowResize,
        CursorIcon::NResize => MouseCursor::NResize,
        CursorIcon::EResize => MouseCursor::EResize,
        CursorIcon::SResize => MouseCursor::SResize,
        CursorIcon::WResize => MouseCursor::WResize,
        CursorIcon::NeResize => MouseCursor::NeResize,
        CursorIcon::NwResize => MouseCursor::NwResize,
        CursorIcon::SeResize => MouseCursor::SeResize,
        CursorIcon::SwResize => MouseCursor::SwResize,
        CursorIcon::EwResize => MouseCursor::EwResize,
        CursorIcon::NsResize => MouseCursor::NsResize,
        CursorIcon::NeswResize => MouseCursor::NeswResize,
        CursorIcon::NwseResize => MouseCursor::NwseResize,
    }
}

/// `#rrggbb` → Color, tolerant of bad input.
pub fn color_hex(hex: &str) -> Color {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color::from_rgb_u8(r, g, b);
        }
    }
    Color::from_rgb_u8(158, 158, 158)
}

/// Does the browsing server follow the archive folder? Until its own control
/// page has been used to name a folder (`None` in the settings), the server
/// serves whatever the archive writes into; after that the choice is its own.
fn follows_archive_root(saved_root: Option<&str>) -> bool {
    saved_root.map(str::trim).filter(|t| !t.is_empty()).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loading_bar_sweep_rolls_the_full_track_and_back() {
        // Positions over one round trip (24 ticks), straight out and back.
        let out: Vec<usize> = (0..LOAD_SWEEP_STEPS).collect();
        let back: Vec<usize> = (0..LOAD_SWEEP_STEPS).rev().collect();
        let trip: Vec<usize> = out.iter().chain(back.iter()).copied().collect();
        for step in 0..60 {
            let p = load_sweep_position(step);
            assert_eq!(
                p, trip[step % trip.len()],
                "step {step}: sweep drifted off the triangle wave"
            );
            assert!(p < LOAD_SWEEP_STEPS, "step {step} left the track: {p}");
        }
        // A position never repeats for three ticks running: the triangle wave
        // touches each end once per round trip and keeps rolling, so an
        // in-flight load cannot sit painted in one place long enough to read
        // as a stalled bar.
        for step in 2..60 {
            let (a, b, c) = (
                load_sweep_position(step - 2),
                load_sweep_position(step - 1),
                load_sweep_position(step),
            );
            assert!(
                !(a == b && b == c),
                "step {step}: the segment stood still ({a}) for three ticks"
            );
        }
    }
    #[test]
    fn the_first_move_always_passes() {
        let now = Instant::now();
        assert!(move_due(now, None));
    }

    #[test]
    fn moves_inside_one_frame_are_dropped_until_the_interval_passes() {
        let t0 = Instant::now();
        assert!(move_due(t0, None));
        // A burst inside the same UI frame: nothing more goes out…
        for i in 1..10 {
            assert!(
                !move_due(t0 + Duration::from_millis(i * 3), Some(t0)),
                "move {i} must be dropped (superseded positions cost the tab a repaint each)"
            );
        }
        // …until the next frame's worth of time has elapsed.
        assert!(move_due(t0 + MOVE_INTERVAL, Some(t0)));
        assert!(move_due(t0 + MOVE_INTERVAL * 4, Some(t0)));
    }

    #[test]
    fn a_press_never_waits() {
        // `page_pointer` only consults the gate for Move; every other kind is
        // sent straight away. Assert the gate itself is the only thing that can
        // hold a pointer event back, i.e. presses do not call it with a recent
        // timestamp and get denied.
        let t0 = Instant::now();
        assert!(move_due(t0, None), "gate opens for the first move");
        assert!(!move_due(t0, Some(t0)), "gate closes again inside a frame");
    }

    // ---- archive browsing server: following the archive folder --------------

    #[test]
    fn the_browsing_server_follows_the_archive_folder_until_aimed_elsewhere() {
        // Never configured (or configured then cleared): the archive root wins.
        assert!(follows_archive_root(None));
        assert!(follows_archive_root(Some("")), "a cleared field follows again");
        assert!(follows_archive_root(Some("   ")), "blank is not a choice");
        // A folder named once is the server's own and survives.
        assert!(!follows_archive_root(Some("/home/you/other")));
    }

    // ---- context menu (task: right-click menu) ------------------------------

    fn target(
        link: Option<&str>,
        image: Option<&str>,
        video: Option<&str>,
        selection: Option<&str>,
        editable: bool,
    ) -> ContextTarget {
        ContextTarget {
            link: link.map(String::from),
            image: image.map(String::from),
            video: video.map(String::from),
            video_playing: false,
            selection: selection.map(String::from),
            editable,
        }
    }

    #[test]
    fn the_plain_page_menu_is_history_reload_and_address() {
        let items = context_menu_items(&target(None, None, None, None, false), Some("https://example.com/"), true, false, true);
        let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            ["← Back", "⟳ Reload", "Copy page address", "⌖ Inspect Element"]
        );
        assert_eq!(items[2].1, MenuAction::CopyText("https://example.com/".into()));
    }

    #[test]
    fn a_link_earns_its_own_row_and_its_address_row() {
        let items = context_menu_items(
            &target(Some("https://example.com/a"), None, None, None, false),
            Some("https://example.com/"),
            false,
            false,
            true,
        );
        let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Open link in new tab",
                "⟳ Reload",
                "Copy link address",
                "⬇ Download linked file",
                "Copy page address",
                "⌖ Inspect Element",
            ]
        );
        // The row the click was about leads the menu (task: first item).
        assert_eq!(
            items[0].1,
            MenuAction::OpenNewTab("https://example.com/a".into())
        );
        assert_eq!(
            items[3].1,
            MenuAction::DownloadLink("https://example.com/a".into())
        );
    }

    #[test]
    fn selection_image_and_video_entries_only_appear_over_their_own_thing() {
        let items = context_menu_items(
            &target(
                None,
                Some("https://example.com/i.png"),
                Some("https://example.com/v.mp4"),
                Some("selected words"),
                true,
            ),
            Some("https://example.com/"),
            false,
            true,
            false,
        );
        let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Open image in new tab",
                "→ Forward",
                "⟳ Reload",
                "Copy",
                "Copy image address",
                "▶ Play video",
                "Copy video address",
                "Paste",
                "Copy page address",
                "⌖ Inspect Element",
            ]
        );
        assert_eq!(items[3].1, MenuAction::CopyText("selected words".into()));
        assert_eq!(items[7].1, MenuAction::Paste);

        // …and a plain right-click has none of them.
        let bare = context_menu_items(
            &target(None, None, None, None, false),
            None,
            false,
            false,
            true,
        );
        assert_eq!(
            bare.len(),
            2,
            "Reload, and the tree of the page the click landed in"
        );
    }

    #[test]
    fn a_playing_video_offers_pause_and_a_src_less_one_no_address() {
        let mut t = target(None, None, Some("https://example.com/v.mp4"), None, false);
        t.video_playing = true;
        let items = context_menu_items(&t, None, false, false, true);
        assert!(items.iter().any(|(l, _)| l == "❚❚ Pause video"));

        t.video = Some(String::new());
        t.video_playing = false;
        let items = context_menu_items(&t, None, false, false, true);
        assert!(items.iter().any(|(l, _)| l == "▶ Play video"));
        assert!(
            !items.iter().any(|(l, _)| l == "Copy video address"),
            "a src-less <video> has no address to copy"
        );
    }

    #[test]
    fn paste_waits_for_a_caret_and_a_non_empty_clipboard() {
        let editable_empty_clip = context_menu_items(
            &target(None, None, None, None, true),
            None,
            false,
            false,
            true,
        );
        assert!(
            !editable_empty_clip.iter().any(|(l, _)| l == "Paste"),
            "nothing copied yet — nothing to paste"
        );
        let editable_full_clip = context_menu_items(
            &target(None, None, None, None, true),
            None,
            false,
            false,
            false,
        );
        assert!(editable_full_clip.iter().any(|(l, _)| l == "Paste"));
        let clip_without_caret = context_menu_items(
            &target(None, None, None, None, false),
            None,
            false,
            false,
            false,
        );
        assert!(
            !clip_without_caret.iter().any(|(l, _)| l == "Paste"),
            "no caret — the menu must not promise a paste"
        );
    }

    #[test]
    fn the_devtools_table_grows_in_place_and_shrinks_on_a_tab_switch() {
        let model: Rc<VecModel<ConsoleRow>> = Rc::new(VecModel::default());
        let row = |t: &str| ConsoleRow {
            level: "log".into(),
            text: t.into(),
            selected: false,
        };
        sync_model(&model, &[row("a"), row("b")]);
        assert_eq!(model.row_count(), 2);
        // The usual tick: the log got longer, the old rows stay where they are
        // (patching, not resetting, is what keeps the list's scroll position).
        sync_model(&model, &[row("a"), row("b"), row("c")]);
        assert_eq!(model.row_count(), 3);
        assert_eq!(model.row_data(0).unwrap().text.as_str(), "a");
        assert_eq!(model.row_data(2).unwrap().text.as_str(), "c");
        // Another tab's log: patched in place, surplus dropped.
        sync_model(&model, &[row("z")]);
        assert_eq!(model.row_count(), 1);
        assert_eq!(model.row_data(0).unwrap().text.as_str(), "z");
        sync_model(&model, &[]);
        assert_eq!(model.row_count(), 0);
    }

    #[test]
    fn devtools_pane_indices_round_trip() {
        assert_eq!(DevToolsView::default(), DevToolsView::Console);
        assert_eq!(DevToolsView::Console.index(), 0);
        assert_eq!(DevToolsView::Network.index(), 1);
        assert_eq!(DevToolsView::from_index(1), DevToolsView::Network);
        assert_eq!(DevToolsView::from_index(0), DevToolsView::Console);
        assert_eq!(
            DevToolsView::from_index(7),
            DevToolsView::Console,
            "an unknown pane falls back to Console"
        );
    }

    #[test]
    fn the_paste_chord_is_detected_whichever_way_the_backend_spells_it() {
        assert!(is_paste_chord("v", true, false, false));
        assert!(is_paste_chord("V", true, false, false));
        assert!(is_paste_chord("\u{16}", true, false, false));
        assert!(!is_paste_chord("v", false, false, false), "a bare v types");
        assert!(!is_paste_chord("v", true, true, false), "Alt+V is not paste");
        assert!(!is_paste_chord("x", true, false, false));
    }

    #[test]
    fn cursor_icons_map_onto_slint_shapes() {
        // Task: hover cursor. The mapping is exhaustive at compile time; these
        // are the spots where it is a deliberate stand-in rather than a twin.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::Default), MouseCursor::Default);
        // The pointing hand — the feature's headline shape.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::Pointer), MouseCursor::Pointer);
        // `cursor: none` is Slint's `None`, not the default arrow.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::Hidden), MouseCursor::None);
        // Slint cannot draw a vertical I-beam; the horizontal one stands in.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::VerticalText), MouseCursor::Text);
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::NwseResize), MouseCursor::NwseResize);
    }

    // ---- DevTools panel height (task: drag the panel's top edge) -----------

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn an_untouched_panel_is_240_tall_or_the_pages_it_fits_in() {
        assert!(near(default_devtools_height(768.), 240.)); // 65 % would be taller
        assert!(near(default_devtools_height(300.), 195.)); // a short area wins
        // A window that was never laid out must not collapse the panel to nil.
        assert!(near(default_devtools_height(0.), DEVTOOLS_DEFAULT_HEIGHT));
    }

    #[test]
    fn a_dragged_height_is_kept_inside_the_page_area() {
        assert!(near(clamp_devtools_height(300., 768.), 300.)); // in range: as asked
        assert!(near(clamp_devtools_height(40., 768.), DEVTOOLS_MIN_HEIGHT)); // floor
        assert!(near(clamp_devtools_height(9000., 768.), 768. * 0.9)); // ceiling
        // An area too short for even the floor gets the area: a panel may never
        // be taller than the page it docks into.
        assert!(near(clamp_devtools_height(500., 40.), 40.));
    }

    #[test]
    fn a_panel_paints_its_own_drag_or_the_default_never_anothers() {
        assert!(near(desired_devtools_height(None, 768.), 240.));
        assert!(near(desired_devtools_height(Some(500.), 768.), 500.));
        // A drag outlives the window size that made it possible, but only as a
        // memory: what paints is the fit, so a shrunk area is never overflowed.
        let fitted = desired_devtools_height(Some(500.), 300.);
        assert!(fitted <= 300. * 0.9 + 0.01 && fitted >= DEVTOOLS_MIN_HEIGHT - 0.01);
    }
}
