# IronSurf security audit — high-severity findings

Scope: `ironsurf-tab` (engine/IPC/JS/network), `ironsurf-shell` (window, archive
server, downloads, IPC dispatch), `ironsurf-storage` (cookies, localStorage,
history), `ironsurf-sandbox`. Method: read every security-relevant path
(`file:`/`data:`/`blob:` handling, cookie jar, storage keys, download naming,
sandbox limits), then prove findings by running the real binaries — the tab over
its stdio protocol headless, the archive server against loopback sockets.

**How to read the PoCs.** Every test in `crates/ironsurf-shell/tests/audit_poc.rs`,
`crates/ironsurf-shell/src/archive_server.rs` (`mod tests`),
`crates/ironsurf-tab/tests/audit_poc_nav.rs`, `.../audit_poc_sop.rs` and
`.../audit_poc_archive.rs` asserts the behaviour a secure browser shows. Each
**fails today**, and the failure message is the evidence. They are written to be
inverted into regression tests by the fixes.

```
cargo test -p ironsurf-shell --test audit_poc        # 8 failures: findings 1,2,3,4
cargo test -p ironsurf-shell --lib                   # 3 failures: findings 2,3,4
  archive_server::tests::an_archive_page_can_start_the_server_change_without_a_token
  archive_server::tests::the_served_origin_discloses_the_control_address
  archive_server::tests::a_twice_decoded_name_kills_the_path_resolver
cargo test -p ironsurf-tab  --test audit_poc_nav     # 4 failures: findings 1,2,5
cargo test -p ironsurf-tab  --test audit_poc_sop     # 2 failures: finding 6
cargo test -p ironsurf-tab  --test audit_poc_archive # 1 failure:  finding 2 (tab half)
```

18 failing PoC tests in total, each naming the finding it proves. The four
`audit_poc_nav` tests that pass (`f1b`, `f1c`, `f1e`, `f1f`) record what is
**not** broken at the `file:` boundary and are listed under "what held up".

Everything else in the workspace suite is green (`-p ironsurf-tab --bins`: 366
pass; `-p ironsurf-storage`, `-p ironsurf-shell --lib` pass apart from the
findings above).

---

## Fix status — this round

| # | Finding | State | What changed |
|---|---|---|---|
| 1 | web page reads the local disk (`file:` navigation) | **FIXED** | `Tab::document_may_reach` (`ironsurf-tab/src/runtime.rs`) is the scheme policy for page-initiated navigation: `http(s)`/`about`/`data`/`blob` anywhere, `file:` only from a local document, `ironsurf:` never by following a page. `nav_from_shell` records the one navigation the user typed. |
| 2 | page re-points the archive server | **FIXED** | Tab: `ask_archive_server` forwards `root`/`host`/`port` only when `nav_from_shell` (address bar), including for `<form action="ironsurf://archive?…">`; a page-driven `ironsurf://` navigation no longer reaches the shell at all. Shell: `normalize_host` pins the bind to loopback and `normalize_root` refuses system folders **before** creating anything. |
| 3 | control socket: unauthenticated mutations | **FIXED** | Per-process session token (`session_token`), embedded in the control page's form and required by `/apply` and `/stop` (`mutation_authorized`), which also rejects a request whose `Origin`/`Referer` is not the control socket's. The served origin no longer defers an untokened change (403) and never prints the token or the control port. |
| 4 | `percent_decode` panic / one-request DoS | **FIXED** | Decode is byte-driven with `hex_digit` validation and `from_utf8_lossy` at the end; no `str` slicing at input-chosen offsets. The listed name `%aé`, its wire form and the twice-decoded path all survive. |
| 5 | no same-origin policy on responses; no private-address rule | **OPEN** | Needs `Origin` threaded into the fetch path and opaque cross-origin responses in `netmod.rs` (+ preflight). PoCs `audit_poc_sop::f5`, `f5b` and `audit_poc_nav::f4` still fail — treat them as the spec for the fix. |
| 6 | `container-id` parsed, never used | **OPEN** | Small: validate the profile dir against the container id (a marker file the shell writes) so a mis-wired launch fails closed. |

Verification of this round: `cargo test -p ironsurf-shell` → 156 lib + 8 integration
tests green **including all 8 audit PoCs**; `cargo test -p ironsurf-tab --bins` →
366 green (no regressions from the navigation policy); `audit_poc_nav` 7/8 and
`audit_poc_archive` 1/1 green. Two project tests and one example were updated to
the new control-page contract (the form carries the session token), and
`ARCHITECTURE`'s "both addresses show the same page" is kept with the token and
control port withheld from the served origin.

## 1. CRITICAL — a web page reads the local disk (`file:` from a remote document)

A page on any origin navigates itself to a `file:` URL and the tab fetches and
renders it. No user gesture, no prompt, no "local files are only reachable from
local documents" rule: `location.href = "file:///home/user/…"`, and the file's
own text lands in the tab (proven: the local file's `<title>` came back).

* `ironsurf-tab/src/runtime.rs:2360` `follow_pending_js_nav` applies a page's
  pending navigation with **no scheme gate**; it resolves the target against the
  document and calls `navigate_post` like any other navigation.
* `ironsurf-tab/src/netmod.rs` answers `file:` locally for the document load, and
  the redirect allow-list `(http|https, file) => Unauthorized` only blocks the
  *server's* redirect, not a script's `location.href`.
* PoC: `crates/ironsurf-tab/tests/audit_poc_nav.rs` → `f3_…` (navigation
  happened, `UrlChanged(file://…)`), `f1d_…` (**content rendered**: the local
  document's title appeared).

Impact: arbitrary local file disclosure by visiting a page — source code,
dotfiles, `/etc`, a `.pem`, anything the browser user can read. `file:`
documents also execute script (`f1e_…` printed `RAN:2`), so any HTML the user
has ever downloaded becomes a privileged document the moment anything navigates
to it, and the tab's own sandbox does not stop the read (Landlock confines writes to the profile dir; reads of the user's files are allowed).

Fix: treat `file:` as unreachable from a non-`file:` document — drop the pending
navigation when `!same_scheme_allowance(document_url, target)` in
`follow_pending_js_nav` (and `Window::open`/`<a target>`), reuse the existing
`redirect_authorizes` rule that already forbids `http → file`; only a URL the
user typed in the shell's own address bar may open a local file.

## 2. CRITICAL — a web page reconfigures the browser's file server

`ironsurf://archive?root=…&host=…&port=…` renders an internal page whose query
parameters are forwarded to the shell as `FromTab::ArchiveServerStart`, and the
shell applies them ("an override on the URL wins over the saved settings"). A
remote page's own `location.href` reaches it — proven against the real tab:

```
location.href = "ironsurf://archive?root=%2Fetc&host=0.0.0.0&port=4321";
→ FromTab::ArchiveServerStart { root: Some("/etc"), host: Some("0.0.0.0"), port: Some(4321) }
```

* `ironsurf-tab/src/runtime.rs:1262` forwards the page's `root`/`host`/`port`.
* `ironsurf-shell/src/app.rs:3980` `start_archive_server` →
  `archive_server_apply` with no confirmation.
* `ironsurf-shell/src/archive_server.rs:186` `normalize_root` accepts **any**
  absolute path (`/etc`, `/`, `~/…`) and *creates* missing directories;
  `:134` `normalize_host` accepts `0.0.0.0`.
* PoCs: `audit_poc_nav.rs` → `f2_…` (tab half); `ironsurf-shell/tests/audit_poc.rs`
  → `f1c_…` (the folder the page named is served: `/etc/hostname` read from the
  browser's own server), `f1b_…` (`root=/` → whole filesystem), `f1c_host…`
  (`host=0.0.0.0` bound and answering).

Impact: one page visit turns the browser into an unauthenticated HTTP file
server exporting any directory the user can read, on an interface and port the
page chose — remote exfiltration without any further user action, and a
directory-planting primitive (`root=/srv/…` creates paths).

Fix: never take server configuration from a URL. Ignore `root`/`host`/`port` on
navigations that did not originate in the shell's own UI (require
`is_user_trigger` + the archive page being the shell's page), pin the host to
loopback in `normalize_host`, restrict `root` to a configured allow-list of
archive roots, and put a modal confirmation in front of any change.

## 3. HIGH — the archive control socket answers unauthenticated mutations over GET

The control socket is a second HTTP server that mutates the file server:
`GET /ironsurf/serve/apply?root=…&host=…&port=…`. It sends no cookie and accepts
requests with no token, no `Origin`, no `Sec-Fetch-Site`, and `apply` inherits no
CSRF guard (only the page's *own* settings form checks a token). A cross-origin
`<img src=…>`, a link, or a redirect chain is therefore a complete attack, and the
`/ironsurf/serve` page discloses the control address on the served origin.

* `archive_server.rs:2662` builds the forward URL from `(path, query)` alone;
  `:2684` only special-cases `token`; `:470` `build_control_response` has no
  origin/token check for the other control paths.
* PoCs: `audit_poc.rs` → `f1_…` (a token-less GET re-points the served folder and
  a secret from another folder comes back), `f3_…` (GET alone mutates), `f4_…`
  (a cross-site request with nothing to authenticate it is accepted).

Fix: require a per-session token (or `Sec-Fetch-Site: same-origin` + a
nonce-bearing POST) on every control mutation, ignore the query when the origin
is not the control page's own, and stop printing the control address on the
served origin.

## 4. HIGH — panic in `percent_decode`, reachable twice over, kills the archive server

`archive_server.rs:228` slices `&str` with byte indices taken from the input:
`u8::from_str_radix(&s[i + 1..i + 3], 16)` never checks that `i + 3` is a char
boundary, so `%` + hex + a multibyte character panics
(`percent_decode("%a\u{e9}")` → *"end byte index 3 is not a char boundary; it is
inside 'é'"*). It is memory-safe (Rust aborts, no UB) but it is a **service
killing input**: the request path is decoded in `parse_request_head` **and again**
per segment inside `resolve_inside`, so the ordinary wire form of a file name the
directory listing itself links (`%25a%C3%A9` → `%aé` → panic) aborts the request
and takes down the serve thread. Afterwards the archive serves nothing at all for
the rest of the session — one request, permanent DoS.

* PoCs: `audit_poc.rs` → `f2_…` (the four-byte input panics), `f2b_…` (the listed
  name aborts the connection and then `Connection refused` — the server is dead);
  `archive_server.rs` `mod tests` → `a_twice_decoded_name_kills_the_path_resolver`.

Fix: decode over bytes and validate UTF-8 at the end (`from_utf8` on the
accumulator), never slice the `str`; decode the request path **once**; run each
connection on a worker whose panic cannot take the accept loop with it.

## 5. MEDIUM/HIGH — no private-address or user-gating on navigations at all

`follow_pending_js_nav` has no scheme allow-list and no user-activation
requirement, and no private-address rule applies to fetches: a page on
`http://127.0.0.1:x` successfully issues `fetch("http://127.0.0.1:9/…")` (PoC
`audit_poc_nav.rs` → `f4_…`, request attempted with the tab's own network stack).
Together with §2 and §3 this is how a page discovers and drives the browser's own
loopback services.

Fix: gate JS navigations to `http(s)` (+ `data:` where intended), and refuse
loopback/link-local/site-local destinations for web content (only the browser's
own UI may talk to localhost).

## 6. HIGH — the same-origin policy is not enforced on responses

`XMLHttpRequest`/`fetch` hand page code the whole envelope — status, headers,
body — whatever the response's origin. Nothing sends `Origin`, nothing consults
`Access-Control-Allow-Origin`, nothing checks the destination. Proven against the
real tab, with two host names for one server (`localhost` vs `127.0.0.1` are
distinct origins):

* `audit_poc_sop.rs` → `f5_…`: a page on `http://localhost:P` read the body of
  `http://127.0.0.1:P/victim` → title `got:200:CROSS-ORIGIN-BANK-STATEMENT`.
* `audit_poc_sop.rs` → `f5b_…`: the same request carried the container's cookie
  jar, and the page read the victim host's **own session cookie** back out of an
  echo endpoint → title `echo:sess=IB-leak-me`.

Impact: any page reads any site's authenticated responses — session tokens, CSRF
tokens, private API payloads — and the browser becomes a proxy into the user's
LAN (see §5). `HttpOnly` does not save a session once the body that echoes it is
readable.

Fix: enforce CORS in the tab before handing bytes to page code: send `Origin` on
every request, and for a cross-origin response return an opaque error unless
`Access-Control-Allow-Origin` matches (plus credentials rules, and the preflight
for non-simple requests). `js.rs` already knows the document origin — the check
belongs where the envelope is built in `netmod.rs`.

## 7. MEDIUM — `container-id` is ignored, so container isolation is the profile dir alone

`TabArgs::container_id` is parsed and never used (`crates/ironsurf-tab/src/main.rs`);
the only thing separating two containers is the `--profile-dir` path the shell
hands the tab, and the tab's `LocalStorage`/cookie jar are opened straight from
it. A tab launched with the wrong (or a shared) profile dir silently shares
cookies, `localStorage` and history with another container, and the sandbox policy
is derived from the same single path (`runtime.rs:386`, `:405`).

Fix: send the container id on the wire and have the tab refuse a profile dir that
does not belong to its container (a marker file written by the shell), so a
mis-wired launch fails closed instead of sharing.

---

## What held up under testing

Worth recording, because these are the paths an attacker tries first:

* **Path traversal in the archive server**: `resolve_inside` canonicalizes and
  compares components, including the ancestor-symlink case (`ln -s /` as a
  directory entry stays inside the archive). No `..`, no absolute-path escape.
* **Cookie attributes**: `HttpOnly`, `Secure` (https-only set *and* send),
  `Domain` suffix rules, IP hosts never accepting `Domain=`, `Max-Age`/`Expires`
  precedence and delete-on-expiry are all implemented per RFC 6265, and
  `document.cookie` filters by domain, path, expiry, `Secure` and hides
  `HttpOnly`.
* **`localStorage` origin key**: `scheme://host:port` (lowercased host, default
  port filled in), so `HTTP://EXAMPLE.COM` and `http://example.com` share one
  area as they should, `file:`/`data:` get none, and the store lives inside the
  profile dir.
* **Download names**: `sanitize_file_name` strips separators, control characters
  and leading dots (so `..` cannot survive), and paths are confined to the
  download root — `../../x` stays inside.
* **The `file:` boundary is not an all-origin reader**: a document loaded from
  disk does run script (`audit_poc_nav.rs` → `f1e_…`, title `RAN:2`), but it did
  **not** manage to read another file with it (`f1c_…`), and a `file:` document
  fetching another origin's cookie-bearing response came back empty
  (`f1f_…`), as did a web page fetching a `file:` URL (`f1b_…`) — so §1 is
  disclosure-by-rendering, not a scripted disk slurp. Fix it anyway: it is the
  disclosure that matters, and the reading rules here are inconsistent rather
  than deliberately narrow.
* **Memory safety**: first-party `unsafe` is confined to `ironsurf-sandbox`
  (`prctl`/`setrlimit`/`landlock`/`getrlimit` calls with scalar arguments) and one
  `libc::poll` in the archive server; no hand parsing of untrusted bytes through
  raw pointers. The one real defect of this class (§4) fails closed as a panic,
  not as memory corruption.
* **Sandbox**: `PR_SET_NO_NEW_PRIVS`, `PR_SET_PDEATHSIG`, `RLIMIT_NOFILE`/`NPROC`,
  and a Landlock ruleset with exactly one read-write path (the container profile
  dir) — degraded loudly to rlimits-only when Landlock is unavailable. Note the
  consequence of §1: a sandbox that lets the *process* read the disk cannot undo a
  `file:` policy gap, since the policy gap runs inside the tab.

## Recommended order of work

1. §1 and §2 — both are single-page-visit, no-consent, remote-triggerable, and
   each gives the other more reach (read the disk *through* the browser, or
   export it over HTTP).
2. §6 — the same-origin check, since it converts every loopback and cross-site
   endpoint into readable data.
3. §3 + §4 — authenticate the control socket and make the decoder non-panicking
   and single-pass.
4. §5 (navigation/private-address policy), §7 (container binding).
