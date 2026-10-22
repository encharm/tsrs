// windows.go: Windows ReadDirectoryChangesW backend.
//
// Uses the Win32 ReadDirectoryChangesW API with overlapped (asynchronous) I/O to monitor directory trees. Unlike
// the Unix backends, there is no shared event loop; each watch owns its own thread (Go: goroutine) that
// independently waits for directory changes.
//
// subscribe() per directory opens the directory (CreateFile with FILE_FLAG_OVERLAPPED), arms the first
// ReadDirectoryChangesW on the caller's thread, so that any filesystem operation after WatchDirectory returns is
// observed, and spawns run(). run() waits for the read to complete, arms the next one (double-buffering), walks the
// FILE_NOTIFY_INFORMATION chain (processOne) and calls dirWatch.notify(), which posts to the shared debouncer:
//   - FILE_ACTION_ADDED / RENAMED_NEW_NAME   → events.create
//   - FILE_ACTION_MODIFIED                   → events.update (files only)
//   - FILE_ACTION_REMOVED / RENAMED_OLD_NAME → events.remove, and the callbacks of a deleted root terminate
//
// Error recovery: ERROR_OPERATION_ABORTED is the normal shutdown (CancelIoEx was called); ERROR_INVALID_PARAMETER
// shrinks the buffer to 64 KB (the network share limit); ERROR_NOTIFY_ENUM_DIR is ErrOverflow (too many changes
// queued); ERROR_ACCESS_DENIED checks whether the watched directory was deleted.
//
// Shutdown: stop() → CancelIoEx cancels the in-flight read → run() returns, closing the directory handle once the
// read has fully finished → done. Go's wait() runs a helper goroutine per read that calls CancelIoEx when stopCh
// closes; here the wait covers the read's event and a stop event (WaitForMultipleObjects), and cancels the read
// itself when the stop event fires.

use std::sync::{Arc, Condvar, Mutex, Weak};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOTIFY_ENUM_DIR,
    ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileAttributesExW, GetFileAttributesW, GetFileExInfoStandard,
    GetFileInformationByHandle, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, ReadDirectoryChangesW,
    WIN32_FILE_ATTRIBUTE_DATA,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

use crate::watcher::{
    ErrOverflow, Error, dirWatch, dirWatchError, err_watched_directory_removed,
    handle_watcher_error, watcher, watcherBase, watcherImpl,
};

fn err_get_file_info() -> Error {
    Error::new("could not get file information")
}
fn err_read_changes() -> Error {
    Error::new("failed to read changes")
}
fn err_get_overlapped_result() -> Error {
    Error::new("GetOverlappedResult failed")
}
fn err_unknown() -> Error {
    Error::new("unknown error")
}

const defaultBufSize: usize = 1024 * 1024;
const networkBufSize: usize = 64 * 1024;

const notifyChangeFilter: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE;

// A HANDLE (a raw pointer) the threads of a watch share; Windows handles may be used from any thread.
#[derive(Clone, Copy)]
struct Handle(HANDLE);
// SAFETY: a kernel object handle is not tied to the thread that opened it.
unsafe impl Send for Handle {}
// SAFETY: as above; every use goes through thread-safe Win32 calls.
unsafe impl Sync for Handle {}

// Go `windows.UTF16PtrFromString`: the NUL-terminated UTF-16 form, or EINVAL when s contains a NUL.
fn utf16_ptr_from_string(s: &str) -> Result<Vec<u16>, Error> {
    if s.contains('\0') {
        return Err(Error::new("invalid argument"));
    }
    Ok(s.encode_utf16().chain(std::iter::once(0)).collect())
}

fn last_error() -> Error {
    // SAFETY: plain Win32 call.
    Error::from_errno(unsafe { GetLastError() } as i32)
}

pub(crate) struct windowsBackend {
    base: watcherBase,
    // Go's subscriptions reach the backend through a pointer; here a weak reference to the Arc that the factory
    // made, for handle_watcher_error.
    me: Weak<windowsBackend>,
}

// windows.go:127 (init)
pub(crate) fn init(mut w: watcher) -> watcher {
    w.factory = Some(|| new_windows_backend() as Arc<dyn watcherImpl>);
    w
}

// windows.go:131
fn new_windows_backend() -> Arc<windowsBackend> {
    Arc::new_cyclic(|me| windowsBackend { base: watcherBase::default(), me: Weak::clone(me) })
}

struct windowsSubscriptionState {
    stopped: bool,
    buf_bytes: usize,
}

pub(crate) struct windowsSubscription {
    mu: Mutex<windowsSubscriptionState>,
    watcher_impl: Weak<windowsBackend>,
    dir_watch: Arc<dirWatch>,
    handle: Handle,
    // Go `stopCh`: a manual-reset event, set by stop_locked
    stop_event: Handle,
    // Go `doneCh`
    done: Mutex<bool>,
    done_cv: Condvar,
    first: Mutex<Option<Box<windowsRead>>>,
}

// One overlapped read. Boxed: the kernel writes to `buf` and `overlapped` until the read completes, so neither may
// move. `buf` is u64s for the DWORD alignment ReadDirectoryChangesW needs.
struct windowsRead {
    buf: Vec<u64>,
    len: usize,
    overlapped: OVERLAPPED,
    event: Handle,
}

// SAFETY: the read is owned by one thread at a time (the subscriber, then run()).
unsafe impl Send for windowsRead {}

impl windowsRead {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `buf` holds at least `len` initialized bytes.
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), self.len) }
    }
}

// windows.go:163
fn new_windows_subscription(
    watcher_impl: &windowsBackend,
    w: &Arc<dirWatch>,
) -> Result<windowsSubscription, dirWatchError> {
    let path = utf16_ptr_from_string(&w.physical_dir)
        .map_err(|err| dirWatchError { err, dir_watch: Arc::clone(w) })?;
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        let err = last_error();
        return Err(dirWatchError {
            err: Error::wrap(format!("invalid handle: {err}"), &err),
            dir_watch: Arc::clone(w),
        });
    }
    // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `h` is the open handle; `info` is writable.
    if unsafe { GetFileInformationByHandle(h, &raw mut info) } == 0 {
        // SAFETY: closing the handle opened above.
        unsafe { CloseHandle(h) };
        return Err(dirWatchError { err: err_get_file_info(), dir_watch: Arc::clone(w) });
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        // SAFETY: closing the handle opened above.
        unsafe { CloseHandle(h) };
        return Err(dirWatchError { err: Error::new("not a directory"), dir_watch: Arc::clone(w) });
    }
    // SAFETY: a manual-reset event, initially not set.
    let stop_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if stop_event.is_null() {
        let err = last_error();
        // SAFETY: closing the handle opened above.
        unsafe { CloseHandle(h) };
        return Err(dirWatchError {
            err: Error::wrap(format!("CreateEvent: {err}"), &err),
            dir_watch: Arc::clone(w),
        });
    }
    Ok(windowsSubscription {
        mu: Mutex::new(windowsSubscriptionState { stopped: false, buf_bytes: defaultBufSize }),
        watcher_impl: Weak::clone(&watcher_impl.me),
        dir_watch: Arc::clone(w),
        handle: Handle(h),
        stop_event: Handle(stop_event),
        done: Mutex::new(false),
        done_cv: Condvar::new(),
        first: Mutex::new(None),
    })
}

impl windowsSubscription {
    // windows.go:200
    fn begin_read(&self) -> Result<Option<Box<windowsRead>>, Error> {
        let st = self.mu.lock().unwrap();
        if st.stopped {
            return Ok(None);
        }
        let buf_size = st.buf_bytes;
        drop(st);

        // SAFETY: zeroed is a valid OVERLAPPED.
        let overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let mut req = Box::new(windowsRead {
            buf: vec![0u64; buf_size.div_ceil(8)],
            len: buf_size,
            overlapped,
            event: Handle(std::ptr::null_mut()),
        });
        // SAFETY: a manual-reset event, initially not set.
        let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if ev.is_null() {
            let err = last_error();
            return Err(Error::wrap(format!("CreateEvent: {err}"), &err));
        }
        req.event = Handle(ev);
        req.overlapped.hEvent = ev;

        let mut bytes_returned = 0u32;
        // SAFETY: `req` is boxed, so `buf` and `overlapped` stay where they are until the read completes (run() waits
        // for it before dropping `req`).
        let ok = unsafe {
            ReadDirectoryChangesW(
                self.handle.0,
                req.buf.as_mut_ptr().cast(),
                req.len as u32,
                i32::from(self.dir_watch.recursive),
                notifyChangeFilter,
                &raw mut bytes_returned,
                &raw mut req.overlapped,
                None,
            )
        };
        if ok == 0 {
            // SAFETY: closing the event created above.
            unsafe { CloseHandle(ev) };
            return Err(dirWatchError {
                err: err_read_changes(),
                dir_watch: Arc::clone(&self.dir_watch),
            }
            .into());
        }
        Ok(Some(req))
    }

    // windows.go:235: the bytes read, the wait's error, the read's error
    fn wait(&self, r: &mut windowsRead) -> (u32, Option<Error>, Option<Error>) {
        let handles = [r.event.0, self.stop_event.0];
        // SAFETY: both handles are open events.
        let mut res = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if res == WAIT_OBJECT_0 + 1 {
            // stopped: cancel this read, then wait for it to finish
            // SAFETY: `r.overlapped` is the in-flight read on our handle.
            unsafe { CancelIoEx(self.handle.0, &raw const r.overlapped) };
            // SAFETY: an open event.
            res = unsafe { WaitForSingleObject(r.event.0, INFINITE) };
        }
        let wait_err = (res == WAIT_FAILED).then(last_error);
        let mut bytes = 0u32;
        // SAFETY: the read has completed (its event is set), so the result can be read without waiting.
        let completion_err = (unsafe {
            GetOverlappedResult(self.handle.0, &raw const r.overlapped, &raw mut bytes, 0)
        } == 0)
            .then(last_error);
        // SAFETY: closing the read's own event.
        unsafe { CloseHandle(r.event.0) };
        (bytes, wait_err, completion_err)
    }

    // windows.go:261
    // run is the per-watch thread. It loops on ReadDirectoryChangesW until the watch is stopped or an unrecoverable
    // error occurs. It closes the directory handle (not stop()) so that any in-flight ReadDirectoryChangesW has
    // completed and GetOverlappedResult has returned before the handle becomes invalid.
    fn run(self: &Arc<Self>) {
        self.run_inner();
        // SAFETY: closing the directory handle and the stop event; no read is in flight any more.
        unsafe {
            CloseHandle(self.handle.0);
            CloseHandle(self.stop_event.0);
        }
        *self.done.lock().unwrap() = true;
        self.done_cv.notify_all();
    }

    fn run_inner(self: &Arc<Self>) {
        let Some(mut current) = self.first.lock().unwrap().take() else {
            // subscribe always arms the initial read before spawning run. Guard the invariant rather than silently
            // producing a watch that delivers neither events nor errors if it ever breaks.
            self.fatal(Error::new("fswatch: windows: missing initial read"));
            return;
        };
        loop {
            let (bytes, wait_err, g_err) = self.wait(&mut current);
            if wait_err.is_some() && g_err.is_some() {
                self.fatal(err_get_overlapped_result());
                return;
            }

            if self.mu.lock().unwrap().stopped {
                return;
            }

            if let Some(g_err) = g_err {
                if self.process_completion(Some(&g_err), &[], bytes) {
                    return;
                }
                match self.begin_read() {
                    Err(err) => {
                        self.fatal(err);
                        return;
                    }
                    Ok(None) => return,
                    Ok(Some(next)) => current = next,
                }
                continue;
            }

            let next = match self.begin_read() {
                Err(err) => {
                    self.fatal(err);
                    return;
                }
                Ok(None) => return,
                Ok(Some(next)) => next,
            };
            if self.process_completion(None, current.bytes(), bytes) {
                return;
            }
            current = next;
        }
    }

    // windows.go:320
    // processCompletion mirrors the body of `Watch::processEvents` for the cases that translate cleanly to the
    // overlapped wrapper.
    fn process_completion(&self, call_err: Option<&Error>, buf: &[u8], bytes: u32) -> bool {
        if let Some(call_err) = call_err {
            let code = call_err.errno().map(|e| e as u32);
            if code == Some(ERROR_OPERATION_ABORTED) {
                return true;
            } else if code == Some(ERROR_INVALID_PARAMETER) {
                self.mu.lock().unwrap().buf_bytes = networkBufSize;
                return false;
            } else if code == Some(ERROR_NOTIFY_ENUM_DIR) {
                self.dir_watch.events.set_error(ErrOverflow.into());
                self.dir_watch.notify();
                return false;
            } else if code == Some(ERROR_ACCESS_DENIED) {
                // Possibly the watched dir was deleted; check and handle.
                let attrs = utf16_ptr_from_string(&self.dir_watch.physical_dir)
                    // SAFETY: `p` is NUL-terminated and outlives the call.
                    .map_or(INVALID_FILE_ATTRIBUTES, |p| unsafe { GetFileAttributesW(p.as_ptr()) });
                if attrs == INVALID_FILE_ATTRIBUTES || attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
                    self.dir_watch.events.remove(&self.dir_watch.dir);
                    self.dir_watch.events.set_error(err_watched_directory_removed());
                    self.dir_watch.notify();
                    self.stop();
                    return true;
                }
            }
            self.fatal(
                dirWatchError { err: err_unknown(), dir_watch: Arc::clone(&self.dir_watch) }.into(),
            );
            return true;
        }

        // Walk the FILE_NOTIFY_INFORMATION chain: NextEntryOffset, Action and FileNameLength (u32 each), then the
        // name (UTF-16, FileNameLength bytes).
        let mut offset = 0usize;
        let bytes = if bytes == 0 { buf.len() } else { bytes as usize };
        let u32_at = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
        while offset < bytes {
            let next_entry_offset = u32_at(offset) as usize;
            let action = u32_at(offset + 4);
            let name_len = u32_at(offset + 8) as usize / 2;
            let name_bytes = &buf[offset + 12..offset + 12 + name_len * 2];
            let units: Vec<u16> =
                name_bytes.as_chunks::<2>().0.iter().map(|c| u16::from_ne_bytes(*c)).collect();
            let name = String::from_utf16_lossy(&units);

            self.process_one(action, &name);

            if next_entry_offset == 0 {
                break;
            }
            offset += next_entry_offset;
        }
        self.dir_watch.notify();
        false
    }

    // windows.go:376
    fn process_one(&self, action: u32, name: &str) {
        let path = format!("{}\\{name}", self.dir_watch.dir);
        let watch_path = format!("{}\\{name}", self.dir_watch.physical_dir);
        match action {
            FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => {
                // Always emit the event, even if the file is already gone by the time we look it up. The kernel
                // told us it was added, and a subsequent REMOVED needs to find this entry in the eventList so the
                // create+delete pair coalesces away.
                self.dir_watch.events.create(&path);
            }
            FILE_ACTION_MODIFIED => {
                if let Ok(p) = utf16_ptr_from_string(&watch_path) {
                    // SAFETY: zeroed is a valid WIN32_FILE_ATTRIBUTE_DATA.
                    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
                    // SAFETY: `p` is NUL-terminated; `data` is the struct GetFileExInfoStandard fills.
                    let ok = unsafe {
                        GetFileAttributesExW(
                            p.as_ptr(),
                            GetFileExInfoStandard,
                            (&raw mut data).cast(),
                        )
                    };
                    if ok != 0 && data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                        self.dir_watch.events.update(&path);
                    }
                }
            }
            FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => {
                let seq = self.dir_watch.events.remove_and_get_sequence(&path);
                if self.dir_watch.terminate_callbacks_for_deleted_root(
                    &path,
                    seq,
                    &err_watched_directory_removed(),
                ) {
                    self.dir_watch.notify();
                }
            }
            _ => {}
        }
    }

    // windows.go:408
    // fatal is invoked when the run thread hits an unrecoverable error. handle_watcher_error eventually calls
    // close_watch, which waits for run() to finish; calling it from run() would deadlock, so it runs on a thread of
    // its own and run() can exit.
    fn fatal(&self, err: Error) {
        let werr = dirWatchError { err, dir_watch: Arc::clone(&self.dir_watch) };
        if let Some(impl_) = self.watcher_impl.upgrade() {
            let impl_: Arc<dyn watcherImpl> = impl_;
            std::thread::spawn(move || handle_watcher_error(&impl_, &werr));
        }
        self.stop();
    }

    // windows.go:414
    fn stop_locked(&self, st: &mut windowsSubscriptionState) {
        if st.stopped {
            return;
        }
        st.stopped = true;
        // SAFETY: setting our own stop event.
        unsafe { SetEvent(self.stop_event.0) };
        // Cancel any in-flight IO so the wait returns; the run thread closes the handle once the IO has fully
        // finished and GetOverlappedResult has returned.
        // SAFETY: cancels every read on our own handle.
        unsafe { CancelIoEx(self.handle.0, std::ptr::null()) };
    }

    // windows.go:426
    fn stop(&self) {
        let mut st = self.mu.lock().unwrap();
        self.stop_locked(&mut st);
    }
}

impl watcherImpl for windowsBackend {
    fn base(&self) -> &watcherBase {
        &self.base
    }

    // windows.go:139
    // start notifies that the backend is ready. Each watch owns its own thread, so there's no shared event loop
    // to start.
    fn start(self: Arc<Self>) -> Result<(), Error> {
        self.base.notify_started();
        Ok(())
    }

    // windows.go:433
    fn subscribe(&self, w: &Arc<dirWatch>) -> Result<(), Error> {
        let sub = new_windows_subscription(self, w)?;
        // Arm the first ReadDirectoryChangesW synchronously so that any file operation a caller performs after
        // subscribe returns is guaranteed to be observed. Doing this in run() would race the spawned thread with
        // the caller's first filesystem op, occasionally missing the initial create event or seeing it as a stray
        // modify.
        let first = match sub.begin_read() {
            Ok(first) => first,
            Err(err) => {
                // SAFETY: closing the handles new_windows_subscription opened; no read is in flight.
                unsafe {
                    CloseHandle(sub.handle.0);
                    CloseHandle(sub.stop_event.0);
                }
                return Err(err);
            }
        };
        *sub.first.lock().unwrap() = first;
        let sub = Arc::new(sub);
        *w.state.lock().unwrap() = Some(Arc::clone(&sub) as Arc<dyn std::any::Any + Send + Sync>);
        std::thread::Builder::new()
            .name("fswatch-windows".to_string())
            .spawn(move || sub.run())
            .expect("failed to spawn fswatch watch thread");
        Ok(())
    }

    // windows.go:459
    // Signals the watch thread to stop and waits for it to finish; that way the directory handle is closed before
    // this returns, so a follow-on operation (e.g. immediately re-watching, deleting the directory) sees a clean
    // slate.
    fn close_watch(&self, w: &Arc<dirWatch>) -> Result<(), Error> {
        let sub =
            w.state.lock().unwrap().take().and_then(|s| s.downcast::<windowsSubscription>().ok());
        let Some(sub) = sub else {
            return Ok(());
        };
        sub.stop();
        let done = sub.done.lock().unwrap();
        let _done = sub.done_cv.wait_while(done, |d| !*d).unwrap();
        Ok(())
    }

    // windows.go:471
    // Nothing to do; each watch owns its thread and is stopped by close_watch.
    fn shutdown(&self) {}
}
