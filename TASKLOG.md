# TASKLOG

- **Security audit (high severity) — done.** Report: `docs/SECURITY_AUDIT.md`.
  18 PoC tests assert the secure behaviour and fail today: 6 findings, top two
  CRITICAL: (1) a web page reads the local disk via `location.href="file://…"`
  (no scheme gate in `follow_pending_js_nav`); (2) `location.href="ironsurf://
  archive?root=%2Fetc&host=0.0.0.0&port=…"` makes the shell export any folder
  over HTTP (runtime.rs:1262 → app.rs:3980 → normalize_root/host). Also: archive
  control socket = unauthenticated GET mutations; `percent_decode` char-boundary
  panic (path decoded twice) kills the server; no CORS on responses (cross-origin
  body + session-cookie read); no private-address rule; `container-id` unused.
- **Audit fixes (round 1).** Fixed findings 1–4 of `docs/SECURITY_AUDIT.md`: scheme
  policy for page navigations + `nav_from_shell` (tab); archive overrides only from the
  address bar, loopback-only bind, system folders refused (shell/shell); session-token +
  Origin authorization on the control socket's `/apply` and `/stop`, no token or control
  port on the served origin; byte-safe `percent_decode`. Findings 5 (CORS/opaque responses
  in `netmod.rs`, private-address rule) and 6 (`container-id`) stay open, their PoCs red.
  `cargo test -p ironsurf-shell` green (156 + 8 audit), `-p ironsurf-tab --bins` 366 green.

