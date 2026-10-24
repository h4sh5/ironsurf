//! Per-container **private browsing** flag (task: private browsing) — the
//! registry half: the flag persists in `containers.json` like `user_agent`
//! and `proxy`, every container starts off, toggling one container leaves
//! the others untouched, and an unknown id is rejected without writing.

use ironsurf_common::ContainerId;
use ironsurf_storage::{Store, StoreError};

fn open(root: &std::path::Path) -> Store {
    Store::open(root).expect("open store")
}

fn private(store: &Store, id: ContainerId) -> bool {
    store.container(id).expect("container exists").private
}

#[test]
fn set_private_round_trips_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    assert!(!private(&store, id), "a fresh container does not browse privately");

    store.set_container_private(id, true).unwrap();
    assert!(private(&store, id));
    drop(store);
    assert!(private(&open(tmp.path()), id), "the flag lives in containers.json");

    store_on(tmp.path()).set_container_private(id, false).unwrap();
    assert!(!private(&open(tmp.path()), id), "and clearing it persists too");
}

fn store_on(root: &std::path::Path) -> Store {
    open(root)
}

#[test]
fn new_and_seeded_containers_start_not_private() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    assert!(!private(&store, store.default_container_id()), "the seeded personal container");
    let lab = store.create_container("lab", "#aa00ff").unwrap();
    assert!(!private(&store, lab), "a created container starts normal");
    drop(store);
    assert!(!private(&open(tmp.path()), lab));
}

#[test]
fn toggling_one_container_leaves_the_others_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let a = store.create_container("a", "#ff0000").unwrap();
    let b = store.create_container("b", "#00ff00").unwrap();
    store.set_container_private(a, true).unwrap();
    assert!(private(&store, a));
    assert!(!private(&store, b));
    assert!(!private(&store, store.default_container_id()));

    // Independent flips, both directions, each its own line on disk.
    store.set_container_private(b, true).unwrap();
    store.set_container_private(a, false).unwrap();
    drop(store);
    let store = open(tmp.path());
    assert!(!private(&store, a));
    assert!(private(&store, b));
}

#[test]
fn unknown_container_is_rejected_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let ghost = ContainerId(999);
    let err = store.set_container_private(ghost, true).unwrap_err();
    assert!(matches!(err, StoreError::NoSuchContainer(id) if id == ghost));
    // The file still holds only the seeded containers, all normal.
    drop(store);
    let store = open(tmp.path());
    assert!(store.containers().iter().all(|c| !c.private));
}

#[test]
fn a_legacy_containers_json_loads_with_the_flag_off() {
    let tmp = tempfile::tempdir().unwrap();
    // A file written before the private field existed (proxy-era layout).
    std::fs::write(
        tmp.path().join("containers.json"),
        r##"[{"id":1,"name":"personal","color":"#ff5500","is_default":true,"user_agent":null,"proxy":"socks"}]"##,
    )
    .unwrap();
    let store = open(tmp.path());
    assert!(!private(&store, store.default_container_id()), "legacy = not private");
    assert_eq!(
        store.container(store.default_container_id()).unwrap().proxy.as_deref(),
        Some("socks"),
        "and the other fields still load"
    );
}
