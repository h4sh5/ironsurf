//! The Blitz tab runtime (task 7c): sandbox + one `HtmlDocument` per
//! navigation, speaking the `ironsurf-common` IPC protocol over stdin/stdout.
//!
//! Lifecycle per `ARCHITECTURE.md` §ironsurf-tab (Blitz edition):
//! 1. apply the OS sandbox (`writable_rw = [profile-dir]`),
//! 2. own the event loop on ONE thread — `poll()` over `[non-blocking stdin,
//!    self-pipe]` (no winit, no engine thread; Blitz is a library we drive —
//!    `HtmlDocument` is neither `Send` nor `Sync`, see `docs/BLITZ_API_NOTES.md` §8),
//! 3. one `HtmlDocument` per navigation: main-document fetch always goes
//!    through `netmod::fetch_document` (managed policy applies), subresources
//!    flow through our `BlitzNetProvider`,
//! 4. `resolve` → `render_to_buffer::<VelloCpuImageRenderer>` →
//!    `FromTab::Frame` (≤30 fps coalesced), `TitleChanged`/`UrlChanged`/
//!    `HistoryChanged` emitted on change,
//! 5. on `Destroy`/EOF: persist the cookie jar, flush stdout, exit 0.
//!
//! INPUT/LINK-CLICKS (task 7d): `ToTab::Pointer/Key/Scroll` route through the
//! single [`handle_input`] extension point; link clicks round-trip via the
//! `NavigationProvider` hook ([`NavHook`]) already wired into every document.
//!
//! RESPONSIVENESS (task 11): a real mouse emits far more hover moves than this
//! single-threaded renderer can afford, so a stdin batch is collapsed to the
//! newest move ([`coalesce_moves`]) and a hover move that lands on the same
//! element skips `resolve` + repaint entirely; hovering an `<a href>` reports
//! its target as `FromTab::StatusText` so links are visibly clickable. See
//! `docs/ENGINE_INTEGRATION.md` §E for the measured before/after numbers.
//!
//! JAVASCRIPT (tasks 8 + real timers): after parse + resolve, every
//! navigation runs the document's `<script>`s in one fresh Boa context
//! (`crate::js` — tiny binding surface, `src=` through our own fetcher,
//! exceptions caught). The context lives for the **lifetime of the document**
//! ([`js::JsRuntime`], owned by [`Tab::js`]): `setTimeout`/`setInterval`/
//! `requestAnimationFrame` create queue entries that outlive the initial
//! script run, and every poll iteration (plus the `--headless` wait window)
//! executes the due callbacks, then settle + coalesced repaint exactly like
//! the input path. A throwing callback is reported like a script error and
//! never stops later timers or the tab. A script's `document.title = …`
//! becomes `FromTab::TitleChanged` + a repaint; script errors and
//! `console.*` output become `FromTab::StatusText`. JS-driven navigation is
//! real: `location.href=`/`assign`/`replace` queue a load the poll loop
//! follows via [`Tab::drain_nav_hook`] (search-engine redirects), and
//! `javascript:` URLs run as programs at [`Tab::navigate`] — the single
//! navigation entry point — with a string completion value becoming a new
//! `about:blank` document (per spec).
//!
//! `data:` URLS: a fetched `data:` document goes through [`render_document`]
//! before it reaches the HTML parser, so `data:text/javascript;base64,…` (and
//! other non-markup media types) is displayed as inert source instead of being
//! parsed as HTML — while a page's own `<script src="data:text/javascript;base64,…">`
//! still fetches and runs (`docs/ENGINE_INTEGRATION.md` §G).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{self, BufWriter, Read, StdoutLock, Write};
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyrender::{render_to_buffer, PaintScene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{
    Document as _, DocumentConfig, ElementData, LocalName, NodeId,
};
use cursor_icon::CursorIcon as BlitzCursorIcon;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, KeyState,
    MouseEventButton, MouseEventButtons, Point, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::shell::{ClipboardError, ColorScheme, ShellProvider, Viewport};
use ironsurf_common::{
    read_frame, write_frame, userscripts_for_url, ContextAction, ContextTarget, ContainerId,
    CursorIcon, DialogKind, DownloadHint, DownloadTrigger, FromTab, InspectSnapshot, KeyEvent,
    Modifiers, NetEvent, PointerEvent, PointerKind, ScrollEvent, ScrollState, TabId, ToTab,
    Userscript,
    MAX_CONTEXT_TEXT, MAX_DIALOG_MESSAGE,
};
use keyboard_types::{Code, Key, Location, Modifiers as KMods};
use kurbo::{Affine, Rect, Size};
use peniko::{Color, Fill};
// stderr diagnostics (see `ironsurf_common::logging`): `info` = the user-visible
// timeline (navigation, load start/finish, title/url), `debug` = per-step detail,
// `trace` = per-frame/per-input noise. stdout is the IPC channel — never a log.
use ironsurf_common::logging;
use ironsurf_common::{debug, error, info, trace, warn};
use ironsurf_sandbox::SandboxPolicy;
use ironsurf_storage::{ContainerStore, CookieJar, NetCache};
use url::Url;

use crate::downloads;
use crate::find;
use crate::forms::{self, FormMethod, FormSubmission};
use crate::inspector;
use crate::js::{self, HostRef};
use crate::media::player::{MediaPlayer, PlayerEvent, PlayerState, MAX_VIDEO_BYTES};
use crate::media::widget::VideoWidget;
use crate::media::MediaError;
use crate::logging::{ConsoleQueue, CssScanState};
use crate::netmod::{self, BlitzNetProvider, MainDocPost, NetPolicy};
use crate::reader;
use crate::select;

/// Default page size until the shell sends `ToTab::Resize` (physical px).
const DEFAULT_SIZE: (u32, u32) = (1024, 768);
/// Frame throttle: never emit more than ~30 fps (coalesce within one window).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Idle poll cadence — stdin/wake events still arrive immediately via poll().
const IDLE_TICK: Duration = Duration::from_millis(250);
/// Media cadence: the poll loop never sleeps past this while a video is
/// alive, so players keep advancing (~25×/s) with no other events — the
/// `RUNNING_POLL_INTERVAL` role of the JS-timer cap, for media.
const MEDIA_TICK: Duration = Duration::from_millis(40);
/// How far `↑` / `↓` take the document, CSS px — "a line", and the same step
/// `window.scrollBy(0, 40)` takes. Four times the wheel's damped line unit
/// ([`WHEEL_SPEED`] on blitz's 20 CSS px notch, `events/pointer.rs`), which is
/// about two lines of body text at the default font size: enough to see that
/// the page moved, small enough that a paragraph is a few presses and a screen
/// is a dozen.
const ARROW_SCROLL_PX: f64 = 40.0;
/// Wheel speed-up factor (task: slower and smoother wheel): the travel the
/// document makes per wheel event, relative to what blitz would make of it
/// (20 CSS px per notch, a raw pixel delta verbatim). Half turns one notch
/// into a gentle 10 px nudge and a fast flick into lines, not screens.
const WHEEL_SPEED: f64 = 0.5;
/// The smooth scroller's cadence: the poll loop never sleeps past one slice
/// interval while a glide is in flight, so at rest slices are fed at ~60 Hz.
const WHEEL_STEP: Duration = Duration::from_millis(16);
/// The glide's decay constant: a slice settles the fraction of what is still
/// owed that `dt` of an exponential of this time constant covers — small
/// quick slices on a fast machine, bigger rarer ones when a software repaint
/// eats the frame, and the same ~3τ (~⅑ s) landing either way. The page is
/// therefore *always* still moving with the first slice (40%+ of the travel)
/// and at rest shortly after the last notch, whatever the paint costs.
const WHEEL_TAU: Duration = Duration::from_millis(30);

/// The tab-side smooth scroller's state (task: slower and smoother wheel):
/// wheel travel — already damped by [`WHEEL_SPEED`] — queued here and gliding
/// into the document one eased slice per [`WHEEL_STEP`] instead of teleporting.
/// A main-document fetch in flight on the network worker (task: faster,
/// responsive loading): the receiver delivers `(fetch result, DevTools
/// events)` for the poll loop to adopt.
struct PendingNavigation {
    rx: mpsc::Receiver<(Result<netmod::DocFetch, netmod::NetError>, Vec<NetEvent>)>,
    url: Url,
    history: NavHistory,
    started: Instant,
}

#[derive(Clone, Copy)]
struct WheelGlide {
    /// Travel still owed to the page, CSS px, winit convention (positive =
    /// up, so the usual negative wheel-down delta glides the document down).
    pending: (f64, f64),
    /// When the last slice was fed — the slice size is the travel a window
    /// this wide eases out, so the glide lands on wall-clock, not on how
    /// many times the loop happened to turn.
    last: Instant,
    /// Earliest moment the next slice may be fed.
    next: Instant,
    /// Where the wheel was last turned — replayed with every slice so blitz
    /// resolves the hovered scroll chain from the same hover state. `None`
    /// until the first wheel event, which is also the only writer of
    /// `pending`, so a running glide always has a place.
    at: Option<PointerCoords>,
    /// The same point in logical px — what [`Tab::report_cursor`] hit-tests.
    over: (f32, f32),
}

/// One slice of the glide: what a window of `dt` settles under an
/// exponential of [`WHEEL_TAU`], the last CSS px fed whole and a window of
/// four τ (or more) just landing the rest — the glide terminates instead of
/// asymptoting, and a starved scroller catches up instead of crawling.
fn wheel_glide_step(remaining: f64, dt: Duration) -> f64 {
    if remaining.abs() <= 1.0 || dt >= 4 * WHEEL_TAU {
        return remaining;
    }
    // Never smaller than one cadence step: the first slice of a gesture has
    // a `dt` of ~0 (the gesture armed the clock “now”), and a kick of ~40%
    // is exactly what makes the page feel attached to the wheel.
    let dt = dt.max(WHEEL_STEP).as_secs_f64();
    let fraction = 1.0 - (-(dt / WHEEL_TAU.as_secs_f64())).exp();
    remaining * fraction
}

// ---------------------------------------------------------------------------
// CLI

#[derive(Clone, Debug)]
pub struct TabArgs {
    pub tab_id: TabId,
    pub container_id: ContainerId,
    pub profile_dir: PathBuf,
    pub url: String,
    /// Directory consulted for `file://` navigations whose path is missing:
    /// `DIR/<basename>` is tried before the fetch fails (fixture layout).
    pub offline_fixtures: Option<PathBuf>,
    pub net_policy: NetPolicy,
    pub no_sandbox: bool,
    /// Video master switch (`--no-media` turns it off): with it off the tab
    /// never attaches a widget, never fetches a `<video src>`, and every
    /// media step of the poll loop returns before touching anything.
    pub media_enabled: bool,
    /// Initial **User-Agent override** (`--user-agent <STRING>`): the tab
    /// sends it on every request until a `ToTab::SetUserAgent` replaces it.
    /// `None` (flag absent, or blank after trim) = the built-in default
    /// [`netmod::USER_AGENT`].
    pub user_agent: Option<String>,
    /// Initial **proxy override** (`--proxy <URL>`): every request the tab
    /// makes rides that proxy until a `ToTab::SetProxy` replaces it. The
    /// shell resolves the container's definition name to its URL and
    /// appends this to that spawn alone, so the very first page load of a
    /// fresh tab already goes out through it. `None` (flag absent, or blank
    /// after trim) = direct connections. A value reqwest cannot parse is a
    /// fatal startup error — the flag came from settings the shell already
    /// validated, so a bad one means the invocation itself is broken.
    pub proxy: Option<String>,
    /// Private browsing at spawn (`--private`, present = on): the tab sends
    /// `DNT: 1` on every request from its very first fetch until a
    /// `ToTab::SetDoNotTrack` clears it (task: private browsing). The
    /// shell appends the flag to spawns in private containers only; the
    /// history half of the mode is shell-side, never the tab's concern.
    pub private: bool,
    /// Archive mode master switch at spawn (`--archive-mode`, present = on):
    /// the shell passes it when its 💾 toggle is on, so a fresh tab archives
    /// from its first fetch; `ToTab::SetArchiveMode` moves it afterwards.
    pub archive_mode: bool,
}

impl TabArgs {
    pub fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut tab_id = TabId(0);
        let mut container_id = ContainerId(1);
        let mut profile_dir: Option<PathBuf> = None;
        let mut url = None;
        let mut offline_fixtures = None;
        let mut managed = false;
        let mut no_sandbox = false;
        let mut no_media = false;
        let mut user_agent = None;
        let mut proxy = None;
        let mut private = false;
        let mut archive_mode = false;

        let mut it = args.iter().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |what: &str| -> Result<String, Box<dyn Error>> {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{what} requires a value").into())
            };
            match arg.as_str() {
                "--tab-id" => tab_id = TabId(value("tab-id")?.parse()?),
                "--container-id" => container_id = ContainerId(value("container-id")?.parse()?),
                "--profile-dir" => profile_dir = Some(PathBuf::from(value("profile-dir")?)),
                "--url" => url = Some(value("url")?),
                "--offline-fixtures" => {
                    offline_fixtures = Some(PathBuf::from(value("offline-fixtures")?))
                }
                "--managed-net" => managed = true,
                "--no-sandbox" => no_sandbox = true,
                "--no-media" => no_media = true,
                "--user-agent" => user_agent = Some(value("user-agent")?),
                "--proxy" => proxy = Some(value("proxy")?),
                "--private" => private = true,
                "--archive-mode" => archive_mode = true,
                // Verbosity flags: the level itself is read from argv by
                // `logging::level_from_args` in `run`; they are accepted here so
                // passing them is not a fatal "unexpected argument".
                "--log" | "--log-level" => {
                    value("log")?;
                }
                "-v" | "-vv" | "--verbose" | "--debug" => {}
                other if other.starts_with("--log=") => {}
                other => return Err(format!("unexpected argument {other:?}").into()),
            }
        }
        let profile_dir = match profile_dir {
            Some(dir) => dir,
            None => {
                let store = ironsurf_storage::Store::open(ironsurf_storage::default_data_root())?;
                store.profile_dir(container_id)
            }
        };
        std::fs::create_dir_all(&profile_dir)?;

        Ok(TabArgs {
            tab_id,
            container_id,
            profile_dir,
            url: url.unwrap_or_else(ironsurf_common::default_url),
            offline_fixtures,
            net_policy: if managed {
                NetPolicy::Managed
            } else {
                NetPolicy::Passthrough
            },
            no_sandbox,
            media_enabled: !no_media,
            user_agent,
            // Fail fast (before the sandbox, before the first fetch) on a
            // proxy the tab could never speak — same resolve as runtime.
            proxy: match netmod::resolve_proxy(proxy.as_deref()) {
                Ok(_) => proxy,
                Err(err) => return Err(err.into()),
            },
            private,
            archive_mode,
        })
    }
}

// ---------------------------------------------------------------------------
// Self-pipe (loop wake-up from ShellProvider / NetWaker worker threads)

/// Non-blocking pipe; every wake-up is one byte. The read end joins the
/// `poll()` set, the write end is shared with the Blitz hooks (all of which
/// are `Send + Sync` and may fire on net-worker threads).
pub struct SelfPipe {
    read_fd: RawFd,
    write_fd: RawFd,
}
// Raw file descriptors we own; writes are single non-blocking `write()` calls.
unsafe impl Send for SelfPipe {}
unsafe impl Sync for SelfPipe {}

impl SelfPipe {
    pub fn new() -> io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        #[cfg(target_os = "linux")]
        {
            // SAFETY: writing two fds into a 2-element array.
            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS lacks pipe2(): pipe() then fcntl the ends non-blocking/close-on-exec.
            // SAFETY: writing two fds into a 2-element array.
            let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            for fd in &fds {
                // SAFETY: F_GETFL/F_SETFL/F_SETFD on our freshly created fds.
                let flags = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
                if flags == -1
                    || unsafe { libc::fcntl(*fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
                    || unsafe { libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1
                {
                    let err = io::Error::last_os_error();
                    // SAFETY: closing our own pipe ends on the setup error path.
                    unsafe {
                        libc::close(fds[0]);
                        libc::close(fds[1]);
                    }
                    return Err(err);
                }
            }
        }
        Ok(SelfPipe {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    pub fn read_fd(&self) -> RawFd {
        self.read_fd
    }

    /// Poke the loop. Ignores `EAGAIN`/full-pipe: a saturated pipe already
    /// means wake-ups are pending.
    pub fn wake(&self) {
        // SAFETY: single-byte write to our own pipe end.
        unsafe {
            let byte: [u8; 1] = [1];
            libc::write(self.write_fd, byte.as_ptr() as *const libc::c_void, 1);
        }
    }

    /// Drain all pending wake-up bytes; returns how many were pending (the
    /// loop uses "something woke us" as the resolve/paint trigger).
    pub fn drain(&self) -> usize {
        let mut buf = [0u8; 256];
        let mut total = 0usize;
        loop {
            // SAFETY: read into a valid buffer from our own pipe end.
            let n = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                return total; // EAGAIN or error: drained.
            }
            total += n as usize;
        }
    }
}

// ---------------------------------------------------------------------------
// Blitz shell / navigation hooks

/// `ShellProvider` bridging document-side signals (redraw requests, title
/// flushes) into the IPC loop via the self-pipe. Shared across navigations;
/// the title slot is reset at the start of every navigation.
struct ShellHook {
    pipe: Arc<SelfPipe>,
    title: Mutex<Option<String>>,
    /// Session clipboard *into* the editors (blitz asks `get_clipboard_text`
    /// on Ctrl+V; the shell pushes contents with `ToTab::Clipboard` right
    /// before relaying the keystroke — the sandboxed tab cannot reach the
    /// clipboard itself).
    clipboard: Mutex<String>,
    /// Session clipboard *out of* the editors (Ctrl+C / Ctrl+X / editor cut):
    /// queued here, drained into `FromTab::ClipboardText` by the poll loop.
    copied: Mutex<Option<String>>,
}

impl ShellProvider for ShellHook {
    fn request_redraw(&self) {
        self.pipe.wake();
    }
    fn set_window_title(&self, title: String) {
        debug!("document title flushed by blitz: {title:?}");
        *self.title.lock().expect("title mutex") = Some(title);
        self.pipe.wake();
    }
    fn get_clipboard_text(&self) -> Result<String, ClipboardError> {
        Ok(self.clipboard.lock().expect("clipboard mutex").clone())
    }
    fn set_clipboard_text(&self, text: String) -> Result<(), ClipboardError> {
        let text = match text.char_indices().nth(MAX_CONTEXT_TEXT) {
            Some((at, _)) => text[..at].to_owned(),
            None => text,
        };
        debug!("clipboard: page copied {} char(s)", text.chars().count());
        *self.copied.lock().expect("clipboard mutex") = Some(text);
        self.pipe.wake();
        Ok(())
    }
}

/// `NavigationProvider`: link clicks / form submits are recorded here (called
/// inline on the doc thread from `handle_ui_event`) and picked up by the loop
/// right after the event — the top-level fetch always goes through
/// `fetch_document`, never through Blitz (ARCHITECTURE §Network layer).
struct NavHook {
    pipe: Arc<SelfPipe>,
    pending: Mutex<Option<NavigationOptions>>,
}

impl NavigationProvider for NavHook {
    fn navigate_to(&self, options: NavigationOptions) {
        debug!("page asked to navigate → {}", options.url);
        *self.pending.lock().expect("nav mutex") = Some(options);
        self.pipe.wake();
    }
}

// ---------------------------------------------------------------------------
// Sandbox (same read-only set the Servo-era runtime needed, see a3cb047)

fn apply_sandbox(args: &TabArgs) {
    let exe = std::env::current_exe().ok();
    let exe_dir = exe
        .as_ref()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut readable: Vec<PathBuf> = ironsurf_sandbox::default_readable_ro()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    // Extras the old runtime listed: the dynamic-loader cache, fontconfig
    // (first text layout) and the CA symlink targets under /etc/ssl/certs.
    for extra in [
        "/etc/ld.so.cache",
        "/etc/glvnd",
        "/usr/share/glvnd",
        "/etc/fonts",
        "/usr/share/fontconfig",
        "/usr/share/ca-certificates",
    ] {
        if std::path::Path::new(extra).exists() {
            readable.push(PathBuf::from(extra));
        }
    }
    readable.push(exe_dir.clone());
    let policy = SandboxPolicy {
        readable_ro: readable,
        writable_rw: vec![args.profile_dir.clone()],
        exec_ro: exe.map(|exe| vec![exe]).unwrap_or_default(),
        ..SandboxPolicy::default()
    };
    debug!(
        "applying sandbox: {} readable, writable {}",
        policy.readable_ro.len(),
        policy.writable_rw.len()
    );
    match ironsurf_sandbox::apply(&policy) {
        Ok(status) => info!(
            "sandbox applied: landlock={} rlimits={} notes={:?}",
            status.landlock_active, status.rlimits_applied, status.notes
        ),
        // Sandboxing must never abort the tab (contract): degrade loudly.
        Err(err) => error!("sandbox failed (degraded, continuing): {err}"),
    }
}

// ---------------------------------------------------------------------------
// URL resolution

/// Resolve shell-provided URL text: parse as-is; if that fails and the text
/// looks like a bare host (no whitespace), assume `https://…` — the same
/// search-or-navigate leniency the shell applies.
/// A `javascript:` URL's program text (case-insensitive scheme, surrounding
/// whitespace stripped), percent-decoded the way a browser decodes a URL
/// before treating it as code. `None` for anything else — every other scheme
/// keeps going through [`parse_input_url`]/the fetch path.
fn javascript_url_code(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed
        .get(.."javascript:".len())?
        .eq_ignore_ascii_case("javascript:")
    {
        let body = &trimmed["javascript:".len()..];
        let code = String::from_utf8_lossy(&netmod::percent_decode(body.as_bytes())).into_owned();
        return Some(code);
    }
    None
}

/// Why a URL can never be saved (task: file downloads): an internal page is a
/// surface the renderer draws, `javascript:` is a program, and only http(s)
/// has bytes the fetch layer can pull. `None` = go and get it.
pub fn download_target_problem(url: &Url) -> Option<String> {
    if url.scheme() == netmod::INTERNAL_SCHEME {
        Some("an internal page is not a file".to_owned())
    } else if url.scheme() == "javascript" {
        Some("a `javascript:` target is a program, not bytes".to_owned())
    } else if netmod::is_http_scheme(url) {
        None
    } else {
        Some(format!("{} is not a scheme the tab can fetch", url.scheme()))
    }
}

pub fn parse_input_url(input: &str) -> Result<Url, Box<dyn Error>> {
    match Url::parse(input) {
        Ok(url) => Ok(url),
        Err(original) => {
            if input.contains(char::is_whitespace) {
                return Err(format!("not a URL: {input:?} ({original})").into());
            }
            Url::parse(&format!("https://{input}"))
                .map_err(|_| format!("not a URL: {input:?} ({original})").into())
        }
    }
}

// ---------------------------------------------------------------------------
// What the HTML parser actually receives

/// Escape text for use in HTML PCDATA (the source view must show `<script>`
/// tags as text, never as live markup).
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
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

/// A read-only viewer for a non-markup document, in the spirit of what a
/// browser shows when you navigate to a script/stylesheet/plain-text URL: a
/// header line plus the source in a `<pre>`.
fn source_view_document(title: &str, meta: &str, source: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title></head>
<body style="margin: 0; background: #ffffff; color: #101010; font-family: monospace">
  <div style="padding: 8px 12px; background: #f0f0f0; border-bottom: 1px solid #d0d0d0; font-size: 12px">
    {meta}
  </div>
  <pre style="margin: 0; padding: 12px; font-size: 13px; white-space: pre-wrap; word-break: break-word">{source}</pre>
</body></html>
"#,
        title = escape_html(title),
        meta = escape_html(meta),
        source = escape_html(source),
    )
}

/// Decide what bytes the HTML parser should receive for a fetched main
/// document.
///
/// `data:` URLs carry their own media type, and most of them are **not**
/// markup. `data:text/javascript;base64,…` (and every other script/stylesheet/
/// JSON/`text/*` payload, base64 or percent-encoded) is displayed as inert
/// source: parsing it as HTML would paint the program text as a page of
/// garbage, and evaluating it would be a `javascript:` URL by another name —
/// those the tab refuses by contract. Markup types (`text/html`, xhtml, svg,
/// xml) and every other scheme (http(s), file, about) are handed to the parser
/// exactly as before, and subresources are unaffected: a page may still load
/// `data:text/javascript;base64,…` through `<script src>`, which is where
/// running that program is the page's own decision.
pub fn render_document(fetch: &netmod::DocFetch) -> String {
    if let Some(data) = netmod::parse_data_url(&fetch.url) {
        if data.renders_as_source() {
            let source = data.text();
            debug!(
                "data: URL is {} ({} B of source) — rendering the source view, not parsing it",
                data.mime,
                data.bytes.len()
            );
            return source_view_document(
                &data.mime,
                &format!(
                    "{} · {} · {} bytes · {}",
                    data.mime,
                    if data.base64 {
                        "base64"
                    } else {
                        "percent-encoded"
                    },
                    data.bytes.len(),
                    fetch.url.as_str(),
                ),
                &source,
            );
        }
        debug!("data: URL is {} — parsing as markup", data.mime);
    }
    fetch.body_text()
}

// ---------------------------------------------------------------------------
// Runtime

pub struct Tab {
    /// Latest size/scale from the shell (physical px + device scale).
    width: u32,
    height: u32,
    scale: f32,
    /// Page zoom factor from the shell (`ToTab::SetZoom`, Ctrl-+/Ctrl--/Ctrl-0
    /// chords). CSS layout px map to logical window px through this factor,
    /// so pointer/wheel coordinates are divided by it on the way in.
    zoom: f32,
    visible: bool,
    doc: Option<HtmlDocument>,
    /// A main-document fetch still in flight on the network worker
    /// (task: faster, responsive loading); `None` when nothing is fetching.
    pending_nav: Option<PendingNavigation>,
    /// The container's cookie jar — the very same `Arc<Mutex<..>>` the
    /// Blitz provider mutates for subresources. One jar per container per
    /// tab: main-document `Set-Cookie`s reach subresource requests (and
    /// vice versa), and shutdown persistence can no longer let one handle
    /// overwrite the other's cookies.
    jar: Arc<Mutex<CookieJar>>,
    /// The tab's response cache — the very same `Arc<Mutex<..>>` the Blitz
    /// provider serves subresources from (see
    /// [`BlitzNetProvider::cache_handle`]). One cache per tab: documents,
    /// `<script src>` bodies and images/CSS/fonts all see each other's hits,
    /// validators and LRU bounds.
    cache: Arc<Mutex<NetCache>>,
    /// The container's `localStorage` store (task: per-origin DOM storage):
    /// opened from the profile dir, so every area physically lives inside
    /// this container's files (the sandbox hides them from other
    /// containers). The JS host gets the very same handle — the native
    /// keys every operation by the origin re-resolved from the CURRENT
    /// document URL, so areas never cross origins.
    local_storage: js::SharedLocalStorage,
    provider: Arc<BlitzNetProvider>,
    shell: Arc<ShellHook>,
    nav: Arc<NavHook>,
    /// URL-only session history (per the common IPC contract).
    history: Vec<Url>,
    history_index: usize,
    last_url: Option<String>,
    /// **Reader view** (task: reader mode): the page currently on screen is
    /// the cleaned-up text-and-images rendering, not the real document.
    /// Set by [`Tab::set_reader`]; every real navigation clears it.
    reader_active: bool,
    /// One-shot guard while [`Tab::set_reader`] renders through `load_html`:
    /// the reader document must not overwrite the source it was made from.
    reader_rendering: bool,
    /// The source the current document was **loaded** from — `(url, raw
    /// html)`, recorded by `load_html` outside reader renders. It is the
    /// reader mode fallback (content scripts cleared out of the live DOM
    /// still stands in the bytes) and what toggling reader back off
    /// re-renders (no network, same history entry).
    last_source: Option<(String, String)>,
    /// `history.pushState`/`replaceState` re-pointed the address bar past
    /// the document's own URL (task: infosec.exchange); wins over
    /// `doc.url()` in `check_meta` until the NEXT real navigation.
    url_override: Option<String>,
    /// Delayed navigation armed by the response's `Refresh:` header or a
    /// `<meta http-equiv="refresh">` in the document (task: Anubis
    /// `metarefresh` gates): `(deadline, target)`, followed by the poll loop
    /// once due. Cleared by every real navigation, replaced when the newly
    /// loaded document carries its own directive.
    refresh_at: Option<(Instant, Url)>,
    last_title: Option<Option<String>>,
    /// `document.title` as last assigned by page JavaScript (task 8). Wins over
    /// the `ShellProvider`/`<title>` node sources while the document is live.
    js_title: Option<String>,
    /// Status text raised by the script run (errors, `console.*`), emitted
    /// once per navigation after the load-finished clear.
    js_status: Option<String>,
    /// The document's JavaScript world (task: real timers): Context + host +
    /// live timer queue, alive for the lifetime of the document so callbacks
    /// scheduled by page scripts fire from the poll loop. Rebuilt per
    /// navigation (which cancels the previous document's timers with it).
    js: Option<js::JsRuntime>,
    /// `<link rel=stylesheet>` nodes whose `load` event has already been
    /// dispatched for the current document. Blitz applies a fetched sheet to
    /// the stylist but never fires the DOM event; the Vite preload helper
    /// awaits exactly that event before it calls the dynamic `import()` for a
    /// lazy route chunk, so without this a route's component never mounts.
    link_loads_fired: HashSet<NodeId>,
    /// Monotonic clock feeding `BaseDocument::resolve`'s
    /// `current_time_for_animations`. Blitz applies CSS animations/transitions
    /// at this time; passing a constant 0 froze every `@keyframes`-driven
    /// property at its 0% value — Mastodon's `.status{animation:…fade}`
    /// (`0%{opacity:0}`) left every rendered post at `opacity:0`, so a column
    /// full of fetched statuses painted blank.
    anim_clock: Instant,
    /// Paint state: set when a resolve produced (or may have produced) new
    /// pixels; coalesced into at most ~30 fps of `FromTab::Frame`.
    needs_paint: bool,
    last_paint: Option<Instant>,
    offline_fixtures: Option<PathBuf>,
    /// Shared wake pipe (owned alongside the hooks; poked by ShellProvider /
    /// NetWaker worker threads).
    #[allow(dead_code)]
    pipe: Arc<SelfPipe>,
    /// Which mouse buttons are currently held (ironsurf `PointerKind::Down/Up`
    /// track the transitions); carried in every `BlitzPointerEvent.buttons`.
    mouse_buttons: MouseEventButtons,
    /// A form submission resolved from a submit-button press or an implicit
    /// Enter (task: `<form>`); consumed by `drain_nav_hook` at the end of the
    /// stdin batch, exactly like the blitz `NavigationProvider` hook.
    pending_form: Option<FormSubmission>,
    /// A submit-control press armed on mouse-down and finalised on release
    /// (a JS `click` handler may cancel it; task: `<form>`).
    form_press: Option<FormPress>,
    /// Left-press on a `<label>` (task: DDG anomaly tiles): the bound
    /// checkbox/radio and its checkedness at press time. On release, if
    /// blitz's own (buggy for nested inputs) default left the state
    /// untouched, the tab applies the activation itself.
    label_press: Option<(forms::LabelActivation, Option<bool>)>,
    /// The `<video>` under the cursor at the last right-click (task: context
    /// menu); `ContextAction::ToggleVideo` plays/pauses exactly that element.
    /// Cleared with the document it points into.
    context_video: Option<NodeId>,
    /// Last hover-reported link target (`StatusText`), for change-detection so
    /// a moving mouse only emits when the target actually changes.
    hover_status: Option<String>,
    /// Last reported pointer shape (`CursorChanged`, task: hover cursor);
    /// `None` = nothing reported for the current document yet. Change
    /// detection works exactly like `hover_status`'s.
    cursor_status: Option<CursorIcon>,
    /// Last reported viewport scroll geometry (`FromTab::ScrollState`, task:
    /// scroll bar); `None` = nothing reported for the current document yet.
    /// Change detection (within half a CSS px) works exactly like
    /// `cursor_status`'s: the shell's overlay bar needs the numbers, a page
    /// that sits still needs no messages.
    scroll_report: Option<ScrollState>,
    /// The wheel's glide queue ([`WheelGlide`]); fed by [`Tab::pump_wheel`]
    /// (task: slower and smoother wheel).
    wheel_glide: WheelGlide,
    /// Wheel-travel multiplier from the settings menu's slider
    /// (`ToTab::SetScrollSpeed`): the default damping ([`WHEEL_SPEED`])
    /// scaled, folded into every queued glide. `WHEEL_SPEED` until told.
    wheel_speed: f64,
    /// Popup blocker (task: block popups — a website opening new tabs without a
    /// user interaction). One shared cell: the `window.open` native answers
    /// through it synchronously, real input arms its engagement credit, and the
    /// poll loop drains its queues into `FromTab::OpenLinkInNewTab` (allowed) /
    /// `FromTab::PopupBlocked` (refused). The tab never spawns a tab itself.
    popups: Arc<crate::popup::PopupHook>,
    /// Set by a left release that landed on an `<a target="_blank">`, carrying
    /// the window name the link asked for (task: block popups). blitz knows
    /// nothing about `target`, so that click comes back as a navigation *of
    /// this tab*; the next `drain_nav_hook` turns it into a popup request
    /// instead. Taken unconditionally at the top of every drain, so a mark can
    /// never outlive the click that made it.
    new_window_click: Option<String>,
    /// Video (task: playback): one player per `<video>` element, in attach
    /// order. The blitz widget on the element shares the very same
    /// `Rc<RefCell<MediaPlayer>>`, so the tab pumps the clock and the widget
    /// paints it without ever touching the document. Dropped with its
    /// document on every navigation.
    media: Vec<MediaEntry>,
    /// Resolved hrefs being fetched right now (dedupe: two elements with one
    /// `src` cost one fetch — every waiter gets the bytes when they land).
    media_inflight: HashSet<String>,
    /// Completion queue from the fetch threads; the shared self-pipe wakes
    /// the loop the moment one arrived (the NavHook/NetWaker pattern).
    media_results: Arc<Mutex<Vec<MediaLoaded>>>,
    /// Download ids for this process (task: file downloads). Minted here, echoed
    /// in every `DownloadChunk`/`DownloadEnd`; the shell keys a row by
    /// `(tab_id, id)`, so ids never need to be global.
    download_ids: downloads::DownloadIds,
    /// Worker-thread side of the download feed (task: file downloads): the
    /// fetch thread that settled a download pushes one [`DownloadShipment`] —
    /// chunks already taken out of the fetch buffer — here, the shared
    /// self-pipe wakes the loop, and [`Tab::drain_downloads`] turns it into
    /// `DownloadChunk`/`DownloadEnd` exactly like `drain_archive_events`.
    download_tx: mpsc::Sender<DownloadShipment>,
    download_rx: mpsc::Receiver<DownloadShipment>,
    /// The tab's **effective User-Agent** cell (task: per-container UA): the
    /// `--user-agent` flag seeds it, `ToTab::SetUserAgent` replaces its
    /// contents, and every fetch (documents, subresources, form POSTs,
    /// media, redirect hops) plus `navigator.userAgent` reads this one cell.
    /// Changing it never forces a reload.
    user_agent: netmod::SharedUserAgent,
    /// Archive-mode sink (the shell's 💾 toggle): seeded by `--archive-mode`,
    /// moved by `ToTab::SetArchiveMode`, shared with the net provider, the
    /// script-src fetcher, and the media worker; the poll loop drains it into
    /// `FromTab::ArchivedFetch` messages exactly like `net_events`.
    archive: Arc<crate::archive::ArchiveState>,
    /// DevTools network queue (the provider's own): subresource fetch threads
    /// push one [`NetEvent`] per settled request here, and the poll loop
    /// drains it into `FromTab::NetworkEvent` messages (a NetWaker wake makes
    /// the drain timely). Navigation events are emitted inline (the fetch is
    /// synchronous on the tab thread).
    net_events: Arc<Mutex<Vec<NetEvent>>>,
    /// The fetch threads' DevTools **console** queue (source `"css"` today:
    /// wrong-`Content-Type` stylesheets — see `src/logging.rs`); drained
    /// every poll tick exactly like `net_events`. The private-address rule
    /// prints its warnings here too.
    console_events: ConsoleQueue,
    /// AUDIT F5 — the **private-address rule** (see [`crate::private_addr`]):
    /// one guard per tab, shared with the net provider and with every
    /// document's JS host, so a page's `fetch`, `XMLHttpRequest`, subresource
    /// and `location.href` hop into the local network are all refused by the
    /// same rule — and announced once on the DevTools console.
    private_guard: crate::private_addr::PrivateAddressGuard,
    /// The document the provider's subresources belong to, written by
    /// [`Tab::load_html`]: Blitz gives a `NetProvider` no way to ask which
    /// document made a load, so the private-address rule needs the tab to say.
    doc_cell: Arc<Mutex<Option<Url>>>,
    /// Per-document bookkeeping for the CSS re-parse seam (content hashes of
    /// already-reported `<style>` bodies / `style="…"` attributes).
    css_scan: CssScanState,
    /// **Find in page** (task: find bar, `Ctrl-F`): the live query the shell
    /// installed with [`ToTab::SetFind`] (`""` = none), its hits in reading
    /// order, and the 0-based index of the **active** (light-blue) hit the
    /// shell's counter is on. The query outlives the document: every new page
    /// re-runs it (see [`Tab::find_on_new_document`]).
    find_query: String,
    find_hits: Vec<find::Hit>,
    find_active: Option<usize>,
    /// The document settled (script, a restyle, a resize, a fresh page) since
    /// the hits were collected, so the byte ranges — or the text itself — may
    /// no longer be the ones the search saw. Cleared by the next re-run, which
    /// happens lazily: only when a jump needs the hits or a frame paints them.
    find_stale: bool,
    /// **DevTools Inspector** (task: tab-side inspector): the pane is the
    /// shell's, the document is ours. `true` between [`ToTab::SetInspect`]
    /// open/close, and while it holds, the poll hook [`Tab::inspect_check`]
    /// re-walks the tree and ships only what changed.
    inspect_enabled: bool,
    /// [`ToTab::SetInspectPick`] — the crosshair. While armed the tab swallows
    /// pointers (the page sees nothing), reports [`FromTab::InspectHover`], and
    /// takes a left release as the pick.
    inspect_pick: bool,
    /// What is under the armed crosshair, and what the shell selected in the
    /// tree (an id of **this** generation; `None` = nothing). Both are painted
    /// on the page, both die with the document.
    inspect_hover: Option<u64>,
    inspect_selected: Option<u64>,
    /// The generation stamped on the rows this tab walked last: node ids mean
    /// nothing outside it, because blitz recycles arena indices (see
    /// [`inspector`]).
    inspect_gen: u64,
    /// The blitz node behind each row id of that walk — `inspect_nodes[i]` is
    /// [`ironsurf_common::InspectNode::id`] `i`. This is what makes selecting,
    /// painting the marker and hit-testing cost nothing per frame.
    inspect_nodes: Vec<NodeId>,
    /// The [`inspector::fingerprint`] of the snapshot last shipped, so an idle
    /// document costs a walk and **no** IPC.
    inspect_fp: Option<u64>,
    /// The `<!DOCTYPE …>` blitz's parser throws away, sniffed out of the source
    /// [`load_html`] handed the parser (see [`inspector::doctype_from_source`]).
    /// Part of every row count: the walk and the hit-test have to agree on it,
    /// or the ids the shell holds shift by one.
    inspect_doctype: Option<String>,
    /// Userscripts installed by the shell (`ToTab::SetUserscripts`, task:
    /// userscripts): **replace-wholesale**, in the shell's store order, every
    /// entry enabled (the shell never sends disabled ones). Matching against a
    /// document URL happens at run time via [`userscripts_for_url`], so a
    /// script still applies after JS-driven redirects and an empty set costs
    /// exactly nothing.
    userscripts: Vec<Userscript>,
    /// Master switch mirroring `TabArgs::media_enabled`.
    media_enabled: bool,
    /// When the media steps must next run even with no events at all; `None`
    /// = no live players, and the loop may idle (`media_poll_tick`).
    media_next_pump: Option<Instant>,
    /// The `ironsurf://archive` page this tab is sitting on, when it is one:
    /// the URL as asked for. The tab cannot serve the archive itself (it is
    /// sandboxed and the archive tree belongs to the shell), so rendering the
    /// page emits [`FromTab::ArchiveServerStart`] and the shell's
    /// [`ToTab::ArchiveServerInfo`] is what the page shows.
    archive_page: Option<String>,
    /// The `ironsurf://history` page this tab is sitting on, when it is one (the
    /// URL as asked for, `?clear=1` included). The visit log is the shell's —
    /// one `<data-root>/history.jsonl` shared by every container, outside every
    /// sandbox — so rendering the page emits [`FromTab::HistoryListRequest`] and
    /// the page shows the shell's answer to [`ToTab::HistoryList`].
    history_page: Option<String>,
    /// The listing the history page on screen was **rendered** from (`None` =
    /// the "asking the browser" placeholder). An answer that differs from it is
    /// worth a re-render; one that repeats it is not, which is what stops the
    /// page from reloading itself forever.
    history_rendered: Option<netmod::HistoryListing>,
    /// `true` while the live navigation is one the **user** asked for (typed in
    /// the shell's address bar, Reload). Page script clears it: only a
    /// user-chosen address may ask the shell to re-point the archive server or
    /// open a local file (see [`Tab::ask_archive_server`] and the audit).
    nav_from_shell: bool,
    /// stdout side of the IPC, shared with [`Tab::dialog`] (a modal pumps
    /// stdin while the loop is inside a JS turn, and must still be able to
    /// emit). All writes go through the `RefCell` — never borrowed across a
    /// re-entrant call.
    out: Rc<RefCell<TabOut>>,
    /// `window.alert`/`confirm`/`prompt` host (task: JS dialogs): attached to
    /// every document's [`js::HostState`]; the JS native blocks here until
    /// the shell answers.
    dialog: Arc<DialogHost>,
    /// The raw stdin buffer the poll loop pumps through — shared with
    /// [`DialogHost`], so bytes read during a modal (which may contain the
    /// reply *and* the shell's next commands) are never lost when the modal
    /// returns mid-buffer.
    stdin_pending: Rc<RefCell<Vec<u8>>>,
}

/// The answer one modal dialog got back from the shell. `confirmed` is the
/// button pressed; `text` is the edited value of an accepted prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialogReply {
    pub confirmed: bool,
    pub text: Option<String>,
}

/// The synchronous-dialog host living inside [`js::HostState`] (task: JS
/// dialogs). `window.alert`/`confirm`/`prompt` block the tab's thread right
/// where the script called them — exactly like a real browser's modal — by
/// emitting [`FromTab::DialogRequest`] and pumping the shell's stdin until
/// the matching [`ToTab::DialogResponse`] lands.
///
/// While the modal is up, every *other* `ToTab` message is queued in order
/// (`take_deferred`) and replayed to `handle_to_tab` on the loop's next
/// stdin drain, so a Resize/Key that raced the dialog is applied after it, never dropped.
/// EOF (shell gone) answers the dialog as cancelled instead of hanging; the
/// main loop's own stdin read then sees the same EOF and shuts down.
pub struct DialogHost {
    out: Rc<RefCell<TabOut>>,
    pipe: Arc<SelfPipe>,
    /// The main loop's very own stdin byte buffer (see [`Tab::stdin_pending`]).
    pending: Rc<RefCell<Vec<u8>>>,
    /// Messages swallowed while a modal is open, oldest first.
    deferred: RefCell<VecDeque<ToTab>>,
    next_dialog_id: Cell<u32>,
    /// `--headless` (and any sink-less harness): `ask` answers cancelled
    /// immediately instead of waiting for a reply nobody can send.
    disabled: Cell<bool>,
}

/// Cap the modal poll at a quarter second — stdin wakes the modal instantly
/// via `poll()`; the tick only bounds how long a wedged shell can keep a
/// dead wake pipe spinning the modal.
const DIALOG_POLL_TICK: Duration = Duration::from_millis(250);

impl DialogHost {
    fn new(out: Rc<RefCell<TabOut>>, pipe: Arc<SelfPipe>, pending: Rc<RefCell<Vec<u8>>>) -> Self {
        DialogHost {
            out,
            pipe,
            pending,
            deferred: RefCell::new(VecDeque::new()),
            next_dialog_id: Cell::new(1),
            disabled: Cell::new(false),
        }
    }

    /// Never block on a shell that cannot answer (headless harness).
    fn disable(&self) {
        self.disabled.set(true);
    }

    /// Take every message deferred while a modal was open (oldest first),
    /// ready to prepend to the next stdin batch.
    fn take_deferred(&self) -> VecDeque<ToTab> {
        std::mem::take(&mut *self.deferred.borrow_mut())
    }

    /// Show one modal dialog and block until the shell answers it. The
    /// defaults (no shell, shell died mid-dialog) are the cancelled answer:
    /// `confirm` reads `false`, `prompt` reads `null` — and the tab stays alive.
    pub(crate) fn ask(&self, kind: DialogKind, message: &str, default: Option<&str>) -> DialogReply {
        if self.disabled.get() {
            debug!("dialog {kind:?} with no live shell: answered cancelled");
            return DialogReply::default();
        }
        // `nth` hands back the byte offset of the first dropped char, which
        // is a char boundary by construction.
        let message = match message.char_indices().nth(MAX_DIALOG_MESSAGE) {
            Some((at, _)) => &message[..at],
            None => message,
        };
        let id = self.next_dialog_id.get();
        self.next_dialog_id.set(id.wrapping_add(1));
        info!("page opened a {kind:?} dialog (#{id}): {}", message.trim());
        self.out.borrow_mut().send(FromTab::DialogRequest {
            dialog_id: id,
            kind,
            message: message.to_owned(),
            default: default.map(str::to_owned),
        });
        let stdin_fd = io::stdin().as_raw_fd();
        loop {
            if self.out.borrow().broken {
                warn!("stdout broke while dialog #{id} was open — answering it cancelled");
                return DialogReply::default();
            }
            // Sleep until the shell speaks (stdin), something wakes the loop,
            // or the tick passes — no repainting meanwhile: the shell owns
            // the screen while its modal dialog is up.
            poll_inputs(stdin_fd, self.pipe.read_fd(), DIALOG_POLL_TICK);
            self.pipe.drain();
            let frames = {
                let mut pending = self.pending.borrow_mut();
                match read_stdin_bytes(stdin_fd, &mut pending) {
                    Ok(true) => {
                        // The shell closed the pipe (window quit, crash): a
                        // cancelled dialog beats a hung tab.
                        debug!("stdin EOF while dialog #{id} was open — answering it cancelled");
                        return DialogReply::default();
                    }
                    Ok(false) => decode_stdin_frames(&mut pending),
                    Err(err) => {
                        error!("stdin broke while dialog #{id} was open: {err}");
                        return DialogReply::default();
                    }
                }
            };
            match frames {
                Ok(frames) => {
                    for msg in frames {
                        match msg {
                            ToTab::DialogResponse { dialog_id, confirmed, text }
                                if dialog_id == id =>
                            {
                                debug!(
                                    "dialog #{id} answered: confirmed={confirmed} text={text:?}"
                                );
                                return DialogReply { confirmed, text };
                            }
                            other => self.deferred.borrow_mut().push_back(other),
                        }
                    }
                }
                Err(err) => {
                    error!("stdin protocol error during dialog #{id}: {err}");
                    return DialogReply::default();
                }
            }
        }
    }
}

impl std::fmt::Debug for DialogHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogHost")
            .field("next_dialog_id", &self.next_dialog_id.get())
            .field("disabled", &self.disabled.get())
            .finish_non_exhaustive()
    }
}

/// stdout side of the IPC: one persistent BufWriter, flushed per frame.
/// In headless harness mode the sink is `/dev/null` instead (see
/// [`Tab::new_headless`]) so the process never speaks the protocol.
struct TabOut {
    writer: Box<dyn Write>,
    broken: bool,
}

impl TabOut {
    fn new() -> Self {
        let stdout: StdoutLock<'static> = io::stdout().lock();
        TabOut {
            writer: Box::new(BufWriter::with_capacity(1 << 20, stdout)),
            broken: false,
        }
    }

    /// Discarding sink (headless mode): `send` keeps working, frames go nowhere.
    fn null() -> Self {
        TabOut {
            writer: Box::new(BufWriter::new(io::sink())),
            broken: false,
        }
    }

    fn send(&mut self, msg: FromTab) {
        if write_frame(&mut self.writer, &msg).is_err() {
            // Shell vanished: flag for the loop to shut down.
            self.broken = true;
        }
        if self.writer.flush().is_err() {
            self.broken = true;
        }
    }

    fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

impl Tab {
    /// A tab wired exactly like a live one (same net provider, hooks, jar,
    /// cache, script pipeline) but with the shell sink discarded: used by the
    /// `--headless` ACID-test harness, which navigates once, waits, and dumps
    /// DOM text + a screenshot instead of streaming frames (see
    /// [`crate::headless`]).
    pub(crate) fn new_headless(args: &TabArgs, pipe: Arc<SelfPipe>) -> Self {
        let mut tab = Tab::new(args, pipe);
        tab.out = Rc::new(RefCell::new(TabOut::null()));
        // No shell to answer a modal: dialogs resolve to their cancelled
        // reading instead of blocking the harness.
        tab.dialog.disable();
        tab
    }

    fn new(args: &TabArgs, pipe: Arc<SelfPipe>) -> Self {
        // Task: file downloads — the one queue a fetch worker uses to hand a
        // settled download back to the single-threaded loop (the same
        // self-pipe wakes the loop, so the drain is timely rather than tick-
        // late). An unbounded channel: a download is one message per file.
        let (download_tx, download_rx) = mpsc::channel();
        let store = ContainerStore::from_profile_dir(args.container_id, &args.profile_dir);
        // One DOM-storage store per container per tab (task: per-origin
        // `localStorage`): areas are one JSON file per origin under the
        // profile dir's `local-storage/`.
        let local_storage: js::SharedLocalStorage =
            Arc::new(Mutex::new(store.local_storage()));
        // One cookie jar per container per tab: the provider owns it, and
        // the tab holds the same Arc handle for main-document fetches
        // (cookies flow both ways; a single save flushes everything).
        // The effective UA first: the provider (subresources) and the tab
        // (documents, media, JS) share this one cell from birth.
        let user_agent = netmod::new_user_agent(args.user_agent.as_deref());
        // …and the container's proxy, before any request can leave: every
        // fetch builder in netmod reads this one process cell at send time
        // (task: proxies). `--proxy` was already resolved in TabArgs::parse,
        // so this only ever writes; absent/blank keeps the born-direct cell.
        if let Some(url) = args.proxy.as_deref().filter(|u| !u.trim().is_empty()) {
            if let Err(err) = netmod::set_process_proxy(Some(url)) {
                warn!("proxy not applied ({err}); connecting direct");
            } else {
                info!("proxy → {url}");
            }
        }
        // …and the container's private-browsing switch (task: private
        // browsing), before any request can leave: every fetch builder in
        // netmod reads this one process switch at build time.
        if args.private {
            netmod::set_do_not_track(true);
            info!("private browsing: sending DNT: 1");
        }
        let archive = crate::archive::ArchiveState::shared(args.archive_mode);
        let provider = Arc::new(
            BlitzNetProvider::new(args.net_policy, store.cookie_jar(), store.cache())
                .with_waker(Arc::new(PipeWaker(Arc::clone(&pipe))))
                .with_user_agent(user_agent.clone())
                .with_archive(archive.clone()),
        );
        // Grab the DevTools network queue before `provider` moves into the tab.
        let net_events = provider.net_events();
        // …and the fetch-side console queue with it (css content-type seam).
        let console_events = provider.console_events();
        // …and the private-address rule (AUDIT F5): the provider judges
        // subresources with it, the tab judges page navigations, and every
        // document's JS host judges `fetch`/XHR — one guard, one console, one
        // "warn once per origin pair" set.
        let private_guard = provider.private_guard();
        let doc_cell = provider.document_cell();
        let out = Rc::new(RefCell::new(TabOut::new()));
        let stdin_pending = Rc::new(RefCell::new(Vec::new()));
        let dialog = Arc::new(DialogHost::new(
            Rc::clone(&out),
            Arc::clone(&pipe),
            Rc::clone(&stdin_pending),
        ));
        Tab {
            out: Rc::clone(&out),
            dialog,
            stdin_pending,
            width: DEFAULT_SIZE.0,
            height: DEFAULT_SIZE.1,
            scale: 1.0,
            zoom: 1.0,
            visible: true,
            doc: None,
            jar: provider.jar(),
            // …and the same single cache the provider serves subresources
            // from, taken from the provider itself (never a second copy).
            cache: provider.cache_handle(),
            local_storage,
            provider,
            shell: Arc::new(ShellHook {
                pipe: Arc::clone(&pipe),
                title: Mutex::new(None),
                clipboard: Mutex::new(String::new()),
                copied: Mutex::new(None),
            }),
            nav: Arc::new(NavHook {
                pipe: Arc::clone(&pipe),
                pending: Mutex::new(None),
            }),
            // The popup gate wakes this very loop, so an allowed popup reaches
            // the shell the moment the page asked for it rather than a tick
            // later (the NetWaker/NavHook pattern).
            popups: Arc::new(crate::popup::PopupHook::with_pipe(Arc::clone(&pipe))),
            new_window_click: None,
            history: Vec::new(),
            history_index: 0,
            last_title: None,
            last_url: None,
            reader_active: false,
            reader_rendering: false,
            last_source: None,
            url_override: None,
            refresh_at: None,
            js_title: None,
            js_status: None,
            js: None,
            link_loads_fired: HashSet::new(),
            anim_clock: Instant::now(),
            needs_paint: false,
            last_paint: None,
            offline_fixtures: args.offline_fixtures.clone(),
            pipe,
            mouse_buttons: MouseEventButtons::None,
            hover_status: None,
            cursor_status: None,
            scroll_report: None,
            pending_nav: None,
            wheel_glide: WheelGlide {
                pending: (0.0, 0.0),
                last: Instant::now(),
                next: Instant::now(),
                at: None,
                over: (0.0, 0.0),
            },
            wheel_speed: WHEEL_SPEED,
            pending_form: None,
            form_press: None,
            label_press: None,
            context_video: None,
            media: Vec::new(),
            media_inflight: HashSet::new(),
            media_results: Arc::new(Mutex::new(Vec::new())),
            download_ids: downloads::DownloadIds::new(),
            download_tx: download_tx,
            download_rx: download_rx,
            media_enabled: args.media_enabled,
            media_next_pump: None,
            archive_page: None,
            history_page: None,
            history_rendered: None,
            nav_from_shell: false,
            net_events,
            console_events,
            private_guard,
            doc_cell,
            css_scan: CssScanState::new(),
            find_query: String::new(),
            find_hits: Vec::new(),
            find_active: None,
            find_stale: false,
            inspect_enabled: false,
            inspect_pick: false,
            inspect_hover: None,
            inspect_selected: None,
            inspect_gen: 0,
            inspect_nodes: Vec::new(),
            inspect_fp: None,
            inspect_doctype: None,
            userscripts: Vec::new(),
            user_agent,
            archive,
        }
    }

    /// The single tab→shell choke point. Every message is logged except frames,
    /// which are `trace`-only because they fire up to 30× a second.
    fn emit(&mut self, msg: FromTab) {
        match &msg {
            FromTab::Frame {
                width,
                height,
                scale,
                rgba,
            } => trace!(
                "frame {}x{}@{scale:.2} ({} B rgba)",
                width,
                height,
                rgba.len()
            ),
            FromTab::UrlChanged { url } => {
                info!("url → {}", url.as_deref().unwrap_or("<none>"))
            }
            FromTab::TitleChanged { title } => {
                info!("title → {}", title.as_deref().unwrap_or("<none>"))
            }
            FromTab::LoadingChanged { loading } => {
                info!("loading {}", if *loading { "started" } else { "finished" })
            }
            FromTab::Error { message } => error!("to shell: {message}"),
            FromTab::ConsoleLog { source, level, message } => {
                info!("to shell: console[{source}:{level}] {message}")
            }
            FromTab::NetworkEvent { event } => {
                debug!(
                    "to shell: net {} {} {}",
                    event.method,
                    event.url,
                    event
                        .status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| event.error.clone().unwrap_or_default())
                )
            }
            other => debug!("to shell: {other:?}"),
        }
        self.out.borrow_mut().send(msg);
    }

    fn viewport(&self) -> Viewport {
        let mut viewport = Viewport::new(
            (self.width as f32 * self.scale) as u32,
            (self.height as f32 * self.scale) as u32,
            self.scale,
            ColorScheme::Light,
        );
        // blitz multiplies hidpi × zoom into its total scale; every new
        // document (and every resize) inherits the tab's current zoom.
        viewport.set_zoom(self.zoom.max(0.0).max(f32::EPSILON));
        viewport
    }

    /// The zoom factor in a safe form for coordinate math (never 0/NaN).
    fn zoom(&self) -> f32 {
        if self.zoom.is_finite() && self.zoom > 0.0 {
            self.zoom
        } else {
            1.0
        }
    }

    fn emit_history_changed(&mut self) {
        let can_back = self.history_index > 0;
        let can_fwd = self.history_index + 1 < self.history.len();
        self.emit(FromTab::HistoryChanged {
            can_go_back: can_back,
            can_go_forward: can_fwd,
        });
    }

    // -- navigation -------------------------------------------------------

    /// Navigate to `raw` (shell text). `history` says what to do with the
    /// URL-only session history (fresh loads truncate forward entries).
    pub fn navigate(&mut self, raw: &str, history: NavHistory) -> bool {
        // Shell text: the address bar (or the shell standing in for the user).
        // This is the one navigation allowed to open a local file or to carry
        // the archive server's `?root=`/`?host=`/`?port=` overrides.
        self.nav_from_shell = true;
        self.navigate_post(raw, history, None)
    }

    /// Render `ironsurf://archive`: ask the shell for its archive browsing
    /// server, honouring the page's query (`?root=…`, `?host=…`, `?port=…`
    /// override the shell's configured values; `?open=1` means "start it and
    /// open the served folder in a **new tab**, leaving this page alone", and
    /// without it this tab is sent to the server's own control page once the
    /// address is known).
    fn ask_archive_server(&mut self, url: &Url) {
        let query = url.query().unwrap_or("").to_string();
        let field = |name: &str| {
            query
                .split('&')
                .rev()
                .find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key == name && !value.trim().is_empty()).then(|| value.to_string())
                })
                // The query value arrives percent-encoded (a folder path has
                // nothing else to be); decode once so the shell reads plain
                // text. The shell's own path resolution is the safety net.
                .map(|v| {
                    String::from_utf8_lossy(&netmod::percent_decode(v.as_bytes()))
                        .into_owned()
                })
        };
        let port = field("port")
            .as_deref()
            .map(|text| text.trim().parse::<u16>().ok())
            .flatten()
            .filter(|p| *p != 0);
        let open = field("open").is_some();
        self.archive_page = Some(url.as_str().to_string());
        // AUDIT F2: the query is *page-controlled text*. A remote page that
        // navigates itself to `ironsurf://archive?root=%2Fetc&host=0.0.0.0`
        // used to have the shell export any folder on any interface with no
        // confirmation. Overrides are honoured for the address bar alone — the
        // documented way a user aims the server — and silently dropped for
        // anything a page arranged.
        let from_shell = self.nav_from_shell;
        if !from_shell {
            let asked = field("root").is_some() || field("host").is_some() || port.is_some();
            if asked {
                warn!("archive server: ignoring root/host/port carried by a page URL");
            }
        }
        self.emit(FromTab::ArchiveServerStart {
            root: from_shell.then(|| field("root")).flatten(),
            host: from_shell.then(|| field("host")).flatten(),
            port: from_shell.then_some(port).flatten(),
            open_in_new_tab: open,
        });
    }

    /// [`Self::navigate`] carrying an HTML form submission's POST body (task:
    /// `<form>`) — the main-document fetch sends a real POST with the
    /// enctype's `Content-Type` instead of a body-less GET.
    pub fn navigate_post(
        &mut self,
        raw: &str,
        history: NavHistory,
        post: Option<&MainDocPost>,
    ) -> bool {
        let started = Instant::now();
        info!(
            "navigate \"{raw}\" ({history:?}{})",
            if post.is_some() { ", form POST" } else { "" }
        );
        // An `<a download>` click (task: file downloads) is a *file promise*,
        // settled before anything is fetched or touched: the page on screen
        // keeps painting, no history entry appears and no URL/title changes.
        // The vendored blitz-dom click handler records the attribute against
        // the URL that click resolved to, and taking it always clears the slot
        // — a typed address, `location.href = …` or a form POST can never
        // inherit an attribute from a link nobody clicked. A bare `download`
        // (or an empty one) suggests no name but still means "save it".
        let anchor = blitz_dom::take_anchor_download(raw);
        if let Some(suggested) = anchor.filter(|_| post.is_none()) {
            let target = Url::parse(raw).ok();
            let problem = target
                .as_ref()
                .and_then(|url| download_target_problem(url));
            match (target, problem) {
                (Some(target), None) => self.start_download(
                    target,
                    suggested,
                    DownloadTrigger::AnchorAttribute,
                    None,
                ),
                (Some(target), Some(reason)) => self.refuse_download(
                    target.as_str(),
                    suggested,
                    DownloadTrigger::AnchorAttribute,
                    reason,
                ),
                // A `javascript:` program inside a download link is refused
                // like any other target the tab cannot fetch, so the shell's
                // row settles instead of hanging on a promise.
                (None, _) => self.refuse_download(
                    raw,
                    suggested,
                    DownloadTrigger::AnchorAttribute,
                    "that address is not a URL the tab can fetch".to_owned(),
                ),
            }
            return false;
        }
        // A real navigation owns the URL again: any pushState address-bar
        // override from the outgoing document dies with it.
        self.url_override = None;
        // …and reader view (task: reader mode) dies with it: the mode is a
        // rendering of one document, not a place navigation keeps.
        if self.reader_active {
            self.reader_active = false;
            self.emit(FromTab::ReaderState { enabled: false });
        }
        // …and so does any delayed navigation the outgoing document armed
        // (its `Refresh:` header / meta refresh). A fresh one is armed below,
        // after the new document is loaded.
        self.refresh_at = None;
        // `javascript:` is never fetched: the text after the scheme is a
        // program for the current page's JS world (address bar, `<a href>`,
        // or a page's own `location.href = "javascript:…"`). Checked on the
        // raw text, before parsing: a URL-bar program contains whitespace,
        // which `Url::parse`/`parse_input_url` would reject.
        if let Some(code) = javascript_url_code(raw) {
            // (Session history is left alone: a `javascript:` program never
            // becomes a history entry.)
            return self.run_javascript_url(&code);
        }
        let mut url = match parse_input_url(raw) {
            Ok(url) => url,
            Err(err) => {
                warn!("bad url {raw:?}: {err}");
                self.emit(FromTab::Error {
                    message: format!("bad url {raw:?}: {err}"),
                });
                return false;
            }
        };
        if history == NavHistory::Fresh {
            self.history.truncate(self.history_index + 1);
        }

        // `ironsurf://archive`: the page is rendered locally like every other
        // internal page, but it describes a server the **shell** owns — tabs
        // bind no sockets and read no archive tree — so ask for it now. The
        // reply (`ToTab::ArchiveServerInfo`) is rendered by the next visit.
        if netmod::is_archive_page(&url) {
            self.ask_archive_server(&url);
        }

        // `ironsurf://history`: rendered locally like every other internal page,
        // but from a log this process may not even read (it belongs to the
        // shell), so ask for it now; the answer arrives as [`ToTab::HistoryList`]
        // and a page that has nothing to show re-renders when it lands. What the
        // fetch below will paint is the listing remembered *right now*.
        if netmod::is_history_page(&url) {
            let clear = url
                .query_pairs()
                .any(|(key, value)| key == "clear" && value != "0");
            // The query is a one-shot instruction (the row clicked was "Clear
            // history"), not part of the page's address: drop it, so the bar
            // reads plain `ironsurf://history` and a **Reload** re-reads the log
            // instead of clearing it a second time.
            if clear {
                url.set_query(None);
            }
            self.history_page = Some(url.as_str().to_string());
            self.history_rendered = netmod::history_listing();
            self.emit(FromTab::HistoryListRequest { clear });
        } else {
            self.history_page = None;
        }

        self.emit(FromTab::LoadingChanged { loading: true });
        self.emit(FromTab::StatusText {
            // Host when we have one; otherwise the URL itself, with `data:`
            // payloads reduced to a length so a multi-MB inline asset cannot
            // spray the status bar.
            text: Some(format!(
                "Waiting for {}",
                if url.scheme() == netmod::INTERNAL_SCHEME {
                    // `ironsurf://welcome` has an opaque host ("welcome"); the
                    // full URL is the meaningful thing to show.
                    netmod::url_for_log(&url)
                } else {
                    url.host_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| netmod::url_for_log(&url))
                }
            )),
        });

        let url = self.apply_offline_fixtures(url);
        debug!(
            "fetching main document {url} (net policy {:?})",
            self_provider_policy(&self.provider)
        );
        // Task: faster, responsive loading — an `http(s)` document must not
        // stall the tab while its bytes cross the network: the fetch (the
        // very exchange the synchronous path below performs) moves onto its
        // own worker thread, and the loop keeps ticking — the page still on
        // screen stays scrollable, clickable and animating, and other
        // documents' subresources keep flowing — until the poll loop picks
        // the result up and runs the same adoption tail a sync load had.
        if matches!(url.scheme(), "http" | "https") {
            let spawned = netmod::spawn_document_fetch_post_report_shared(
                url.clone(),
                post.cloned(),
                self_provider_policy(&self.provider),
                Arc::clone(&self.jar),
                Arc::clone(&self.cache),
                netmod::now_unix(),
                Arc::clone(&self.user_agent),
            );
            match spawned {
                Ok(rx) => {
                    self.pending_nav = Some(PendingNavigation {
                        rx,
                        url,
                        history,
                        started,
                    });
                    return true;
                }
                Err(err) => {
                    return self.finish_main_doc_fetch(&url, history, Err(err), started)
                }
            }
        }
        // Local schemes (`file:`/`data:`/`about:`/`ironsurf:`) answer
        // instantly — their fetch is a read(), so it stays on-thread.
        // DevTools network reporting: the document fetch runs synchronously
        // on the tab thread, so its events (one per completed/failed request,
        // one per redirect hop) queue locally and ship right after the fetch
        // — before any subresource event, exactly like Firefox's panel.
        let doc_events = Mutex::new(Vec::<NetEvent>::new());
        // Shared handles, locked per cookie/cache operation only: the
        // navigation never blocks the subresources the provider is fetching
        // for the same page (and is not blocked by them).
        let fetch = netmod::fetch_document_post_report_shared(
            &url,
            post,
            self_provider_policy(&self.provider),
            &self.jar,
            &self.cache,
            netmod::now_unix(),
            &self.user_agent,
            Some(&|mut event: NetEvent| {
                event.initiator = Some("document".to_owned());
                doc_events
                    .lock()
                    .expect("doc net events mutex")
                    .push(event)
            }),
        );
        for event in doc_events.lock().expect("doc net events mutex").drain(..) {
            self.emit(FromTab::NetworkEvent { event });
        }
        self.finish_main_doc_fetch(&url, history, fetch, started)
    }

    /// The adoption tail of a main-document fetch: the bytes (or the block
    /// page) become the new document, scripts run, and the load bookkeeping
    /// ships. Reached when an on-thread fetch returned (`file:` and friends)
    /// or when the poll loop collected an `http(s)` worker result (task:
    /// faster, responsive loading).
    fn finish_main_doc_fetch(
        &mut self,
        url: &Url,
        history: NavHistory,
        fetch: Result<netmod::DocFetch, netmod::NetError>,
        started: Instant,
    ) -> bool {
        match fetch {
            Ok(mut doc_fetch) => {
                debug!(
                    "document fetched: {} status {} ({} B) in {} ms",
                    doc_fetch.url,
                    doc_fetch.status,
                    doc_fetch.body.len(),
                    started.elapsed().as_millis()
                );
                // Task: file downloads — a top-level response the server wants
                // saved instead of painted (`Content-Disposition: attachment`,
                // whatever its media type) or built of a media type no page is
                // made of. The bytes go to the shell and the document that was
                // on screen stays loaded: no history, no archive document, no
                // `check_meta`, no URL/title change. A form `POST` that answers
                // an attachment is a download too — the page stays put.
                let trigger = downloads::attachment_for(
                    doc_fetch.content_type.as_deref(),
                    doc_fetch.content_disposition.as_deref(),
                    // The anchor slot was taken at the top of this function, so
                    // nothing else suggests itself here.
                    None,
                );
                if let Some((trigger, suggested)) = trigger {
                    let hint = self.download_hint(
                        doc_fetch.url.as_str(),
                        suggested,
                        trigger,
                        doc_fetch.content_type.clone(),
                    );
                    // The body is already in hand: hand its chunks over by
                    // taking them out, so a big file is never mapped twice in
                    // RAM at once.
                    self.emit_download(hint, &mut doc_fetch.body);
                    // Unwind the "Waiting for…" the navigation set; the shell
                    // keeps showing the page that is loaded.
                    self.emit(FromTab::LoadingChanged { loading: false });
                    self.emit(FromTab::StatusText { text: None });
                    return false;
                }
                // Archive mode: the served document rides to the shell as
                // `FromTab::ArchivedFetch` (documents become the host's
                // `index.html`); error statuses are not "visited pages".
                if doc_fetch.status < 400 {
                    self.archive
                        .store(&doc_fetch.url, true, &doc_fetch.body);
                }
                let html = render_document(&doc_fetch);
                self.load_html(&doc_fetch.url, &html, history);
                // Arm a `Refresh:`-header / meta-refresh navigation (task:
                // Anubis `metarefresh` gates) after the new document exists,
                // so a script-added `<meta http-equiv=refresh>` is honoured
                // too. Header first (it wins when both are present).
                self.schedule_refresh(doc_fetch.refresh.clone());
                info!(
                    "loaded {} in {} ms",
                    doc_fetch.url,
                    started.elapsed().as_millis()
                );
                self.emit(FromTab::LoadingChanged { loading: false });
                self.emit(FromTab::StatusText { text: None });
                // Script output outranks the "load finished" clear (task 8).
                self.emit_js_status();
                // External `<script src>` fetches settled during load_html;
                // ship their network events with the navigation, not a tick late.
                self.drain_net_events();
                self.drain_archive_events();
                self.drain_console_events();
                true
            }
            Err(err) => {
                // Fetch failure: report, keep the previous document on screen.
                match err.error_page() {
                    Some(page) => {
                        // Blocked https-upgrade: render the block page at the
                        // requested URL (it stays in history as the user typed it).
                        warn!("navigation blocked by policy: {err}");
                        self.load_html(&url, &page, history);
                        self.emit(FromTab::LoadingChanged { loading: false });
                        self.emit_js_status();
                        false
                    }
                    None => {
                        error!(
                            "navigation to {url} failed after {} ms: {err}",
                            started.elapsed().as_millis()
                        );
                        self.emit(FromTab::Error {
                            message: err.to_string(),
                        });
                        self.emit(FromTab::LoadingChanged { loading: false });
                        self.emit(FromTab::StatusText { text: None });
                        false
                    }
                }
            }
        }
    }

    /// A main-document fetch that returned from the network worker: its
    /// DevTools events ship first, then the document is adopted. The poll
    /// loop calls this every cycle; while the bytes are still arriving the
    /// `try_recv` is empty and this returns immediately (task: faster,
    /// responsive loading).
    fn drain_pending_navigation(&mut self) {
        let arrived = match self.pending_nav.as_ref() {
            None => return,
            Some(p) => match p.rx.try_recv() {
                Ok(v) => Some(v),
                Err(mpsc::TryRecvError::Disconnected) => Some((
                    Err(netmod::NetError::Transport {
                        url: p.url.clone(),
                        message: "document fetch worker died".to_owned(),
                    }),
                    Vec::new(),
                )),
                Err(mpsc::TryRecvError::Empty) => None,
            },
        };
        let Some((res, events)) = arrived else { return };
        let Some(p) = self.pending_nav.take() else { return };
        for event in events {
            self.emit(FromTab::NetworkEvent { event });
        }
        self.finish_main_doc_fetch(&p.url, p.history, res, p.started);
    }

    /// Drive a main-document fetch to completion (headless harness and
    /// tests — places where no poll loop is turning). Returns false if the
    /// navigation is still in flight when the timeout expires.
    pub fn wait_for_navigation(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.pending_nav.is_some() {
            self.drain_pending_navigation();
            if self.pending_nav.is_some() {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        true
    }

    /// While a main-document fetch is in flight, keep the poll tick short so
    /// its result is adopted promptly (task: faster, responsive loading).
    fn nav_poll_tick(&self, base: Duration) -> Duration {
        if self.pending_nav.is_some() {
            base.min(Duration::from_millis(16))
        } else {
            base
        }
    }

    /// `--offline-fixtures DIR` semantics: a `file://` navigation whose path
    /// does not exist resolves against the fixture dir by basename (e.g. the
    /// shell gives `file:///index.html` for fixtures served under `DIR`).
    fn apply_offline_fixtures(&self, url: Url) -> Url {
        let Some(dir) = self.offline_fixtures.as_ref() else {
            return url;
        };
        if url.scheme() != "file" || url.to_file_path().map(|p| p.exists()).unwrap_or(false) {
            return url;
        }
        let name = std::path::Path::new(url.path())
            .file_name()
            .map(|n| n.to_owned())
            .filter(|n| n != "/" && !n.as_os_str().is_empty());
        match name {
            Some(name) => {
                let candidate = dir.join(name);
                if candidate.exists() {
                    Url::from_file_path(candidate).unwrap_or(url)
                } else {
                    url
                }
            }
            None => url,
        }
    }

    /// Build a fresh `HtmlDocument` for an already-parsed document, run its
    /// scripts, drive it to a first paint; then emit the metadata deltas
    /// (url/title/history).
    /// **Reader view** (task: reader mode, Ctrl-Alt-`R` / the 📄 button):
    /// swap the live document for a cleaned-up rendering of it as text and
    /// images only — or swap the real document back.
    ///
    /// Not a navigation: the reader document replaces the current history
    /// entry at the same address, and the source is the **live DOM** at the
    /// moment of the request — everything JavaScript and XHR had loaded in —
    /// with the fetched bytes as the fallback when scripts had *cleared*
    /// richer content out of the DOM (see [`reader::extract_best`]).
    /// Toggling off re-renders the remembered source with no network, so the
    /// page's scripts run again. Every real navigation leaves reader mode
    /// (see [`navigate_post`](Tab::navigate_post)); the shell gets the
    /// authoritative state as [`FromTab::ReaderState`] either way, including
    /// the refusal when the page holds nothing readable.
    fn set_reader(&mut self, enabled: bool) {
        if enabled == self.reader_active {
            // already in (or out of) the requested view: just confirm it.
            self.emit(FromTab::ReaderState { enabled: self.reader_active });
            return;
        }
        if !enabled {
            self.reader_active = false;
            // The real document comes back from the remembered source: same
            // URL, same history entry, scripts run again.
            if let (Some((src_url, src)), false) = (self.last_source.clone(), self.history.is_empty())
            {
                if let Ok(url) = Url::parse(&src_url) {
                    let idx = self.history_index.min(self.history.len() - 1);
                    self.reader_rendering = true;
                    self.load_html(&url, &src, NavHistory::Session(idx));
                    self.reader_rendering = false;
                }
            }
            self.emit(FromTab::ReaderState { enabled: false });
            return;
        }
        // On: the live DOM is the source of truth, the fetched bytes the
        // richer candidate (a failing client-side app can empty the page).
        let live = self.doc.as_deref();
        let source = self
            .last_source
            .as_ref()
            .map(|(u, s)| (u.as_str(), s.as_str()));
        let extracted = reader::extract_best(live, source);
        let (Some(rd), Some(url)) = (extracted, self.history.get(self.history_index).cloned())
        else {
            self.emit(FromTab::StatusText {
                text: Some("reader mode: nothing readable found on this page".to_string()),
            });
            self.emit(FromTab::ReaderState { enabled: false });
            return;
        };
        self.reader_active = true;
        self.reader_rendering = true;
        self.load_html(&url, &rd.html, NavHistory::Session(self.history_index));
        self.reader_rendering = false;
        info!(
            "reader mode on for {url}: {} text chars, {} images",
            rd.text_len, rd.images
        );
        self.emit(FromTab::ReaderState { enabled: true });
    }

    fn load_html(&mut self, url: &Url, html: &str, history: NavHistory) {
        // A fresh doc must not inherit the previous doc's flushed title —
        // and the previous doc's JS world dies here too: replacing `self.js`
        // drops the old Context and every timer queued on it.
        *self.shell.title.lock().expect("title mutex") = None;
        self.js_title = None;
        self.js_status = None;
        // Popup blocker (task: block popups): requests are judged against *this*
        // document's origin, and the previous page's user gesture does not carry
        // over — the click that brought the user here belongs to the page they
        // left.
        self.popups.set_document(url);
        // AUDIT F5: from here on, every subresource the provider loads belongs
        // to this document — that is the origin the private-address rule
        // compares a local destination against.
        *self.doc_cell.lock().expect("document cell mutex") = Some(url.clone());
        // The new document has nothing under the cursor yet; the next hover
        // transition must be allowed to report again.
        self.hover_status = None;
        // …same for the pointer shape, so a stale hand from the old document
        // gets cleared by the next pointer event (task: hover cursor).
        self.cursor_status = None;
        // …and no right-click into it survives either (task: context menu).
        self.context_video = None;
        // …nor the scroll geometry: the new document reports its own range and
        // offset with the next poll step whatever the old one had said (task:
        // scroll bar).
        self.scroll_report = None;
        // …and an unfinished wheel glide belongs to the document it scrolled:
        // the new page starts at its own top, not halfway down a gesture that
        // was travelling through the old one (task: slower and smoother wheel).
        self.wheel_glide.pending = (0.0, 0.0);
        // The previous document's players die with it (their widgets die with
        // its node tree; their decode threads end when the players drop).
        self.reset_media();
        let config = DocumentConfig {
            base_url: Some(url.as_str().to_owned()),
            viewport: Some(self.viewport()),
            net_provider: Some(
                Arc::clone(&self.provider) as Arc<dyn blitz_traits::net::NetProvider>
            ),
            navigation_provider: Some(
                Arc::clone(&self.nav) as Arc<dyn blitz_traits::navigation::NavigationProvider>
            ),
            shell_provider: Some(
                Arc::clone(&self.shell) as Arc<dyn blitz_traits::shell::ShellProvider>
            ),
            // blitz's default UA sheet (blitz-dom `assets/default.css`) has no
            // `input[type=hidden] { display: none }`, so hidden controls got real
            // layout boxes: inside a flexed `<form>` they ate every pixel and the
            // query field collapsed to width 0 — startpage.com's search bar was
            // painted by its container but unhittable/unclickable. Feed blitz's
            // own DEFAULT_CSS back plus this one-rule patch (config
            // `ua_stylesheets` *replaces* the default when `Some`, so it goes
            // first); the values still submit — forms.rs reads attributes, not
            // boxes (task: startpage.com search bar).
            ua_stylesheets: Some(vec![
                blitz_dom::DEFAULT_CSS.to_owned(),
                forms::UA_PATCH_CSS.to_owned(),
            ]),
            ..Default::default()
        };
        // Remember what this document was loaded from (task: reader mode) —
        // except when the reader itself is re-rendering: then the recorded
        // source (what the reader was built from) must survive the swap.
        if !self.reader_rendering {
            self.last_source = Some((url.as_str().to_owned(), html.to_owned()));
        }
        let parsed = Instant::now();
        let mut document = HtmlDocument::from_html(html, config);
        // `<form>` groundwork (task: `<form>`): seed textarea editors from
        // their child text and the selected/value state of every select,
        // before the first layout freezes those in.
        forms::prepare_document(&mut document);
        self.doc = Some(document);
        self.settle();
        debug!(
            "document parsed+settled for {url} ({} B html) in {} ms",
            html.len(),
            parsed.elapsed().as_millis()
        );
        // Task: faster, responsive loading — show content before scripts
        // finish: the document is parsed, its CSS settled (stylesheets,
        // webfonts) and laid out, so paint that first and the user sees the
        // page while a script-heavy document is still coming alive — boa can
        // spend seconds in `run_scripts`, and the pre-script frame is the
        // difference between "loading" and "frozen". Anything a script then
        // mutates queues a repaint through the Shell provider's
        // invalidation, so the post-script state follows immediately after.
        self.needs_paint = true;
        self.last_paint = None;
        self.maybe_repaint();
        // JavaScript runs on the settled DOM (ARCHITECTURE §JavaScript (Boa)).
        self.run_scripts(url);
        // Video (task: playback): every `<video>` gets its widget and its
        // fetch before the first frame; ones scripts create later are picked
        // up by the poll loop's media cadence (see `pump_media`).
        self.attach_video_widgets();

        match history {
            NavHistory::Fresh => {
                // Re-visiting an entry jumps to it; a new URL is pushed and
                // becomes current (previously the index stayed behind, which
                // left Back dead after shell-driven navigations).
                match self.history.iter().position(|h| h == url) {
                    Some(pos) => self.history_index = pos,
                    None => {
                        self.history.push(url.clone());
                        self.history_index = self.history.len() - 1;
                    }
                }
            }
            NavHistory::Session(session) => {
                self.history_index = session;
                self.history[session] = url.clone(); // redirects replace entries
            }
        }
        self.emit_history_changed();

        // A find bar left open across this navigation keeps searching: re-run
        // the shell's live query against the new document (it emits the new
        // counts, and the frame below already paints the highlights).
        self.find_on_new_document();

        // The inspector's row ids died with the previous document, and the
        // doctype this source carries is part of the next walk's row count.
        self.inspect_doctype = inspector::doctype_from_source(html);
        self.inspect_on_new_document();

        // Force a first frame for the new document (bypasses the throttle).
        self.needs_paint = true;
        self.last_paint = None;
        self.maybe_repaint();
        self.check_meta();
    }

    /// resolve → (drain pending criticals via messages) → resolve, as the
    /// canonical headless flow prescribes (`docs/BLITZ_API_NOTES.md` §1/§3).
    fn settle(&mut self) {
        // Restyling/relaying can rewrite the laid-out text the hits point into
        // (script, a lazy load, an animation), so the next use re-runs the
        // query instead of trusting stale byte ranges. Cheap when idle: only a
        // live query costs anything, and only once per settle.
        self.find_stale = true;
        let now = self.anim_clock.elapsed().as_secs_f64();
        {
            let Some(doc) = self.doc.as_mut() else { return };
            doc.resolve(now);
            for _ in 0..8 {
                if !doc.has_pending_critical_resources() {
                    break;
                }
                doc.handle_messages();
                doc.resolve(now);
            }
            doc.handle_messages();
            doc.resolve(now);
        }
        // A stylesheet that arrived in this settle has its `load` event owed
        // to the page (blitz applies the sheet but never dispatches the DOM
        // event — see `dispatch_stylesheet_loads`).
        self.dispatch_stylesheet_loads();
    }

    /// Fire the `load` event on every `<link rel=stylesheet>` whose sheet
    /// blitz has applied since the last settle. Blitz's resource plumbing
    /// stops at `add_stylesheet_for_node` (it marks the node's
    /// `SpecialElementData::Stylesheet`) and never dispatches a DOM event, so
    /// the ledger here is the browser side of the contract. The Vite preload
    /// helper (`__vitePreload`) resolves its CSS dependency promises on
    /// exactly this event and only then calls the route chunk's dynamic
    /// `import()`: without it a lazy route stays on its loading spinner
    /// forever even though every byte was fetched.
    fn dispatch_stylesheet_loads(&mut self) {
        if self.js.is_none() {
            return;
        }
        let ready: Vec<NodeId> = match self.doc.as_ref() {
            Some(doc) => doc
                .tree()
                .iter()
                .filter(|(id, node)| {
                    !self.link_loads_fired.contains(id)
                        && node.element_data().is_some_and(|el| {
                            el.name.local.as_ref() == "link"
                                && matches!(
                                    el.special_data,
                                    blitz_dom::node::SpecialElementData::Stylesheet(_)
                                )
                        })
                })
                .map(|(id, _)| id)
                .collect(),
            None => return,
        };
        for id in ready {
            self.link_loads_fired.insert(id);
            debug!("stylesheet loaded: firing `load` on link node {id:?}");
            let js = self.js.as_mut();
            let doc = self.doc.as_mut();
            if let (Some(js), Some(doc)) = (js, doc) {
                js.fire_event(doc, Some(id), "load", "{}");
            }
        }
    }

    fn resolve_and_maybe_paint(&mut self) {
        if self.doc.is_none() {
            return;
        }
        self.settle();
        self.maybe_repaint();
        self.check_meta();
    }

    // -- javascript ---------------------------------------------------------

    /// Run the freshly parsed document's `<script>`s (task 8), keeping the
    /// Boa context alive in [`Tab::js`] for the lifetime of the document so
    /// timers scheduled by those scripts fire later from the poll loop.
    ///
    /// Scripts are collected in document order; `src=` bodies go through the
    /// tab's own document fetcher, so the container net policy (cookies, cache,
    /// https-upgrade) applies to them too. The external bodies are fetched
    /// CONCURRENTLY on a small worker pool over the shared client (capped, NOT
    /// one thread per URL — an unbounded fan-out is EAGAIN on a page with many
    /// scripts, which is exactly what killed tab 4 on startpage.com) so the
    /// screen is not blocked for the sum of the script round-trips, but they
    /// are still assembled — and therefore executed — strictly in document
    /// order. Nothing here can kill the tab: extraction failures degrade to
    /// "no scripts", thread-spawn failures degrade to fewer workers (the tab
    /// thread fetches the rest itself), and every `eval` is caught inside
    /// [`js::JsRuntime::run_scripts`].
    /// Fetch one `<script src>` (or ES-module) body on cloned shared handles:
    /// the jar/cache mutexes are locked **per cookie/cache operation** (via
    /// the `JarAccess`/`CacheAccess` adapters `fetch_document_*_report_shared`
    /// takes), so parallel script threads never serialise on each other, and
    /// every body reports into the tab's DevTools queue and the archive sink
    /// exactly like the old sequential fetch did. Also handed to the
    /// [`js::UrlModuleLoader`] (task: `<script type="module">` loading), whose
    /// lazy graph fetches run on the tab thread during evaluation.
    fn fetch_script_body(
        script_url: &Url,
        policy: NetPolicy,
        jar: &Arc<Mutex<CookieJar>>,
        cache: &Arc<Mutex<NetCache>>,
        net_events: &Arc<Mutex<Vec<NetEvent>>>,
        user_agent: &netmod::SharedUserAgent,
        archive: &Arc<crate::archive::ArchiveState>,
        private: &crate::private_addr::PrivateAddressGuard,
        document: Option<&Url>,
    ) -> Result<String, String> {
        // AUDIT F5: an external script body (a `<script src>`, an ES module,
        // a worker's script) is a page-initiated read of a URL the page chose:
        // a page on the web may not run code out of the browser's own network.
        private.check(document, script_url, "script")?;
        let events = Arc::clone(net_events);
        netmod::fetch_document_report_shared(
            script_url,
            policy,
            jar,
            cache,
            netmod::now_unix(),
            user_agent,
            Some(&|mut event: NetEvent| {
                event.initiator = Some("script".to_owned());
                events.lock().expect("net events mutex").push(event)
            }),
        )
        .map(|script| {
            // Archive mode: an external script payload is a
            // subresource, whatever its scheme ended up being.
            archive.store(&script.url, false, &script.body);
            script.body_text()
        })
        .map_err(|err| err.to_string())
    }

    fn run_scripts(&mut self, url: &Url) {
        // The document being replaced takes its Web Workers with it (task: real
        // worker threads): a worker thread must not outlive the realm that
        // created it, and `attach_workers` below hands the new document its own
        // wiring (the spec's "workers die with their document", enforced here
        // rather than trusted to the page calling `terminate()`).
        if let Some(previous) = self.js.as_ref() {
            previous.close_workers();
        }
        // Fresh document: the CSS seam forgets last document's content hashes
        // so the same sheet on a new page is reported once again.
        self.css_scan.reset();
        let policy = self_provider_policy(&self.provider);
        let mut extraction = js::Extraction::default();
        let mut body_text = None;
        if let Some(doc) = self.doc.as_ref() {
            // Disjoint field borrows: `doc` stays immutable while scripts are
            // fetched through the SAME jar/cache handles the Blitz provider
            // owns — one shared state, locked per operation, not per request.
            let jar = Arc::clone(&self.jar);
            let cache = Arc::clone(&self.cache);
            // External `<script src>` bodies ride the document fetcher, so
            // they report into the same DevTools queue the poll loop drains
            // (one `FromTab::NetworkEvent` per script, like any subresource).
            let net_events = Arc::clone(&self.net_events);
            let user_agent = Arc::clone(&self.user_agent);
            let archive = Arc::clone(&self.archive);
            // AUDIT F5: the rule every page-chosen request is judged by, and
            // the origin the script bodies belong to.
            let private = self.private_guard.clone();
            let document = Some(url.clone());

            // Two-phase batch: the document walk hands us every `src` URL at
            // once, so a bounded pool of fetcher threads downloads them in
            // parallel on the ONE shared client (task D) instead of the screen
            // paying the sum of the sequential round-trips. Results are
            // collected positionally, so assembly — and execution — stay in
            // document order. Cookie visibility note: the old sequential fetch
            // let script #2 observe a Set-Cookie sent by script #1; with
            // parallel fetches that ordering is gone (arrivals settle whenever
            // the wire returns them). Accepted: scripts on distinct hosts
            // almost never rely on it, and any cookie a script sets is still
            // in the jar for every later request of the page.
            //
            // Fetcher threads are CAPPED: each fetcher is an OS thread, and a
            // sandboxed tab must never spawn one per `<script src>` — a page
            // with a hundred scripts under a tight thread limit takes EAGAIN
            // on thread creation and the whole tab dies (the startpage.com
            // crash). Eight concurrent fetchers saturate the shared reqwest
            // pool as well as an unbounded fan-out ever did.
            const MAX_SCRIPT_FETCHERS: usize = 8;
            let mut fetch_many = |script_urls: &[&Url]| -> Vec<Result<String, String>> {
                if script_urls.len() < 2 {
                    // Zero or one external script: fetching inline on the tab
                    // thread is exactly as fast and skips the thread overhead.
                    return script_urls
                        .iter()
                        .map(|script_url| {
                            Self::fetch_script_body(
                                script_url,
                                policy,
                                &jar,
                                &cache,
                                &net_events,
                                &user_agent,
                                &archive,
                                &private,
                                document.as_ref(),
                            )
                        })
                        .collect();
                }
                // Pull-based work queue: `next` hands out URL indices and
                // every worker writes its result into ITS slot, so positional
                // order (and thus execution order) survives whatever order the
                // responses arrive in. The tab thread is a worker too, and
                // spawns go through `Builder::spawn_scoped` — which reports
                // EAGAIN as an error instead of panicking like `scope.spawn`
                // does — so hitting the OS thread limit degrades to fewer (or
                // even zero) extra threads, never to a dead tab. (The shared
                // state lives OUTSIDE the scope: scoped threads may only
                // borrow data that outlives the scope itself.)
                let count = script_urls.len();
                let next = AtomicUsize::new(0);
                let results: Mutex<Vec<Option<Result<String, String>>>> =
                    Mutex::new((0..count).map(|_| None).collect());
                // The fetch loop every worker (and the tab thread) runs:
                // grab the next index, fetch on per-worker clones of the
                // tab's shared handles — same jar, cache, UA, event queue,
                // and archive sink, each locked only per operation — and
                // store the result positionally.
                let work = || loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        return;
                    }
                    let script_url = script_urls[index].clone();
                    let jar = Arc::clone(&jar);
                    let cache = Arc::clone(&cache);
                    let net_events = Arc::clone(&net_events);
                    let user_agent = Arc::clone(&user_agent);
                    let archive = Arc::clone(&archive);
                    let private = private.clone();
                    let document = document.clone();
                    let body = Self::fetch_script_body(
                        &script_url,
                        policy,
                        &jar,
                        &cache,
                        &net_events,
                        &user_agent,
                        &archive,
                        &private,
                        document.as_ref(),
                    );
                    results.lock().expect("script results mutex")[index] = Some(body);
                };
                std::thread::scope(|scope| {
                    let mut handles = Vec::new();
                    for _ in 0..count.min(MAX_SCRIPT_FETCHERS) {
                        match std::thread::Builder::new().spawn_scoped(scope, || work()) {
                            Ok(handle) => handles.push(handle),
                            // Thread creation failed (resource limit): stop
                            // adding workers; the tab thread below fetches
                            // whatever is left.
                            Err(err) => {
                                warn!(
                                    "script fetcher spawn failed ({err}); \
                                     fetching remaining scripts on the tab thread"
                                );
                                break;
                            }
                        }
                    }
                    // The tab thread works the same queue: with zero workers
                    // this is exactly the old sequential fetch.
                    work();
                    for handle in handles.drain(..) {
                        // A dead worker leaves its in-flight slot `None`; the
                        // positional read below turns that into a fetch error
                        // for that one script instead of unwinding the tab.
                        let _ = handle.join();
                    }
                });
                results
                    .into_inner()
                    .expect("script results mutex")
                    .into_iter()
                    .map(|slot| slot.unwrap_or_else(|| Err("script fetch thread panicked".to_string())))
                    .collect()
            };
            extraction = js::extract_scripts_batch(doc, url, &mut fetch_many);
            body_text = js::body_text_snapshot(doc);
        }

        // The title JS starts from is the one blitz parsed for this document.
        let parsed_title = self
            .shell
            .title
            .lock()
            .expect("title mutex")
            .clone()
            .or_else(|| {
                self.doc
                    .as_ref()
                    .and_then(|doc| doc.find_title_node())
                    .map(|node| node.text_content())
            });
        let host: HostRef = HostRef::new(RefCell::new(js::HostState::new(
            url.as_str(),
            parsed_title.as_deref(),
            body_text,
        )));
        // window.alert/confirm/prompt block on the shell through this cell
        // (task: JS dialogs); headless already disabled it.
        host.borrow_mut().dialog = Some(Arc::clone(&self.dialog));
        // window.innerWidth/innerHeight read the tab's live CSS-px viewport
        // (task: React geometry): seed it before any script runs, and keep it
        // in step on every Resize/zoom below.
        host.borrow_mut().viewport_width = self.width.max(1);
        host.borrow_mut().viewport_height = self.height.max(1);
        // `navigator.userAgent` reads the tab's live effective-UA cell
        // (same shared-handle pattern as the dialog host above).
        host.borrow_mut().user_agent = Some(Arc::clone(&self.user_agent));
        // `window.localStorage` reaches the container store through this
        // handle (task: per-origin DOM storage); the per-origin rule is
        // enforced natively against the host URL set just above.
        host.borrow_mut().local_storage = Some(Arc::clone(&self.local_storage));
        // `document.cookie` reaches the container jar through this handle
        // (task: the axios XSRF read): the SAME `Arc` the provider captures
        // `Set-Cookie` into, so scripts read the cookies the wire rides on
        // (minus HttpOnly, enforced in the native) and cookies scripts set
        // go out on the next request — one jar, both directions.
        host.borrow_mut().cookies = Some(Arc::clone(&self.jar));
        // The JS `fetch()` backend (`js::TabFetcher`) reports into this very
        // queue, so a page `fetch()` lands in the DevTools network panel like
        // XHR and subresources (task: XHR/fetch in the network panel).
        host.borrow_mut().net_events = Some(Arc::clone(&self.net_events));
        // AUDIT F5: the page's `fetch()`/`XMLHttpRequest` are refused by the
        // very rule its subresources are, and warn on the same console queue.
        host.borrow_mut().private_guard = Some(self.private_guard.clone());
        // `window.open`, and a script-activated `<a target="_blank">`, ask the
        // popup gate (task: block popups) — the very cell the poll loop drains
        // and that the shell's `SetPopupPolicy` writes into.
        host.borrow_mut().popups = Some(Arc::clone(&self.popups));
        // `XMLHttpRequest.send` runs on THIS network stack (task:
        // infosec.exchange posts): Mastodon's axios picks its XHR adapter
        // whenever the constructor exists, and without it not one `/api/`
        // request is ever issued — an empty timeline in zero console output.
        // Same client, same jar, same UA cell, same DevTools queue as every
        // other tab fetch (the cache stays out: an API body must never be
        // replayed stale to the page).
        let xhr_fetch = {
            let jar = Arc::clone(&self.jar);
            let net_events = Arc::clone(&self.net_events);
            let user_agent = Arc::clone(&self.user_agent);
            js::XhrFetch::new(Rc::new(move |method, url, headers, body| {
                let parsed = url::Url::parse(url).map_err(|err| err.to_string())?;
                let events = Arc::clone(&net_events);
                let jar_ref = Arc::clone(&jar);
                let mut jar_access: &std::sync::Mutex<_> = &jar_ref;
                netmod::xhr_fetch_report(
                    method,
                    &parsed,
                    headers,
                    body.map(str::as_bytes),
                    &mut jar_access,
                    netmod::now_unix(),
                    &user_agent,
                    Some(&|mut event: NetEvent| {
                        event.initiator = Some("xhr".to_owned());
                        events.lock().expect("net events mutex").push(event)
                    }),
                )
            }))
        };
        host.borrow_mut().xhr = Some(xhr_fetch);
        debug!(
            "running {} script(s) for {url}; skipped: {:?}",
            extraction.scripts.len(),
            extraction.notes
        );
        let js_started = Instant::now();
        // `<script type="module">` (task: module loading): the module graph
        // fetches its dependencies LAZILY, during evaluation — so the loader
        // gets the tab's own network handle: the same `fetch_script_body`
        // path the `<script src>` pool uses (one jar/cache, locked per
        // operation; every module body reports into the DevTools queue and
        // the archive sink like any other subresource).
        let module_fetch: js::ModuleFetch = {
            let jar = Arc::clone(&self.jar);
            let cache = Arc::clone(&self.cache);
            let net_events = Arc::clone(&self.net_events);
            let user_agent = Arc::clone(&self.user_agent);
            let archive = Arc::clone(&self.archive);
            let private = self.private_guard.clone();
            let document = url.clone();
            Rc::new(move |module_url: &Url| {
                Self::fetch_script_body(
                    module_url,
                    policy,
                    &jar,
                    &cache,
                    &net_events,
                    &user_agent,
                    &archive,
                    &private,
                    Some(&document),
                )
            })
        };
        // The runtime (Context + host + timer queue) survives this call: it
        // lives on as `self.js` until the next navigation replaces it.
        let mut runtime = js::JsRuntime::new_with_modules(host, url, module_fetch);
        // `<script type="importmap">` (task: importmap support): the walk
        // parsed + validated the map during extraction — the runtime keeps it
        // for the document's lifetime (module resolution consumes it).
        if let Some(map) = extraction.import_map.clone() {
            debug!(
                "importmap registered for {url}: {} specifier(s), {} scope(s)",
                map.len(),
                map.scopes_len()
            );
            runtime.set_import_map(Some(map));
        }
        // `@run-at document-start` userscripts (Tampermonkey directive, task:
        // infosec.exchange forensics): scripts whose source carries the
        // directive run BEFORE any page script, so they can wrap realm
        // singletons (timers, XHR, fetch) before the page captures them.
        // The directive lives in the code itself — the wire format is
        // unchanged. Scripts without the directive keep running after the
        // page's own scripts, as before.
        let start_scripts: Vec<(String, String)> = self
            .userscripts
            .iter()
            .filter(|s| s.enabled && Self::runs_at_start(&s.code))
            .filter(|s| {
                userscripts_for_url(std::slice::from_ref(s), url.as_str())
                    .iter()
                    .any(|m| m.id == s.id)
            })
            .map(|s| (s.name.clone(), s.code.clone()))
            .collect();
        if !start_scripts.is_empty() {
            debug!(
                "running {} document-start userscript(s) for {url}",
                start_scripts.len()
            );
            for (name, code) in &start_scripts {
                runtime.run_userscript(name, code, self.doc.as_mut());
            }
        }
        // Web Workers (task: real worker threads): wire the realm's worker host
        // BEFORE any page script runs, so a worker built by the first inline
        // script already has a document base URL, a script backend, the tab's
        // cookie jar / User-Agent cell / DevTools queue and the wake pipe. The
        // fetch backend is the very `fetch_script_body` path the `<script src>`
        // pool and the module loader use (one jar/cache locked per operation,
        // archive sink, managed policy), so a worker's script and
        // `importScripts` bodies are indistinguishable from a `<script src>` in
        // the network log — and it is `Arc`-based (not the `Rc` the in-thread
        // callers use) because the worker thread calls it from its own thread.
        let worker_policy = policy;
        runtime.attach_workers(crate::worker::WorkerAttachment {
            base_url: url.as_str().to_owned(),
            fetch: {
                let jar = Arc::clone(&self.jar);
                let cache = Arc::clone(&self.cache);
                let net_events = Arc::clone(&self.net_events);
                let user_agent = Arc::clone(&self.user_agent);
                let archive = Arc::clone(&self.archive);
                let private = self.private_guard.clone();
                let document = url.clone();
                Arc::new(move |script_url: &Url| {
                    Self::fetch_script_body(
                        script_url,
                        worker_policy,
                        &jar,
                        &cache,
                        &net_events,
                        &user_agent,
                        &archive,
                        &private,
                        Some(&document),
                    )
                })
            },
            private: self.private_guard.clone(),
            jar: Arc::clone(&self.jar),
            net_events: Arc::clone(&self.net_events),
            user_agent: Arc::clone(&self.user_agent),
            // A worker's reply wakes this loop out of `poll_inputs`, so the
            // page's `onmessage` runs on the next tick instead of whenever the
            // idle timer happens to fire.
            wake: {
                let pipe = Arc::clone(&self.pipe);
                Box::new(move || pipe.wake())
            },
        });
        // The live document is registered for the duration of the run, so
        // page JS can read and mutate the real DOM (docId 0).
        let errors = runtime.run_scripts(&extraction.scripts, self.doc.as_mut());
        debug!(
            "script run complete: {} error(s) in {} ms, timers queued: {}",
            errors.len(),
            js_started.elapsed().as_millis(),
            runtime.has_timers()
        );
        self.js = Some(runtime);
        // A new document has a new node space: no link of the old one can
        // resolve here, so start the stylesheet-load ledger empty.
        self.link_loads_fired.clear();
        // Page scripts may have mutated the DOM (created nodes, changed
        // attributes/styles): restyle + relayout now so the pending paint
        // (and any title mutation) reflects the post-script tree.
        self.settle();
        self.consume_js_status(extraction.notes.first().cloned(), &errors);
        self.take_js_title();
        // Userscripts (task: userscripts) run **after** the page's own scripts,
        // once per document, so they see the DOM the page built.
        self.run_userscripts(url);
    }

    /// True when a userscript's source declares the Tampermonkey directive
    /// `@run-at document-start` (checked over the leading comment block only —
    /// the same window managers scan). Everything else runs at document end.
    fn runs_at_start(code: &str) -> bool {
        code.lines()
            .take_while(|line| {
                let t = line.trim_start();
                t.starts_with("//") || t.starts_with("*") || t.is_empty()
            })
            .any(|line| {
                let t = line.to_ascii_lowercase();
                t.contains("@run-at") && (t.contains("document-start") || t.contains("document_start"))
            })
    }

    /// Run the userscripts that apply to this document (task: userscripts).
    ///
    /// Called at the end of every `run_scripts`, i.e. once per loaded document
    /// — including documents reached through a JS-driven redirect, because the
    /// filter runs against the **current document URL at run time**, which is
    /// exactly what [`userscripts_for_url`] is for. Matching scripts run in the
    /// shell's store order, in the page's own Boa realm
    /// ([`js::JsRuntime::run_userscript`]), each caught individually: one bad
    /// script never stops the others and never kills the tab.
    ///
    /// Zero cost when no scripts are installed (the common case): the function
    /// returns before touching the runtime.
    fn run_userscripts(&mut self, url: &Url) {
        if self.userscripts.is_empty() || self.js.is_none() {
            return;
        }
        let matching: Vec<(String, String)> = userscripts_for_url(&self.userscripts, url.as_str())
            .into_iter()
            // document-start scripts already ran before the page's own
            // scripts; running them twice would double-wrap patched globals.
            .filter(|script| !Self::runs_at_start(&script.code))
            .map(|script| (script.name.clone(), script.code.clone()))
            .collect();
        if matching.is_empty() {
            return;
        }
        info!(
            "running {} userscript(s) for {url} (of {} installed)",
            matching.len(),
            self.userscripts.len()
        );
        // The runtime leaves `self` for the duration so the live document can
        // be borrowed mutably alongside it (the same dance `run_scripts` does
        // by building the runtime locally); it goes straight back, timers and
        // any queued navigation included.
        let mut runtime = self.js.take().expect("js runtime presence checked above");
        let mut errors = Vec::new();
        for (name, code) in &matching {
            if let Some(err) = runtime.run_userscript(name, code, self.doc.as_mut()) {
                errors.push(err);
            }
        }
        self.js = Some(runtime);
        // Scripts may have restyled/extended the DOM: restyle + relayout before
        // the pending paint, and fold their side effects into the IPC feed.
        self.settle();
        self.needs_paint = true;
        self.consume_userscript_status(&errors);
        self.take_js_title();
    }

    /// Run a `javascript:` URL's program (task: JS-driven navigation): the
    /// code executes in the **current document's** JS world (same realm, same
    /// live DOM — which is exactly what makes bookmarklets work in every
    /// browser). A string completion value becomes a brand-new `about:blank`
    /// document at the current history
    /// entry (browser/spec behaviour); `undefined` changes nothing; a thrown
    /// exception is reported like any script error. Never touches the
    /// network, never kills the tab.
    fn run_javascript_url(&mut self, code: &str) -> bool {
        if code.trim().is_empty() {
            return true; // `javascript:` alone is a no-op everywhere
        }
        if self.js.is_none() {
            warn!("javascript: URL with no page to run it in");
            self.emit(FromTab::StatusText {
                text: Some("javascript: URL: no page is loaded to run it in".to_string()),
            });
            return false;
        }
        info!("running javascript: URL program ({} B)", code.len());
        let outcome = {
            let runtime = self.js.as_mut().expect("js world checked above");
            runtime.eval_javascript_url(code, self.doc.as_mut())
        };
        match outcome {
            Ok(Some(body)) => {
                // The string becomes the document (per spec) at `about:blank`,
                // replacing the current history entry; the program's other
                // side effects die with the document it mutated.
                let blank = Url::parse("about:blank").expect("about:blank parses");
                let history = if self.history.is_empty() {
                    NavHistory::Fresh
                } else {
                    NavHistory::Session(self.history_index)
                };
                self.load_html(&blank, &body, history);
                self.emit_js_status();
                true
            }
            Ok(None) => {
                // The program may have mutated the DOM/title: same settle +
                // repaint shape a timer batch gets.
                self.settle();
                self.needs_paint = true;
                self.consume_js_status(None, &[]);
                self.take_js_title();
                self.check_meta();
                self.maybe_repaint();
                self.emit_js_status();
                true
            }
            Err(err) => {
                self.settle();
                self.consume_js_status(None, &[err]);
                self.take_js_title();
                self.check_meta();
                self.maybe_repaint();
                self.emit_js_status();
                false
            }
        }
    }

    /// Follow the navigation the current document's scripts queued during
    /// the last JS turn (`location.href=`/`assign`/`replace`/`reload`) —
    /// search-engine redirect pages live this way. Relative targets resolve
    /// against the document URL; a `javascript:` target runs as a program.
    /// Called from [`Tab::drain_nav_hook`], i.e. from the poll loop, never
    /// from inside the stack that queued it (a self-redirecting page spins
    /// the loop like any browser's redirect loop; it cannot grow the stack).
    /// The schemes a document may navigate *itself* into (task: the audit's
    /// `file:` gap). The web is `http`/`https`, plus `about`, `data` and `blob`
    /// which pages generate themselves. Local schemes are not reachable from
    /// the web: `file:` only from a document that is itself a local file, and
    /// the shell's internal `ironsurf:` never by following a link — those are
    /// addresses the user types, and [`Tab::nav_from_shell`] is how the tab
    /// knows the difference. An unknown scheme is the address bar's to try.
    fn document_may_reach(&self, from: Option<&Url>, to: &Url) -> bool {
        let to_scheme = to.scheme();
        if matches!(
            to_scheme,
            "http" | "https" | "about" | "data" | "blob" | "ws" | "wss"
        ) {
            return true;
        }
        if to_scheme == "ironsurf" {
            return false;
        }
        from.is_some_and(|base| base.scheme() == to_scheme)
    }

    fn follow_pending_js_nav(&mut self) -> bool {
        let Some(nav) = self
            .js
            .as_ref()
            .and_then(|runtime| runtime.take_pending_navigation())
        else {
            return false;
        };
        if let Some(code) = javascript_url_code(&nav.target) {
            return self.run_javascript_url(&code);
        }
        // Resolve against the live document URL (fall back to the history
        // entry we are showing, same precedence as `check_meta`).
        let base = self
            .doc
            .as_ref()
            .map(|doc| doc.url().clone())
            .or_else(|| self.history.get(self.history_index).cloned());
        let resolved = match base.as_ref().and_then(|base| base.join(&nav.target).ok()) {
            Some(url) => url,
            None => {
                warn!("js navigation to {:?} did not resolve", nav.target);
                self.emit(FromTab::StatusText {
                    text: Some(format!("invalid navigation target {}", nav.target)),
                });
                return true;
            }
        };
        // AUDIT F1: a page may only go where a redirect from it may go. A
        // document on `http(s)` reaching `file:` turned any page visit into
        // arbitrary local file disclosure (the fetch is answered locally and the
        // file renders), and `ironsurf:` let the page talk to the shell. Local
        // schemes stay reachable from a local document and from the user's own
        // address bar, which is what `nav_from_shell` records.
        if !self.document_may_reach(base.as_ref(), &resolved) {
            warn!("refusing page-initiated navigation to {resolved}");
            self.emit(FromTab::StatusText {
                text: Some(format!(
                    "a page may not navigate to {} — open it from the address bar",
                    resolved
                )),
            });
            // The pending navigation is consumed: the hostile target is dropped,
            // and the document on screen stays exactly where it was.
            return true;
        }
        // AUDIT F5: the **private-address rule**. `location.href =
        // "http://127.0.0.1:9/…"` from a page on the web used to be a request
        // the tab made on the page's behalf — how a page discovers (and drives)
        // the services on the user's own network, the tab's archive server
        // included. A page-driven hop is refused unless it stays on the
        // document's own origin; the address bar, Reload and a link the user
        // clicked are the browser's own UI and still reach it. The refusal is
        // queued as a DevTools console warning by the guard.
        if let Err(reason) = self.private_guard.check(base.as_ref(), &resolved, "navigation") {
            self.emit(FromTab::StatusText {
                text: Some(format!("{reason} — open it from the address bar")),
            });
            return true;
        }
        // From here the navigation is page-chosen, not user-chosen.
        self.nav_from_shell = false;
        info!(
            "following js navigation → {resolved} ({})",
            if nav.replace {
                "replace entry"
            } else {
                "push entry"
            }
        );
        let history = if nav.replace && !self.history.is_empty() {
            NavHistory::Session(self.history_index)
        } else {
            NavHistory::Fresh
        };
        self.navigate_post(resolved.as_str(), history, None)
    }

    /// Arm the delayed navigation a freshly loaded document asks for: the
    /// response's `Refresh:` header (already parsed + resolved by `netmod`, so
    /// `header` is the winning directive when present) or, failing that, the
    /// first `<meta http-equiv="refresh">` in the live document. The meta tag's
    /// `content` goes through the very same `netmod::parse_refresh`, resolved
    /// against the document URL. Task: Anubis `metarefresh` gates — Anubis
    /// randomly picks between the header and the tag, so both must be followed.
    /// The deadline is `now + delay`; `0` fires on the next poll iteration,
    /// never inline (a refresh is a queued navigation, like `location.href=`).
    fn schedule_refresh(&mut self, header: Option<netmod::RefreshSpec>) {
        match header.or_else(|| self.document_meta_refresh()) {
            Some(spec) => {
                debug!("refresh armed: navigate to {} in {:?}", spec.url, spec.delay);
                self.refresh_at = Some((Instant::now() + spec.delay, spec.url));
            }
            None => self.refresh_at = None,
        }
    }

    /// The first usable `<meta http-equiv="refresh" content="…">` in the live
    /// document, resolved against the document URL. `http-equiv` matches
    /// case-insensitively (as browsers do); an unparseable `content` or one
    /// without a target reloads the current URL.
    fn document_meta_refresh(&self) -> Option<netmod::RefreshSpec> {
        let doc = self.doc.as_ref()?;
        let base = doc.url().clone();
        let ids = doc.query_selector_all("meta").ok()?;
        for id in ids {
            let Some(node) = doc.get_node(id) else { continue };
            let Some(element) = node.element_data() else { continue };
            let equiv = element
                .attr(blitz_dom::local_name!("http-equiv"))
                .unwrap_or_default();
            if !equiv.trim().eq_ignore_ascii_case("refresh") {
                continue;
            }
            let Some(content) = element.attr(blitz_dom::local_name!("content")) else {
                continue;
            };
            let Some((seconds, target)) = netmod::parse_refresh(content) else {
                continue;
            };
            let url = match target {
                Some(target) => base.join(&target).unwrap_or_else(|_| base.clone()),
                None => base.clone(),
            };
            return Some(netmod::RefreshSpec {
                delay: Duration::from_secs(seconds),
                url,
            });
        }
        None
    }

    /// Follow the delayed navigation a loaded document armed, once its deadline
    /// has passed. Called from [`Tab::drain_nav_hook`] (the poll loop and the
    /// headless harness) after the queued JS navigations, never inline with the
    /// load that armed it — a page that refreshes to itself spins the loop like
    /// any browser's refresh loop; it cannot grow the stack.
    fn follow_due_refresh(&mut self) -> bool {
        let Some((deadline, url)) = self.refresh_at.clone() else {
            return false;
        };
        if Instant::now() < deadline {
            return false;
        }
        self.refresh_at = None;
        info!("following delayed refresh → {url}");
        // AUDIT F5: a `Refresh:` the *page* answered with is the page's own
        // hop, so the private-address rule applies to it — a public page may
        // not bounce the tab onto a LAN address the user never typed. (A local
        // page refreshing itself stays same-origin and is never refused.)
        let from = self.doc.as_ref().map(|doc| doc.url().clone());
        if let Err(reason) = self.private_guard.check(from.as_ref(), &url, "refresh") {
            self.emit(FromTab::StatusText {
                text: Some(format!("{reason} — open it from the address bar")),
            });
            return true;
        }
        // A refresh is a real navigation: it pushes a history entry (browsers
        // do the same), and the destination's own `Refresh:` (if any) replaces
        // the deadline we just cleared.
        self.navigate(url.as_str(), NavHistory::Fresh)
    }

    /// How long the poll loop may sleep before the armed refresh deadline,
    /// folded into the timer/media tick caps (see `timer_poll_tick`).
    fn refresh_sleep_hint(&self) -> Option<Duration> {
        self.refresh_at
            .as_ref()
            .map(|(deadline, _)| deadline.saturating_duration_since(Instant::now()))
    }

    /// Fold the JS host cell's side effects (console output, notes) and these
    /// script errors into the tab's pending status text, draining what was
    /// read so the next batch only reports what is new — **and** ship every
    /// drained item over IPC as `FromTab::ConsoleLog` (task: DevTools console):
    ///
    /// * every `console.*` entry → `{ source: "js", level, message }`;
    /// * every uncaught script exception → `{ source, level: "error" }`, the
    ///   source `"dom"` when the throw came out of a native DOM binding and
    ///   `"js"` otherwise (one report per throw — the DOM classification
    ///   replaces the generic `"js"` line, it never doubles it);
    /// * then the CSS re-parse seam runs (`scan_css_diagnostics`), reporting
    ///   anything invalid the document grew since the last JS turn.
    ///
    /// The `StatusText` behaviour is unchanged: newest console line and
    /// newest error still flow to the status bar exactly as before.
    fn consume_js_status(&mut self, extraction_note: Option<String>, errors: &[js::ScriptError]) {
        let mut console_entries = Vec::new();
        let mut note = extraction_note;
        if let Some(runtime) = self.js.as_ref() {
            let (console, host_note) = {
                let host = runtime.host().borrow_mut();
                (host.console.clone(), host.latest_note().map(str::to_owned))
            };
            {
                // Drain what was just reported: the next batch only shows news.
                let mut host = runtime.host().borrow_mut();
                host.console.clear();
                host.notes.clear();
                // DOM throws are classified into `errors` before this point
                // (JsRuntime::classify_throw); the log itself is per-batch.
                host.dom_throws.clear();
                host.repaint = false;
            }
            console_entries = console;
            if note.is_none() {
                note = host_note;
            }
        }
        // 1. Every `console.*` line of this JS turn, in order, as a DevTools
        // console entry (the old behaviour only ever showed the newest one).
        // Page scripts stay `source: "js"` with verbatim text; a line tagged
        // with a userscript name (which only happens inside a userscript turn —
        // see `consume_userscript_status`) is attributed the same way there.
        for line in &console_entries {
            let source = if line.userscript.is_some() {
                "userscript"
            } else {
                "js"
            };
            self.emit(FromTab::ConsoleLog {
                source: source.to_string(),
                level: line.level.as_str().to_string(),
                message: line.display_message(),
            });
        }
        // 2. Uncaught exceptions (compile errors included): `"dom"` for
        // natives, `"js"` for everything else; readable message as thrown.
        for err in errors {
            self.emit(FromTab::ConsoleLog {
                source: err.source.to_string(),
                level: "error".to_string(),
                message: err.message.clone(),
            });
        }
        // 3. CSS: `<style>`/`style=` re-parse of anything new this document
        // has (scripts inject sheets at runtime; the scan dedupes by content).
        self.scan_css_diagnostics();
        // One status text: script errors outrank `console.*` output, which
        // outranks the "skipped script" extraction notes.
        let console_last = console_entries
            .last()
            .map(|line| format!("js console {}: {}", line.level.as_str(), line.display_message()));
        let status = js::status_text_for_errors(errors)
            .or(console_last)
            .or(note);
        if status.is_some() {
            self.js_status = status;
        }
    }

    /// Fold a **userscript** turn into the IPC feed (task: userscripts).
    ///
    /// Same shape as [`Tab::consume_js_status`], one difference per entry: a
    /// `console.*` line authored by a userscript ships as
    /// `FromTab::ConsoleLog { source: "userscript", … }` with the message
    /// prefixed `"{name}: "`, and a script that threw ships as
    /// `{ source: "userscript", level: "error", message: "{name}: {err}" }`
    /// (the name is baked in by [`js::JsRuntime::run_userscript`]), so the
    /// DevTools console reads `[userscript] Boom script: Error: …` and the
    /// culprit is obvious at a glance. The status bar gets the one-line
    /// `Userscript error in {name}` (matching the existing script-error
    /// behaviour of one line, newest error wins).
    fn consume_userscript_status(&mut self, errors: &[js::ScriptError]) {
        let mut lines = Vec::new();
        if let Some(runtime) = self.js.as_ref() {
            let mut host = runtime.host().borrow_mut();
            lines = std::mem::take(&mut host.console);
            host.notes.clear();
            // Anything a native threw during the userscript turn has already
            // been rolled back by `run_userscript`; clear the slate so no stale
            // entry can classify a later page-script error as `"dom"`.
            host.dom_throws.clear();
            if host.repaint {
                self.needs_paint = true;
                host.repaint = false;
            }
        }
        for line in &lines {
            // A page-script line in this drain (a timer callback that fired
            // inside the turn, say) keeps its honest `"js"` source.
            let source = if line.userscript.is_some() {
                "userscript"
            } else {
                "js"
            };
            self.emit(FromTab::ConsoleLog {
                source: source.to_string(),
                level: line.level.as_str().to_string(),
                message: line.display_message(),
            });
        }
        for err in errors {
            self.emit(FromTab::ConsoleLog {
                source: err.source.to_string(),
                level: "error".to_string(),
                message: err.message.clone(),
            });
        }
        if let Some(err) = errors.last() {
            self.js_status = Some(format!("Userscript error in {}", err.origin));
        } else if let Some(line) = lines.last() {
            self.js_status = Some(format!(
                "userscript console {}: {}",
                line.level.as_str(),
                line.display_message()
            ));
        }
    }

    /// The CSS seam of the DevTools console (see `src/logging.rs` for what
    /// this can and cannot see): re-parse the document's `<style>` bodies and
    /// inline `style="…"` attributes through Stylo's real error reporter and
    /// ship each new problem as `FromTab::ConsoleLog { source: "css" }`.
    fn scan_css_diagnostics(&mut self) {
        let entries = match self.doc.as_ref() {
            Some(doc) => {
                let base = doc.url().clone();
                crate::logging::scan_css(doc, &base, &mut self.css_scan)
            }
            None => return,
        };
        for entry in entries {
            self.emit(FromTab::ConsoleLog {
                source: entry.source,
                level: entry.level,
                message: entry.message,
            });
        }
    }

    /// Ship every console entry the fetch threads queued since the last poll
    /// (wrong-`Content-Type` `.css` responses — `src/logging.rs`): same
    /// cadence and same NetWaker wake as the network drain, never a tick late.
    fn drain_console_events(&mut self) {
        let entries =
            std::mem::take(&mut *self.console_events.lock().expect("console events mutex"));
        for entry in entries {
            self.emit(FromTab::ConsoleLog {
                source: entry.source,
                level: entry.level,
                message: entry.message,
            });
        }
    }

    /// Consume a `document.title = …` assignment raised since the last check
    /// (host → `js_title` + repaint request; `check_meta` emits the delta).
    fn take_js_title(&mut self) {
        let assigned = self
            .js
            .as_ref()
            .and_then(|runtime| runtime.host().borrow_mut().title_assigned.take());
        if let Some(title) = assigned {
            self.js_title = Some(title);
            self.needs_paint = true;
        }
    }

    /// Poll timeout capped by the earliest JS timer deadline: a page that
    /// armed `setTimeout(fn, 10)` must not wait for the next `IDLE_TICK`.
    /// A pending `Refresh:`/meta-refresh navigation caps it the same way.
    fn timer_poll_tick(&self, base: Duration) -> Duration {
        let mut tick = base;
        if let Some(deadline) = self
            .js
            .as_ref()
            .and_then(|runtime| runtime.next_timer_deadline())
        {
            tick = tick.min(deadline.saturating_duration_since(Instant::now()));
        }
        if let Some(hint) = self.refresh_sleep_hint() {
            tick = tick.min(hint);
        }
        tick
    }

    /// Timer step (task: real timers): execute every JS timer callback due
    /// now, then settle + refresh metadata exactly like the input path does.
    /// Exceptions are caught per callback inside the runtime; the newest one
    /// surfaces as `FromTab::StatusText` (same shape as a `ScriptError`).
    /// `0` = nothing scheduled or nothing due (cheap on the idle path).
    fn run_due_timers(&mut self) -> usize {
        // Web Worker replies first (task: real worker threads): a worker posts
        // from its own thread, and the page's `onmessage` has to run on *this*
        // thread — the only one that may touch the realm. Delivered replies are
        // DOM-visible (`#status.textContent = …` is how an Anubis gate shows its
        // result), so the settle/repaint bookkeeping below is owed for them even
        // when the page has no JS timer armed.
        if self.drain_workers() > 0 {
            self.settle();
            self.needs_paint = true;
            // The handler the delivery ran may have logged or thrown (a worker
            // reply is how an Anubis page reports itself, and DevTools shows
            // those lines) — the host capture fills during the turn, so it folds
            // here, exactly as the timer callbacks below do, rather than waiting
            // for a wake an idle page never gets.
            self.consume_js_status(None, &[]);
            self.take_js_title();
            self.check_meta();
            self.maybe_repaint();
            self.emit_js_status();
        }
        if self
            .js
            .as_ref()
            .and_then(|runtime| runtime.next_timer_deadline())
            .is_none_or(|deadline| deadline > Instant::now())
        {
            return 0;
        }
        // (block ends the borrow of `self.js` before the IPC-side steps)
        let (fired, errors) = {
            let Some(runtime) = self.js.as_mut() else {
                return 0;
            };
            let before = runtime.host().borrow().timers_fired;
            // Same live-DOM registration as the initial script batch.
            let errors = runtime.run_due_timers(Instant::now(), self.doc.as_mut());
            let fired = usize::try_from(runtime.host().borrow().timers_fired - before)
                .unwrap_or(usize::MAX);
            (fired, errors)
        };
        debug!(
            "timer batch: {fired} callback(s) fired, {} error(s)",
            errors.len()
        );
        // Same coalescing shape as the input path: settle (title/url may
        // have moved) and mark for a repaint; the loop's paint step emits at
        // ≤30 fps. A throwing callback must never stop the tab (contract).
        self.settle();
        if fired > 0 || !errors.is_empty() {
            self.needs_paint = true;
        }
        self.consume_js_status(None, &errors);
        self.take_js_title();
        self.check_meta();
        self.maybe_repaint();
        // A status text raised by a callback goes out immediately (there is
        // no "load finished" ordering to honour outside navigation time).
        self.emit_js_status();
        fired
    }

    /// Deliver every Web Worker reply queued since the last poll (task: real
    /// worker threads) — cheap when nothing is pending, which is the state of
    /// every page that never constructs a worker (one `Vec` emptiness test).
    /// `0` whenever there is no JS world; a throwing page handler is caught
    /// inside the realm glue and never reaches here.
    fn drain_workers(&mut self) -> usize {
        self.js
            .as_mut()
            .map(|runtime| runtime.deliver_worker_messages())
            .unwrap_or(0)
    }

    /// Flush the status text the last script run raised. Called after the
    /// load-finished `StatusText(None)` so the shell actually gets to show it.
    fn emit_js_status(&mut self) {
        if let Some(text) = self.js_status.take() {
            self.emit(FromTab::StatusText { text: Some(text) });
        }
    }

    // -- scroll geometry --------------------------------------------------------

    /// The viewport's vertical scroll geometry ([`ScrollState`]) — the same
    /// numbers blitz computes for its own viewport `scroll_state`, read through
    /// the public API: the root element's scrollable extent (its border-box
    /// height or whatever overflows it below, whichever is larger) measured
    /// against the window height in CSS px, and the document's live scroll
    /// offset.
    ///
    /// `range` carries the engine's overscroll rule (a document that scrolls at
    /// all can be pulled half a window past its content bottom, so its last
    /// lines rest mid-window): `y` runs `0 …= range`, and the content spans
    /// `viewport_h + range`. A document that fits its window reports
    /// `range == 0` — no scroll, and no scroll bar. `None` only while the tab
    /// has no document (or no root element) to measure.
    fn viewport_scroll_state(&self) -> Option<ScrollState> {
        let doc = self.doc.as_ref()?;
        let root = doc.try_root_element()?;
        let layout = root.final_layout();
        let content_h =
            f64::from(layout.size.height).max(f64::from(layout.scrollable_overflow_rect.bottom));
        let viewport = doc.viewport();
        let scale = viewport.scale_f64().max(f64::EPSILON);
        let window_h = (viewport.window_size.1 as f64 / scale).max(0.0);
        let beyond = content_h - window_h;
        let range = match beyond > 0.0 {
            true => beyond + window_h * 0.5,
            false => 0.0,
        };
        let y = doc.viewport_scroll().y;
        // Snap the top edge: a glide feeds its slices through f32-crunched
        // engine deltas, so a document brought to the top can rest on a
        // float residue (5.7e-14 observed) rather than on zero. Below a
        // millionth of a CSS px *is* the top; the shell's scroll-bar test
        // — and its readers — may say so.
        let y = if y < 1e-6 { 0.0 } else { y };
        Some(ScrollState {
            y: y.clamp(0.0, range) as f32,
            range: range as f32,
            viewport_h: window_h as f32,
        })
    }

    /// Tell the shell what the page's scroll geometry is, but only when it
    /// moved ([`ScrollState::same_within`], half a CSS px): the overlay scroll
    /// bar the shell draws over the page needs the numbers to place its thumb,
    /// and a page that sits still must not cost a message per poll tick. Even
    /// one damped wheel notch moves the document ~10 px (and the glide feeds
    /// slices of ≥1 px), so the gate never misses a scroll.
    fn report_scroll_state(&mut self) {
        let Some(state) = self.viewport_scroll_state() else { return };
        // An edge reached is news even under the half-px gate: the shell may
        // hold the last 0.4 px of a glide as "the" offset, and only the
        // report of a parked 0.0 tells it the page came home.
        let reached_edge = self.scroll_report.is_some_and(|prev| {
            (state.y == 0.0 && prev.y != 0.0) || (state.y == state.range && prev.y != state.range)
        });
        if !reached_edge && self.scroll_report.is_some_and(|prev| prev.same_within(&state, 0.5)) {
            return;
        }
        self.scroll_report = Some(state);
        self.emit(FromTab::ScrollState { state });
    }

    /// Scroll the document's viewport by `travel` CSS px (positive = down),
    /// clamped to the range [`Self::viewport_scroll_state`] reports — which is
    /// also what says whether the document scrolls at all: a page that fits its
    /// window has `range == 0` and stays exactly where it is.
    ///
    /// This is the *document* viewport, the same thing the wheel and the shell's
    /// scroll bar move, so an `overflow: scroll` box inside the page keeps its own
    /// keys and [`FromTab::ScrollState`] (the bar's geometry) follows the keystroke.
    fn scroll_viewport(&mut self, travel: f64) {
        let Some(state) = self.viewport_scroll_state() else {
            return;
        };
        if state.range <= 0.0 || travel == 0.0 {
            return;
        }
        let y = (f64::from(state.y) + travel).clamp(0.0, f64::from(state.range));
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let mut scroll = doc.viewport_scroll();
        scroll.y = y;
        doc.set_viewport_scroll(scroll);
        // The scroll bar follows through `report_scroll_state` on the next tick;
        // the pixels are this tab's job.
        self.needs_paint = true;
        debug!("key scroll \u{2192} y = {y:.0} of {:.0}", f64::from(state.range));
    }

    /// One screen of PageUp/PageDown travel: a viewport less the tenth kept as
    /// overlap, so a line or two stays in sight and the reader knows they have not
    /// fallen off the page.
    ///
    /// blitz has no page-level binding for either key — its keymap spells them as
    /// editor no-ops (`node/text.rs`: `"scrollPageDown:" => {}`) — so unless the tab
    /// does it itself the keys do nothing at all outside a text field. And
    /// `Ctrl`/`⌘`-PageDown scrolls the same as the bare key: that chord is
    /// deliberately not bound (it is not the shell's to take).
    fn scroll_page(&mut self, up: bool) {
        let Some(state) = self.viewport_scroll_state() else {
            return;
        };
        let screen = (f64::from(state.viewport_h) * 0.9).max(1.0);
        self.scroll_viewport(if up { -screen } else { screen });
    }

    /// One line of `↑` / `↓` travel ([`ARROW_SCROLL_PX`]) — the fine motion to
    /// PageUp/PageDown's screen, for keeping an exact line in view.
    fn scroll_line(&mut self, up: bool) {
        self.scroll_viewport(if up {
            -ARROW_SCROLL_PX
        } else {
            ARROW_SCROLL_PX
        });
    }

    // -- smooth wheel --------------------------------------------------------

    /// Cap the poll sleep at the next slice of the wheel glide: while a glide
    /// is in flight the loop must wake to feed it even when the page and the
    /// shell are otherwise silent.
    fn wheel_poll_tick(&self, base: Duration) -> Duration {
        let (x, y) = self.wheel_glide.pending;
        if x == 0.0 && y == 0.0 {
            return base;
        }
        self.wheel_glide
            .next
            .saturating_duration_since(Instant::now())
            .min(base)
    }

    /// Feed one eased slice of the queued wheel travel into the document:
    /// the page *chases* the wheel instead of teleporting, and a stream of
    /// events flows — each event adds to what is owed, each slice takes a
    /// step toward it. A slice is an ordinary wheel event in pixel units,
    /// so blitz's scroll-chain resolution and clamping are untouched; only
    /// the step size is ours now. `force` runs the whole remainder at once
    /// (tests and any caller that must know the final offset now); the poll
    /// loop never forces.
    fn pump_wheel(&mut self, force: bool) {
        let (px, py) = self.wheel_glide.pending;
        if px == 0.0 && py == 0.0 {
            return;
        }
        let now = Instant::now();
        if !force && now < self.wheel_glide.next {
            return;
        }
        let dt = now.saturating_duration_since(self.wheel_glide.last);
        self.wheel_glide.last = now;
        self.wheel_glide.next = now + WHEEL_STEP;
        let (cx, cy) = if force {
            (px, py)
        } else {
            (wheel_glide_step(px, dt), wheel_glide_step(py, dt))
        };
        self.wheel_glide.pending = (px - cx, py - cy);
        let Some(coords) = self.wheel_glide.at else {
            self.wheel_glide.pending = (0.0, 0.0);
            return;
        };
        let (ox, oy) = self.wheel_glide.over;
        self.feed_ui_event(UiEvent::Wheel(BlitzWheelEvent {
            delta: BlitzWheelDelta::Pixels(cx, cy),
            coords,
            buttons: self.mouse_buttons,
            mods: KMods::empty(),
            element: Point::default(),
        }));
        // The page moved under a stationary pointer: what sits under the
        // cursor — and its cursor shape — is in question again, exactly as
        // for the wheel event itself (`handle_input`).
        self.report_cursor(ox, oy, false);
        self.needs_paint = true;
    }

    // -- painting -----------------------------------------------------------

    /// Paint the document into an RGBA8 buffer and emit `FromTab::Frame`,
    /// coalesced to at most ~30 fps. Invisible tabs drop paint requests
    /// (they stay pending via `needs_paint` until the next visible repaint).
    fn maybe_repaint(&mut self) {
        if !self.needs_paint {
            return;
        }
        if !self.visible {
            return; // pending: flushed on SetVisible(true)
        }
        if let Some(last) = self.last_paint {
            if last.elapsed() < FRAME_INTERVAL {
                return; // coalesce; the tick loop retries once due
            }
        }
        self.paint_now();
    }

    fn paint_now(&mut self) {
        let Some(_doc) = self.doc.as_mut() else {
            return;
        };
        let (width, height) = (
            (self.width as f32 * self.scale) as u32,
            (self.height as f32 * self.scale) as u32,
        );
        if width == 0 || height == 0 {
            return;
        }
        let scale = self.scale;
        // Paint converts geometry, so it needs blitz's **total** scale (hidpi ×
        // zoom), not the hidpi factor: DOM boxes stay in CSS px while parley's
        // inline layout (glyph runs, selection rects, inline backgrounds) is
        // already in device px. Handing `paint_scene` the hidpi-only scale left
        // every block box at its CSS numbers once zoom ≠ 1 — the page painted
        // into the top-left of the canvas — and, worse, every inline text run
        // was translated by `pos × hidpi` while its own glyph/selection rects
        // were device-sized. The two spaces disagreed by `(zoom − 1) × pos`, so
        // a highlight dragged in one paragraph landed on the paragraph above it
        // (and at 50 %/125 % on whichever element happened to share the
        // squashed-up row). Devtools only made this visible: its Inspector
        // marker (`inspect_highlights`) measures CSS rects the same way.
        let paint_scale = self
            .doc
            .as_ref()
            .map(|doc| doc.viewport().scale_f64())
            .unwrap_or(f64::from(scale));
        // Find-in-page highlights are measured here — before the paint closure
        // takes the document mutably — and drawn **last**, over the finished
        // page, so the text underneath them stays readable.
        let highlights = self.find_highlights(paint_scale);
        // …and the Inspector's marker for the selected/hovered element, measured
        // in the same space and painted over everything else.
        let inspected = self.inspect_highlights(paint_scale);
        let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(255, 255, 255),
                    None,
                    &Rect::from_origin_size(
                        (0.0, 0.0),
                        Size::new(f64::from(width), f64::from(height)),
                    ),
                );
                // `&mut HtmlDocument` coerces to `&mut BaseDocument` via DerefMut.
                paint_scene(
                    scene,
                    self.doc.as_mut().expect("doc"),
                    paint_scale,
                    width,
                    height,
                    0,
                    0,
                );
                find::paint(scene, &highlights);
                inspector::paint(scene, &inspected);
            },
            width,
            height,
        );
        self.last_paint = Some(Instant::now());
        self.needs_paint = false;
        self.emit(FromTab::Frame {
            width,
            height,
            scale,
            rgba: buffer,
        });
    }

    // -- metadata deltas ------------------------------------------------------

    /// Compare title/url against the cached last values and emit
    /// `TitleChanged`/`UrlChanged` on change. The title prefers the value
    /// flushed through the `ShellProvider` hook (which JS/DOM mutations
    /// update), falling back to polling the `<title>` node.
    fn check_meta(&mut self) {
        // A pushState'd address bar wins over the (unchanged) document URL.
        let mut current_url = self.url_override.clone();
        if current_url.is_none() {
            current_url = self.doc.as_ref().map(|doc| doc.url().as_str().to_owned());
        }
        if current_url.is_none() {
            // Before the first successful load the tab still reports the
            // URL it is trying to show (stable contract for the shell).
            current_url = self
                .history
                .get(self.history_index)
                .map(|u| u.as_str().to_owned());
        }
        if current_url != self.last_url {
            self.last_url = current_url;
            let url = self.last_url.clone();
            self.emit(FromTab::UrlChanged { url });
        }

        let js = self.js_title.clone();
        let hooked = self.shell.title.lock().expect("title mutex").clone();
        let polled = self
            .doc
            .as_ref()
            .and_then(|doc| doc.find_title_node())
            .map(|node| node.text_content());
        let title = js.or(hooked).or(polled);
        if Some(&title) != self.last_title.as_ref() {
            self.last_title = Some(title.clone());
            self.emit(FromTab::TitleChanged { title });
        }

        // The document tree is a delta like the title and the url, and this is
        // the one place every path — navigation, a JS timer, the live poll loop,
        // a headless pump — lands after a settle, so it is where an open
        // Inspector pane learns the markup moved. A closed pane costs a boolean.
        self.inspect_check();
    }

    // -- find in page (Ctrl-F) ------------------------------------------------

    /// Install (or clear, with `""`) the shell's find query and report the count
    /// back. The first hit becomes active and is scrolled into view — like a
    /// browser that jumps to a match while you type.
    fn set_find(&mut self, query: String) {
        self.find_query = query;
        self.find_rerun(true);
        self.emit_find_result();
        self.needs_paint = true;
        self.maybe_repaint();
    }

    /// Jump to the next (forward) or previous (back) hit, **wrapping** at the
    /// ends of the document — what the shell's Enter/Shift-Enter and the bar's
    /// ▲/▼ buttons ask for.
    fn find_jump(&mut self, forward: bool) {
        if self.find_query.is_empty() {
            return; // no query: nothing to walk
        }
        if self.find_stale || self.find_hits.is_empty() {
            // The document moved under the hits (or nothing was found yet):
            // re-run rather than trust byte ranges into text that has changed.
            self.find_rerun(true);
        } else {
            let n = self.find_hits.len();
            self.find_active = Some(match self.find_active {
                Some(i) if forward => (i + 1) % n,
                // Wrapping backwards: the hit before the first is the last one.
                Some(i) => {
                    if i == 0 {
                        n - 1
                    } else {
                        i - 1
                    }
                }
                None if forward => 0,
                None => n - 1,
            });
            self.focus_active_hit();
        }
        self.emit_find_result();
        self.needs_paint = true;
        self.maybe_repaint();
    }

    /// Re-run the live query against the current document. `jump_first` makes
    /// the **first** hit active and scrolls to it (a new query, or the document
    /// having moved); otherwise the counter keeps its position wherever the new
    /// hit list still has it.
    fn find_rerun(&mut self, jump_first: bool) {
        self.find_stale = false;
        self.find_hits.clear();
        self.find_active = None;
        if self.find_query.is_empty() {
            return;
        }
        let Some(doc) = self.doc.as_ref() else { return };
        self.find_hits = find::collect(doc, &self.find_query);
        if self.find_hits.is_empty() {
            return;
        }
        let n = self.find_hits.len();
        self.find_active = Some(if jump_first {
            0
        } else {
            self.find_active.unwrap_or(0).min(n - 1)
        });
        if jump_first {
            self.focus_active_hit();
        }
    }

    /// Scroll so the active hit sits in the viewport (see
    /// [`find::scroll_hit_into_view`]). Script may have dropped the node since
    /// the search — then there is nothing to scroll to.
    fn focus_active_hit(&mut self) {
        let Some(hit) = self
            .find_active
            .and_then(|i| self.find_hits.get(i))
            .copied()
        else {
            return;
        };
        // The viewport the rectangles are measured against is the window in CSS
        // px (logical px — the same space `on_key_chord` hit-tests in).
        let (w, h) = (f64::from(self.width), f64::from(self.height));
        if let Some(doc) = self.doc.as_mut() {
            find::scroll_hit_into_view(doc, &hit, w, h);
        }
    }

    /// Emit the counts the find bar shows (`active` is 1-based; 0 = none).
    fn emit_find_result(&mut self) {
        self.emit(FromTab::FindResult {
            query: self.find_query.clone(),
            matches: self.find_hits.len(),
            active: self.find_active.map(|i| i + 1).unwrap_or(0),
        });
    }

    /// A fresh document arrived: re-run the query the shell still holds, because
    /// browsers keep the find bar open across navigations. The first hit becomes
    /// active, but the viewport is **not** yanked — a page that just loaded
    /// should not scroll itself before the reader has seen its top.
    fn find_on_new_document(&mut self) {
        self.find_stale = false;
        if self.find_query.is_empty() {
            self.find_hits.clear();
            self.find_active = None;
            return;
        }
        let Some(doc) = self.doc.as_ref() else { return };
        self.find_hits = find::collect(doc, &self.find_query);
        self.find_active = if self.find_hits.is_empty() {
            None
        } else {
            Some(0)
        };
        self.emit_find_result();
    }

    /// This frame's highlights, measured from the live layout. A query whose
    /// text may have moved (the document settled) is re-run first, so a stale
    /// byte range never paints the wrong glyphs.
    pub(crate) fn find_highlights(&mut self, scale: f64) -> Vec<find::Highlight> {
        if self.find_query.is_empty() {
            return Vec::new();
        }
        if self.find_stale {
            self.find_rerun(false);
        }
        let Some(doc) = self.doc.as_ref() else {
            return Vec::new();
        };
        find::highlight_rects(doc, &self.find_hits, self.find_active, scale)
    }

    // -- devtools inspector (the F12 tree + "inspect element") ---------------

    /// Open or close the Inspector pane for this tab. Opening makes the tab
    /// responsible for the shell's tree: the current one goes out immediately,
    /// and [`Tab::inspect_check`] keeps it current from here on. Closing drops
    /// the whole tab-side half — rows, ids, selection, marker — and stops the
    /// stream; nothing about the page changes either way.
    fn set_inspect(&mut self, open: bool) {
        if open == self.inspect_enabled {
            return;
        }
        self.inspect_enabled = open;
        if !open {
            self.inspect_pick = false;
            self.inspect_selected = None;
            self.inspect_hover = None;
            self.inspect_nodes.clear();
            self.inspect_fp = None;
            info!("inspect: pane closed");
            // The marker was painted on the page; closing the pane takes it off.
            self.needs_paint = true;
            self.maybe_repaint();
            return;
        }
        info!("inspect: pane open");
        self.inspect_snapshot(true);
        self.flush_stdout();
        self.needs_paint = true;
        self.maybe_repaint();
    }

    /// Arm or disarm the *inspect element* crosshair (the shell's Ctrl-Shift-C
    /// and its context-menu item). Arming swallows pointers (see
    /// [`Tab::handle_pointer`]); disarming leaves the selection and its marker
    /// alone but takes the hovered one, and tells the shell the cursor stopped
    /// answering.
    fn set_inspect_pick(&mut self, armed: bool) {
        if self.inspect_pick == armed {
            return;
        }
        self.inspect_pick = armed;
        debug!("inspect: picker {}", if armed { "armed" } else { "disarmed" });
        if !armed && self.inspect_hover.take().is_some() {
            self.emit(FromTab::InspectHover { node: None });
            self.needs_paint = true;
            self.maybe_repaint();
        }
    }

    /// The shell clicked a row of the tree or a breadcrumb step (or sent `None`
    /// to clear): paint that element's box and scroll it fully into view when
    /// the page shows it only partly. An id of a **previous** generation
    /// resolves to nothing, so both the marker and the scroll do nothing — the
    /// contract's "ignored, never an error", for a shell that may be a message
    /// behind the page.
    fn inspect_select(&mut self, node: Option<u64>) {
        self.inspect_selected = node;
        if let Some(rect) = node.and_then(|id| self.inspect_box(id)) {
            let (w, h) = self.inspect_viewport();
            if let Some(doc) = self.doc.as_mut() {
                inspector::scroll_rect_into_view(doc, rect, w, h);
            }
        }
        self.needs_paint = true;
        self.maybe_repaint();
    }

    /// The deepest element at a point in [`ToTab::Pointer`] space — window CSS
    /// px, with the page zoom still multiplied in, exactly as the shell sends it
    /// (the house hit-tests divide by zoom first, see [`Tab::context_target`]).
    /// `None` for a point outside the viewport, or for no document.
    fn inspect_hit(&self, x: f32, y: f32) -> Option<u64> {
        let z = self.zoom();
        let doc = self.doc.as_ref()?;
        inspector::hit_element_at_with(doc, x / z, y / z, self.inspect_doctype.as_deref())
    }

    /// The pick — a click with the crosshair armed, or the context menu's
    /// **Inspect Element** ([`ToTab::InspectPickAt`], the same thing minus the
    /// event): select what is under the point, mark it, scroll it into view, and
    /// answer with the same generation carrying the new selection plus
    /// [`FromTab::InspectPicked`]. For a real pick the crosshair then disarms
    /// itself — one pick per arming, like Firefox.
    ///
    /// This works with the pane **closed**, which is a context menu's normal
    /// state: the shell asked, so it gets the snapshot and the highlight, and
    /// opening the pane sends [`ToTab::SetInspect`] to take over the stream.
    fn inspect_pick_at(&mut self, x: f32, y: f32) {
        let Some(id) = self.inspect_hit(x, y) else {
            debug!("inspect: nothing under ({x:.0}, {y:.0}) to inspect");
            return;
        };
        debug!("inspect: picked row {id} at ({x:.0}, {y:.0})");
        self.inspect_select(Some(id));
        self.inspect_snapshot(true);
        self.flush_stdout();
        self.emit(FromTab::InspectPicked { node: id });
        self.set_inspect_pick(false);
        self.flush_stdout();
    }

    /// While the crosshair is armed, what is under the cursor: reported as it
    /// changes (never twice for the same element) and painted on the page, so
    /// the user sees what a click would take before taking it.
    fn inspect_hover_at(&mut self, ev: &PointerEvent) {
        let id = match ev.kind {
            PointerKind::Out => None,
            PointerKind::Over | PointerKind::Move => self.inspect_hit(ev.x, ev.y),
            // A press is not a change of cursor; the release is the pick.
            PointerKind::Down | PointerKind::Up => self.inspect_hover,
        };
        if id == self.inspect_hover {
            return;
        }
        self.inspect_hover = id;
        self.emit(FromTab::InspectHover { node: id });
        self.needs_paint = true;
        self.maybe_repaint();
    }

    /// A new document. Its nodes are new arena entries and the previous
    /// generation's handles must never be resolved again, so they go, and the
    /// selection dies with them. An open pane gets this tree under a fresh
    /// generation (contract: a snapshot after every navigation while it is open).
    fn inspect_on_new_document(&mut self) {
        self.inspect_gen += 1;
        self.inspect_nodes.clear();
        self.inspect_selected = None;
        self.inspect_hover = None;
        self.inspect_fp = None;
        if self.inspect_enabled {
            self.inspect_snapshot(true);
            self.flush_stdout();
        }
    }

    /// The poll hook, run from [`Tab::check_meta`] after every settle: re-walk
    /// the settled document and ship only when the walk differs from the
    /// snapshot the shell holds. An idle page with the pane open costs one walk
    /// per settle and no IPC; a script that appends a `<section>` reaches the
    /// pane on the next tick (see [`inspector::fingerprint`]).
    fn inspect_check(&mut self) {
        if self.inspect_enabled {
            self.inspect_snapshot(false);
        }
    }

    /// Walk the current document and — unless the walk is the tree the shell
    /// already has (`force` skips that comparison) — ship it as
    /// [`FromTab::InspectSnapshot`]. The walk's row→node handles are kept either
    /// way: a row set the fingerprint matches still names the nodes to paint and
    /// to hit-test, and keeping them refreshed is what survives a page that
    /// removes and re-adds identical markup.
    ///
    /// Returns the snapshot whether or not it went out, which is how
    /// `--inspect-out` dumps the wire format instead of re-deriving a tree.
    fn inspect_snapshot(&mut self, force: bool) -> Option<InspectSnapshot> {
        let (url, walk) = {
            let doc = self.doc.as_ref()?;
            let url = self
                .url_override
                .clone()
                .unwrap_or_else(|| doc.url().as_str().to_owned());
            let walk = inspector::walk(
                doc,
                &url,
                self.inspect_gen,
                self.inspect_selected,
                self.inspect_doctype.as_deref(),
            );
            (url, walk)
        };
        let snapshot = walk.snapshot;
        let stamp = inspector::fingerprint(&snapshot);
        // The row→node handles are kept **either** way, before the comparison: a
        // page that removes and re-adds identical markup fingerprints the same
        // while blitz recycles the arena ids underneath it, and the marker, the
        // scroll and the hit-test all resolve rows through this table.
        self.inspect_nodes = walk.nodes;
        if !force && self.inspect_fp == Some(stamp) {
            return Some(snapshot);
        }
        self.inspect_fp = Some(stamp);
        debug!(
            "inspect: snapshot gen {} · {} row(s){} · {url}",
            snapshot.generation,
            snapshot.nodes.len(),
            if snapshot.truncated { " (truncated)" } else { "" },
        );
        self.emit(FromTab::InspectSnapshot {
            snapshot: snapshot.clone(),
        });
        Some(snapshot)
    }

    /// The live border box of a row, in CSS px (see [`inspector::box_rect`]).
    /// `None` for a stale id, for a non-element row, or for a node a script
    /// removed — which is also why a stale selection paints nothing instead of
    /// guessing at a box.
    fn inspect_box(&mut self, id: u64) -> Option<Rect> {
        let node = inspector::node_of(&self.inspect_nodes, id)?;
        let doc = self.doc.as_ref()?;
        let node = doc.get_node(node)?;
        inspector::box_rect(doc, node)
    }

    /// The viewport the rows are measured against: the window in CSS px, the
    /// same reading [`Tab::focus_active_hit`] takes for find's jump.
    fn inspect_viewport(&self) -> (f64, f64) {
        (f64::from(self.width), f64::from(self.height))
    }

    /// This frame's marker(s): the selection, plus the hovered element while the
    /// crosshair is armed (at most one of each). Measured from the live document
    /// **every frame it is shown**, so scrolling and scripted restyles move the
    /// box without a new snapshot — and it never asks for a repaint of its own
    /// (contract: "never a repaint loop"), so an idle page with a selection
    /// costs geometry and no frames.
    pub(crate) fn inspect_highlights(&mut self, scale: f64) -> Vec<inspector::Highlight> {
        let mut nodes = Vec::new();
        for id in [self.inspect_selected, self.inspect_hover]
            .into_iter()
            .flatten()
        {
            if let Some(node) = inspector::node_of(&self.inspect_nodes, id) {
                if !nodes.contains(&node) {
                    nodes.push(node);
                }
            }
        }
        if nodes.is_empty() {
            return Vec::new();
        }
        let Some(doc) = self.doc.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for node in nodes {
            out.extend(inspector::marker(doc, node, scale));
        }
        out
    }

    /// `--inspect-out` (headless harness): the snapshot a DevTools pane would be
    /// handed, produced by the commands and the walk the live tab uses. The dump
    /// is the wire format, not a second serializer. (A headless tab's `FromTab`
    /// sink is `/dev/null`, hence handing the tree back directly.)
    pub(crate) fn inspect_dump(&mut self) -> Option<InspectSnapshot> {
        self.inspect_snapshot(true)
    }

    // -- shell event handling -------------------------------------------------

    pub fn handle_to_tab(&mut self, msg: ToTab) -> Option<i32> {
        // Raw shell traffic: commands are `debug`, the input flood (every
        // mouse packet) is `trace` so `--log debug` stays readable.
        match &msg {
            ToTab::Pointer(_) | ToTab::Key(_) | ToTab::Scroll(_) => trace!("from shell: {msg:?}"),
            other => debug!("from shell: {other:?}"),
        }
        match msg {
            ToTab::Navigate { url } => {
                // The address bar is the user: this navigation may carry the
                // archive overrides and open a local file.
                self.nav_from_shell = true;
                self.navigate(&url, NavHistory::Fresh);
            }
            ToTab::Reload => {
                self.nav_from_shell = true;
                let Some(url) = self.history.get(self.history_index).cloned() else {
                    return None;
                };
                let session = self.history_index;
                self.navigate(url.as_str(), NavHistory::Session(session));
            }
            ToTab::Stop => {} // v1 no-op placeholder, per contract
            ToTab::Back => {
                if self.history_index > 0 {
                    self.history_index -= 1;
                    self.emit_history_changed();
                    let url = self.history[self.history_index].clone();
                    let session = self.history_index;
                    self.navigate(url.as_str(), NavHistory::Session(session));
                }
            }
            ToTab::Forward => {
                if self.history_index + 1 < self.history.len() {
                    self.history_index += 1;
                    self.emit_history_changed();
                    let url = self.history[self.history_index].clone();
                    let session = self.history_index;
                    self.navigate(url.as_str(), NavHistory::Session(session));
                }
            }
            ToTab::Resize {
                width,
                height,
                scale,
            } => {
                self.width = width.max(1);
                self.height = height.max(1);
                self.scale = if scale.is_finite() && scale > 0.0 {
                    scale
                } else {
                    self.scale
                };
                // The realm reads innerWidth/innerHeight live: push the new
                // CSS-px size before the page's resize handlers run.
                if let Some(runtime) = self.js.as_ref() {
                    let mut host = runtime.host().borrow_mut();
                    host.viewport_width = self.width;
                    host.viewport_height = self.height;
                }
                let viewport = self.viewport();
                if let Some(doc) = self.doc.as_mut() {
                    // `set_viewport` is THE resize entry point (no resize()
                    // in beta.2) — rebuilds the Stylo device and clamps scroll.
                    doc.set_viewport(viewport);
                }
                self.needs_paint = true;
                self.last_paint = None; // force a frame at the new size
                self.resolve_and_maybe_paint();
                // Browsers fire `resize` on the window AFTER the viewport and
                // layout have settled (task: React geometry) — a page's
                // responsive handler must read the new innerWidth, not the old.
                if let (Some(runtime), Some(doc)) = (self.js.as_mut(), self.doc.as_mut()) {
                    runtime.fire_window_event(doc, "resize");
                }
            }
            ToTab::SetZoom { zoom } => {
                // The shell owns the step/clamp policy; the tab only rejects
                // nonsense values and keeps the frame honest.
                let zoom = if zoom.is_finite() && zoom > 0.0 {
                    zoom.clamp(0.1, 16.0)
                } else {
                    self.zoom
                };
                info!("zoom → {:.0}%", zoom * 100.0);
                self.zoom = zoom;
                let viewport = self.viewport();
                if let Some(doc) = self.doc.as_mut() {
                    // `set_viewport` rebuilds the Stylo device (zoom feeds
                    // root font sizes), clamps scroll, and invalidates
                    // inline contexts — exactly the zoom restyle entry.
                    doc.set_viewport(viewport);
                }
                self.needs_paint = true;
                self.last_paint = None; // force a frame at the new zoom
                self.resolve_and_maybe_paint();
            }
            ToTab::SetVisible { visible } => {
                self.visible = visible;
                if visible {
                    self.needs_paint = true;
                    self.last_paint = None;
                    self.resolve_and_maybe_paint();
                }
            }
            ev @ (ToTab::Pointer(_) | ToTab::Key(_) | ToTab::Scroll(_)) => {
                self.handle_input(&ev);
            }
            ToTab::ArchiveServerInfo { ok, ref address, ref error, .. } => {
                netmod::set_archive_server_status(Some(netmod::ArchiveServerStatus {
                    ok,
                    address: address.clone(),
                    error: error.clone(),
                }));
                if ok {
                    info!("archive server answered at {address}");
                } else {
                    warn!("archive server: {error:?}");
                }
                // This tab is still sitting on `ironsurf://archive`: without
                // `?open=1` (which the shell answers with a *new* tab on the
                // listing), the page's own job is done and it steps through to
                // the server's control page, where the folder/host/port are
                // editable. A tab that already navigated elsewhere (`None`)
                // is never dragged along.
                let asked_for_new_tab = self.archive_page.as_deref().is_some_and(|page| {
                    page.split_once('?')
                        .map(|(_, query)| {
                            query.split('&').any(|pair| {
                                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                                key == "open" && !value.is_empty()
                            })
                        })
                        .unwrap_or(false)
                });
                if ok && !asked_for_new_tab {
                    if let Some(page) = self.archive_page.take() {
                        let control = format!(
                            "{}/{}",
                            address.trim_end_matches('/'),
                            netmod::ARCHIVE_CONTROL_PATH.trim_start_matches('/')
                        );
                        debug!("archive page {page} → the server's control page {control}");
                        self.navigate(&control, NavHistory::Fresh);
                    }
                }
            }
            ToTab::HistoryList { ref entries, total, .. } => {
                let fresh =
                    netmod::HistoryListing { entries: entries.clone(), total };
                // A page still showing the answer it asked with (`None`, or an
                // older log) renders the new one in place — same session slot,
                // so Back still steps over the history page once. An answer that
                // repeats what is on screen changes nothing, and that is what
                // keeps the reload after the request from looping.
                let stale =
                    self.history_page.is_some() && self.history_rendered.as_ref() != Some(&fresh);
                info!("history page: {total} visit(s) in the shared log");
                netmod::set_history_listing(Some(fresh));
                if stale {
                    // `?clear=1` (the page's "Clear history" link) is a one-shot:
                    // the re-render asks for the plain page, or it would clear
                    // the freshly emptied log again on every reload.
                    self.navigate(netmod::HISTORY_URL, NavHistory::Session(self.history_index));
                }
            }
            ToTab::Clipboard { text } => {
                // The shell pushes the session clipboard just before it
                // relays Ctrl+V; blitz's editors read it back through
                // `ShellProvider::get_clipboard_text`.
                *self.shell.clipboard.lock().expect("clipboard mutex") = text;
            }
            ToTab::ContextAction(action) => self.handle_context_action(action),
            ToTab::SetUserscripts { scripts } => {
                // Replace-wholesale per the contract: the shell sends every
                // ENABLED script (patterns included) and an empty vec clears
                // the tab-side set. Scripts apply to the next document that
                // loads — the per-URL filter runs at script time, never here.
                info!(
                    "userscripts: {} script(s) installed{}",
                    scripts.len(),
                    if scripts.is_empty() {
                        " (set cleared)"
                    } else {
                        ""
                    }
                );
                self.userscripts = scripts;
            }
            ToTab::SetUserAgent { user_agent } => {
                // Replace the cell's contents in place: every request issued
                // after this point (documents, subresources, form POSTs,
                // redirect hops) and `navigator.userAgent` carry the new
                // value; `Some(v)` trims to empty or `None` reset to the
                // built-in default. The contract says nothing reloads — the
                // live document stands until the next navigation.
                let effective = netmod::resolve_user_agent(user_agent.as_deref());
                info!("user-agent \u{2192} {effective}");
                *self
                    .user_agent
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = effective;
            }
            ToTab::SetDoNotTrack { enabled } => {
                // The container's private-browsing flag flipped (task:
                // private browsing). From this point every request —
                // documents, subresources, form POSTs, redirect hops,
                // XHR/fetch — carries `DNT: 1` while `enabled`, and none
                // carries it once cleared. Nothing reloads — the live
                // document stands until its next request; history
                // recording is shell-side and not this tab's concern.
                netmod::set_do_not_track(enabled);
                info!("do-not-track \u{2192} {enabled}");
            }
            ToTab::SetProxy { proxy } => {
                // The container's proxy moved (re-selected, cleared, or one
                // of the shell's definitions edited). From this point every
                // request — documents, subresources, form POSTs, redirect
                // hops, XHR/fetch, media, downloads — rides the new proxy;
                // `None` (or blank) means direct again. A URL reqwest won't
                // parse is a contract break at the shell boundary: warn,
                // stay on the previous proxy rather than silently sending
                // authenticated traffic direct. Nothing reloads — the live
                // document stands until its next request.
                match netmod::set_process_proxy(proxy.as_deref()) {
                    Ok(()) => {
                        match proxy.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                            Some(u) => info!("proxy → {u}"),
                            None => info!("proxy → direct"),
                        }
                    }
                    Err(err) => warn!("{err}; keeping the current proxy"),
                }
            }
            ToTab::SetArchiveMode { enabled } => {
                // The 💾 toggle: from here on (or off), every successful
                // http(s) response — documents, subresources, scripts, media
                // — is queued for `FromTab::ArchivedFetch` and written by
                // the shell. Nothing is written inside the tab.
                info!("archive mode → {}", if enabled { "on" } else { "off" });
                self.archive.set_enabled(enabled);
            }
            // Find in page: the shell owns the find bar (the chord, the text
            // entry, the counters); the tab owns the document, so matching,
            // highlighting and jumping happen here (see src/find.rs).
            ToTab::SetFind { query } => self.set_find(query),
            ToTab::FindNext => self.find_jump(true),
            ToTab::FindPrevious => self.find_jump(false),
            // The tab half of the DevTools Inspector (task: Inspector — tab side,
            // see src/inspector.rs): the shell owns the pane, the folding and the
            // selection it drives; we own the walk, the ids, the marker and the
            // hit-tests. Dispatched outside the stdin block, like the find bar's
            // commands, because the pane wants its first tree now — not on the
            // tick after the shell's next message.
            ToTab::SetInspect { open } => self.set_inspect(open),
            ToTab::SetInspectPick { enabled } => self.set_inspect_pick(enabled),
            ToTab::InspectSelect { node } => self.inspect_select(node),
            ToTab::InspectPickAt { x, y } => self.inspect_pick_at(x, y),
            ToTab::SetReader { enabled } => self.set_reader(enabled),
            ToTab::SetPopupPolicy { block, allowed } => self.set_popup_policy(block, allowed),
            ToTab::SetScrollSpeed { speed } => {
                // The settings menu's slider: a multiplier on the default
                // damping (like zoom, the shell owns the policy — the tab
                // only rejects nonsense and clamps hard).
                self.wheel_speed = if speed.is_finite() && speed > 0.0 {
                    WHEEL_SPEED * (speed as f64).clamp(0.1, 4.0)
                } else {
                    WHEEL_SPEED
                };
                info!("wheel speed → {speed:.2}× ({:.2} px per unit)", self.wheel_speed);
            }
            ToTab::DialogResponse { dialog_id, .. } => {
                // A reply with no modal open: it either raced the tab-side
                // cancel (shell quit mid-dialog) or is stale — nothing to
                // apply, and never a protocol error.
                warn!("dialog response #{dialog_id} arrived with no dialog open — ignored");
            }
            // "Download linked file" (task: file downloads): the shell's
            // context menu asked, so save it — never render it, even an
            // `http(s)` URL that would paint. The fetch runs on a worker
            // thread; the reply is `DownloadStart` → chunks → `DownloadEnd`,
            // and a URL the tab can never fetch gets the same sequence with
            // a chunk-free `DownloadEnd { error }`, so a row always settles.
            ToTab::DownloadUrl { url, suggested_name } => {
                match parse_input_url(&url).ok() {
                    Some(target) => match download_target_problem(&target) {
                        Some(reason) => self.refuse_download(
                            target.as_str(),
                            suggested_name,
                            DownloadTrigger::Requested,
                            reason,
                        ),
                        None => self.start_download(
                            target,
                            suggested_name,
                            DownloadTrigger::Requested,
                            None,
                        ),
                    },
                    None => self.refuse_download(
                        &url,
                        suggested_name,
                        DownloadTrigger::Requested,
                        "that address is not a URL the tab can fetch".to_owned(),
                    ),
                }
            }
            ToTab::Destroy => {
                self.persist();
                return Some(0);
            }
        }
        None
    }

    /// Input entry point (task 7d): map `ToTab::Pointer/Key/Scroll` onto
    /// `blitz_traits::events::UiEvent` and feed `document.handle_ui_event`,
    /// then `resolve` + coalesced repaint (`docs/BLITZ_API_NOTES.md` §4).
    ///
    /// Coordinate space: ironsurf shell coordinates are logical px (the same
    /// space as `ToTab::Resize`'s `width`/`height`); blitz hit-tests and
    /// default actions run in **CSS px**, which for `Viewport::new(w*scale,
    /// h*scale, scale, …)` equals those logical px. `screen`/`client` are the
    /// unscrolled viewport position, `page` adds the document scroll offset
    /// (what blitz's hover/click hit-testing consumes, events/driver.rs).
    fn handle_input(&mut self, ev: &ToTab) {
        // Must this event produce pixels? A plain hover move only does when it
        // changes what is under the cursor: a mouse travelling across the page
        // fires one event per device packet, and each `resolve` + vello_cpu
        // repaint costs hundreds of milliseconds — unconditionally repainting
        // buried the click that follows under seconds of stale frames.
        let mut paint = true;
        match ev {
            ToTab::Pointer(pointer) => paint = self.handle_pointer(pointer),
            ToTab::Key(key) => self.handle_key(key),
            ToTab::Scroll(scroll) => self.handle_scroll(scroll),
            _ => {}
        }
        // Links announce themselves: hovering an `<a href>` puts its target in
        // the status bar, leaving it clears that again.
        self.report_hover();
        // …and the pointer takes the shape the page asks for (task: hover
        // cursor), reported from the very same hover state, only on change.
        match ev {
            ToTab::Pointer(pointer) => self.report_cursor(
                pointer.x,
                pointer.y,
                pointer.kind == PointerKind::Out,
            ),
            // A scroll moves the page under a stationary pointer: what sits
            // under the cursor may well have changed with it.
            ToTab::Scroll(scroll) => self.report_cursor(scroll.x, scroll.y, false),
            _ => {}
        }
        if !paint {
            return;
        }
        // Every handled input wants fresh pixels (contract §ironsurf-tab.3);
        // still coalesced by the ≤30 fps throttle in `maybe_repaint`.
        self.needs_paint = true;
        // A link click filled the NavigationProvider hook inline (or a form
        // press filled `pending_form`); skip painting the outgoing page — the
        // loop's `drain_nav_hook` replaces the document (and paints the
        // incoming one) right after this returns.
        if self.pending_form.is_none() && self.nav.pending.lock().expect("nav mutex").is_none() {
            self.resolve_and_maybe_paint();
        }
    }

    /// Nearest `<a href>` target under the cursor, resolved against the
    /// document URL. `None` = not over a link; a `#fragment` target is kept
    /// (blitz scrolls in place for those).
    fn hover_link(&self) -> Option<String> {
        self.hover_anchor().map(|(href, _)| href)
    }

    /// The link under the cursor and what it asks for: `(resolved href, target)`
    /// where the target is `Some(name)` **only when the `target` attribute names
    /// another window** (`_blank` or a window name) — `_self`, `_top`, `_parent`,
    /// `""` and an absent attribute all mean “this tab” and read as `None`
    /// (task: block popups; `popupTargetName` in the JS prelude applies the same
    /// rule to `window.open` and to `el.click()`).
    fn hover_anchor(&self) -> Option<(String, Option<String>)> {
        let doc = self.doc.as_ref()?;
        let mut node_id = doc.get_hover_node_id()?;
        while let Some(node) = doc.get_node(node_id) {
            if let Some(el) = node.element_data() {
                if el.name.local == blitz_dom::local_name!("a") {
                    if let Some(href) = el.attr(blitz_dom::local_name!("href")) {
                        let href = href.trim();
                        if !href.is_empty() {
                            let resolved = doc
                                .url()
                                .join(href)
                                .map(|u| u.to_string())
                                .unwrap_or_else(|_| href.to_owned());
                            let target = crate::popup::new_window_target(el.attr(
                                blitz_dom::local_name!("target"),
                            ));
                            return Some((resolved, target));
                        }
                    }
                }
            }
            node_id = node.parent?;
        }
        None
    }

    /// Hit-test a right-click at (`x`, `y`) — logical px, divided by zoom
    /// into blitz's CSS-px viewport space — and gather everything the shell
    /// needs to build the context menu (task: context menu). Ancestors are
    /// walked to the nearest interesting element, the same way
    /// [`Self::hover_link`] finds `<a href>`. Returns the context alongside
    /// the `<video>` node (if any) so `ContextAction::ToggleVideo` can find
    /// the very same player.
    fn context_target(&self, x: f32, y: f32) -> (ContextTarget, Option<NodeId>) {
        let mut target = ContextTarget::default();
        let Some(doc) = self.doc.as_ref() else {
            return (target, None);
        };
        let z = self.zoom();
        let mut video_node = None;
        if let Some(start) = doc.element_from_point(x / z, y / z) {
            let mut node_id = start;
            while let Some(node) = doc.get_node(node_id) {
                if let Some(el) = node.element_data() {
                    let name = el.name.local.clone();
                    if name == blitz_dom::local_name!("a") && target.link.is_none() {
                        target.link = resolved_attr(doc, el, blitz_dom::local_name!("href"));
                    }
                    if name == blitz_dom::local_name!("img") && target.image.is_none() {
                        target.image = resolved_attr(doc, el, blitz_dom::local_name!("src"));
                    }
                    if name == blitz_dom::local_name!("video") && video_node.is_none() {
                        video_node = Some(node_id);
                        // A `src`-less `<video>` is still a video (its player
                        // exists); report the empty URL rather than nothing.
                        target.video =
                            Some(resolved_attr(doc, el, blitz_dom::local_name!("src")).unwrap_or_default());
                    }
                    if !target.editable && is_editable_element(el) {
                        target.editable = true;
                    }
                }
                node_id = match node.parent {
                    Some(p) => p,
                    None => break,
                };
            }
        }
        if let Some(video) = video_node {
            target.video_playing = self
                .media
                .iter()
                .find(|entry| entry.node_id == video)
                .map(|entry| entry.player.borrow().is_playing())
                .unwrap_or(false);
        }
        // Page text selection (the menu's Copy entry); whitespace-only and
        // over-cap selections are no selection at all.
        target.selection = doc
            .get_selected_text()
            .filter(|text| !text.trim().is_empty())
            .map(|mut text| {
                if text.len() > MAX_CONTEXT_TEXT {
                    let mut cut = MAX_CONTEXT_TEXT;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                }
                text
            });
        // A paste also makes sense with the cursor over anything at all, as
        // long as a text control currently holds focus.
        if !target.editable {
            target.editable = doc
                .get_focussed_node_id()
                .and_then(|id| doc.get_node(id))
                .and_then(|node| node.element_data())
                .is_some_and(is_editable_element);
        }
        (target, video_node)
    }

    /// A context-menu choice the *engine* services (task: context menu).
    fn handle_context_action(&mut self, action: ContextAction) {
        match action {
            ContextAction::Paste { text } => {
                // Load the cache the editors read, then hand blitz a Ctrl+V:
                // its focused-input path inserts the clipboard verbatim
                // (blitz-dom node/text.rs), caret/selection semantics and all.
                *self.shell.clipboard.lock().expect("clipboard mutex") = text;
                let pressed = BlitzKeyEvent {
                    key: Key::Character("v".to_string()),
                    code: Code::KeyV,
                    modifiers: blitz_action_chord(),
                    location: Location::Standard,
                    is_auto_repeating: false,
                    is_composing: false,
                    state: KeyState::Pressed,
                    text: None,
                };
                self.feed_ui_event(UiEvent::KeyDown(pressed.clone()));
                self.feed_ui_event(UiEvent::KeyUp(BlitzKeyEvent {
                    state: KeyState::Released,
                    ..pressed
                }));
                debug!("context menu: pasted into the focused control");
                self.needs_paint = true;
                self.resolve_and_maybe_paint();
            }
            ContextAction::ToggleVideo => {
                let Some(video) = self.context_video else {
                    debug!("context menu: no video under the last right-click");
                    return;
                };
                if let Some(entry) = self.media.iter().find(|e| e.node_id == video) {
                    let mut player = entry.player.borrow_mut();
                    if player.is_playing() {
                        player.pause();
                        debug!("context menu: video paused");
                    } else {
                        player.play();
                        debug!("context menu: video playing");
                    }
                    self.needs_paint = true;
                }
            }
        }
    }

    /// Ship every subresource request that settled since the last poll as
    /// one `FromTab::NetworkEvent` each (DevTools network panel): the fetch
    /// threads push into the provider's queue, and the NetWaker wake that
    /// follows makes this drain immediate — never a tick late.
    fn drain_net_events(&mut self) {
        let events = std::mem::take(&mut *self.net_events.lock().expect("net events mutex"));
        for event in events {
            self.emit(FromTab::NetworkEvent { event });
        }
    }

    /// Ship every response archived since the last poll as one
    /// `FromTab::ArchivedFetch` each (the shell's 💾 archive mode): the fetch
    /// threads, the script-src fetcher, and the media worker push into the
    /// shared sink, and this loop drains it — documents drained inline with
    /// the navigation just like their network events.
    fn drain_archive_events(&mut self) {
        for fetch in self.archive.take_pending() {
            self.emit(FromTab::ArchivedFetch {
                url: fetch.url,
                is_document: fetch.is_document,
                bytes: fetch.bytes,
            });
        }
    }

    /// Surface whatever the page copied (Ctrl+C / Ctrl+X / editor cut) through
    /// the `ShellProvider` hook as `FromTab::ClipboardText` — the shell owns
    /// the session clipboard (task: context menu).
    fn drain_copied(&mut self) {
        let copied = self.shell.copied.lock().expect("clipboard mutex").take();
        if let Some(text) = copied {
            self.emit(FromTab::ClipboardText { text });
        }
    }

    /// Emit `FromTab::StatusText` when the hovered link target changes (and
    /// only then) — this is the user-visible signal that the page is not a
    /// picture: hovering a link shows where the click would take them.
    fn report_hover(&mut self) {
        let target = self.hover_link();
        if target == self.hover_status {
            return;
        }
        self.hover_status = target.clone();
        self.emit(FromTab::StatusText { text: target });
    }

    /// Emit `FromTab::CursorChanged` when the pointer shape wanted under
    /// (`x`, `y`) — logical px — changes (task: hover cursor); `left_viewport`
    /// is the pointer leaving the page (`Default` — the shell's own chrome
    /// owns the cursor again). Emitted from the same hover state as
    /// [`Self::report_hover`], and only on change, like `StatusText`.
    fn report_cursor(&mut self, x: f32, y: f32, left_viewport: bool) {
        let icon =
            if left_viewport { CursorIcon::Default } else { self.cursor_under(x, y) };
        if self.cursor_status == Some(icon) {
            return;
        }
        self.cursor_status = Some(icon);
        debug!("cursor → {icon:?} at ({x:.0},{y:.0})");
        self.emit(FromTab::CursorChanged { cursor: icon });
    }

    /// The pointer shape for the point (`x`, `y`) (logical px). One walk up
    /// from the hit node, deepest first: a computed CSS `cursor:` keyword
    /// (inherited, so an explicit one shows up at every descendant) is the
    /// page speaking and always wins; then this engine's clickability
    /// affordances — controls a click actuates anyway (so the hand appears
    /// over a `<button>`'s label too) and the clickable regions of a
    /// `<video>` widget. Nothing asked ⇒ blitz's general resolution decides
    /// the leftovers (I-beam over selectable text, hand over ancestor links).
    fn cursor_under(&self, x: f32, y: f32) -> CursorIcon {
        let Some(doc) = self.doc.as_ref() else { return CursorIcon::Default };
        let z = self.zoom();
        let (cx, cy) = (x / z, y / z);
        let Some(start) = doc.element_from_point(cx, cy).or_else(|| doc.get_hover_node_id())
        else {
            return CursorIcon::Default;
        };
        let mut node_id = start;
        while let Some(node) = doc.get_node(node_id) {
            if let Some(styles) = node.primary_styles() {
                if let Some(icon) = css_cursor(&styles) {
                    return icon;
                }
            }
            if let Some(el) = node.element_data() {
                if is_clickable_element(el) {
                    return CursorIcon::Pointer;
                }
                if el.name.local == blitz_dom::local_name!("video") {
                    let icon = self.video_cursor(doc, node_id, cx, cy);
                    if icon != CursorIcon::Default {
                        return icon;
                    }
                }
            }
            node_id = match node.parent {
                Some(p) => p,
                None => break,
            };
        }
        match doc.get_cursor() {
            Some(BlitzCursorIcon::Text) => CursorIcon::Text,
            Some(BlitzCursorIcon::Pointer) => CursorIcon::Pointer,
            _ => CursorIcon::Default,
        }
    }

    /// What the pointer means over a `<video>` element at viewport point
    /// (`cx`, `cy`) — CSS px. The element's own widget owns clicks on it,
    /// so [`VideoWidget::click_action`] answers with exactly the regions a
    /// click would actuate: the pointing hand over the picture and the
    /// play/pause glyph, the sideways I-beam over the position bar (dragging
    /// it scrubs the play head). Falls back to the CSS box while the widget
    /// has not painted its geometry yet.
    fn video_cursor(&self, doc: &HtmlDocument, node_id: NodeId, cx: f32, cy: f32) -> CursorIcon {
        // No box (unstyled/invisible): the element is still a clickable
        // thing — the hand is the honest answer.
        let Some(rect) = doc.get_client_bounding_rect(node_id) else {
            return CursorIcon::Pointer;
        };
        let at = (cx - rect.x as f32, cy - rect.y as f32);
        let painted = self
            .media
            .iter()
            .find(|entry| entry.node_id == node_id)
            .map(|entry| entry.player.borrow().painted_geometry());
        let (w, h) = match painted {
            Some((w, h)) if w > 0.0 && h > 0.0 => (w, h),
            // Before the first paint the CSS box stands in for the widget's.
            _ => (rect.width as f32, rect.height as f32),
        };
        match VideoWidget::click_action(w, h, at) {
            crate::media::widget::Click::TogglePlay => CursorIcon::Pointer,
            crate::media::widget::Click::Seek { .. } => CursorIcon::EwResize,
            crate::media::widget::Click::Nothing => CursorIcon::Default,
        }
    }

    /// Pointer coordinates (logical px) → blitz `PointerCoords` (CSS px,
    /// with the document scroll added into the page axes).
    fn pointer_coords(&mut self, x: f32, y: f32) -> PointerCoords {
        // Logical px map to CSS layout px through the zoom factor (a page at
        // 200% is half as wide in CSS px, and the point at logical x sits at
        // CSS x/zoom).
        let z = self.zoom();
        let (x, y) = (x / z, y / z);
        // `viewport_scroll` is a `blitz_dom::util::Point<f64>` (no Default impl).
        let (scroll_x, scroll_y) = self
            .doc
            .as_ref()
            .map(|doc| {
                let s = doc.viewport_scroll();
                (s.x as f32, s.y as f32)
            })
            .unwrap_or((0.0, 0.0));
        PointerCoords {
            page_x: x + scroll_x,
            page_y: y + scroll_y,
            screen_x: x,
            screen_y: y,
            client_x: x,
            client_y: y,
        }
    }

    /// Feed one pointer event to blitz. Returns whether fresh pixels are
    /// wanted: presses/releases always are (active/focus/caret state), a bare
    /// hover move only when it moved onto a different element.
    fn handle_pointer(&mut self, ev: &PointerEvent) -> bool {
        use ironsurf_common::MouseButton;

        // **Inspect element** (task: Inspector — tab side): while the crosshair
        // is armed the page sees nothing — no hover DOM events, no link or
        // button actuation, no selection drag — because none of this reaches
        // blitz. The tab reports the cursor instead ([`FromTab::InspectHover`])
        // and a left release *is* the pick ([`FromTab::InspectPicked`], one per
        // arming, then it disarms itself).
        if self.inspect_pick {
            if matches!(ev.kind, PointerKind::Up) && ev.button == MouseButton::Left {
                self.inspect_pick_at(ev.x, ev.y);
            } else {
                self.inspect_hover_at(ev);
            }
            return true;
        }

        let button = match ev.button {
            MouseButton::Left => MouseEventButton::Main,
            MouseButton::Right => MouseEventButton::Secondary,
            MouseButton::Middle => MouseEventButton::Auxiliary,
        };

        // **A real user gesture just happened** (task: block popups): every
        // pointer event reaching a tab was put there by a human, and a press is
        // what the page's later `window.open` spends ([`crate::popup`]). Moves
        // arm nothing: a page may watch the cursor all day without earning a
        // window.
        if matches!(ev.kind, PointerKind::Down) {
            self.popups.arm();
        }

        // `<form>` presses (task: `<form>`), decided from blitz's live hover
        // node (the shell always moves the mouse before pressing). Nothing is
        // swallowed: clicking an `<option>` selects it (this engine renders
        // selects as always-open lists), and pressing a submit control *arms*
        // the submission — finalised on release, after the page's own JS
        // `click` handlers have run and possibly cancelled it.
        if ev.button == MouseButton::Left {
            if matches!(ev.kind, PointerKind::Down) {
                if let Some(hover) = self.doc.as_ref().and_then(|doc| doc.get_hover_node_id()) {
                    if let Some((select, option)) = self
                        .doc
                        .as_ref()
                        .and_then(|doc| forms::option_for_click(doc, hover))
                    {
                        if let Some(doc) = self.doc.as_mut() {
                            forms::apply_option_click(doc, select, option);
                        }
                        debug!("form: option selected");
                        self.needs_paint = true;
                    }
                    self.form_press = self
                        .doc
                        .as_ref()
                        .and_then(|doc| forms::submit_control_for_click(doc, hover))
                        .map(|(submitter, form)| FormPress {
                            submitter,
                            form,
                        });
                    // Label-bound checkbox/radio under the cursor (task:
                    // DDG anomaly tiles): resolve now, apply on release.
                    self.label_press = None;
                    if let Some(doc) = self.doc.as_ref() {
                        let z = self.zoom();
                        if let Some(hit) = doc.element_from_point(ev.x / z, ev.y / z) {
                            if let Some(act) = forms::label_activation_target(doc, hit) {
                                let before = forms::activation_checked(doc, act);
                                self.label_press = Some((act, before));
                            }
                        }
                    }
                }
                // The page sees the press too (mousedown handlers, :active
                // styling logic of any page script); its cancellation is the
                // engine's business only for the default action of the *pair*,
                // decided on release.
                self.fire_js_pointer("mousedown", ev);
            }
        }
        // Track the held-button set the way the DOM does: Down adds this
        // button, Up clears it (blitz reads `buttons` for drag/caret state,
        // events/mod.rs `may_move_text_input_caret`).
        // The right button is not tracked and never reaches blitz below: it
        // is the context-menu pair only (see the block after the middle
        // click), so a held right button must not look like a drag.
        match (ev.kind, ev.button) {
            (PointerKind::Down, MouseButton::Left | MouseButton::Middle) => {
                self.mouse_buttons |= MouseEventButtons::from(button)
            }
            (PointerKind::Up, MouseButton::Left | MouseButton::Middle) => {
                self.mouse_buttons = MouseEventButtons::None
            }
            _ => {}
        }

        // Middle-click release over a link → "open in new tab": the Up event
        // is deliberately *not* fed to blitz, so no DOM `click` fires, the
        // NavigationProvider stays empty, and this tab keeps its document.
        // The shell spawns the sibling tab (same container) on this message.
        if matches!(ev.kind, PointerKind::Up) && ev.button == MouseButton::Middle {
            if let Some(url) = self.hover_link() {
                self.emit(FromTab::OpenLinkInNewTab { url });
                return true;
            }
        }

        // The same gesture for readers whose mouse has no middle button: a
        // **primary-modifier left release** over a link (Ctrl on Linux/Windows,
        // ⌘ on macOS — `Modifiers::action`) opens it in a new tab instead of
        // navigating here. Handled exactly like the middle click above, and for
        // exactly the same reason: nothing of this release reaches blitz, so the
        // link's default action (this tab changing page) never happens. Over
        // anything that is not a link the modifiers are ignored and the click
        // goes through to the page untouched — a Ctrl-click on a checkbox is the
        // page's own business.
        if matches!(ev.kind, PointerKind::Up)
            && ev.button == MouseButton::Left
            && ev.mods.action()
        {
            if let Some(url) = self.hover_link() {
                // The page still sees its gesture — `mouseup` and `click`, with
                // the modifier flags set — and it may still say no: a handler
                // calling `preventDefault()` keeps this tab where it is.
                self.fire_js_pointer("mouseup", ev);
                let cancelled = self.fire_js_pointer("click", ev);
                // A handler that wrote to the page (its own usual thing) has to
                // be flushed the way every other JS turn is, or its title change
                // sits in the hook until something else repaints.
                self.needs_paint = true;
                self.take_js_title();
                self.check_meta();
                self.maybe_repaint();
                if cancelled {
                    debug!("action-modifier link click cancelled by the page: {url} stays put");
                } else {
                    debug!("action-modifier link click \u{2192} new tab: {url}");
                    self.emit(FromTab::OpenLinkInNewTab { url });
                }
                return true;
            }
        }

        // A left release over a link whose `target` names another window
        // (task: block popups): blitz ignores `target`, so the click's default
        // action comes back as a navigation **of this tab** — the one thing
        // `target="_blank"` does not mean. Mark it; the drain that picks up this
        // click's navigation turns it into a popup request, and a click the page
        // cancelled leaves nothing to answer.
        if matches!(ev.kind, PointerKind::Up) && ev.button == MouseButton::Left {
            self.new_window_click = self.hover_anchor().and_then(|(_, target)| target);
        }

        // The right-button press/release pair is the tab's own: it exists to
        // report what the cursor is over (link / image / video / selection /
        // editable) so the shell can open its context menu, and it is
        // deliberately never fed to blitz. Blitz collapses the text selection
        // on any button press (events/pointer.rs), which would eat the menu's
        // own Copy target — the selection would vanish between the press and
        // the menu opening; real browsers keep a selection through a
        // right-click. Nothing else is lost: the page never sees right-button
        // DOM events in this engine (the JS replay above is Left-only, and
        // blitz's `contextmenu` DomEvent has no consumer here). Menu choices
        // come back as `ToTab::ContextAction` or are serviced by the shell.
        if ev.button == MouseButton::Right && matches!(ev.kind, PointerKind::Down | PointerKind::Up) {
            if matches!(ev.kind, PointerKind::Up) {
                let (x, y) = (ev.x, ev.y);
                let (target, video) = self.context_target(x, y);
                self.context_video = video;
                debug!("context menu at ({x:.0},{y:.0}): {target:?}");
                self.emit(FromTab::ContextMenu { x, y, target });
            }
            return true;
        }

        // blitz synthesises enter/over/out/leave *DOM events* from
        // `PointerMove` (events/driver.rs::handle_pointer_move); there is no
        // Over/Out UiEvent, so ironsurf hover transitions arrive as moves.
        let hovering = matches!(
            ev.kind,
            PointerKind::Move | PointerKind::Over | PointerKind::Out
        ) && self.mouse_buttons.is_empty();
        let hover_before = self.doc.as_ref().and_then(|doc| doc.get_hover_node_id());

        let ui = match ev.kind {
            PointerKind::Move | PointerKind::Over | PointerKind::Out => {
                // Hover transitions carry the currently-held buttons (none for
                // a plain move), not the `button` field of the frame.
                UiEvent::PointerMove(self.pointer_event(ev, self.mouse_buttons))
            }
            PointerKind::Down => UiEvent::PointerDown(self.pointer_event(ev, self.mouse_buttons)),
            // On release `buttons` must already have the button cleared.
            PointerKind::Up => UiEvent::PointerUp(self.pointer_event(ev, MouseEventButtons::None)),
        };
        self.feed_ui_event(ui);

        // Release: blitz has just run the click default actions (which for a
        // submit control is its own submission — an incomplete entry list that
        // forgets selects, and nothing at all for `file:`/`ironsurf:` actions).
        // Replay the release through the page's JS first; then the tab's own
        // complete submission wins where the press armed one.
        if ev.button == ironsurf_common::MouseButton::Left
            && matches!(ev.kind, PointerKind::Up)
        {
            self.fire_js_pointer("mouseup", ev);
            let prevented = self.fire_js_pointer("click", ev);
            if let Some((act, before)) = self.label_press.take() {
                // blitz ran its click default during the Up above; if the
                // bound input is still exactly as the press found it, that
                // default never fired (its label-forwarding is dead for
                // inputs nested inside the label — the DDG tile shape).
                let now = self
                    .doc
                    .as_ref()
                    .and_then(|doc| forms::activation_checked(doc, act));
                if prevented {
                    debug!("label: activation cancelled by a JS click handler");
                } else if now.is_some() && now == before {
                    if let Some(doc) = self.doc.as_mut() {
                        forms::apply_label_activation(doc, act);
                        self.needs_paint = true;
                        debug!(
                            "label: click activated the bound {} input",
                            if act.radio { "radio" } else { "checkbox" }
                        );
                    }
                }
            }
            if let Some(press) = self.form_press.take() {
                // Whatever blitz's hook now holds was produced by this very
                // press — the tab's entry list replaces it either way.
                self.nav
                    .pending
                    .lock()
                    .expect("nav mutex")
                    .take();
                if prevented {
                    debug!("form: submission cancelled by a JS click handler");
                } else if let Some(doc) = self.doc.as_ref() {
                    match forms::build_submission(doc, press.form, Some(press.submitter)) {
                        Ok(sub) => {
                            debug!(
                                "form: {} submission of {} entry(s) → {}",
                                if sub.method == FormMethod::Post { "POST" } else { "GET" },
                                sub.entries,
                                sub.url
                            );
                            self.pending_form = Some(sub);
                        }
                        Err(err) => {
                            warn!("form submission failed: {err}");
                            self.emit(FromTab::StatusText {
                                text: Some(format!("form: {err}")),
                            });
                        }
                    }
                }
            }
        }

        if hovering {
            let hover_after = self.doc.as_ref().and_then(|doc| doc.get_hover_node_id());
            hover_before != hover_after
        } else {
            true
        }
    }

    fn pointer_event(
        &mut self,
        ev: &PointerEvent,
        buttons: MouseEventButtons,
    ) -> BlitzPointerEvent {
        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: self.pointer_coords(ev.x, ev.y),
            button: match ev.button {
                ironsurf_common::MouseButton::Left => MouseEventButton::Main,
                ironsurf_common::MouseButton::Right => MouseEventButton::Secondary,
                ironsurf_common::MouseButton::Middle => MouseEventButton::Auxiliary,
            },
            buttons,
            // The flags the user held (Ctrl-left / ⌘-left clicks, shift-clicks)
            // — `to_blitz_mods` does the one macOS swap the shell's flags need.
            mods: to_blitz_mods(ev.mods),
            details: PointerDetails::default(),
            // The event driver overwrites `element` from the target's
            // bounding rect (driver.rs::adjust_element_coords).
            element: Point::default(),
            // Mouse input: the shared active-pointer (multi-touch) list stays empty.
            active_pointers: Default::default(),
        }
    }

    /// The action chord exactly as the shell forwards it: the letter with
    /// just the platform's primary modifier (Ctrl everywhere, Cmd on macOS)
    /// and nothing else (the shell never intercepts clipboard chords —
    /// `keymap::clipboard_chords_always_reach_the_page` covers `a`).
    /// Shift/Alt combinations are different chords (blitz's editor maps
    /// Ctrl+Shift+A to *deselect*), and auto-repeat must not re-run the select.
    fn is_select_all_chord(ev: &KeyEvent) -> bool {
        is_action_chord(ev)
            && !ev.mods.shift()
            && !ev.mods.alt()
            && !ev.repeat
            && ev.key.eq_ignore_ascii_case("a")
    }

    /// Is a text control (`<input>` / `<textarea>`) the focused element? Then the
    /// caret answers the arrows and Ctrl-A before the page does: blitz moves the
    /// caret on the key it was given (`node/text.rs`), and the document keeps its
    /// scroll offset.
    fn editor_focused(&self) -> bool {
        let Some(doc) = self.doc.as_ref() else {
            return false;
        };
        doc.get_focussed_node_id()
            .and_then(|id| doc.get_node(id))
            .and_then(|node| node.element_data())
            .is_some_and(|data| data.text_input_data().is_some())
    }

    /// Keyboard input: `ToTab::Key` is a key *press* (the contract has no
    /// release direction). blitz beta.2 has no separate `UnicodeInput`
    /// variant — printable text rides on `BlitzKeyEvent.text` and the whole
    /// default action (Tab focus moves, Ctrl+C, text-input edits) consumes
    /// `UiEvent::KeyDown` (events/mod.rs:211). We emit the matching
    /// `KeyUp` afterwards purely for DOM-fidelity (its default action is a
    /// no-op, events/mod.rs:222); there is no release in the contract to
    /// model beyond that.
    fn handle_key(&mut self, ev: &KeyEvent) {
        // A keystroke is a user interaction as much as a click is (task: block
        // popups): the Enter that activates a link and the key a page answers
        // with `window.open` are the same thing from here, and both are the
        // user asking.
        self.popups.arm();
        // The page's JS sees every press first: a `keydown` handler that
        // calls `preventDefault()` swallows the default action — no editor
        // insert, no form submission, no focus move (task: `<form>` glue with
        // the JS event system).
        if self.fire_js_key("keydown", ev) {
            self.fire_js_key("keyup", ev);
            return;
        }
        // `<form>` implicit submission (task: `<form>`): Enter in a
        // single-line text field submits the owning form — resolved here (on
        // the live entry list, every action scheme) rather than by blitz,
        // whose version forgets selects and only acts on http(s)/data/
        // mailto. Textareas keep inserting newlines (blitz's own path), and
        // a focused node outside a form is blitz's business as before.
        if ev.key == "Enter" && !ev.repeat && self.pending_form.is_none() {
            let focused = self
                .doc
                .as_ref()
                .and_then(|doc| doc.get_focussed_node_id());
            if let Some(focused) = focused {
                if let Some(outcome) = self
                    .doc
                    .as_ref()
                    .and_then(|doc| forms::implicit_submission(doc, focused))
                {
                    match outcome {
                        Ok(sub) => {
                            debug!(
                                "form: implicit Enter submission ({} entries) → {}",
                                sub.entries, sub.url
                            );
                            self.pending_form = Some(sub);
                            self.fire_js_key("keyup", ev);
                            return;
                        }
                        Err(note) => {
                            info!("form: {note}");
                            self.emit(FromTab::StatusText {
                                text: Some(note),
                            });
                            self.fire_js_key("keyup", ev);
                            return;
                        }
                    }
                }
            }
        }
        let key = ev.key.parse::<Key>().unwrap_or(Key::Unidentified);
        let code = ev.code.parse::<Code>().unwrap_or(Code::Unidentified);
        let text = ev
            .text
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(blitz_traits::SmolStr::from);
        let base = BlitzKeyEvent {
            key,
            code,
            modifiers: to_blitz_mods(ev.mods),
            location: Location::Standard,
            is_auto_repeating: ev.repeat,
            is_composing: false,
            state: KeyState::Pressed,
            text,
        };
        self.feed_ui_event(UiEvent::KeyDown(base.clone()));
        // macOS: the delete key has to be named as an AppKit editing command as
        // well, or the engine ignores it (see [`apple_keybinding`]).
        if APPLE_KEYBINDINGS {
            if let Some(command) = apple_keybinding(ev) {
                self.feed_ui_event(UiEvent::AppleStandardKeybinding(command.into()));
            }
        }
        // Ctrl+A outside an editor selects all the text on screen (task:
        // select all). blitz's own Ctrl+A is editor-only (`node/text.rs`) and
        // ran on the feed above, so with a text control focused we stay out
        // of its way; with none focused blitz ignores the chord, and the
        // document's whole laid-out text becomes the selection. A page that
        // `preventDefault()`ed the key never gets here — the `keydown` gate
        // at the top of this function returns first.
        if Self::is_select_all_chord(ev) {
            if !self.editor_focused()
                && self.doc.as_mut().is_some_and(|doc| select::select_all(doc))
            {
                let chars = self
                    .doc
                    .as_ref()
                    .and_then(|doc| doc.get_selected_text())
                    .map(|text| text.chars().count())
                    .unwrap_or(0);
                debug!("select-all: {chars} char(s) of page text selected");
            }
        }
        // PageUp / PageDown: the page moves a screen at a time (see
        // [`Self::scroll_page`]). Done between the press and the release so the
        // page still sees the whole gesture — and sees nothing of the scroll
        // itself, because a `keydown` handler that `preventDefault()`ed the press
        // returned out of this function before ever getting here.
        // `↑` / `↓` take the page a line at a time (see [`Self::scroll_line`]) —
        // but only with no text control under the caret: with one focused the arrow
        // moves the caret and the page stays put (blitz's own edit default,
        // `node/text.rs`), which is what a reader filling a form at the bottom of a
        // long page expects. PageUp/PageDown do scroll the document either way,
        // because a textarea answers neither key and a screen is a screen.
        match ev.key.as_str() {
            "PageUp" => self.scroll_page(true),
            "PageDown" => self.scroll_page(false),
            "ArrowUp" | "ArrowDown" if !self.editor_focused() => {
                self.scroll_line(ev.key == "ArrowUp")
            }
            _ => {}
        }
        let release = BlitzKeyEvent {
            state: KeyState::Released,
            is_auto_repeating: false,
            // The release carries no committed text (DOM semantics).
            text: None,
            ..base
        };
        self.feed_ui_event(UiEvent::KeyUp(release));
        self.fire_js_key("keyup", ev);
    }

    /// Wheel/scroll: blitz's default action scrolls by `Lines(x, y) * 20`
    /// CSS px or `Pixels(x, y)` verbatim (events/pointer.rs::handle_wheel),
    /// and it reads the winit/AccessKit sign convention: a POSITIVE delta
    /// scrolls UP/LEFT (`scrollTop` shrinks), a negative one scrolls down —
    /// verified against beta.2 by `wheel_down_scrolls_the_document_down_and_
    /// wheel_up_back_up`. winit (and therefore Slint's `scroll-event`, which
    /// the shell forwards untouched) emits the same convention, so the deltas
    /// arriving here need no sign surgery.
    ///
    /// The contract doesn't pin units, so: deltas with magnitude ≤ 3 are
    /// treated as wheel notches — one line of content each, whatever the
    /// zoom (blitz's 20 CSS px `Lines` unit, spelled out here because the
    /// glide feeds everything as pixel deltas) — and everything else as
    /// logical-px pixel deltas, divided by the zoom so the page moves the
    /// same *visual* distance at every zoom.
    ///
    /// Slower and smoother (task): the travel is damped by [`WHEEL_SPEED`]
    /// — scaled by the settings menu's scroll-speed slider, see
    /// [`Tab::wheel_speed`] — and queued onto [`Tab::pump_wheel`] instead of
    /// being applied whole —
    /// the page glides to the new offset over a few frames, and a wheel in
    /// motion keeps queueing onto the same glide. The first slice is fed with
    /// the event, so a gesture never waits a frame to start moving.
    fn handle_scroll(&mut self, ev: &ScrollEvent) {
        let z = self.zoom() as f64;
        let unit = if ev.delta_x.abs().max(ev.delta_y.abs()) <= 3.0 {
            20.0 // one notch, CSS px — blitz's `Lines` unit, baked in
        } else {
            1.0 / z // logical px → CSS px
        };
        let glide = &mut self.wheel_glide;
        if glide.pending == (0.0, 0.0) {
            // A resting glide restarts its clock with the gesture, so the
            // first slice is the kick, not the whole travel.
            glide.last = Instant::now();
        }
        glide.pending = (
            glide.pending.0 + f64::from(ev.delta_x) * unit * self.wheel_speed,
            glide.pending.1 + f64::from(ev.delta_y) * unit * self.wheel_speed,
        );
        let coords = self.pointer_coords(ev.x, ev.y);
        let glide = &mut self.wheel_glide;
        glide.at = Some(coords);
        glide.over = (ev.x, ev.y);
        self.pump_wheel(false);
    }

    /// The single document-input funnel: everything below (7e Boa scripting,
    /// synthetic events) should reach blitz through here.
    fn feed_ui_event(&mut self, ui: UiEvent) {
        if let Some(doc) = self.doc.as_mut() {
            doc.handle_ui_event(ui);
        }
    }

    /// Replay a real pointer interaction through the page's own JS event
    /// system (capture/bubble over the hover node's path, inline `onclick`,
    /// listeners — task: `<form>`: `<button onclick>` must run). Returns the
    /// event's `defaultPrevented`.
    fn fire_js_pointer(&mut self, kind: &str, ev: &PointerEvent) -> bool {
        let node = self.doc.as_ref().and_then(|doc| doc.get_hover_node_id());
        let init = format!(
            "{{ clientX: {}, clientY: {}, button: 0{} }}",
            ev.x,
            ev.y,
            js_modifier_init(ev.mods)
        );
        self.fire_js_event(node, kind, &init)
    }

    /// The same for keyboard input: the focused node is the target (a plain
    /// key with nothing focused still reaches window-level listeners).
    fn fire_js_key(&mut self, kind: &str, ev: &KeyEvent) -> bool {
        let node = self
            .doc
            .as_ref()
            .and_then(|doc| doc.get_focussed_node_id());
        let init = format!(
            "{{ key: \"{}\", code: \"{}\", repeat: {} }}",
            js_string_lit(&ev.key),
            js_string_lit(&ev.code),
            ev.repeat
        );
        self.fire_js_event(node, kind, &init)
    }

    /// The single JS-event funnel (`__ironsurfFire`, the glue's entry point).
    /// No page, no JS runtime, or a failed dispatch all mean "not prevented":
    /// the engine's default actions never wait on the page.
    fn fire_js_event(&mut self, node: Option<NodeId>, kind: &str, init: &str) -> bool {
        let js = self.js.as_mut();
        let doc = self.doc.as_mut();
        match (js, doc) {
            (Some(js), Some(doc)) => js.fire_event(doc, node, kind, init),
            _ => false,
        }
    }

    /// Drain a pending navigation: first the tab's own form submissions
    /// (task: `<form>` — resolved by the form layer from a submit press or an
    /// implicit Enter), then the `NavigationProvider` hook (link clicks, and
    /// — as a backstop — any blitz-side submission that slips through, whose
    /// POST body is honoured here rather than dropped), and finally the page's
    /// own queued JS navigation (`location.href=`/`assign`/`replace`/`reload`;
    /// task: JS-driven navigation).
    fn drain_nav_hook(&mut self) -> bool {
        // A press that never got its release is not a click; forget it.
        self.form_press = None;
        self.label_press = None;
        // Taken first and unconditionally (task: block popups): the mark only
        // ever describes the click blitz was just fed, and an unanswered mark
        // must never turn an unrelated later navigation into a popup.
        let new_window = self.new_window_click.take();
        if let Some(sub) = self.pending_form.take() {
            self.navigate_submission(&sub);
            return true;
        }
        let pending = self.nav.pending.lock().expect("nav mutex").take();
        if let Some(options) = pending {
            let url = options.url.as_str().to_owned();
            if let Some(target) = new_window {
                // The user clicked a link that asked for a NEW window, so this
                // tab does not navigate — that is what `target` means — and the
                // request goes to the gate on the page's behalf.
                // `PopupVia::Link` carries the fact that a human clicked, which
                // is the interaction the blocker exists to wait for, so it
                // passes (task: block popups).
                info!("target={target:?} click → popup request {url}");
                self.popups
                    .request(&url, Some(target), crate::popup::PopupVia::Link);
            } else {
                match form_body_from_nav(&options) {
                    Some(post) => {
                        info!("form POST (blitz hook) → {url} ({} B)", post.body.len());
                        // The page wrote the action URL, so it is not the
                        // address bar's navigation.
                        self.nav_from_shell = false;
                        self.navigate_post(&url, NavHistory::Fresh, Some(&post));
                    }
                    None => {
                        self.navigate(&url, NavHistory::Fresh);
                    }
                }
                return true;
            }
        }
        // 3. `history.pushState`/`replaceState` (task: infosec.exchange): a
        //    URL-only change — address bar follows, document stays put.
        if self.follow_pending_js_url() {
            return true;
        }
        // 4. The page's own navigation request (`location.href=`/`assign`/
        //    `replace`/`reload` from scripts, timers, or event handlers).
        if self.follow_pending_js_nav() {
            return true;
        }
        // 5. A `Refresh:` header / `<meta http-equiv="refresh">` deadline that
        //    has come due (task: Anubis `metarefresh` gates).
        self.follow_due_refresh()
    }

    /// Popups (task: block popups): every window the page asked for since the
    /// last poll leaves the tab here. An **allowed** request becomes
    /// [`FromTab::OpenLinkInNewTab`] — the shell spawns a sibling tab in this
    /// tab's container, because tabs belong to the shell and that is the only
    /// way a page's `window.open` ever becomes a window. A **refused** one
    /// becomes [`FromTab::PopupBlocked`], so the user can see the refusal and
    /// allow the site. Nothing else happens either way: no document loads here,
    /// no history entry is written, no address bar moves.
    fn drain_popups(&mut self) {
        let pending = self.popups.take_pending();
        if pending.is_empty() {
            return;
        }
        for request in pending.allowed {
            info!(
                "popup allowed ({:?}) → {}",
                request.via, request.url
            );
            self.emit(FromTab::OpenLinkInNewTab { url: request.url });
        }
        for request in pending.blocked {
            let url = crate::popup::cap_reported_url(&request.url);
            warn!("popup blocked ({:?}) → {url}", request.via);
            self.emit(FromTab::PopupBlocked { url });
        }
        self.flush_stdout();
    }

    /// The shell's popup rules (task: block popups): the master switch and the
    /// site allow-list, replaced wholesale. No reload is involved — the gate is
    /// consulted per request, so a change applies to the page on screen as well
    /// as to every document loaded afterwards.
    fn set_popup_policy(&mut self, block: bool, allowed: Vec<String>) {
        info!(
            "popup policy: block={block}, {} allowed origin(s)",
            allowed.len()
        );
        self.popups.configure(block, allowed);
    }

    /// Apply the URL-only address-bar change `history.pushState`/`replaceState`
    /// queued during the last JS turn: resolve against the live document URL,
    /// enforce the SAME-ORIGIN rule tab-side (never trusting the page's JS),
    /// then let `check_meta` ship the `UrlChanged`. The document is not
    /// refetched and engine history is untouched — spec-wise the page did not
    /// navigate, it only relabelled where it is.
    fn follow_pending_js_url(&mut self) -> bool {
        let Some(target) = self
            .js
            .as_ref()
            .and_then(|runtime| runtime.take_pending_url())
        else {
            return false;
        };
        let base = self
            .doc
            .as_ref()
            .map(|doc| doc.url().clone())
            .or_else(|| self.history.get(self.history_index).cloned());
        let resolved = match base.as_ref().and_then(|base| base.join(&target).ok()) {
            Some(url) => url,
            None => {
                warn!("js url update to {target:?} did not resolve");
                return true;
            }
        };
        let crosses_origin = base
            .as_ref()
            .is_some_and(|base| base.origin() != resolved.origin());
        if crosses_origin {
            warn!("js url update to {resolved} crosses the document origin; ignored");
            return true;
        }
        debug!("history.pushState → {resolved} (url-only, no fetch)");
        self.url_override = Some(resolved.as_str().to_owned());
        // `location.href` and the address bar must agree from here on.
        if let Some(runtime) = self.js.as_ref() {
            runtime.set_url(resolved.as_str());
        }
        self.check_meta();
        true
    }

    /// Act on a submission the form layer resolved: POST only goes to the
    /// network (a local scheme has nowhere to put a body — say so in the
    /// status bar instead of silently dropping it); GET navigations of any
    /// scheme we fetch (`http(s)`, `file`, `data`, `ironsurf:`, …) proceed.
    fn navigate_submission(&mut self, sub: &FormSubmission) {
        let scheme = sub.url.scheme().to_owned();
        if sub.method == FormMethod::Post && !(scheme == "http" || scheme == "https") {
            info!("form: refusing to POST to a {scheme}: URL");
            self.emit(FromTab::StatusText {
                text: Some(format!(
                    "form: cannot POST to a {scheme}: URL (no transport for the body)"
                )),
            });
            return;
        }
        let post = (sub.method == FormMethod::Post).then(|| MainDocPost {
            content_type: sub.content_type.clone(),
            body: sub.body.clone(),
        });
        // As above: a `<form method=get action="ironsurf://archive?root=…">` is
        // page-authored, however it was submitted.
        self.nav_from_shell = false;
        self.navigate_post(sub.url.as_str(), NavHistory::Fresh, post.as_ref());
    }


    // -- downloads -----------------------------------------------------------
    //
    // See `ARCHITECTURE.md` §File downloads for the whole feature: the tab
    // owns *whether* a fetch is a file (it owns the response) and the shell
    // owns *where* the bytes land. The tab never touches disk.

    /// Start a download of `url` **now**, on a worker thread, and answer
    /// immediately with `DownloadStart` so the shell can file a row while the
    /// bytes are still coming (a 500 MB file must not freeze the UI: the tab
    /// cannot answer stdin while a fetch blocks the tab thread, and the
    /// shell's own pump has nothing to chew on). `trigger` says what made this
    /// a download; `suggested` is the name the server or the clicked `<a>`
    /// wanted — a *suggestion*, sanitized here and sanitized again by the
    /// shell, because a name from the network is untrusted input.
    ///
    /// Nothing else is emitted and nothing else is touched: no history, no
    /// URL/title change, no `check_meta` — the document on screen stays
    /// loaded (a download is not a navigation).
    fn start_download(
        &mut self,
        url: Url,
        suggested: Option<String>,
        trigger: DownloadTrigger,
        media_type: Option<String>,
    ) {
        // `media_type` is known only when the tab already fetched on its own
        // thread (a response whose headers revealed it was a file); the
        // worker's fetch has not settled, so `None` = not yet known.
        let hint = self.download_hint(url.as_str(), suggested, trigger, media_type);
        let id = hint.id;
        self.emit(FromTab::DownloadStart { hint });
        self.spawn_download(id, url);
    }

    /// Mint the [`DownloadHint`] for a download and log its start (task: file
    /// downloads): a fresh id from this process's counter, the URL the tab
    /// fetches, the name the server or the clicked `<a download>` wanted (a
    /// *suggestion* — `sanitize_file_name` here, the shell sanitizes again),
    /// and what made this a download at all.
    fn download_hint(
        &mut self,
        url: &str,
        suggested: Option<String>,
        trigger: DownloadTrigger,
        media_type: Option<String>,
    ) -> DownloadHint {
        let hint = DownloadHint {
            id: self.download_ids.next(),
            url: url.to_owned(),
            name: downloads::name_suggestion(suggested.as_deref(), url),
            media_type,
            trigger,
        };
        info!(
            "download {:?} {}{}",
            trigger,
            url,
            hint.name
                .as_deref()
                .map(|n| format!(" → {n}"))
                .unwrap_or_default()
        );
        hint
    }

    /// The worker thread behind [`Self::start_download`]: fetch `url` as a GET
    /// with this tab's own jar/cache/UA (the same code path every navigation
    /// uses, `fetch_document_report_shared`-style, so cookies, redirects, the
    /// archive hooks and the DevTools feed all apply), then hand the buffered
    /// body back through the download queue. A failed fetch is reported
    /// verbatim — `DownloadEnd { error }` always closes the sequence, so a row
    /// never sticks at 0 bytes. Its network event carries
    /// `NetEvent::initiator` `"download"`, so the DevTools panel shows the
    /// request as one.
    fn spawn_download(&mut self, id: u64, url: Url) {
        let policy = self_provider_policy(&self.provider);
        let jar = Arc::clone(&self.jar);
        let cache = Arc::clone(&self.cache);
        let user_agent = self.user_agent.clone();
        let net_events = Arc::clone(&self.net_events);
        let archive = Arc::clone(&self.archive);
        let pipe = Arc::clone(&self.pipe);
        let tx = self.download_tx.clone();
        let worker_url = url.clone();
        let spawn = std::thread::Builder::new()
            .name("ironsurf-download".to_string())
            .spawn(move || {
                let events = Arc::clone(&net_events);
                let mut report = |event: NetEvent| {
                    // A download is a request the page never asked for; the
                    // panel still shows it, tagged as ours.
                    let mut event = event;
                    event.initiator = Some("download".to_owned());
                    events.lock().expect("net events mutex").push(event);
                };
                let fetch = netmod::fetch_document_report_shared(
                    &worker_url,
                    policy,
                    &jar,
                    &cache,
                    netmod::now_unix(),
                    &user_agent,
                    Some(&mut report),
                );
                let (chunks, error) = match fetch {
                    Ok(fetch) => {
                        info!(
                            "download {} fetched {} B ({})",
                            netmod::url_for_log(&fetch.url),
                            fetch.body.len(),
                            fetch.status
                        );
                        // Archive mode: the bytes are a response like any
                        // other, so they ride the same feed — but never as
                        // *the host's document*, which is what an archived
                        // navigation would overwrite (contract §File
                        // downloads: a download never touches the archive's
                        // document bookkeeping).
                        archive.store(&fetch.url, false, &fetch.body);
                        let mut body = fetch.body;
                        match downloads::refusal_for(body.len()) {
                            Some(reason) => {
                                warn!("download refused: {reason}");
                                (Vec::new(), Some(reason))
                            }
                            None => (downloads::into_chunks(&mut body), None),
                        }
                    }
                    Err(err) => {
                        // The user asked to save this; the reason is for them.
                        warn!("download fetch failed: {err}");
                        (Vec::new(), Some(err.to_string()))
                    }
                };
                if tx
                    .send(DownloadShipment {
                        id,
                        chunks,
                        error,
                    })
                    .is_err()
                {
                    // The tab is being torn down; nothing to ship to.
                    return;
                }
                pipe.wake();
            });
        if let Err(err) = spawn {
            // No thread, no download: say so instead of leaving a spinner.
            warn!("cannot spawn download worker: {err}");
            self.emit(FromTab::DownloadEnd {
                id,
                error: Some(format!("cannot start the download: {err}")),
            });
        }
    }

    /// Ship a download whose bytes the tab thread already holds (task: file
    /// downloads): `DownloadStart`, then every chunk, then `DownloadEnd`. The
    /// bytes are **taken out** of `body` ([`downloads::into_chunks`]) rather
    /// than copied, so a big file never exists twice, and every chunk leaves
    /// before the next frame is written.
    fn emit_download(&mut self, hint: DownloadHint, body: &mut Vec<u8>) {
        let id = hint.id;
        let bytes = body.len();
        let reason = downloads::refusal_for(body.len());
        let chunks = downloads::into_chunks(body);
        info!(
            "download {} shipped {} B ({})",
            hint.url,
            bytes,
            reason.as_deref().unwrap_or("ok")
        );
        self.emit(FromTab::DownloadStart { hint });
        for chunk in chunks {
            self.emit(FromTab::DownloadChunk { id, bytes: chunk });
        }
        self.emit(FromTab::DownloadEnd { id, error: reason });
    }

    /// Refuse a download the tab cannot fetch at all (task: file downloads):
    /// a bad scheme, an internal page, a `javascript:` target. The shell still
    /// gets the full sequence — `DownloadStart` then a chunk-free
    /// `DownloadEnd { error }` — so its row always settles rather than
    /// spinning at 0 bytes, and the page on screen stays exactly as it was.
    fn refuse_download(
        &mut self,
        url: &str,
        suggested: Option<String>,
        trigger: DownloadTrigger,
        reason: String,
    ) {
        let hint = self.download_hint(url, suggested, trigger, None);
        let id = hint.id;
        info!("download of {url} refused: {reason}");
        self.emit(FromTab::DownloadStart { hint });
        self.emit(FromTab::DownloadEnd { id, error: Some(reason) });
    }

    /// Turn one settled worker download into its `DownloadChunk`* →
    /// `DownloadEnd` tail (task: file downloads). The worker pushes then wakes
    /// the shared self-pipe, so this runs the moment the fetch lands — never a
    /// tick late, and always before the next frame is written.
    fn drain_downloads(&mut self) {
        while let Ok(shipment) = self.download_rx.try_recv() {
            for bytes in shipment.chunks {
                self.emit(FromTab::DownloadChunk {
                    id: shipment.id,
                    bytes,
                });
            }
            self.emit(FromTab::DownloadEnd {
                id: shipment.id,
                error: shipment.error,
            });
        }
    }

    pub(crate) fn persist(&mut self) {
        // Best-effort: the one shared jar (main doc + subresources) lives
        // inside the sandbox-writable profile dir.
        debug!("persisting cookie jar to profile dir");
        if let Err(err) = self.provider.save_cookies() {
            warn!("cookie jar not saved: {err}");
        }
    }

    fn flush_stdout(&mut self) {
        self.out.borrow_mut().flush();
    }

    /// One harness pump: drain a pending navigation request, else run any
    /// due JS timers (task: real timers — the `--wait` window is the
    /// headless poll loop), then settle (resolve + document messages) and
    /// refresh the title/url caches. Timer title changes land in `js_title`
    /// and show up in the summary (`title_now`) and `--body-out` capture.
    pub(crate) fn headless_pump(&mut self) {
        if self.drain_nav_hook() {
            return;
        }
        self.run_due_timers();
        self.drain_console_events();
        self.settle();
        self.check_meta();
    }

    /// How long the harness may sleep before its next pump without missing
    /// a JS timer deadline (`None` = nothing scheduled) — nor a pending
    /// `Refresh:`/meta-refresh navigation deadline.
    pub(crate) fn timer_sleep_hint(&self) -> Option<Duration> {
        let timer = self
            .js
            .as_ref()
            .and_then(|runtime| runtime.next_timer_deadline())
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        match (timer, self.refresh_sleep_hint()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The live document, for harness-side DOM/serializer access.
    pub(crate) fn document_mut(&mut self) -> Option<&mut blitz_html::HtmlDocument> {
        self.doc.as_mut()
    }

    /// Title with the `check_meta` precedence (JS assignment > blitz flush >
    /// `<title>` node poll), resolved on demand for the harness summary.
    pub(crate) fn title_now(&mut self) -> Option<String> {
        let js = self.js_title.clone();
        let hooked = self.shell.title.lock().expect("title mutex").clone();
        let polled = self
            .doc
            .as_ref()
            .and_then(|doc| doc.find_title_node())
            .map(|node| node.text_content());
        js.or(hooked).or(polled)
    }
}

// ---------------------------------------------------------------------------
// Video plumbing (task: playback)
//
// The engine (`crate::media`) owns demux/decode/clock; this section owns the
// three things only the tab can do:
//
// 1. **Attach** — every `<video>` element gets a [`VideoWidget`] via blitz's
//    `set_custom_widget` (blitz 0.3's custom-widget entry point; the DOM
//    traversal has no CSS box sizes — layout has not necessarily run — so
//    the widget's size hint comes from the element's `width`/`height`
//    attributes, 640×360 otherwise, exactly as `media/widget.rs` documents).
// 2. **Fetch** — `src=` is resolved against the document URL and pulled on
//    a worker thread through the tab's own network layer (container cookies,
//    cache, https policy — the same rules every main-document load obeys),
//    delivered over the shared self-pipe like every other wake source.
// 3. **Pump** — once per poll iteration (capped to `MEDIA_TICK` while any
//    player is alive) every player advances, its widget repaints through the
//    shared handle, `PlayerEvent`s are replayed into the page as DOM events
//    via `__ironsurfFire`, and the per-element [`MediaSnapshot`] the page's
//    `v.currentTime`-style reads go through is refreshed.
//
// `MediaHost` is the JS side of that contract: page writes (`v.play()`,
// `v.currentTime = 5`) queue [`MediaCommand`]s the poll loop applies; page
// reads see the snapshot the last pump wrote.

/// One `<video>` element's player (the widget holds the same `Rc`).
struct MediaEntry {
    node_id: NodeId,
    /// The href the bytes were fetched for; `Some` once a fetch was started
    /// for this element (also dedupes multi-element one-clip pages: every
    /// entry sharing an href gets the delivered bytes).
    href: Option<String>,
    player: Rc<RefCell<MediaPlayer>>,
}

/// A media fetch's outcome, as delivered to the loop over the self-pipe.
/// `Err` is a user-facing [`MediaError`] (rendered into the widget's failure
/// placeholder and `v.mediaError`), never a panic or a stack trace.
struct MediaLoaded {
    node_id: NodeId,
    href: String,
    outcome: Result<Vec<u8>, MediaError>,
}

/// One download a fetch worker settled (task: file downloads). The tab already
/// emitted `DownloadStart` when it spawned the worker, so this carries only
/// what the fetch learned: the body, sliced into chunk-sized `Vec`s **taken
/// out of** the fetch buffer (never copied: a 512 MiB download must not exist
/// twice in RAM), or the reason it never got bytes. [`Tab::drain_downloads`]
/// turns it into the `DownloadChunk`* → `DownloadEnd` tail of the sequence.
pub(crate) struct DownloadShipment {
    id: u64,
    chunks: Vec<Vec<u8>>,
    error: Option<String>,
}

/// What page JavaScript asked the players to do (`v.play()`, `v.currentTime
/// = 5`, `v.muted = true`); applied by the poll loop between pumps.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaCommand {
    Play { node: u64 },
    Pause { node: u64 },
    Seek { node: u64, seconds: f64 },
    SetRate { node: u64, rate: f64 },
    SetMuted { node: u64, muted: bool },
}

/// The per-element values the page reads through `HTMLMediaElement` glue
/// (js.rs); rewritten from the real players on every pump, so JS is stale by
/// at most one media tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaSnapshot {
    pub duration: f64,
    pub current_time: f64,
    pub paused: bool,
    pub ended: bool,
    pub rate: f64,
    pub muted: bool,
    pub width: u32,
    pub height: u32,
    pub error: Option<String>,
}

impl MediaSnapshot {
    fn of(player: &MediaPlayer) -> Self {
        let (width, height) = player.natural_size().unwrap_or((0, 0));
        MediaSnapshot {
            duration: player.duration(),
            current_time: player.position(),
            paused: player.is_paused(),
            ended: player.state() == PlayerState::Ended,
            rate: player.rate(),
            muted: player.is_muted(),
            width,
            height,
            error: player.error().map(|err| err.to_string()),
        }
    }
}

/// The media cell living inside [`js::HostState`]: snapshots out (reads),
/// commands in (writes). The tab owns both ends and keeps them in step.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaHost {
    /// Node raw id → last-pumped snapshot.
    pub snapshots: HashMap<u64, MediaSnapshot>,
    /// Page writes awaiting application, in order.
    pub commands: Vec<MediaCommand>,
}

impl MediaHost {
    pub fn snapshot(&self, node: u64) -> Option<&MediaSnapshot> {
        self.snapshots.get(&node)
    }

    pub fn enqueue(&mut self, command: MediaCommand) {
        self.commands.push(command);
    }

    pub fn take_commands(&mut self) -> Vec<MediaCommand> {
        std::mem::take(&mut self.commands)
    }
}

/// Decode one `__ironsurfMediaCmd` call into a [`MediaCommand`] (`None` for
/// unknown ops — the native then queues nothing).
pub fn media_command_from_js(node: u64, op: &str, arg: f64) -> Option<MediaCommand> {
    match op {
        "play" => Some(MediaCommand::Play { node }),
        "pause" => Some(MediaCommand::Pause { node }),
        "seek" if arg.is_finite() && arg >= 0.0 => Some(MediaCommand::Seek { node, seconds: arg }),
        "rate" if arg.is_finite() && arg > 0.0 => Some(MediaCommand::SetRate { node, rate: arg }),
        "muted" => Some(MediaCommand::SetMuted {
            node,
            muted: arg != 0.0,
        }),
        _ => None,
    }
}

/// The DOM event name a [`PlayerEvent`] becomes — every engine event has a
/// standard DOM counterpart (`canplay` is included: pages legitimately wait
/// on it before `play()`).
fn media_event_name(event: &PlayerEvent) -> Option<&'static str> {
    Some(match event {
        PlayerEvent::LoadedMetadata => "loadedmetadata",
        PlayerEvent::LoadedData => "loadeddata",
        PlayerEvent::CanPlay => "canplay",
        PlayerEvent::Play => "play",
        PlayerEvent::Pause => "pause",
        PlayerEvent::Seeking => "seeking",
        PlayerEvent::Seeked => "seeked",
        PlayerEvent::TimeUpdate { .. } => "timeupdate",
        PlayerEvent::RateChange => "ratechange",
        PlayerEvent::MutedChange => "volumechange",
        PlayerEvent::Ended => "ended",
        PlayerEvent::Error { .. } => "error",
    })
}

/// Run one media step where a panic must never escape the poll loop (the
/// engine's own contract is panic-free; this is the unwind boundary).
fn media_guard(what: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        error!("media step `{what}` panicked; the tab keeps running");
    }
}

impl Tab {
    /// Drop every player of the outgoing document (navigation) and the
    /// snapshots/commands that referenced them. Cheap with no media; the
    /// whole point of the `media_enabled` guard is that feature-off tabs
    /// never grow a player in the first place, so every step below is a
    /// branch on an empty Vec.
    fn reset_media(&mut self) {
        self.media.clear();
        self.media_inflight.clear();
        self.media_next_pump = None;
        // Whatever queue existed for the old document dies with it.
        if let Ok(mut results) = self.media_results.lock() {
            results.clear();
        }
        if let Some(js) = self.js.as_ref() {
            let mut host = js.host().borrow_mut();
            host.media.snapshots.clear();
            host.media.commands.clear();
        }
    }

    /// Every `<video>` element without a widget gets one — and with it the
    /// player the whole media path hangs off. The traversal is the document
    /// node map (there is no CSS box to consult at this point); the widget's
    /// size hint is the element's `width`/`height` attributes, falling back
    /// to 640×360, per `media/widget.rs`.
    pub fn attach_video_widgets(&mut self) {
        if !self.media_enabled {
            return;
        }
        media_guard("attach", || self.attach_video_widgets_inner());
    }

    fn attach_video_widgets_inner(&mut self) {
        {
            // Collect first (the borrow of `self.doc` for reads must end
            // before `set_custom_widget` takes it mutably again).
            let mut late_src: Vec<NodeId> = Vec::new();
            let candidates: Vec<(NodeId, f32, f32)> = {
                let Some(doc) = self.doc.as_ref() else { return };
                doc.tree()
                    .iter()
                    .filter_map(|(id, node)| {
                        let el = node.element_data()?;
                        if el.name.local != blitz_dom::local_name!("video") {
                            return None;
                        }
                        if el.custom_widget_data().is_some() {
                            return None; // already a player's window
                        }
                        // The attributes are the only sizes the traversal can
                        // see (no CSS box here); parse leniently.
                        let width = el
                            .attr(blitz_dom::local_name!("width"))
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|v| *v > 0.0 && v.is_finite())
                            .unwrap_or(640.0);
                        let height = el
                            .attr(blitz_dom::local_name!("height"))
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|v| *v > 0.0 && v.is_finite())
                            .unwrap_or(360.0);
                        Some((id, width, height))
                    })
                    .collect()
            };
            let mut attached = 0;
            if !candidates.is_empty() {
                let doc = self.doc.as_mut().expect("doc checked above");
                for (node_id, width, height) in candidates {
                    let player = Rc::new(RefCell::new(MediaPlayer::new()));
                    let widget =
                        VideoWidget::with_size_hint(Rc::clone(&player), width, height);
                    // blitz 0.3's custom-widget entry point (the `Node::Widget`
                    // insertion of the design doc, in the API the engine ships).
                    doc.set_custom_widget(node_id, Box::new(widget));
                    self.media.push(MediaEntry {
                        node_id,
                        href: None,
                        player,
                    });
                    attached += 1;
                }
            }
            if attached > 0 {
                debug!("media: attached {attached} video widget(s)");
                // Restyle so the new replaced-element boxes are laid out
                // before the first frame (cheap when nothing changed).
                self.settle();
            }
            // Elements whose `src` appeared *after* attach (a script setting
            // `v.src` on a parser-created element): players still Idle.
            for entry in &self.media {
                if entry.href.is_none() && entry.player.borrow().state() == PlayerState::Idle
                {
                    late_src.push(entry.node_id);
                }
            }
            for node_id in late_src {
                self.fetch_media(node_id);
            }
            // Elements removed from the tree take their players with them.
            if let Some(doc) = self.doc.as_ref() {
                self.media.retain(|entry| doc.tree().contains_key(entry.node_id));
            }
        }
    }

    /// Queue the fetch of one element's `src` (the media counterpart of the
    /// image/net path): resolve against the document URL, dedupe in-flight
    /// hrefs, pull the bytes on a worker thread through the tab's own
    /// network layer (container cookies/cache/policy apply exactly as they
    /// do to documents), and hand `Vec<u8>` back over the self-pipe. Only
    /// 2xx responses with at most `MAX_VIDEO_BYTES` of body ever reach the
    /// decoder; everything else arrives as a `MediaError`.
    pub fn fetch_media(&mut self, node_id: NodeId) {
        if !self.media_enabled {
            return;
        }
        let Some(node_idx) = self.media.iter().position(|e| e.node_id == node_id) else {
            return;
        };
        // Resolve `src` against the document URL (the `enqueue_image_load`
        // pattern: `base.join_str(&src)`; blitz 0.3 gives `Url::join`).
        let Some(url) = self.doc.as_ref().and_then(|doc| {
            let el = doc.get_node(node_id)?.element_data()?;
            let src = el.attr(blitz_dom::local_name!("src"))?.trim().to_owned();
            if src.is_empty() {
                return None;
            }
            match doc.url().join(&src) {
                Ok(url) => Some(url),
                Err(err) => {
                    warn!("media src {src:?} did not resolve against {}: {err}", doc.url());
                    None
                }
            }
        }) else {
            return; // no (or unresolvable) src — the placeholder waits
        };
        let href = url.to_string();
        self.media_inflight.insert(href.clone());
        self.media[node_idx].href = Some(href.clone());
        self.media[node_idx]
            .player
            .borrow_mut()
            .begin_load(); // spinner on the element while bytes travel
        // Keep the loop ticking at the media cadence so the load placeholder
        // animates and the completion is noticed even without wake-ups.
        self.media_next_pump = Some(Instant::now() + MEDIA_TICK);

        let slot = Arc::clone(&self.media_results);
        let pipe = Arc::clone(&self.pipe);
        let jar = Arc::clone(&self.jar);
        let policy = self_provider_policy(&self.provider);
        let user_agent = self.user_agent.clone();
        let archive = Arc::clone(&self.archive);
        let worker_href = href.clone();
        let spawn = std::thread::Builder::new()
            .name("ironsurf-media-fetch".to_string())
            .spawn(move || {
                let href = worker_href;
                let outcome = fetch_media_bytes(&url, policy, jar, user_agent, &href);
                match outcome {
                    Ok(bytes) => {
                        debug!("media: {href} fetched ({} B)", bytes.len());
                        // Archive mode: media bodies ride the same pipe as
                        // any other file loaded from the site.
                        archive.store(&url, false, &bytes);
                        slot.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(MediaLoaded {
                                node_id,
                                href,
                                outcome: Ok(bytes),
                            });
                    }
                    Err(err) => {
                        warn!("media: {href} failed: {err}");
                        slot.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(MediaLoaded {
                                node_id,
                                href,
                                outcome: Err(err),
                            });
                    }
                }
                // The one cross-thread signal the loop already listens for.
                pipe.wake();
            });
        if let Err(err) = spawn {
            // Thread exhaustion: fail honestly rather than spin a spinner.
            self.media_inflight.remove(&href);
            self.media[node_idx]
                .player
                .borrow_mut()
                .fail(MediaError::Unreachable {
                    detail: format!("could not start the fetch worker: {err}"),
                });
        }
    }

    /// Land fetched bytes: demux through the engine (`open_bytes` applies
    /// `MAX_VIDEO_BYTES` again as the last line of defence), start playback,
    /// or fail the player with the user-facing reason. The widget shares the
    /// player, so there is nothing to "set" on it — it repaints from the
    /// state it already points at; `request_paint_of_element` on this engine
    /// is a document repaint, which is what the frame throttle coalesces.
    fn drain_media_loaded(&mut self) {
        if !self.media_enabled {
            return;
        }
        let loaded = std::mem::take(
            &mut *self
                .media_results
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for item in loaded {
            self.media_inflight.remove(&item.href);
            media_guard("load", || {
                let mut landed = false;
                for entry in &mut self.media {
                    // Deliver to every element waiting on this href (the
                    // dedupe delivered one fetch to all of them).
                    let waiting = entry.node_id == item.node_id
                        || entry.href.as_deref() == Some(item.href.as_str());
                    if !waiting {
                        continue;
                    }
                    entry.href.get_or_insert_with(|| item.href.clone());
                    match &item.outcome {
                        Ok(bytes) => {
                            let mut player = entry.player.borrow_mut();
                            match crate::media::player::open_bytes(Arc::new(
                                bytes.clone(),
                            )) {
                                Ok(track) => {
                                    info!(
                                        "media: {} — {}",
                                        item.href,
                                        track.info().describe()
                                    );
                                    player.load(track);
                                    player.play();
                                }
                                Err(err) => {
                                    warn!("media: {} refused: {err}", item.href);
                                    player.fail(err);
                                }
                            }
                        }
                        Err(err) => entry.player.borrow_mut().fail(err.clone()),
                    }
                    landed = true;
                }
                if landed {
                    self.needs_paint = true; // the element repainted itself
                }
            });
        }
    }

    /// Apply the page's media writes from the JS turn(s) since the last pump.
    fn drain_media_js_commands(&mut self) {
        let commands = match self.js.as_ref() {
            Some(js) => js.host().borrow_mut().media.take_commands(),
            None => return,
        };
        if commands.is_empty() {
            return;
        }
        media_guard("commands", || {
            for command in commands {
                let node = match command {
                    MediaCommand::Play { node }
                    | MediaCommand::Pause { node }
                    | MediaCommand::Seek { node, .. }
                    | MediaCommand::SetRate { node, .. }
                    | MediaCommand::SetMuted { node, .. } => node,
                };
                let Some(entry) = self.media.iter().find(|e| e.node_id.as_u64() == node)
                else { continue };
                let mut player = entry.player.borrow_mut();
                match command {
                    MediaCommand::Play { .. } => player.play(),
                    MediaCommand::Pause { .. } => player.pause(),
                    MediaCommand::Seek { seconds, .. } => player.seek(seconds),
                    MediaCommand::SetRate { rate, .. } => player.set_rate(rate),
                    MediaCommand::SetMuted { muted, .. } => player.set_muted(muted),
                }
            }
        });
    }

    /// Replay player events into the page through the glue's own event
    /// system (`__ironsurfFire`, the same path form presses and blitz
    /// interactions take), so `v.addEventListener("timeupdate", …)` works.
    fn fire_media_events(&mut self, events: Vec<(NodeId, PlayerEvent)>) {
        if events.is_empty() || !self.media_enabled {
            return;
        }
        if self.js.is_none() || self.doc.is_none() {
            return;
        }
        media_guard("events", || {
            for (node, event) in events {
                let Some(name) = media_event_name(&event) else { continue };
                let init = match &event {
                    PlayerEvent::TimeUpdate { position } => {
                        format!("{{ position: {position} }}")
                    }
                    PlayerEvent::Error { detail } => {
                        format!("{{ message: {} }}", js_string_lit(detail))
                    }
                    _ => "{}".to_string(),
                };
                if let (Some(js), Some(doc)) = (self.js.as_mut(), self.doc.as_mut()) {
                    js.fire_event(doc, Some(node), name, &init);
                }
            }
        });
    }

    /// Rewrite the snapshots the page's `HTMLMediaElement` properties read.
    fn refresh_media_snapshots(&mut self) {
        let Some(js) = self.js.as_ref() else { return };
        let mut host = js.host().borrow_mut();
        host.media.snapshots.clear();
        for entry in &self.media {
            let player = entry.player.borrow();
            host.media
                .snapshots
                .insert(entry.node_id.as_u64(), MediaSnapshot::of(&player));
        }
    }

    /// One media step of the poll loop (task: playback). Called after every
    /// event the loop handles, and on its own at least every `MEDIA_TICK`
    /// while any player is alive (the loop must not idle-block past it).
    /// Every media call runs inside [`media_guard`]: nothing here may panic
    /// out of the tab thread.
    pub fn pump_media(&mut self) {
        if !self.media_enabled {
            return;
        }
        // (Also the per-tick `attach_video_widgets` pass: widgets for
        // `<video>`s the parser or a script created since last time.)
        self.attach_video_widgets();
        self.drain_media_loaded();
        self.drain_media_js_commands();

        let mut repaint = false;
        let mut fired: Vec<(NodeId, PlayerEvent)> = Vec::new();
        for entry in &self.media {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || {
                    let mut player = entry.player.borrow_mut();
                    let presented = player.pump();
                    let events = player.take_events();
                    let needs = player.needs_repaint();
                    (presented, events, needs)
                },
            ));
            match outcome {
                Ok((presented, events, needs)) => {
                    // The widget reads the frame through the shared player —
                    // `set_frame`/`set_state` are the widget's own views into
                    // this same object, so a presented frame plus a repaint
                    // request is everything it needs.
                    if presented || needs {
                        repaint = true;
                    }
                    fired.extend(events.into_iter().map(|event| (entry.node_id, event)));
                }
                Err(_) => error!(
                    "media: player for node {} panicked; element abandoned",
                    entry.node_id.as_u64()
                ),
            }
        }
        // The cadence anchor: alive while anything is pumping or waiting on
        // bytes; silent once every player is parked.
        self.media_next_pump = if self.media.is_empty() {
            None
        } else if self.media.iter().any(|entry| {
            let player = entry.player.borrow();
            player.needs_repaint() || matches!(player.state(), PlayerState::Loading | PlayerState::Playing)
        }) {
            Some(Instant::now() + MEDIA_TICK)
        } else {
            // A failed/ended/parked document needs no clock at all — but
            // keep one slow tick while a fetch is still in flight.
            if self.media_inflight.is_empty() {
                None
            } else {
                Some(Instant::now() + MEDIA_TICK)
            }
        };

        if repaint {
            self.needs_paint = true;
        }
        self.fire_media_events(fired);
        media_guard("snapshots", || self.refresh_media_snapshots());
    }

    /// Cap the poll timeout by the next media step (the `timer_poll_tick`
    /// shape, for media): with no live players this returns `base` — the
    /// media feature costs an empty `Vec` check.
    fn media_poll_tick(&self, base: Duration) -> Duration {
        match self.media_next_pump {
            Some(due) => base.min(due.saturating_duration_since(Instant::now())),
            None => base,
        }
    }
}

/// A throwaway temp dir for one media fetch's private cache (unique per
/// href per process — the dedupe guarantees one fetch per href).
fn private_media_cache_dir(href: &str) -> PathBuf {
    // FNV-1a — a temp-dir name needs uniqueness, not cryptography.
    let mut hash: u64 = 0xcbf2_9ce3_4ea2_7b17;
    for byte in href.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    std::env::temp_dir().join(format!(
        "ironsurf-media-{}-{hash:016x}",
        std::process::id()
    ))
}

/// The media fetch worker (runs off the tab thread): the tab's own document
/// fetcher for the resolved URL (so container cookies, the cache, and the
/// https policy apply to media exactly as they do to pages), accepting only
/// 2xx bodies of at most `MAX_VIDEO_BYTES`. Errors come back as the engine's
/// user-facing [`MediaError`]s, which is what the element paints and
/// `v.mediaError` reports.
fn fetch_media_bytes(
    url: &Url,
    policy: NetPolicy,
    jar: Arc<Mutex<CookieJar>>,
    user_agent: netmod::SharedUserAgent,
    href: &str,
) -> Result<Vec<u8>, MediaError> {
    // The tab's own cache stays on the tab thread; this worker gets a private
    // temp cache for its one URL, deleted before it returns — media never
    // touches the container cache. Neither the shared jar nor this private
    // cache is held across the wire: the fetch locks per operation.
    let cache_dir = private_media_cache_dir(href);
    let fetched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cache = Mutex::new(NetCache::with_limits(cache_dir.clone(), 1, MAX_VIDEO_BYTES * 2));
        netmod::fetch_document_report_shared(
            url,
            policy,
            &jar,
            &cache,
            netmod::now_unix(),
            &user_agent,
            None,
        )
    }));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let outcome = fetched.map_err(|_| MediaError::Unreachable {
        detail: format!("{href}: the fetch worker panicked"),
    })?;
    match outcome {
        Ok(fetch) => {
            if !(200..=299).contains(&fetch.status) {
                return Err(MediaError::Unreachable {
                    detail: format!("{href}: HTTP {}", fetch.status),
                });
            }
            if fetch.body.len() > MAX_VIDEO_BYTES {
                return Err(MediaError::TooLarge {
                    bytes: fetch.body.len(),
                    limit: MAX_VIDEO_BYTES,
                });
            }
            Ok(fetch.body)
        }
        Err(err) => Err(MediaError::Unreachable {
            detail: err.to_string(),
        }),
    }
}

/// Translate the ironsurf modifier bits (`ironsurf_common::Modifiers`, its own
/// `u8` layout) into `keyboard_types::Modifiers` — the two bit layouts differ,
/// so this must never be a cast (`docs/BLITZ_API_NOTES.md` §4).
/// The element's `href`/`src` attribute resolved against the document URL
/// (context-menu targets, task: context menu) — the same fallback
/// [`Tab::hover_link`] uses: an attribute that will not resolve is reported
/// trimmed and verbatim, empty attributes report no target at all.
fn resolved_attr(doc: &HtmlDocument, el: &ElementData, name: LocalName) -> Option<String> {
    let raw = el.attr(name).map(str::trim).filter(|v| !v.is_empty())?;
    Some(
        doc.url()
            .join(raw)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| raw.to_owned()),
    )
}

/// Can this element take a caret (and therefore a paste)? Exactly the
/// textish `<input>`s plus `<textarea>` — this engine has no
/// `contenteditable` (blitz beta.2), and button/checkbox/radio-style inputs
/// have no editor to paste into.
fn is_editable_element(el: &ElementData) -> bool {
    if el.name.local == blitz_dom::local_name!("textarea") {
        return true;
    }
    if el.name.local != blitz_dom::local_name!("input") {
        return false;
    }
    !matches!(
        el.attr(blitz_dom::local_name!("type"))
            .map(|t| t.trim().to_ascii_lowercase())
            .as_deref(),
        Some(
            "button" | "checkbox" | "color" | "file" | "hidden" | "image" | "radio" | "range"
            | "reset" | "submit"
        )
    )
}

/// The page's own computed CSS `cursor:` keyword on this node (task: hover
/// cursor): `None` for `auto` (nothing said — this engine's heuristics
/// decide); `cursor: none` reports [`CursorIcon::Hidden`]. Mirrors blitz's
/// private `stylo_to_cursor_icon` (its `Document::get_cursor` applies the
/// same mapping, but this tab must consult the keyword *before* deciding
/// whether its own affordances apply — a `<button>`'s label is selectable
/// text, and blitz would report the I-beam).
fn css_cursor(
    styles: &impl std::ops::Deref<Target = style::servo_arc::Arc<style::properties::ComputedValues>>,
) -> Option<CursorIcon> {
    use style::values::computed::ui::CursorKind;
    match styles.clone_cursor().keyword {
        CursorKind::Auto => None,
        CursorKind::None => Some(CursorIcon::Hidden),
        keyword => Some(ipc_cursor(match keyword {
            CursorKind::Default => BlitzCursorIcon::Default,
            CursorKind::Pointer => BlitzCursorIcon::Pointer,
            CursorKind::Text => BlitzCursorIcon::Text,
            CursorKind::Wait => BlitzCursorIcon::Wait,
            CursorKind::Progress => BlitzCursorIcon::Progress,
            CursorKind::Help => BlitzCursorIcon::Help,
            CursorKind::Crosshair => BlitzCursorIcon::Crosshair,
            CursorKind::Alias => BlitzCursorIcon::Alias,
            CursorKind::Copy => BlitzCursorIcon::Copy,
            CursorKind::Move => BlitzCursorIcon::Move,
            CursorKind::NoDrop => BlitzCursorIcon::NoDrop,
            CursorKind::NotAllowed => BlitzCursorIcon::NotAllowed,
            CursorKind::Grab => BlitzCursorIcon::Grab,
            CursorKind::Grabbing => BlitzCursorIcon::Grabbing,
            CursorKind::ColResize => BlitzCursorIcon::ColResize,
            CursorKind::RowResize => BlitzCursorIcon::RowResize,
            CursorKind::NResize => BlitzCursorIcon::NResize,
            CursorKind::EResize => BlitzCursorIcon::EResize,
            CursorKind::SResize => BlitzCursorIcon::SResize,
            CursorKind::WResize => BlitzCursorIcon::WResize,
            CursorKind::NeResize => BlitzCursorIcon::NeResize,
            CursorKind::NwResize => BlitzCursorIcon::NwResize,
            CursorKind::SeResize => BlitzCursorIcon::SeResize,
            CursorKind::SwResize => BlitzCursorIcon::SwResize,
            CursorKind::EwResize => BlitzCursorIcon::EwResize,
            CursorKind::NsResize => BlitzCursorIcon::NsResize,
            CursorKind::NeswResize => BlitzCursorIcon::NeswResize,
            CursorKind::NwseResize => BlitzCursorIcon::NwseResize,
            CursorKind::ContextMenu => BlitzCursorIcon::ContextMenu,
            CursorKind::Cell => BlitzCursorIcon::Cell,
            CursorKind::VerticalText => BlitzCursorIcon::VerticalText,
            CursorKind::AllScroll => BlitzCursorIcon::AllScroll,
            CursorKind::ZoomIn => BlitzCursorIcon::ZoomIn,
            CursorKind::ZoomOut => BlitzCursorIcon::ZoomOut,
            CursorKind::Auto | CursorKind::None => unreachable!("handled above"),
        })),
    }
}

/// Map blitz's `cursor-icon` vocabulary (what the CSS `cursor` property
/// resolves to, task: hover cursor) onto the IPC [`CursorIcon`]. Slint's
/// `MouseCursor` cannot draw a handful of the exotic shapes; those fall back
/// to the plain arrow rather than being mis-drawn.
fn ipc_cursor(icon: BlitzCursorIcon) -> CursorIcon {
    match icon {
        BlitzCursorIcon::Default => CursorIcon::Default,
        BlitzCursorIcon::Pointer => CursorIcon::Pointer,
        BlitzCursorIcon::Text => CursorIcon::Text,
        BlitzCursorIcon::VerticalText => CursorIcon::VerticalText,
        BlitzCursorIcon::Wait => CursorIcon::Wait,
        BlitzCursorIcon::Progress => CursorIcon::Progress,
        BlitzCursorIcon::Help => CursorIcon::Help,
        BlitzCursorIcon::Crosshair => CursorIcon::Crosshair,
        BlitzCursorIcon::Alias => CursorIcon::Alias,
        BlitzCursorIcon::Copy => CursorIcon::Copy,
        BlitzCursorIcon::Move => CursorIcon::Move,
        BlitzCursorIcon::NoDrop => CursorIcon::NoDrop,
        BlitzCursorIcon::NotAllowed => CursorIcon::NotAllowed,
        BlitzCursorIcon::Grab => CursorIcon::Grab,
        BlitzCursorIcon::Grabbing => CursorIcon::Grabbing,
        BlitzCursorIcon::ColResize => CursorIcon::ColResize,
        BlitzCursorIcon::RowResize => CursorIcon::RowResize,
        BlitzCursorIcon::NResize => CursorIcon::NResize,
        BlitzCursorIcon::EResize => CursorIcon::EResize,
        BlitzCursorIcon::SResize => CursorIcon::SResize,
        BlitzCursorIcon::WResize => CursorIcon::WResize,
        BlitzCursorIcon::NeResize => CursorIcon::NeResize,
        BlitzCursorIcon::NwResize => CursorIcon::NwResize,
        BlitzCursorIcon::SeResize => CursorIcon::SeResize,
        BlitzCursorIcon::SwResize => CursorIcon::SwResize,
        BlitzCursorIcon::EwResize => CursorIcon::EwResize,
        BlitzCursorIcon::NsResize => CursorIcon::NsResize,
        BlitzCursorIcon::NeswResize => CursorIcon::NeswResize,
        BlitzCursorIcon::NwseResize => CursorIcon::NwseResize,
        // No Slint shape for these (and `None` never reaches here — it means
        // "no hover node", which the caller treats as "nothing asked").
        _ => CursorIcon::Default,
    }
}

/// Does this element ask to be clicked without the page having said so in
/// CSS (task: hover cursor)? Exactly the controls a click actuates anyway —
/// `<button>`, `<summary>`, the actionable `<input>` types (a bare `type`
/// is `text`, which stays an I-beam via blitz), `[onclick]`, and ARIA roles
/// that read as buttons/links. The plain-textish inputs are deliberately
/// absent: blitz already reports `Text` for the caret controls.
fn is_clickable_element(el: &ElementData) -> bool {
    let name = &el.name.local;
    if *name == blitz_dom::local_name!("button") || *name == blitz_dom::local_name!("summary") {
        return true;
    }
    if *name == blitz_dom::local_name!("input") {
        return matches!(
            el.attr(blitz_dom::local_name!("type"))
                .map(|t| t.trim().to_ascii_lowercase())
                .as_deref(),
            Some(
                "button" | "checkbox" | "color" | "file" | "image" | "radio" | "range" | "reset"
                | "submit"
            )
        );
    }
    if el.attr(blitz_dom::local_name!("onclick")).is_some() {
        return true;
    }
    matches!(
        el.attr(blitz_dom::local_name!("role"))
            .map(|r| r.trim().to_ascii_lowercase())
            .as_deref(),
        Some(
            "button" | "link" | "tab" | "menuitem" | "option" | "checkbox" | "radio" | "switch"
                | "slider"
        )
    )
}

/// The DOM `*Key` flags of a pointer event, as a fragment of the JS object
/// literal handed to `__ironsurfFire` (`""` when nothing is held, so the
/// common case reads exactly as it used to).
///
/// macOS needs one swap: the shell forwards Command in `ctrl` (Slint swaps
/// Command and Control so that cross-platform `Control-…` bindings read as ⌘),
/// while a page testing `event.metaKey` means that very Command key.
fn js_modifier_init(m: Modifiers) -> String {
    let (ctrl, meta) = if cfg!(target_os = "macos") {
        (m.meta(), m.ctrl())
    } else {
        (m.ctrl(), m.meta())
    };
    let mut out = String::new();
    if ctrl {
        out.push_str(", ctrlKey: true");
    }
    if meta {
        out.push_str(", metaKey: true");
    }
    if m.shift() {
        out.push_str(", shiftKey: true");
    }
    if m.alt() {
        out.push_str(", altKey: true");
    }
    out
}

fn to_blitz_mods(m: Modifiers) -> KMods {
    let mut k = KMods::empty();
    if m.ctrl() {
        k |= KMods::CONTROL;
        // macOS: this flag *is* Command. The shell forwards Slint's flags
        // untouched and Slint swaps the two keys on Apple platforms (Command →
        // `control`, Control → `meta`) so that cross-platform `Control-…`
        // bindings read as ⌘ — while blitz spells the platform's editing chord
        // `util::ACTION_MOD` = SUPER. Swap it back or ⌘-A / ⌘-C / ⌘-V in a text
        // field read as Control-letters and the editor never moves.
        if cfg!(target_os = "macos") {
            k |= KMods::SUPER;
        }
    }
    if m.shift() {
        k |= KMods::SHIFT;
    }
    if m.alt() {
        k |= KMods::ALT;
    }
    if m.meta() {
        k |= KMods::META;
        // macOS: a backend that does *not* swap hands over Command here, and it
        // means the same chord (`Modifiers::action` accepts both spellings).
        if cfg!(target_os = "macos") {
            k |= KMods::SUPER;
        }
    }
    k
}

/// The modifier the tab itself keys its editing chords on — mirrors
/// `blitz-dom`: Ctrl everywhere, Cmd on macOS.
///
/// macOS accepts **either** spelling: the shell forwards Command in `ctrl`
/// because Slint swaps the two keys on Apple platforms (`KeyboardModifiers`:
/// Command → `control`, Control → `meta`) so that cross-platform
/// `@keys(Control-…)` bindings read as ⌘, while backends that do not swap keep
/// Command in `meta`. See [`Modifiers::action`].
#[cfg(target_os = "macos")]
fn is_action_chord(ev: &KeyEvent) -> bool {
    ev.mods.action()
}

/// The modifier the tab itself keys its editing chords on — mirrors
/// `blitz-dom`: Ctrl everywhere, Cmd on macOS.
#[cfg(not(target_os = "macos"))]
fn is_action_chord(ev: &KeyEvent) -> bool {
    ev.mods.ctrl() && !ev.mods.meta()
}

/// The AppKit standard-keybinding command this key press stands for, or `None`
/// when it is not an editing command.
///
/// Why the tab has to say it: blitz-dom's text-input key handling skips
/// `Key::Backspace` on macOS outright — "On macOS this is handled by the apple
/// standard keybindings" (`node/text.rs`) — and expects its embedder to send the
/// `deleteBackward:`-style command instead. Blitz's own winit provider gets
/// those from Cocoa; IronSurf's window is a Slint window, whose delete key
/// arrives here as an ordinary key event. Without this the key did nothing at
/// all in a page's `<input>` on a Mac (task: backspace no longer works on mac
/// forms), while working on every other platform.
///
/// Only the deleting key is translated — it is the one the engine cfg's out.
/// Arrows, Home/End, forward Delete and character input all reach blitz from
/// the key event itself, and the page's own `keydown` is fired separately by
/// [`Runtime::fire_js_key`], so nothing is dropped and nothing happens twice.
pub fn apple_keybinding(ev: &KeyEvent) -> Option<&'static str> {
    match ev.key.as_str() {
        // The key labelled `delete` on an Apple keyboard *is* Backspace; with ⌥
        // it deletes a word, matching the word-delete the other platforms get
        // from the same chord.
        "Backspace" => Some(if ev.mods.alt() {
            "deleteWordBackward:"
        } else {
            "deleteBackward:"
        }),
        _ => None,
    }
}

/// Whether this build must feed the AppKit commands at all: elsewhere blitz-dom
/// handles `Key::Backspace` from the key event, so feeding the command too would
/// delete twice.
pub const APPLE_KEYBINDINGS: bool = cfg!(target_os = "macos");

/// A freshly-synthesized editing chord (context-menu Paste) must carry the
/// platform's action modifier exactly as blitz expects it.
#[cfg(target_os = "macos")]
fn blitz_action_chord() -> KMods {
    KMods::SUPER
}

/// A freshly-synthesized editing chord (context-menu Paste) must carry the
/// platform's action modifier exactly as blitz expects it.
#[cfg(not(target_os = "macos"))]
fn blitz_action_chord() -> KMods {
    KMods::CONTROL
}

/// Net policy is fixed at startup; read it back from the provider so the
/// runtime never carries a second copy.
fn self_provider_policy(provider: &Arc<BlitzNetProvider>) -> NetPolicy {
    provider.policy()
}

/// A submit control press armed on mouse-down (task: `<form>`): finalised on
/// the matching release, after the page's JS `click` handlers have run.
#[derive(Clone, Copy)]
struct FormPress {
    submitter: NodeId,
    form: NodeId,
}

/// Escape a Rust string for embedding inside a JS string literal (the event
/// init of `__ironsurfFire`).
fn js_string_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A `NavigationOptions` carrying a `POST` body (blitz's own submission —
/// the fallback path if a form ever submits past the tab's interception):
/// rebuild the request body from `Body::Form` entries (urlencoded) or bytes
/// so the fetch sends a real POST with the right `Content-Type` instead of a
/// bodyless GET. `None` = an ordinary GET navigation (link clicks).
fn form_body_from_nav(options: &NavigationOptions) -> Option<MainDocPost> {
    use blitz_traits::net::Body;
    let content_type = match &options.document_resource {
        Body::Empty => return None,
        Body::Form(_) => Some(
            options
                .content_type
                .clone()
                .unwrap_or_else(|| "application/x-www-form-urlencoded".to_owned()),
        ),
        Body::Bytes(_) => options.content_type.clone(),
    };
    let body = match &options.document_resource {
        Body::Form(form) => {
            let entries: Vec<(String, String)> = form
                .0
                .iter()
                .map(|entry| (entry.name.clone(), entry.value.as_ref().to_owned()))
                .collect();
            forms::serialize_urlencoded(&entries).into_bytes()
        }
        Body::Bytes(bytes) => bytes.to_vec(),
        Body::Empty => Vec::new(),
    };
    Some(MainDocPost { content_type, body })
}

/// `blitz_traits::net::NetWaker`: Blitz's own providers wake the doc client
/// on completion (even on error, which has no delivery path); ours pokes the
/// same self-pipe the poll-loop watches.
struct PipeWaker(Arc<SelfPipe>);
impl blitz_traits::net::NetWaker for PipeWaker {
    fn wake(&self, _client_id: usize) {
        self.0.wake();
    }
}

/// What a navigation should do to the URL-only session history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavHistory {
    /// Fresh load: truncate forward entries (redirect replaces the entry).
    Fresh,
    /// In-place load of history slot `usize` (Back/Forward/Reload).
    Session(usize),
}

// ---------------------------------------------------------------------------
// stdin pump + main loop

/// Mark stdin non-blocking: the loop only reads when `poll()` says readable,
/// partial frames accumulate in `pending` and can never park the loop.
fn set_stdin_nonblocking() -> io::Result<()> {
    let fd = io::stdin().as_raw_fd();
    // SAFETY: F_GETFL/F_SETFL on a valid descriptor.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `read_frame`-compatible reader over a fixed in-memory frame: the length
/// prefix has already been validated, `len` payload bytes are available.
struct CursorReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Read for CursorReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.buf[self.pos..];
        let n = remaining.len().min(out.len());
        out[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

/// Pull everything currently available on non-blocking stdin into `pending`.
/// `Ok(true)` = EOF (the shell closed the pipe). Shared by the poll loop
/// ([`drain_stdin`]) and the modal dialog pump ([`DialogHost::ask`]).
fn read_stdin_bytes(fd: RawFd, pending: &mut Vec<u8>) -> io::Result<bool> {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        // SAFETY: read into a valid buffer from stdin.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n > 0 {
            pending.extend_from_slice(&chunk[..n as usize]);
            if (n as usize) < chunk.len() {
                return Ok(false);
            }
            continue;
        }
        if n == 0 {
            return Ok(true); // Shell closed the pipe: same as Destroy.
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            return Ok(false);
        }
        return Err(err);
    }
}

/// Decode every complete `ToTab` frame currently sitting in `pending`
/// (length-prefixed bincode via common), consuming exactly those bytes; a
/// partial frame at the tail waits for the rest. Shared by the poll loop and
/// the modal dialog pump — the buffer lives outside both so bytes read across
/// a re-entrant modal can never be lost.
fn decode_stdin_frames(pending: &mut Vec<u8>) -> io::Result<Vec<ToTab>> {
    let mut batch: Vec<ToTab> = Vec::new();
    loop {
        if pending.len() < 4 {
            return Ok(batch);
        }
        let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
        if len > ironsurf_common::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame too large: {len} bytes"),
            ));
        }
        if pending.len() < 4 + len {
            return Ok(batch); // wait for the rest
        }
        // Deserialize exactly one frame (length-prefixed bincode via common).
        let mut reader = CursorReader {
            buf: &pending[..4 + len],
            pos: 0,
        };
        let msg: ToTab = read_frame(&mut reader)?;
        pending.drain(..4 + len);
        batch.push(msg);
    }
}

/// Read stdin (into the shared `pending` buffer) and handle every complete
/// `ToTab` frame. `Ok(Some(code))` requests shutdown.
///
/// Frames deferred by a modal dialog ([`DialogHost`]) go first — they were
/// read off the pipe before anything this tick pulled in — and the batch is
/// then dispatched with superseded hover moves dropped (see [`coalesce_moves`]).
fn drain_stdin(tab: &mut Tab, pending: &RefCell<Vec<u8>>) -> io::Result<Option<i32>> {
    let fd = io::stdin().as_raw_fd();
    let eof = {
        let mut pending = pending.borrow_mut();
        read_stdin_bytes(fd, &mut pending)?
    };
    let mut batch: Vec<ToTab> = Vec::new();
    batch.extend(tab.dialog.take_deferred());
    batch.extend(decode_stdin_frames(&mut pending.borrow_mut())?);

    for msg in coalesce_moves(batch) {
        if let Some(code) = tab.handle_to_tab(msg) {
            return Ok(Some(code));
        }
    }

    if eof {
        debug!("stdin EOF: shell closed the pipe, persisting and exiting");
        tab.persist();
        return Ok(Some(0));
    }
    Ok(None)
}

/// Is this a bare pointer move (no buttons involved)?
fn is_move(msg: &ToTab) -> bool {
    matches!(
        msg,
        ToTab::Pointer(PointerEvent { kind, .. })
            if matches!(kind, PointerKind::Move | PointerKind::Over | PointerKind::Out)
    )
}

/// Collapse runs of consecutive hover moves down to the newest one.
///
/// A mouse travelling across the page emits far more move events per second
/// than the tab can hit-test and repaint, and every stale position it does
/// process delays the click that follows it. Only the final coordinate of an
/// uninterrupted run of moves is worth resolving; presses, releases, keys,
/// scrolls and every shell command keep their order and are never dropped.
fn coalesce_moves(msgs: Vec<ToTab>) -> Vec<ToTab> {
    let mut out: Vec<ToTab> = Vec::with_capacity(msgs.len());
    for msg in msgs {
        if is_move(&msg) && out.last().is_some_and(is_move) {
            // Replace a pending move; a move following any other event is kept.
            let last = out.last_mut().expect("last is a move");
            *last = msg;
            continue;
        }
        out.push(msg);
    }
    out
}

#[cfg(test)]
mod input_shaping_tests {
    use super::*;

    fn move_at(x: f32, y: f32) -> ToTab {
        ToTab::Pointer(PointerEvent {
            x,
            y,
            kind: PointerKind::Move,
            button: ironsurf_common::MouseButton::Left,
            mods: Modifiers::NONE,
        })
    }

    fn press_at(x: f32, y: f32) -> ToTab {
        ToTab::Pointer(PointerEvent {
            x,
            y,
            kind: PointerKind::Down,
            button: ironsurf_common::MouseButton::Left,
            mods: Modifiers::NONE,
        })
    }

    #[test]
    fn a_run_of_moves_collapses_to_the_newest() {
        let batch = vec![move_at(1.0, 1.0), move_at(2.0, 2.0), move_at(3.0, 3.0)];
        let out = coalesce_moves(batch);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            move_at(3.0, 3.0),
            "only the newest position of a move run survives"
        );
    }

    #[test]
    fn presses_and_commands_are_never_dropped_or_reordered() {
        let batch = vec![
            move_at(1.0, 1.0),
            move_at(2.0, 2.0),
            press_at(2.0, 2.0),
            move_at(4.0, 4.0),
            ToTab::Reload,
            move_at(5.0, 5.0),
            move_at(6.0, 6.0),
        ];
        let out = coalesce_moves(batch);
        assert_eq!(
            out,
            vec![
                move_at(2.0, 2.0),
                press_at(2.0, 2.0),
                move_at(4.0, 4.0),
                ToTab::Reload,
                move_at(6.0, 6.0)
            ],
            "the click and the shell command survive in order"
        );
    }
}

/// Poll `[stdin, self-pipe]` until readable or `tick` elapses.
fn poll_inputs(stdin_fd: RawFd, wake_fd: RawFd, tick: Duration) -> bool {
    let mut pfd = [
        libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: two valid pollfd entries.
    let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 2, tick.as_millis() as i32) };
    ready > 0
}

/// Run the tab loop; returns the process exit code.
pub fn run(args: TabArgs) -> i32 {
    // Label every line with this tab id; level comes from `--log`/`-v`, then
    // `IRONSURF_LOG`/`RUST_LOG` (inherited from the shell), then `info`.
    let argv: Vec<String> = std::env::args().collect();
    let level = logging::init(
        &format!("tab {}", args.tab_id.0),
        logging::level_from_args(&argv),
    );
    info!(
        "starting: container {} profile {} start url {:?} net policy {:?} sandbox {} log {:?}",
        args.container_id.0,
        args.profile_dir.display(),
        args.url,
        args.net_policy,
        !args.no_sandbox,
        level,
    );
    if args.no_sandbox {
        warn!("sandbox disabled (--no-sandbox): this tab is not confined");
    } else {
        // fds (stdin/stdout/profile) survive Landlock: apply touches the
        // filesystem view only, already-open descriptors keep working.
        apply_sandbox(&args);
    }

    let pipe = match SelfPipe::new() {
        Ok(pipe) => Arc::new(pipe),
        Err(err) => {
            error!("cannot create the wake self-pipe: {err}");
            let mut out = TabOut::new();
            out.send(FromTab::Error {
                message: format!("self-pipe: {err}"),
            });
            return 1;
        }
    };
    let mut tab = Tab::new(&args, Arc::clone(&pipe));

    if set_stdin_nonblocking().is_err() {
        // A blocking stdin would deadlock the poll loop; without a usable
        // descriptor we cannot speak IPC anyway — degrade by exiting.
        error!("stdin non-blocking failed: cannot run the poll loop");
        return 1;
    }

    tab.emit(FromTab::Ready {
        tab_id: args.tab_id,
    });

    // Initial navigation (the shell may still send Navigate later; the
    // start URL mirrors the old runtime's behaviour of loading eagerly).
    let start = args.url.clone();
    tab.navigate(&start, NavHistory::Fresh);

    let stdin_fd = io::stdin().as_raw_fd();
    let wake_fd = pipe.read_fd();
    // The stdin byte buffer lives on the tab (the modal dialog pump reads
    // through the very same one).
    let pending = Rc::clone(&tab.stdin_pending);
    let mut exit_code = 0;

    loop {
        let tick = if tab.needs_paint {
            FRAME_INTERVAL
        } else {
            IDLE_TICK
        };
        // Never sleep past the earliest JS timer deadline (task: real timers),
        // nor past the media cadence while a video is alive (task: playback).
        let tick = tab.media_poll_tick(tab.timer_poll_tick(tick));
        // …nor past the next slice of a gliding wheel (task: slower and
        // smoother wheel) — the glide is fed on its own clock, not on wakes.
        let tick = tab.wheel_poll_tick(tick);
        let tick = tab.nav_poll_tick(tick);
        poll_inputs(stdin_fd, wake_fd, tick);
        // Wake-ups from ShellProvider::request_redraw / the NetWaker.
        let woken = pipe.drain() > 0;
        let mut stdin_frames = false;

        // 1. Shell messages.
        match drain_stdin(&mut tab, &pending) {
            Ok(Some(code)) => {
                exit_code = code;
                break;
            }
            Ok(None) => {}
            Err(err) => {
                error!("stdin protocol error: {err}");
                tab.emit(FromTab::Error {
                    message: format!("protocol error: {err}"),
                });
                exit_code = 2;
                break;
            }
        }
        if !pending.borrow().is_empty() {
            stdin_frames = true;
        }

        // 1b. A main-document fetch that finished while the rest of this
        // cycle was handled (task: faster, responsive loading).
        tab.drain_pending_navigation();

        // 2. Link clicks / form submits (NavigationProvider hook).
        if tab.drain_nav_hook() {
            continue;
        }

        // 2a⁵ Popups (task: block popups): the windows the page asked for since
        // the last poll — allowed ones go to the shell to open, refused ones are
        // reported so the user can see it happened. Runs right after the nav
        // hook because that is where a `target="_blank"` click's request lands.
        tab.drain_popups();

        // 2a. Page copies (Ctrl+C/X, editor cut) surface to the shell here
        // (the shell owns the session clipboard; task: context menu).
        tab.drain_copied();

        // 2a′. Network: every request that settled since the last poll
        // becomes one FromTab::NetworkEvent (DevTools network panel).
        tab.drain_net_events();
        // 2a‴. Archive mode: every archived response settled since the last
        // poll ships to the shell, which owns the archive folder.
        tab.drain_archive_events();

        // 2a⁗. Downloads (task: file downloads): bytes fetched on the worker
        // thread land here — the wake it sends makes this drain immediate, so
        // the shell's panel grows with the file instead of a tick late.
        tab.drain_downloads();

        // 2a″. Console: fetch-side CSS diagnostics (wrong `Content-Type` on a
        // `.css` response) join the DevTools console here (src/logging.rs).
        tab.drain_console_events();

        // 2a‴. Web Workers (task: real worker threads): the timer step below
        // delivers every message a worker thread posted since the last poll,
        // because a page's `onmessage` is a JS turn like a timer callback's and
        // owes the same settle/repaint/console bookkeeping. Draining here instead
        // would leave that turn's console entries (and its title/DOM writes)
        // sitting in the realm until a wake that an idle page never gets.

        // 2b. JS timers: execute every callback due since the last poll
        // (setTimeout/setInterval/rAF), settle, and repaint coalesced exactly
        // like the input path above.
        tab.run_due_timers();

        // 2c. Video: deliver fetched bytes, attach late `<video>`s, advance
        // every player, forward page events, and refresh the snapshots the
        // page's media properties read (task: playback).
        tab.pump_media();

        // 2c′. The wheel glide: feed one eased slice of the travel the wheel
        // events queued since the last slice (task: slower and smoother
        // wheel); the pixels it earns are step 3's business.
        tab.pump_wheel(false);

        // 3. Settle net completions / redraw requests and coalesce frames.
        //    Invisible tabs stay idle — repaints resume on SetVisible(true).
        let paint_due = tab.needs_paint
            && tab
                .last_paint
                .map(|last| last.elapsed() >= FRAME_INTERVAL)
                .unwrap_or(true);
        let activity = woken || stdin_frames;
        if tab.visible && (activity || paint_due) {
            tab.resolve_and_maybe_paint();
        }

        // 3′. Viewport scroll geometry (task: scroll bar): the shell draws the
        //     page's overlay bar from the document's range and offset, so they
        //     travel up the pipe the moment they change. Three field reads and a
        //     half-pixel comparison when nothing moved, and no activity is owed
        //     to notice a JS scroll or a resize that painted no frame.
        tab.report_scroll_state();

        if tab.out.borrow().broken {
            // Shell vanished: persist what we can, then die quietly.
            debug!("stdout write failed: shell is gone, shutting down");
            break;
        }
    }

    info!("exiting with code {exit_code}");
    tab.persist();
    tab.flush_stdout();
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_url_accepts_absolute_urls() {
        assert_eq!(
            parse_input_url("file:///tmp/x/index.html")
                .unwrap()
                .as_str(),
            "file:///tmp/x/index.html"
        );
        assert_eq!(
            parse_input_url("https://example.com/").unwrap().as_str(),
            "https://example.com/"
        );
        assert!(parse_input_url("about:blank").is_ok());
    }

    fn data_fetch(raw: &str, body: &str) -> netmod::DocFetch {
        netmod::DocFetch {
            url: Url::parse(raw).expect("data url"),
            status: 200,
            body: body.as_bytes().to_vec(),
            refresh: None,
            // A `data:` URL has no response headers, so the download decision
            // never comes from here (task: file downloads).
            content_type: None,
            content_disposition: None,
        }
    }

    #[test]
    fn javascript_data_url_renders_as_inert_source() {
        // base64 of `console.log('<b>hi</b>'); document.title='pwned';` — the
        // `<b>hi</b>` inside the program text proves the viewer escapes: tags
        // must be shown as text, never handed back as live markup.
        let source = "console.log('<b>hi</b>'); document.title='pwned';";
        let raw = format!(
            "data:text/javascript;base64,{}",
            crate::netmod::encode_base64(source.as_bytes())
        );
        let html = render_document(&data_fetch(&raw, source));
        assert!(
            html.contains("&lt;b&gt;hi&lt;/b&gt;"),
            "source not escaped: {html}"
        );
        assert!(
            !html.contains("<b>hi</b>"),
            "raw markup leaked into the viewer: {html}"
        );
        // A text document titled by its media type, with the program visible.
        assert!(html.contains("<title>text/javascript</title>"), "{html}");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("base64"), "{html}");
        // Nothing the parser could execute.
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn javascript_data_url_percent_encoded_is_source_too() {
        let html = render_document(&data_fetch(
            "data:application/javascript,document.title%3D1",
            "document.title=1",
        ));
        assert!(html.contains("document.title=1"), "{html}");
        assert!(html.contains("percent-encoded"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn markup_and_other_schemes_reach_the_parser_unchanged() {
        // `text/html` data URLs: parsed exactly as before.
        let html = "<!doctype html><title>ok</title><p>hi</p>";
        assert_eq!(
            render_document(&data_fetch("data:text/html,<p>hi</p>", html)),
            html
        );
        // http(s) and file: bodies never go through the source view, even when
        // the body happens to look like a script.
        let script = "document.title = 'from http'";
        let fetched = netmod::DocFetch {
            url: Url::parse("https://example.com/a.js").unwrap(),
            status: 200,
            body: script.as_bytes().to_vec(),
            refresh: None,
            content_type: None,
            content_disposition: None,
        };
        assert_eq!(render_document(&fetched), script);
    }

    #[test]
    fn parse_input_url_upgrades_bare_hosts() {
        assert_eq!(
            parse_input_url("example.com").unwrap().as_str(),
            "https://example.com/"
        );
        // Bare hosts (Url::parse fails on them) get the https:// upgrade;
        // strings url can read as opaque schemes are kept as-is.
        assert_eq!(
            parse_input_url("example.com/path").unwrap().as_str(),
            "https://example.com/path"
        );
    }

    /// What a page sees of a chord click: the flags pass through to its event,
    /// and on macOS the primary key is spelled the way a page spells it.
    #[test]
    fn pointer_events_carry_their_modifiers_into_the_page() {
        assert_eq!(js_modifier_init(Modifiers::NONE), "");
        assert_eq!(js_modifier_init(Modifiers::NONE.with_shift()), ", shiftKey: true");
        assert_eq!(js_modifier_init(Modifiers::NONE.with_alt()), ", altKey: true");
        // Here the shell forwards Ctrl in `ctrl` and a page calls it `ctrlKey`…
        #[cfg(not(target_os = "macos"))]
        assert_eq!(js_modifier_init(Modifiers::NONE.with_ctrl()), ", ctrlKey: true");
        // …on macOS it forwards ⌘ there, and a page calls that `metaKey`.
        #[cfg(target_os = "macos")]
        assert_eq!(js_modifier_init(Modifiers::NONE.with_ctrl()), ", metaKey: true");
    }

    #[test]
    fn parse_input_url_rejects_free_text() {
        assert!(parse_input_url("not a url").is_err());
        assert!(parse_input_url("").is_err());
    }

    #[test]
    fn tab_args_defaults() {
        let args = TabArgs::parse(&[
            "ironsurf-tab".to_string(),
            "--tab-id".into(),
            "3".into(),
            "--container-id".into(),
            "2".into(),
            "--profile-dir".into(),
            std::env::temp_dir()
                .join("ironsurf-tab-args-test")
                .display()
                .to_string(),
        ])
        .expect("parse");
        assert_eq!(args.tab_id, TabId(3));
        assert_eq!(args.container_id, ContainerId(2));
        assert_eq!(args.url, ironsurf_common::default_url());
        assert_eq!(args.net_policy, NetPolicy::Passthrough);
        assert!(!args.no_sandbox);
        assert!(args.offline_fixtures.is_none());
    }

    #[test]
    fn tab_args_all_flags() {
        let args = TabArgs::parse(&[
            "ironsurf-tab".to_string(),
            "--tab-id".into(),
            "7".into(),
            "--container-id".into(),
            "1".into(),
            "--profile-dir".into(),
            std::env::temp_dir()
                .join("ironsurf-tab-args-test2")
                .display()
                .to_string(),
            "--url".into(),
            "https://example.org".into(),
            "--offline-fixtures".into(),
            "/tmp/fixtures".into(),
            "--managed-net".into(),
            "--no-sandbox".into(),
        ])
        .expect("parse");
        assert_eq!(args.url, "https://example.org");
        assert_eq!(args.net_policy, NetPolicy::Managed);
        assert!(args.no_sandbox);
        assert_eq!(args.offline_fixtures, Some(PathBuf::from("/tmp/fixtures")));
    }

    #[test]
    fn verbosity_flags_are_accepted_not_fatal() {
        // `--log`/`-v` reach the tab from the shell (`--log debug`) or a human;
        // they must parse cleanly (the level itself is read from argv by logging).
        let base = || {
            vec![
                "ironsurf-tab".to_string(),
                "--profile-dir".into(),
                std::env::temp_dir()
                    .join("ironsurf-tab-args-log")
                    .display()
                    .to_string(),
            ]
        };
        for flag in ["--log", "--log=trace", "-v", "-vv", "--verbose", "--debug"] {
            let mut args = base();
            args.push(flag.to_string());
            if flag == "--log" {
                args.push("debug".to_string());
            }
            let parsed = TabArgs::parse(&args);
            assert!(parsed.is_ok(), "{flag} must be accepted: {parsed:?}");
        }
        // A bad level is not an argument error either (logging falls back to the env/default).
        let mut args = base();
        args.extend(["--log".to_string(), "nope".to_string()]);
        assert!(TabArgs::parse(&args).is_ok());
    }

    #[test]
    fn self_pipe_wakes_and_drains() {
        let pipe = SelfPipe::new().expect("pipe");
        pipe.wake();
        pipe.wake();
        // Data is pending: poll(0) must report readable immediately.
        assert!(poll_inputs(-1, pipe.read_fd(), Duration::from_millis(0)));
        pipe.drain();
        assert!(!poll_inputs(-1, pipe.read_fd(), Duration::from_millis(0)));
    }

    // ------------------------------------------------------------------
    // Chord clicks (Ctrl/⌘-left, middle) and what the page is told
    // ------------------------------------------------------------------

    const CHORD_PAGE: &str = r#"<!doctype html>
<html><head><title>CH</title></head>
<body style="margin:0">
  <a id="l" href="next.html" style="display:block;width:200px;height:40px">link</a>
  <p id="out">-</p>
  <script>
    document.getElementById("l").addEventListener("click", function (e) {
      document.getElementById("out").textContent = "ctrl=" + e.ctrlKey + " meta=" + e.metaKey;
    });
  </script>
</body></html>"#;

    /// The page must see the modifiers of the click the shell forwarded — that
    /// is how a page gets a chance to cancel the chord (`e.preventDefault()`).
    /// The same event that opens the new tab, in other words.
    #[test]
    fn a_chord_click_reports_its_modifiers_to_the_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("chord.html");
        std::fs::write(&page, CHORD_PAGE).expect("write page");
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        tab.navigate(
            url::Url::from_file_path(&page).expect("file url").as_str(),
            NavHistory::Fresh,
        );

        // Held-Ctrl release over the link: the click reaches the page, with the
        // primary key spelled the way a page spells it (⌘ on macOS, Ctrl here).
        tab.handle_to_tab(ptr_mods(100.0, 20.0, PointerKind::Move, Modifiers::NONE.with_ctrl()));
        tab.handle_to_tab(ptr_mods(100.0, 20.0, PointerKind::Down, Modifiers::NONE.with_ctrl()));
        tab.handle_to_tab(ptr_mods(100.0, 20.0, PointerKind::Up, Modifiers::NONE.with_ctrl()));
        let seen = tab
            .document_mut()
            .and_then(|doc| text_of(doc, "#out"))
            .unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        assert_eq!(seen, "ctrl=true meta=false", "the page saw Ctrl-click");
        #[cfg(target_os = "macos")]
        assert_eq!(seen, "ctrl=false meta=true", "the page saw ⌘-click");
    }

    // ------------------------------------------------------------------
    // Form controls ("<input>, <button> do not work") — reproduction
    // ------------------------------------------------------------------

    const FORM_PAGE: &str = r#"<!doctype html>
<html><head><title>Form</title></head>
<body style="margin:0">
  <input id="i" style="display:block;width:200px;height:40px">
  <button id="b" style="display:block;width:100px;height:40px">Go</button>
  <div id="out"></div>
  <script>
    var i = document.querySelector('#i');
    var b = document.querySelector('#b');
    var out = document.querySelector('#out');
    b.addEventListener('click', function () { out.textContent = 'clicked:' + i.value; });
  </script>
</body></html>"#;

    /// One cookie jar per container per tab: the main-document handle and
    /// the Blitz subresource provider must be the SAME jar, and the single
    /// shutdown flush must persist whatever either put into it.
    #[test]
    fn the_tab_and_its_provider_share_one_cookie_jar() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let args = TabArgs {
            tab_id: TabId(1),
            container_id: ContainerId(1),
            profile_dir: dir.path().to_path_buf(),
            url: "about:blank".into(),
            offline_fixtures: None,
            net_policy: NetPolicy::Managed,
            no_sandbox: true,
            media_enabled: true,
            user_agent: None,
            proxy: None,
            archive_mode: false,
            private: false,
        };
        let pipe = Arc::new(SelfPipe::new().expect("pipe"));
        let tab = Tab::new_headless(&args, pipe);
        assert!(
            Arc::ptr_eq(&tab.jar, &tab.provider.jar()),
            "Tab::jar and the provider must alias one jar, not two copies"
        );
        // Same rule for the cache: documents, `<script src>` bodies and
        // subresources all read and write ONE NetCache per tab.
        assert!(
            Arc::ptr_eq(&tab.cache, &tab.provider.cache_handle()),
            "Tab::cache and the provider must alias one cache, not two copies"
        );
        // Set through the tab's main-document handle, flush through the
        // provider (the Destroy path) — the cookie must land on disk.
        {
            let mut jar = tab.jar.lock().expect("jar");
            jar.set_from_set_cookie(
                &url::Url::parse("https://example.com/").expect("url"),
                "shared=1; Path=/; Secure",
                netmod::now_unix(),
            );
        }
        tab.provider.save_cookies().expect("save");
        let on_disk =
            std::fs::read_to_string(dir.path().join("cookies.json")).expect("cookies.json");
        assert!(on_disk.contains("shared"), "{on_disk}");
    }

    /// CSS animations advance with the tab's monotonic clock: blitz resolves
    /// keyframe values at `current_time_for_animations`, and a constant 0 left
    /// every `animation: … fade` (`0% { opacity: 0 }`) element invisible — the
    /// reason Mastodon's fetched `/explore` posts painted blank.
    #[test]
    fn css_animation_advances_with_the_tab_clock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = form_tab(dir.path());
        let url = write_page(
            dir.path(),
            "<!doctype html><html><head><style>\
             @keyframes fade{0%{opacity:0}100%{opacity:1}}\
             .status{opacity:1;animation:.15s linear fade}\
             </style></head><body><div class=\"status\" id=\"s\">post</div></body></html>",
        );
        assert!(tab.navigate(&url, NavHistory::Fresh));
        // The animation starts at the clock's zero (navigate resolves there).
        // Jump the clock a full second past the 0.15 s fade and settle again:
        // a clock-driven resolve reads the finished animation (opacity 1),
        // while the old constant-0 resolve stayed pinned at the 0% keyframe.
        tab.anim_clock = Instant::now() - Duration::from_secs(1);
        tab.settle();
        let end: f64 = status_opacity(&mut tab).parse().expect("opacity number");
        assert!(end > 0.95, "after the fade the status is opaque, got {end}");
    }

    fn status_opacity(tab: &mut Tab) -> String {
        let doc = tab.doc.as_ref().expect("document loaded");
        let id = doc.get_element_by_id("s").expect("#s in the document");
        let node = doc.get_node(id).expect("live node");
        let styles = node.primary_styles().expect("resolved styles");
        crate::js::computed_style_value(&*styles, "opacity")
    }

    fn form_tab(dir: &std::path::Path) -> Tab {
        let args = TabArgs {
            tab_id: TabId(1),
            container_id: ContainerId(1),
            profile_dir: dir.to_path_buf(),
            url: "about:blank".into(),
            offline_fixtures: None,
            net_policy: NetPolicy::Passthrough,
            no_sandbox: true,
            media_enabled: true,
            user_agent: None,
            proxy: None,
            archive_mode: false,
            private: false,
        };
        let pipe = Arc::new(SelfPipe::new().expect("pipe"));
        Tab::new_headless(&args, pipe)
    }

    /// Text the history page is showing (its `<body>`), for asserting the shell's
    /// answer actually reached the screen.
    fn body_text(tab: &mut Tab) -> String {
        match tab.doc.as_mut().and_then(|doc| doc.query_selector("body").ok()) {
            Some(Some(id)) => tab
                .doc
                .as_mut()
                .and_then(|doc| doc.get_node(id))
                .map(|n| n.text_content())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn visit(url: &str, title: &str) -> ironsurf_common::HistoryVisit {
        ironsurf_common::HistoryVisit {
            url: url.into(),
            title: Some(title.into()),
            at: 1_700_000_000,
        }
    }

    /// The history page (task: `ironsurf://history`): rendered locally, but the
    /// visit log is the shell's file, so navigating there remembers the page,
    /// asks (the emit goes to the harness's null sink; the shell's own test
    /// covers the wire), and shows the shell's answer when it lands — with the
    /// page on screen re-rendering in place, so Back still steps over it once.
    #[test]
    fn the_history_page_shows_the_shell_answer_and_its_revisions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = form_tab(dir.path());
        netmod::set_history_listing(None);

        assert!(tab.navigate(netmod::HISTORY_URL, NavHistory::Fresh));
        assert_eq!(tab.history_page.as_deref(), Some(netmod::HISTORY_URL));
        assert!(
            body_text(&mut tab).contains("Asking the browser for the visit log"),
            "the page waits honestly, inventing neither rows nor a count"
        );

        // The shell answers: the listing lands on the screen that asked for it.
        tab.handle_to_tab(ToTab::HistoryList {
            tab_id: TabId(1),
            entries: vec![visit("https://example.org/one", "One")],
            total: 1,
        });
        let text = body_text(&mut tab);
        assert!(text.contains("https://example.org/one"), "the visit is not on screen: {text}");
        assert!(text.contains("One"), "the page's own title should head the row: {text}");

        // The same answer again changes nothing on screen — which is what keeps
        // the request/answer pair from reloading the page in a loop.
        let before = text;
        tab.handle_to_tab(ToTab::HistoryList {
            tab_id: TabId(1),
            entries: vec![visit("https://example.org/one", "One")],
            total: 1,
        });
        assert_eq!(body_text(&mut tab), before);

        // The log emptied under the page (Clear history in the settings drawer):
        // the open page is told and redraws itself empty.
        tab.handle_to_tab(ToTab::HistoryList { tab_id: TabId(1), entries: vec![], total: 0 });
        let text = body_text(&mut tab);
        assert!(text.contains("Nothing yet."), "the emptied log is not on screen: {text}");
        assert!(!text.contains("example.org/one"));

        // The page's own address is the plain one, query and all: a link to
        // `?clear=1` empties the log and stays on `ironsurf://history`, so a
        // later Reload re-reads the log instead of clearing it again.
        assert!(tab.navigate(&format!("{}?clear=1", netmod::HISTORY_URL), NavHistory::Fresh));
        assert_eq!(tab.history_page.as_deref(), Some(netmod::HISTORY_URL));
        netmod::set_history_listing(None);
    }

    /// Task: faster, responsive loading — an `http(s)` main-document fetch
    /// runs on the network worker, not on the tab thread: `navigate` returns
    /// once the fetch is *in flight* (the page on screen stays alive and
    /// interactive), and `wait_for_navigation` adopts the result — the very
    /// `drain_pending_navigation` the poll loop runs between ticks. Local
    /// schemes stay synchronous (their fetch is a read).
    #[test]
    fn an_http_document_loads_on_the_network_worker() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback test server");
        let port = listener.local_addr().expect("test addr").port();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).ok();
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nconnection: close\r\n\r\n\
                       <!doctype html><html><head><title>Worker page</title>\
                       <style>body{margin:0;height:4000px}</style></head>\
                       <body><p>served off-thread</p></body></html",
                );
            }
        });
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        let url = format!("http://127.0.0.1:{port}/served");
        assert!(tab.navigate(&url, NavHistory::Fresh), "the navigation starts");
        assert!(
            tab.wait_for_navigation(std::time::Duration::from_secs(15)),
            "the worker fetch completes"
        );
        tab.settle();
        assert_eq!(
            text_of(tab.document_mut().expect("doc"), "title").unwrap_or_default(),
            "Worker page",
            "the served document is the loaded one at {url}"
        );
        assert_eq!(
            text_of(tab.document_mut().expect("doc"), "p").unwrap_or_default(),
            "served off-thread",
            "the served document is laid out and painted"
        );
        server.join().ok();
    }

    /// A tab whose tab→shell frames land in a buffer the test can decode
    /// (`new_headless` sends them to a discarding sink).
    fn capturing_tab(dir: &std::path::Path) -> (Tab, Arc<std::sync::Mutex<Vec<u8>>>) {
        let mut tab = form_tab(dir);
        let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
        tab.out = Rc::new(RefCell::new(TabOut {
            writer: Box::new(Capture(Arc::clone(&sink))),
            broken: false,
        }));
        (tab, sink)
    }

    /// [`std::io::Write`] into a shared buffer.
    #[derive(Clone)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The URLs a (capturing) tab has fetched through the network stack.
    fn fetched_urls(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<String> {
        emitted(sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::NetworkEvent { event } => Some(event.url),
                _ => None,
            })
            .collect()
    }

    /// Every [`FromTab`] a (capturing) tab has emitted so far.
    fn emitted(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<FromTab> {
        let bytes = sink.lock().expect("lock").clone();
        let mut cursor = std::io::Cursor::new(bytes);
        let mut msgs = Vec::new();
        while let Ok(msg) = ironsurf_common::read_frame(&mut cursor) {
            msgs.push(msg);
        }
        msgs
    }

    /// `ironsurf://archive` renders the internal summary in this tab *and*
    /// asks the shell for its browsing server, carrying the page's query
    /// (`?root=` …) through as typed overrides.
    #[test]
    fn the_archive_page_renders_and_asks_the_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = capturing_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        netmod::set_archive_server_status(None);
        assert!(
            tab.navigate(
                "ironsurf://archive?root=%2Ftmp%2Fmy%20archive&port=8123",
                NavHistory::Fresh
            ),
            "the archive page is a real page"
        );

        let asks: Vec<_> = emitted(&sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::ArchiveServerStart { root, host, port, open_in_new_tab } => {
                    Some((root, host, port, open_in_new_tab))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            asks.len(),
            1,
            "one request per render of the page: {asks:?}"
        );
        assert_eq!(
            asks[0],
            (
                Some("/tmp/my archive".into()),
                None,
                Some(8123),
                false,
            ),
            "the query rides through, percent-decoded"
        );
        // The page stayed: no `open=1` means the shell answers this tab, and
        // an address is not known yet.
        let urls: Vec<String> = emitted(&sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::UrlChanged { url } => url,
                _ => None,
            })
            .collect();
        assert!(
            urls.last()
                .is_some_and(|url| url.starts_with("ironsurf://archive")),
            "the summary is the current page: {urls:?}"
        );
    }

    /// With the server up, the summary tab steps through to the server's own
    /// control page; a tab that asked for `?open=1` (the shell opens the
    /// listing in a *new* tab) is left where it is.
    #[test]
    fn the_answer_takes_the_summary_page_to_the_control_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = capturing_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        netmod::set_archive_server_status(None);
        assert!(tab.navigate("ironsurf://archive", NavHistory::Fresh));
        tab.handle_to_tab(ToTab::ArchiveServerInfo {
            tab_id: TabId(1),
            ok: true,
            address: "http://127.0.0.1:1".into(), // deliberately dead: the *intent* is asserted
            error: None,
        });
        // The hand-off is an `http(s)` navigation on the network worker
        // (task: faster, responsive loading) — let it land (here: refuse) so
        // its network events ship before the assertions read them.
        tab.wait_for_navigation(std::time::Duration::from_secs(5));
        // The tab asked the server's own page: a fetch of
        // `<address>/ironsurf/serve` leaves this tab (the address itself is
        // dead here on purpose — only the intent is under test).
        let fetched: Vec<String> = fetched_urls(&sink);
        assert!(
            fetched
                .iter()
                .any(|url| url == "http://127.0.0.1:1/ironsurf/serve"),
            "the summary hands the tab to the server: {fetched:?}"
        );

        // `?open=1`: the summary stays put (the listing belongs to a new tab).
        let (mut opener, sink) = capturing_tab(dir.path());
        opener.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        assert!(opener.navigate("ironsurf://archive?open=1", NavHistory::Fresh));
        opener.handle_to_tab(ToTab::ArchiveServerInfo {
            tab_id: TabId(1),
            ok: true,
            address: "http://127.0.0.1:1".into(),
            error: None,
        });
        let fetched: Vec<String> = fetched_urls(&sink);
        assert!(
            !fetched.iter().any(|url| url.starts_with("http://")),
            "the ?open=1 summary stays put: {fetched:?}"
        );
        netmod::set_archive_server_status(None);
    }

    fn ptr(x: f32, y: f32, kind: PointerKind) -> ToTab {
        ptr_mods(x, y, kind, Modifiers::NONE)
    }

    fn ptr_mods(x: f32, y: f32, kind: PointerKind, mods: Modifiers) -> ToTab {
        ToTab::Pointer(PointerEvent { x, y, kind, button: ironsurf_common::MouseButton::Left, mods })
    }

    /// Tag name of the currently focused node (if it is an element).
    fn focused_tag(doc: &mut blitz_html::HtmlDocument) -> Option<blitz_dom::LocalName> {
        let id = doc.get_focussed_node_id()?;
        let node = doc.get_node(id)?;
        Some(node.element_data()?.name.local.clone())
    }

    /// Live editor text of the currently focused text input.
    fn focused_input_value(doc: &mut blitz_html::HtmlDocument) -> Option<String> {
        let id = doc.get_focussed_node_id()?;
        let node = doc.get_node(id)?;
        let el = node.element_data()?;
        Some(el.text_input_data()?.editor.raw_text().to_string())
    }

    /// Text content of the first element matching `selector`.
    fn text_of(doc: &mut blitz_html::HtmlDocument, selector: &str) -> Option<String> {
        let id = doc.query_selector(selector).ok()??;
        Some(doc.get_node(id)?.text_content())
    }

    fn key(ch: char) -> ToTab {
        ToTab::Key(KeyEvent {
            key: ch.to_string(),
            code: format!("Key{}", ch.to_ascii_uppercase()),
            text: Some(ch.to_string()),
            repeat: false,
            mods: Modifiers::NONE,
        })
    }

    // -- page zoom (Ctrl-+ / Ctrl-- / Ctrl-0) --------------------------------

    /// `ToTab::SetZoom` drives the document viewport, survives resize, is
    /// inherited by fresh documents, and rescales pointer coordinates.
    #[test]
    fn zoom_rescales_viewport_frames_and_pointer_space() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        tab.navigate("about:blank", NavHistory::Fresh);
        assert!(tab.document_mut().is_some(), "about:blank must load a document");
        assert_eq!(tab.document_mut().expect("doc").viewport().zoom(), 1.0);

        // 150%: viewport zoom follows, the document is told, frames keep
        // their physical size (the zoom is CSS-side, not a resize).
        tab.handle_to_tab(ToTab::SetZoom { zoom: 1.5 });
        assert!((tab.viewport().zoom() - 1.5).abs() < 1e-6);
        assert!((tab.document_mut().expect("doc").viewport().zoom() - 1.5).abs() < 1e-6);

        // Resize keeps the zoom (both directions through viewport()).
        tab.handle_to_tab(ToTab::Resize { width: 640, height: 480, scale: 1.0 });
        assert!((tab.viewport().zoom() - 1.5).abs() < 1e-6);

        // Pointer coordinates: logical px ÷ zoom = CSS px (at scroll 0 the
        // page axes equal the client axes).
        let coords = tab.pointer_coords(300.0, 150.0);
        assert!((coords.client_x - 200.0).abs() < 1e-3, "client_x = {}", coords.client_x);
        assert!((coords.page_x - coords.client_x).abs() < 1e-3);

        // Nonsense values are rejected (zoom stays at the last good factor).
        tab.handle_to_tab(ToTab::SetZoom { zoom: f32::NAN });
        tab.handle_to_tab(ToTab::SetZoom { zoom: -2.0 });
        assert!((tab.viewport().zoom() - 1.5).abs() < 1e-6);

        // A fresh document (new navigation) is built with the current zoom.
        tab.navigate("about:blank", NavHistory::Fresh);
        assert!((tab.document_mut().expect("doc").viewport().zoom() - 1.5).abs() < 1e-6);
    }

    /// Click the <input>, type "hi", click the <button>; the page's click
    /// handler must run and report the typed value.
    #[test]
    fn repro_input_button_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("f.html");
        std::fs::write(&page, FORM_PAGE).expect("write page");
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        tab.navigate(
            url::Url::from_file_path(&page).expect("file url").as_str(),
            NavHistory::Fresh,
        );

        // 1. Click the input (centre of the 200x40 box at the top-left).
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Move));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Down));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Up));
        let focused_is_input = tab
            .document_mut()
            .and_then(focused_tag)
            .is_some_and(|name| name == blitz_dom::local_name!("input"));
        assert!(focused_is_input, "clicking <input> must focus it");

        // 2. Type "hi".
        tab.handle_to_tab(key('h'));
        tab.handle_to_tab(key('i'));
        let value = tab.document_mut().and_then(focused_input_value);
        assert_eq!(value.as_deref(), Some("hi"), "typing into the focused <input>");

        // 3. Click the button (the 100x40 box under the input): its click
        //    handler writes "clicked:hi" into #out.
        tab.handle_to_tab(ptr(50.0, 60.0, PointerKind::Move));
        tab.handle_to_tab(ptr(50.0, 60.0, PointerKind::Down));
        tab.handle_to_tab(ptr(50.0, 60.0, PointerKind::Up));
        let out_text = tab
            .document_mut()
            .and_then(|doc| text_of(doc, "#out"))
            .unwrap_or_default();
        assert_eq!(out_text, "clicked:hi", "<button> click handler must run with the live value");
    }

    // ------------------------------------------------------------------
    // Context menu (task: right-click menu)
    // ------------------------------------------------------------------

    const CONTEXT_PAGE: &str = r#"<!doctype html>
<html><head><title>C</title></head>
<body style="margin:0">
  <a href="next.html"><img src="pic.png" style="display:block;width:200px;height:100px"></a>
  <video src="v.mp4" width="320" height="160" style="display:block"></video>
  <input id="i" style="display:block;width:200px;height:40px">
</body></html>"#;

    fn context_tab(dir: &std::path::Path) -> (Tab, url::Url) {
        let page = dir.join("c.html");
        std::fs::write(&page, CONTEXT_PAGE).expect("write page");
        let mut tab = form_tab(dir);
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        let base = url::Url::from_file_path(&page).expect("file url");
        tab.navigate(base.as_str(), NavHistory::Fresh);
        (tab, base)
    }

    #[test]
    fn a_right_click_reports_the_link_image_and_video_under_the_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tab, base) = context_tab(dir.path());

        // Over the image — which sits inside the link: both are targets.
        let (target, video) = tab.context_target(100.0, 50.0);
        assert!(video.is_none(), "no video up there");
        assert_eq!(
            target.image.as_deref(),
            Some(base.join("pic.png").expect("join").as_str())
        );
        assert_eq!(
            target.link.as_deref(),
            Some(base.join("next.html").expect("join").as_str()),
            "the ancestor <a href> is part of the context too"
        );
        assert!(!target.editable);

        // Over the video: its (resolved) src, no player running yet, and
        // the node identity a ToggleVideo action later looks the player up by.
        let (target, video) = tab.context_target(100.0, 180.0);
        assert!(video.is_some(), "the video node is reported alongside");
        assert_eq!(
            target.video.as_deref(),
            Some(base.join("v.mp4").expect("join").as_str())
        );
        assert!(!target.video_playing, "nothing plays before the menu is used");

        // Over the input: an editable target, nothing else.
        let (target, _) = tab.context_target(100.0, 280.0);
        assert!(target.editable, "<input> can take a paste");
        assert!(target.link.is_none() && target.image.is_none() && target.video.is_none());

        // Over the blank page: the empty menu, and no stale target.
        let (target, _) = tab.context_target(600.0, 500.0);
        assert!(
            target == ContextTarget::default(),
            "plain page must not invent targets: {target:?}"
        );
    }

    #[test]
    fn the_context_paste_lands_in_the_focused_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, _base) = context_tab(dir.path());

        // Focus the input the way a user does: a left click in its box.
        tab.handle_to_tab(ptr(100.0, 280.0, PointerKind::Move));
        tab.handle_to_tab(ptr(100.0, 280.0, PointerKind::Down));
        tab.handle_to_tab(ptr(100.0, 280.0, PointerKind::Up));
        assert!(
            tab.document_mut()
                .and_then(focused_tag)
                .is_some_and(|t| t == blitz_dom::local_name!("input")),
            "the input is focused before the paste"
        );

        // The menu's Paste row: the shell hands the clipboard text over and
        // the tab drives it through blitz's own Ctrl+V path.
        tab.handle_context_action(ContextAction::Paste {
            text: "pasted words".into(),
        });
        assert_eq!(
            tab.document_mut().and_then(focused_input_value).as_deref(),
            Some("pasted words"),
            "the paste went into the focused editor"
        );

        // The clipboard cache is what blitz's Ctrl+V reads: a later real
        // Ctrl+V keystroke must insert the same text again.
        tab.handle_to_tab(ToTab::Clipboard { text: "via chord".into() });
        tab.handle_to_tab(ToTab::Key(KeyEvent {
            key: "v".into(),
            code: "KeyV".into(),
            text: Some("v".into()),
            repeat: false,
            mods: action_mod(),
        }));
        assert_eq!(
            tab.document_mut().and_then(focused_input_value).as_deref(),
            Some("pasted wordsvia chord"),
            "menu paste then Ctrl+V, one editor"
        );
    }

    #[test]
    fn shell_copies_surface_as_clipboard_text_and_toggle_video_is_safe_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, _base) = context_tab(dir.path());

        // The ShellProvider copy slot drains empty-safe…
        tab.drain_copied();
        assert!(tab.shell.copied.lock().expect("copied").is_none());
        // …and drains the page copy blitz queued (what a real Ctrl+C does).
        let copied = ShellProvider::set_clipboard_text(&*tab.shell, "from the page".into());
        assert!(copied.is_ok(), "the copy hook must accept the page's copy");
        assert_eq!(
            tab.shell.copied.lock().expect("copied").as_deref(),
            Some("from the page")
        );
        tab.drain_copied(); // emits FromTab::ClipboardText (headless: /dev/null)
        assert!(tab.shell.copied.lock().expect("copied").is_none(), "drained exactly once");

        // ToggleVideo with no right-clicked video is a no-op, and with an
        // unloaded (Idle) player `play()` cannot start anything — neither
        // may panic.
        tab.handle_context_action(ContextAction::ToggleVideo);
        tab.context_video = tab
            .doc
            .as_ref()
            .and_then(|doc| {
                doc.tree().iter().find_map(|(id, node)| {
                    let el = node.element_data()?;
                    (el.name.local == blitz_dom::local_name!("video")).then_some(id)
                })
            });
        tab.handle_context_action(ContextAction::ToggleVideo);
        assert!(
            tab.media.iter().all(|e| !e.player.borrow().is_playing()),
            "an Idle player (unfetched src) must not pretend to play"
        );
    }

    // ------------------------------------------------------------------
    // Text selection copy & paste (task: selection copy/paste)
    //
    // Dragging selects page text (blitz paints the highlight), a right-click
    // reports the live selection to the shell's menu, and Ctrl+C ships
    // exactly that selection to the shell as `FromTab::ClipboardText` — the
    // same hook an editor copy rides on. The shell never consumes the chord;
    // it pushes its session clipboard in front of every Ctrl+V.
    // ------------------------------------------------------------------

    const SELECT_PAGE: &str = r#"<!doctype html>
<html><head><title>S</title></head>
<body style="margin:0">
  <p id="t" style="margin:0;font-size:24px">The quick brown fox jumps over the lazy dog</p>
</body></html>"#;

    fn selection_tab(dir: &std::path::Path) -> (Tab, Arc<std::sync::Mutex<Vec<u8>>>) {
        std::fs::write(dir.join("s.html"), SELECT_PAGE).expect("write page");
        let (mut tab, sink) = capturing_tab(dir);
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        tab.navigate(
            url::Url::from_file_path(dir.join("s.html")).expect("file url").as_str(),
            NavHistory::Fresh,
        );
        (tab, sink)
    }

    /// The platform's primary chord modifier as the shell forwards it: Ctrl
    /// everywhere, Cmd on macOS (mirrors `blitz-dom::util::ACTION_MOD`).
    fn action_mod() -> Modifiers {
        #[cfg(target_os = "macos")]
        {
            Modifiers::NONE.with_meta()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Modifiers::NONE.with_ctrl()
        }
    }

    /// The action chord exactly as the shell forwards it (the chord is the
    /// letter with just the primary modifier — the shell intercepts copy by
    /// contract never).
    fn ctrl_c() -> ToTab {
        ToTab::Key(KeyEvent {
            key: "c".into(),
            code: "KeyC".into(),
            text: Some("c".into()),
            repeat: false,
            mods: action_mod(),
        })
    }

    /// The paste chord as the shell forwards it, right after pushing the
    /// session clipboard with `ToTab::Clipboard`.
    fn ctrl_v() -> ToTab {
        ToTab::Key(KeyEvent {
            key: "v".into(),
            code: "KeyV".into(),
            text: Some("v".into()),
            repeat: false,
            mods: action_mod(),
        })
    }

    /// Select-all as the shell forwards it — also never a shell-intercepted
    /// chord.
    fn ctrl_a() -> ToTab {
        ToTab::Key(KeyEvent {
            key: "a".into(),
            code: "KeyA".into(),
            text: Some("a".into()),
            repeat: false,
            mods: action_mod(),
        })
    }

    fn named_key(key: &str, code: &str, shift: bool) -> ToTab {
        ToTab::Key(KeyEvent {
            key: key.into(),
            code: code.into(),
            text: None,
            repeat: false,
            mods: if shift { Modifiers::NONE.with_shift() } else { Modifiers::NONE },
        })
    }

    /// Every `FromTab::ClipboardText` a (capturing) tab has shipped.
    fn clipboard_texts(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<String> {
        emitted(sink)
            .into_iter()
            .filter_map(|m| match m {
                FromTab::ClipboardText { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    /// The selection highlight is painted where the selected text actually is,
    /// at every page-zoom level. The highlight rectangles come out of parley
    /// in *layout* units, which are device pixels once page zoom scales the
    /// text layout — painting them straight into device space (without
    /// dividing by the layout scale) draws them 2× too far/wide at 200 %
    /// (covering the words to the right of the selection, even the next
    /// paragraph) and half-size at 50 %. Pin the geometry: the highlight box
    /// at 200 % must be exactly twice the box at 100 % (same CSS layout, twice
    /// the device pixels), and likewise at 50 %.
    #[test]
    fn selection_highlight_tracks_the_text_at_every_zoom() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("hl.html"),
            r#"<!doctype html>
<html><head><title>H</title></head>
<body style="margin:0">
  <p style="margin:0;font-size:24px">AAA BBB CCC DDD EEE</p>
</body></html>"#,
        )
        .expect("write");
        let (mut tab, sink) = selection_tab(dir.path());
        tab.navigate(
            url::Url::from_file_path(dir.path().join("hl.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );

        /// `f64::abs_diff` is unstable and `abs_diff` on ints is unsigned
        /// only — a tiny helper for "within N" comparisons.
        fn within(a: f64, b: f64, eps: f64) -> bool {
            (a - b).abs() <= eps
        }

        /// A box scaled by `k` (a 5-tuple's pixel-count field is dropped).
        fn scale_box(b: Box5, k: f64) -> (f64, f64, f64, f64) {
            (b.0 as f64 * k, b.1 as f64 * k, b.2 as f64 * k, b.3 as f64 * k)
        }

        fn near(a: (f64, f64, f64, f64), b: Box5, eps: f64) -> bool {
            let b = scale_box(b, 1.0);
            [
                within(a.0, b.0, eps),
                within(a.1, b.1, eps),
                within(a.2, b.2, eps),
                within(a.3, b.3, eps),
            ]
            .iter()
            .all(|v| *v)
        }

        // The highlight rectangles blitz-paint fills with SELECTION_COLOR
        // (180, 213, 255); the page has nothing else of that colour.
        fn highlight_box(tab: &mut Tab, sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Box5 {
            tab.paint_now();
            let frame = emitted(sink)
                .into_iter()
                .rev()
                .find_map(|m| match m {
                    FromTab::Frame { width, height, rgba, .. } => Some((width, height, rgba)),
                    _ => None,
                })
                .expect("frame");
            let (w, h, rgba) = (frame.0 as usize, frame.1 as usize, frame.2);
            let hit = |p: &[u8]| {
                (p[0] as i32 - 180).abs() <= 6
                    && (p[1] as i32 - 213).abs() <= 6
                    && (p[2] as i32 - 255).abs() <= 6
            };
            let mut box_: Option<(usize, usize, usize, usize)> = None;
            let mut n = 0usize;
            for y in 0..h {
                for x in 0..w {
                    if hit(&rgba[(y * w + x) * 4..]) {
                        n += 1;
                        box_ = Some(match box_ {
                            Some((x0, y0, x1, y1)) => {
                                (x0.min(x), y0.min(y), x1.max(x), y1.max(y))
                            }
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            let (x0, y0, x1, y1) = box_.expect("a highlight was painted");
            // +1: the box covers its last painted pixel inclusive.
            (x0 as u32, y0 as u32, (x1 + 1) as u32, (y1 + 1) as u32, n as u32)
        }

        type Box5 = (u32, u32, u32, u32, u32);

        // Drag-selects "AAA BBB …" at `zoom`, keeping the CSS viewport 800 px
        // wide (window px = CSS px × zoom, so a 200 % window is 1600 px) and
        // returning the painted highlight box + covered pixel count. The tab
        // divides pointer coords by the zoom, so logical x = CSS x × zoom.
        let at_zoom = |tab: &mut Tab, sink: &Arc<std::sync::Mutex<Vec<u8>>>, zoom: f32, win: u32| -> Box5 {
            tab.handle_to_tab(ToTab::SetZoom { zoom });
            tab.handle_to_tab(ToTab::Resize { width: win, height: 600, scale: 1.0 });
            // CSS y 12 is inside the line box whatever the zoom; CSS x 100 is
            // just past "AAA BBB" (7 chars ≈ 93 px), so the same words are
            // selected at every zoom.
        tab.handle_to_tab(ptr(2.0 * zoom, 12.0 * zoom, PointerKind::Down));
            tab.handle_to_tab(ptr(100.0 * zoom, 12.0 * zoom, PointerKind::Move));
            tab.handle_to_tab(ptr(100.0 * zoom, 12.0 * zoom, PointerKind::Up));
            let sel = tab.document_mut().expect("doc").get_selected_text();
            assert!(
                sel.as_deref().is_some_and(|s| s.starts_with("AAA BB") && s.len() < 12),
                "the drag selects the same words at zoom {zoom}: {sel:?}"
            );
            highlight_box(tab, sink)
        };

        let b100 = at_zoom(&mut tab, &sink, 1.0, 800);
        let b200 = at_zoom(&mut tab, &sink, 2.0, 1600);
        let b50 = at_zoom(&mut tab, &sink, 0.5, 400);
        println!(
            "100% box={b100:?} · 200% box={b200:?} · 50% box={b50:?} (last field = painted px count)"
        );
        assert!(
            near(scale_box(b100, 2.0), b200, 2.0),
            "200% highlight must be 100%'s box doubled: {b100:?} vs {b200:?}"
        );
        assert!(
            near(scale_box(b100, 0.5), b50, 2.0),
            "50% highlight must be 100%'s box halved: {b100:?} vs {b50:?}"
        );
    }

    /// The report in full: a highlight dragged in the **second** paragraph
    /// paints on the second paragraph — never on the first, and never across
    /// a sibling element — at 100 %, 200 % and 50 %, with the Inspector pane
    /// open as well.
    ///
    /// The failure was a space mismatch in the paint: block boxes were
    /// converted from CSS px with the tab's *hidpi* factor, while parley's
    /// inline layer (glyph runs, selection rects, inline backgrounds) had
    /// already been laid out at blitz's **total** scale (hidpi × zoom). At
    /// zoom ≠ 1 the two layers disagreed by `(zoom − 1) × position`, so the
    /// highlight of a drag in paragraph two landed on paragraph one (at 50 %,
    /// on whichever element happened to share the row). Opening devtools only
    /// made it obvious — its Inspector marker measures CSS rects in the same
    /// pass, so the wrong-element highlight became impossible to miss.
    #[test]
    fn a_highlight_paints_on_the_paragraph_you_dragged_at_every_zoom() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("two.html"),
            r#"<!doctype html>
<html><head><title>T</title></head>
<body style="margin:0">
  <p style="margin:0;font-size:24px;color:#111">AAA BBB CCC DDD EEE</p>
  <p style="margin:8px 0 0;font-size:24px;color:#0a0">111 222 333 444 555</p>
</body></html>"#,
        )
        .expect("write");
        let (mut tab, sink) = selection_tab(dir.path());
        tab.navigate(
            url::Url::from_file_path(dir.path().join("two.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );

        fn frame(tab: &mut Tab, sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> (u32, u32, Vec<u8>) {
            tab.paint_now();
            emitted(sink)
                .into_iter()
                .rev()
                .find_map(|m| match m {
                    FromTab::Frame { width, height, rgba, .. } => Some((width, height, rgba)),
                    _ => None,
                })
                .expect("frame")
        }

        /// Rows + horizontal extent of a colour class, so each paragraph can
        /// be located by the colour of *its own* text.
        fn extent(rgba: &[u8], w: usize, h: usize, on: impl Fn(&[u8]) -> bool) -> Option<(usize, usize, usize, usize)> {
            let mut box_: Option<(usize, usize, usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if on(&rgba[(y * w + x) * 4..]) {
                        box_ = Some(match box_ {
                            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                            None => (x, y, x, y),
                        });
                    }
                }
            }
            box_
        }

        // Each paragraph is painted in its own colour, so "where is paragraph
        // two" never needs guessing from row bands.
        let black = |p: &[u8]| p[0] < 90 && p[1] < 90 && p[2] < 90;
        let green = |p: &[u8]| p[1] as i32 > p[0] as i32 + 20 && p[1] as i32 > p[2] as i32 + 20;
        // SELECTION_COLOR (180, 213, 255) — nothing else on the page, nor the
        // Inspector's orange/yellow markers, comes near it.
        let highlight = |p: &[u8]| {
            (p[0] as i32 - 180).abs() <= 6
                && (p[1] as i32 - 213).abs() <= 6
                && (p[2] as i32 - 255).abs() <= 6
        };

        // The selection rect is the full line box of the laid-out font, and
        // that box is taller on macOS (system font metrics); let it hug the
        // ink within one extra line's worth of slack there, otherwise 14 px.
        fn highlight_bottom_slack() -> usize {
            #[cfg(target_os = "macos")]
            {
                28
            }
            #[cfg(not(target_os = "macos"))]
            {
                14
            }
        }

        for (zoom, win, devtools) in [
            (1.0f32, 800u32, false),
            (2.0, 1600, false),
            (0.5, 400, false),
            (2.0, 1600, true),
        ] {
            tab.handle_to_tab(ToTab::SetZoom { zoom });
            tab.handle_to_tab(ToTab::Resize { width: win, height: 600, scale: 1.0 });
            if devtools {
                tab.handle_to_tab(ToTab::SetInspect { open: true });
            }
            // A plain click collapses the previous drag's selection so the
            // next frame shows where the text itself sits.
            tab.handle_to_tab(ptr(300.0 * zoom, 12.0 * zoom, PointerKind::Down));
            tab.handle_to_tab(ptr(300.0 * zoom, 12.0 * zoom, PointerKind::Up));
            let (fw, fh, rgba) = frame(&mut tab, &sink);
            let (w, h) = (fw as usize, fh as usize);
            let p1 = extent(&rgba, w, h, black).expect("paragraph one is painted dark");
            let p2 = extent(&rgba, w, h, green).expect("paragraph two is painted green");
            assert!(
                p1.3 < p2.1,
                "paragraph two sits below paragraph one at zoom {zoom}: {p1:?} vs {p2:?}"
            );

            // Drag across "111 222" inside the second paragraph: CSS y 51 is
            // inside its line box whatever the zoom (the CSS viewport stays
            // 800 px wide because window px = CSS px × zoom).
            tab.handle_to_tab(ptr(2.0 * zoom, 51.0 * zoom, PointerKind::Down));
            tab.handle_to_tab(ptr(96.0 * zoom, 51.0 * zoom, PointerKind::Move));
            tab.handle_to_tab(ptr(96.0 * zoom, 51.0 * zoom, PointerKind::Up));

            let sel = tab.document_mut().expect("doc").get_selected_text();
            assert!(
                sel.as_deref().is_some_and(|s| s.starts_with("111")),
                "the drag selects paragraph two's own words at zoom {zoom}: {sel:?}"
            );

            let (fw, fh, rgba) = frame(&mut tab, &sink);
            let sel_box = extent(&rgba, fw as usize, fh as usize, highlight)
                .expect("the drag paints a highlight");
            assert!(
                sel_box.1 > p1.3,
                "the highlight stays below paragraph one's ink at zoom {zoom}                  (devtools {devtools}): sel {sel_box:?}, paragraph one {p1:?}",
            );
            assert!(
                sel_box.3 < p2.3 + highlight_bottom_slack(),
                "the highlight does not spill below paragraph two's ink at zoom {zoom}                  (devtools {devtools}): sel {sel_box:?}, paragraph two {p2:?}",
            );
            assert!(
                sel_box.0 + 6 >= p2.0,
                "the highlight starts no earlier than paragraph two's own text at zoom {zoom}                  (devtools {devtools}): sel {sel_box:?}, paragraph two {p2:?}",
            );
            assert!(
                sel_box.2 <= p2.2 + 6,
                "the highlight ends no later than paragraph two's own text at zoom {zoom}                  (devtools {devtools}): sel {sel_box:?}, paragraph two {p2:?}",
            );
        }
    }

    /// Drag-selection hits the paragraph under the cursor **after the page has
    /// been scrolled down** (and after zoom + scroll): the selection lands on
    /// the words whose line the pointer crossed, never on text from another
    /// element. This is the pointer-space bug class — page coordinates must
    /// carry the viewport scroll in, and the text-offset hit test must run in
    /// the same space the hit test resolved.
    #[test]
    fn a_drag_selects_the_paragraph_under_the_cursor_after_scrolling_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("scroll-sel.html"),
            r#"<!doctype html>
<html><head><title>SS</title></head>
<body style="margin:0">
  <p id="one" style="margin:0;font-size:24px">AAA BBB CCC DDD EEE</p>
  <div style="height:1600px"></div>
  <p id="two" style="margin:0;font-size:24px">111 222 333 444 555</p>
  <div style="height:1600px"></div>
</body></html>"#,
        )
        .expect("write");
        let (mut tab, _sink) = selection_tab(dir.path());
        tab.navigate(
            url::Url::from_file_path(dir.path().join("scroll-sel.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );

        // Wheel down far enough that paragraph two sits in the viewport
        // (the glide is landed with `pump_wheel(true)` — the test harness
        // has no poll loop to feed it).
        tab.handle_to_tab(ptr(400.0, 300.0, PointerKind::Move));
        tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
            x: 400.0,
            y: 300.0,
            delta_x: 0.0,
            delta_y: -2600.0,
        }));
        tab.pump_wheel(true);
        let y = scroll_y(&mut tab);
        assert!(y > 0.0, "the wheel scrolled the page down: scroll_y={y}");

        // Paragraph two's own box, in client CSS px, after the scroll.
        let (_cx, cy) = {
            let doc = tab.document_mut().expect("doc");
            let id = doc.get_element_by_id("two").expect("#two");
            let rect = inspector::box_rect(doc, doc.get_node(id).expect("node")).expect("box");
            assert!(
                rect.y0 >= 0.0 && rect.y1 <= 600.0,
                "paragraph two is fully visible after the scroll: {rect:?}"
            );
            (((rect.x0 + rect.x1) / 2.0) as f32, ((rect.y0 + rect.y1) / 2.0) as f32)
        };

        // Drag across the start of its line: from just inside its left edge.
        let x0 = 2.0;
        tab.handle_to_tab(ptr(x0, cy, PointerKind::Move));
        tab.handle_to_tab(ptr(x0, cy, PointerKind::Down));
        tab.handle_to_tab(ptr(x0 + 94.0, cy, PointerKind::Move));
        tab.handle_to_tab(ptr(x0 + 94.0, cy, PointerKind::Up));

        let sel = tab.document_mut().expect("doc").get_selected_text();
        assert!(
            sel.as_deref().is_some_and(|s| s.starts_with("111")),
            "the drag after a scroll selects the paragraph under the cursor: {sel:?}"
        );
    }

    /// Scrolled down **and** zoomed in, a drag highlights the paragraph under
    /// the cursor: the selected text is that paragraph's own words and the
    /// painted highlight sits on its own ink - never on another element.
    #[test]
    fn a_highlight_lands_on_the_scrolled_paragraph_at_every_zoom() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("scroll-hl.html"),
            r#"<!doctype html>
<html><head><title>SH</title></head>
<body style="margin:0">
  <p id="one" style="margin:0;font-size:24px">AAA BBB CCC DDD EEE</p>
  <div style="height:1600px"></div>
  <p id="two" style="margin:0;font-size:24px;color:#0a0">111 222 333 444 555</p>
  <div style="height:1600px"></div>
</body></html>"#,
        )
        .expect("write");
        let (mut tab, sink) = selection_tab(dir.path());
        tab.navigate(
            url::Url::from_file_path(dir.path().join("scroll-hl.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );

        fn frame(tab: &mut Tab, sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> (u32, u32, Vec<u8>) {
            tab.paint_now();
            emitted(sink)
                .into_iter()
                .rev()
                .find_map(|m| match m {
                    FromTab::Frame { width, height, rgba, .. } => Some((width, height, rgba)),
                    _ => None,
                })
                .expect("frame")
        }
        fn rows(
            rgba: &[u8],
            w: usize,
            h: usize,
            on: impl Fn(&[u8]) -> bool,
        ) -> Option<(usize, usize)> {
            let mut r: Option<(usize, usize)> = None;
            for y in 0..h {
                for x in 0..w {
                    if on(&rgba[(y * w + x) * 4..]) {
                        r = Some(match r {
                            Some((y0, y1)) => (y0.min(y), y1.max(y)),
                            None => (y, y),
                        });
                        break;
                    }
                }
            }
            r
        }
        let green = |p: &[u8]| p[1] as i32 > p[0] as i32 + 20 && p[1] as i32 > p[2] as i32 + 20;
        let highlight = |p: &[u8]| {
            (p[0] as i32 - 180).abs() <= 6
                && (p[1] as i32 - 213).abs() <= 6
                && (p[2] as i32 - 255).abs() <= 6
        };

        for zoom in [1.0f32, 2.0] {
            tab.handle_to_tab(ToTab::SetZoom { zoom });
            tab.handle_to_tab(ToTab::Resize {
                width: (800.0 * zoom) as u32,
                height: 600,
                scale: 1.0,
            });
            // Wheel far down the page (logical px: the page moves
            // delta/zoom CSS px, half of that at the damped wheel), after
            // winding the page back to the top from the previous zoom.
            tab.handle_to_tab(ptr(400.0 * zoom, 300.0 * zoom, PointerKind::Move));
            tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
                x: 400.0 * zoom,
                y: 300.0 * zoom,
                delta_x: 0.0,
                delta_y: 9000.0 * zoom,
            }));
            tab.pump_wheel(true);
            tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
                x: 400.0 * zoom,
                y: 300.0 * zoom,
                delta_x: 0.0,
                delta_y: -3000.0 * zoom,
            }));
            tab.pump_wheel(true);
            let y = scroll_y(&mut tab);
            assert!(y > 1400.0, "the page is scrolled down at zoom {zoom}: {y}");

            // Paragraph two's own line, in client CSS px after the scroll.
            let cy = {
                let doc = tab.document_mut().expect("doc");
                let id = doc.get_element_by_id("two").expect("#two");
                let rect =
                    inspector::box_rect(doc, doc.get_node(id).expect("node")).expect("box");
                assert!(
                    rect.y0 >= 0.0 && rect.y1 <= (600.0 / zoom as f64),
                    "paragraph two is visible after the scroll at zoom {zoom}: {rect:?}"
                );
                ((rect.y0 + rect.y1) / 2.0) as f32
            };

            let (fw, fh, rgba) = frame(&mut tab, &sink);
            let ink = rows(&rgba, fw as usize, fh as usize, green)
                .expect("paragraph two's green ink is painted");

            tab.handle_to_tab(ptr(2.0 * zoom, cy * zoom, PointerKind::Down));
            tab.handle_to_tab(ptr(96.0 * zoom, cy * zoom, PointerKind::Move));
            tab.handle_to_tab(ptr(96.0 * zoom, cy * zoom, PointerKind::Up));

            let sel = tab.document_mut().expect("doc").get_selected_text();
            assert!(
                sel.as_deref().is_some_and(|s| s.starts_with("111")),
                "the drag after a scroll selects paragraph two's words at zoom {zoom}: {sel:?}"
            );

            let (fw, fh, rgba) = frame(&mut tab, &sink);
            let hl = rows(&rgba, fw as usize, fh as usize, highlight)
                .expect("the drag paints a highlight");
            assert!(
                hl.0 <= ink.1 + 2 && hl.1 >= ink.0 - 2,
                "the highlight overlaps paragraph two's own ink at zoom {zoom}: hl {hl:?} ink {ink:?}"
            );
        }
    }

    /// After scrolling down, everything that hit-tests viewport-relative
    /// coordinates — `element_from_point` and the Inspect element picker —
    /// names the element under the cursor, not the one `viewport_scroll`
    /// away from it.
    #[test]
    fn a_scrolled_page_inspects_the_element_under_the_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("scroll-hit.html"),
            r#"<!doctype html>
<html><head><title>SH</title></head>
<body style="margin:0">
  <p id="one" style="margin:0;font-size:24px">AAA BBB CCC DDD EEE</p>
  <div id="filler" style="height:1600px"></div>
  <p id="two" style="margin:0;font-size:24px">111 222 333 444 555</p>
  <div style="height:1600px"></div>
</body></html>"#,
        )
        .expect("write");
        let (mut tab, _sink) = selection_tab(dir.path());
        tab.navigate(
            url::Url::from_file_path(dir.path().join("scroll-hit.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );
        tab.handle_to_tab(ptr(400.0, 300.0, PointerKind::Move));
        tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
            x: 400.0,
            y: 300.0,
            delta_x: 0.0,
            delta_y: -2600.0,
        }));
        tab.pump_wheel(true);
        assert!(scroll_y(&mut tab) > 1000.0, "the page is scrolled down");

        let doctype = tab.inspect_doctype.clone();
        let (two, row, cy) = {
            let doc = tab.document_mut().expect("doc");
            let two = doc.get_element_by_id("two").expect("#two");
            let rect = inspector::box_rect(doc, doc.get_node(two).expect("node")).expect("box");
            (
                two.as_u64(),
                inspector::id_of_node(doc, two, doctype.as_deref()),
                (rect.y0 + 2.0) as f32,
            )
        };
        assert!(row.is_some(), "#two has an inspect row");
        assert_eq!(
            tab.document_mut().expect("doc").element_from_point(10.0, cy).map(|id| id.as_u64()),
            Some(two),
            "a client-space point after a scroll names the paragraph under it"
        );

        // The user-visible path: the Inspect element picker reports the same node.
        tab.handle_to_tab(ToTab::SetInspectPick { enabled: true });
        tab.handle_to_tab(ptr(10.0, cy, PointerKind::Move));
        assert_eq!(
            tab.inspect_hover,
            row,
            "inspect hovers the paragraph under the cursor, not one screenful above it"
        );
    }

    /// A user who reaches the bottom of the document can keep scrolling past
    /// it by half a viewport — the last lines come to rest clear of the
    /// window edge, where they can be selected and inspected. A page that
    /// fits the window has no bottom to overshoot and never scrolls.
    #[test]
    fn the_user_can_scroll_half_a_viewport_past_the_bottom_of_the_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("overscroll.html"),
            r#"<!doctype html>
<html><head><title>OS</title></head>
<body style="margin:0">
  <p style="margin:0;font-size:24px">AAA BBB CCC DDD EEE</p>
  <div style="height:1600px"></div>
  <p id="tail" style="margin:0;font-size:24px">LAST LINE</p>
  <div style="height:1600px"></div>
</body></html>"#,
        )
        .expect("write");
        std::fs::write(
            dir.path().join("short.html"),
            "<!doctype html><html><body><p>fits</p></body></html>",
        )
        .expect("write");
        let (mut tab, _sink) = selection_tab(dir.path());
        let wheel = |y: f32| {
            ToTab::Scroll(ScrollEvent { x: 400.0, y: 300.0, delta_x: 0.0, delta_y: y })
        };
        tab.handle_to_tab(ptr(400.0, 300.0, PointerKind::Move));

        tab.navigate(
            url::Url::from_file_path(dir.path().join("overscroll.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );
        let content_h = {
            let doc = tab.document_mut().expect("doc");
            let root = doc.try_root_element().expect("root element");
            let layout = root.final_layout();
            f64::from(layout.size.height).max(f64::from(layout.scrollable_overflow_rect.bottom))
        };
        let base = (content_h - 600.0).max(0.0);
        assert!(base > 0.0, "the document is taller than the window");

        // One huge wheel reaches the overscroll limit: half a viewport past
        // the bottom, no further.
        tab.handle_to_tab(wheel(-100000.0));
        tab.pump_wheel(true);
        let y = scroll_y(&mut tab);
        assert!(
            (y - (base + 300.0)).abs() < 1.0,
            "the wheel stops half a window past the content bottom: {y} vs {}",
            base + 300.0
        );
        // ... and the document's last line ends up mid-window: visible with a
        // screen half its height below it, ready to be selected or inspected.
        assert!(
            (content_h - y - 300.0).abs() < 1.0,
            "the bottom of the page sits at the window's middle, not its edge: {y}"
        );
        tab.handle_to_tab(wheel(-1000.0));
        tab.pump_wheel(true);
        assert_eq!(scroll_y(&mut tab), y, "there is nothing past the overscroll limit");

        // A page that fits the window never scrolls at all.
        tab.navigate(
            url::Url::from_file_path(dir.path().join("short.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );
        tab.handle_to_tab(ptr(400.0, 300.0, PointerKind::Move));
        tab.handle_to_tab(wheel(-5000.0));
        tab.pump_wheel(true);
        assert_eq!(scroll_y(&mut tab), 0.0, "a page that fits the window does not scroll");
    }

    /// A mouse drag selects page text; the selection survives to the context
    /// menu, and Ctrl+C ships exactly the reported selection to the shell —
    /// so the menu's Copy row and the keyboard chord can never disagree.
    #[test]
    fn a_drag_selection_survives_to_the_menu_and_ctrl_c_ships_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = selection_tab(dir.path());

        // Press before the first word and drag across two of them.
        tab.handle_to_tab(ptr(2.0, 12.0, PointerKind::Move));
        tab.handle_to_tab(ptr(2.0, 12.0, PointerKind::Down));
        tab.handle_to_tab(ptr(60.0, 12.0, PointerKind::Move));
        tab.handle_to_tab(ptr(120.0, 12.0, PointerKind::Move));
        tab.handle_to_tab(ptr(120.0, 12.0, PointerKind::Up));

        let selected = tab
            .document_mut()
            .and_then(|doc| doc.get_selected_text())
            .expect("the drag selected page text");
        assert!(
            SELECT_PAGE.contains(selected.as_str()) && !selected.trim().is_empty(),
            "the selection is real text of the page: {selected:?}"
        );

        // A right-click over it reports the selection to the shell's menu
        // (Down+Up, exactly the sequence the shell forwards).
        for kind in [PointerKind::Down, PointerKind::Up] {
            tab.handle_to_tab(ToTab::Pointer(PointerEvent {
                x: 100.0,
                y: 12.0,
                kind,
                button: ironsurf_common::MouseButton::Right,
                mods: Modifiers::NONE,
            }));
        }
        let target = emitted(&sink)
            .into_iter()
            .rev()
            .find_map(|m| match m {
                FromTab::ContextMenu { target, .. } => Some(target),
                _ => None,
            })
            .expect("the right-click reports its target");
        assert_eq!(
            target.selection.as_deref(),
            Some(selected.as_str()),
            "the menu Copy row sees the very selection Ctrl+C copies"
        );

        // Ctrl+C: the page copy surfaces as FromTab::ClipboardText, verbatim.
        tab.handle_to_tab(ctrl_c());
        tab.drain_copied();
        assert_eq!(
            clipboard_texts(&sink).last().map(String::as_str),
            Some(selected.as_str()),
            "Ctrl+C copies the live page selection to the shell"
        );
    }

    /// Inside a focused editor the same chords are the engine's: Ctrl+C
    /// copies the editor selection to the session clipboard, and the next
    /// Ctrl+V (with `ToTab::Clipboard` pushed first, as the shell does)
    /// replaces the selection with exactly what was copied.
    #[test]
    fn an_editor_round_trips_copy_and_paste_through_the_session_clipboard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("e.html");
        std::fs::write(
            &page,
            r#"<!doctype html><html><body style="margin:0">
  <input id="i" style="display:block;width:300px;height:40px">
</body></html>"#,
        )
        .expect("write page");
        let (mut tab, sink) = capturing_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
        tab.navigate(
            url::Url::from_file_path(&page).expect("file url").as_str(),
            NavHistory::Fresh,
        );

        // Focus the input and type a word pair.
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Move));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Down));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Up));
        for c in "hello world".chars() {
            tab.handle_to_tab(key(c));
        }

        // Home, Shift+Right x5 → "hello" selected; Ctrl+C copies exactly it.
        tab.handle_to_tab(named_key("Home", "Home", false));
        for _ in 0..5 {
            tab.handle_to_tab(named_key("ArrowRight", "ArrowRight", true));
        }
        tab.handle_to_tab(ctrl_c());
        tab.drain_copied();
        assert_eq!(
            clipboard_texts(&sink).last().map(String::as_str),
            Some("hello"),
            "Ctrl+C copies the editor selection"
        );

        // The shell's Ctrl+V protocol: clipboard first, chord second.
        // Ctrl+A selects all, Ctrl+V replaces the whole value.
        tab.handle_to_tab(ToTab::Clipboard { text: "hello".into() });
        tab.handle_to_tab(ctrl_a());
        tab.handle_to_tab(ctrl_v());
        assert_eq!(
            tab.document_mut().and_then(focused_input_value).as_deref(),
            Some("hello"),
            "Ctrl+V replaced the selection with the session clipboard"
        );

        // A later chord with different clipboard text replaces the whole
        // value again — the push-then-chord protocol is idempotent.
        tab.handle_to_tab(named_key("End", "End", false));
        tab.handle_to_tab(ToTab::Clipboard { text: "PASTED".into() });
        tab.handle_to_tab(ctrl_a());
        tab.handle_to_tab(ctrl_v());
        assert_eq!(
            tab.document_mut().and_then(focused_input_value).as_deref(),
            Some("PASTED"),
            "the second paste replaced the whole value again"
        );
    }

    /// Ctrl+A with no editor focused selects every word of laid-out page
    /// text — spans and paragraphs included, `display: none` text excluded —
    /// and the result is an ordinary selection: Ctrl+C ships it verbatim.
    #[test]
    fn ctrl_a_selects_all_page_text_and_ctrl_c_ships_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("all.html");
        std::fs::write(
            &page,
            r#"<!doctype html>
<html><head><title>A</title></head>
<body style="margin:0">
  <p style="margin:0;font-size:20px">Alpha <span>beta</span> gamma</p>
  <p style="margin:0;font-size:20px">Delta paragraph two</p>
  <p style="margin:0;display:none">Invisible zebra</p>
</body></html>"#,
        )
        .expect("write page");
        let (mut tab, sink) = capturing_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(
            url::Url::from_file_path(&page).expect("file url").as_str(),
            NavHistory::Fresh,
        );

        // Nothing focused, nothing dragged: the chord itself makes the selection.
        tab.handle_to_tab(ctrl_a());
        let selected = tab
            .document_mut()
            .and_then(|doc| doc.get_selected_text())
            .expect("Ctrl+A selects the page text");
        assert!(selected.contains("Alpha beta gamma"), "first paragraph: {selected:?}");
        assert!(selected.contains("Delta paragraph two"), "second paragraph: {selected:?}");
        assert!(!selected.contains("Invisible"), "display:none stays unselected: {selected:?}");
        assert!(!selected.contains("<head>"), "no markup leaks: {selected:?}");

        // And it is the one selection the copy pipeline already knows.
        tab.handle_to_tab(ctrl_c());
        tab.drain_copied();
        assert_eq!(
            clipboard_texts(&sink).last().map(String::as_str),
            Some(selected.as_str()),
            "Ctrl+C copies the Ctrl+A selection verbatim"
        );
    }

    /// Ctrl+A selects nothing on a page with no laid-out text (the old
    /// selection survives), and Ctrl+Shift+A — a different chord — never
    /// selects the page.
    #[test]
    fn ctrl_a_leaves_the_selection_alone_when_it_can_or_must_not_select() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, _sink) = selection_tab(dir.path());

        // The action chord + Shift: not the select-all chord.
        tab.handle_to_tab(ToTab::Key(KeyEvent {
            key: "A".into(),
            code: "KeyA".into(),
            text: Some("A".into()),
            repeat: false,
            mods: action_mod() | Modifiers::NONE.with_shift(),
        }));
        assert!(
            tab.document_mut()
                .and_then(|doc| doc.get_selected_text())
                .is_none(),
            "Ctrl+Shift+A must not select the page"
        );

        // An empty document has nothing to select; the chord is inert.
        let empty = dir.path().join("empty.html");
        std::fs::write(&empty, "<!doctype html><html><head><title>E</title></head><body></body></html>")
            .expect("write page");
        tab.navigate(
            url::Url::from_file_path(&empty).expect("file url").as_str(),
            NavHistory::Fresh,
        );
        tab.handle_to_tab(ctrl_a());
        assert!(
            tab.document_mut()
                .and_then(|doc| doc.get_selected_text())
                .is_none(),
            "no laid-out text, no selection"
        );
    }

    // ------------------------------------------------------------------
    // `<form>` end to end (task: `<form>`)
    // ------------------------------------------------------------------

    const SUBMIT_PAGE: &str = r#"<!doctype html>
<html><head><title>F</title></head>
<body style="margin:0">
  <form action="out.html" method="get">
    <input name="q" style="display:block;width:200px;height:40px">
    <select name="where">
      <option value="a" selected>A</option>
      <option value="b">B</option>
    </select>
    <button type="submit" style="display:block">Go</button>
  </form>
</body></html>"#;

    fn key_named(key: &str, code: &str) -> ToTab {
        ToTab::Key(KeyEvent {
            key: key.to_owned(),
            code: code.to_owned(),
            text: None,
            repeat: false,
            mods: Modifiers::NONE,
        })
    }

    fn submit_tab(dir: &std::path::Path, page_html: &str) -> Tab {
        std::fs::write(dir.join("index.html"), page_html).expect("write index");
        std::fs::write(
            dir.join("out.html"),
            "<!doctype html><html><body><p>out</p></body></html>",
        )
        .expect("write out");
        let args = TabArgs {
            tab_id: TabId(1),
            container_id: ContainerId(1),
            profile_dir: dir.to_path_buf(),
            url: "about:blank".into(),
            offline_fixtures: None,
            net_policy: NetPolicy::Passthrough,
            no_sandbox: true,
            media_enabled: true,
            user_agent: None,
            proxy: None,
            archive_mode: false,
            private: false,
        };
        let pipe = Arc::new(SelfPipe::new().expect("pipe"));
        let mut tab = Tab::new_headless(&args, pipe);
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(
            url::Url::from_file_path(dir.join("index.html"))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );
        tab
    }

    /// Enter in the form's only text field submits it — with the *typed*
    /// value and the selected `<option>` (which blitz's own entry list
    /// forgets) — and the GET navigation against a `file:` action works
    /// (which blitz drops entirely).
    #[test]
    fn implicit_enter_get_submits_selects_and_navigates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = submit_tab(dir.path(), SUBMIT_PAGE);

        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Move));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Down));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Up));
        tab.handle_to_tab(key('h'));
        tab.handle_to_tab(key('i'));
        tab.handle_to_tab(key_named("Enter", "Enter"));
        assert!(tab.drain_nav_hook(), "the Enter press submits the form");

        let url = tab
            .document_mut()
            .map(|doc| doc.url().as_str().to_owned())
            .unwrap_or_default();
        let base = url::Url::from_file_path(dir.path().join("out.html")).expect("file url");
        assert_eq!(
            url,
            format!("{}?q=hi&where=a", base.as_str()),
            "the Enter-submitted GET carries the typed value and the selected option"
        );
        let body = tab
            .document_mut()
            .and_then(|doc| text_of(doc, "p"))
            .unwrap_or_default();
        assert_eq!(body, "out", "the submitted document was loaded");
    }

    /// A POST to a local scheme has nowhere to put its body: honest status,
    /// no navigation, the page stays put.
    #[test]
    fn post_to_a_local_scheme_is_refused_honestly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = SUBMIT_PAGE.replace("method=\"get\"", "method=\"post\"");
        let mut tab = submit_tab(dir.path(), &page);

        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Move));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Down));
        tab.handle_to_tab(ptr(100.0, 20.0, PointerKind::Up));
        tab.handle_to_tab(key('h'));
        tab.handle_to_tab(key_named("Enter", "Enter"));
        assert!(tab.drain_nav_hook(), "the submission is drained");

        let url = tab
            .document_mut()
            .map(|doc| doc.url().as_str().to_owned())
            .unwrap_or_default();
        assert!(
            url.ends_with("index.html"),
            "no navigation happened for a POST to file:: {url}"
        );
    }

    /// Wheel direction (regression: the wheel scrolled the wrong way).
    ///
    /// (Here "wheel down" = the document scrolls down = `scrollTop` grows, so
    /// the content slides up the screen.)
    ///
    /// blitz beta.2 reads the winit/AccessKit convention — a NEGATIVE `delta_y`
    /// moves the document down (`scrollTop` grows), a positive one moves it up —
    /// and winit (hence Slint's `scroll-event`) produces deltas in that very
    /// convention, so the shell forwards them unchanged. Pinned here for both
    /// delta flavours (pixel deltas, and `|d| <= 3` notches = 10 CSS px each
    /// at the damped wheel), plus the clamp at the top of the document. The
    /// glide is landed with `pump_wheel(true)` — where it stops and what it
    /// travels is [`the_wheel_glides_instead_of_jumping`]'s business.
    #[test]
    fn wheel_down_scrolls_the_document_down_and_wheel_up_back_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("tall.html");
        std::fs::write(
            &page,
            "<!doctype html><meta charset=utf-8><style>body{margin:0;height:4000px}</style><p>tall</p>",
        )
        .expect("write");
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(
            url::Url::from_file_path(&page).expect("url").as_str(),
            NavHistory::Fresh,
        );
        tab.settle();
        // Wheel scrolling follows the hovered scroll chain.
        tab.handle_to_tab(ptr(100.0, 100.0, PointerKind::Move));
        let wheel = |d: f32| {
            ToTab::Scroll(ScrollEvent {
                x: 100.0,
                y: 100.0,
                delta_x: 0.0,
                delta_y: d,
            })
        };
        let scroll_y = |tab: &mut Tab| tab.document_mut().expect("doc").viewport_scroll().y;

        assert_eq!(scroll_y(&mut tab), 0.0, "documents start at the top");

        // Wheel down (negative pixel delta) → content moves up, scrollTop
        // grows — by half the delta at the damped wheel.
        tab.handle_to_tab(wheel(-200.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 100.0, "wheel down scrolls down");

        // Wheel up (positive pixel delta) → back towards the top.
        tab.handle_to_tab(wheel(50.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 75.0, "wheel up scrolls up");

        // A `|d| <= 3` delta is a wheel notch: 10 CSS px (half blitz's
        // 20 px line unit), same direction.
        tab.handle_to_tab(wheel(-1.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 85.0, "one notch down = 10 px down");

        // Clamped at the top (a positive notch at scroll 0 does not go negative).
        tab.handle_to_tab(wheel(1000.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 0.0, "scrolling up clamps at the top");
    }

    /// Settings menu (task): the scroll-speed slider travels every tab's
    /// wheel — `ToTab::SetScrollSpeed { speed }` multiplies the damped
    /// travel (`2.0×` doubles it, `0.5×` halves it), and nonsense values
    /// (`NaN`, zero, negative) fall back to the default damping.
    #[test]
    fn settings_scroll_speed_scales_the_wheel_travel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("tall.html");
        std::fs::write(
            &page,
            "<!doctype html><meta charset=utf-8><style>body{margin:0;height:4000px}</style><p>tall</p>",
        )
        .expect("write");
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(
            url::Url::from_file_path(&page).expect("url").as_str(),
            NavHistory::Fresh,
        );
        tab.settle();
        tab.handle_to_tab(ptr(100.0, 100.0, PointerKind::Move));
        let wheel = |d: f32| {
            ToTab::Scroll(ScrollEvent {
                x: 100.0,
                y: 100.0,
                delta_x: 0.0,
                delta_y: d,
            })
        };
        let scroll_y = |tab: &mut Tab| tab.document_mut().expect("doc").viewport_scroll().y;
        let at_top = |tab: &mut Tab| {
            tab.handle_to_tab(wheel(10000.0));
            tab.pump_wheel(true);
            tab.settle();
        };

        // Default damping: a −200 px wheel travels 100 px (half the delta).
        tab.handle_to_tab(wheel(-200.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 100.0, "default travel is unchanged");

        // The slider at 2×: the same turn travels twice as far.
        tab.handle_to_tab(ToTab::SetScrollSpeed { speed: 2.0 });
        at_top(&mut tab);
        tab.handle_to_tab(wheel(-200.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 200.0, "2× doubles the wheel travel");

        // And at 50%: half as far.
        tab.handle_to_tab(ToTab::SetScrollSpeed { speed: 0.5 });
        at_top(&mut tab);
        tab.handle_to_tab(wheel(-200.0));
        tab.pump_wheel(true);
        tab.settle();
        assert_eq!(scroll_y(&mut tab), 50.0, "0.5× halves the wheel travel");

        // Nonsense is not a speed: the default damping stands.
        for junk in [f32::NAN, 0.0, -1.0] {
            tab.handle_to_tab(ToTab::SetScrollSpeed { speed: junk });
            at_top(&mut tab);
            tab.handle_to_tab(wheel(-200.0));
            tab.pump_wheel(true);
            tab.settle();
            assert_eq!(scroll_y(&mut tab), 100.0, "{junk} falls back to default");
        }
    }

    /// Slower *and* smoother (task): one wheel event starts the page moving
    /// with the event itself, but lands only part of its damped travel; the
    /// rest follows over the next few [`WHEEL_STEP`]s, easing out. The poll
    /// loop feeds those slices; this harness plays the loop's part.
    #[test]
    fn the_wheel_glides_instead_of_jumping() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = dir.path().join("glide.html");
        std::fs::write(
            &page,
            "<!doctype html><meta charset=utf-8><style>body{margin:0;height:4000px}</style><p>tall</p>",
        )
        .expect("write");
        let mut tab = form_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(
            url::Url::from_file_path(&page).expect("url").as_str(),
            NavHistory::Fresh,
        );
        tab.settle();
        tab.handle_to_tab(ptr(100.0, 100.0, PointerKind::Move));
        let scroll_y = |tab: &mut Tab| tab.document_mut().expect("doc").viewport_scroll().y;

        tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
            x: 100.0,
            y: 100.0,
            delta_x: 0.0,
            delta_y: -400.0,
        }));
        // The event itself moves the page — but not all the way: the kick
        // slice is only a fraction of the damped travel, so a wheel event
        // never teleports the document again.
        let mid = scroll_y(&mut tab);
        let total = WHEEL_SPEED * 400.0;
        assert!(mid > 0.0, "the gesture moves with the event: {mid}");
        assert!(
            f64::from(mid) < total - 1.0,
            "the first slice is part of the travel, not all of it: {mid} of {total}"
        );

        // The rest follows over the next slices — busy-pumping (cheap: only
        // the pump, no layout read per turn) until the scroller falls
        // silent — and the glide comes to rest on the damped travel, never
        // past it. How many slices lie between kick and rest is a question
        // for the machine's frame clock; that every slice is only a
        // fraction of what remains is pinned by
        // [`wheel_glide_slices_shrink_and_terminate`].
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            tab.pump_wheel(false);
        }
        let rest = scroll_y(&mut tab);
        assert!(rest >= mid, "the glide never reverses: {mid} → {rest}");
        assert!(
            (f64::from(rest) - total).abs() < 0.5,
            "the glide lands on the damped travel: {rest} vs {total}"
        );
    }

    /// The glide's slice function: shrink-only, signed, and terminating —
    /// the last CSS px is fed whole, so a glide always ends rather than
    /// asymptoting into sub-pixel slices forever — and it lands on
    /// wall-clock: a window of four τ or more just settles the rest.
    #[test]
    fn wheel_glide_slices_shrink_and_terminate() {
        let mut remaining = 200.0_f64;
        let mut fed = 0.0;
        let mut steps = 0;
        while remaining != 0.0 {
            let slice = wheel_glide_step(remaining, WHEEL_STEP);
            assert!(slice.abs() <= remaining.abs() && slice.signum() == remaining.signum());
            remaining -= slice;
            fed += slice;
            steps += 1;
            assert!(steps < 32, "the glide must terminate, sat at {remaining}");
        }
        assert!((fed - 200.0).abs() < 1e-9, "the slices sum to the travel: {fed}");
        assert!(
            steps as u64 * 16 <= 400,
            "a 60 Hz glide comes to rest within a few frames: {steps} steps"
        );
        // A starved scroller (one slice after 200 ms of paint) lands the rest.
        assert_eq!(wheel_glide_step(120.0, Duration::from_millis(200)), 120.0);
    }

    // -------------------------------------------------------------------
    // Video wiring (task: playback) — headless, local files only

    /// One `<video>` with a `src` that does not exist on disk (the fetch is
    /// spawned and fails on its own thread; the test never waits for the
    /// network — and needs none).
    const VIDEO_PAGE: &str = r#"<!doctype html>
<html><head><title>Tube</title></head>
<body>
  <video id="v" src="x.mp4" width="320" height="240"></video>
  <p>caption</p>
</body></html>"#;

    fn write_page(dir: &std::path::Path, html: &str) -> String {
        std::fs::write(dir.join("index.html"), html).expect("write page");
        format!("file://{}", dir.join("index.html").display())
    }

    /// (videos, widget-carrying elements) over the live document tree —
    /// the same traversal `attach_video_widgets` performs.
    fn count_videos_and_widgets(tab: &mut Tab) -> (usize, usize) {
        let doc = tab.document_mut().expect("document loaded");
        let mut videos = 0;
        let mut widgets = 0;
        for (_id, node) in doc.tree().iter() {
            if let Some(el) = node.element_data() {
                if el.name.local == blitz_dom::local_name!("video") {
                    videos += 1;
                }
                if el.custom_widget_data().is_some() {
                    widgets += 1;
                }
            }
        }
        (videos, widgets)
    }

    #[test]
    fn a_video_document_gets_exactly_one_widget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = form_tab(dir.path());
        let url = write_page(dir.path(), VIDEO_PAGE);
        assert!(tab.navigate(&url, NavHistory::Fresh), "local page loads");
        let (videos, widgets) = count_videos_and_widgets(&mut tab);
        assert_eq!(videos, 1, "the page has one <video>");
        assert_eq!(
            widgets, 1,
            "exactly the <video> carries a widget — nothing else"
        );
        assert_eq!(tab.media.len(), 1, "one player for one element");
        assert_eq!(
            tab.media_inflight.len(),
            1,
            "the src fetch was queued for the element"
        );
        // And the pump tolerates the pending fetch (no panic escapes).
        tab.pump_media();
    }

    #[test]
    fn a_plain_document_attaches_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = form_tab(dir.path());
        let url = write_page(
            dir.path(),
            "<!doctype html><html><body><p>no media here</p></body></html>",
        );
        assert!(tab.navigate(&url, NavHistory::Fresh));
        let (videos, widgets) = count_videos_and_widgets(&mut tab);
        assert_eq!((videos, widgets), (0, 0));
        assert!(tab.media.is_empty());
        tab.pump_media(); // feature on, no media: a walk that attaches zero
    }

    #[test]
    fn media_off_attaches_no_widget_even_for_a_video_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = TabArgs {
            tab_id: TabId(1),
            container_id: ContainerId(1),
            profile_dir: dir.path().to_path_buf(),
            url: "about:blank".into(),
            offline_fixtures: None,
            net_policy: NetPolicy::Passthrough,
            no_sandbox: true,
            media_enabled: false,
            user_agent: None,
            proxy: None,
            archive_mode: false,
            private: false,
        };
        let pipe = Arc::new(SelfPipe::new().expect("pipe"));
        let mut tab = Tab::new_headless(&args, pipe);
        let url = write_page(dir.path(), VIDEO_PAGE);
        assert!(tab.navigate(&url, NavHistory::Fresh));
        let (videos, widgets) = count_videos_and_widgets(&mut tab);
        assert_eq!(videos, 1, "the page still has its <video>");
        assert_eq!(widgets, 0, "--no-media: no widget, no fetch, no player");
        assert!(tab.media.is_empty());
        assert!(tab.media_inflight.is_empty());
    }

    /// The stock fixture decodes for real: demux reports H.264 (the flower
    /// clip is 960×540 — the asserted size is the demuxer's own reading, and
    /// the natural size agrees), and the player presents frames from the
    /// software decoder. Skips (not fails) when the fixture is not on disk.
    #[test]
    fn flower_fixture_demuxes_as_h264_and_decodes() {
        let path = std::path::Path::new("/tmp/vid/flower.mp4");
        if !path.exists() {
            eprintln!("skipping: /tmp/vid/flower.mp4 is not present");
            return;
        }
        let bytes = Arc::new(std::fs::read(path).expect("fixture readable"));
        let track = crate::media::player::open_bytes(Arc::clone(&bytes)).expect("demuxes");
        let info = track.info();
        assert!(
            matches!(info.codec, crate::media::codec::VideoCodec::H264),
            "codec {:?}",
            info.codec
        );
        assert_eq!((info.width, info.height), (960, 540), "{}", info.describe());

        let mut player = MediaPlayer::new();
        player.load(track);
        player.play();
        let mut presented = 0usize;
        for _ in 0..80 {
            if player.pump() {
                presented += 1;
            }
            if presented >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(presented >= 1, "the software decoder presented no frames");
        assert_eq!(player.natural_size(), Some((960, 540)));
        assert!(!player.is_paused());
    }

    // -------------------------------------------------------------------
    // DevTools Inspector — the tab half (task: Inspector — tab side)
    //
    // `src/inspector.rs` holds the pure half (the walk, the ids, the
    // fingerprint, the marker). These drive a real `Tab` with the messages a real
    // shell sends, in the order it sends them, and read the `FromTab` frames back
    // off the tab's own output stream — so the contract is tested at its seam,
    // not against a paraphrase of it.
    // -------------------------------------------------------------------

    use crate::inspector;
    use ironsurf_common::{InspectKind, InspectSnapshot, MAX_INSPECT_NODES};

    /// A page laid out for the inspector: a link and a nested box in the first
    /// screenful (so a pick is unambiguous and a swallowed click is unmistakable
    /// — the page itself records whether it felt a pointer), a tall gap and a row
    /// below the fold for the "jump only when hidden" rule, and a script that
    /// appends a row a quarter-second in, for the delta stream.
    const INSPECT_PAGE: &str = r##"<!doctype html>
<html lang="en"><head><title>I</title><style>body{margin:0;font:16px monospace}</style></head>
<body>
  <!-- a note between tags -->
  <a id="l" href="#next" style="display:block;width:200px;height:40px">link</a>
  <span id="o">untouched</span>
  <div id="deep" style="display:block;padding:8px"><span id="span">x</span></div>
  <div id="tall" style="height:1200px"></div>
  <p id="far">far below</p>
  <script>
    document.getElementById('l').addEventListener('click', function () {
      document.getElementById('o').textContent = 'seen';
    });
    setTimeout(function () {
      var s = document.createElement('section');
      s.id = 'born-late';
      document.body.appendChild(s);
    }, 250);
  </script>
</body></html>"##;

    /// A capturing tab sized 800×600 with `INSPECT_PAGE` loaded — the state a
    /// shell is in when its user hits F12.
    fn inspect_tab(dir: &std::path::Path) -> (Tab, Arc<std::sync::Mutex<Vec<u8>>>) {
        let (mut tab, sink) = capturing_tab(dir);
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        let url = write_page(dir, INSPECT_PAGE);
        tab.navigate(&url, NavHistory::Fresh);
        (tab, sink)
    }

    /// Every [`FromTab::InspectSnapshot`] a tab has shipped, oldest first.
    fn snapshots_of(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<InspectSnapshot> {
        emitted(sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::InspectSnapshot { snapshot } => Some(snapshot),
                _ => None,
            })
            .collect()
    }

    /// The newest tree the pane was sent.
    fn latest_snapshot(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> InspectSnapshot {
        snapshots_of(sink)
            .pop()
            .expect("the pane was sent a tree")
    }

    /// The `FromTab::InspectPicked` nodes, in order.
    fn picks_of(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u64> {
        emitted(sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::InspectPicked { node } => Some(node),
                _ => None,
            })
            .collect()
    }

    /// The `FromTab::InspectHover` nodes, in order (`None` = the cursor stopped
    /// answering, which is what disarming reports).
    fn hovers_of(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<Option<u64>> {
        emitted(sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::InspectHover { node } => Some(node),
                _ => None,
            })
            .collect()
    }

    /// The row of the element carrying `id="…"` — the way a test names an element
    /// without knowing how many rows the walk produced before it.
    fn row_of<'a>(snapshot: &'a InspectSnapshot, id_attr: &str) -> Option<&'a ironsurf_common::InspectNode> {
        snapshot.nodes.iter().find(|node| {
            node.kind == InspectKind::Element
                && node
                    .attrs
                    .iter()
                    .any(|(name, value)| name == "id" && value == id_attr)
        })
    }

    /// The centre of the element's live box, in [`ToTab::Pointer`] space (window
    /// CSS px): measured, not guessed, so the test survives a font change.
    fn point_of(tab: &mut Tab, id_attr: &str) -> (f32, f32) {
        let doc = tab.document_mut().expect("document loaded");
        let id = doc
            .get_element_by_id(id_attr)
            .unwrap_or_else(|| panic!("#{id_attr} is in the page"));
        let rect = inspector::box_rect(doc, doc.get_node(id).expect("node"))
            .unwrap_or_else(|| panic!("#{id_attr} has a box"));
        (
            ((rect.x0 + rect.x1) / 2.0) as f32,
            ((rect.y0 + rect.y1) / 2.0) as f32,
        )
    }

    /// Root scroller position (the jump tests read the page, not the tab).
    fn scroll_y(tab: &mut Tab) -> f64 {
        tab.document_mut().expect("doc").viewport_scroll().y
    }

    #[test]
    fn opening_the_inspector_ships_the_tree_and_reopening_ships_nothing() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (mut tab, sink) = inspect_tab(dir.path());

        // A closed pane costs the tab nothing at all: the document exists and no
        // tree was ever shipped.
        assert!(
            snapshots_of(&sink).is_empty(),
            "a closed inspector streams nothing"
        );

        tab.handle_to_tab(ToTab::SetInspect { open: true });
        let shots = snapshots_of(&sink);
        assert_eq!(shots.len(), 1, "opening the pane ships exactly one tree");
        let shot = &shots[0];
        assert_eq!(shot.generation, 1, "the first document is generation 1");
        assert_eq!(shot.url, write_page_url(dir.path()).as_str(), "the page the pane shows");
        assert_eq!(shot.selected, None, "nothing is selected yet");
        assert!(!shot.truncated, "a small page is not truncated");

        // One root row for the document (`depth 0` is the shell's indent unit),
        // and the doctype is a row of the tree even though blitz's parser throws
        // the text away — `<html>` hangs off the document beside it, not off it.
        assert_eq!(shot.nodes[0].kind, InspectKind::Document);
        assert_eq!(shot.nodes[0].parent, None);
        assert_eq!(shot.nodes[0].depth, 0);
        assert_eq!(shot.nodes[1].kind, InspectKind::Doctype);
        assert_eq!(shot.nodes[1].text, "html");
        assert_eq!(shot.nodes[1].parent, Some(0));
        assert_eq!(shot.nodes[2].kind, InspectKind::Element);
        assert_eq!(shot.nodes[2].tag, "html");
        assert_eq!(shot.nodes[2].parent, Some(0), "<html> is a child of the document row");

        // Row ids are dense positions, and anonymous layout boxes never get one:
        // every row is something the serializer can show.
        for (i, node) in shot.nodes.iter().enumerate() {
            assert_eq!(node.id, i as u64, "row ids are positions");
            assert!(
                matches!(
                    node.kind,
                    InspectKind::Document
                        | InspectKind::Doctype
                        | InspectKind::Element
                        | InspectKind::Text
                        | InspectKind::Comment
                ),
                "every row is a real DOM node: {node:?}"
            );
            if node.kind != InspectKind::Element {
                assert_eq!(node.tag, "", "only elements carry a tag: {node:?}");
            }
        }
        // Text and comments are rows (the shell folds them), and `<head>` is not
        // skipped: the pane shows a document, not just a body.
        assert!(
            shot.nodes.iter().any(|n| n.kind == InspectKind::Comment),
            "the HTML comment between tags is a row"
        );
        assert!(
            shot.nodes
                .iter()
                .any(|n| n.kind == InspectKind::Text && n.text == "link"),
            "the link's text node is a row of its own"
        );
        assert!(shot.nodes.iter().any(|n| n.tag == "title"), "<head> children are rows");
        assert!(
            shot.nodes.iter().any(|n| n.tag == "script"),
            "the <script> element is a row (its body is the text row under it)"
        );

        // Elements on screen carry live bounds; `#l` sits two levels under the
        // root and its box is where the layout put it.
        let link = row_of(shot, "l").expect("#l is a row");
        assert_eq!(link.tag, "a");
        assert_eq!(link.depth, 3, "document > html > body > a");
        let bounds = link.bounds.expect("#l is laid out");
        assert!(bounds.x < 1.0 && bounds.y < 1.0, "the link starts at the origin: {bounds:?}");
        assert!((bounds.width - 200.0).abs() < 0.6, "width 200 CSS px: {bounds:?}");

        // Reopening an open pane is not a re-request: no second tree.
        tab.handle_to_tab(ToTab::SetInspect { open: true });
        assert_eq!(
            snapshots_of(&sink).len(),
            1,
            "open then open again ships one tree, not two"
        );
    }

    /// `write_page` returns the URL string; tests need it as a URL to compare
    /// against the snapshot's own `url` field.
    fn write_page_url(dir: &std::path::Path) -> url::Url {
        url::Url::from_file_path(dir.join("index.html")).expect("file url")
    }

    #[test]
    fn a_new_document_renumbers_the_tree_and_drops_the_selection() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (mut tab, sink) = inspect_tab(dir.path());
        tab.handle_to_tab(ToTab::SetInspect { open: true });
        let first = latest_snapshot(&sink);
        let far = row_of(&first, "far").expect("#far is a row").id;
        tab.handle_to_tab(ToTab::InspectSelect { node: Some(far) });
        assert_eq!(tab.inspect_selected, Some(far));

        // A second document with the same shape as the first: blitz recycles its
        // node arena, so the row ids mean something else entirely now. The tab
        // re-mints them from 0 under a new generation and drops the selection —
        // which is the whole reason the shell must treat an unknown id as stale.
        let second = dir.path().join("second.html");
        std::fs::write(
            &second,
            INSPECT_PAGE.replace("<title>I</title>", "<title>II</title>")
                .replace(r##"<p id="far">far below</p>"##, r##"<p id="far">still far</p>"##),
        )
        .expect("write second page");
        tab.navigate(
            url::Url::from_file_path(&second).expect("file url").as_str(),
            NavHistory::Fresh,
        );
        let latest = latest_snapshot(&sink);
        assert!(
            latest.generation > first.generation,
            "a navigation bumps the generation ({} → {})",
            first.generation,
            latest.generation
        );
        assert_eq!(latest.url, url::Url::from_file_path(&second).expect("file url").as_str());
        assert_eq!(
            latest.nodes[0].kind,
            InspectKind::Document,
            "the document row stays row 0 in a new generation"
        );
        assert_eq!(latest.nodes[1].kind, InspectKind::Doctype, "and the doctype row 1");
        assert_eq!(latest.selected, None, "a fresh document has no selection");
        assert_eq!(tab.inspect_selected, None);

        // An id from the dead generation resolves to nothing: no marker, and the
        // page does not move (contract: ignored, never an error).
        let before = scroll_y(&mut tab);
        tab.handle_to_tab(ToTab::InspectSelect {
            node: Some(first.nodes.len() as u64 + 7),
        });
        assert!(
            tab.inspect_highlights(1.0).is_empty(),
            "a stale id paints nothing"
        );
        assert_eq!(scroll_y(&mut tab), before, "and scrolls nothing");
    }

    #[test]
    fn inspect_element_picks_the_deepest_element_and_answers_with_it() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (mut tab, sink) = inspect_tab(dir.path());
        tab.handle_to_tab(ToTab::SetInspect { open: true });
        let shot = latest_snapshot(&sink);
        let span = row_of(&shot, "span").expect("#span is a row");
        let deep = row_of(&shot, "deep").expect("#deep is a row");
        assert_eq!(span.parent, Some(deep.id), "#span hangs off #deep in the tree");

        // Pick the middle of the <span> nested in the padded <div>: the deepest
        // ELEMENT answers — not its ancestor, and not the text node inside it.
        let (x, y) = point_of(&mut tab, "span");
        tab.handle_to_tab(ToTab::InspectPickAt { x, y });
        assert_eq!(
            picks_of(&sink).last().copied(),
            Some(span.id),
            "the deepest element at the point is the pick"
        );
        assert_ne!(span.id, deep.id);

        // …answered with the same generation, carrying the new selection.
        let after = latest_snapshot(&sink);
        assert_eq!(after.generation, shot.generation, "a pick is not a new document");
        assert_eq!(after.selected, Some(span.id));
        assert_eq!(tab.inspect_selected, Some(span.id));

        // And the marker is on the page: one content-box fill plus the outline
        // band, measured where the element actually is.
        let marks = tab.inspect_highlights(1.0);
        assert_eq!(marks.len(), 5, "content fill + four outline edges");
        assert_eq!(marks[0].1, inspector::COLOR_FILL, "the fill is the content box");
        assert!(marks[0].0.contains(kurbo::Point::new(f64::from(x), f64::from(y))));
        assert!(
            marks[1..].iter().all(|(_, color)| *color == inspector::COLOR_OUTLINE),
            "the band around the border box"
        );

        // The same hit-test the status bar and `document.elementFromPoint()` use:
        // a point over the link picks the link, wherever its text lands.
        let (x, y) = point_of(&mut tab, "l");
        tab.handle_to_tab(ToTab::InspectPickAt { x, y });
        let picks = picks_of(&sink);
        assert_eq!(picks.len(), 2);
        let shot = latest_snapshot(&sink);
        assert_eq!(row_of(&shot, "l").expect("#l").id, picks[1]);

        // A point off the page answers nothing — and says nothing either.
        tab.handle_to_tab(ToTab::InspectPickAt {
            x: 100_000.0,
            y: 100_000.0,
        });
        assert_eq!(
            picks_of(&sink).len(),
            2,
            "a point outside the viewport picks nothing and emits nothing"
        );
        assert!(
            tab.inspect_highlights(1.0).len() <= 5,
            "still only the one selection marked"
        );
    }

    #[test]
    fn the_armed_picker_swallows_the_page_and_reports_the_cursor() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (mut tab, sink) = inspect_tab(dir.path());
        tab.handle_to_tab(ToTab::SetInspect { open: true });
        let shot = latest_snapshot(&sink);
        let link = row_of(&shot, "l").expect("#l is a row").id;
        let (x, y) = point_of(&mut tab, "l");

        // Arming the picker is inert until the cursor moves somewhere.
        tab.handle_to_tab(ToTab::SetInspectPick { enabled: true });
        assert!(tab.inspect_pick);
        assert!(hovers_of(&sink).is_empty(), "arming reports nothing");

        // Move: the cursor's element is reported and painted.
        tab.handle_to_tab(ptr(x, y, PointerKind::Move));
        let hovers = hovers_of(&sink);
        assert_eq!(hovers, vec![Some(link)], "the cursor names the link's row");
        assert_eq!(tab.inspect_hover, Some(link));
        assert_eq!(tab.inspect_selected, None, "hovering is not selecting");

        // Motion within one element is not news: no second report.
        tab.handle_to_tab(ptr(x + 4.0, y + 1.0, PointerKind::Move));
        assert_eq!(
            hovers_of(&sink).len(),
            1,
            "moving inside the same element stays quiet"
        );

        // Click the link with the crosshair armed: the page feels nothing. Its
        // own click handler writes into #o, and it never runs.
        tab.handle_to_tab(ptr(x, y, PointerKind::Down));
        tab.handle_to_tab(ptr(x, y, PointerKind::Up));
        let untouched = tab
            .document_mut()
            .and_then(|doc| text_of(doc, "#o"))
            .unwrap_or_default();
        assert_eq!(
            untouched, "untouched",
            "the page's click handler never ran — nothing reached blitz"
        );
        assert_eq!(
            picks_of(&sink).last().copied(),
            Some(link),
            "the release was the pick"
        );
        assert!(!tab.inspect_pick, "one pick per arming: the crosshair disarms");

        // Disarmed (the shell's Esc), the very same click goes to the engine.
        tab.handle_to_tab(ptr(x, y, PointerKind::Move));
        tab.handle_to_tab(ptr(x, y, PointerKind::Down));
        tab.handle_to_tab(ptr(x, y, PointerKind::Up));
        let seen = tab
            .document_mut()
            .and_then(|doc| text_of(doc, "#o"))
            .unwrap_or_default();
        assert_eq!(seen, "seen", "with the picker off, blitz takes the click");

        // Re-arm, hover, then disarm: the hover report is taken back, and the
        // selection the pick made survives.
        tab.handle_to_tab(ToTab::SetInspectPick { enabled: true });
        tab.handle_to_tab(ptr(x, y, PointerKind::Move));
        let mut expected = hovers_of(&sink);
        let before = expected.len();
        assert_eq!(expected.last().copied(), Some(Some(link)));
        tab.handle_to_tab(ToTab::SetInspectPick { enabled: false });
        assert_eq!(tab.inspect_hover, None);
        expected.truncate(before);
        expected.push(None);
        assert_eq!(
            hovers_of(&sink),
            expected,
            "disarming says the cursor stopped answering"
        );
        assert_eq!(tab.inspect_selected, Some(link), "the pick's selection survives");
        assert_eq!(tab.inspect_highlights(1.0).len(), 5, "and is the only marker");
    }

    #[test]
    fn selecting_a_row_scrolls_only_when_the_page_hides_it() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (mut tab, sink) = inspect_tab(dir.path());
        tab.handle_to_tab(ToTab::SetInspect { open: true });
        let shot = latest_snapshot(&sink);

        // A row already fully on screen must not jump the page (contract).
        let deep = row_of(&shot, "deep").expect("#deep is a row").id;
        tab.handle_to_tab(ToTab::InspectSelect { node: Some(deep) });
        assert_eq!(scroll_y(&mut tab), 0.0, "a visible row does not move the page");
        assert_eq!(tab.inspect_selected, Some(deep), "but it is selected");
        assert_eq!(tab.inspect_highlights(1.0).len(), 5, "and marked");

        // The row below the fold does: select it and the page brings it up.
        let far = row_of(&shot, "far").expect("#far is a row").id;
        tab.handle_to_tab(ToTab::InspectSelect { node: Some(far) });
        let y = scroll_y(&mut tab);
        assert!(y > 0.0, "a hidden row scrolls the page to it (scroll_y {y})");
        let doc = tab.document_mut().expect("doc");
        let id = doc.get_element_by_id("far").expect("#far");
        let rect = inspector::box_rect(doc, doc.get_node(id).expect("node")).expect("box");
        assert!(
            rect.y0 >= 0.0 && rect.y1 <= 600.0,
            "after the jump it is fully visible: {rect:?}"
        );

        // Clearing the selection takes the marker off and leaves the page put.
        tab.handle_to_tab(ToTab::InspectSelect { node: None });
        assert_eq!(scroll_y(&mut tab), y, "clearing does not scroll");
        assert!(tab.inspect_highlights(1.0).is_empty(), "and unmarks");

        // The marker is re-measured from the stored node every frame, so it
        // rides the scroll without the tree being walked again: wheel the page
        // and the same selection's box moves with it, landing on the element's
        // live box — and no snapshot is needed for it, or shipped.
        tab.handle_to_tab(ToTab::InspectSelect { node: Some(deep) });
        let shots = snapshots_of(&sink).len();
        let before = tab.inspect_highlights(1.0);
        tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
            x: 400.0,
            y: 300.0,
            delta_x: 0.0,
            delta_y: 120.0,
        }));
        let after = tab.inspect_highlights(1.0);
        assert_eq!(before.len(), 5);
        assert_eq!(after.len(), 5);
        assert_ne!(before[0].0.y0, after[0].0.y0, "the marker moved with the page");
        assert_eq!(
            snapshots_of(&sink).len(),
            shots,
            "scrolling is not a tree change: no snapshot for it"
        );
        assert_eq!(
            after[0].0.x0, before[0].0.x0,
            "the marker stayed on the element's own column as the page moved"
        );
        assert_eq!(after[0].0.width(), before[0].0.width(), "and kept its width");
        // (The box moved because it is measured from the node every frame: a box
        // remembered from the snapshot would still be at `before`.)
    }

    #[test]
    fn the_open_pane_hears_a_dom_mutation_and_stays_quiet_when_nothing_moved() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (mut tab, sink) = inspect_tab(dir.path());
        tab.handle_to_tab(ToTab::SetInspect { open: true });
        let before = latest_snapshot(&sink);
        assert!(
            !before.nodes.iter().any(|n| n.tag == "section"),
            "the page's script has not run yet"
        );

        // A script appends a <section> 250 ms in. The next pump runs it, and the
        // pane learns the tree moved — under the SAME generation, because a DOM
        // mutation is not a navigation.
        std::thread::sleep(std::time::Duration::from_millis(400));
        tab.headless_pump();
        let after = latest_snapshot(&sink);
        assert!(
            after.nodes.iter().any(|n| n.tag == "section"),
            "createElement + appendChild reaches the pane"
        );
        assert_eq!(
            after.generation, before.generation,
            "a mutation is not a new document"
        );
        assert!(after.nodes.len() > before.nodes.len());
        assert_eq!(after.nodes.len(), before.nodes.len() + 1, "one row was born");

        // …and an idle page then says nothing at all: the walk still happens, the
        // fingerprint keeps the wire quiet.
        let quiet = snapshots_of(&sink).len();
        for _ in 0..4 {
            tab.headless_pump();
        }
        assert_eq!(
            snapshots_of(&sink).len(),
            quiet,
            "an unchanged tree ships once, not per pump"
        );

        // Closing the pane drops the tab's half of the state and stops the stream.
        tab.handle_to_tab(ToTab::SetInspect { open: false });
        assert!(!tab.inspect_enabled);
        assert!(tab.inspect_nodes.is_empty(), "closing drops the node handles");
        assert_eq!(tab.inspect_selected, None);
        assert!(tab.inspect_fp.is_none());
        for _ in 0..3 {
            tab.headless_pump();
        }
        assert_eq!(
            snapshots_of(&sink).len(),
            quiet,
            "a closed pane is silent again"
        );
        // …and its ids are gone: even an id that existed a moment ago is inert.
        tab.handle_to_tab(ToTab::InspectSelect { node: Some(1) });
        assert!(tab.inspect_highlights(1.0).is_empty());
    }

    /// The caps in `ironsurf-common` are the tab's to honour: a page deeper or
    /// wider than the contract allows is trimmed, flagged, and never a panic or a
    /// message over the frame limit.
    #[test]
    fn an_enormous_document_is_trimmed_and_flagged_not_dropped() {
        let dir = tempfile::tempdir().expect("tmpdir");
        // One element with 40 attributes (the contract allows 24) and a value
        // twice the length limit; then a tree of more rows than MAX_INSPECT_NODES.
        let many_attrs = (0..40)
            .map(|i| format!(r#" data-a{i}="{}""#, "v".repeat(60)))
            .collect::<String>();
        let long_value = "l".repeat(3 * ironsurf_common::MAX_INSPECT_ATTR_VALUE);
        let mut html = format!(
            r##"<!doctype html><html><head><title>Big</title></head><body><div {many_attrs} data-long="{long_value}" id="wide">"##
        );
        let rows_to_build = MAX_INSPECT_NODES + 64;
        for _ in 0..rows_to_build {
            html.push_str("<p>x</p>");
        }
        html.push_str("</div></body></html>");
        let url = write_page(dir.path(), &html);

        let (mut tab, sink) = capturing_tab(dir.path());
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(&url, NavHistory::Fresh);
        tab.handle_to_tab(ToTab::SetInspect { open: true });

        let shot = latest_snapshot(&sink);
        assert!(shot.truncated, "the walk stopped at the row cap");
        assert_eq!(
            shot.nodes.len(),
            MAX_INSPECT_NODES,
            "exactly the capped row count (the cap is the contract)"
        );
        // Every shipped row stays inside its own caps, whatever the page did.
        assert!(
            shot.nodes.iter().all(|n| n.attrs.len() <= ironsurf_common::MAX_INSPECT_ATTRS),
            "attributes capped per element"
        );
        assert!(
            shot.nodes
                .iter()
                .all(|n| n.text.chars().count() <= ironsurf_common::MAX_INSPECT_TEXT),
            "content capped per row"
        );
        let wide = shot
            .nodes
            .iter()
            .find(|n| n.kind == InspectKind::Element && n.tag == "div" && !n.attrs.is_empty())
            .expect("the wide element's row");
        assert_eq!(wide.attrs.len(), ironsurf_common::MAX_INSPECT_ATTRS);
        assert!(wide
            .attrs
            .iter()
            .all(|(_, value)| value.chars().count() <= ironsurf_common::MAX_INSPECT_ATTR_VALUE));
        // The tree the shell gets is well-formed regardless: ids dense, every
        // parent older than its child (so a pane can build it in one pass).
        for (i, node) in shot.nodes.iter().enumerate() {
            assert_eq!(node.id, i as u64);
            if let Some(parent) = node.parent {
                assert!(parent < node.id, "parents precede children: {node:?}");
            }
        }
    }

    // ------------------------------------------------------------------
    // SCROLL BAR (task: overlay scroll bar over the shell's page view). The
    // document lives in this process, so the viewport's scroll geometry — the
    // offset, how far it can go, and the window it scrolls inside — ships to
    // the shell, which paints the thumb over the page. One message per change.
    // ------------------------------------------------------------------

    /// Every [`FromTab::ScrollState`] a (capturing) tab shipped, oldest first.
    fn scroll_states(sink: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<ScrollState> {
        emitted(sink)
            .into_iter()
            .filter_map(|msg| match msg {
                FromTab::ScrollState { state } => Some(state),
                _ => None,
            })
            .collect()
    }

    /// A tab on a `height: 1600px` page (two thirds more than its 600 px
    /// window) and a tab on a page that fits its window with room to spare.
    fn scroll_tab(
        dir: &std::path::Path,
        name: &str,
        html: &str,
    ) -> (Tab, Arc<std::sync::Mutex<Vec<u8>>>) {
        std::fs::write(dir.join(name), html).expect("write page");
        let (mut tab, sink) = capturing_tab(dir);
        tab.handle_to_tab(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab.navigate(
            url::Url::from_file_path(dir.join(name))
                .expect("file url")
                .as_str(),
            NavHistory::Fresh,
        );
        tab.settle();
        (tab, sink)
    }

    /// The window height the fixtures above are measured in.
    const SCROLL_WINDOW_H: f64 = 600.0;

    #[test]
    fn the_tab_ships_scroll_geometry_for_the_bar_once_per_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = scroll_tab(
            dir.path(),
            "tall.html",
            "<!doctype html><meta charset=utf-8><style>body{margin:0;height:1600px}</style><p>tall</p>",
        );
        let content_h = {
            let doc = tab.document_mut().expect("doc");
            let root = doc.try_root_element().expect("root element");
            let l = root.final_layout();
            f64::from(l.size.height).max(f64::from(l.scrollable_overflow_rect.bottom))
        };
        assert!(
            content_h > SCROLL_WINDOW_H * 1.5,
            "the fixture is taller than its window: {content_h}"
        );

        tab.report_scroll_state();
        let states = scroll_states(&sink);
        assert_eq!(states.len(), 1, "one report of the fresh document's geometry");
        let first = states[0];
        assert!(
            (f64::from(first.viewport_h) - SCROLL_WINDOW_H).abs() < 1.0,
            "the window the bar's track stands for: {:?}",
            first.viewport_h
        );
        assert_eq!(first.y, 0.0, "documents start at the top");
        assert!(first.is_scrollable(), "a tall page scrolls: {first:?}");

        // The `range` the report claims is the ceiling the engine actually
        // answers: one enormous wheel reaches exactly it — the content bottom
        // plus the half-window overscroll the engine allows — and the report
        // follows the offset while it moves.
        tab.handle_to_tab(ptr(100.0, 100.0, PointerKind::Move));
        let wheel = |d: f32| {
            ToTab::Scroll(ScrollEvent {
                x: 100.0,
                y: 100.0,
                delta_x: 0.0,
                delta_y: d,
            })
        };
        tab.handle_to_tab(wheel(-100000.0));
        tab.pump_wheel(true);
        tab.settle();
        let scrolled = scroll_y(&mut tab);
        tab.report_scroll_state();
        let states = scroll_states(&sink);
        assert_eq!(states.len(), 2, "the offset moved: one message");
        let bottom = states[1];
        assert!(
            (f64::from(bottom.y) - scrolled).abs() < 1.0,
            "the report follows the document: {bottom:?} vs scroll y {scrolled}"
        );
        assert!(
            (f64::from(bottom.range) - f64::from(bottom.y)).abs() < 1.0,
            "the wheel stopped exactly at the reported ceiling: {bottom:?}"
        );
        assert!(
            (f64::from(bottom.range) - (content_h - SCROLL_WINDOW_H + SCROLL_WINDOW_H * 0.5))
                .abs()
                < 1.0,
            "the ceiling is the content bottom + half a window of overscroll: {bottom:?}"
        );
        assert!(
            (bottom.viewport_h - first.viewport_h).abs() < 0.01,
            "the window did not move"
        );

        // Sitting still costs nothing: nothing arrives between two identical
        // reports, which is what keeps a poll tick per page cheap.
        tab.report_scroll_state();
        tab.report_scroll_state();
        assert_eq!(
            scroll_states(&sink).len(),
            2,
            "an unchanged geometry is silence"
        );

        // Back to the top is a change like any other.
        tab.handle_to_tab(wheel(100000.0));
        tab.pump_wheel(true);
        tab.settle();
        tab.report_scroll_state();
        let states = scroll_states(&sink);
        assert_eq!(states.len(), 3);
        assert_eq!(states[2].y, 0.0, "back at the top");
        assert!(
            states[2].range > 0.0,
            "the page is still scrollable: {:?}",
            states[2]
        );
    }

    /// PageUp / PageDown move the document one screen at a time: a viewport of
    /// travel less the tenth kept as overlap, clamped at the top, with the
    /// geometry the shell's bar draws from following. blitz binds neither key at
    /// page level (`node/text.rs` spells them as editor no-ops), so the gesture
    /// is the tab's own — and a chord key held while paging changes nothing,
    /// because Ctrl/⌘-PageDown is deliberately not bound as a chord anywhere.
    #[test]
    fn page_keys_scroll_one_screen_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = scroll_tab(
            dir.path(),
            "tall.html",
            "<!doctype html><meta charset=utf-8><style>body{margin:0;height:1600px}</style><p>tall</p>",
        );
        tab.report_scroll_state();
        assert_eq!(scroll_states(&sink).len(), 1, "the fresh geometry");

        let page_key = |key: &str, mods: Modifiers| {
            ToTab::Key(KeyEvent {
                key: key.into(),
                code: key.into(),
                text: None,
                repeat: false,
                mods,
            })
        };
        let screen = SCROLL_WINDOW_H * 0.9;

        // One press, one screen down — and the shell hears about it.
        tab.handle_to_tab(page_key("PageDown", Modifiers::NONE));
        tab.settle();
        let one = scroll_y(&mut tab);
        assert!(
            (one - screen).abs() < 1.0,
            "PageDown travels a viewport less its overlap ({screen}): {one}"
        );
        tab.report_scroll_state();
        let states = scroll_states(&sink);
        assert_eq!(states.len(), 2, "the bar follows the page");
        assert!(
            (f64::from(states[1].y) - one).abs() < 1.0,
            "the report is the page: {:?} vs {one}",
            states[1]
        );

        // PageUp: the same travel back, stopping at the top instead of using the
        // engine's half-window of overscroll.
        tab.handle_to_tab(page_key("PageUp", Modifiers::NONE));
        tab.settle();
        assert!(scroll_y(&mut tab).abs() < 1.0, "PageUp is back at the top");
        tab.handle_to_tab(page_key("PageUp", Modifiers::NONE));
        tab.settle();
        assert!(scroll_y(&mut tab).abs() < 1.0, "and it does not go past it");

        // Ctrl held is not a chord: it pages exactly the same way.
        tab.handle_to_tab(page_key("PageDown", Modifiers::NONE.with_ctrl()));
        tab.settle();
        let held = scroll_y(&mut tab);
        assert!(
            (held - screen).abs() < 1.0,
            "Ctrl-PageDown scrolls like the bare key: {held}"
        );

        // Walking down the page stops at the reported ceiling.
        for _ in 0..10 {
            tab.handle_to_tab(page_key("PageDown", Modifiers::NONE));
        }
        tab.settle();
        let ceiling = f64::from(scroll_states(&sink).last().expect("geometry").range);
        let bottom = scroll_y(&mut tab);
        assert!(bottom > screen, "ten presses went down the page: {bottom}");
        assert!(
            bottom <= ceiling + 1.0,
            "the last press stopped at the reported ceiling {ceiling}: {bottom}"
        );
    }

    /// `↑` / `↓` move the document a line at a time ([`ARROW_SCROLL_PX`] — 40 CSS
    /// px, twice the wheel's line unit): the fine motion to PageUp/PageDown's
    /// screen, so a reader can put one exact line in view. blitz binds the arrows
    /// for editors only, so without this the keys do nothing at page level. With a
    /// text control focused the caret answers instead and the page does not move.
    #[test]
    fn arrow_keys_scroll_a_line_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = scroll_tab(
            dir.path(),
            "tall.html",
            "<!doctype html><meta charset=utf-8><style>body{margin:0;height:1600px}</style>\
             <input style=\"width:200px;height:40px\">",
        );
        tab.report_scroll_state();
        assert!(!tab.editor_focused(), "the page has the caret to start with");

        let arrow = |key: &str| {
            ToTab::Key(KeyEvent {
                key: key.into(),
                code: key.into(),
                text: None,
                repeat: false,
                mods: Modifiers::NONE,
            })
        };

        // One press, one line …
        tab.handle_to_tab(arrow("ArrowDown"));
        tab.settle();
        assert!(
            (scroll_y(&mut tab) - ARROW_SCROLL_PX).abs() < 1.0,
            "one press is one line ({ARROW_SCROLL_PX} CSS px): {}",
            scroll_y(&mut tab)
        );
        // … and presses add up.
        for _ in 0..3 {
            tab.handle_to_tab(arrow("ArrowDown"));
        }
        tab.settle();
        let four = scroll_y(&mut tab);
        assert!(
            (four - 4.0 * ARROW_SCROLL_PX).abs() < 1.0,
            "four presses went four lines: {four}"
        );
        tab.report_scroll_state();
        let states = scroll_states(&sink);
        assert!(
            states.len() >= 2,
            "the bar followed the page down"
        );
        assert!(
            (f64::from(states[states.len() - 1].y) - four).abs() < 1.0,
            "the report is the page: {:?} vs {four}",
            states[states.len() - 1]
        );

        // Up is the same step back, and the top is as far up as a page goes.
        tab.handle_to_tab(arrow("ArrowUp"));
        tab.settle();
        assert!(
            (scroll_y(&mut tab) - 3.0 * ARROW_SCROLL_PX).abs() < 1.0,
            "one line back: {}",
            scroll_y(&mut tab)
        );
        for _ in 0..10 {
            tab.handle_to_tab(arrow("ArrowUp"));
        }
        tab.settle();
        assert!(
            scroll_y(&mut tab).abs() < 1.0,
            "the arrows stop at the top: {}",
            scroll_y(&mut tab)
        );

        // With the caret in a text control the arrow belongs to the editor: blitz
        // moves the caret and the document keeps its offset.
        tab.handle_to_tab(ptr(60.0, 20.0, PointerKind::Move));
        tab.handle_to_tab(ptr(60.0, 20.0, PointerKind::Down));
        tab.handle_to_tab(ptr(60.0, 20.0, PointerKind::Up));
        tab.settle();
        assert!(tab.editor_focused(), "the click focused the field");
        tab.handle_to_tab(arrow("ArrowDown"));
        tab.settle();
        assert!(
            scroll_y(&mut tab).abs() < 1.0,
            "the caret answered, the page did not move: {}",
            scroll_y(&mut tab)
        );
    }

    /// A page that fits its window has no range, so the shell shows no bar —
    /// and the wheel that cannot move it produces no further message either.
    #[test]
    fn a_page_that_fits_reports_no_scroll_and_stays_quiet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut tab, sink) = scroll_tab(
            dir.path(),
            "short.html",
            "<!doctype html><meta charset=utf-8><style>body{margin:0}</style><p>fits</p>",
        );
        tab.report_scroll_state();
        let states = scroll_states(&sink);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].range, 0.0, "a fitting page does not scroll");
        assert_eq!(states[0].y, 0.0);
        assert!(!states[0].is_scrollable());

        tab.handle_to_tab(ptr(100.0, 100.0, PointerKind::Move));
        tab.handle_to_tab(ToTab::Scroll(ScrollEvent {
            x: 100.0,
            y: 100.0,
            delta_x: 0.0,
            delta_y: -100.0,
        }));
        tab.settle();
        tab.report_scroll_state();
        assert_eq!(
            scroll_states(&sink).len(),
            1,
            "nothing moved, so nothing was reported"
        );
    }

    #[test]
    fn the_mac_delete_key_names_its_appkit_command() {
        // The engine skips `Key::Backspace` on macOS and waits for the AppKit
        // editing command from its embedder instead, so the tab names it —
        // task: backspace no longer works on mac forms.
        let press = |key: &str, mods: Modifiers| KeyEvent {
            key: key.into(),
            code: key.into(),
            text: Some("\u{8}".into()),
            repeat: false,
            mods,
        };
        assert_eq!(
            apple_keybinding(&press("Backspace", Modifiers::NONE)),
            Some("deleteBackward:")
        );
        assert_eq!(
            apple_keybinding(&press("Backspace", Modifiers::NONE.with_alt())),
            Some("deleteWordBackward:"),
            "⌥ + delete deletes a word, like the word-delete of every other platform"
        );
        // Forward delete, the caret keys and plain characters are not commands:
        // they reach blitz from the key event, and naming them too would act twice.
        assert_eq!(apple_keybinding(&press("Delete", Modifiers::NONE)), None);
        assert_eq!(
            apple_keybinding(&press("ArrowLeft", Modifiers::NONE.with_alt())),
            None
        );
        assert_eq!(apple_keybinding(&press("a", Modifiers::NONE)), None);
        // Only a Mac build feeds them at all.
        assert_eq!(APPLE_KEYBINDINGS, cfg!(target_os = "macos"));
    }
}

#[cfg(test)]
mod user_agent_args_tests {
    use super::*;

    /// Parse a minimal argv (`--tab-id/--container-id/--profile-dir`) plus the
    /// flags under test; the temp profile dir is created by `parse` itself.
    fn parse_args(flags: &[&str]) -> Result<TabArgs, Box<dyn Error>> {
        let dir = tempfile::tempdir().expect("profile dir");
        let mut args: Vec<String> = vec![
            "ironsurf-tab".to_string(),
            "--tab-id".to_string(),
            "7".to_string(),
            "--container-id".to_string(),
            "1".to_string(),
            "--profile-dir".to_string(),
            dir.path().to_str().expect("utf-8 path").to_string(),
        ];
        args.extend(flags.iter().map(|flag| flag.to_string()));
        // `dir` stays alive until here; `parse` only records the path.
        let parsed = TabArgs::parse(&args);
        drop(dir);
        parsed
    }

    #[test]
    fn user_agent_flag_captures_the_initial_override() {
        let args = parse_args(&["--user-agent", "UA-TEST/1.0"]).expect("parses");
        assert_eq!(args.user_agent.as_deref(), Some("UA-TEST/1.0"));
    }

    #[test]
    fn user_agent_flag_absent_means_the_builtin_default() {
        let args = parse_args(&[]).expect("parses");
        assert_eq!(
            args.user_agent, None,
            "no flag ⇒ no override; the tab seeds its cell with netmod::USER_AGENT"
        );
        // …and the seeded cell indeed reports the default.
        assert_eq!(
            netmod::read_user_agent(&netmod::new_user_agent(args.user_agent.as_deref())),
            netmod::USER_AGENT
        );
    }

    #[test]
    fn blank_flag_value_seeds_the_default_cell() {
        // Some("") (and pure whitespace) parse fine but resolve back to the
        // built-in default — the same reset rule as `SetUserAgent { Some("") }`.
        let args = parse_args(&["--user-agent", "   "]).expect("parses");
        assert_eq!(args.user_agent.as_deref(), Some("   "));
        assert_eq!(
            netmod::resolve_user_agent(args.user_agent.as_deref()).as_str(),
            netmod::USER_AGENT
        );
    }

    #[test]
    fn user_agent_flag_requires_a_value() {
        assert!(
            parse_args(&["--user-agent"]).is_err(),
            "--user-agent without a value is a usage error"
        );
    }
}
