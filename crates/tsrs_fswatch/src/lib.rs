// Port of Go `internal/fswatch` (itself a Go port of @parcel/watcher): in-process filesystem watching with
// FSEvents and kqueue (macOS / BSD), inotify (Linux) and ReadDirectoryChangesW (Windows). Not ported:
// `fanotify_linux.go` (Linux uses inotify, the same fallback Go takes on kernels without fanotify), `walkdir_windows.go`
// and `walkDirGeneric` (only the backends that walk a tree themselves call walkDir, and the Windows one watches
// subtrees natively).

mod canonicalize;
mod debounce;
mod event;
#[cfg(target_os = "macos")]
mod fsevents_darwin;
#[cfg(target_os = "macos")]
mod fsevents_darwin_ffi;
#[cfg(target_os = "linux")]
mod inotify_linux;
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
mod kqueue;
mod pathcompare;
mod pathkey;
#[cfg(unix)]
mod walkdir_unix;
mod watcher;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod eventlist_test;
#[cfg(test)]
mod testutil_test;
#[cfg(test)]
mod watcher_test;

pub use canonicalize::path_comparer_for_path;
pub use event::{Event, EventKind};
pub use pathkey::{NativePathComparisonAvailable, PathComparer};
pub use watcher::{
    ErrFilesystemUnsupported, ErrOverflow, ErrUnavailable, ErrWatchTerminated, Error, IgnoreFunc,
    Sentinel, Watch, WatchCallback, WatchDirectoryRequest, WatchOption, Watcher, all_watchers,
    default, fanotify, fsevents, inotify, kqueue, set_write_settling, windows, with_ignore,
    with_recursive,
};
