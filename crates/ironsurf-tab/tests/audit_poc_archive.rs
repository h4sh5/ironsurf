//! AUDIT proof, end to end against the **real `ironsurf-tab` binary**.
//!
//! `ironsurf://archive?root=…&host=…&port=…` is an internal page: rendering it
//! emits [`FromTab::ArchiveServerStart`] with `root`/`host`/`port` taken from
//! the **URL query**, and the shell applies them
//! (`app.rs:4005` — "an override on the URL wins over the saved settings").
//! There is no scheme gate and no user-activation requirement on a JS-driven
//! navigation (`follow_pending_js_nav`, `runtime.rs:2360`), so a remote page
//! can reach it:
//!
//! ```html
//! <script>location.href = "ironsurf://archive?root=%2Fetc&host=0.0.0.0&port=80";</script>
//! ```
//!
//! One visit and the browser is exporting an arbitrary directory over HTTP, on
//! the interface and port the page chose. This test visits such a page with the
//! real tab and proves the shell was told what the page asked for. Headless
//! (no `$DISPLAY`), localhost only. See docs/SECURITY_AUDIT.md.

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(90);
/// What the hostile page asks the shell to export, and where.
const WANTED_ROOT: &str = "/etc";
const WANTED_HOST: &str = "0.0.0.0";

/// The drive-by: a normal page on a normal origin, one line of script.
const HOSTILE_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>hostile</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      location.href = "ironsurf://archive?root=%2Fetc&host=0.0.0.0&port=4321";
    </script>
  </body>
</html>
"#;

/// Serve [`HOSTILE_PAGE`] from a loopback origin; return its address.
fn serve_hostile_page() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HOSTILE_PAGE}",
                HOSTILE_PAGE.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

struct TabChild {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<FromTab>,
    seen: Vec<FromTab>,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame");
    }

    fn collect(&mut self, for_long: Duration) {
        let deadline = Instant::now() + for_long;
        while Instant::now() < deadline {
            if let Ok(msg) = self.events.recv_timeout(Duration::from_millis(100)) {
                self.seen.push(msg);
            }
        }
    }

    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(&pred) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; seen {:#?}", self.seen);
            }
            match self.events.recv_timeout(Duration::from_millis(250)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    while let Ok(msg) = self.events.try_recv() {
                        self.seen.push(msg);
                    }
                    if self.seen.iter().any(&pred) {
                        return;
                    }
                    panic!("stdout closed while waiting for {what}; seen {:#?}", self.seen);
                }
            }
        }
    }
}

fn spawn_at(url: &str, profile_dir: &Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("11")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(profile_dir.join("tab-stderr.log")).expect("stderr log"))
        .spawn()
        .expect("spawn ironsurf-tab");
    let stdout = child.stdout.take().expect("stdout");
    let stdin = child.stdin.take().expect("stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    TabChild { child, stdin, events, seen: Vec::new() }
}

/// FINDING 1 (tab half): a page's own `location.href` is enough to reach the
/// internal archive page, and the folder/host/port it names are forwarded to
/// the shell verbatim — the shell then binds them.
#[test]
fn f1_a_page_navigation_configures_the_archive_server() {
    let page_addr = serve_hostile_page();
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("http://{page_addr}/index.html");
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });

    // Give a page-driven `ironsurf://archive?…` long enough to reach the shell
    // (it renders in well under a second), then look at what it produced.
    for _ in 0..8 {
        tab.collect(Duration::from_millis(500));
    }
    let asked: Vec<(Option<String>, Option<String>, Option<u16>)> = tab
        .seen
        .iter()
        .filter_map(|msg| match msg {
            FromTab::ArchiveServerStart { root, host, port, .. } => {
                Some((root.clone(), host.clone(), *port))
            }
            _ => None,
        })
        .collect();
    let carried = asked
        .iter()
        .find(|(root, host, port)| root.is_some() || host.is_some() || port.is_some());
    assert!(
        carried.is_none(),
        "a page script reached FromTab::ArchiveServerStart carrying its own \
         parameters: {carried:?} (all asks: {asked:?}); the shell applies these \
         verbatim (app.rs:4005) and normalize_root accepted {WANTED_ROOT}"
    );
}
