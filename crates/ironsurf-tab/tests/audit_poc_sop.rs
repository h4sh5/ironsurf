//! AUDIT proof — the same-origin policy is not enforced on responses.
//!
//! A page on one origin reads the **body** of a response fetched from a
//! different origin, with the container's cookies attached to that request:
//! `XMLHttpRequest`/`fetch` (`js.rs` + `netmod.rs`) hand the whole envelope
//! back to page code with no `Origin` sent, no `Access-Control-Allow-Origin`
//! consulted and no private-address check.
//!
//! The two origins are `http://localhost:P` and `http://127.0.0.1:P`
//! (host-name-equal rule → distinct origins) served by one test server.
//! Headless, localhost only. See docs/SECURITY_AUDIT.md.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);
/// The victim body no foreign page may read, and the session cookie the
/// victim host sets for itself.
const SECRET: &str = "CROSS-ORIGIN-BANK-STATEMENT";
const SESSION: &str = "sess=IB-leak-me";

/// A page on `http://localhost:P` reading `http://127.0.0.1:P/victim` with a
/// plain cross-origin synchronous XHR; reports what it got as its title.
const PAGE_STEAL: &str = r#"<!doctype html><html><head><title>steal</title></head>
<body style="margin:0;background:#ffffff"><script>
var x = new XMLHttpRequest();
x.open("GET", "http://127.0.0.1:@PORT@/victim", false);
x.send(null);
document.title = "got:" + x.status + ":" + x.responseText;
</script></body></html>
"#;

/// The same, against the victim's `Cookie:` echo — i.e. the victim's own
/// session cookie, read by a foreign page.
const PAGE_STEAL_COOKIE: &str = r#"<!doctype html><html><head><title>steal2</title></head>
<body style="margin:0;background:#ffffff"><script>
var x = new XMLHttpRequest();
x.open("GET", "http://127.0.0.1:@PORT@/echo", false);
x.send(null);
document.title = "echo:" + x.responseText;
</script></body></html>
"#;

fn http_response(status: u16, content_type: &str, body: &[u8], extra: &str) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    ) + &String::from_utf8_lossy(body)
}

/// One server, every route. Deliberately never sends
/// `Access-Control-Allow-Origin`.
fn spawn_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        let steal = PAGE_STEAL.replace("@PORT@", &port.to_string());
        let steal_cookie = PAGE_STEAL_COOKIE.replace("@PORT@", &port.to_string());
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
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
                .split('?')
                .next()
                .unwrap_or("/")
                .to_string();
            let cookie = head
                .lines()
                .find(|line| line.to_ascii_lowercase().replace(' ', "").starts_with("cookie:"))
                .and_then(|line| line.split_once(':').map(|(_, v)| v.trim().to_owned()))
                .unwrap_or_default();
            let response = match path.as_str() {
                "/steal" => http_response(200, "text/html", steal.as_bytes(), ""),
                "/steal2" => http_response(200, "text/html", steal_cookie.as_bytes(), ""),
                "/victim" => http_response(200, "text/plain", SECRET.as_bytes(), ""),
                "/echo" => http_response(
                    200,
                    "text/plain",
                    if cookie.is_empty() { "none".as_bytes() } else { cookie.as_bytes() },
                    "",
                ),
                // A legitimate first visit to the victim host, which hands it
                // its own session cookie.
                "/grant" => http_response(
                    200,
                    "text/html",
                    b"<title>granted</title>",
                    &format!("Set-Cookie: {SESSION}; Path=/\r\n"),
                ),
                _ => http_response(404, "text/plain", b"no such route", ""),
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
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

    /// Collect for up to `wait`; `false` keeps it to a short quiet window.
    fn collect(&mut self, wait: bool) {
        let until = Instant::now() + if wait { DEADLINE } else { Duration::from_millis(200) };
        while Instant::now() < until {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// The latest title, retried until a load-handler script has had its say.
    fn last_title(&mut self) -> String {
        let mut last = String::new();
        for _ in 0..30 {
            self.collect(true);
            if let Some(FromTab::TitleChanged { title, .. }) = self
                .seen
                .iter()
                .rev()
                .find(|m| matches!(m, FromTab::TitleChanged { .. }))
            {
                last = title.clone().unwrap_or_default();
            }
            if last.starts_with("got:") || last.starts_with("echo:") {
                return last;
            }
        }
        last
    }
}

fn spawn(url: &str, profile_dir: &Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("9")
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
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    TabChild { child, stdin, events, seen: Vec::new() }
}

/// FINDING 5 — a page reads the body of a cross-origin response. The victim
/// sends no `Access-Control-Allow-Origin`; the browser never asks for one.
#[test]
fn f5_a_foreign_page_reads_a_cross_origin_response_body() {
    let port = spawn_server();
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("http://localhost:{port}/steal");
    let mut tab = spawn(&url, dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    let title = tab.last_title();
    assert!(
        !title.contains(SECRET),
        "a page on http://localhost:{port} read http://127.0.0.1:{port}'s body \
         (no same-origin check on responses); title was {title:?}"
    );
}

/// FINDING 5b — and with the container's cookie jar attached to that request:
/// the foreign page reads the victim's session cookie out of the echo.
#[test]
fn f5b_a_foreign_page_reads_a_credentialed_cross_origin_response() {
    let port = spawn_server();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut tab = spawn(&format!("http://127.0.0.1:{port}/grant"), dir.path());
    tab.send(ToTab::Resize { width: 800, height: 600, scale: 1.0 });
    tab.collect(true); // the granting visit, so the jar holds the victim's cookie

    tab.send(ToTab::Navigate {
        url: format!("http://localhost:{port}/steal2"),
    });
    let title = tab.last_title();
    assert!(
        !title.contains(SESSION),
        "a page on http://localhost:{port} read the session cookie the victim \
         host set for itself; title was {title:?}"
    );
}
