//! Private browsing, end to end on the testing backend (task: private
//! browsing) — per container, toggled from the container manager.
//!
//! The scripted fake tab (`--expect-dnt`) echoes its *current* DNT value on
//! the status line — the launch `--private` at startup and every inbound
//! `ToTab::SetDoNotTrack` afterwards (`dnt:on|off`) — so the whole flow is
//! assertable over the real pipe: toggling a container's Private checkbox
//! persists the flag to the Store and broadcasts it to that container's
//! **own live tabs only**; a newly spawned tab of a private container
//! launches with `--private` and reports `dnt:on` from its very first line;
//! the top-bar badge (`container-private`) tracks the container of the tab
//! **on screen**; and the shared `history.jsonl` grows from every container's
//! visits **except** a private container's — UrlChanged is dropped and
//! TitleChanged has nothing to stamp — until the flag is cleared, when
//! recording resumes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::Model;

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The status line — the fake tab's `dnt:<value>` echo lands here.
fn status(ui: &AppWindow) -> String {
    ui.get_status_text().to_string()
}

/// The container manager rows exactly as the overlay renders them:
/// (name, private).
fn container_rows(ui: &AppWindow) -> Vec<(String, bool)> {
    let rows = ui.get_containers();
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (r.name.to_string(), r.private)
        })
        .collect()
}

#[test]
fn private_browsing_is_per_container_and_leaves_no_history() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let log = ironsurf_storage::History::open(dir.path());
    let log_urls = || log.entries().into_iter().map(|e| e.url).collect::<Vec<_>>();

    // Seed a second container ("lab", id 2) before the shell opens its
    // Store, so the toggle has two containers to stay apart of.
    {
        let mut seed = ironsurf_storage::Store::open(dir.path()).unwrap();
        let lab = seed.create_container("lab", "#aa00ff").unwrap();
        assert_eq!(lab, ironsurf_common::ContainerId(2));
    }
    let disk_private = |id: u32| {
        ironsurf_storage::Store::open(dir.path())
            .unwrap()
            .containers()
            .into_iter()
            .find(|c| c.id.0 == id)
            .is_some_and(|c| c.private)
    };

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake.clone(), vec!["--expect-dnt".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();
    let tabs = || core.tab_snapshot();

    // ── Tab 1, default container, not private ⇒ dnt:off, badge dark ─────────
    wait_for("tab ready, launched without --private ⇒ dnt:off", || {
        pump();
        tabs().first().is_some_and(|t| t.ready) && status(&ui) == "dnt:off"
    });
    assert!(!ui.get_container_private(), "no badge for a normal container");
    assert!(container_rows(&ui).iter().all(|(_, p)| !*p));

    // ── The manager's Private checkbox: persists, broadcasts to that
    // container's live tabs (container 2 has none yet — just the flag) ───────
    ui.invoke_open_manager();
    ui.invoke_mgr_toggle_private(1);
    pump();
    assert!(disk_private(2), "the flag is stored on the container");
    assert!(!disk_private(1), "the other container is untouched");
    assert_eq!(
        container_rows(&ui),
        vec![
            ("personal".to_string(), false),
            ("lab".to_string(), true),
        ],
        "the manager rows mirror the stored flags"
    );

    // ── A tab born in the private container spawns with --private ───────────
    ui.invoke_mgr_new_tab_in(1);
    wait_for("the private tab is ready", || {
        pump();
        tabs().len() == 2 && tabs()[1].ready
    });
    wait_for("it reports the launch --private flag on its status line", || {
        pump();
        status(&ui) == "dnt:on"
    });
    assert!(ui.get_container_private(), "the badge lights for the on-screen private container");

    // ── Its visits never touch the shared log (UrlChanged dropped, so
    // TitleChanged has nothing to stamp) ─────────────────────────────────────
    ui.invoke_navigate("http://private.example/secret".into());
    wait_for("the private navigation settles on the tab", || {
        pump();
        tabs().get(1).is_some_and(|t| t.url.as_deref() == Some("http://private.example/secret"))
    });
    std::thread::sleep(Duration::from_millis(150));
    pump();
    assert!(log_urls().is_empty(), "a private visit leaves no trace in the history log");

    // ── The normal container's tab: badge dark, visits recorded as always ───
    ui.invoke_activate_tab(0);
    pump();
    assert_eq!(status(&ui), "dnt:off", "the private broadcast never reached container 1");
    assert!(!ui.get_container_private(), "the badge follows the tab on screen");
    ui.invoke_navigate("http://public.example/".into());
    wait_for("the normal visit lands in the shared log", || {
        pump();
        log_urls() == vec!["http://public.example/"]
    });

    // ── Toggling the private container OFF: its live tab hears the wire at
    // once, and its visits resume recording ──────────────────────────────────
    ui.invoke_activate_tab(1);
    wait_for("the private tab is on screen again", || {
        pump();
        status(&ui) == "dnt:on"
    });
    ui.invoke_open_manager();
    ui.invoke_mgr_toggle_private(1);
    pump();
    assert!(!disk_private(2), "the cleared flag is stored");
    wait_for("the live tab receives the cleared broadcast", || {
        pump();
        status(&ui) == "dnt:off"
    });
    assert!(!ui.get_container_private(), "and the badge goes dark with it");
    ui.invoke_navigate("http://back.example/".into());
    wait_for("recording resumes for the un-privatised container", || {
        pump();
        log_urls().contains(&"http://back.example/".to_string())
    });

    // ── The DEFAULT container can go private too: its live tab hears it, its
    // badge lights, and its visits stop mid-session ──────────────────────────
    ui.invoke_activate_tab(0);
    wait_for("the default tab is on screen", || {
        pump();
        status(&ui) == "dnt:off"
    });
    ui.invoke_open_manager();
    ui.invoke_mgr_toggle_private(0);
    pump();
    assert!(disk_private(1));
    assert!(!disk_private(2), "container 2 keeps its (cleared) state");
    wait_for("the live default-container tab starts sending DNT", || {
        pump();
        status(&ui) == "dnt:on"
    });
    assert!(ui.get_container_private());
    ui.invoke_navigate("http://gone.example/".into());
    wait_for("that visit settles on the tab", || {
        pump();
        tabs().first().is_some_and(|t| t.url.as_deref() == Some("http://gone.example/"))
    });
    std::thread::sleep(Duration::from_millis(150));
    pump();
    assert!(
        !log_urls().contains(&"http://gone.example/".to_string()),
        "a container that just went private stops leaving traces"
    );

    app.shutdown();
}
