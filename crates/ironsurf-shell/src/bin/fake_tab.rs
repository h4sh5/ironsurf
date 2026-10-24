//! fake_tab — a scripted stand-in for `ironsurf-tab` used by the shell's
//! headless pump tests (and as an escape hatch when the engine build is not
//! available). Speaks the same framing: `FromTab` on stdout, `ToTab` on
//! stdin; exits 0 on `Destroy` or EOF. `--crash` exits 101 right after the
//! preamble with a fake panic + backtrace on stderr (crash-report tests).

use std::io::BufReader;

use ironsurf_common::{
    read_frame, write_frame, ContextAction, ContextTarget, ContainerId, CursorIcon, DialogKind,
    DownloadHint, DownloadTrigger, FromTab, InspectKind, InspectNode, InspectSnapshot, MouseButton,
    PointerKind, TabId, ToTab,
};

/// The fake tab's "page": the text its find-in-page searches, standing in for
/// the real engine's laid-out document (task: find bar). The word `page`
/// appears three times, so `--find page` is a scripted three-hit search.
const FAKE_PAGE: &str =
    "Fake page. This fake page says page three times, and nothing else worth finding.";

/// Case-insensitive, non-overlapping hits of `query` in [`FAKE_PAGE`] — the
/// fake's stand-in for the engine's search, so the shell's counter, its wrap
/// and its empty-query behaviour are all testable without a document.
fn fake_find_matches(query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    let hay = FAKE_PAGE.to_lowercase();
    let needle = query.to_lowercase();
    let mut from = 0usize;
    let mut hits = 0usize;
    while let Some(at) = hay[from..].find(&needle) {
        hits += 1;
        from += at + needle.len();
    }
    hits
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut tab_id = TabId(0);
    let mut _container = ContainerId(1);
    let mut url = "about:blank".to_string();
    let mut size = (400u32, 300u32);
    let mut crash = false;
    let mut devtools_feed = false;
    let mut dialog_feed = false;
    let mut cursor = false;
    let mut expect_userscripts = false;
    // Inspector harness (task: Inspector — shell side): counts every
    // `ToTab::SetInspect` this fake receives, so a test can prove the shell asks
    // for a document once when the pane opens and never on a redraw.
    let mut inspect_streamed = 0u32;
    // ⌖ *inspect element* is the tab's own business: while it is armed, the next
    // left click in the page names a node and is answered with `InspectPicked`
    // (the same scripted hit as `ToTab::InspectPickAt`), then the mode is off.
    let mut inspect_pick = false;
    // User-Agent-switcher harness (task: per-container UA): the launch
    // `--user-agent` is this fake tab's *initial* current value (exactly how
    // the real tab seeds its UA cell), and with `--expect-user-agent` every
    // current value — the launch one and each inbound ToTab::SetUserAgent —
    // echoes on the status line as `ua:<value>` (`ua:default` for `None`).
    let mut expect_user_agent = false;
    let mut current_ua: Option<String> = None;
    // Proxy harness (task: proxies): `--proxy` is this fake tab's initial
    // route (exactly how the real tab seeds its process proxy cell), and
    // with `--expect-proxy` every current value — the launch one and each
    // inbound ToTab::SetProxy — echoes on the status line as
    // `proxy:<url>` (`proxy:direct` for `None`).
    let mut expect_proxy = false;
    let mut current_proxy: Option<String> = None;
    // Archive-mode harness (task: archive mode): `--archive-mode` is the
    // spawn-time switch (exactly how the real tab seeds its archive sink),
    // and with `--expect-archive-mode` every current value — the launch one
    // and each inbound ToTab::SetArchiveMode — echoes as `archive:on|off`.
    let mut expect_archive_mode = false;
    // Private-browsing harness (task: private browsing): `--private` is the
    // spawn-time switch (exactly how the real tab seeds its DNT cell), and
    // with `--expect-dnt` every current value — the launch one and each
    // inbound ToTab::SetDoNotTrack — echoes as `dnt:on|off`.
    let mut expect_dnt = false;
    let mut current_dnt = false;
    let mut expect_copy = false;
    // Selection-copy harness (task: text-selection copy & paste): Ctrl+C
    // answers with `FromTab::ClipboardText` (what the real tab's engine
    // ships when a page copy settles) and a `ToTab::Clipboard` push is
    // echoed on the status line, so the tests can watch the shell's chord
    // protocol: clipboard first, keystroke second.
    let mut current_archive = false;
    // With `--expect-popup-policy`: echo the popup policy the shell installs
    // (task: block popups). The rules live in the real tab; a fake only proves a
    // decision written by the shell reached the page's gate.
    let mut expect_popup_policy = false;
    // With `--popup-blocked`: a page whose address contains "popup" asks for a
    // window nobody clicked for, and the tab refuses it (task: block popups) —
    // the fake has no engine, so it reports the refusal the real runtime reports
    // (`FromTab::PopupBlocked`) and lets the test follow what the shell makes of
    // it: a notice on the status line, gone with the next page.
    let mut popup_blocked = false;
    let mut current_block = true;
    let mut current_allowed: Vec<String> = Vec::new();

    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |what: &str| -> String {
            it.next()
                .unwrap_or_else(|| panic!("--{what} requires a value"))
                .clone()
        };
        match arg.as_str() {
            "--tab-id" => tab_id = TabId(value("tab-id").parse().unwrap()),
            "--container-id" => _container = ContainerId(value("container-id").parse().unwrap()),
            "--profile-dir" => {
                value("profile-dir");
            }
            "--url" => url = value("url"),
            "--size" => {
                let v = value("size");
                let (w, h) = v.split_once('x').unwrap_or(("400", "300"));
                size = (w.parse().unwrap_or(400), h.parse().unwrap_or(300));
            }
            "--crash" => crash = true,
            // Scripted DevTools feed (console + network) so the shell's panel is
            // testable without an engine: one batch at startup, one more after
            // every Reload.
            "--devtools-feed" => devtools_feed = true,
            // Scripted JS dialogs (task: window.alert/confirm/prompt): one
            // alert at startup, then one prompt after every Reload — each
            // echoed back on the status line when the shell answers it.
            "--dialog" => dialog_feed = true,
            // Scripted hover cursor (task: cursor icon): the fake page is a
            // "link" at x ≥ 100 — hover reports the pointing hand there and
            // the plain arrow elsewhere (reported only on change, as the
            // real tab does).
            "--cursor" => cursor = true,
            // Userscript-menu harness (task: userscript menu): every inbound
            // ToTab::SetUserscripts is echoed on the status line with the ids
            // it carries (comma-joined), so the tests can assert the shell
            // seeded this tab and rebroadcast every mutation.
            "--expect-userscripts" => expect_userscripts = true,
            "--expect-user-agent" => expect_user_agent = true,
            "--expect-proxy" => expect_proxy = true,
            "--archive-mode" => current_archive = true,
            "--private" => current_dnt = true,
            "--expect-dnt" => expect_dnt = true,
            "--expect-popup-policy" => expect_popup_policy = true,
            "--popup-blocked" => popup_blocked = true,
            "--expect-archive-mode" => expect_archive_mode = true,
            "--expect-copy" => expect_copy = true,
            // the shell spawns a UA container's tabs with the container's UA;
            // mirror the real tab: blank/absent ⇒ the built-in default.
            "--user-agent" => {
                let v = value("user-agent");
                let v = v.trim().to_string();
                if !v.is_empty() {
                    current_ua = Some(v);
                }
            }
            // the shell spawns a proxied container's tabs with the resolved
            // URL; mirror the real tab: blank/absent ⇒ direct.
            "--proxy" => {
                let v = value("proxy").trim().to_string();
                if !v.is_empty() {
                    current_proxy = Some(v);
                }
            }
            _ => {} // tolerate the real tab's flags
        }
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut emit = |msg: FromTab| {
        let _ = write_frame(&mut out, &msg);
    };

    emit(FromTab::Ready { tab_id });
    // `ironsurf://archive` at launch asks for the archive server exactly like
    // the real runtime does while rendering that page (and like the fake does
    // on a later Navigate to it).
    ask_archive(&mut emit, &url);
    // …and the same for the browsing-history page: the visit log is the shell's,
    // so a tab launched onto it asks for the list right away (the real runtime
    // asks while rendering the page, launch URL included).
    ask_history(&mut emit, &url);
    announce(&mut emit, &url, false);
    emit(frame(size)); // like the real tab: a first frame without any Resize

    // The launch UA (from `--user-agent`, when the shell passed one) is the
    // current value from the very first page load: echo it so a test can see
    // the spawn path delivered it.
    if expect_user_agent {
        emit(FromTab::StatusText {
            text: Some(format!(
                "ua:{}",
                current_ua.clone().unwrap_or_else(|| "default".into())
            )),
        });
    }
    // …and the launch proxy likewise (task: proxies): the spawn path
    // delivered the container's resolved URL before the first paint.
    if expect_proxy {
        emit(FromTab::StatusText {
            text: Some(format!(
                "proxy:{}",
                current_proxy.clone().unwrap_or_else(|| "direct".into())
            )),
        });
    }

    // The spawn-time private switch echoes too, so a test can see the
    // shell passed `--private` to a tab born in a private container.
    if expect_dnt {
        emit(FromTab::StatusText {
            text: Some(format!("dnt:{}", if current_dnt { "on" } else { "off" })),
        });
    }

    // The spawn-time archive switch echoes too, so a test can see the
    // shell passed `--archive-mode` to a tab born while the mode was on.
    if expect_archive_mode {
        emit(FromTab::StatusText {
            text: Some(format!("archive:{}", if current_archive { "on" } else { "off" })),
        });
        // A tab *born with the mode on* ships archived fetches from its very
        // first load — the fake emits them right away: a document, a
        // query-stringed subresource and a query-stringed document (both
        // queries ride on the file name), and a `file:` asset the shell must
        // drop (a local file is no website).
        if current_archive {
            emit(FromTab::ArchivedFetch {
                url: "http://example.com/".into(),
                is_document: true,
                bytes: b"ARCHIVE-PAGE".to_vec(),
            });
            emit(FromTab::ArchivedFetch {
                url: "http://example.com/html/?q=asdf".into(),
                is_document: true,
                bytes: b"ARCHIVE-QUERY-PAGE".to_vec(),
            });
            emit(FromTab::ArchivedFetch {
                url: "http://example.com/js/app.js?v=2".into(),
                is_document: false,
                bytes: b"ARCHIVE-APP".to_vec(),
            });
            emit(FromTab::ArchivedFetch {
                url: "file:///etc/passwd".into(),
                is_document: false,
                bytes: b"MUST-NOT-LAND".to_vec(),
            });
        }
    }

    if devtools_feed {
        emit_feed(&mut emit, 1);
    }

    // Downloads (task: file downloads): the next id this fake mints — the real
    // tab mints ids per process too, never globally.
    let mut download_seq: u64 = 0;

    // The scripted dialogs: `dialog_seq` is the next request id (the shell
    // must answer it before this fake tab speaks again on the topic).
    let mut dialog_seq: u32 = 0;
    if dialog_feed {
        dialog_seq += 1;
        emit(FromTab::DialogRequest {
            dialog_id: dialog_seq,
            kind: DialogKind::Alert,
            message: "Fake page greets you!".into(),
            default: None,
        });
    }

    if crash {
        // With `--dialog` too: the startup modal above is up *before* the
        // fake panic — the shell must close that overlay when it notices
        // the crash (test: dialogs).
        if dialog_feed {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        // Stand in for a panicking `ironsurf-tab`: the panic line + backtrace
        // go to stderr (which the shell captures), then a crash exit code.
        eprintln!("thread 'main' panicked at src/bin/fake_tab.rs:99:10:\ncrash requested");
        eprintln!("tab process {} crashing: crash requested", std::process::id());
        eprintln!("stack backtrace:\n   0: fake_backtrace_frame_0\n   1: fake_backtrace_frame_1");
        std::process::exit(101);
    }

    // Find in page (task: find bar): the live query and the 1-based position of
    // the active hit, exactly as the real tab keeps them.
    let mut find_query = String::new();
    let mut reader_on = false;
    let mut find_at: usize = 0;

    // Last scripted cursor (`--cursor`); `None` = nothing reported yet.
    let mut last_cursor: Option<CursorIcon> = None;

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    loop {
        let msg = match read_frame::<_, ToTab>(&mut reader) {
            Ok(msg) => msg,
            Err(_) => break, // EOF or corrupt: clean death
        };
        match msg {
            ToTab::Navigate { url: new } => {
                // `ironsurf://archive` stands in for the real page: the tab
                // cannot serve, so it asks the shell for the archive server
                // (the same message the real runtime emits when it renders
                // that page) and waits for the address back.
                ask_archive(&mut emit, &new);
                ask_history(&mut emit, &new);
                url = new;
                announce(&mut emit, &url, true);
                // The scripted "popup page": one window asked for, one refusal
                // reported (task: block popups).
                if popup_blocked && url.contains("popup") {
                    emit(FromTab::PopupBlocked {
                        url: "https://ads.test/interstitial".to_string(),
                    });
                }
            }
            // The shell's answer: report it on the status line, which is what
            // the tests read as the page's summary.
            ToTab::ArchiveServerInfo { ok, address, error, .. } => {
                emit(FromTab::StatusText {
                    text: Some(format!(
                        "archive-server:{}:{}",
                        if ok { address.as_str() } else { "off" },
                        error.as_deref().unwrap_or("")
                    )),
                });
                emit(FromTab::ConsoleLog {
                    source: "log".into(),
                    level: if ok { "info".into() } else { "error".into() },
                    message: if ok {
                        format!("archive server at {address}")
                    } else {
                        format!("archive server failed: {}", error.as_deref().unwrap_or("unknown"))
                    },
                });
            }
            // The browsing-history page's answer (`ironsurf://history`): the
            // fake reports the count the shell handed over, which is what a test
            // reads as the page having been fed.
            ToTab::HistoryList { ref entries, total, .. } => {
                emit(FromTab::StatusText {
                    text: Some(format!("history:{total}:{}", entries.len())),
                });
                for visit in entries.iter().take(3) {
                    emit(FromTab::ConsoleLog {
                        source: "log".into(),
                        level: "info".into(),
                        message: format!("visit {} {}", visit.url, visit.title.as_deref().unwrap_or("-")),
                    });
                }
            }
            ToTab::Reload => {
                announce(&mut emit, &url, true);
                if devtools_feed {
                    emit_feed(&mut emit, 2);
                }
                if dialog_feed {
                    dialog_seq += 1;
                    emit(FromTab::DialogRequest {
                        dialog_id: dialog_seq,
                        kind: DialogKind::Prompt,
                        message: "Give it a name:".into(),
                        default: Some("ada".into()),
                    });
                }
            }
            // Reader view (task: reader mode): the fake has no real document
            // to clean up, so it plays along — it always has something
            // "readable" and confirms the requested state back, which is
            // exactly what drives the shell's 📄 button.
            ToTab::SetReader { enabled } => {
                reader_on = enabled;
                emit(FromTab::ReaderState { enabled: reader_on });
            }
            // Find in page (task: find bar): the fake searches its own canned
            // page text — the shell cannot tell the difference from the real
            // engine, which searches the document's laid-out text.
            ToTab::SetFind { query } => {
                find_query = query;
                let matches = fake_find_matches(&find_query);
                // Like the real tab: the first hit becomes active (1-based) and
                // an empty query reports nothing at all.
                find_at = if matches == 0 { 0 } else { 1 };
                emit(FromTab::FindResult {
                    query: find_query.clone(),
                    matches,
                    active: find_at,
                });
            }
            ToTab::FindNext => {
                let matches = fake_find_matches(&find_query);
                if matches > 0 {
                    // Wrapping at both ends, 1-based, like `Tab::find_jump`.
                    find_at = if find_at >= matches { 1 } else { find_at + 1 };
                }
                emit(FromTab::FindResult {
                    query: find_query.clone(),
                    matches,
                    active: find_at,
                });
            }
            ToTab::FindPrevious => {
                let matches = fake_find_matches(&find_query);
                if matches > 0 {
                    find_at = if find_at <= 1 { matches } else { find_at - 1 };
                }
                emit(FromTab::FindResult {
                    query: find_query.clone(),
                    matches,
                    active: find_at,
                });
            }
            // The shell's answer to a scripted dialog: echo it on the status
            // line so the tests can see the full round trip.
            ToTab::DialogResponse {
                dialog_id,
                confirmed,
                text,
            } => {
                emit(FromTab::StatusText {
                    text: Some(format!(
                        "dialog{dialog_id}:{}:{}",
                        confirmed as u8,
                        text.unwrap_or_default()
                    )),
                });
            }
            ToTab::Resize { width, height, .. } => {
                size = (width.clamp(16, 4096), height.clamp(16, 4096));
                emit(frame(size));
            }
            ToTab::Back | ToTab::Forward | ToTab::Stop => {
                emit(FromTab::HistoryChanged { can_go_back: false, can_go_forward: false });
            }
            // A right-click release reports a fixed scripted target so the
            // shell's context-menu logic is testable without an engine
            // (task: context menu).
            ToTab::Pointer(p) => {
                if p.button == MouseButton::Right && p.kind == PointerKind::Up {
                    emit(FromTab::ContextMenu {
                        x: p.x,
                        y: p.y,
                        target: ContextTarget {
                            link: Some(format!("{url}/menu-link")),
                            image: None,
                            video: None,
                            video_playing: false,
                            selection: Some("fake selection".into()),
                            editable: true,
                        },
                    });
                }
                if inspect_pick && p.button == MouseButton::Left && p.kind == PointerKind::Up {
                    inspect_pick = false;
                    let hit = inspect_base(tab_id) + 6;
                    emit(FromTab::StatusText {
                        text: Some(format!("picked-at:{},{}", p.x, p.y)),
                    });
                    emit(FromTab::InspectSnapshot {
                        snapshot: fake_document(&url, tab_id, Some(hit)),
                    });
                    emit(FromTab::InspectPicked { node: hit });
                }
                if cursor
                    && p.button == MouseButton::Left
                    && matches!(p.kind, PointerKind::Move | PointerKind::Out)
                {
                    let icon = if p.kind != PointerKind::Out && p.x >= 100.0 {
                        CursorIcon::Pointer
                    } else {
                        CursorIcon::Default
                    };
                    if Some(icon) != last_cursor {
                        last_cursor = Some(icon);
                        emit(FromTab::CursorChanged { cursor: icon });
                    }
                }
            }
            // The UA switcher's wire (task: per-container UA): replace the
            // current value and — with `--expect-user-agent` — echo it on the
            // status line, `ua:default` for `None` (the built-in default).
            // The real tab never reloads for this; neither does the fake.
            ToTab::SetUserAgent { user_agent } => {
                current_ua = user_agent
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty());
                if expect_user_agent {
                    emit(FromTab::StatusText {
                        text: Some(format!(
                            "ua:{}",
                            current_ua.clone().unwrap_or_else(|| "default".into())
                        )),
                    });
                }
            }
            // The private-browsing wire (task: private browsing): replace the
            // current DNT value and — with `--expect-dnt` — echo it on the
            // status line. The real tab applies it to the next request built;
            // nothing reloads for this; neither does the fake.
            ToTab::SetDoNotTrack { enabled } => {
                current_dnt = enabled;
                if expect_dnt {
                    emit(FromTab::StatusText {
                        text: Some(format!("dnt:{}", if current_dnt { "on" } else { "off" })),
                    });
                }
            }
            // The proxy wire (task: proxies): replace the current route and
            // — with `--expect-proxy` — echo it on the status line,
            // `proxy:direct` for `None`. Nothing reloads for this; neither
            // does the fake.
            ToTab::SetProxy { proxy } => {
                current_proxy = proxy
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty());
                if expect_proxy {
                    emit(FromTab::StatusText {
                        text: Some(format!(
                            "proxy:{}",
                            current_proxy.clone().unwrap_or_else(|| "direct".into())
                        )),
                    });
                }
            }
            // The archive-mode wire (task: archive mode): replace the current
            // value and — with `--expect-archive-mode` — echo it. The real
            // tab ships future fetches to the shell; this fake just tracks
            // that the toggle arrived, in order.
            ToTab::SetArchiveMode { enabled } => {
                current_archive = enabled;
                if expect_archive_mode {
                    emit(FromTab::StatusText {
                        text: Some(format!("archive:{}", if current_archive { "on" } else { "off" })),
                    });
                }
            }
            // The popup policy wire (task: block popups): hold what the shell
            // asked for and — with `--expect-popup-policy` — echo it as
            // `popups:block:` / `popups:allow:https://a.test,https://b.test`, the
            // same shape every other per-tab setting echoes in. The real tab
            // applies the rules to its own pages; neither this fake nor the
            // shell rewrites what the tab then decided.
            ToTab::SetPopupPolicy { block, allowed } => {
                current_block = block;
                current_allowed = allowed.clone();
                if expect_popup_policy {
                    emit(FromTab::StatusText {
                        text: Some(format!(
                            "popups:{}:{}",
                            if current_block { "block" } else { "allow" },
                            current_allowed.join(",")
                        )),
                    });
                }
            }
            ToTab::Key(k) => {
                // The real tab answers Ctrl+C over a selection by shipping
                // that selection to the shell; the fake page's selection is
                // the same "fake selection" its right-click target reports.
                if expect_copy && k.key == "c" && k.mods.ctrl() {
                    emit(FromTab::ClipboardText {
                        text: "fake selection".into(),
                    });
                }
            }
            ToTab::Scroll(_) | ToTab::SetVisible { .. } => {}

            ToTab::Clipboard { text } => {
                // Cache push (Ctrl+V or a menu Paste): echo it when asked.
                if expect_copy {
                    emit(FromTab::StatusText {
                        text: Some(format!("clipboard:{text}")),
                    });
                }
            }
            ToTab::ContextAction(ContextAction::Paste { text }) => {
                emit(FromTab::StatusText {
                    text: Some(format!("pasted:{text}")),
                });
            }
            ToTab::ContextAction(ContextAction::ToggleVideo) => emit(FromTab::StatusText {
                text: Some("video-toggled".into()),
            }),
            ToTab::SetZoom { .. } => {} // no real layout to rescale
            ToTab::SetScrollSpeed { .. } => {} // no wheel here to feel the difference
            // DevTools Inspector (task: Inspector — shell side): the fake tab has no
            // DOM, so the pane's commands are accepted and answered the way the real
            // tab would at best — an empty tree of a blank document, and status-line
            // echoes that let the shell-side tests prove the wiring fires.
            ToTab::SetInspect { open } => {
                inspect_streamed += 1;
                if open {
                    emit(FromTab::InspectSnapshot {
                        snapshot: fake_document(&url, tab_id, None),
                    });
                }
                // Counted echo: the shell-side tests prove the pane asks for a
                // document once when it opens and never again on a redraw.
                emit(FromTab::StatusText {
                    text: Some(format!(
                        "inspect-stream:{}:{inspect_streamed}",
                        if open { "on" } else { "off" }
                    )),
                });
            }
            ToTab::SetInspectPick { enabled } => {
                inspect_pick = enabled;
                emit(FromTab::StatusText {
                    text: Some(format!("pick:{}", if enabled { "on" } else { "off" })),
                });
            }
            ToTab::InspectSelect { node } => emit(FromTab::StatusText {
                text: Some(format!("select:{}", node.map(|n| n.to_string()).unwrap_or("-".into()))),
            }),
            // …and answering "what is under this point" the way the real tab
            // does: highlight it, send the same generation carrying the new
            // selection, answer with `InspectPicked`, disarm.
            ToTab::InspectPickAt { x, y } => {
                emit(FromTab::StatusText {
                    text: Some(format!("pick-at:{x},{y}")),
                });
                let hit = inspect_base(tab_id) + 6;
                emit(FromTab::InspectSnapshot {
                    snapshot: fake_document(&url, tab_id, Some(hit)),
                });
                emit(FromTab::InspectPicked { node: hit });
            }
            // Userscripts (task: userscript execution): only the REAL tab runs
            // them; this fake has no JS engine, so the set is accepted and
            // dropped. The point is that the message never breaks the pump.
            // With `--expect-userscripts` the ids echo on the status line so
            // the shell-side menu test can see exactly what arrived, in order.
            ToTab::SetUserscripts { scripts } => {
                if expect_userscripts {
                    let ids = scripts
                        .iter()
                        .map(|s| s.id.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    emit(FromTab::StatusText {
                        text: Some(format!("userscripts:[{ids}]")),
                    });
                }
            }
            // Downloads (task: file downloads): the fake has no engine, no
            // network, and no disk — so it scripts the fetch the real tab
            // would make, in the same order: `DownloadStart` (whose `name` is
            // the **untrusted** hint handed in, so a test can watch the shell
            // sanitize it), the body in two chunks, `DownloadEnd`. A URL
            // carrying "fail" fails before a byte ships — which is also the
            // scripted zero-byte download; the echo names the row's id.
            // No status-line echo: the real tab answers a download with
            // `StatusText(None)` (the download is not a navigation), so the
            // shell's own row is what the line shows.
            ToTab::DownloadUrl { url, suggested_name } => {
                download_seq += 1;
                let id = download_seq;
                emit(FromTab::DownloadStart {
                    hint: DownloadHint {
                        id,
                        url: url.clone(),
                        name: suggested_name,
                        media_type: Some("application/zip".into()),
                        trigger: DownloadTrigger::Requested,
                    },
                });
                if url.contains("fail") {
                    emit(FromTab::DownloadEnd {
                        id,
                        error: Some("dns failure".into()),
                    });
                } else {
                    emit(FromTab::DownloadChunk {
                        id,
                        bytes: b"FAKE-DOWNLOAD-".to_vec(),
                    });
                    emit(FromTab::DownloadChunk {
                        id,
                        bytes: b"BODY".to_vec(),
                    });
                    emit(FromTab::DownloadEnd { id, error: None });
                }
            }
            ToTab::Destroy => break,
        }
    }
    std::process::exit(0);
}

/// A scripted DevTools feed: batch 1 is what the page said while it loaded (a
/// log line, a CSS warning, one answered request, one that never got anywhere),
/// batch 2 adds an error line and a redirect. Shapes mirror the real tab:
/// `error` is `Some` exactly when `status` is `None`.
fn emit_feed(emit: &mut impl FnMut(FromTab), batch: u8) {
    if batch == 1 {
        emit(FromTab::ConsoleLog {
            source: "js".into(),
            level: "log".into(),
            message: "fake tab awake".into(),
        });
        emit(FromTab::ConsoleLog {
            source: "css".into(),
            level: "warn".into(),
            message: "unsupported property zoom".into(),
        });
        emit(FromTab::NetworkEvent {
            event: ironsurf_common::NetEvent {
                method: "GET".into(),
                url: "https://example.com/".into(),
                status: Some(200),
                response_bytes: Some(1234),
                duration_ms: Some(42),
                error: None,
                initiator: Some("document".into()),
            },
        });
        emit(FromTab::NetworkEvent {
            event: ironsurf_common::NetEvent {
                method: "POST".into(),
                url: "https://api.invalid/upload".into(),
                status: None,
                response_bytes: None,
                duration_ms: Some(7),
                error: Some("dns failure".into()),
                initiator: Some("xhr".into()),
            },
        });
    } else {
        emit(FromTab::ConsoleLog {
            source: "js".into(),
            level: "error".into(),
            message: "reloaded and upset".into(),
        });
        emit(FromTab::NetworkEvent {
            event: ironsurf_common::NetEvent {
                method: "GET".into(),
                url: "https://example.com/next".into(),
                status: Some(301),
                response_bytes: Some(0),
                duration_ms: Some(1500),
                error: None,
                initiator: Some("document".into()),
            },
        });
    }
}

/// Emit [`FromTab::ArchiveServerStart`] when a URL is the archive page (see
/// [`archive_request_args`]); silence otherwise.
fn ask_archive(emit: &mut impl FnMut(FromTab), url: &str) {
    if !url.starts_with("ironsurf://archive") {
        return;
    }
    let (root, host, port, open) = archive_request_args(url);
    emit(FromTab::ArchiveServerStart {
        root,
        host,
        port,
        open_in_new_tab: open,
    });
}

/// `ironsurf://history` stands in for the real page: the visit log belongs to
/// the shell, so the page asks for it (and says so when the row it opened was
/// "Clear history") and waits for the list back.
fn ask_history(emit: &mut impl FnMut(FromTab), url: &str) {
    if !url.starts_with("ironsurf://history") {
        return;
    }
    let clear = url
        .split_once('?')
        .map(|(_, query)| {
            query.split('&').any(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                key == "clear" && !value.is_empty() && value != "0"
            })
        })
        .unwrap_or(false);
    emit(FromTab::HistoryListRequest { clear });
}

/// Read the `ironsurf://archive` page's query the way the real page does:
/// `?root=…&host=…&port=…&open=1`. Blank fields are `None` ("use the shell's
/// configured value"), an unparsable port is `None` too.
fn archive_request_args(url: &str) -> (Option<String>, Option<String>, Option<u16>, bool) {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let field = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.split_once('='))
            .filter(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
            .filter(|v| !v.is_empty())
    };
    // `find_map` above keeps only the first match; blank-or-missing is None.
    let get = |name: &str| {
        query.split('&').rev().find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name && !v.is_empty()).then(|| v.to_string())
        })
    };
    let _ = field;
    (
        get("root").map(|v| percent_decode(&v)),
        get("host").map(|v| percent_decode(&v)),
        get("port").and_then(|p| p.parse().ok()),
        get("open").is_some(),
    )
}

/// Percent-decode a query value (`%2Ftmp%2Fx` → `/tmp/x`); invalid UTF-8 after
/// decoding is left as the raw text, same as the real page's behaviour.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn announce(emit: &mut impl FnMut(FromTab), url: &str, with_nav_history: bool) {
    emit(FromTab::LoadingChanged { loading: true });
    emit(FromTab::UrlChanged { url: Some(url.to_string()) });
    emit(FromTab::TitleChanged { title: Some(format!("Fake — {url}")) });
    emit(FromTab::HistoryChanged {
        can_go_back: with_nav_history,
        can_go_forward: false,
    });
    emit(FromTab::LoadingChanged { loading: false });
}

/// Node ids of this fake tab's scripted document. Offset by tab id so a test
/// with two tabs can tell whose tree the window is painting.
fn inspect_base(tab: TabId) -> u64 {
    u64::from(tab.0) * 1000
}

/// The scripted document the Inspector pane shows: `document > html > (head >
/// title > text)` and `(body > p > text)`, with this tab's URL as the title's
/// text so the pane's summary names the tab it came from. `selected` is what the
/// tab has highlighted, reported back exactly as the real tab reports it.
fn fake_document(url: &str, tab: TabId, selected: Option<u64>) -> InspectSnapshot {
    let b = inspect_base(tab);
    let node = |id: u64, parent: Option<u64>, depth: usize, kind: InspectKind, tag: &str, text: &str, attrs: &[(&str, &str)]| InspectNode {
        id: b + id,
        parent: parent.map(|p| b + p),
        kind,
        tag: tag.into(),
        attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        text: text.into(),
        depth,
        bounds: None,
    };
    InspectSnapshot {
        generation: 1,
        url: url.into(),
        selected,
        truncated: false,
        nodes: vec![
            node(0, None, 0, InspectKind::Document, "", "", &[]),
            node(1, Some(0), 1, InspectKind::Element, "html", "", &[]),
            node(2, Some(1), 2, InspectKind::Element, "head", "", &[("lang", "en")]),
            node(3, Some(2), 3, InspectKind::Element, "title", "", &[]),
            node(4, Some(3), 4, InspectKind::Text, "", &format!("Fake — {url}"), &[]),
            node(5, Some(1), 2, InspectKind::Element, "body", "", &[("id", "main")]),
            node(6, Some(5), 3, InspectKind::Element, "p", "", &[("class", "note")]),
            node(7, Some(6), 4, InspectKind::Text, "", "fake body text", &[]),
        ],
    }
}

fn frame(size: (u32, u32)) -> FromTab {
    let (w, h) = (size.0.min(1024) as usize, size.1.min(1024) as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            rgba.push((x % 251) as u8);
            rgba.push((y % 253) as u8);
            rgba.push(128);
            rgba.push(255);
        }
    }
    FromTab::Frame { width: w as u32, height: h as u32, scale: 1.0, rgba }
}
