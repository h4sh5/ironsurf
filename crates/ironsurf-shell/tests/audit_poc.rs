//! AUDIT proofs for the shell's **archive browsing server**
//! (`ironsurf-shell/src/archive_server.rs`).
//!
//! Every test asserts the behaviour a secure server shows, so **each one fails
//! today** and the failure message is the evidence. They are written to double
//! as regression tests for the fixes. Headless: localhost sockets only, no
//! display, no Slint window. See docs/SECURITY_AUDIT.md.
//!
//! * F1  — a token-less, `Origin`-less request (what a cross-origin `<img>`
//!   can send) re-points the served folder at any directory on disk;
//! * F1b — `root=/` is accepted: the whole filesystem is served;
//! * F1c — `host=0.0.0.0` is accepted and bound: the archive is reachable
//!   from the network with no authentication;
//! * F2  — `percent_decode` panics on `%` + hex + a multibyte character;
//! * F2b — reached through a request path: the path is decoded **twice**, so a
//!   name the directory listing itself links to kills the serve thread and the
//!   archive stops serving for the session;
//! * F3  — `apply` also answers **GET**, so a bare `<img>`/`no-referrer` link
//!   is a complete one-request attack;
//! * F4  — the control socket is a plain HTTP server with no CSRF token, no
//!   `Sec-Fetch-Site` check and no same-origin guard on any of its mutations.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use ironsurf_shell::archive_server::{self, ServeEvent, ServeHandle, Spec};

fn sandbox(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ironsurf-audit-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("sandbox");
    dir
}

/// One request on a raw socket: what a page's `fetch()` / `<img>` / top-level
/// navigation puts on the wire once `url` has encoded the target.
fn http_get(addr: &str, path_and_query: &str) -> String {
    match TcpStream::connect(addr) {
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream
                .write_all(format!("GET {path_and_query} HTTP/1.1\r\n\r\n").as_bytes());
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        }
        // Refused/unreachable is a legitimate (and, here, desired) answer.
        Err(err) => format!("<<not listening: {err}>>"),
    }
}

/// Percent-encode everything a URL path may not carry raw — the encoding a
/// browser produces for the same target.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn up(root: PathBuf) -> ServeHandle {
    let (tx, _rx) = mpsc::channel::<ServeEvent>();
    ServeHandle::start(Spec { root, host: "127.0.0.1".into(), port: 0 }, tx).expect("start")
}

fn serve_addr_of(handle: &ServeHandle) -> SocketAddr {
    handle
        .address
        .as_ref()
        .expect("a bound address")
        .trim_start_matches("http://")
        .parse()
        .expect("address")
}

/// F1 — the control socket takes a folder change from a request that carries
/// no token, no `Origin` and no confirmation, and an `<img>` tag can send
/// exactly that. The folder the server exports afterwards is any directory on
/// the machine.
#[test]
fn f1_a_cross_origin_request_cannot_repoint_the_served_folder() {
    let served = sandbox("served");
    let secret = sandbox("secret");
    std::fs::write(secret.join("id_rsa"), b"PRIVATE KEY MATERIAL").unwrap();
    std::fs::write(served.join("page.html"), b"<p>archived</p>").unwrap();
    let handle = up(served);
    let serve = serve_addr_of(&handle);
    let control = handle.control_addr.to_string();

    // No cookies, no Origin, no token: an `<img>` cannot send any of them, and
    // the 302 response is never read — the mutation alone is the attack.
    let reply = http_get(
        &control,
        &format!("/ironsurf/serve/apply?root={}", enc(secret.to_str().unwrap())),
    );
    std::thread::sleep(Duration::from_millis(200));
    let body = http_get(&serve.to_string(), "/id_rsa");
    handle.shutdown();
    assert!(
        !body.contains("PRIVATE KEY MATERIAL"),
        "a token-less GET re-pointed the archive server at another folder and \
         served a secret from it.\napply reply: {reply}\nserve reply: {body}"
    );
}

/// F1b — `root` is any absolute path (and is *created* when missing), so the
/// same single request serves the whole filesystem, or plants directories
/// anywhere the shell's user can write.
#[test]
fn f1b_root_cannot_escape_the_archive_folder() {
    let handle = up(sandbox("rooted"));
    let serve = serve_addr_of(&handle);
    let control = handle.control_addr.to_string();
    let outside = sandbox("planted").join("a").join("b").join("c");

    http_get(&control, "/ironsurf/serve/apply?root=%2F");
    std::thread::sleep(Duration::from_millis(200));
    let hostname = http_get(&serve.to_string(), "/etc/hostname");
    let planting = http_get(
        &control,
        &format!("/ironsurf/serve/apply?root={}", enc(&outside.to_string_lossy())),
    );
    handle.shutdown();
    assert!(
        !hostname.starts_with("HTTP/1.1 200"),
        "root=%2F escaped the archive folder: /etc/hostname was served back \
         (HTTP/1.1 200 with the file body). /etc/hostname reply: {hostname}; \
         directory-planting reply: {planting}"
    );
}

/// F1c — the served host is not restricted to loopback: `0.0.0.0` is accepted
/// and bound, so the archive (and, after F1b, the filesystem) is reachable
/// from other machines with no authentication.
#[test]
fn f1c_host_cannot_widen_past_loopback() {
    let handle = up(sandbox("wide"));
    let control = handle.control_addr.to_string();
    let free = 21_500 + (std::process::id() % 900) as u16;
    http_get(&control, &format!("/ironsurf/serve/apply?host=0.0.0.0&port={free}"));
    std::thread::sleep(Duration::from_millis(250));
    let wide = http_get(&format!("127.0.0.1:{free}"), "/ironsurf/serve");
    handle.shutdown();
    assert!(
        !wide.starts_with("HTTP/1.1 200") && !wide.starts_with("HTTP/1.1 302"),
        "host=0.0.0.0 was accepted and bound: the archive server answers on \
         every interface, port {free}. Reply: {}",
        wide.chars().take(80).collect::<String>()
    );
}

/// F2 — `percent_decode` slices a `String` at byte offsets taken from the
/// input, so `%` + a hex digit followed by a multibyte character panics
/// (out of bounds / off the char boundary).
#[test]
fn f2_percent_decode_survives_a_malformed_escape_before_a_multibyte_char() {
    let r = std::panic::catch_unwind(|| archive_server::percent_decode("%a\u{e9}"));
    assert!(
        r.is_ok(),
        "percent_decode slices at an unvalidated byte index: the four-byte \
         input percent-a + U+00E9 panics"
    );
}

/// F2b — the same panic on the path the server actually serves: the request
/// path is percent-decoded in `parse_request_head` and decoded **again** per
/// segment inside `resolve_inside`, so the ordinary wire form of a name like
/// `%aé` — which the directory listing itself links to — aborts the request
/// and kills the serve thread. The archive then serves nothing for the rest
/// of the session: one request, one click, permanent (until restart) DoS.
#[test]
fn f2b_one_listed_file_name_cannot_stop_the_server() {
    let root = sandbox("panic-root");
    std::fs::write(root.join("%a\u{e9}"), b"planted").expect("plant");
    let handle = up(root);
    let serve = serve_addr_of(&handle);

    // The href the listing emits for that name is what a browser sends.
    let once = archive_server::percent_decode("%25a%C3%A9");
    assert_eq!(once, "%a\u{e9}", "the wire form decodes to the file name");
    let first = http_get(&serve.to_string(), "/%25a%C3%A9");
    let after = http_get(&serve.to_string(), "/");
    handle.shutdown();
    assert!(
        first.starts_with("HTTP/1.1") && after.starts_with("HTTP/1.1 200"),
        "the twice percent-decoded path aborted the connection and then killed \
         the server: first reply {:?}, later reply {:?}",
        first.chars().take(40).collect::<String>(),
        after.chars().take(40).collect::<String>()
    );
}

/// F3 — the mutations answer **GET**, so a bare navigable URL (an `<img>`, a
/// link, a redirect chain, a `no-referrer` context) is a complete one-request
/// attack: no form, no script, no CORS preflight.
#[test]
fn f3_server_mutations_are_not_post_only() {
    let served = sandbox("get-served");
    let target = sandbox("get-target");
    std::fs::write(target.join("x.txt"), b"after the change").unwrap();
    let handle = up(served);
    let control = handle.control_addr.to_string();
    let serve = serve_addr_of(&handle);
    let reply = http_get(
        &control,
        &format!("/ironsurf/serve/apply?root={}", enc(target.to_str().unwrap())),
    );
    std::thread::sleep(Duration::from_millis(200));
    let body = http_get(&serve.to_string(), "/x.txt");
    handle.shutdown();
    assert!(
        !body.contains("after the change"),
        "a plain GET performed a server mutation (no method restriction, no \
         token, no Origin check): reply {} / body {}",
        reply.chars().take(90).collect::<String>(),
        body.chars().take(90).collect::<String>()
    );
}

/// F4 — nothing on the control socket distinguishes the user's own control
/// page from a request another site arranged: no CSRF token, no `Origin` /
/// `Sec-Fetch-Site` requirement, and the loopback bind is the only guard.
#[test]
fn f4_the_control_socket_requires_an_authenticating_header() {
    let handle = up(sandbox("csrf"));
    let control = handle.control_addr.to_string();
    // A request as any cross-origin page can send it: no cookie (the socket
    // sets none), no `Origin` an <img> could add, `Sec-Fetch-Site: cross-site`.
    let reply = http_get(&control, "/ironsurf/serve/apply?root=%2Fetc&port=4322");
    handle.shutdown();
    assert!(
        !reply.contains("302"),
        "the control socket accepted a cross-site request with nothing to \
         authenticate it: {reply}"
    );
}

/// F1+ — the whole drive-by chain, shell half. `FromTab::ArchiveServerStart`
/// carries `root`/`host`/`port` parsed from the page's own
/// `ironsurf://archive?…` URL (proved in
/// `ironsurf-tab/tests/audit_poc_nav.rs`: a page's `location.href` produces
/// exactly that message with the page's values), and the shell feeds them to
/// `spec_from` → `normalize_root`/`normalize_host` → `set_spec` with **no
/// confirmation**. This is what the shell ends up serving.
#[test]
fn f1c_a_page_chooses_which_folder_the_browser_exports_and_where_it_listens() {
    // Exactly what the tab forwarded from the page's URL.
    let spec = archive_server::spec_from(
        Some("/etc"),
        sandbox("chain-default"),
        Some("0.0.0.0".to_string()),
        Some(0),
    );
    // FIXED: neither the folder nor the interface a page named survives
    // `spec_from` — the host is pinned to loopback (the served folder has no
    // authentication at all) and a system folder is refused, so the spec falls
    // back to the configured archive root.
    assert_ne!(
        (spec.root.to_str().unwrap(), spec.host.as_str()),
        ("/etc", "0.0.0.0"),
        "the page's root/host were accepted as the server's spec"
    );
    assert_eq!(spec.host, "127.0.0.1", "only loopback may serve the archive");
    assert!(
        !spec.root.starts_with("/etc"),
        "a system folder must not become the served root: {:?}",
        spec.root
    );
    // And it really does serve it: bind on loopback with that root and read a
    // file outside any archive folder.
    let (tx, _rx) = mpsc::channel::<ServeEvent>();
    let mut spec = spec;
    spec.host = "127.0.0.1".into();
    let handle = ServeHandle::start(spec, tx).expect("start with the page's root");
    let serve = serve_addr_of(&handle);
    let body = http_get(&serve.to_string(), "/hostname");
    handle.shutdown();
    assert!(
        !body.starts_with("HTTP/1.1 200"),
        "the folder the page named is exported over HTTP: /etc/hostname read \\
         from the browser's own server: {}",
        body.chars().take(120).collect::<String>()
    );
}
