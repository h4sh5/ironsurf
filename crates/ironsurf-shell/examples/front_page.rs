//! Render the archive front page for a folder and print it (visual check of
//! the pre-traversed timeline). Not part of the app.
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: front_page <root>"));
    let state = ironsurf_shell::archive_server::PageState {
        spec: ironsurf_shell::archive_server::Spec {
            root,
            host: "127.0.0.1".into(),
            port: 8123,
        },
        serving: Some("http://127.0.0.1:8123".into()),
        control: Some("http://127.0.0.1:9001".into()),
        // The preview stands in for the control page, which is the only render
        // that carries a session token.
        token: "preview-token".into(),
    };
    let resp = ironsurf_shell::archive_server::route_request("GET", "/", "", &state);
    print!("{}", String::from_utf8_lossy(&resp.body));
}
