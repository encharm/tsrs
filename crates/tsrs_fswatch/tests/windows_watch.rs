// The ReadDirectoryChangesW backend (windows.rs): the events a watch delivers for creating, changing and deleting
// files, in a directory and below it, the end of a watch whose directory is deleted, and that closing a watch
// releases the directory.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tsrs_fswatch::{ErrWatchTerminated, EventKind, windows, with_recursive};

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("tsrs-fswatch-windows-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Default)]
struct Seen {
    // (kind, path) of every event, in order
    events: Vec<(EventKind, String)>,
    errors: Vec<tsrs_fswatch::Error>,
}

type Shared = Arc<Mutex<Seen>>;

fn watch(dir: &Path, recursive: bool) -> (Shared, Box<dyn tsrs_fswatch::Watch>) {
    let seen: Shared = Arc::default();
    let s = Arc::clone(&seen);
    let opts = if recursive { vec![with_recursive()] } else { Vec::new() };
    let w = windows()
        .watch_directory(
            dir.to_str().unwrap(),
            Arc::new(move |events, err| {
                let mut s = s.lock().unwrap();
                s.events.extend(events.into_iter().map(|e| (e.kind, e.path)));
                s.errors.extend(err);
            }),
            opts,
        )
        .expect("watch_directory");
    (seen, w)
}

fn wait_until(seen: &Shared, what: &str, done: impl Fn(&Seen) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if done(&seen.lock().unwrap()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let s = seen.lock().unwrap();
    panic!("timed out waiting for {what}; events {:?}, errors {:?}", s.events, s.errors);
}

fn has(seen: &Seen, kind: EventKind, path: &Path) -> bool {
    let path = path.to_str().unwrap();
    seen.events.iter().any(|(k, p)| *k == kind && p.eq_ignore_ascii_case(path))
}

#[test]
fn windows_backend_is_the_default_and_available() {
    assert!(windows().available());
    assert_eq!(tsrs_fswatch::default().name(), "windows");
}

#[test]
fn creating_changing_and_deleting_a_file() {
    let dir = temp_dir("files");
    let (seen, w) = watch(&dir, false);
    let file = dir.join("a.ts");

    std::fs::write(&file, "export const a = 1;\n").unwrap();
    wait_until(&seen, "the create", |s| has(s, EventKind::Update, &file));

    seen.lock().unwrap().events.clear();
    std::fs::write(&file, "export const a = 2;\n").unwrap();
    wait_until(&seen, "the change", |s| has(s, EventKind::Update, &file));

    seen.lock().unwrap().events.clear();
    std::fs::remove_file(&file).unwrap();
    wait_until(&seen, "the delete", |s| has(s, EventKind::Delete, &file));

    w.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_recursive_watch_sees_a_subdirectory() {
    let dir = temp_dir("recursive");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let (seen, w) = watch(&dir, true);
    let file = dir.join("src").join("b.ts");

    std::fs::write(&file, "export const b = 1;\n").unwrap();
    wait_until(&seen, "the nested create", |s| has(s, EventKind::Update, &file));

    w.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_non_recursive_watch_ignores_a_subdirectory() {
    let dir = temp_dir("flat");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let (seen, w) = watch(&dir, false);
    let nested = dir.join("src").join("c.ts");
    let marker = dir.join("marker.ts");

    std::fs::write(&nested, "export {};\n").unwrap();
    std::fs::write(&marker, "export {};\n").unwrap();
    wait_until(&seen, "the marker", |s| has(s, EventKind::Update, &marker));
    assert!(!has(&seen.lock().unwrap(), EventKind::Update, &nested));

    w.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn deleting_the_watched_directory_ends_the_watch() {
    let parent = temp_dir("root-parent");
    let dir = parent.join("watched");
    std::fs::create_dir_all(&dir).unwrap();
    let (seen, w) = watch(&dir, true);

    std::fs::remove_dir_all(&dir).unwrap();
    wait_until(&seen, "the terminating error", |s| {
        s.errors.iter().any(|e| e.is(ErrWatchTerminated))
    });

    let _ = w.close();
    std::fs::remove_dir_all(&parent).unwrap();
}

#[test]
fn closing_a_watch_releases_the_directory() {
    let dir = temp_dir("close");
    let (_seen, w) = watch(&dir, true);
    w.close().unwrap();
    // the directory handle is closed: the directory can be deleted, and watched again once it exists
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let (seen, w) = watch(&dir, false);
    let file = dir.join("again.ts");
    std::fs::write(&file, "export {};\n").unwrap();
    wait_until(&seen, "the create after re-watching", |s| has(s, EventKind::Update, &file));
    w.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
